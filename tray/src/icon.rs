//! The FlowCursor mark — a cursor arrow crossing between two screens —
//! drawn in code, so the tray needs no image files and every state variant
//! is rendered at the exact size the tray host asks for. The same drawing
//! produces the app icon files (`cargo run -p flow-tray --example
//! gen-icons`): PNGs, the multi-size `.ico` embedded in the Windows exes.
//!
//! Shapes are rasterised with 4×4 supersampling; good enough for 16–256 px.

/// What the icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    /// The app icon (no state).
    App,
    /// Connected to the hub, nothing crossing.
    Idle,
    /// Controlling another machine or being controlled.
    Active,
    /// Sharing paused by the user or not on an allowed network.
    Away,
    /// No hub connection or the agent is not running.
    Disconnected,
    /// Not joined, revoked, rejected or a missing permission.
    Error,
}

impl Variant {
    pub const ALL: [Variant; 6] =
        [Variant::App, Variant::Idle, Variant::Active, Variant::Away, Variant::Disconnected, Variant::Error];

    pub fn name(self) -> &'static str {
        match self {
            Variant::App => "app",
            Variant::Idle => "idle",
            Variant::Active => "active",
            Variant::Away => "away",
            Variant::Disconnected => "disconnected",
            Variant::Error => "error",
        }
    }
}

type Rgb = [u8; 3];

const BEZEL: Rgb = [0x26, 0x32, 0x38];
const OUTLINE: Rgb = [0x1a, 0x20, 0x24];
const WHITE: Rgb = [0xff, 0xff, 0xff];

/// Screen colour per state.
fn screen(v: Variant) -> Rgb {
    match v {
        Variant::App => [0x3b, 0x8e, 0xd0],
        Variant::Idle => [0x2a, 0x9d, 0x8f],
        Variant::Active => [0x2f, 0x80, 0xed],
        Variant::Away => [0xe0, 0xa1, 0x2b],
        Variant::Disconnected | Variant::Error => [0x8a, 0x93, 0x9b],
    }
}

/// RGBA pixels (row-major, straight alpha) of `v` at `size`×`size`.
pub fn render(v: Variant, size: u32) -> Vec<u8> {
    let mut c = Canvas::new(size);
    // Two screens with stands; small sizes get thicker bezels.
    let small = size <= 24;
    let bezel = if small { 0.07 } else { 0.05 };
    for (x0, x1) in [(0.02, 0.47), (0.53, 0.98)] {
        let (y0, y1) = (0.26, 0.68);
        c.fill(BEZEL, |x, y| round_rect(x, y, x0, y0, x1, y1, 0.06));
        c.fill(screen(v), |x, y| round_rect(x, y, x0 + bezel, y0 + bezel, x1 - bezel, y1 - bezel, 0.03));
        let mid = (x0 + x1) / 2.0;
        c.fill(BEZEL, |x, y| rect(x, y, mid - 0.04, y1, mid + 0.04, 0.78));
        c.fill(BEZEL, |x, y| round_rect(x, y, mid - 0.13, 0.76, mid + 0.13, 0.82, 0.02));
    }
    if v == Variant::Disconnected {
        // Hollow screens: the link is gone.
        for (x0, x1) in [(0.02, 0.47), (0.53, 0.98)] {
            let i = bezel + 0.06;
            c.fill(BEZEL, |x, y| round_rect(x, y, x0 + i, 0.26 + i, x1 - i, 0.68 - i, 0.02));
        }
    }
    // The cursor arrow over the gap between the screens, white with a
    // dark outline.
    let arrow = arrow_polygon(0.38, 0.20, if v == Variant::Active { 0.66 } else { 0.60 });
    let outline = if small { 0.055 } else { 0.035 };
    c.fill(OUTLINE, |x, y| inside_polygon(&arrow, x, y) || dist_to_polygon(&arrow, x, y) < outline);
    c.fill(WHITE, |x, y| inside_polygon(&arrow, x, y) && dist_to_polygon(&arrow, x, y) > outline * 0.2);

    // State badge, bottom right.
    let (bx, by, br) = (0.78, 0.78, if small { 0.24 } else { 0.21 });
    let badge = |c: &mut Canvas, color: Rgb| {
        c.fill(WHITE, |x, y| circle(x, y, bx, by, br + 0.03));
        c.fill(color, |x, y| circle(x, y, bx, by, br));
    };
    match v {
        Variant::App | Variant::Idle => {}
        Variant::Active => {
            badge(&mut c, [0x22, 0xa8, 0x4a]);
            let tri = [(bx - 0.08, by - 0.11), (bx + 0.11, by), (bx - 0.08, by + 0.11)];
            c.fill(WHITE, |x, y| inside_polygon(&tri, x, y));
        }
        Variant::Away => {
            badge(&mut c, [0xc2, 0x7c, 0x0e]);
            for dx in [-0.06, 0.06] {
                c.fill(WHITE, |x, y| rect(x, y, bx + dx - 0.035, by - 0.10, bx + dx + 0.035, by + 0.10));
            }
        }
        Variant::Disconnected => {
            badge(&mut c, [0x5f, 0x67, 0x6e]);
            let d = 0.09;
            c.fill(WHITE, |x, y| {
                dist_to_segment(x, y, (bx - d, by - d), (bx + d, by + d)) < 0.035
                    || dist_to_segment(x, y, (bx - d, by + d), (bx + d, by - d)) < 0.035
            });
        }
        Variant::Error => {
            badge(&mut c, [0xd6, 0x33, 0x2f]);
            c.fill(WHITE, |x, y| round_rect(x, y, bx - 0.035, by - 0.13, bx + 0.035, by + 0.035, 0.02));
            c.fill(WHITE, |x, y| circle(x, y, bx, by + 0.095, 0.04));
        }
    }
    c.px
}

