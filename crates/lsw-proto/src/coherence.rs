//! Coherence channel: Windows windows mirrored as native Linux windows.
//!
//! Runs on its own virtio-serial port ([`COHERENCE_PORT_NAME`]) so large
//! frames never delay clipboard or control traffic. The guest announces
//! top-level windows and sends their pixels as damaged tiles (BGRA rows,
//! LZ4-compressed); the host asks for activation, resizes, closing and
//! hit-tests (to turn a press on a Windows title bar into a native move).
//!
//! Everything from the guest is validated: tile geometry against the window
//! size, decompressed sizes against the tile size, string and list lengths.

use crate::{MAX_STRING_LEN, ProtoError, Wire};
use serde::{Deserialize, Serialize};

/// virtio-serial port of the Coherence channel.
pub const COHERENCE_PORT_NAME: &str = "org.lsw.coherence.0";

/// Largest frame on this channel (one window update).
pub const MAX_CO_FRAME: u32 = 32 << 20;

/// Largest window (in pixels) that is mirrored.
pub const MAX_WINDOW_PIXELS: u64 = 64 << 20;

/// Most tiles in one frame.
pub const MAX_TILES: usize = 8192;

/// Tile edge length used by the guest encoder.
pub const TILE: u32 = 64;

/// A rectangle in guest desktop pixels.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// An application window (becomes an `xdg_toplevel`).
    Normal,
    /// An owned dialog (a child toplevel of its owner).
    Dialog,
    /// A menu, tooltip or dropdown (an `xdg_popup` of its owner).
    Popup,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Normal,
    Minimized,
    Maximized,
}

/// Where a point of a window is, as Windows' `WM_NCHITTEST` sees it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitArea {
    Client,
    /// The title bar: the host starts a native move.
    Caption,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// Minimize, maximize, close and other buttons: forward the click.
    Button,
}

/// Static information about a window.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u64,
    pub owner: Option<u64>,
    pub kind: WindowKind,
    pub title: String,
    /// Executable path, e.g. `C:\Windows\System32\notepad.exe` (identifies
    /// the app: one Linux app per Windows app).
    pub exe: String,
    pub rect: Rect,
    pub state: WindowState,
}

/// One damaged tile: `width`×`height` BGRA pixels at (`x`, `y`) inside the
/// window, LZ4-compressed (block format, size not prepended).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub lz4: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum CoMessage {
    // ---- host → guest
    /// Start mirroring (capture runs only while a host client is attached).
    Start {
        max_fps: u8,
    },
    /// Stop mirroring.
    Stop,
    /// Bring a window to the front (the user focused its Linux window).
    Activate {
        id: u64,
    },
    /// The Linux window was resized: resize the Windows window.
    Resize {
        id: u64,
        width: u32,
        height: u32,
    },
    /// Close the window (`WM_CLOSE`).
    Close {
        id: u64,
    },
    Minimize {
        id: u64,
    },
    Restore {
        id: u64,
    },
    /// What is at (`x`, `y`) inside window `id`?
    HitTest {
        id: u64,
        serial: u32,
        x: i32,
        y: i32,
    },

    // ---- guest → host
    /// The guest desktop size (window positions are relative to it).
    Desktop {
        width: u32,
        height: u32,
    },
    Opened(WindowInfo),
    /// Title, position, size or state changed.
    Changed(WindowInfo),
    Closed {
        id: u64,
    },
    /// New pixels for a window of `width`×`height`. The first frame after
    /// a size change covers the whole window.
    Frame {
        id: u64,
        width: u32,
        height: u32,
        tiles: Vec<Tile>,
    },
    /// The foreground window changed.
    Foreground {
        id: Option<u64>,
    },
    HitTestResult {
        id: u64,
        serial: u32,
        area: HitArea,
    },
}

fn check_window(width: u32, height: u32) -> Result<(), ProtoError> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_WINDOW_PIXELS {
        return Err(ProtoError::BadRange);
    }
    Ok(())
}

