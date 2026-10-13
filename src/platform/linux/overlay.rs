//! The dictation indicator on Wayland (X11: see `overlay_x11`): the same pill as on Windows (see `crate::ui::overlay`), shown
//! as a wlr-layer-shell surface at the bottom of the screen that never takes focus and lets
//! clicks through. Compositors without layer shell (GNOME) cannot place a window like that, so
//! there the indicator becomes a desktop notification that is updated in place.
//!
//! The indicator lives on its own thread with its own Wayland connection; `Overlay` only sends
//! it messages, so it can be used from the UI thread like the Windows version.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, EventQueue, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

use super::{runtime, sans_font};
use crate::ui::overlay::{apply_alpha, draw_dot, fill_pill, fit, Kind, HINT_RGB, MAX_LINES, TEXT_RGB};

pub(super) enum Msg {
    Show { kind: Kind, text: String, hint: Option<String> },
    Hide,
}

pub struct Overlay {
    tx: Sender<Msg>,
}

impl Overlay {
    pub fn new() -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let x11 = super::use_x11();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || match if x11 { Err(anyhow::anyhow!("X11 session")) } else { LayerOverlay::connect() } {
                Ok(layer) => layer.run(rx),
                Err(_) if x11 => match super::overlay_x11::XOverlay::connect() {
                    Ok(window) => window.run(rx),
                    Err(e) => {
                        log::info!("indicator: no X11 window ({e:#}); using notifications");
                        notifications(rx);
                    }
                },
                Err(e) => {
                    log::info!("indicator: no layer shell ({e:#}); using notifications");
                    notifications(rx);
                }
            })
            .ok()?;
        Some(Self { tx })
    }

    /// Show `text` (and an optional small `hint` line under it), sizing the pill to fit.
    pub fn show(&self, kind: Kind, text: &str, hint: Option<&str>) {
        let _ = self.tx.send(Msg::Show { kind, text: text.to_owned(), hint: hint.map(str::to_owned) });
    }

    pub fn hide(&self) {
        let _ = self.tx.send(Msg::Hide);
    }
}

// ---- rendering --------------------------------------------------------------------------------

/// Text layout and drawing with a system font.
pub(super) struct Text {
    font: FontVec,
}

impl Text {
    pub(super) fn load() -> Option<Self> {
        let (data, index) = sans_font()?;
        Some(Self { font: FontVec::try_from_vec_and_index(data, index).ok()? })
    }

    /// The scale for an em size of `em` pixels (as GDI sizes fonts on Windows).
    fn scale(&self, em: f32) -> PxScale {
        let units = self.font.units_per_em().unwrap_or(1000.0);
        PxScale::from(em * self.font.height_unscaled() / units)
    }

    fn line_height(&self, scale: PxScale) -> i32 {
        let f = self.font.as_scaled(scale);
        (f.height() + f.line_gap()).ceil() as i32
    }

    fn width(&self, scale: PxScale, s: &str) -> f32 {
        let f = self.font.as_scaled(scale);
        let mut prev = None;
        s.chars()
            .map(|c| {
                let id = f.glyph_id(c);
                let kern = prev.map_or(0.0, |p| f.kern(p, id));
                prev = Some(id);
                kern + f.h_advance(id)
            })
            .sum()
    }

