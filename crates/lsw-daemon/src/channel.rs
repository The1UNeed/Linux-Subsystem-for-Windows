//! Host end of a guest agent channel (virtio-serial port exposed by QEMU as
//! a Unix socket).
//!
//! The guest is untrusted: every byte goes through [`FrameDecoder`], which
//! bounds frame sizes and validates every field before a message reaches
//! the caller. A protocol error ends the connection.

use lsw_proto::{
    AppEntry, Envelope, ErrorCode, FrameDecoder, Message, PROTOCOL_VERSION, ProtoError, ShareSpec,
    encode, negotiate,
};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("channel I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("guest protocol violation: {0}")]
    Protocol(#[from] ProtoError),
    #[error("channel closed")]
    Closed,
    #[error("the guest is not reading what it asked for; disconnecting")]
    Overloaded,
}

/// A framed, bidirectional channel.
pub struct Channel<S> {
    stream: S,
    decoder: FrameDecoder,
    next_seq: u64,
    buf: Box<[u8]>,
}

impl Channel<UnixStream> {
    /// Connect to QEMU's chardev socket, retrying while QEMU starts.
    pub async fn connect(path: &Path, timeout: Duration) -> Result<Self, ChannelError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match UnixStream::connect(path).await {
                Ok(s) => return Ok(Self::new(s)),
                Err(e) if tokio::time::Instant::now() < deadline => {
                    tracing::trace!("agent socket not ready: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Channel<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            decoder: FrameDecoder::new(),
            next_seq: 1,
            buf: vec![0u8; 64 * 1024].into_boxed_slice(),
        }
    }

    /// Send a message; returns its sequence number.
    pub async fn send(&mut self, msg: Message) -> Result<u64, ChannelError> {
        let seq = self.take_seq();
        self.write(&Envelope::new(seq, msg)).await?;
        Ok(seq)
    }

    /// Send a reply to `request_seq`.
    pub async fn reply(&mut self, request_seq: u64, msg: Message) -> Result<(), ChannelError> {
        let seq = self.take_seq();
        self.write(&Envelope::reply(seq, request_seq, msg)).await
    }

    /// Receive the next validated envelope.
    pub async fn recv(&mut self) -> Result<Envelope, ChannelError> {
        loop {
            if let Some(env) = self.decoder.next_envelope()? {
                return Ok(env);
            }
            let n = self.stream.read(&mut self.buf).await?;
            if n == 0 {
                return Err(ChannelError::Closed);
            }
            self.decoder.push(&self.buf[..n])?;
        }
    }

    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    async fn write(&mut self, env: &Envelope) -> Result<(), ChannelError> {
        let frame = encode(env)?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }
}

/// What the supervisor learns from the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// The agent (re)connected and was accepted.
    Connected {
        agent_version: String,
        os_build: String,
        protocol: u16,
    },
    /// A share was mounted (or failed to mount) in the guest.
    Share {
        tag: String,
        ok: bool,
        detail: String,
    },
}

/// Control-channel protocol state on the host: handshake, keep-alive and
/// share configuration. Pure logic, driven by [`ControlSession::handle`].
#[derive(Debug, Clone)]
pub struct ControlSession {
    shares: Vec<ShareSpec>,
    accepted: bool,
}

impl ControlSession {
    pub fn new(shares: Vec<ShareSpec>) -> Self {
        Self {
            shares,
            accepted: false,
        }
    }

    pub fn accepted(&self) -> bool {
        self.accepted
    }

    /// The guest closed its end of the port (agent stopped).
    pub fn reset(&mut self) {
        self.accepted = false;
    }

    /// React to one guest message: the replies to send (`(reply_to, msg)`)
    /// and any event for the supervisor.
    pub fn handle(&mut self, env: &Envelope) -> (Vec<(Option<u64>, Message)>, Option<AgentEvent>) {
        let mut out = Vec::new();
        let event = match &env.msg {
            Message::Hello {
                protocol,
                agent_version,
                os_build,
            } => match negotiate(*protocol) {
                Ok(v) => {
                    // A new Hello means the agent restarted: start over.
                    self.accepted = true;
                    out.push((Some(env.seq), Message::Welcome { protocol: v }));
                    out.push((
                        None,
                        Message::ShareFolders {
                            shares: self.shares.clone(),
                        },
                    ));
                    Some(AgentEvent::Connected {
                        agent_version: agent_version.clone(),
                        os_build: os_build.clone(),
                        protocol: v,
                    })
                }
                Err(_) => {
                    self.accepted = false;
                    out.push((
                        Some(env.seq),
                        Message::Error {
                            code: ErrorCode::UnsupportedVersion,
                            detail: format!(
                                "agent protocol {protocol} is not supported (host speaks {PROTOCOL_VERSION})"
                            ),
                        },
                    ));
                    None
                }
            },
            _ if !self.accepted => {
                out.push((
                    Some(env.seq),
                    Message::Error {
                        code: ErrorCode::BadRequest,
                        detail: "Hello first".into(),
                    },
                ));
                None
            }
            Message::Ping { nonce } => {
                out.push((Some(env.seq), Message::Pong { nonce: *nonce }));
                None
            }
            Message::ShareStatus { tag, ok, detail } => Some(AgentEvent::Share {
                tag: tag.clone(),
                ok: *ok,
                detail: detail.clone(),
            }),
            Message::Pong { .. } | Message::Error { .. } => None,
            // Session-channel and host-to-guest messages have no business
            // on the control channel from the guest.
            _ => {
                out.push((
                    Some(env.seq),
                    Message::Error {
                        code: ErrorCode::BadRequest,
                        detail: "unexpected message on the control channel".into(),
                    },
                ));
                None
            }
        };
        (out, event)
    }
}

/// Requests the supervisor sends through the control channel.
#[derive(Debug)]
pub enum ControlCommand {
    /// List installed Windows apps.
    ListApps(AppsReply),
    /// Start an app by its shell id.
    Launch(String, oneshot::Sender<Result<(), String>>),
}

/// Where an app listing is delivered.
pub type AppsReply = oneshot::Sender<Result<Vec<AppEntry>, String>>;

/// Most app entries accepted in one listing (the guest is untrusted).
const MAX_APPS: usize = 2048;

/// Ping interval once the agent is accepted.
pub const KEEPALIVE: Duration = Duration::from_secs(5);

/// Serve the control channel until it closes, reporting events.
///
/// `port_open` carries QEMU's `VSERPORT_CHANGE` state for the guest end of
/// the port: the host sends `Attach` once on connect and again whenever the
/// guest (re)opens the port, never periodically (frames written while the
/// guest has the port closed queue up and would all arrive at once).
pub async fn serve_control<S: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut Channel<S>,
    session: &mut ControlSession,
    port_open: &mut mpsc::UnboundedReceiver<bool>,
    commands: &mut mpsc::UnboundedReceiver<ControlCommand>,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<(), ChannelError> {
    // A listing in progress: the requester and the entries so far.
    let mut listing: Option<(AppsReply, Vec<AppEntry>)> = None;
    let attach = Message::Attach {
        protocol: PROTOCOL_VERSION,
    };
    channel.send(attach.clone()).await?;
    let mut tick = tokio::time::interval(KEEPALIVE);
    tick.tick().await;
    let mut nonce = 0u64;
    loop {
        let env = tokio::select! {
            env = channel.recv() => env?,
            open = port_open.recv() => {
                match open {
                    Some(true) => channel.send(attach.clone()).await.map(|_| ())?,
                    Some(false) => session.reset(),
                    None => {}
                }
                continue;
            }
            _ = tick.tick(), if session.accepted() => {
                nonce += 1;
                channel.send(Message::Ping { nonce }).await?;
                continue;
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(ControlCommand::ListApps(reply)) if session.accepted() => {
                        channel.send(Message::ListApps).await?;
                        listing = Some((reply, Vec::new()));
                    }
                    Some(ControlCommand::Launch(id, reply)) if session.accepted() => {
                        channel.send(Message::Launch { launch: id }).await?;
                        let _ = reply.send(Ok(()));
                    }
                    Some(ControlCommand::ListApps(reply)) => {
                        let _ = reply.send(Err("the guest agent is not connected".into()));
                    }
                    Some(ControlCommand::Launch(_, reply)) => {
                        let _ = reply.send(Err("the guest agent is not connected".into()));
                    }
                    None => {}
                }
                continue;
            }
        };
        // App listings are collected here, not in the session state machine.
        match &env.msg {
            Message::App(app) if session.accepted() => {
                if let Some((_, apps)) = listing.as_mut()
                    && apps.len() < MAX_APPS
                {
                    apps.push(app.clone());
                }
                continue;
            }
            Message::AppsDone if session.accepted() => {
                if let Some((reply, apps)) = listing.take() {
                    let _ = reply.send(Ok(apps));
                }
                continue;
            }
            _ => {}
        }
        let (replies, event) = session.handle(&env);
        for (reply_to, msg) in replies {
            match reply_to {
                Some(seq) => channel.reply(seq, msg).await?,
                None => {
                    channel.send(msg).await?;
                }
            }
        }
        if let Some(e) = event {
            on_event(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsw_proto::KnownFolder;

    fn hello(seq: u64, protocol: u16) -> Envelope {
        Envelope::new(
            seq,
            Message::Hello {
                protocol,
                agent_version: "0.0.1-alpha".into(),
                os_build: "10.0.26300".into(),
            },
        )
    }

    fn shares() -> Vec<ShareSpec> {
        vec![ShareSpec {
            tag: "lsw-documents".into(),
            folder: KnownFolder::Documents,
        }]
    }

    #[test]
    fn handshake_then_shares_then_ping() {
        let mut s = ControlSession::new(shares());
        // Nothing but Hello is accepted before the handshake.
        let (out, ev) = s.handle(&Envelope::new(1, Message::Ping { nonce: 1 }));
        assert!(matches!(
            out[0].1,
            Message::Error {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
        assert!(ev.is_none());

        let (out, ev) = s.handle(&hello(2, PROTOCOL_VERSION));
        assert_eq!(
            out[0],
            (
                Some(2),
                Message::Welcome {
                    protocol: PROTOCOL_VERSION
                }
            )
        );
        assert_eq!(out[1], (None, Message::ShareFolders { shares: shares() }));
        assert!(matches!(ev, Some(AgentEvent::Connected { .. })));
        assert!(s.accepted());

        let (out, _) = s.handle(&Envelope::new(3, Message::Ping { nonce: 9 }));
        assert_eq!(out, vec![(Some(3), Message::Pong { nonce: 9 })]);

        let (_, ev) = s.handle(&Envelope::new(
            4,
            Message::ShareStatus {
                tag: "lsw-documents".into(),
                ok: true,
                detail: String::new(),
            },
        ));
        assert!(matches!(ev, Some(AgentEvent::Share { ok: true, .. })));
    }

    #[test]
    fn rejects_old_agents_and_misplaced_messages() {
        let mut s = ControlSession::new(vec![]);
        let (out, ev) = s.handle(&hello(1, 0));
        assert!(matches!(
            out[0].1,
            Message::Error {
                code: ErrorCode::UnsupportedVersion,
                ..
            }
        ));
        assert!(ev.is_none() && !s.accepted());
        s.handle(&hello(2, PROTOCOL_VERSION));
        let (out, _) = s.handle(&Envelope::new(
            3,
            Message::DisplayHint {
                width: 800,
                height: 600,
                scale_percent: 100,
            },
        ));
        assert!(matches!(
            out[0].1,
            Message::Error {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn channel_end_to_end_with_fake_agent() {
        let (host, guest) = tokio::io::duplex(4096);
        let mut host = Channel::new(host);
        let mut guest = Channel::new(guest);
        let agent = tokio::spawn(async move {
            let attach = guest.recv().await.unwrap();
            assert!(matches!(attach.msg, Message::Attach { .. }));
            guest
                .send(Message::Hello {
                    protocol: PROTOCOL_VERSION,
                    agent_version: "t".into(),
                    os_build: "b".into(),
                })
                .await
                .unwrap();
            let welcome = guest.recv().await.unwrap();
            assert_eq!(welcome.reply_to, Some(1));
            assert!(matches!(welcome.msg, Message::Welcome { .. }));
            let shares = guest.recv().await.unwrap();
            assert!(matches!(shares.msg, Message::ShareFolders { .. }));
            // A hostile frame: oversized length header.
            use tokio::io::AsyncWriteExt;
            guest
                .stream
                .write_all(&u32::MAX.to_le_bytes())
                .await
                .unwrap();
        });
        let mut session = ControlSession::new(shares());
        let mut events = Vec::new();
        let (_open_tx, mut open_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let r = serve_control(&mut host, &mut session, &mut open_rx, &mut cmd_rx, |e| {
            events.push(e)
        })
        .await;
        agent.await.unwrap();
        assert!(matches!(
            r,
            Err(ChannelError::Protocol(ProtoError::FrameTooLarge(_)))
        ));
        assert_eq!(events.len(), 1);
    }
}
