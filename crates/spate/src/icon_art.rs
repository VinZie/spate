// The Spate app icon as plain RGBA pixels: a rounded blue tile with a white
// down arrow whose shaft is split into blocks (a nod to segmented downloading)
// over a landing bar.
//
// This file has no dependencies because it is also `include!`d by the build
// scripts (`winres.rs`), which render the same artwork into the Windows .ico
// embedded in the executables. Keep it free of `crate::` paths and inner
// attributes.

/// Render the icon at `size`×`size` as straight (non-premultiplied) RGBA.
pub fn app_icon_rgba(size: u32) -> Vec<u8> {
    let n = size as usize;
    let mut rgba = vec![0u8; n * n * 4];
    const SS: usize = 4; // supersampling per axis
    for py in 0..n {
        for px in 0..n {
            let mut bg = 0.0f32;
            let mut fg = 0.0f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / n as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / n as f32;
                    if in_round_rect(x, y, 0.03, 0.03, 0.97, 0.97, 0.22) {
                        bg += 1.0;
                        if icon_glyph(x, y) {
                            fg += 1.0;
                        }
                    }
                }
            }
            let total = (SS * SS) as f32;
            let a_bg = bg / total;
            let a_fg = fg / total;
            let yf = py as f32 / n as f32;
            // Vertical gradient for the tile.
            let top = [74.0, 150.0, 255.0];
            let bottom = [27.0, 86.0, 204.0];
            // White glyph composited over the tile (fg coverage is a subset of bg).
            let glyph_mix = if a_bg > 0.0 { a_fg / a_bg } else { 0.0 };
            let c: [f32; 3] = std::array::from_fn(|i| {
                let tile = top[i] + (bottom[i] - top[i]) * yf;
                tile * (1.0 - glyph_mix) + 255.0 * glyph_mix
            });
            let o = (py * n + px) * 4;
            rgba[o] = c[0] as u8;
            rgba[o + 1] = c[1] as u8;
            rgba[o + 2] = c[2] as u8;
            rgba[o + 3] = (a_bg * 255.0).round() as u8;
        }
    }
    rgba
}

fn icon_glyph(x: f32, y: f32) -> bool {
    let shaft = |y0: f32, y1: f32| in_round_rect(x, y, 0.425, y0, 0.575, y1, 0.02);
    let blocks = shaft(0.14, 0.23) || shaft(0.27, 0.36) || shaft(0.40, 0.50);
    let head = in_triangle((x, y), (0.25, 0.47), (0.75, 0.47), (0.5, 0.73));
    let bar = in_round_rect(x, y, 0.22, 0.79, 0.78, 0.87, 0.04);
    blocks || head || bar
}

fn in_round_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let mid = |a: f32, b: f32| ((a + r).min((a + b) / 2.0), (b - r).max((a + b) / 2.0));
    let (lx, hx) = mid(x0, x1);
    let (ly, hy) = mid(y0, y1);
    let cx = x.max(lx).min(hx);
    let cy = y.max(ly).min(hy);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1);
    let d1 = sign(p, a, b);
    let d2 = sign(p, b, c);
    let d3 = sign(p, c, a);
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}
