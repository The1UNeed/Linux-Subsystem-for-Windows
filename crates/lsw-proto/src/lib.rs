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
//! # Compatibility
//!
//! Variants are only ever appended to [`Message`], never reordered or removed.
//! Peers exchange [`PROTOCOL_VERSION`] in [`Message::Hello`] /
//! [`Message::Welcome`] and the host refuses incompatible agents.

use serde::{Deserialize, Serialize};

/// Current protocol version. Bumped whenever [`Message`] gains a variant.
pub const PROTOCOL_VERSION: u16 = 1;

/// Oldest agent protocol the host still speaks.
pub const MIN_SUPPORTED_VERSION: u16 = 1;

/// Largest accepted frame payload (1 MiB). Bulk data (pixels, files) never
/// travels on the control channel.
pub const MAX_FRAME_LEN: u32 = 1 << 20;

/// Largest accepted string field.
pub const MAX_STRING_LEN: usize = 4096;

/// vsock port the guest agent listens on for the control channel.
pub const CONTROL_PORT: u32 = 5700;

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
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN}-byte limit")]
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
            Message::Welcome { .. }
            | Message::Ping { .. }
            | Message::Pong { .. }
            | Message::Shutdown { .. } => Ok(()),
        }
    }
}

/// Encode one envelope into a complete frame (header + payload).
pub fn encode(env: &Envelope) -> Result<Vec<u8>, ProtoError> {
    env.validate()?;
    let mut out =
        postcard::to_extend(env, vec![0u8; HEADER_LEN]).map_err(|_| ProtoError::Malformed)?;
    let payload_len = out.len() - HEADER_LEN;
    let len = u32::try_from(payload_len).map_err(|_| ProtoError::FrameTooLarge(u32::MAX))?;
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge(len));
    }
    out[..HEADER_LEN].copy_from_slice(&len.to_le_bytes());
    Ok(out)
}

/// Decode a single complete payload (no header).
pub fn decode_payload(payload: &[u8]) -> Result<Envelope, ProtoError> {
    let (env, rest): (Envelope, &[u8]) =
        postcard::take_from_bytes(payload).map_err(|_| ProtoError::Malformed)?;
    if !rest.is_empty() {
        return Err(ProtoError::TrailingBytes(rest.len()));
    }
    env.validate()?;
    Ok(env)
}

/// Incremental decoder for a byte stream of frames.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    poisoned: bool,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes read from the transport.
    ///
    /// The buffer never grows past one maximum frame plus its header: once a
    /// header announces an oversized frame the decoder poisons itself instead
    /// of buffering.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtoError> {
        if self.poisoned {
            return Err(ProtoError::Poisoned);
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() >= HEADER_LEN {
            let len = self.peek_len();
            if len > MAX_FRAME_LEN {
                self.poisoned = true;
                self.buf.clear();
                return Err(ProtoError::FrameTooLarge(len));
            }
        }
        Ok(())
    }

    /// Pop the next complete envelope, if one is buffered.
    pub fn next_envelope(&mut self) -> Result<Option<Envelope>, ProtoError> {
        if self.poisoned {
            return Err(ProtoError::Poisoned);
        }
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = self.peek_len();
        if len > MAX_FRAME_LEN {
            self.poisoned = true;
            self.buf.clear();
            return Err(ProtoError::FrameTooLarge(len));
        }
        let total = HEADER_LEN + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let result = decode_payload(&self.buf[HEADER_LEN..total]);
        self.buf.drain(..total);
        match result {
            Ok(env) => Ok(Some(env)),
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

    #[test]
    fn negotiation_window() {
        assert_eq!(negotiate(PROTOCOL_VERSION), Ok(PROTOCOL_VERSION));
        assert_eq!(negotiate(PROTOCOL_VERSION + 5), Ok(PROTOCOL_VERSION));
        assert!(negotiate(0).is_err());
    }
}
