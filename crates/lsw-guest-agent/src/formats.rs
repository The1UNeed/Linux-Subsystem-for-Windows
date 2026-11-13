//! Conversions between Windows clipboard formats and the wire formats.
//!
//! * `HTML Format` (CF_HTML) ↔ an HTML fragment.
//! * `CF_DIB` (a packed device-independent bitmap) ↔ PNG.
//!
//! Pure Rust and platform independent so they are tested on any host.

#![forbid(unsafe_code)]

/// Largest image either side converts (64 Mpx ≈ 256 MiB RGBA).
const MAX_PIXELS: u64 = 64 << 20;

/// Wrap an HTML fragment in the CF_HTML envelope Windows apps expect.
pub fn cf_html_wrap(fragment: &str) -> Vec<u8> {
    const PREFIX: &str = "<html><body>\r\n<!--StartFragment-->";
    const SUFFIX: &str = "<!--EndFragment-->\r\n</body></html>";
    // Fixed-width offsets keep the header length independent of the values.
    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_html = header_len;
    let start_fragment = start_html + PREFIX.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + SUFFIX.len();
    let mut out = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    debug_assert_eq!(out.len(), header_len);
    out.push_str(PREFIX);
    out.push_str(fragment);
    out.push_str(SUFFIX);
    out.into_bytes()
}

/// Extract the fragment from CF_HTML data (UTF-8, offsets in bytes).
pub fn cf_html_fragment(data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(data.strip_suffix(&[0]).unwrap_or(data)).ok()?;
    let field = |name: &str| -> Option<usize> {
        let line = text.lines().find_map(|l| l.strip_prefix(name))?;
        line.trim().parse().ok()
    };
    let start = field("StartFragment:")?;
    let end = field("EndFragment:")?;
    let fragment = text.get(start..end)?;
    Some(fragment.to_owned())
}

/// Convert a packed DIB (`BITMAPINFOHEADER` + optional masks + pixels) to
/// PNG. Supports the formats apps actually put on the clipboard:
/// uncompressed 24 and 32 bpp (`BI_RGB`, `BI_BITFIELDS`), either row order.
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at =
        |o: usize| -> Option<u32> { Some(u32::from_le_bytes(dib.get(o..o + 4)?.try_into().ok()?)) };
    let i32_at = |o: usize| -> Option<i32> { u32_at(o).map(|v| v as i32) };
    let header_size = u32_at(0)? as usize;
    if header_size < 40 || header_size > dib.len() {
        return None;
    }
    let width = i32_at(4)?;
    let height = i32_at(8)?;
    let bpp = u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?);
    let compression = u32_at(16)?;
    let colors_used = u32_at(32)?;
    if width <= 0 || height == 0 || !(bpp == 24 || bpp == 32) {
        return None;
    }
    let (w, h) = (width as u64, height.unsigned_abs() as u64);
    if w * h > MAX_PIXELS {
        return None;
    }
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    let mut offset = header_size;
    // With a plain BITMAPINFOHEADER, BI_BITFIELDS masks follow the header.
    if compression == BI_BITFIELDS && header_size == 40 {
        offset += 12;
    } else if compression != BI_RGB && compression != BI_BITFIELDS {
        return None;
    }
    offset += colors_used as usize * 4;
    let stride = ((w * u64::from(bpp)).div_ceil(32) * 4) as usize;
    let pixels = dib.get(offset..offset.checked_add(stride.checked_mul(h as usize)?)?)?;
    // 32 bpp: use alpha only if the image actually has some (many apps
    // leave it zero), otherwise treat the image as opaque.
    let has_alpha = bpp == 32 && pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h as usize {
        let src_row = if height > 0 {
            h as usize - 1 - row
        } else {
            row
        };
        let line = &pixels[src_row * stride..src_row * stride + stride];
        for x in 0..w as usize {
            let px = &line[x * (bpp as usize / 8)..];
            let a = if has_alpha { px[3] } else { 255 };
            rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
    }
    Some(out)
}

