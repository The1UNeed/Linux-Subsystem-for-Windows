//! Host end of the session channel, used by whichever LSW UI shows Windows
//! (the Window/Full Screen app now, the Coherence client in M2): clipboard
//! in both directions and display size hints.

use crate::channel::ChannelError;
use lsw_proto::{
    ClipFormat, ClipboardAssembler, Envelope, FrameDecoder, Message, PROTOCOL_VERSION,
    clipboard_chunks, encode, negotiate,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Most bytes waiting for the guest to read. A guest that keeps requesting
/// clipboard content without reading the replies hits this and is
/// disconnected instead of growing host memory.
pub const MAX_QUEUED_BYTES: usize = 64 << 20;

/// Something the UI should react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// The agent attached (or re-attached after a restart).
    Attached { agent_version: String },
    /// The Windows clipboard changed; fetch it with
    /// [`SessionClient::fetch`] if the host should mirror it.
    GuestClipboard {
        serial: u64,
        formats: Vec<ClipFormat>,
    },
    /// A requested clipboard transfer completed.
    GuestClipboardData {
        serial: u64,
        format: ClipFormat,
        data: Vec<u8>,
    },
    /// The agent reported an error.
    AgentError { detail: String },
}

/// Collects the formats of one guest clipboard offer. Only the current
/// offer's serial is accepted, each requested format once; unsolicited,
/// duplicate and stale results are dropped, and everything kept together
/// stays within [`lsw_proto::MAX_CLIPBOARD_TOTAL`].
#[derive(Debug, Default)]
pub struct GuestClipboard {
    serial: Option<u64>,
    pending: Vec<ClipFormat>,
    items: Vec<(ClipFormat, Vec<u8>)>,
    bytes: usize,
}

impl GuestClipboard {
    /// A new offer replaces the previous one. Returns the formats to fetch
    /// (each once).
    pub fn offer(&mut self, serial: u64, formats: &[ClipFormat]) -> Vec<ClipFormat> {
        let mut pending = Vec::new();
        for f in formats {
            if !pending.contains(f) {
                pending.push(*f);
            }
        }
        *self = Self {
            serial: Some(serial),
            pending: pending.clone(),
            ..Self::default()
        };
        pending
    }

    /// One transfer completed. Returns the whole clipboard once every
    /// requested format has arrived (None while waiting, or if empty).
    pub fn data(
        &mut self,
        serial: u64,
        format: ClipFormat,
        data: Vec<u8>,
    ) -> Option<Vec<(ClipFormat, Vec<u8>)>> {
        if self.serial != Some(serial) {
            return None; // stale or unsolicited
        }
        let i = self.pending.iter().position(|f| *f == format)?; // not asked, or a duplicate
        self.pending.swap_remove(i);
        let total = lsw_proto::MAX_CLIPBOARD_TOTAL as usize;
        if !data.is_empty() && self.bytes + data.len() <= total {
            self.bytes += data.len();
            self.items.push((format, data));
        }
        if !self.pending.is_empty() {
            return None;
        }
        self.serial = None;
        self.bytes = 0;
        let items = std::mem::take(&mut self.items);
        (!items.is_empty()).then_some(items)
    }
}

/// The UI end of the session channel.
///
/// Cancel-safe: [`Self::next_event`] may be dropped mid-way (it runs in a
/// `select!` with input handling). Its only await point is a socket read
/// whose bytes go straight into the decoder; replies are queued as whole
/// frames to a writer task that owns the write half, so a cancelled future
/// can never leave half a frame on the wire.
pub struct SessionClient<S> {
    reader: tokio::io::ReadHalf<S>,
    decoder: FrameDecoder,
    buf: Box<[u8]>,
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes queued but not yet written (shared with the writer task).
    queued: Arc<AtomicUsize>,
    /// The writer task, stopped with the client: a guest that stopped
    /// reading would otherwise keep it blocked in a write forever, holding
    /// the socket and every queued frame.
    writer_task: tokio::task::AbortHandle,
    next_seq: u64,
    attached: bool,
    /// Host clipboard content offered to the guest, by serial.
    host_serial: u64,
    host_items: Vec<(ClipFormat, Vec<u8>)>,
    assembler: ClipboardAssembler,
}

