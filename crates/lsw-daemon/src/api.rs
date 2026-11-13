//! The supervisor's local API: one JSON request per connection on
//! `$XDG_RUNTIME_DIR/lsw/<vm>/lsw.sock` (a `0700` directory, so only the
//! user can connect). Used by `lsw launch`, `lsw apps sync` and launchers.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// Longest accepted request line.
const MAX_REQUEST: usize = 16 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Status,
    SyncApps,
    Launch {
        launch: String,
    },
    /// From the Coherence client: how many windows it mirrors (idle
    /// detection).
    Windows {
        count: u32,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `status`: whether the guest agent is connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    /// `sync-apps`: number of launchers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<usize>,
    /// `status`: whether the VM is paused because nothing is visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            agent: None,
            apps: None,
            paused: None,
        }
    }
    pub fn err(e: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(e.into()),
            ..Self::ok()
        }
    }
}

/// A request waiting for the supervisor's answer.
pub struct Call {
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}

/// Accept API connections until the listener fails.
pub async fn serve(path: &Path, calls: mpsc::UnboundedSender<Call>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let calls = calls.clone();
        tokio::spawn(async move {
            let _ = handle(stream, calls).await;
        });
    }
}

async fn handle(stream: UnixStream, calls: mpsc::UnboundedSender<Call>) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(tokio::io::AsyncReadExt::take(r, MAX_REQUEST as u64));
    let read = reader.read_line(&mut line);
    let response = match tokio::time::timeout(Duration::from_secs(5), read).await {
        Ok(Ok(_)) => match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) => {
                let (tx, rx) = oneshot::channel();
                if calls.send(Call { request, reply: tx }).is_err() {
                    Response::err("the supervisor is stopping")
                } else {
                    rx.await.unwrap_or_else(|_| Response::err("no answer"))
                }
            }
            Err(e) => Response::err(format!("bad request: {e}")),
        },
        _ => Response::err("request timed out"),
    };
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    w.write_all(&out).await
}

/// Send one request to a running supervisor.
pub async fn call(path: &Path, request: &Request, timeout: Duration) -> anyhow::Result<Response> {
    let stream = UnixStream::connect(path).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    let mut answer = String::new();
    tokio::time::timeout(timeout, BufReader::new(r).read_line(&mut answer)).await??;
    Ok(serde_json::from_str(answer.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_roundtrip_and_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("lsw.sock");
        let (tx, mut rx) = mpsc::unbounded_channel::<Call>();
        let server_path = sock.clone();
        tokio::spawn(async move { serve(&server_path, tx).await });
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let response = match call.request {
                    Request::Launch { launch } if launch == "x!App" => Response::ok(),
                    Request::Status => Response {
                        agent: Some(true),
                        ..Response::ok()
                    },
                    _ => Response::err("nope"),
                };
                let _ = call.reply.send(response);
            }
        });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let t = Duration::from_secs(2);
        let r = call(
            &sock,
            &Request::Launch {
                launch: "x!App".into(),
            },
            t,
        )
        .await
        .unwrap();
        assert!(r.ok);
        let r = call(&sock, &Request::Status, t).await.unwrap();
        assert_eq!(r.agent, Some(true));
        // Garbage is answered with an error, not a crash.
        let mut s = UnixStream::connect(&sock).await.unwrap();
        s.write_all(b"{nonsense}\n").await.unwrap();
        let mut answer = String::new();
        BufReader::new(s).read_line(&mut answer).await.unwrap();
        let r: Response = serde_json::from_str(answer.trim()).unwrap();
        assert!(!r.ok && r.error.unwrap().starts_with("bad request"));
    }

    #[test]
    fn wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Launch { launch: "a".into() }).unwrap(),
            r#"{"cmd":"launch","launch":"a"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SyncApps).unwrap(),
            r#"{"cmd":"sync-apps"}"#
        );
    }
}
