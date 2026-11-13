//! Minimal asynchronous QMP (QEMU Machine Protocol) client.
//!
//! QMP is newline-delimited JSON over a Unix socket. The client performs the
//! capabilities handshake, tags every command with an `id`, and queues
//! asynchronous events that arrive while a command is in flight.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Largest QMP line accepted (QEMU's own replies stay far below this).
pub const MAX_LINE: usize = 16 << 20;

#[derive(Debug, thiserror::Error)]
pub enum QmpError {
    #[error("QMP I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("QMP sent invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("QMP connection closed")]
    Closed,
    #[error("QMP line exceeds {MAX_LINE} bytes")]
    LineTooLong,
    #[error("unexpected QMP greeting: {0}")]
    Greeting(String),
    #[error("QMP command `{command}` failed: {class}: {desc}")]
    Command {
        command: String,
        class: String,
        desc: String,
    },
    #[error("timed out waiting for QMP")]
    Timeout,
}

/// An asynchronous QMP event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: String,
    pub data: Value,
}

pub struct Qmp {
    reader: BufReader<OwnedReadHalf>,
    /// Bytes of a line read so far; kept here so a cancelled read (e.g. a
    /// `select!` branch losing) never loses data.
    partial: Vec<u8>,
    writer: OwnedWriteHalf,
    next_id: u64,
    events: VecDeque<Event>,
    /// QEMU version from the greeting, `(major, minor, micro)`.
    pub version: (u64, u64, u64),
}

