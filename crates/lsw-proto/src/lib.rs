//! LSW guest/host control protocol.
//!
//! # Wire format
//!
//! Every frame is a little-endian `u32` payload length followed by a
//! [postcard]-encoded [`Envelope`]. Frames larger than [`MAX_FRAME_LEN`] are
//! rejected before any allocation happens.
//!
//! # Trust
//!
//! The host treats the guest as untrusted. [`FrameDecoder`] therefore:
//!
//! * bounds the frame length before buffering it,
//! * rejects trailing bytes after a decoded envelope,
//! * validates every string and collection length with [`Envelope::validate`],
//! * poisons itself after the first error so a confused peer cannot resync
//!   into a half-parsed stream; the caller must drop the connection.
//!
//! # Handshake
//!
//! The host speaks first: whenever a host endpoint (re)connects to a port it
//! sends [`Message::Attach`]. The guest answers with [`Message::Hello`] and
//! the host accepts with [`Message::Welcome`]. Until it has seen `Attach`
//! the guest never writes to a port, so it cannot block on a port nobody is
//! reading.
//!
//! # Compatibility
//!
//! Variants are only ever appended to [`Message`], never reordered or removed.
//! Peers exchange [`PROTOCOL_VERSION`] in [`Message::Hello`] /
//! [`Message::Welcome`] and the host refuses incompatible agents.

use serde::{Deserialize, Serialize};

pub mod coherence;

/// Current protocol version. Bumped whenever [`Message`] gains a variant.
pub const PROTOCOL_VERSION: u16 = 1;

/// Oldest agent protocol the host still speaks.
pub const MIN_SUPPORTED_VERSION: u16 = 1;

/// Largest accepted frame payload (1 MiB). Bulk data (pixels, files) never
/// travels on the control channel.
pub const MAX_FRAME_LEN: u32 = 1 << 20;

/// Largest accepted string field.
pub const MAX_STRING_LEN: usize = 4096;

/// virtio-serial port of the control channel (host side: the VM
/// supervisor). The guest opens it as `\\.\Global\org.lsw.control.0`.
/// Carries lifecycle and shared-folder messages.
pub const CONTROL_PORT_NAME: &str = "org.lsw.control.0";

/// virtio-serial port of the session channel (host side: whichever LSW UI
/// is showing Windows). Carries clipboard and display messages: on Wayland
/// only a focused client may own the clipboard, so the UI process speaks
/// for the host desktop directly instead of through the supervisor.
pub const SESSION_PORT_NAME: &str = "org.lsw.session.0";

/// Largest data chunk in one [`Message::ClipboardData`] frame.
pub const MAX_CLIPBOARD_CHUNK: usize = 512 * 1024;

/// Largest clipboard payload either side accepts (sum of all chunks).
pub const MAX_CLIPBOARD_TOTAL: u64 = 32 << 20;

/// Most entries in a list field (clipboard formats, shares).
pub const MAX_LIST_LEN: usize = 32;

/// Longest share tag (virtiofs tags are at most 36 bytes).
pub const MAX_TAG_LEN: usize = 36;

/// Largest app icon (PNG) in an [`Message::App`] entry.
pub const MAX_ICON_LEN: usize = 64 * 1024;

/// Length of the frame header in bytes.
pub const HEADER_LEN: usize = 4;

/// One protocol frame.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Sender-assigned sequence number; replies echo it in `reply_to`.
    pub seq: u64,
    /// Sequence number of the request this frame answers, if any.
    pub reply_to: Option<u64>,
    pub msg: Message,
}

