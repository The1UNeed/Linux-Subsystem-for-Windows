//! Host end of the Coherence channel (window list and frames).

use crate::channel::ChannelError;
use lsw_proto::coherence::CoMessage;
use lsw_proto::{Decoder, encode_message};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Receiving half: validated messages from the guest.
pub struct CoReader {
    stream: OwnedReadHalf,
    decoder: Decoder<CoMessage>,
    buf: Box<[u8]>,
}

/// Sending half: requests to the guest.
pub struct CoWriter {
    stream: OwnedWriteHalf,
}

/// Connect to QEMU's Coherence chardev socket.
pub async fn connect(path: &Path, timeout: Duration) -> Result<(CoReader, CoWriter), ChannelError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let stream = loop {
        match UnixStream::connect(path).await {
            Ok(s) => break s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let (r, w) = stream.into_split();
    Ok((
        CoReader {
            stream: r,
            decoder: Decoder::new(),
            buf: vec![0u8; 256 * 1024].into_boxed_slice(),
        },
        CoWriter { stream: w },
    ))
}

impl CoReader {
    /// Drop any partial frame: the guest's Coherence port closed or
    /// reopened (agent restart, Windows reboot), so the next byte starts a
    /// new stream.
    pub fn reset(&mut self) {
        self.decoder = Decoder::new();
    }

    /// Next validated message. Cancel-safe.
    pub async fn recv(&mut self) -> Result<CoMessage, ChannelError> {
        loop {
            if let Some(msg) = self.decoder.next_message()? {
                return Ok(msg);
            }
            let n = self.stream.read(&mut self.buf).await?;
            if n == 0 {
                return Err(ChannelError::Closed);
            }
            self.decoder.push(&self.buf[..n])?;
        }
    }
}

impl CoWriter {
    pub async fn send(&mut self, msg: &CoMessage) -> Result<(), ChannelError> {
        let frame = encode_message(msg)?;
        self.stream.write_all(&frame).await?;
        Ok(())
    }
}
