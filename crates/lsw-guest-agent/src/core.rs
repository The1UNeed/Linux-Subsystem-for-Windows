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

/// What the LSW Tools installer should be run for, if anything: another
/// agent build ([`should_update`]), or the CD's build plus `+winfsp` when
/// WinFsp is missing and the CD carries it (a guest installed without it is
/// repaired). The result is recorded as the last attempt, so neither loops.
pub fn install_target(
    own_build: &str,
    cd_build: &str,
    last_attempt: Option<&str>,
    winfsp_missing: bool,
) -> Option<String> {
    let last = last_attempt.map(str::trim);
    if winfsp_missing {
        let target = format!("{}+winfsp", cd_build.trim());
        return (last != Some(target.as_str())).then_some(target);
    }
    should_update(own_build, cd_build, last_attempt).then(|| cd_build.trim().to_owned())
}

/// Milliseconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET_MS: i64 = 11_644_473_600_000;

/// Unix milliseconds → FILETIME (100 ns ticks since 1601).
pub fn unix_ms_to_filetime(ms: i64) -> u64 {
    ((ms + FILETIME_UNIX_OFFSET_MS) as u64) * 10_000
}

/// FILETIME → Unix milliseconds.
pub fn filetime_to_unix_ms(ft: u64) -> i64 {
    (ft / 10_000) as i64 - FILETIME_UNIX_OFFSET_MS
}

/// Whether the guest clock is far enough from the host's to be set (small
/// differences are transport delay, not drift).
pub fn clock_is_off(guest_ms: i64, host_ms: i64) -> bool {
    (guest_ms - host_ms).abs() > 2_000
}

/// Files the Windows shell drops into folders on its own: not user data.
pub fn is_shell_metadata(path: &std::path::Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n.eq_ignore_ascii_case("desktop.ini") || n.eq_ignore_ascii_case("thumbs.db")
    })
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
    /// Set the clock to `utc_ms` and, unless empty, the time zone to the
    /// IANA zone `time_zone`.
    fn set_clock(&mut self, utc_ms: i64, time_zone: &str) -> Result<(), String>;
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
            (PortId::Control, Message::Clock { utc_ms, time_zone }) => {
                match self.platform.set_clock(utc_ms, &time_zone) {
                    Ok(()) => Vec::new(),
                    Err(e) => vec![error(ErrorCode::Internal, &e)],
                }
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
        clocks: Vec<(i64, String)>,
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
        fn set_clock(&mut self, utc_ms: i64, time_zone: &str) -> Result<(), String> {
            self.clocks.push((utc_ms, time_zone.to_owned()));
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
    fn clock_conversions() {
        // 2026-11-13T00:00:00Z
        let ms = 1_794_528_000_000;
        assert_eq!(filetime_to_unix_ms(unix_ms_to_filetime(ms)), ms);
        // The Unix epoch is 116444736000000000 FILETIME ticks.
        assert_eq!(unix_ms_to_filetime(0), 116_444_736_000_000_000);
        assert!(!clock_is_off(ms, ms + 1_500), "transport delay");
        assert!(clock_is_off(ms, ms + 5 * 60_000), "a 5-minute pause");
        assert!(clock_is_off(ms + 36_000_000, ms), "10 h ahead");
    }

    #[test]
    fn shell_metadata_is_not_user_data() {
        use std::path::Path;
        assert!(is_shell_metadata(Path::new(
            "C:/Users/u/Linux/Desktop/desktop.ini"
        )));
        assert!(is_shell_metadata(Path::new("Desktop/Thumbs.db")));
        assert!(!is_shell_metadata(Path::new("Desktop/report.docx")));
        assert!(!is_shell_metadata(Path::new("Desktop/desktop.ini.bak")));
    }

    #[test]
    fn a_missing_winfsp_is_repaired_once() {
        assert_eq!(install_target("v+a", "v+a\n", None, false), None);
        assert_eq!(
            install_target("v+a", "v+a\n", None, true).as_deref(),
            Some("v+a+winfsp"),
            "same agent build, WinFsp missing"
        );
        assert_eq!(
            install_target("v+a", "v+a", Some("v+a+winfsp\r\n"), true),
            None,
            "the repair already failed once"
        );
        assert_eq!(
            install_target("v+a", "v+b", Some("v+a+winfsp"), true).as_deref(),
            Some("v+b+winfsp"),
            "a new CD gets a new attempt"
        );
        assert_eq!(
            install_target("v+a", "v+b", None, false).as_deref(),
            Some("v+b")
        );
    }

    #[test]
    fn the_host_clock_is_applied_from_the_control_port_only() {
        let mut a = Agent::new(Mock::default());
        attach(&mut a, PortId::Control);
        let clock = || Message::Clock {
            utc_ms: 1_794_600_000_000,
            time_zone: "Europe/Berlin".into(),
        };
        assert!(a.on_message(PortId::Control, env(9, clock())).is_empty());
        assert_eq!(
            a.platform.clocks,
            [(1_794_600_000_000, "Europe/Berlin".to_owned())]
        );
        attach(&mut a, PortId::Session);
        a.on_message(PortId::Session, env(10, clock()));
        assert_eq!(a.platform.clocks.len(), 1, "not from the session port");
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
