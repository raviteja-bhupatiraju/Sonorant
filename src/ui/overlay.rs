//! The dictation indicator's look, shared by the platform implementations (`platform::Overlay`):
//! a dark rounded pill with a status dot and up to a few lines of text, rendered into 32-bit
//! pixels (`0xAARRGGBB`, premultiplied alpha) that both Windows layered windows and Wayland shm
//! buffers take as they are.

/// Most lines of transcript shown before it is cut with an ellipsis.
pub const MAX_LINES: i32 = 4;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// Recording; the text is "Recording" or the live transcript.
    Recording,
    Working,
    /// A finished transcript (shown briefly when it was not typed into an application).
    Done,
    Notice,
}

impl Kind {
    /// Status dot: red recording, amber working, green done, grey notice.
    pub fn dot_rgb(self) -> (u8, u8, u8) {
        match self {
            Kind::Recording => (232, 72, 72),
            Kind::Working => (238, 174, 40),
            Kind::Done => (72, 190, 104),
            Kind::Notice => (150, 150, 156),
        }
    }
}

pub const TEXT_RGB: (u8, u8, u8) = (240, 240, 242);
pub const HINT_RGB: (u8, u8, u8) = (150, 150, 156);

/// Cut `text` (at a word boundary where possible) and add an ellipsis until `measure` (wrapped
/// width, height) says it fits in `max_h`. With `keep_end` the start is cut instead, so the newest
/// words of a growing text stay visible.
pub fn fit(text: &str, max_h: i32, keep_end: bool, mut measure: impl FnMut(&str) -> (i32, i32)) -> (String, i32, i32) {
    let (w, h) = measure(text);
    if h <= max_h {
        return (text.to_owned(), w, h);
    }
    let chars: Vec<char> = text.chars().collect();
    // Keep `n` characters, preferring a word boundary unless that would throw away too much.
    let candidate = |n: usize| -> String {
        let is_cut = |c: char| c.is_whitespace() || ",;:-".contains(c);
        if keep_end {
            let s: String = chars[chars.len() - n..].iter().collect();
            let s = match s.find(char::is_whitespace) {
                Some(pos) if pos < 24 => &s[pos..],
                _ => &s,
            };
            format!("\u{2026}{}", s.trim_start_matches(is_cut))
        } else {
            let mut s: String = chars[..n].iter().collect();
            if let Some(pos) = s.rfind(char::is_whitespace) {
                if s.len() - pos < 24 {
                    s.truncate(pos);
                }
            }
            format!("{}\u{2026}", s.trim_end_matches(is_cut))
        }
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&candidate(mid)).1 <= max_h {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let t = candidate(lo);
    let (w, h) = measure(&t);
    (t, w, h)
}

/// Coverage (0..1) of the pixel centred at (`px`, `py`) by a rounded rectangle of `w` x `h` with
/// corner `radius`, plus the signed distance from its edge (negative inside).
pub fn rounded_rect(px: f32, py: f32, w: f32, h: f32, radius: f32) -> (f32, f32) {
    let (qx, qy) = ((px - w / 2.0).abs() - (w / 2.0 - radius), (py - h / 2.0).abs() - (h / 2.0 - radius));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let dist = outside + qx.max(qy).min(0.0) - radius;
    ((0.5 - dist).clamp(0.0, 1.0), dist)
}

/// Fill `pixels` (`w` x `h`, alpha not yet applied) with the opaque dark pill and its thin light
/// outline (visible on dark applications too). Returns the shape's coverage per pixel, which
/// `apply_alpha` turns into the alpha channel once the text has been drawn.
pub fn fill_pill(pixels: &mut [u32], w: i32, h: i32, scale: f32) -> Vec<u8> {
    let radius = (22.0 * scale).min(h as f32 / 2.0).max(1.0);
    let edge = scale.max(1.0);
    let mut coverage = vec![0u8; pixels.len()];
    for y in 0..h {
        for x in 0..w {
            let (cov, dist) = rounded_rect(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
            let i = (y * w + x) as usize;
            coverage[i] = (cov * 255.0).round() as u8;
            let (r, g, b) = if dist > -edge { (92u32, 92, 100) } else { (32u32, 32, 34) };
            pixels[i] = (r << 16) | (g << 8) | b;
        }
    }
    coverage
}

/// Anti-aliased status dot of radius `r` centred at (`cx`, `cy`), drawn over the pill.
pub fn draw_dot(pixels: &mut [u32], w: i32, (cx, cy): (f32, f32), r: f32, rgb: (u8, u8, u8)) {
    let h = pixels.len() as i32 / w;
    let (x0, x1) = (((cx - r - 1.0) as i32).max(0), ((cx + r + 1.0) as i32).min(w - 1));
    let (y0, y1) = (((cy - r - 1.0) as i32).max(0), ((cy + r + 1.0) as i32).min(h - 1));
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (cov, _) = rounded_rect(x as f32 + 0.5 - (cx - r), y as f32 + 0.5 - (cy - r), 2.0 * r, 2.0 * r, r);
            let i = (y * w + x) as usize;
            pixels[i] = blend(pixels[i], rgb, cov);
        }
    }
}

/// Mix `rgb` into the opaque pixel `px` (`0x00RRGGBB`) with weight `a` (0..1).
pub fn blend(px: u32, (r, g, b): (u8, u8, u8), a: f32) -> u32 {
    let mix = |dst: u32, src: u8| (dst as f32 + (src as f32 - dst as f32) * a).round() as u32;
    (mix((px >> 16) & 0xff, r) << 16) | (mix((px >> 8) & 0xff, g) << 8) | mix(px & 0xff, b)
}

/// Apply the pill's coverage as (premultiplied) alpha.
pub fn apply_alpha(pixels: &mut [u32], coverage: &[u8]) {
    for (px, &cov) in pixels.iter_mut().zip(coverage) {
        let a = cov as u32;
        let (r, g, b) = ((*px >> 16) & 0xff, (*px >> 8) & 0xff, *px & 0xff);
        *px = (a << 24) | ((r * a / 255) << 16) | ((g * a / 255) << 8) | (b * a / 255);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cuts_the_start_or_the_end() {
        // A fake measure: 10 characters per line, 10 px per line.
        let measure = |t: &str| (100, (t.chars().count() as i32 + 9) / 10 * 10);
        let text = "one two three four five six seven eight nine ten";
        let (head, _, h) = fit(text, 20, false, measure);
        assert!(h <= 20 && head.starts_with("one") && head.ends_with('\u{2026}'), "{head}");
        let (tail, _, h) = fit(text, 20, true, measure);
        assert!(h <= 20 && tail.ends_with("ten") && tail.starts_with('\u{2026}'), "{tail}");
    }
}