/// Messages exchanged on the control channel.
///
/// Append-only: new variants go at the end.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Guest → host, first frame after connecting.
    Hello {
        protocol: u16,
        agent_version: String,
        /// Windows build string, e.g. `10.0.26300.9457`.
        os_build: String,
    },
    /// Host → guest, accepts the agent.
    Welcome { protocol: u16 },
    /// Either direction, liveness check.
    Ping { nonce: u64 },
    /// Answer to [`Message::Ping`].
    Pong { nonce: u64 },
    /// Either direction, a request failed.
    Error { code: ErrorCode, detail: String },
    /// Host → guest, shut down or reboot Windows cleanly.
    Shutdown { reboot: bool },
    /// Either direction: the sender's clipboard changed and now holds these
    /// formats. Nothing is transferred until the peer asks.
    ClipboardOffer {
        serial: u64,
        formats: Vec<ClipFormat>,
    },
    /// Either direction: send the clipboard content `serial` in `format`.
    ClipboardRequest { serial: u64, format: ClipFormat },
    /// Answer to [`Message::ClipboardRequest`], split into chunks of at most
    /// [`MAX_CLIPBOARD_CHUNK`] bytes sent in order; `total` is the full size.
    ClipboardData {
        serial: u64,
        format: ClipFormat,
        total: u64,
        offset: u64,
        chunk: Vec<u8>,
    },
    /// Host → guest: the host view is this size; resize the Windows display.
    DisplayHint {
        width: u32,
        height: u32,
        /// Windows scale factor in percent (100, 125, 150, …).
        scale_percent: u16,
    },
    /// Host → guest: mount these virtiofs shares and point the matching
    /// Windows known folders at them. Replaces any earlier list.
    ShareFolders { shares: Vec<ShareSpec> },
    /// Guest → host: result for one share from [`Message::ShareFolders`].
    ShareStatus {
        tag: String,
        ok: bool,
        detail: String,
    },
    /// Host → guest, first frame after a host endpoint connects: the guest
    /// answers with [`Message::Hello`].
    Attach { protocol: u16 },
    /// Host → guest: list the installed apps (answered with one
    /// [`Message::App`] per app, then [`Message::AppsDone`]).
    ListApps,
    /// Guest → host: one installed app.
    App(AppEntry),
    /// Guest → host: end of the app list.
    AppsDone,
    /// Host → guest: start an app by its [`AppEntry::launch`] id.
    Launch { launch: String },
}

/// An installed Windows app, as listed in Start.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    /// Shell id used to start it (`shell:AppsFolder\<launch>`).
    pub launch: String,
    /// Display name.
    pub name: String,
    /// Identity shared with its windows ([`coherence::WindowInfo::app`]):
    /// the AppUserModelID, else the executable file name.
    pub app: String,
    /// 64×64 PNG icon (may be empty).
    pub icon_png: Vec<u8>,
}

/// Clipboard formats carried between the hosts.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipFormat {
    /// UTF-8 text.
    Text,
    /// An HTML fragment (UTF-8).
    Html,
    /// A PNG image.
    Png,
}

/// Windows known folders that can be paired with a Linux folder.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownFolder {
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
}

/// One shared folder: the virtiofs `tag` and the known folder it backs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ShareSpec {
    pub tag: String,
    pub folder: KnownFolder,
}

/// Error codes carried by [`Message::Error`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    UnsupportedVersion,
    BadRequest,
    Internal,
}

/// Errors produced while encoding or decoding frames.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("frame of {0} bytes exceeds the channel's limit")]
    FrameTooLarge(u32),
    #[error("malformed frame payload")]
    Malformed,
    #[error("{0} trailing bytes after the envelope")]
    TrailingBytes(usize),
    #[error("field `{field}` is {len} long, limit {limit}")]
    FieldTooLong {
        field: &'static str,
        len: usize,
        limit: usize,
    },
    #[error("decoder is poisoned by an earlier error; drop the connection")]
    Poisoned,
    #[error("numeric field out of range")]
    BadRange,
    #[error("invalid share tag")]
    BadTag,
}

impl Envelope {
    /// Build a frame that is not a reply.
    pub fn new(seq: u64, msg: Message) -> Self {
        Self {
            seq,
            reply_to: None,
            msg,
        }
    }

    /// Build a reply to `request_seq`.
    pub fn reply(seq: u64, request_seq: u64, msg: Message) -> Self {
        Self {
            seq,
            reply_to: Some(request_seq),
            msg,
        }
    }