fn check_info(info: &WindowInfo) -> Result<(), ProtoError> {
    for (field, s) in [("title", &info.title), ("exe", &info.exe)] {
        if s.len() > MAX_STRING_LEN {
            return Err(ProtoError::FieldTooLong {
                field,
                len: s.len(),
                limit: MAX_STRING_LEN,
            });
        }
    }
    // Minimized windows report a tiny or empty rectangle; only bound it.
    if u64::from(info.rect.width) * u64::from(info.rect.height) > MAX_WINDOW_PIXELS {
        return Err(ProtoError::BadRange);
    }
    Ok(())
}

impl Wire for CoMessage {
    const MAX_LEN: u32 = MAX_CO_FRAME;

    fn validate(&self) -> Result<(), ProtoError> {
        match self {
            CoMessage::Opened(info) | CoMessage::Changed(info) => check_info(info),
            CoMessage::Desktop { width, height } => check_window(*width, *height),
            CoMessage::Resize { width, height, .. } => check_window(*width, *height),
            CoMessage::Frame {
                width,
                height,
                tiles,
                ..
            } => {
                check_window(*width, *height)?;
                if tiles.len() > MAX_TILES {
                    return Err(ProtoError::FieldTooLong {
                        field: "tiles",
                        len: tiles.len(),
                        limit: MAX_TILES,
                    });
                }
                for t in tiles {
                    let fits_x = t.x.checked_add(t.width).is_some_and(|e| e <= *width);
                    let fits_y = t.y.checked_add(t.height).is_some_and(|e| e <= *height);
                    if t.width == 0 || t.height == 0 || !fits_x || !fits_y {
                        return Err(ProtoError::BadRange);
                    }
                }
                Ok(())
            }
            CoMessage::Start { .. }
            | CoMessage::Stop
            | CoMessage::Activate { .. }
            | CoMessage::Close { .. }
            | CoMessage::Minimize { .. }
            | CoMessage::Restore { .. }
            | CoMessage::HitTest { .. }
            | CoMessage::Closed { .. }
            | CoMessage::Foreground { .. }
            | CoMessage::HitTestResult { .. } => Ok(()),
        }
    }
}

/// Compress one tile of `frame` (tightly packed BGRA, `frame_width` wide).
pub fn encode_tile(frame: &[u8], frame_width: u32, x: u32, y: u32, w: u32, h: u32) -> Tile {
    let stride = frame_width as usize * 4;
    let mut raw = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let start = row as usize * stride + x as usize * 4;
        raw.extend_from_slice(&frame[start..start + w as usize * 4]);
    }
    Tile {
        x,
        y,
        width: w,
        height: h,
        lz4: lz4_flex::block::compress(&raw),
    }
}

/// Decompress a validated tile into `frame` (`frame_width` wide, tightly
/// packed BGRA). Fails if the data does not decompress to exactly the
/// tile's size.
pub fn apply_tile(frame: &mut [u8], frame_width: u32, tile: &Tile) -> Result<(), ProtoError> {
    let expected = tile.width as usize * tile.height as usize * 4;
    let raw =
        lz4_flex::block::decompress(&tile.lz4, expected).map_err(|_| ProtoError::Malformed)?;
    if raw.len() != expected {
        return Err(ProtoError::Malformed);
    }
    let stride = frame_width as usize * 4;
    let row_len = tile.width as usize * 4;
    for (i, row) in raw.chunks_exact(row_len).enumerate() {
        let start = (tile.y as usize + i) * stride + tile.x as usize * 4;
        frame
            .get_mut(start..start + row_len)
            .ok_or(ProtoError::BadRange)?
            .copy_from_slice(row);
    }
    Ok(())
}