/// Classic pointer arrow with its tip at (`tx`,`ty`), `h` tall (unit coords).
fn arrow_polygon(tx: f32, ty: f32, h: f32) -> Vec<(f32, f32)> {
    [(0.0, 0.0), (0.0, 0.80), (0.20, 0.62), (0.33, 0.92), (0.46, 0.86), (0.33, 0.57), (0.58, 0.57)]
        .iter()
        .map(|&(x, y)| (tx + x * h, ty + y * h))
        .collect()
}

struct Canvas {
    size: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(size: u32) -> Canvas {
        Canvas { size, px: vec![0; (size * size * 4) as usize] }
    }

    /// Paints `color` where `inside(x, y)` holds (unit coordinates),
    /// alpha-blended by the fraction of 4×4 subsamples covered.
    fn fill(&mut self, color: Rgb, inside: impl Fn(f32, f32) -> bool) {
        const N: u32 = 4;
        let s = self.size as f32;
        for py in 0..self.size {
            for px in 0..self.size {
                let mut hits = 0;
                for sy in 0..N {
                    for sx in 0..N {
                        let x = (px as f32 + (sx as f32 + 0.5) / N as f32) / s;
                        let y = (py as f32 + (sy as f32 + 0.5) / N as f32) / s;
                        if inside(x, y) {
                            hits += 1;
                        }
                    }
                }
                if hits == 0 {
                    continue;
                }
                let a = hits as f32 / (N * N) as f32;
                let i = ((py * self.size + px) * 4) as usize;
                let dst_a = self.px[i + 3] as f32 / 255.0;
                let out_a = a + dst_a * (1.0 - a);
                for k in 0..3 {
                    let d = self.px[i + k] as f32 / 255.0;
                    let c = color[k] as f32 / 255.0;
                    let v = (c * a + d * dst_a * (1.0 - a)) / out_a;
                    self.px[i + k] = (v * 255.0).round() as u8;
                }
                self.px[i + 3] = (out_a * 255.0).round() as u8;
            }
        }
    }
}

fn rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> bool {
    x >= x0 && x < x1 && y >= y0 && y < y1
}

fn round_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if !rect(x, y, x0, y0, x1, y1) {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// Even-odd rule.
fn inside_polygon(p: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut j = p.len() - 1;
    for i in 0..p.len() {
        let (xi, yi) = p[i];
        let (xj, yj) = p[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn dist_to_segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0) };
    let (px, py) = (a.0 + t * dx, a.1 + t * dy);
    ((x - px).powi(2) + (y - py).powi(2)).sqrt()
}

fn dist_to_polygon(p: &[(f32, f32)], x: f32, y: f32) -> f32 {
    (0..p.len()).map(|i| dist_to_segment(x, y, p[i], p[(i + 1) % p.len()])).fold(f32::MAX, f32::min)
}

/// PNG file of `v` at `size`.
pub fn png(v: Variant, size: u32) -> Vec<u8> {
    encode_png(&render(v, size), size)
}

fn encode_png(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, size, size);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().expect("PNG header");
        w.write_image_data(rgba).expect("PNG data");
    }
    out
}

/// Sizes in the Windows `.ico`.
pub const ICO_SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];

/// A Windows `.ico` with every size in [`ICO_SIZES`]: 32-bit BMP entries
/// (what every consumer, including resource compilers and NSIS, reads)
/// and a PNG entry for 256 px (BMP would be 256 KiB).
pub fn ico(v: Variant) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = ICO_SIZES
        .iter()
        .map(|&s| (s, if s >= 256 { encode_png(&render(v, s), s) } else { ico_bmp(&render(v, s), s) }))
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, data) in &images {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend_from_slice(&data);
    }
    out
}

/// BITMAPINFOHEADER + bottom-up BGRA rows + 1-bit AND mask.
fn ico_bmp(rgba: &[u8], size: u32) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    for v in [40u32, size, size * 2] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    let image_size = size * size * 4 + mask_row * size;
    for v in [0u32, image_size, 0, 0, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_row as usize];
        for x in 0..size {
            if rgba[((y * size + x) * 4 + 3) as usize] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_differ_and_have_transparent_corners() {
        let imgs: Vec<Vec<u8>> = Variant::ALL.iter().map(|&v| render(v, 32)).collect();
        for (i, a) in imgs.iter().enumerate() {
            assert_eq!(a.len(), 32 * 32 * 4);
            assert_eq!(a[3], 0, "top-left corner is transparent");
            assert!(a.chunks(4).filter(|p| p[3] == 255).count() > 200, "{:?} is mostly drawn", Variant::ALL[i]);
            for b in &imgs[i + 1..] {
                assert_ne!(a, b, "every state looks different");
            }
        }
    }

    #[test]
    fn ico_layout() {
        let ico = ico(Variant::App);
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        let n = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        assert_eq!(n, ICO_SIZES.len());
        for (k, &s) in ICO_SIZES.iter().enumerate() {
            let e = &ico[6 + 16 * k..6 + 16 * (k + 1)];
            assert_eq!(e[0] as u32, s % 256);
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
            let off = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
            assert!(off + len <= ico.len());
            let data = &ico[off..off + len];
            if s == 256 {
                assert_eq!(&data[1..4], b"PNG");
            } else {
                assert_eq!(u32::from_le_bytes(data[0..4].try_into().unwrap()), 40);
                assert_eq!(len as u32, 40 + s * s * 4 + s.div_ceil(32) * 4 * s);
            }
        }
    }
}