    /// Check every bounded field. Called by [`FrameDecoder`] on every frame
    /// and by [`encode`] so a buggy sender fails loudly on its own side.
    pub fn validate(&self) -> Result<(), ProtoError> {
        fn bounded(field: &'static str, s: &str) -> Result<(), ProtoError> {
            if s.len() > MAX_STRING_LEN {
                return Err(ProtoError::FieldTooLong {
                    field,
                    len: s.len(),
                    limit: MAX_STRING_LEN,
                });
            }
            Ok(())
        }
        fn list(field: &'static str, len: usize) -> Result<(), ProtoError> {
            if len > MAX_LIST_LEN {
                return Err(ProtoError::FieldTooLong {
                    field,
                    len,
                    limit: MAX_LIST_LEN,
                });
            }
            Ok(())
        }
        fn tag(t: &str) -> Result<(), ProtoError> {
            if is_valid_tag(t) {
                Ok(())
            } else {
                Err(ProtoError::BadTag)
            }
        }
        match &self.msg {
            Message::Hello {
                agent_version,
                os_build,
                ..
            } => {
                bounded("agent_version", agent_version)?;
                bounded("os_build", os_build)
            }
            Message::Error { detail, .. } => bounded("detail", detail),
            Message::ClipboardOffer { formats, .. } => list("formats", formats.len()),
            Message::ClipboardData {
                total,
                offset,
                chunk,
                ..
            } => {
                if chunk.len() > MAX_CLIPBOARD_CHUNK {
                    return Err(ProtoError::FieldTooLong {
                        field: "chunk",
                        len: chunk.len(),
                        limit: MAX_CLIPBOARD_CHUNK,
                    });
                }
                let end = offset.checked_add(chunk.len() as u64);
                if *total > MAX_CLIPBOARD_TOTAL || end.is_none_or(|e| e > *total) {
                    return Err(ProtoError::BadRange);
                }
                Ok(())
            }
            Message::ShareFolders { shares } => {
                list("shares", shares.len())?;
                shares.iter().try_for_each(|s| tag(&s.tag))
            }
            Message::ShareStatus { tag: t, detail, .. } => {
                tag(t)?;
                bounded("detail", detail)
            }
            Message::DisplayHint {
                width,
                height,
                scale_percent,
            } => {
                let ok = (320..=16384).contains(width)
                    && (200..=16384).contains(height)
                    && (100..=500).contains(scale_percent);
                if ok {
                    Ok(())
                } else {
                    Err(ProtoError::BadRange)
                }
            }
            Message::App(app) => {
                bounded("launch", &app.launch)?;
                bounded("name", &app.name)?;
                bounded("app", &app.app)?;
                if app.icon_png.len() > MAX_ICON_LEN {
                    return Err(ProtoError::FieldTooLong {
                        field: "icon_png",
                        len: app.icon_png.len(),
                        limit: MAX_ICON_LEN,
                    });
                }
                Ok(())
            }
            Message::Launch { launch } => bounded("launch", launch),
            Message::ListApps | Message::AppsDone => Ok(()),
            Message::Welcome { .. }
            | Message::Ping { .. }
            | Message::Pong { .. }
            | Message::Shutdown { .. }
            | Message::ClipboardRequest { .. }
            | Message::Attach { .. } => Ok(()),
        }
    }
}

/// The Linux identity of a Windows app: `lsw.<slug>` where the slug keeps
/// `[a-z0-9]` and turns everything else into `-`. Used as the Wayland
/// `app_id` of its windows and the `StartupWMClass` of its launcher, so
/// docks group them.
pub fn linux_app_id(app: &str) -> String {
    let slug: String = app
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(64).collect();
    format!("lsw.{}", if slug.is_empty() { "windows" } else { &slug })
}

/// App identity from an AppUserModelID or an executable path.
pub fn app_identity(aumid: Option<&str>, exe: &str) -> String {
    match aumid.map(str::trim).filter(|a| !a.is_empty()) {
        Some(a) => a.to_owned(),
        None => exe
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase(),
    }
}

/// Whether `tag` is a valid share tag: 1–[`MAX_TAG_LEN`] bytes of
/// `[a-z0-9-]`, so it is safe in QEMU options, file names and Windows paths.
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_TAG_LEN
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A message type that travels in length-prefixed postcard frames.
pub trait Wire: Serialize + for<'de> Deserialize<'de> {
    /// Largest accepted payload for this channel.
    const MAX_LEN: u32;
    /// Check every bounded field (called on encode and decode).
    fn validate(&self) -> Result<(), ProtoError>;
}

impl Wire for Envelope {
    const MAX_LEN: u32 = MAX_FRAME_LEN;
    fn validate(&self) -> Result<(), ProtoError> {
        Envelope::validate(self)
    }
}