/// Tiles of `current` that differ from `previous` (same size, tightly
/// packed BGRA); all tiles when there is no previous frame.
pub fn damaged_tiles(
    current: &[u8],
    previous: Option<&[u8]>,
    width: u32,
    height: u32,
) -> Vec<Tile> {
    let stride = width as usize * 4;
    let mut tiles = Vec::new();
    let mut y = 0;
    while y < height {
        let h = TILE.min(height - y);
        let mut x = 0;
        while x < width {
            let w = TILE.min(width - x);
            let changed = previous.is_none_or(|prev| {
                (y..y + h).any(|row| {
                    let s = row as usize * stride + x as usize * 4;
                    let e = s + w as usize * 4;
                    current[s..e] != prev[s..e]
                })
            });
            if changed {
                tiles.push(encode_tile(current, width, x, y, w, h));
            }
            x += w;
        }
        y += h;
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decoder, encode_message};

    fn frame(width: u32, height: u32, seed: u8) -> Vec<u8> {
        (0..width * height * 4)
            .map(|i| (i as u8).wrapping_mul(seed))
            .collect()
    }

    #[test]
    fn tiles_roundtrip_through_the_wire() {
        let (w, h) = (150, 70); // not a multiple of the tile size
        let first = frame(w, h, 3);
        let tiles = damaged_tiles(&first, None, w, h);
        assert_eq!(tiles.len(), 3 * 2);
        let msg = CoMessage::Frame {
            id: 7,
            width: w,
            height: h,
            tiles,
        };
        let bytes = encode_message(&msg).unwrap();
        let mut d = Decoder::<CoMessage>::new();
        d.push(&bytes).unwrap();
        let Some(CoMessage::Frame { tiles, .. }) = d.next_message().unwrap() else {
            panic!()
        };
        let mut out = vec![0u8; first.len()];
        for t in &tiles {
            apply_tile(&mut out, w, t).unwrap();
        }
        assert_eq!(out, first);
    }

    #[test]
    fn only_changed_tiles_are_sent() {
        let (w, h) = (128, 128);
        let a = frame(w, h, 5);
        let mut b = a.clone();
        b[(70 * w as usize + 100) * 4] ^= 0xff; // one pixel in tile (1, 1)
        let tiles = damaged_tiles(&b, Some(&a), w, h);
        assert_eq!(tiles.len(), 1);
        assert_eq!((tiles[0].x, tiles[0].y), (64, 64));
        assert!(damaged_tiles(&a, Some(&a), w, h).is_empty());
    }

    #[test]
    fn hostile_frames_are_rejected() {
        let tile = |x, y, w, h| Tile {
            x,
            y,
            width: w,
            height: h,
            lz4: vec![],
        };
        let frame_msg = |tiles| CoMessage::Frame {
            id: 1,
            width: 100,
            height: 100,
            tiles,
        };
        assert_eq!(
            frame_msg(vec![tile(90, 0, 20, 10)]).validate(),
            Err(ProtoError::BadRange)
        );
        assert_eq!(
            frame_msg(vec![tile(u32::MAX, 0, 2, 1)]).validate(),
            Err(ProtoError::BadRange)
        );
        assert_eq!(
            frame_msg(vec![tile(0, 0, 0, 1)]).validate(),
            Err(ProtoError::BadRange)
        );
        assert!(
            frame_msg(vec![tile(0, 0, 1, 1); MAX_TILES + 1])
                .validate()
                .is_err()
        );
        let huge = CoMessage::Frame {
            id: 1,
            width: 100_000,
            height: 100_000,
            tiles: vec![],
        };
        assert_eq!(huge.validate(), Err(ProtoError::BadRange));
        // Data that decompresses to the wrong size is refused.
        let mut out = vec![0u8; 100 * 100 * 4];
        let lie = Tile {
            lz4: lz4_flex::block::compress(&[1u8; 10]),
            ..tile(0, 0, 4, 4)
        };
        assert_eq!(apply_tile(&mut out, 100, &lie), Err(ProtoError::Malformed));
        let long_title = CoMessage::Opened(WindowInfo {
            id: 1,
            owner: None,
            kind: WindowKind::Normal,
            title: "x".repeat(MAX_STRING_LEN + 1),
            exe: String::new(),
            rect: Rect::default(),
            state: WindowState::Normal,
        });
        assert!(matches!(
            long_title.validate(),
            Err(ProtoError::FieldTooLong { .. })
        ));
    }
}