/// Convert PNG to a packed 32 bpp top-down DIB (`BITMAPINFOHEADER`,
/// `BI_RGB`), the form every Windows app can paste.
pub fn png_to_dib(data: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return None,
    };
    let mut dib = Vec::with_capacity(40 + (w * h * 4) as usize);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&32u16.to_le_bytes()); // bpp
    dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib.extend_from_slice(&(w * h * 4).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]); // resolution, palette counts
    for row in buf[..frame.buffer_size()].chunks_exact(frame.line_size) {
        for px in row.chunks_exact(channels).take(w as usize) {
            let (r, g, b, a) = match channels {
                4 => (px[0], px[1], px[2], px[3]),
                3 => (px[0], px[1], px[2], 255),
                2 => (px[0], px[0], px[0], px[1]),
                _ => (px[0], px[0], px[0], 255),
            };
            dib.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_roundtrip() {
        let frag = "<b>héllo</b> <i>world</i>";
        let wrapped = cf_html_wrap(frag);
        assert_eq!(cf_html_fragment(&wrapped).as_deref(), Some(frag));
        let text = String::from_utf8(wrapped).unwrap();
        assert!(text.starts_with("Version:0.9\r\nStartHTML:"));
        // A trailing NUL (as Windows often stores it) is tolerated.
        let mut z = cf_html_wrap("x");
        z.push(0);
        assert_eq!(cf_html_fragment(&z).as_deref(), Some("x"));
    }

    #[test]
    fn cf_html_rejects_bad_offsets() {
        let bad = b"Version:0.9\r\nStartFragment:999\r\nEndFragment:1000\r\n";
        assert_eq!(cf_html_fragment(bad), None);
        assert_eq!(cf_html_fragment(b"garbage"), None);
    }

    /// 2x2 bottom-up 24 bpp DIB: bottom row blue, red; top row green, white.
    fn dib24() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&2i32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]);
        // Stride 8 (6 bytes of pixels + 2 padding). BGR order.
        d.extend_from_slice(&[255, 0, 0, 0, 0, 255, 0, 0]); // bottom: blue, red
        d.extend_from_slice(&[0, 255, 0, 255, 255, 255, 0, 0]); // top: green, white
        d
    }

    fn decode_rgba(png_data: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut r = png::Decoder::new(std::io::Cursor::new(png_data))
            .read_info()
            .unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let f = r.next_frame(&mut buf).unwrap();
        buf.truncate(f.buffer_size());
        (f.width, f.height, buf)
    }

    #[test]
    fn dib_to_png_handles_bottom_up_24bpp() {
        let png_data = dib_to_png(&dib24()).unwrap();
        let (w, h, px) = decode_rgba(&png_data);
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            px,
            [
                0, 255, 0, 255, 255, 255, 255, 255, // top: green, white
                0, 0, 255, 255, 255, 0, 0, 255, // bottom: blue, red
            ]
        );
    }

    #[test]
    fn png_dib_png_roundtrip() {
        let png_data = dib_to_png(&dib24()).unwrap();
        let dib = png_to_dib(&png_data).unwrap();
        let again = dib_to_png(&dib).unwrap();
        assert_eq!(decode_rgba(&png_data), decode_rgba(&again));
    }

    #[test]
    fn hostile_dibs_are_rejected() {
        assert_eq!(dib_to_png(&[]), None);
        let mut huge = dib24();
        huge[4..8].copy_from_slice(&100_000i32.to_le_bytes());
        huge[8..12].copy_from_slice(&100_000i32.to_le_bytes());
        assert_eq!(dib_to_png(&huge), None, "too many pixels");
        let mut short = dib24();
        short.truncate(50);
        assert_eq!(dib_to_png(&short), None, "truncated pixels");
        let mut rle = dib24();
        rle[16] = 1; // BI_RLE8
        assert_eq!(dib_to_png(&rle), None);
        assert_eq!(png_to_dib(b"not a png"), None);
    }
}
