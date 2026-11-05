//! The app icon, drawn from one geometry definition (shared with
//! scripts/make_icon.py, which produces assets/icon.ico and icon.svg):
//! a navy tile, a white envelope with a navy flap seam, and a light-blue
//! folded corner. Small sizes drop the fold and thicken the seam.

/// Colour at (x, y) in the 100-unit design space, or None (transparent).
pub fn sample(x: f32, y: f32, small: bool) -> Option<[u8; 3]> {
    const NAVY: [u8; 3] = [0x17, 0x36, 0x5C];
    const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
    const FOLD: [u8; 3] = [0x70, 0xA9, 0xED];
    if !in_round_rect(x, y, 0.0, 0.0, 100.0, 100.0, 20.0) {
        return None;
    }
    let (l, t, r, b) = if small { (14.0, 27.0, 86.0, 73.0) } else { (20.0, 32.0, 80.0, 70.0) };
    if !in_round_rect(x, y, l, t, r, b, 4.0) {
        return Some(NAVY);
    }
    // Folded corner: a triangle at the envelope's upper-right.
    if !small && x >= 68.0 && y <= 44.0 && (x - 68.0) >= (y - 32.0) {
        return Some(FOLD);
    }
    // Flap seam: two strokes meeting below the centre.
    let mid = ((l + r) / 2.0, t + (b - t) * 0.55);
    let seam = if small { 3.4 } else { 2.0 };
    if dist_to_segment(x, y, (l + 1.0, t + 1.0), mid) < seam || dist_to_segment(x, y, mid, (r - 1.0, t + 1.0)) < seam {
        return Some(NAVY);
    }
    Some(WHITE)
}

fn in_round_rect(x: f32, y: f32, l: f32, t: f32, r: f32, b: f32, rad: f32) -> bool {
    if x < l || x > r || y < t || y > b {
        return false;
    }
    let cx = x.clamp(l + rad, r - rad);
    let cy = y.clamp(t + rad, b - rad);
    (x - cx).powi(2) + (y - cy).powi(2) <= rad * rad
}

fn dist_to_segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (px, py) = (a.0 + t * dx, a.1 + t * dy);
    ((x - px).powi(2) + (y - py).powi(2)).sqrt()
}

/// RGBA pixels at `n`×`n`, 4×4 supersampled.
pub fn render(n: usize) -> Vec<u8> {
    let small = n <= 24;
    let ss = 4;
    let mut out = vec![0u8; n * n * 4];
    for py in 0..n {
        for px in 0..n {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..ss {
                for sx in 0..ss {
                    let x = (px as f32 + (sx as f32 + 0.5) / ss as f32) * 100.0 / n as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / ss as f32) * 100.0 / n as f32;
                    if let Some(c) = sample(x, y, small) {
                        r += c[0] as u32;
                        g += c[1] as u32;
                        b += c[2] as u32;
                        a += 255;
                    }
                }
            }
            let i = (py * n + px) * 4;
            let k = (a / 255).max(1);
            out[i..i + 4].copy_from_slice(&[(r / k) as u8, (g / k) as u8, (b / k) as u8, (a / (ss * ss) as u32) as u8]);
        }
    }
    out
}
