//! Platform-independent agent logic: what to answer to each host message.
//!
//! The Windows glue (`win/`) owns the ports, the clipboard window and the
//! system calls; it feeds events into [`Agent`] and sends what comes out.

#![forbid(unsafe_code)]

use lsw_proto::{
    AppEntry, ClipFormat, ClipboardAssembler, Envelope, ErrorCode, Message, PROTOCOL_VERSION,
    ShareSpec, clipboard_chunks,
};

/// Agent version reported in [`Message::Hello`].
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether to run the LSW Tools installer: the CD carries another build
/// than ours, and we have not already tried to install exactly that build
/// (so a failing installer cannot loop).
pub fn should_update(own_build: &str, cd_build: &str, last_attempt: Option<&str>) -> bool {
    let cd_build = cd_build.trim();
    !cd_build.is_empty() && cd_build != own_build && last_attempt.map(str::trim) != Some(cd_build)
}

/// The largest of `modes` that fits inside `width`×`height`.
pub fn best_fitting_mode(modes: &[(u32, u32)], width: u32, height: u32) -> Option<(u32, u32)> {
    modes
        .iter()
        .copied()
        .filter(|&(w, h)| w <= width && h <= height)
        .max_by_key(|&(w, h)| u64::from(w) * u64::from(h))
}

/// The two virtio-serial ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortId {
    /// Supervisor: lifecycle, shared folders.
    Control,
    /// UI: clipboard, display.
    Session,
}

impl PortId {
    fn index(self) -> usize {
        match self {
            PortId::Control => 0,
            PortId::Session => 1,
        }
    }
}

/// A frame to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    pub port: PortId,
    pub reply_to: Option<u64>,
    pub msg: Message,
}

/// The operating-system side, implemented for Windows in `win/` and mocked
/// in tests.
pub trait Platform {
    /// Windows build, e.g. `10.0.26300.9457`.
    fn os_build(&self) -> String;
    fn shutdown(&mut self, reboot: bool) -> Result<(), String>;
    /// Mount a virtiofs share and redirect its known folder; returns where
    /// it was mounted.
    fn mount_share(&mut self, share: &ShareSpec) -> Result<String, String>;
    fn set_display(&mut self, width: u32, height: u32, scale_percent: u16) -> Result<(), String>;
    /// Formats currently on the Windows clipboard that the wire supports.
    fn clipboard_formats(&mut self) -> Vec<ClipFormat>;
    fn clipboard_read(&mut self, format: ClipFormat) -> Option<Vec<u8>>;
    /// Replace the Windows clipboard with these representations.
    fn clipboard_write(&mut self, items: &[(ClipFormat, Vec<u8>)]) -> Result<(), String>;
    /// Installed apps, as in Start.
    fn list_apps(&mut self) -> Vec<AppEntry>;
    /// Start an app by its shell id.
    fn launch(&mut self, launch: &str) -> Result<(), String>;
}

/// Port state: the host attached, and accepted our Hello.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PortState {
    attached: bool,
    welcomed: bool,
}

/// A clipboard offer from the host that is being fetched.
#[derive(Debug, Default)]
struct Incoming {
    serial: u64,
    pending: Vec<ClipFormat>,
    received: Vec<(ClipFormat, Vec<u8>)>,
}

pub struct Agent<P: Platform> {
    pub platform: P,
    ports: [PortState; 2],
    /// Serial of our latest clipboard offer to the host.
    local_serial: u64,
    incoming: Option<Incoming>,
    assembler: ClipboardAssembler,
}

impl<P: Platform> Agent<P> {
    pub fn new(platform: P) -> Self {
        Self {
            platform,
            ports: [PortState::default(); 2],
            local_serial: 0,
            incoming: None,
            assembler: ClipboardAssembler::default(),
        }
    }

    /// Whether the host side of `port` is attached (safe to write).
    pub fn attached(&self, port: PortId) -> bool {
        self.ports[port.index()].attached
    }