impl<S> Drop for SessionClient<S> {
    fn drop(&mut self) {
        self.writer_task.abort();
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> SessionClient<S> {
    /// Wrap a connected stream and send `Attach`.
    pub async fn new(stream: S) -> Result<Self, ChannelError> {
        let (reader, mut writer) = tokio::io::split(stream);
        let (out, mut frames) = mpsc::unbounded_channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        let written = queued.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                let ok = writer.write_all(&frame).await.is_ok();
                written.fetch_sub(frame.len(), Ordering::AcqRel);
                if !ok {
                    break;
                }
            }
        })
        .abort_handle();
        let mut c = Self {
            reader,
            decoder: FrameDecoder::new(),
            buf: vec![0u8; 64 * 1024].into_boxed_slice(),
            out,
            queued,
            writer_task,
            next_seq: 1,
            attached: false,
            host_serial: 0,
            host_items: Vec::new(),
            assembler: ClipboardAssembler::default(),
        };
        c.attach().await?;
        Ok(c)
    }

    /// Queue one whole frame.
    fn queue(&mut self, reply_to: Option<u64>, msg: Message) -> Result<(), ChannelError> {
        let env = Envelope {
            seq: self.next_seq,
            reply_to,
            msg,
        };
        self.next_seq += 1;
        let frame = encode(&env)?;
        let len = frame.len();
        if self.queued.fetch_add(len, Ordering::AcqRel) + len > MAX_QUEUED_BYTES {
            self.queued.fetch_sub(len, Ordering::AcqRel);
            return Err(ChannelError::Overloaded);
        }
        self.out.send(frame).map_err(|_| ChannelError::Closed)
    }

    /// Ask the agent to introduce itself (on connect and whenever the guest
    /// reopens its port).
    pub async fn attach(&mut self) -> Result<(), ChannelError> {
        self.attached = false;
        self.queue(
            None,
            Message::Attach {
                protocol: PROTOCOL_VERSION,
            },
        )
    }

    pub fn attached(&self) -> bool {
        self.attached
    }

    /// Offer the host clipboard to Windows. The guest fetches the formats
    /// it wants; requests are answered inside [`Self::next_event`].
    pub async fn offer(&mut self, items: Vec<(ClipFormat, Vec<u8>)>) -> Result<(), ChannelError> {
        self.host_serial += 1;
        let formats = items.iter().map(|(f, _)| *f).collect();
        self.host_items = items;
        if self.attached {
            let serial = self.host_serial;
            self.queue(None, Message::ClipboardOffer { serial, formats })?;
        }
        Ok(())
    }

    /// Request the guest clipboard `serial` in `format`; the data arrives as
    /// [`SessionEvent::GuestClipboardData`].
    pub async fn fetch(&mut self, serial: u64, format: ClipFormat) -> Result<(), ChannelError> {
        self.queue(None, Message::ClipboardRequest { serial, format })
    }

    /// Wait until everything queued has been written to the guest, up to
    /// `timeout`. For short-lived callers (a command that sends one request
    /// and exits): dropping the client stops its writer at once.
    pub async fn flush(&self, timeout: Duration) -> Result<(), ChannelError> {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.queued.load(Ordering::Acquire) > 0 {
            if self.writer_task.is_finished() {
                return Err(ChannelError::Closed);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChannelError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the guest did not take the request in time",
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// Resize the Windows display.
    pub async fn display_hint(
        &mut self,
        width: u32,
        height: u32,
        scale_percent: u16,
    ) -> Result<(), ChannelError> {
        self.queue(
            None,
            Message::DisplayHint {
                width,
                height,
                scale_percent,
            },
        )
    }

    /// Process guest messages until something the UI cares about happens.
    /// Handshake replies, pings and clipboard requests are answered here.
    /// Cancel-safe (see the type documentation).
    pub async fn next_event(&mut self) -> Result<SessionEvent, ChannelError> {
        loop {
            while let Some(env) = self.decoder.next_envelope()? {
                if let Some(event) = self.handle(env)? {
                    return Ok(event);
                }
            }
            let n = self.reader.read(&mut self.buf).await?;
            if n == 0 {
                return Err(ChannelError::Closed);
            }
            self.decoder.push(&self.buf[..n])?;
        }
    }

    fn handle(&mut self, env: Envelope) -> Result<Option<SessionEvent>, ChannelError> {
        match env.msg {
            Message::Hello {
                protocol,
                agent_version,
                ..
            } => match negotiate(protocol) {
                Ok(v) => {
                    self.queue(Some(env.seq), Message::Welcome { protocol: v })?;
                    self.attached = true;
                    // Bring Windows up to date with the host clipboard.
                    if !self.host_items.is_empty() {
                        let formats = self.host_items.iter().map(|(f, _)| *f).collect();
                        let serial = self.host_serial;
                        self.queue(None, Message::ClipboardOffer { serial, formats })?;
                    }
                    Ok(Some(SessionEvent::Attached { agent_version }))
                }
                Err(_) => Ok(Some(SessionEvent::AgentError {
                    detail: format!("unsupported agent protocol {protocol}"),
                })),
            },
            _ if !self.attached => Ok(None),
            Message::Ping { nonce } => {
                self.queue(Some(env.seq), Message::Pong { nonce })?;
                Ok(None)
            }
            Message::ClipboardOffer { serial, formats } => {
                Ok(Some(SessionEvent::GuestClipboard { serial, formats }))
            }
            Message::ClipboardRequest { serial, format } => {
                let data = if serial == self.host_serial {
                    self.host_items
                        .iter()
                        .find(|(f, _)| *f == format)
                        .map(|(_, d)| d.clone())
                        .unwrap_or_default()
                } else {
                    Vec::new() // stale: an empty answer completes the request
                };
                let frames = clipboard_chunks(serial, format, &data)
                    .unwrap_or_else(|_| clipboard_chunks(serial, format, &[]).unwrap_or_default());
                for frame in frames {
                    self.queue(Some(env.seq), frame)?;
                }
                Ok(None)
            }
            Message::ClipboardData {
                serial,
                format,
                total,
                offset,
                chunk,
            } => match self.assembler.push(serial, format, total, offset, &chunk) {
                Ok(Some((serial, format, data))) => Ok(Some(SessionEvent::GuestClipboardData {
                    serial,
                    format,
                    data,
                })),
                Ok(None) => Ok(None),
                Err(e) => Err(ChannelError::Protocol(e)),
            },
            Message::Error { detail, .. } => Ok(Some(SessionEvent::AgentError { detail })),
            _ => Ok(None),
        }
    }
}

/// Connect to the session socket and attach.
pub async fn connect(
    path: &std::path::Path,
) -> Result<SessionClient<tokio::net::UnixStream>, ChannelError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(s) => break s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    SessionClient::new(stream).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;

    /// A guest that answers Attach, offers its clipboard, and serves a
    /// request; then accepts the host's offer by requesting it.
    async fn fake_guest(stream: tokio::io::DuplexStream) {
        let mut g = Channel::new(stream);
        let attach = g.recv().await.unwrap();
        assert!(matches!(attach.msg, Message::Attach { .. }));
        g.reply(
            attach.seq,
            Message::Hello {
                protocol: PROTOCOL_VERSION,
                agent_version: "t".into(),
                os_build: "b".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            g.recv().await.unwrap().msg,
            Message::Welcome { .. }
        ));
        g.send(Message::ClipboardOffer {
            serial: 5,
            formats: vec![ClipFormat::Text],
        })
        .await
        .unwrap();
        let req = g.recv().await.unwrap();
        assert_eq!(
            req.msg,
            Message::ClipboardRequest {
                serial: 5,
                format: ClipFormat::Text
            }
        );
        for f in clipboard_chunks(5, ClipFormat::Text, b"from windows").unwrap() {
            g.reply(req.seq, f).await.unwrap();
        }
        // Host offers its clipboard; fetch it.
        let Message::ClipboardOffer { serial, formats } = g.recv().await.unwrap().msg else {
            panic!()
        };
        assert_eq!(formats, [ClipFormat::Html]);
        g.send(Message::ClipboardRequest {
            serial,
            format: ClipFormat::Html,
        })
        .await
        .unwrap();
        let data = g.recv().await.unwrap();
        assert!(
            matches!(data.msg, Message::ClipboardData { ref chunk, .. } if chunk == b"<b>linux</b>")
        );
        assert!(matches!(
            g.recv().await.unwrap().msg,
            Message::DisplayHint { width: 1600, .. }
        ));
    }

    #[tokio::test]
    async fn clipboard_both_ways_and_display_hint() {
        let (host, guest) = tokio::io::duplex(1 << 16);
        let guest = tokio::spawn(fake_guest(guest));
        let mut s = SessionClient::new(host).await.unwrap();
        assert!(matches!(
            s.next_event().await.unwrap(),
            SessionEvent::Attached { .. }
        ));
        let SessionEvent::GuestClipboard { serial, formats } = s.next_event().await.unwrap() else {
            panic!()
        };
        assert_eq!(formats, [ClipFormat::Text]);
        s.fetch(serial, ClipFormat::Text).await.unwrap();
        assert_eq!(
            s.next_event().await.unwrap(),
            SessionEvent::GuestClipboardData {
                serial: 5,
                format: ClipFormat::Text,
                data: b"from windows".to_vec()
            }
        );
        s.offer(vec![(ClipFormat::Html, b"<b>linux</b>".to_vec())])
            .await
            .unwrap();
        // Serving the guest's request happens inside next_event; the guest
        // then expects the display hint, so run both concurrently.
        let pump = async {
            let _ = tokio::time::timeout(Duration::from_millis(300), s.next_event()).await;
            s.display_hint(1600, 900, 100).await.unwrap();
        };
        pump.await;
        guest.await.unwrap();
    }

    /// Regression: cancelling `next_event` while it answers a large
    /// clipboard request (input arriving in a `select!`) used to leave half
    /// a frame on the wire.
    #[tokio::test]
    async fn cancelling_next_event_never_splits_a_frame() {
        let (host, guest) = tokio::io::duplex(1024); // small: writes block
        let guest = tokio::spawn(async move {
            let mut g = Channel::new(guest);
            let attach = g.recv().await.unwrap();
            g.reply(
                attach.seq,
                Message::Hello {
                    protocol: PROTOCOL_VERSION,
                    agent_version: "t".into(),
                    os_build: "b".into(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                g.recv().await.unwrap().msg,
                Message::Welcome { .. }
            ));
            let Message::ClipboardOffer { serial, .. } = g.recv().await.unwrap().msg else {
                panic!()
            };
            g.send(Message::ClipboardRequest {
                serial,
                format: ClipFormat::Text,
            })
            .await
            .unwrap();
            // Every frame decodes: the data arrives whole, then the hint.
            let mut asm = ClipboardAssembler::default();
            let data = loop {
                let Message::ClipboardData {
                    serial,
                    format,
                    total,
                    offset,
                    chunk,
                } = g.recv().await.unwrap().msg
                else {
                    panic!("frame boundary lost")
                };
                if let Some((_, _, d)) = asm.push(serial, format, total, offset, &chunk).unwrap() {
                    break d;
                }
            };
            assert_eq!(data.len(), 700_000);
            assert!(matches!(
                g.recv().await.unwrap().msg,
                Message::DisplayHint { .. }
            ));
        });
        let mut s = SessionClient::new(host).await.unwrap();
        s.offer(vec![(ClipFormat::Text, vec![b'x'; 700_000])])
            .await
            .unwrap();
        // Keep cancelling next_event, as input handling does.
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            tokio::select! {
                _ = s.next_event() => {}
                _ = tokio::time::sleep(Duration::from_millis(1)) => {}
            }
        }
        s.display_hint(800, 600, 100).await.unwrap();
        guest.await.unwrap();
    }

    /// Regression: a guest that requests the clipboard over and over
    /// without reading the replies is disconnected at the byte budget
    /// instead of growing host memory without bound.
    #[tokio::test]
    async fn a_guest_that_never_reads_is_cut_off() {
        let (host, guest) = tokio::io::duplex(4096);
        let mut s = SessionClient::new(host).await.unwrap();
        let mut g = Channel::new(guest);
        let attach = g.recv().await.unwrap();
        g.reply(
            attach.seq,
            Message::Hello {
                protocol: PROTOCOL_VERSION,
                agent_version: "t".into(),
                os_build: "b".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            s.next_event().await.unwrap(),
            SessionEvent::Attached { .. }
        ));
        s.offer(vec![(ClipFormat::Text, vec![b'x'; 700_000])])
            .await
            .unwrap();
        let serial = s.host_serial;
        // Flood requests; the guest reads nothing after this point.
        let flood = async {
            for _ in 0..200 {
                g.send(Message::ClipboardRequest {
                    serial,
                    format: ClipFormat::Text,
                })
                .await
                .unwrap();
            }
        };
        let host_side = async {
            loop {
                if let Err(e) = s.next_event().await {
                    return e;
                }
            }
        };
        let (_, err) = tokio::join!(flood, host_side);
        assert!(matches!(err, ChannelError::Overloaded), "{err:?}");
        assert!(s.queued.load(Ordering::Acquire) <= MAX_QUEUED_BYTES);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_flushed_request_arrives_even_if_the_client_exits_right_after() {
        let (host, guest) = tokio::io::duplex(64 * 1024);
        let mut s = SessionClient::new(host).await.unwrap();
        s.display_hint(1600, 900, 100).await.unwrap();
        s.flush(Duration::from_secs(5)).await.unwrap();
        drop(s);
        let mut g = Channel::new(guest);
        assert!(matches!(
            g.recv().await.unwrap().msg,
            Message::Attach { .. }
        ));
        assert!(matches!(
            g.recv().await.unwrap().msg,
            Message::DisplayHint {
                width: 1600,
                height: 900,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn flush_gives_up_when_the_guest_does_not_read() {
        let (host, _guest) = tokio::io::duplex(256);
        let mut s = SessionClient::new(host).await.unwrap();
        for nonce in 0..200 {
            s.queue(None, Message::Ping { nonce }).unwrap();
        }
        assert!(s.flush(Duration::from_millis(200)).await.is_err());
    }

    #[test]
    fn guest_clipboard_accepts_each_requested_format_once() {
        let mut c = GuestClipboard::default();
        assert_eq!(
            c.offer(7, &[ClipFormat::Text, ClipFormat::Html, ClipFormat::Text]),
            vec![ClipFormat::Text, ClipFormat::Html],
            "each format fetched once"
        );
        assert_eq!(
            c.data(7, ClipFormat::Text, b"a".to_vec()),
            None,
            "HTML pending"
        );
        // Duplicates, unsolicited formats and stale serials are dropped.
        for _ in 0..64 {
            assert_eq!(c.data(7, ClipFormat::Text, vec![b'x'; 1 << 20]), None);
        }
        assert_eq!(c.data(7, ClipFormat::Png, b"img".to_vec()), None);
        assert_eq!(c.data(6, ClipFormat::Html, b"old".to_vec()), None);
        assert_eq!(c.items.len(), 1);
        assert_eq!(c.bytes, 1);
        let done = c.data(7, ClipFormat::Html, b"<b>a</b>".to_vec()).unwrap();
        assert_eq!(
            done,
            vec![
                (ClipFormat::Text, b"a".to_vec()),
                (ClipFormat::Html, b"<b>a</b>".to_vec())
            ]
        );
        // The offer is finished: nothing more is accepted for it.
        assert_eq!(c.data(7, ClipFormat::Html, b"again".to_vec()), None);
    }

    #[test]
    fn guest_clipboard_keeps_its_total_within_the_limit() {
        let mut c = GuestClipboard::default();
        let max = lsw_proto::MAX_CLIPBOARD_TOTAL as usize;
        c.offer(1, &[ClipFormat::Text, ClipFormat::Html]);
        assert_eq!(c.data(1, ClipFormat::Text, vec![b't'; max]), None);
        // The second format would exceed the total: it is left out.
        let done = c.data(1, ClipFormat::Html, vec![b'h'; 10]).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, ClipFormat::Text);
        // A new offer replaces an unfinished one.
        c.offer(2, &[ClipFormat::Text, ClipFormat::Html]);
        c.data(2, ClipFormat::Text, b"x".to_vec());
        c.offer(3, &[ClipFormat::Text]);
        assert_eq!(c.data(2, ClipFormat::Html, b"late".to_vec()), None);
        assert_eq!(
            c.data(3, ClipFormat::Text, b"y".to_vec()),
            Some(vec![(ClipFormat::Text, b"y".to_vec())])
        );
    }

    /// A stream that records when it is dropped.
    struct Tracked {
        inner: tokio::io::DuplexStream,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    impl AsyncRead for Tracked {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for Tracked {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    #[tokio::test]
    async fn dropping_a_client_whose_guest_stopped_reading_releases_the_socket() {
        let (host, guest) = tokio::io::duplex(1024);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut s = SessionClient::new(Tracked {
            inner: host,
            dropped: dropped.clone(),
        })
        .await
        .unwrap();
        // The guest never reads: fill the pipe until the writer blocks.
        for nonce in 0..2000 {
            s.queue(None, Message::Ping { nonce }).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            s.queued.load(Ordering::Acquire) > 0,
            "the writer must be blocked"
        );
        drop(s);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            dropped.load(Ordering::Acquire),
            "the blocked writer kept the socket and its queue"
        );
        drop(guest);
    }
}
