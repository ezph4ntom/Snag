//! The Snag logo, drawn in code: a play button with a bite taken out of its
//! top-right edge, and the accent dot sitting in the bite ("the clip,
//! snagged"). Geometry is taken from the brand board (120-unit viewBox):
//!
//! * triangle (34,22) (34,98) (98,60), drawn filled with a 14-unit round
//!   stroke, i.e. rounded by 7 units;
//! * bite: a circle of radius 18 at (74,37) cut out of the triangle;
//! * dot: radius 7 at (74,37), in the accent color.
//! * At 24 px and below the board uses a heavier variant (stroke 18,
//!   bite 19, dot 9) so the shape survives tiny sizes.
//!
//! One rasterizer feeds every place the logo appears: tray icon, window and
//! taskbar icon, the header inside the app, and the .exe icon (`build.rs`
//! includes this file, so it must only use `std`).

/// Brand colors (sRGB).
pub const INK: [u8; 3] = [0x0E, 0x11, 0x16];
pub const LIGHT: [u8; 3] = [0xF2, 0xF4, 0xF8];
pub const DARK_GLYPH: [u8; 3] = [0x12, 0x14, 0x1A];
pub const EMBER: [u8; 3] = [0xF0, 0x4A, 0x24];
pub const MUTED: [u8; 3] = [0x9A, 0xA3, 0xB2];
pub const MUTED_ON_LIGHT: [u8; 3] = [0x4A, 0x4F, 0x5A];

struct Style {
    /// Half the stroke width: how much the triangle is rounded/grown.
    round: f32,
    bite: f32,
    dot: f32,
}

fn style_for(glyph_px: f32) -> Style {
    if glyph_px <= 24.0 {
        Style { round: 9.0, bite: 19.0, dot: 9.0 }
    } else {
        Style { round: 7.0, bite: 18.0, dot: 7.0 }
    }
}

const TRI: [(f32, f32); 3] = [(34.0, 22.0), (34.0, 98.0), (98.0, 60.0)];
const BITE_CENTER: (f32, f32) = (74.0, 37.0);

fn dot2(a: (f32, f32), b: (f32, f32)) -> f32 {
    a.0 * b.0 + a.1 * b.1
}

/// Signed distance from `p` to the triangle (negative inside).
fn sd_triangle(p: (f32, f32)) -> f32 {
    let [p0, p1, p2] = TRI;
    let e = [(p1.0 - p0.0, p1.1 - p0.1), (p2.0 - p1.0, p2.1 - p1.1), (p0.0 - p2.0, p0.1 - p2.1)];
    let v = [(p.0 - p0.0, p.1 - p0.1), (p.0 - p1.0, p.1 - p1.1), (p.0 - p2.0, p.1 - p2.1)];
    let s = (e[0].0 * e[2].1 - e[0].1 * e[2].0).signum();
    let mut best_d2 = f32::MAX;
    let mut inside = f32::MAX;
    for i in 0..3 {
        let t = (dot2(v[i], e[i]) / dot2(e[i], e[i])).clamp(0.0, 1.0);
        let pq = (v[i].0 - e[i].0 * t, v[i].1 - e[i].1 * t);
        best_d2 = best_d2.min(dot2(pq, pq));
        inside = inside.min(s * (v[i].0 * e[i].1 - v[i].1 * e[i].0));
    }
    -best_d2.sqrt() * inside.signum()
}

/// Distances (in glyph units) to the glyph body and to the dot at `p`.
fn glyph_distances(p: (f32, f32), st: &Style) -> (f32, f32) {
    let to_bite = ((p.0 - BITE_CENTER.0).powi(2) + (p.1 - BITE_CENTER.1).powi(2)).sqrt();
    let body = (sd_triangle(p) - st.round).max(st.bite - to_bite);
    let dot = to_bite - st.dot;
    (body, dot)
}

/// Where the glyph sits in an image: glyph units `[u0, u0 + span]` (both
/// axes, starting at `(ux, uy)`) map onto pixels `[off, off + px]`.
struct Placement {
    ux: f32,
    uy: f32,
    span: f32,
    off: f32,
    px: f32,
}

/// Tight square around the glyph (with room for the heavy variant), so tiny
/// icons use their pixels for the shape rather than for padding.
fn tight(off: f32, px: f32) -> Placement {
    Placement { ux: 19.0, uy: 13.0, span: 94.0, off, px }
}

/// The brand board's own 120-unit framing (used on the large app icon).
fn framed(off: f32, px: f32) -> Placement {
    Placement { ux: 0.0, uy: 0.0, span: 120.0, off, px }
}

fn coverage(distance_px: f32) -> f32 {
    (0.5 - distance_px).clamp(0.0, 1.0)
}

