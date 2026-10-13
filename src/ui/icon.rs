//! Procedurally drawn app icon, shared by the tray, the window and `build.rs` (exe icon).

/// Draw the app icon: a rounded square with a speaker and sound waves.
/// `active == false` renders it greyed out (model still loading / error).
pub fn icon_rgba(size: u32, active: bool) -> Vec<u8> {
    let bg = if active { [37.0, 99.0, 235.0] } else { [110.0, 116.0, 128.0] };
    let speaker = [(0.20, 0.40), (0.34, 0.40), (0.52, 0.24), (0.52, 0.76), (0.34, 0.60), (0.20, 0.60)];
    const SS: u32 = 3; // supersampling per axis
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let (mut cov_bg, mut cov_fg) = (0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if in_rounded_square(x, y, 0.24) {
                        cov_bg += 1.0;
                        if in_polygon(x, y, &speaker) || on_wave(x, y) {
                            cov_fg += 1.0;
                        }
                    }
                }
            }
            let n = (SS * SS) as f32;
            let a = cov_bg / n;
            let fg = if cov_bg > 0.0 { cov_fg / cov_bg } else { 0.0 };
            let i = ((py * size + px) * 4) as usize;
            for c in 0..3 {
                rgba[i + c] = (bg[c] * (1.0 - fg) + 255.0 * fg) as u8;
            }
            rgba[i + 3] = (a * 255.0) as u8;
        }
    }
    rgba
}

fn in_rounded_square(x: f32, y: f32, r: f32) -> bool {
    let (dx, dy) = ((x - 0.5).abs() - (0.5 - r), (y - 0.5).abs() - (0.5 - r));
    if dx <= 0.0 || dy <= 0.0 {
        x > 0.0 && x < 1.0 && y > 0.0 && y < 1.0
    } else {
        dx * dx + dy * dy <= r * r
    }
}

fn in_polygon(x: f32, y: f32, pts: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn on_wave(x: f32, y: f32) -> bool {
    let (dx, dy) = (x - 0.52, y - 0.5);
    if dx <= 0.0 {
        return false;
    }
    let d = (dx * dx + dy * dy).sqrt();
    let angle = dy.atan2(dx).abs();
    angle < 0.75 && [0.21f32, 0.32].iter().any(|r| (d - r).abs() < 0.04)
}