    /// The port broke or a write timed out: stop writing until the host
    /// sends `Attach` again.
    pub fn port_lost(&mut self, port: PortId) {
        self.ports[port.index()] = PortState::default();
        if port == PortId::Session {
            self.incoming = None;
            self.assembler = ClipboardAssembler::default();
        }
    }

    /// The Windows clipboard changed (not by us): offer it to the host.
    pub fn local_clipboard_changed(&mut self) -> Vec<Out> {
        if !self.ports[PortId::Session.index()].welcomed {
            return Vec::new();
        }
        self.offer_clipboard()
    }

    fn offer_clipboard(&mut self) -> Vec<Out> {
        let formats = self.platform.clipboard_formats();
        if formats.is_empty() {
            return Vec::new();
        }
        self.local_serial += 1;
        vec![Out {
            port: PortId::Session,
            reply_to: None,
            msg: Message::ClipboardOffer {
                serial: self.local_serial,
                formats,
            },
        }]
    }

    /// Handle one validated host message.
    pub fn on_message(&mut self, port: PortId, env: Envelope) -> Vec<Out> {
        let reply = |msg| Out {
            port,
            reply_to: Some(env.seq),
            msg,
        };
        let error = |code, detail: &str| Out {
            port,
            reply_to: Some(env.seq),
            msg: Message::Error {
                code,
                detail: detail.to_owned(),
            },
        };
        let state = self.ports[port.index()];
        match env.msg {
            Message::Attach { .. } => {
                // A (re)connected host: introduce ourselves, forget old state.
                self.port_lost(port);
                self.ports[port.index()].attached = true;
                return vec![reply(Message::Hello {
                    protocol: PROTOCOL_VERSION,
                    agent_version: AGENT_VERSION.to_owned(),
                    os_build: self.platform.os_build(),
                })];
            }
            _ if !state.attached => return Vec::new(),
            Message::Welcome { .. } => {
                self.ports[port.index()].welcomed = true;
                // Bring a freshly attached UI up to date.
                if port == PortId::Session {
                    return self.offer_clipboard();
                }
                return Vec::new();
            }
            Message::Error { code, detail } => {
                if code == ErrorCode::UnsupportedVersion {
                    self.ports[port.index()].welcomed = false;
                }
                eprintln!("host error on {port:?}: {code:?}: {detail}");
                return Vec::new();
            }
            _ if !state.welcomed => {
                return vec![error(ErrorCode::BadRequest, "not welcomed yet")];
            }
            Message::Ping { nonce } => return vec![reply(Message::Pong { nonce })],
            Message::Pong { .. } => return Vec::new(),
            _ => {}
        }
        match (port, env.msg) {
            (PortId::Control, Message::Shutdown { reboot }) => {
                match self.platform.shutdown(reboot) {
                    Ok(()) => Vec::new(),
                    Err(e) => vec![error(ErrorCode::Internal, &e)],
                }
            }
            (PortId::Control, Message::ListApps) => {
                let mut out: Vec<Out> = self
                    .platform
                    .list_apps()
                    .into_iter()
                    .map(|app| reply(Message::App(app)))
                    .collect();
                out.push(reply(Message::AppsDone));
                out
            }
            (PortId::Control, Message::Launch { launch }) => match self.platform.launch(&launch) {
                Ok(()) => Vec::new(),
                Err(e) => vec![error(ErrorCode::Internal, &e)],
            },
            (PortId::Control, Message::ShareFolders { shares }) => shares
                .iter()
                .map(|share| {
                    let (ok, detail) = match self.platform.mount_share(share) {
                        Ok(path) => (true, path),
                        Err(e) => (false, e),
                    };
                    Out {
                        port,
                        reply_to: Some(env.seq),
                        msg: Message::ShareStatus {
                            tag: share.tag.clone(),
                            ok,
                            detail: truncate(detail),
                        },
                    }
                })
                .collect(),
            (
                PortId::Session,
                Message::DisplayHint {
                    width,
                    height,
                    scale_percent,
                },
            ) => match self.platform.set_display(width, height, scale_percent) {
                Ok(()) => Vec::new(),
                Err(e) => vec![error(ErrorCode::Internal, &e)],
            },
            (PortId::Session, Message::ClipboardOffer { serial, formats }) => {
                let mut wanted: Vec<ClipFormat> = Vec::new();
                for f in [ClipFormat::Text, ClipFormat::Html, ClipFormat::Png] {
                    if formats.contains(&f) {
                        wanted.push(f);
                    }
                }
                self.assembler = ClipboardAssembler::default();
                self.incoming = Some(Incoming {
                    serial,
                    pending: wanted.clone(),
                    received: Vec::new(),
                });
                wanted
                    .into_iter()
                    .map(|format| Out {
                        port,
                        reply_to: None,
                        msg: Message::ClipboardRequest { serial, format },
                    })
                    .collect()
            }
            (PortId::Session, Message::ClipboardRequest { serial, format }) => {
                if serial != self.local_serial {
                    return vec![error(ErrorCode::BadRequest, "stale clipboard serial")];
                }
                let data = self.platform.clipboard_read(format).unwrap_or_default();
                match clipboard_chunks(serial, format, &data) {
                    Ok(frames) => frames.into_iter().map(reply).collect(),
                    Err(_) => vec![error(ErrorCode::Internal, "clipboard content too large")],
                }
            }
            (
                PortId::Session,
                Message::ClipboardData {
                    serial,
                    format,
                    total,
                    offset,
                    chunk,
                },
            ) => {
                let done = match self.assembler.push(serial, format, total, offset, &chunk) {
                    Ok(done) => done,
                    Err(_) => return vec![error(ErrorCode::BadRequest, "bad clipboard chunk")],
                };
                let Some((serial, format, data)) = done else {
                    return Vec::new();
                };
                let Some(incoming) = self.incoming.as_mut().filter(|i| i.serial == serial) else {
                    return Vec::new(); // superseded offer
                };
                incoming.pending.retain(|f| *f != format);
                if !data.is_empty() {
                    incoming.received.push((format, data));
                }
                if incoming.pending.is_empty() {
                    let items = std::mem::take(&mut incoming.received);
                    self.incoming = None;
                    if !items.is_empty()
                        && let Err(e) = self.platform.clipboard_write(&items)
                    {
                        return vec![error(ErrorCode::Internal, &e)];
                    }
                }
                Vec::new()
            }
            _ => vec![error(
                ErrorCode::BadRequest,
                "message not valid on this port",
            )],
        }
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > lsw_proto::MAX_STRING_LEN {
        let mut cut = lsw_proto::MAX_STRING_LEN;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsw_proto::KnownFolder;

    #[derive(Default)]
    struct Mock {
        clipboard: Vec<(ClipFormat, Vec<u8>)>,
        shutdowns: Vec<bool>,
        displays: Vec<(u32, u32, u16)>,
        mounts: Vec<String>,
        launched: Vec<String>,
    }

    impl Platform for Mock {
        fn os_build(&self) -> String {
            "10.0.26300.1".into()
        }
        fn shutdown(&mut self, reboot: bool) -> Result<(), String> {
            self.shutdowns.push(reboot);
            Ok(())
        }
        fn mount_share(&mut self, share: &ShareSpec) -> Result<String, String> {
            if share.tag == "lsw-broken" {
                return Err("no such device".into());
            }
            self.mounts.push(share.tag.clone());
            Ok(format!("C:\\Users\\u\\Linux\\{:?}", share.folder))
        }
        fn set_display(&mut self, w: u32, h: u32, s: u16) -> Result<(), String> {
            self.displays.push((w, h, s));
            Ok(())
        }
        fn clipboard_formats(&mut self) -> Vec<ClipFormat> {
            self.clipboard.iter().map(|(f, _)| *f).collect()
        }
        fn clipboard_read(&mut self, format: ClipFormat) -> Option<Vec<u8>> {
            self.clipboard
                .iter()
                .find(|(f, _)| *f == format)
                .map(|(_, d)| d.clone())
        }
        fn clipboard_write(&mut self, items: &[(ClipFormat, Vec<u8>)]) -> Result<(), String> {
            self.clipboard = items.to_vec();
            Ok(())
        }
        fn list_apps(&mut self) -> Vec<AppEntry> {
            vec![AppEntry {
                launch: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into(),
                name: "Calculator".into(),
                app: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into(),
                icon_png: vec![],
            }]
        }
        fn launch(&mut self, launch: &str) -> Result<(), String> {
            self.launched.push(launch.to_owned());
            Ok(())
        }
    }

    fn env(seq: u64, msg: Message) -> Envelope {
        Envelope::new(seq, msg)
    }

    fn attach(agent: &mut Agent<Mock>, port: PortId) {
        let out = agent.on_message(port, env(1, Message::Attach { protocol: 1 }));
        assert!(matches!(out[0].msg, Message::Hello { .. }));
        assert_eq!(out[0].reply_to, Some(1));
        agent.on_message(port, env(2, Message::Welcome { protocol: 1 }));
    }

    #[test]
    fn best_fitting_mode_picks_largest_that_fits() {
        let modes = [(1024, 768), (1280, 800), (1600, 900), (1920, 1080)];
        assert_eq!(best_fitting_mode(&modes, 1600, 1000), Some((1600, 900)));
        assert_eq!(best_fitting_mode(&modes, 1300, 2000), Some((1280, 800)));
        assert_eq!(best_fitting_mode(&modes, 800, 600), None);
    }

    #[test]
    fn update_decision() {
        assert!(should_update("v+a", "v+b\r\n", None));
        assert!(!should_update("v+a", "v+a", None), "same build");
        assert!(!should_update("v+a", "v+b", Some("v+b\n")), "already tried");
        assert!(
            should_update("v+a", "v+c", Some("v+b")),
            "newer than the failed one"
        );
        assert!(!should_update("v+a", "  ", None), "empty file");
    }

    #[test]
    fn silent_until_attached() {
        let mut a = Agent::new(Mock::default());
        assert!(
            a.on_message(PortId::Control, env(1, Message::Ping { nonce: 1 }))
                .is_empty()
        );
        a.platform.clipboard = vec![(ClipFormat::Text, b"x".to_vec())];
        assert!(a.local_clipboard_changed().is_empty());
        assert!(!a.attached(PortId::Session));
    }

    #[test]
    fn control_port_shares_shutdown_ping() {
        let mut a = Agent::new(Mock::default());
        attach(&mut a, PortId::Control);
        let out = a.on_message(
            PortId::Control,
            env(
                5,
                Message::ShareFolders {
                    shares: vec![
                        ShareSpec {
                            tag: "lsw-documents".into(),
                            folder: KnownFolder::Documents,
                        },
                        ShareSpec {
                            tag: "lsw-broken".into(),
                            folder: KnownFolder::Music,
                        },
                    ],
                },
            ),
        );
        assert_eq!(out.len(), 2);
        assert!(
            matches!(&out[0].msg, Message::ShareStatus { ok: true, detail, .. } if detail.ends_with("Documents"))
        );
        assert!(matches!(
            &out[1].msg,
            Message::ShareStatus { ok: false, .. }
        ));
        a.on_message(PortId::Control, env(6, Message::Shutdown { reboot: true }));
        assert_eq!(a.platform.shutdowns, [true]);
        let out = a.on_message(PortId::Control, env(7, Message::Ping { nonce: 4 }));
        assert_eq!(out[0].msg, Message::Pong { nonce: 4 });
        // Session messages are refused on the control port.
        let out = a.on_message(
            PortId::Control,
            env(
                8,
                Message::DisplayHint {
                    width: 800,
                    height: 600,
                    scale_percent: 100,
                },
            ),
        );
        assert!(matches!(out[0].msg, Message::Error { .. }));
        assert!(a.platform.displays.is_empty());
    }

    #[test]
    fn apps_are_listed_and_launched_on_the_control_port() {
        let mut a = Agent::new(Mock::default());
        attach(&mut a, PortId::Control);
        let out = a.on_message(PortId::Control, env(5, Message::ListApps));
        assert!(matches!(&out[0].msg, Message::App(app) if app.name == "Calculator"));
        assert_eq!(out.last().unwrap().msg, Message::AppsDone);
        assert!(out.iter().all(|o| o.reply_to == Some(5)));
        a.on_message(
            PortId::Control,
            env(
                6,
                Message::Launch {
                    launch: "x!App".into(),
                },
            ),
        );
        assert_eq!(a.platform.launched, ["x!App"]);
        // Not on the session port.
        attach(&mut a, PortId::Session);
        let out = a.on_message(PortId::Session, env(7, Message::ListApps));
        assert!(matches!(out[0].msg, Message::Error { .. }));
    }

    #[test]
    fn guest_to_host_clipboard() {
        let mut a = Agent::new(Mock::default());
        a.platform.clipboard = vec![(ClipFormat::Text, b"from windows".to_vec())];
        // Welcome on the session port offers the current clipboard.
        a.on_message(PortId::Session, env(1, Message::Attach { protocol: 1 }));
        let out = a.on_message(PortId::Session, env(2, Message::Welcome { protocol: 1 }));
        let Message::ClipboardOffer {
            serial,
            ref formats,
        } = out[0].msg
        else {
            panic!("{out:?}")
        };
        assert_eq!(formats, &[ClipFormat::Text]);
        let out = a.on_message(
            PortId::Session,
            env(
                3,
                Message::ClipboardRequest {
                    serial,
                    format: ClipFormat::Text,
                },
            ),
        );
        assert_eq!(out[0].reply_to, Some(3));
        assert!(
            matches!(&out[0].msg, Message::ClipboardData { chunk, .. } if chunk == b"from windows")
        );
        // A stale serial is refused.
        let out = a.on_message(
            PortId::Session,
            env(
                4,
                Message::ClipboardRequest {
                    serial: serial + 9,
                    format: ClipFormat::Text,
                },
            ),
        );
        assert!(matches!(out[0].msg, Message::Error { .. }));
    }

    #[test]
    fn host_to_guest_clipboard_waits_for_all_formats() {
        let mut a = Agent::new(Mock::default());
        attach(&mut a, PortId::Session);
        let out = a.on_message(
            PortId::Session,
            env(
                3,
                Message::ClipboardOffer {
                    serial: 11,
                    formats: vec![ClipFormat::Html, ClipFormat::Text],
                },
            ),
        );
        // Requested in preference order.
        let requested: Vec<_> = out
            .iter()
            .map(|o| match o.msg {
                Message::ClipboardRequest { format, .. } => format,
                _ => panic!(),
            })
            .collect();
        assert_eq!(requested, [ClipFormat::Text, ClipFormat::Html]);
        let data = |seq, format, bytes: &[u8]| {
            env(
                seq,
                Message::ClipboardData {
                    serial: 11,
                    format,
                    total: bytes.len() as u64,
                    offset: 0,
                    chunk: bytes.to_vec(),
                },
            )
        };
        a.on_message(PortId::Session, data(4, ClipFormat::Text, b"hi"));
        assert!(a.platform.clipboard.is_empty(), "still waiting for HTML");
        a.on_message(PortId::Session, data(5, ClipFormat::Html, b"<b>hi</b>"));
        assert_eq!(
            a.platform.clipboard,
            [
                (ClipFormat::Text, b"hi".to_vec()),
                (ClipFormat::Html, b"<b>hi</b>".to_vec())
            ]
        );
    }

    #[test]
    fn reattach_resets_state() {
        let mut a = Agent::new(Mock::default());
        attach(&mut a, PortId::Session);
        a.port_lost(PortId::Session);
        assert!(!a.attached(PortId::Session));
        let out = a.on_message(
            PortId::Session,
            env(
                9,
                Message::DisplayHint {
                    width: 1920,
                    height: 1080,
                    scale_percent: 100,
                },
            ),
        );
        assert!(out.is_empty() && a.platform.displays.is_empty());
        attach(&mut a, PortId::Session);
        a.on_message(
            PortId::Session,
            env(
                10,
                Message::DisplayHint {
                    width: 1920,
                    height: 1080,
                    scale_percent: 100,
                },
            ),
        );
        assert_eq!(a.platform.displays, [(1920, 1080, 100)]);
    }
}
