//! Conversions between Windows device-independent bitmaps (CF_DIB /
//! CF_DIBV5 clipboard data) and PNG, the image interchange format of
//! FlowCursor. Platform independent so it can be tested anywhere.

/// Maximum accepted image dimension (pixels per side).
const MAX_DIM: i64 = 16384;

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn i32le(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Encodes RGBA8 pixels as PNG.
pub fn rgba_to_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().ok()?;
        wr.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// Decodes a PNG into (width, height, RGBA8 pixels).
pub fn png_to_rgba(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w as i64 > MAX_DIM || h as i64 > MAX_DIM {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let px = (w * h) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => buf[..px * 3].as_chunks::<3>().0.iter().flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::Grayscale => buf[..px].iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => {
            buf[..px * 2].as_chunks::<2>().0.iter().flat_map(|c| [c[0], c[0], c[0], c[1]]).collect()
        }
        png::ColorType::Indexed => return None,
    };
    Some((w, h, rgba))
}

/// CF_DIB / CF_DIBV5 bytes → PNG. Supports 24- and 32-bit uncompressed
/// (BI_RGB / BI_BITFIELDS with the usual BGRA masks) bitmaps.
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let hsize = u32le(dib, 0)? as usize;
    if !(40..=124).contains(&hsize) {
        return None;
    }
    let w = i32le(dib, 4)? as i64;
    let h = i32le(dib, 8)? as i64;
    let bpp = u16le(dib, 14)?;
    let comp = u32le(dib, 16)?;
    let clr_used = u32le(dib, 32)? as usize;
    if w <= 0 || h == 0 || w > MAX_DIM || h.abs() > MAX_DIM || !(bpp == 24 || bpp == 32) || !(comp == 0 || comp == 3) {
        return None;
    }
    let mut off = hsize + clr_used * 4;
    if comp == 3 && hsize == 40 {
        off += 12; // three DWORD masks follow a plain BITMAPINFOHEADER
    }
    let (wu, hu) = (w as usize, h.unsigned_abs() as usize);
    let stride = (wu * bpp as usize).div_ceil(32) * 4;
    let pixels = dib.get(off..off + stride * hu)?;
    let mut rgba = vec![0u8; wu * hu * 4];
    let mut any_alpha = false;
    for row in 0..hu {
        let src_row = if h > 0 { hu - 1 - row } else { row };
        let src = &pixels[src_row * stride..];
        for x in 0..wu {
            let d = &mut rgba[(row * wu + x) * 4..][..4];
            if bpp == 32 {
                let p = &src[x * 4..x * 4 + 4];
                d.copy_from_slice(&[p[2], p[1], p[0], p[3]]);
                any_alpha |= p[3] != 0;
            } else {
                let p = &src[x * 3..x * 3 + 3];
                d.copy_from_slice(&[p[2], p[1], p[0], 255]);
            }
        }
    }
    if bpp == 32 && !any_alpha {
        // 32-bit BI_RGB usually leaves alpha at 0: treat as opaque.
        for a in rgba.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
    }
    rgba_to_png(w as u32, hu as u32, &rgba)
}

/// PNG → CF_DIB bytes (BITMAPINFOHEADER, 32-bit BI_RGB, bottom-up BGRA).
pub fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = png_to_rgba(png)?;
    let (wu, hu) = (w as usize, h as usize);
    let mut out = Vec::with_capacity(40 + wu * hu * 4);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((wu * hu * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution, colours used/important
    for row in (0..hu).rev() {
        for x in 0..wu {
            let p = &rgba[(row * wu + x) * 4..][..4];
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (u32, u32, Vec<u8>) {
        let (w, h) = (3u32, 2u32);
        let mut px = Vec::new();
        for i in 0..(w * h) as u8 {
            px.extend_from_slice(&[i * 10, 200 - i, i, 255]);
        }
        (w, h, px)
    }

    #[test]
    fn png_dib_roundtrip() {
        let (w, h, px) = sample();
        let png = rgba_to_png(w, h, &px).unwrap();
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(dib.len(), 40 + (w * h * 4) as usize);
        let png2 = dib_to_png(&dib).unwrap();
        assert_eq!(png_to_rgba(&png2).unwrap(), (w, h, px));
    }

    #[test]
    fn dib_24bit_top_down_with_padding() {
        // 1x2, 24 bpp → stride 4 bytes; top-down (negative height).
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&(-2i32).to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0u8; 24]);
        dib.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        let (w, h, px) = png_to_rgba(&dib_to_png(&dib).unwrap()).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(px, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn malformed_bitmaps_are_rejected() {
        assert!(dib_to_png(&[]).is_none());
        assert!(dib_to_png(&[40, 0, 0, 0, 1]).is_none());
        let (w, h, px) = sample();
        let mut dib = png_to_dib(&rgba_to_png(w, h, &px).unwrap()).unwrap();
        dib.truncate(50);
        assert!(dib_to_png(&dib).is_none(), "truncated pixel data");
        assert!(png_to_dib(b"not a png").is_none());
    }
}