impl Qmp {
    /// Connect, retrying until `timeout` while QEMU creates the socket.
    pub async fn connect_with_retry(path: &Path, timeout: Duration) -> Result<Self, QmpError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return Self::handshake(stream).await,
                Err(e) if tokio::time::Instant::now() < deadline => {
                    tracing::trace!("QMP not ready yet: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => return Err(QmpError::Timeout),
            }
        }
    }

    /// Perform the greeting + `qmp_capabilities` handshake on a stream.
    pub async fn handshake(stream: UnixStream) -> Result<Self, QmpError> {
        let (r, w) = stream.into_split();
        let mut qmp = Self {
            reader: BufReader::new(r),
            partial: Vec::new(),
            writer: w,
            next_id: 1,
            events: VecDeque::new(),
            version: (0, 0, 0),
        };
        let greeting = qmp.read_message().await?;
        let v = greeting
            .get("QMP")
            .and_then(|q| q.get("version"))
            .and_then(|v| v.get("qemu"))
            .ok_or_else(|| QmpError::Greeting(greeting.to_string()))?;
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        qmp.version = (n("major"), n("minor"), n("micro"));
        qmp.execute("qmp_capabilities", None).await?;
        Ok(qmp)
    }

    /// Run a command and return its `return` value.
    pub async fn execute(
        &mut self,
        command: &str,
        arguments: Option<Value>,
    ) -> Result<Value, QmpError> {
        self.execute_inner(command, arguments, None).await
    }

    /// Run a command with a file descriptor attached (`SCM_RIGHTS`), as
    /// `getfd` and `add-fd` require.
    pub async fn execute_with_fd(
        &mut self,
        command: &str,
        arguments: Option<Value>,
        fd: std::os::fd::BorrowedFd<'_>,
    ) -> Result<Value, QmpError> {
        self.execute_inner(command, arguments, Some(fd)).await
    }

    async fn execute_inner(
        &mut self,
        command: &str,
        arguments: Option<Value>,
        fd: Option<std::os::fd::BorrowedFd<'_>>,
    ) -> Result<Value, QmpError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({ "execute": command, "id": id });
        if let Some(args) = arguments {
            req["arguments"] = args;
        }
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        match fd {
            None => self.writer.write_all(&line).await?,
            Some(fd) => self.send_with_fd(&line, fd).await?,
        }
        loop {
            let msg = self.read_message().await?;
            if let Some(event) = as_event(&msg) {
                self.events.push_back(event);
                continue;
            }
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                tracing::warn!("ignoring QMP reply for another id: {msg}");
                continue;
            }
            if let Some(ret) = msg.get("return") {
                return Ok(ret.clone());
            }
            let err = msg.get("error").cloned().unwrap_or(Value::Null);
            return Err(QmpError::Command {
                command: command.to_owned(),
                class: err
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                desc: err
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
            });
        }
    }

    /// Write `line` with `fd` as ancillary data. The fd travels with the
    /// first byte; the rest of the line follows normally.
    async fn send_with_fd(
        &mut self,
        line: &[u8],
        fd: std::os::fd::BorrowedFd<'_>,
    ) -> std::io::Result<()> {
        // Rust ignores SIGPIPE process-wide, so no MSG_NOSIGNAL is needed.
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
        use std::os::fd::AsFd;
        let stream: &tokio::net::UnixStream = self.writer.as_ref();
        let sent = loop {
            stream.writable().await?;
            let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            let fds = [fd];
            control.push(SendAncillaryMessage::ScmRights(&fds));
            let iov = [std::io::IoSlice::new(line)];
            match stream.try_io(tokio::io::Interest::WRITABLE, || {
                sendmsg(stream.as_fd(), &iov, &mut control, SendFlags::empty())
                    .map_err(std::io::Error::from)
            }) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        };
        self.writer.write_all(&line[sent..]).await
    }

    /// Next asynchronous event (queued or freshly read). Cancel-safe.
    pub async fn next_event(&mut self) -> Result<Event, QmpError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(e);
        }
        loop {
            let msg = self.read_message().await?;
            if let Some(e) = as_event(&msg) {
                return Ok(e);
            }
        }
    }

    /// Wait for a named event, discarding others, up to `timeout`.
    pub async fn wait_event(&mut self, name: &str, timeout: Duration) -> Result<Event, QmpError> {
        let fut = async {
            loop {
                let e = self.next_event().await?;
                if e.name == name {
                    return Ok(e);
                }
            }
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| QmpError::Timeout)?
    }

    /// `query-status` → e.g. `"running"`, `"paused"`, `"shutdown"`.
    pub async fn status(&mut self) -> Result<String, QmpError> {
        let r = self.execute("query-status", None).await?;
        Ok(r.get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned())
    }

    /// Press and release one key (QKeyCode name, e.g. `"ret"`).
    pub async fn send_key(&mut self, qcode: &str) -> Result<(), QmpError> {
        self.execute(
            "send-key",
            Some(json!({ "keys": [{ "type": "qcode", "data": qcode }], "hold-time": 50 })),
        )
        .await
        .map(|_| ())
    }

    /// Press several keys together, e.g. `["meta_l", "r"]` for Win+R.
    pub async fn send_combo(&mut self, qcodes: &[&str]) -> Result<(), QmpError> {
        let keys: Vec<Value> = qcodes
            .iter()
            .map(|q| json!({ "type": "qcode", "data": q }))
            .collect();
        self.execute("send-key", Some(json!({ "keys": keys, "hold-time": 50 })))
            .await
            .map(|_| ())
    }

    /// Type ASCII text on a US keyboard layout (tests and bootstrapping).
    /// Characters without a mapping are skipped and returned.
    pub async fn type_text(&mut self, text: &str) -> Result<Vec<char>, QmpError> {
        let mut skipped = Vec::new();
        for c in text.chars() {
            match qcodes_for_char(c) {
                Some(keys) => {
                    self.send_combo(&keys).await?;
                    // `send-key` returns at once and QEMU replays the press
                    // and release with the hold time; pace to that, or
                    // QEMU's input queue overflows and drops keys.
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
                None => skipped.push(c),
            }
        }
        Ok(skipped)
    }

    /// Save a PNG screenshot of the primary display.
    pub async fn screendump_png(&mut self, path: &Path) -> Result<(), QmpError> {
        self.execute(
            "screendump",
            Some(json!({ "filename": path.to_string_lossy(), "format": "png" })),
        )
        .await
        .map(|_| ())
    }

    /// Read one non-empty JSON line, bounded by [`MAX_LINE`]. Cancel-safe:
    /// bytes are moved from the reader into `self.partial` before the next
    /// await point.
    async fn read_message(&mut self) -> Result<Value, QmpError> {
        loop {
            loop {
                let buf = self.reader.fill_buf().await?;
                if buf.is_empty() {
                    return Err(QmpError::Closed);
                }
                let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
                    Some(i) => (&buf[..=i], true),
                    None => (buf, false),
                };
                if self.partial.len() + chunk.len() > MAX_LINE {
                    return Err(QmpError::LineTooLong);
                }
                self.partial.extend_from_slice(chunk);
                let n = chunk.len();
                self.reader.consume(n);
                if done {
                    break;
                }
            }
            let line = std::mem::take(&mut self.partial);
            let trimmed = line.trim_ascii();
            if !trimmed.is_empty() {
                return Ok(serde_json::from_slice(trimmed)?);
            }
        }
    }
}