    /// Greedy word wrap to `width` pixels (words longer than a line are broken anywhere).
    fn wrap(&self, scale: PxScale, text: &str, width: i32) -> Vec<String> {
        let fits = |s: &str| self.width(scale, s) <= width as f32;
        let mut lines = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let candidate = if line.is_empty() { word.to_owned() } else { format!("{line} {word}") };
            if fits(&candidate) {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            for c in word.chars() {
                if !fits(&format!("{line}{c}")) && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
        }
        if !line.is_empty() || lines.is_empty() {
            lines.push(line);
        }
        lines
    }

    /// Wrapped size of `text`, like GDI's `DT_CALCRECT | DT_WORDBREAK`.
    fn measure(&self, scale: PxScale, text: &str, width: i32) -> (i32, i32) {
        let lines = self.wrap(scale, text, width);
        let w = lines.iter().map(|l| self.width(scale, l).ceil() as i32).max().unwrap_or(0);
        (w, lines.len() as i32 * self.line_height(scale))
    }

    /// Draw `text` wrapped to `width` with its top-left corner at (`x`, `y`) into `pixels`, `w` wide.
    fn draw(&self, (pixels, w): (&mut [u32], i32), scale: PxScale, text: &str, (x, y): (i32, i32), width: i32, rgb: (u8, u8, u8)) {
        let f = self.font.as_scaled(scale);
        let h = pixels.len() as i32 / w;
        for (n, line) in self.wrap(scale, text, width).iter().enumerate() {
            let baseline = y as f32 + n as f32 * self.line_height(scale) as f32 + f.ascent();
            let mut pen = x as f32;
            let mut prev = None;
            for c in line.chars() {
                let id = f.glyph_id(c);
                pen += prev.map_or(0.0, |p| f.kern(p, id));
                prev = Some(id);
                if let Some(outline) = self.font.outline_glyph(id.with_scale_and_position(scale, ab_glyph::point(pen, baseline))) {
                    let b = outline.px_bounds();
                    outline.draw(|gx, gy, cov| {
                        let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                        if (0..w).contains(&px) && (0..h).contains(&py) {
                            let i = (py * w + px) as usize;
                            pixels[i] = crate::ui::overlay::blend(pixels[i], rgb, cov.min(1.0));
                        }
                    });
                }
                pen += f.h_advance(id);
            }
        }
    }
}

/// A rendered pill: premultiplied `0xAARRGGBB` pixels.
pub(super) struct Image {
    pub pixels: Vec<u32>,
    pub w: i32,
    pub h: i32,
}

/// Lay out and draw the pill at `scale` (device pixels per logical pixel) for a screen
/// `screen_w` device pixels wide. The size is rounded up to whole logical pixels.
pub(super) fn render(text: &Text, kind: Kind, msg: &str, hint: Option<&str>, scale: f32, screen_w: i32) -> Image {
    let px = |v: f32| (v * scale) as i32;
    let (pad_left, pad_right, pad_v) = (px(44.0), px(20.0), px(12.0));
    let (min_w, min_h) = (px(230.0), px(46.0));
    let max_w = px(560.0).min(screen_w * 7 / 10).max(min_w);
    let max_text_w = max_w - pad_left - pad_right;
    let (main, small) = (text.scale(15.0 * scale), text.scale(11.5 * scale));

    let line_h = text.line_height(main);
    let (msg, text_w, text_h) =
        fit(msg, line_h * MAX_LINES, kind == Kind::Recording, |t| text.measure(main, t, max_text_w));
    let (hint_w, hint_h) = hint.map_or((0, 0), |h| text.measure(small, h, max_text_w));
    let gap = if hint_h > 0 { px(2.0) } else { 0 };
    let slack = px(8.0);
    let round = |v: i32| (v as f32 / scale).ceil() as i32 * scale.round() as i32;
    let w = round((pad_left + text_w.max(hint_w) + slack + pad_right).clamp(min_w, max_w));
    let content_h = text_h + gap + hint_h;
    let h = round((content_h + 2 * pad_v).max(min_h));
    let top = (h - content_h) / 2;
    let text_area_w = w - pad_left - pad_right;

    let mut pixels = vec![0u32; (w * h) as usize];
    let coverage = fill_pill(&mut pixels, w, h, scale);
    draw_dot(&mut pixels, w, (22.0 * scale, (top + line_h / 2) as f32), 6.0 * scale, kind.dot_rgb());
    text.draw((&mut pixels, w), main, &msg, (pad_left, top), text_area_w, TEXT_RGB);
    if let Some(hint) = hint {
        text.draw((&mut pixels, w), small, hint, (pad_left, top + text_h + gap), text_area_w, HINT_RGB);
    }
    apply_alpha(&mut pixels, &coverage);
    Image { pixels, w, h }
}

// ---- layer shell ------------------------------------------------------------------------------

struct LayerOverlay {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    text: Text,
}

struct State {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    configured: bool,
}

impl LayerOverlay {
    fn connect() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let shm = Shm::bind(&globals, &qh)?;
        let mut state = State {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh)?,
            layer_shell: LayerShell::bind(&globals, &qh)?,
            pool: SlotPool::new(256 * 1024, &shm)?,
            shm,
            layer: None,
            configured: false,
        };
        queue.roundtrip(&mut state)?; // learn the outputs
        let text = Text::load().ok_or_else(|| anyhow::anyhow!("no usable system font"))?;
        Ok(Self { conn, queue, state, text })
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Msg::Show { kind, text, hint }) => {
                    if let Err(e) = self.show(kind, &text, hint.as_deref()) {
                        log::warn!("indicator: {e:#}");
                    }
                }
                Ok(Msg::Hide) => {
                    self.state.layer = None;
                    let _ = self.conn.flush();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Keep up with the compositor's events (buffer releases, output changes) without blocking.
            let _ = self.conn.flush();
            if let Some(guard) = self.conn.prepare_read() {
                let _ = guard.read();
            }
            let _ = self.queue.dispatch_pending(&mut self.state);
        }
    }

    /// The scale and logical width of the (first) output.
    fn output(&self) -> (i32, i32) {
        let info = self.state.outputs.outputs().next().and_then(|o| self.state.outputs.info(&o));
        let scale = info.as_ref().map_or(1, |i| i.scale_factor.max(1));
        let width = info.and_then(|i| i.logical_size).map_or(1920, |(w, _)| w);
        (scale, width)
    }

    fn show(&mut self, kind: Kind, text: &str, hint: Option<&str>) -> anyhow::Result<()> {
        let (scale, logical_w) = self.output();
        let img = render(&self.text, kind, text, hint, scale as f32, logical_w * scale);
        let (lw, lh) = ((img.w / scale) as u32, (img.h / scale) as u32);
        let qh = self.queue.handle();

        if self.state.layer.is_none() {
            let surface = self.state.compositor.create_surface(&qh);
            // Clicks go through to the application underneath.
            let region = Region::new(&self.state.compositor)?;
            surface.set_input_region(Some(region.wl_region()));
            let layer = self.state.layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("sonorant-indicator"), None);
            layer.set_anchor(Anchor::BOTTOM);
            layer.set_margin(0, 0, 28, 0);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.set_size(lw, lh);
            layer.commit();
            self.state.layer = Some(layer);
            self.state.configured = false;
            self.queue.roundtrip(&mut self.state)?; // wait for the first configure
        }
        let layer = self.state.layer.as_ref().expect("created above");
        if !self.state.configured {
            anyhow::bail!("the compositor did not configure the indicator");
        }
        layer.set_size(lw, lh);
        let (buffer, canvas) =
            self.state.pool.create_buffer(img.w, img.h, img.w * 4, wl_shm::Format::Argb8888)?;
        for (dst, px) in canvas.as_chunks_mut::<4>().0.iter_mut().zip(&img.pixels) {
            *dst = px.to_le_bytes();
        }
        let surface = layer.wl_surface();
        surface.set_buffer_scale(scale);
        buffer.attach_to(surface)?;
        surface.damage_buffer(0, 0, img.w, img.h);
        layer.commit();
        self.conn.flush()?;
        Ok(())
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.layer = None;
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        self.configured = true;
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_registry!(State);
delegate_dispatch2!(State);