/// Paints the glyph over an RGBA buffer (straight alpha), anti-aliased.
fn paint_glyph(rgba: &mut [u8], size: u32, place: &Placement, body: [u8; 3], dot: [u8; 3]) {
    let scale = place.px / place.span; // pixels per glyph unit
    let st = style_for(120.0 * scale);
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let u = (place.ux + (px - place.off) / scale, place.uy + (py - place.off) / scale);
            let (db, dd) = glyph_distances(u, &st);
            let (ab, ad) = (coverage(db * scale), coverage(dd * scale));
            if ab <= 0.0 && ad <= 0.0 {
                continue;
            }
            let i = ((y * size + x) * 4) as usize;
            for (color, a) in [(body, ab), (dot, ad)] {
                if a <= 0.0 {
                    continue;
                }
                // "over" compositing in straight alpha.
                let dst_a = rgba[i + 3] as f32 / 255.0;
                let out_a = a + dst_a * (1.0 - a);
                for c in 0..3 {
                    let d = rgba[i + c] as f32;
                    rgba[i + c] = ((color[c] as f32 * a + d * dst_a * (1.0 - a)) / out_a.max(1e-6)).round() as u8;
                }
                rgba[i + 3] = (out_a * 255.0).round() as u8;
            }
        }
    }
}

/// The glyph alone on a transparent background, filling the image. Used
/// for the tray icon and the in-app header. `body`: triangle color (light
/// on dark backgrounds, dark on light ones); `dot`: the accent.
pub fn glyph_rgba(size: u32, body: [u8; 3], dot: [u8; 3]) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    paint_glyph(&mut rgba, size, &tight(0.0, size as f32), body, dot);
    rgba
}

/// The app icon: an ink rounded square (corner radius 23% of the size, as
/// on the brand board) with the light glyph and accent dot. Used for the
/// window/taskbar icon and the .exe icon.
pub fn app_icon_rgba(size: u32, dot: [u8; 3]) -> Vec<u8> {
    let s = size as f32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let radius = 0.23 * s;
    let half = s / 2.0;
    for y in 0..size {
        for x in 0..size {
            // Rounded-square signed distance, in pixels.
            let qx = ((x as f32 + 0.5 - half).abs() - (half - radius)).max(0.0);
            let qy = ((y as f32 + 0.5 - half).abs() - (half - radius)).max(0.0);
            let d = (qx * qx + qy * qy).sqrt() - radius;
            let a = coverage(d);
            let i = ((y * size + x) * 4) as usize;
            rgba[i..i + 3].copy_from_slice(&INK);
            rgba[i + 3] = (a * 255.0).round() as u8;
        }
    }
    // Board framing at normal sizes; at small sizes a bigger, tighter glyph
    // (a 62%-wide glyph is only ~10 px tall in a 16 px icon).
    let place = if size >= 40 {
        framed(0.19 * s, 0.62 * s)
    } else {
        tight(0.14 * s, 0.72 * s)
    };
    paint_glyph(&mut rgba, size, &place, LIGHT, dot);
    rgba
}

/// A Windows .ico file containing the app icon at several sizes (32-bit
/// BMP entries with alpha). Built at compile time by `build.rs`.
pub fn app_icon_ico(sizes: &[u32], dot: [u8; 3]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_entry(&app_icon_rgba(s, dot), s)).collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]); // reserved, type 1 = icon
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 }; // 0 means 256
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // color planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        ico.extend_from_slice(&img);
    }
    ico
}

/// One icon image: BITMAPINFOHEADER + bottom-up BGRA pixels + an all-zero
/// AND mask (alpha does the masking).
fn bmp_entry(rgba: &[u8], size: u32) -> Vec<u8> {
    let mask_row = (size.div_ceil(32) * 4) as usize;
    let mut out = Vec::with_capacity(40 + rgba.len() + mask_row * size as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // image + mask
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]); // BI_RGB, sizes/resolution/palette unused
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.extend(std::iter::repeat_n(0u8, mask_row * size as usize));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(rgba: &[u8], size: u32, x: u32, y: u32) -> u8 {
        rgba[((y * size + x) * 4 + 3) as usize]
    }

    #[test]
    fn glyph_shape() {
        // Inside the triangle, away from the bite: solid. In the bite gap
        // between the dot and the triangle: empty. On the dot: accent.
        assert!(sd_triangle((45.0, 60.0)) < 0.0);
        assert!(sd_triangle((10.0, 10.0)) > 0.0);
        let st = style_for(120.0);
        let (body, dot) = glyph_distances((74.0, 37.0), &st);
        assert!(dot < 0.0 && body > 0.0, "the dot sits in the bite");
        let (body, _) = glyph_distances((74.0, 24.0), &st);
        assert!(body > 0.0, "the bite is cut out of the triangle");
        let (body, _) = glyph_distances((45.0, 60.0), &st);
        assert!(body < 0.0);
    }

    #[test]
    fn icons_render_and_ico_is_well_formed() {
        let g = glyph_rgba(32, LIGHT, EMBER);
        assert!(alpha_at(&g, 32, 0, 0) == 0, "transparent corner");
        assert!(g.chunks(4).filter(|p| p[3] > 200).count() > 150, "glyph has substance at 32 px");
        let icon = app_icon_rgba(64, EMBER);
        assert_eq!(alpha_at(&icon, 64, 32, 2), 255, "opaque square");
        assert_eq!(alpha_at(&icon, 64, 0, 0), 0, "rounded corner");
        let ico = app_icon_ico(&[16, 32, 256], EMBER);
        assert_eq!(&ico[0..6], &[0, 0, 1, 0, 3, 0]);
        let first_offset = u32::from_le_bytes([ico[18], ico[19], ico[20], ico[21]]);
        assert_eq!(first_offset, 6 + 16 * 3);
    }
}
