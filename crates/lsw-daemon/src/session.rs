//! Host end of the session channel, used by whichever LSW UI shows Windows
//! (the Window/Full Screen app now, the Coherence client in M2): clipboard
//! in both directions and display size hints.

use crate::channel::{Channel, ChannelError};
use lsw_proto::{
    ClipFormat, ClipboardAssembler, Envelope, Message, PROTOCOL_VERSION, clipboard_chunks,
    negotiate,
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

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

pub struct SessionClient<S> {
    channel: Channel<S>,
    attached: bool,
    /// Host clipboard content offered to the guest, by serial.
    host_serial: u64,
    host_items: Vec<(ClipFormat, Vec<u8>)>,
    assembler: ClipboardAssembler,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SessionClient<S> {
    /// Wrap a connected channel and send `Attach`.
    pub async fn new(channel: Channel<S>) -> Result<Self, ChannelError> {
        let mut c = Self {
            channel,
            attached: false,
            host_serial: 0,
            host_items: Vec::new(),
            assembler: ClipboardAssembler::default(),
        };
        c.attach().await?;
        Ok(c)
    }

    /// Ask the agent to introduce itself (on connect and whenever the guest
    /// reopens its port).
    pub async fn attach(&mut self) -> Result<(), ChannelError> {
        self.attached = false;
        self.channel
            .send(Message::Attach {
                protocol: PROTOCOL_VERSION,
            })
            .await
            .map(|_| ())
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
            self.channel
                .send(Message::ClipboardOffer {
                    serial: self.host_serial,
                    formats,
                })
                .await?;
        }
        Ok(())
    }

    /// Request the guest clipboard `serial` in `format`; the data arrives as
    /// [`SessionEvent::GuestClipboardData`].
    pub async fn fetch(&mut self, serial: u64, format: ClipFormat) -> Result<(), ChannelError> {
        self.channel
            .send(Message::ClipboardRequest { serial, format })
            .await
            .map(|_| ())
    }

    /// Resize the Windows display.
    pub async fn display_hint(
        &mut self,
        width: u32,
        height: u32,
        scale_percent: u16,
    ) -> Result<(), ChannelError> {
        self.channel
            .send(Message::DisplayHint {
                width,
                height,
                scale_percent,
            })
            .await
            .map(|_| ())
    }

    /// Process guest messages until something the UI cares about happens.
    /// Handshake replies, pings and clipboard requests are answered here.
    pub async fn next_event(&mut self) -> Result<SessionEvent, ChannelError> {
        loop {
            let env = self.channel.recv().await?;
            if let Some(event) = self.handle(env).await? {
                return Ok(event);
            }
        }
    }

    async fn handle(&mut self, env: Envelope) -> Result<Option<SessionEvent>, ChannelError> {
        match env.msg {
            Message::Hello {
                protocol,
                agent_version,
                ..
            } => match negotiate(protocol) {
                Ok(v) => {
                    self.channel
                        .reply(env.seq, Message::Welcome { protocol: v })
                        .await?;
                    self.attached = true;
                    // Bring Windows up to date with the host clipboard.
                    if !self.host_items.is_empty() {
                        let formats = self.host_items.iter().map(|(f, _)| *f).collect();
                        self.channel
                            .send(Message::ClipboardOffer {
                                serial: self.host_serial,
                                formats,
                            })
                            .await?;
                    }
                    Ok(Some(SessionEvent::Attached { agent_version }))
                }
                Err(_) => Ok(Some(SessionEvent::AgentError {
                    detail: format!("unsupported agent protocol {protocol}"),
                })),
            },
            _ if !self.attached => Ok(None),
            Message::Ping { nonce } => {
                self.channel.reply(env.seq, Message::Pong { nonce }).await?;
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
                    self.channel.reply(env.seq, frame).await?;
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
    SessionClient::new(Channel::connect(path, Duration::from_secs(5)).await?).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut s = SessionClient::new(Channel::new(host)).await.unwrap();
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
}