/// Encode one message into a complete frame (header + payload).
pub fn encode_message<T: Wire>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    msg.validate()?;
    let mut out =
        postcard::to_extend(msg, vec![0u8; HEADER_LEN]).map_err(|_| ProtoError::Malformed)?;
    let payload_len = out.len() - HEADER_LEN;
    let len = u32::try_from(payload_len).map_err(|_| ProtoError::FrameTooLarge(u32::MAX))?;
    if len > T::MAX_LEN {
        return Err(ProtoError::FrameTooLarge(len));
    }
    out[..HEADER_LEN].copy_from_slice(&len.to_le_bytes());
    Ok(out)
}

/// Encode one control/session envelope.
pub fn encode(env: &Envelope) -> Result<Vec<u8>, ProtoError> {
    encode_message(env)
}

/// Decode a single complete payload (no header).
pub fn decode_message<T: Wire>(payload: &[u8]) -> Result<T, ProtoError> {
    let (msg, rest): (T, &[u8]) =
        postcard::take_from_bytes(payload).map_err(|_| ProtoError::Malformed)?;
    if !rest.is_empty() {
        return Err(ProtoError::TrailingBytes(rest.len()));
    }
    msg.validate()?;
    Ok(msg)
}

/// Decode a single control/session payload (no header).
pub fn decode_payload(payload: &[u8]) -> Result<Envelope, ProtoError> {
    decode_message(payload)
}

/// Incremental decoder for a byte stream of frames of `T`.
#[derive(Debug)]
pub struct Decoder<T> {
    buf: Vec<u8>,
    poisoned: bool,
    _msg: std::marker::PhantomData<fn() -> T>,
}

/// Decoder for the control and session channels.
pub type FrameDecoder = Decoder<Envelope>;

impl<T: Wire> Default for Decoder<T> {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            poisoned: false,
            _msg: std::marker::PhantomData,
        }
    }
}

impl<T: Wire> Decoder<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes read from the transport.
    ///
    /// Once a header announces an oversized frame the decoder poisons
    /// itself instead of buffering it.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtoError> {
        if self.poisoned {
            return Err(ProtoError::Poisoned);
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= HEADER_LEN {
            let len = self.peek_len();
            if len > T::MAX_LEN {
                self.poisoned = true;
                self.buf.clear();
                return Err(ProtoError::FrameTooLarge(len));
            }
        }
        Ok(())
    }

    /// Pop the next complete message, if one is buffered.
    pub fn next_message(&mut self) -> Result<Option<T>, ProtoError> {
        if self.poisoned {
            return Err(ProtoError::Poisoned);
        }
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = self.peek_len();
        if len > T::MAX_LEN {
            self.poisoned = true;
            self.buf.clear();
            return Err(ProtoError::FrameTooLarge(len));
        }
        let total = HEADER_LEN + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let result = decode_message(&self.buf[HEADER_LEN..total]);
        self.buf.drain(..total);
        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(e) => {
                self.poisoned = true;
                self.buf.clear();
                Err(e)
            }
        }
    }

    /// Bytes currently buffered (for tests and metrics).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    fn peek_len(&self) -> u32 {
        u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
    }
}

impl Decoder<Envelope> {
    /// Pop the next complete envelope, if one is buffered.
    pub fn next_envelope(&mut self) -> Result<Option<Envelope>, ProtoError> {
        self.next_message()
    }
}

/// Identity of a guest agent build: `<version>+<first 16 hex digits of the
/// executable's SHA-256>`. Written to the LSW Tools CD by the host and
/// computed by the agent over its own executable; a mismatch triggers the
/// agent's self-update.
pub fn agent_build_id(version: &str, exe: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(exe);
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{version}+{hex}")
}

/// Split clipboard content into [`Message::ClipboardData`] frames.
pub fn clipboard_chunks(
    serial: u64,
    format: ClipFormat,
    data: &[u8],
) -> Result<Vec<Message>, ProtoError> {
    let total = data.len() as u64;
    if total > MAX_CLIPBOARD_TOTAL {
        return Err(ProtoError::BadRange);
    }
    if data.is_empty() {
        return Ok(vec![Message::ClipboardData {
            serial,
            format,
            total: 0,
            offset: 0,
            chunk: Vec::new(),
        }]);
    }
    Ok(data
        .chunks(MAX_CLIPBOARD_CHUNK)
        .enumerate()
        .map(|(i, c)| Message::ClipboardData {
            serial,
            format,
            total,
            offset: (i * MAX_CLIPBOARD_CHUNK) as u64,
            chunk: c.to_vec(),
        })
        .collect())
}