// ---- notification fallback --------------------------------------------------------------------

/// Show the indicator as a desktop notification, replaced in place as it changes. Updates are
/// limited to a few per second (live transcripts change often; notification servers animate).
fn notifications(rx: Receiver<Msg>) {
    use notify_rust::{Hint, Notification, Timeout, Urgency};
    const MIN_GAP: Duration = Duration::from_millis(350);
    let mut shown: Option<notify_rust::NotificationHandle> = None;
    let mut last = Instant::now() - MIN_GAP;
    while let Ok(mut msg) = rx.recv() {
        // Wait out the rate limit, keeping only the newest message.
        let deadline = last + MIN_GAP;
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(newer) => msg = newer,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        last = Instant::now();
        match msg {
            Msg::Show { kind, text, hint } => {
                let summary = match kind {
                    Kind::Recording => "Sonorant - recording",
                    Kind::Done => "Sonorant - dictated text",
                    Kind::Working | Kind::Notice => "Sonorant",
                };
                let body = hint.map_or(text.clone(), |h| format!("{text}
{h}"));
                let mut n = Notification::new();
                n.appname("Sonorant")
                    .summary(summary)
                    .body(&body)
                    .icon("audio-input-microphone")
                    .hint(Hint::Transient(true))
                    .urgency(Urgency::Low)
                    .timeout(Timeout::Never);
                if let Some(old) = &shown {
                    n.id(old.id()); // replace it
                }
                match runtime().block_on(n.show_async()) {
                    Ok(handle) => shown = Some(handle),
                    Err(e) => log::warn!("indicator notification failed: {e}"),
                }
            }
            Msg::Hide => {
                if let Some(handle) = shown.take() {
                    runtime().block_on(handle.close_async());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the indicator to a PNG-free PPM in the temp folder for a visual check.
    #[test]
    fn renders_a_live_transcript() {
        let Some(text) = Text::load() else { return }; // no fonts on this machine
        let long = "So this is a much longer dictation to check how the indicator copes with a lot of text, \
                    and the newest words must stay visible at the end of it.";
        let img = render(&text, Kind::Recording, long, None, 2.0, 3840);
        assert!(img.w > 0 && img.h > 0 && img.w % 2 == 0 && img.h % 2 == 0);
        let mut ppm = format!("P6 {} {} 255\n", img.w, img.h).into_bytes();
        for px in &img.pixels {
            let a = px >> 24;
            // Composite over a mid-grey background.
            let ch = |s: u32| (((px >> s) & 0xff) + 128 * (255 - a) / 255).min(255) as u8;
            ppm.extend([ch(16), ch(8), ch(0)]);
        }
        let _ = std::fs::write(std::env::temp_dir().join("sonorant-indicator.ppm"), ppm);
    }
}