/// QKeyCodes that produce `c` on a US layout (with shift when needed).
pub fn qcodes_for_char(c: char) -> Option<Vec<&'static str>> {
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    let plain = |k| Some(vec![k]);
    let shifted = |k| Some(vec!["shift", k]);
    match c {
        'a'..='z' => plain(LETTERS[(c as u8 - b'a') as usize]),
        'A'..='Z' => shifted(LETTERS[(c as u8 - b'A') as usize]),
        '0'..='9' => plain(DIGITS[(c as u8 - b'0') as usize]),
        ' ' => plain("spc"),
        '\n' => plain("ret"),
        '\t' => plain("tab"),
        '-' => plain("minus"),
        '=' => plain("equal"),
        '[' => plain("bracket_left"),
        ']' => plain("bracket_right"),
        '\\' => plain("backslash"),
        ';' => plain("semicolon"),
        '\'' => plain("apostrophe"),
        '`' => plain("grave_accent"),
        ',' => plain("comma"),
        '.' => plain("dot"),
        '/' => plain("slash"),
        '_' => shifted("minus"),
        '+' => shifted("equal"),
        '{' => shifted("bracket_left"),
        '}' => shifted("bracket_right"),
        '|' => shifted("backslash"),
        ':' => shifted("semicolon"),
        '"' => shifted("apostrophe"),
        '~' => shifted("grave_accent"),
        '<' => shifted("comma"),
        '>' => shifted("dot"),
        '?' => shifted("slash"),
        '!' => shifted("1"),
        '@' => shifted("2"),
        '#' => shifted("3"),
        '$' => shifted("4"),
        '%' => shifted("5"),
        '^' => shifted("6"),
        '&' => shifted("7"),
        '*' => shifted("8"),
        '(' => shifted("9"),
        ')' => shifted("0"),
        _ => None,
    }
}

fn as_event(msg: &Value) -> Option<Event> {
    let name = msg.get("event")?.as_str()?.to_owned();
    Some(Event {
        name,
        data: msg.get("data").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    /// A scripted fake QEMU: replies to each request with the next response,
    /// optionally preceded by events.
    async fn fake_qemu(listener: UnixListener, script: Vec<(Vec<Value>, Value)>) {
        let (mut s, _) = listener.accept().await.unwrap();
        let greeting = json!({"QMP": {"version": {"qemu": {"major": 10, "minor": 1, "micro": 2}, "package": ""}, "capabilities": []}});
        s.write_all(format!("{greeting}\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = Vec::new();
        for (events, reply) in script {
            // Read one request line.
            loop {
                let mut byte = [0u8; 1];
                if s.read(&mut byte).await.unwrap() == 0 {
                    return;
                }
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
            }
            let req: Value = serde_json::from_slice(&buf).unwrap();
            buf.clear();
            for e in events {
                s.write_all(format!("{e}\r\n").as_bytes()).await.unwrap();
            }
            let mut reply = reply;
            reply["id"] = req["id"].clone();
            s.write_all(format!("{reply}\r\n").as_bytes())
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn handshake_command_events_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let script = vec![
            (vec![], json!({"return": {}})),
            (
                vec![
                    json!({"event": "RESUME", "data": {}, "timestamp": {"seconds": 1, "microseconds": 0}}),
                ],
                json!({"return": {"status": "running", "running": true}}),
            ),
            (
                vec![],
                json!({"error": {"class": "GenericError", "desc": "nope"}}),
            ),
        ];
        let server = tokio::spawn(fake_qemu(listener, script));
        let mut qmp = Qmp::connect_with_retry(&sock, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(qmp.version, (10, 1, 2));
        assert_eq!(qmp.status().await.unwrap(), "running");
        // The event that arrived mid-command is queued, not lost.
        assert_eq!(qmp.next_event().await.unwrap().name, "RESUME");
        match qmp.execute("bogus", None).await {
            Err(QmpError::Command { class, desc, .. }) => {
                assert_eq!(class, "GenericError");
                assert_eq!(desc, "nope");
            }
            other => panic!("unexpected {other:?}"),
        }
        server.await.unwrap();
    }

    #[test]
    fn us_layout_qcodes() {
        assert_eq!(qcodes_for_char('a'), Some(vec!["a"]));
        assert_eq!(qcodes_for_char('Q'), Some(vec!["shift", "q"]));
        assert_eq!(qcodes_for_char(':'), Some(vec!["shift", "semicolon"]));
        assert_eq!(qcodes_for_char('\\'), Some(vec!["backslash"]));
        assert_eq!(qcodes_for_char('\n'), Some(vec!["ret"]));
        assert_eq!(qcodes_for_char('é'), None);
        // Every printable ASCII character is covered.
        assert!((' '..='~').all(|c| qcodes_for_char(c).is_some()));
    }

    #[tokio::test]
    async fn connect_times_out_without_socket() {
        let dir = tempfile::tempdir().unwrap();
        let r =
            Qmp::connect_with_retry(&dir.path().join("missing.sock"), Duration::from_millis(120))
                .await;
        assert!(matches!(r, Err(QmpError::Timeout)));
    }
}