/// Reassembles one clipboard transfer from [`Message::ClipboardData`]
/// chunks. Chunks must arrive in order and agree on serial, format and
/// total; anything else is an error and the transfer is dropped.
#[derive(Debug, Default)]
pub struct ClipboardAssembler {
    current: Option<(u64, ClipFormat, u64, Vec<u8>)>,
}

impl ClipboardAssembler {
    /// Feed one chunk. Returns the complete content once the last chunk
    /// arrived. The chunk itself was already validated by the decoder.
    pub fn push(
        &mut self,
        serial: u64,
        format: ClipFormat,
        total: u64,
        offset: u64,
        chunk: &[u8],
    ) -> Result<Option<(u64, ClipFormat, Vec<u8>)>, ProtoError> {
        if offset == 0 {
            // A new transfer replaces any unfinished one. `total` is bounded
            // by the decoder, so this allocation is too.
            self.current = Some((serial, format, total, Vec::with_capacity(total as usize)));
        }
        let Some((s, f, t, buf)) = self.current.as_mut() else {
            return Err(ProtoError::BadRange);
        };
        if *s != serial || *f != format || *t != total || buf.len() as u64 != offset {
            self.current = None;
            return Err(ProtoError::BadRange);
        }
        buf.extend_from_slice(chunk);
        if buf.len() as u64 == *t {
            let (s, f, _, buf) = self.current.take().expect("present");
            return Ok(Some((s, f, buf)));
        }
        Ok(None)
    }
}

