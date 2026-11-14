//! The guest framebuffer as received from QEMU's D-Bus display.
//!
//! QEMU forwards what the guest draws, so every size and rectangle is
//! checked before it touches memory.

/// `PIXMAN_x8r8g8b8`: 32-bit pixels, blue first in memory (little endian).
pub const PIXMAN_X8R8G8B8: u32 = 0x2002_0888;
/// `PIXMAN_a8r8g8b8`.
pub const PIXMAN_A8R8G8B8: u32 = 0x2002_8888;

/// Largest framebuffer accepted (16384² is virtio-gpu's own limit; this
/// caps memory at 256 MiB).
const MAX_PIXELS: u64 = 64 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FbError {
    #[error("unsupported pixel format {0:#x}")]
    Format(u32),
    #[error("bad framebuffer geometry")]
    Geometry,
    #[error("update outside the framebuffer")]
    Rect,
}

/// Tightly packed BGRX pixels (`stride == width * 4`).
#[derive(Debug, Default, Clone)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

fn check_format(format: u32) -> Result<(), FbError> {
    if format == PIXMAN_X8R8G8B8 || format == PIXMAN_A8R8G8B8 {
        Ok(())
    } else {
        Err(FbError::Format(format))
    }
}

impl Framebuffer {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// A new full frame.
    pub fn scanout(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> Result<(), FbError> {
        check_format(format)?;
        let (w, h, s) = (width as usize, height as usize, stride as usize);
        if width == 0
            || height == 0
            || u64::from(width) * u64::from(height) > MAX_PIXELS
            || s < w * 4
            || data.len() < s * (h - 1) + w * 4
        {
            return Err(FbError::Geometry);
        }
        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            out.extend_from_slice(&data[row * s..row * s + w * 4]);
        }
        self.width = width;
        self.height = height;
        self.data = out;
        Ok(())
    }

    /// Copy a damaged rectangle into the frame.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        format: u32,
        data: &[u8],
    ) -> Result<(), FbError> {
        check_format(format)?;
        if x < 0 || y < 0 || width <= 0 || height <= 0 {
            return Err(FbError::Rect);
        }
        let (x, y, w, h, s) = (
            x as usize,
            y as usize,
            width as usize,
            height as usize,
            stride as usize,
        );
        if x.checked_add(w).is_none_or(|e| e > self.width as usize)
            || y.checked_add(h).is_none_or(|e| e > self.height as usize)
        {
            return Err(FbError::Rect);
        }
        if s < w * 4 || data.len() < s * (h - 1) + w * 4 {
            return Err(FbError::Geometry);
        }
        let fb_stride = self.stride();
        for row in 0..h {
            let dst = (y + row) * fb_stride + x * 4;
            self.data[dst..dst + w * 4].copy_from_slice(&data[row * s..row * s + w * 4]);
        }
        Ok(())
    }

    /// Map a point in a view of `view_w`×`view_h` pixels that shows the
    /// frame scaled to fit (letterboxed) to frame coordinates. `None` when
    /// the point is in the letterbox.
    pub fn view_to_frame(&self, view_w: f64, view_h: f64, x: f64, y: f64) -> Option<(u32, u32)> {
        if self.width == 0 || self.height == 0 || view_w <= 0.0 || view_h <= 0.0 {
            return None;
        }
        let (fw, fh) = (f64::from(self.width), f64::from(self.height));
        let scale = (view_w / fw).min(view_h / fh);
        let (ox, oy) = ((view_w - fw * scale) / 2.0, (view_h - fh * scale) / 2.0);
        let (gx, gy) = ((x - ox) / scale, (y - oy) / scale);
        if gx < 0.0 || gy < 0.0 || gx >= fw || gy >= fh {
            return None;
        }
        Some((gx as u32, gy as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanout_strips_padding_and_update_patches() {
        let mut fb = Framebuffer::default();
        // 2x2 with stride 12 (4 bytes padding per row).
        let data = [
            1, 1, 1, 1, 2, 2, 2, 2, 9, 9, 9, 9, //
            3, 3, 3, 3, 4, 4, 4, 4, 9, 9, 9, 9,
        ];
        fb.scanout(2, 2, 12, PIXMAN_X8R8G8B8, &data).unwrap();
        assert_eq!(fb.data, [1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4]);
        fb.update(1, 1, 1, 1, 4, PIXMAN_X8R8G8B8, &[7, 7, 7, 7])
            .unwrap();
        assert_eq!(&fb.data[12..], &[7, 7, 7, 7]);
    }

    #[test]
    fn hostile_geometry_is_rejected() {
        let mut fb = Framebuffer::default();
        assert_eq!(
            fb.scanout(2, 2, 8, 0x1234, &[0; 16]),
            Err(FbError::Format(0x1234))
        );
        assert_eq!(
            fb.scanout(2, 2, 4, PIXMAN_X8R8G8B8, &[0; 16]),
            Err(FbError::Geometry),
            "stride below width"
        );
        assert_eq!(
            fb.scanout(2, 2, 8, PIXMAN_X8R8G8B8, &[0; 15]),
            Err(FbError::Geometry),
            "short data"
        );
        assert_eq!(
            fb.scanout(100_000, 100_000, 400_000, PIXMAN_X8R8G8B8, &[]),
            Err(FbError::Geometry)
        );
        fb.scanout(2, 2, 8, PIXMAN_X8R8G8B8, &[0; 16]).unwrap();
        assert_eq!(
            fb.update(1, 1, 2, 1, 8, PIXMAN_X8R8G8B8, &[0; 8]),
            Err(FbError::Rect)
        );
        assert_eq!(
            fb.update(-1, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4]),
            Err(FbError::Rect)
        );
        assert_eq!(
            fb.update(i32::MAX, 0, 1, 1, 4, PIXMAN_X8R8G8B8, &[0; 4]),
            Err(FbError::Rect)
        );
    }

    #[test]
    fn letterboxed_view_mapping() {
        let mut fb = Framebuffer::default();
        fb.scanout(100, 50, 400, PIXMAN_X8R8G8B8, &vec![0; 400 * 50])
            .unwrap();
        // 200x200 view: frame scaled 2x to 200x100, 50 px bars top and bottom.
        assert_eq!(fb.view_to_frame(200.0, 200.0, 0.0, 50.0), Some((0, 0)));
        assert_eq!(fb.view_to_frame(200.0, 200.0, 199.0, 149.0), Some((99, 49)));
        assert_eq!(fb.view_to_frame(200.0, 200.0, 100.0, 10.0), None);
    }
}