/// Version negotiation on the host side: accept `agent` if it is inside the
/// supported window and answer with the version both sides will speak.
pub fn negotiate(agent: u16) -> Result<u16, ProtoError> {
    if agent < MIN_SUPPORTED_VERSION {
        return Err(ProtoError::Malformed);
    }
    Ok(agent.min(PROTOCOL_VERSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Envelope {
        Envelope::new(
            1,
            Message::Hello {
                protocol: PROTOCOL_VERSION,
                agent_version: "0.0.1-alpha".into(),
                os_build: "10.0.26300.9457".into(),
            },
        )
    }

    #[test]
    fn roundtrip_single_frame() {
        let frame = encode(&hello()).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&frame).unwrap();
        assert_eq!(d.next_envelope().unwrap(), Some(hello()));
        assert_eq!(d.next_envelope().unwrap(), None);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn byte_at_a_time_and_back_to_back() {
        let mut stream = encode(&hello()).unwrap();
        stream.extend(encode(&Envelope::reply(2, 1, Message::Welcome { protocol: 1 })).unwrap());
        let mut d = FrameDecoder::new();
        let mut got = Vec::new();
        for b in stream {
            d.push(&[b]).unwrap();
            while let Some(env) = d.next_envelope().unwrap() {
                got.push(env);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].reply_to, Some(1));
    }

    #[test]
    fn oversized_header_poisons_without_buffering() {
        let mut d = FrameDecoder::new();
        let err = d.push(&(MAX_FRAME_LEN + 1).to_le_bytes()).unwrap_err();
        assert_eq!(err, ProtoError::FrameTooLarge(MAX_FRAME_LEN + 1));
        assert_eq!(d.buffered(), 0);
        assert_eq!(d.push(&[0]), Err(ProtoError::Poisoned));
        assert_eq!(d.next_envelope(), Err(ProtoError::Poisoned));
    }

    #[test]
    fn garbage_payload_poisons() {
        let mut d = FrameDecoder::new();
        d.push(&3u32.to_le_bytes()).unwrap();
        d.push(&[0xff, 0xff, 0xff]).unwrap();
        assert_eq!(d.next_envelope(), Err(ProtoError::Malformed));
        assert_eq!(d.next_envelope(), Err(ProtoError::Poisoned));
    }

    #[test]
    fn trailing_bytes_rejected() {
        let frame = encode(&Envelope::new(9, Message::Ping { nonce: 7 })).unwrap();
        let mut payload = frame[HEADER_LEN..].to_vec();
        payload.push(0);
        assert_eq!(decode_payload(&payload), Err(ProtoError::TrailingBytes(1)));
    }

    #[test]
    fn long_strings_rejected_on_both_sides() {
        let env = Envelope::new(
            1,
            Message::Error {
                code: ErrorCode::Internal,
                detail: "x".repeat(MAX_STRING_LEN + 1),
            },
        );
        assert!(matches!(encode(&env), Err(ProtoError::FieldTooLong { .. })));
        // A malicious peer bypassing `encode` is caught by the decoder too.
        let mut raw = vec![0u8; HEADER_LEN];
        raw = postcard::to_extend(&env, raw).unwrap();
        let len = (raw.len() - HEADER_LEN) as u32;
        raw[..HEADER_LEN].copy_from_slice(&len.to_le_bytes());
        let mut d = FrameDecoder::new();
        d.push(&raw).unwrap();
        assert!(matches!(
            d.next_envelope(),
            Err(ProtoError::FieldTooLong { .. })
        ));
    }

    fn roundtrip(msg: Message) -> Result<Envelope, ProtoError> {
        let frame = encode(&Envelope::new(1, msg))?;
        decode_payload(&frame[HEADER_LEN..])
    }

    #[test]
    fn m1_messages_roundtrip() {
        let msgs = [
            Message::ClipboardOffer {
                serial: 3,
                formats: vec![ClipFormat::Text, ClipFormat::Png],
            },
            Message::ClipboardRequest {
                serial: 3,
                format: ClipFormat::Html,
            },
            Message::ClipboardData {
                serial: 3,
                format: ClipFormat::Text,
                total: 10,
                offset: 5,
                chunk: b"world".to_vec(),
            },
            Message::DisplayHint {
                width: 2560,
                height: 1440,
                scale_percent: 150,
            },
            Message::ShareFolders {
                shares: vec![ShareSpec {
                    tag: "lsw-documents".into(),
                    folder: KnownFolder::Documents,
                }],
            },
            Message::ShareStatus {
                tag: "lsw-documents".into(),
                ok: true,
                detail: String::new(),
            },
        ];
        for m in msgs {
            assert_eq!(roundtrip(m.clone()).unwrap().msg, m);
        }
    }

    #[test]
    fn clipboard_ranges_are_checked() {
        let data = |total, offset, len| Message::ClipboardData {
            serial: 1,
            format: ClipFormat::Png,
            total,
            offset,
            chunk: vec![0; len],
        };
        assert_eq!(roundtrip(data(4, 3, 2)), Err(ProtoError::BadRange));
        assert_eq!(
            roundtrip(data(u64::MAX, u64::MAX, 1)),
            Err(ProtoError::BadRange)
        );
        assert_eq!(
            roundtrip(data(MAX_CLIPBOARD_TOTAL + 1, 0, 1)),
            Err(ProtoError::BadRange)
        );
        assert!(matches!(
            roundtrip(data(1 << 20, 0, MAX_CLIPBOARD_CHUNK + 1)),
            Err(ProtoError::FieldTooLong { .. })
        ));
        // A full-size chunk still fits in one frame.
        assert!(roundtrip(data(1 << 20, 0, MAX_CLIPBOARD_CHUNK)).is_ok());
    }

    #[test]
    fn share_tags_and_display_hints_are_checked() {
        let share = |tag: &str| Message::ShareFolders {
            shares: vec![ShareSpec {
                tag: tag.into(),
                folder: KnownFolder::Music,
            }],
        };
        assert!(roundtrip(share("lsw-music")).is_ok());
        for bad in ["", "Upper", "a/b", "..", &"x".repeat(37)] {
            assert_eq!(roundtrip(share(bad)), Err(ProtoError::BadTag), "{bad:?}");
        }
        let hint = |w, h, s| Message::DisplayHint {
            width: w,
            height: h,
            scale_percent: s,
        };
        assert!(roundtrip(hint(1920, 1080, 100)).is_ok());
        assert_eq!(roundtrip(hint(0, 1080, 100)), Err(ProtoError::BadRange));
        assert_eq!(roundtrip(hint(1920, 1080, 50)), Err(ProtoError::BadRange));
        let many = Message::ClipboardOffer {
            serial: 1,
            formats: vec![ClipFormat::Text; MAX_LIST_LEN + 1],
        };
        assert!(matches!(
            roundtrip(many),
            Err(ProtoError::FieldTooLong { .. })
        ));
    }

    #[test]
    fn clipboard_chunks_reassemble() {
        let data: Vec<u8> = (0..(MAX_CLIPBOARD_CHUNK * 2 + 17))
            .map(|i| i as u8)
            .collect();
        let frames = clipboard_chunks(7, ClipFormat::Png, &data).unwrap();
        assert_eq!(frames.len(), 3);
        let mut asm = ClipboardAssembler::default();
        let mut done = None;
        for f in frames {
            // Every chunk survives the wire.
            let Message::ClipboardData {
                serial,
                format,
                total,
                offset,
                chunk,
            } = roundtrip(f).unwrap().msg
            else {
                panic!()
            };
            done = asm.push(serial, format, total, offset, &chunk).unwrap();
        }
        assert_eq!(done, Some((7, ClipFormat::Png, data)));
        // Empty content is one empty chunk.
        let empty = clipboard_chunks(8, ClipFormat::Text, &[]).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(
            asm.push(8, ClipFormat::Text, 0, 0, &[]).unwrap(),
            Some((8, ClipFormat::Text, vec![]))
        );
    }

    #[test]
    fn clipboard_assembler_rejects_gaps_and_mixups() {
        let mut asm = ClipboardAssembler::default();
        assert!(
            asm.push(1, ClipFormat::Text, 10, 5, b"x").is_err(),
            "no start"
        );
        assert_eq!(
            asm.push(1, ClipFormat::Text, 10, 0, b"hello").unwrap(),
            None
        );
        assert!(asm.push(1, ClipFormat::Text, 10, 6, b"x").is_err(), "gap");
        // After an error the transfer is gone.
        assert!(asm.push(1, ClipFormat::Text, 10, 5, b"world").is_err());
        asm.push(2, ClipFormat::Text, 4, 0, b"ab").unwrap();
        assert!(
            asm.push(2, ClipFormat::Html, 4, 2, b"cd").is_err(),
            "format"
        );
        asm.push(3, ClipFormat::Text, 4, 0, b"ab").unwrap();
        assert!(asm.push(3, ClipFormat::Text, 5, 2, b"cd").is_err(), "total");
        assert!(
            clipboard_chunks(
                1,
                ClipFormat::Png,
                &vec![0; MAX_CLIPBOARD_TOTAL as usize + 1]
            )
            .is_err()
        );
    }

    #[test]
    fn build_id_is_stable_and_content_addressed() {
        let a = agent_build_id("0.0.1-alpha", b"MZ one");
        assert_eq!(a, agent_build_id("0.0.1-alpha", b"MZ one"));
        assert_ne!(a, agent_build_id("0.0.1-alpha", b"MZ two"));
        assert!(a.starts_with("0.0.1-alpha+") && a.len() == "0.0.1-alpha+".len() + 16);
    }

    #[test]
    fn app_identities_match_between_windows_and_launchers() {
        let calc = app_identity(
            Some("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"),
            "C:\\x\\ApplicationFrameHost.exe",
        );
        assert_eq!(
            linux_app_id(&calc),
            "lsw.microsoft-windowscalculator-8wekyb3d8bbwe-app"
        );
        let notepad = app_identity(None, "C:\\Windows\\System32\\NOTEPAD.EXE");
        assert_eq!(notepad, "notepad.exe");
        assert_eq!(linux_app_id(&notepad), "lsw.notepad-exe");
        assert_eq!(linux_app_id(""), "lsw.windows");
        assert_eq!(app_identity(Some("  "), "a.exe"), "a.exe");
    }

    #[test]
    fn app_entries_are_bounded() {
        let app = |icon: usize| {
            Message::App(AppEntry {
                launch: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into(),
                name: "Calculator".into(),
                app: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into(),
                icon_png: vec![0; icon],
            })
        };
        assert!(roundtrip(app(1000)).is_ok());
        assert!(matches!(
            roundtrip(app(MAX_ICON_LEN + 1)),
            Err(ProtoError::FieldTooLong { .. })
        ));
    }

    #[test]
    fn negotiation_window() {
        assert_eq!(negotiate(PROTOCOL_VERSION), Ok(PROTOCOL_VERSION));
        assert_eq!(negotiate(PROTOCOL_VERSION + 5), Ok(PROTOCOL_VERSION));
        assert!(negotiate(0).is_err());
    }
}
