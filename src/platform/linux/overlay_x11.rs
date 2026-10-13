//! The dictation indicator on X11: the same pill as everywhere else, in an override-redirect
//! window at the bottom of the monitor under the pointer. It never takes focus and lets clicks
//! through (empty input shape). With a compositing manager the pill is translucent (a 32-bit ARGB
//! window); without one the window is cut to the pill's outline with the Shape extension.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, ColormapAlloc, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat, StackMode, VisualClass, Window,
    WindowClass,
};
use x11rb::protocol::Event;

use super::overlay::{render, Image, Msg, Text};
use super::x11::{self, Display};
use crate::ui::overlay::Kind;

pub struct XOverlay {
    d: Display,
    text: Text,
    window: Window,
    gc: u32,
    depth: u8,
    /// A 32-bit window with alpha (a compositing manager is running), else a shaped opaque one.
    argb: bool,
    shaped: bool,
    scale: f32,
    /// The pill on screen, redrawn when the window is exposed.
    image: Option<Image>,
    visible: bool,
}

impl XOverlay {
    pub fn connect() -> Result<Self> {
        let d = x11::connect()?;
        let text = Text::load().context("no usable system font")?;
        let screen = &d.conn.setup().roots[d.screen];

        // A compositing manager announces itself by owning this selection.
        let composited = {
            let atom = d.conn.intern_atom(false, format!("_NET_WM_CM_S{}", d.screen).as_bytes())?.reply()?.atom;
            d.conn.get_selection_owner(atom)?.reply()?.owner != x11rb::NONE
        };
        let argb_visual = screen
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
            .flat_map(|depth| &depth.visuals)
            .find(|v| v.class == VisualClass::TRUE_COLOR && v.red_mask == 0xff0000 && v.green_mask == 0xff00 && v.blue_mask == 0xff)
            .map(|v| v.visual_id);
        let (depth, visual, argb) = match argb_visual {
            Some(v) if composited => (32, v, true),
            _ => (screen.root_depth, screen.root_visual, false),
        };

        let window = d.conn.generate_id()?;
        let mut aux = CreateWindowAux::new().override_redirect(1).border_pixel(0).event_mask(EventMask::EXPOSURE);
        if argb {
            let colormap = d.conn.generate_id()?;
            d.conn.create_colormap(ColormapAlloc::NONE, colormap, d.root, visual)?;
            aux = aux.colormap(colormap);
        }
        d.conn.create_window(depth, window, d.root, 0, 0, 1, 1, 0, WindowClass::INPUT_OUTPUT, visual, &aux)?;
        let gc = d.conn.generate_id()?;
        d.conn.create_gc(gc, window, &CreateGCAux::new())?;

        // Clicks go through to the application underneath.
        let shaped = d.conn.shape_query_version()?.reply().is_ok();
        if shaped {
            d.conn.shape_rectangles(shape::SO::SET, shape::SK::INPUT, xproto::ClipOrdering::UNSORTED, window, 0, 0, &[])?;
        }
        d.conn.flush()?;
        let scale = dpi_scale(&d);
        Ok(Self { d, text, window, gc, depth, argb, shaped, scale, image: None, visible: false })
    }

    pub fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Msg::Show { kind, text, hint }) => {
                    if let Err(e) = self.show(kind, &text, hint.as_deref()) {
                        log::warn!("indicator: {e:#}");
                    }
                }
                Ok(Msg::Hide) => {
                    self.visible = false;
                    let _ = self.d.conn.unmap_window(self.window);
                    let _ = self.d.conn.flush();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(Some(event)) = self.d.conn.poll_for_event() {
                if let (Event::Expose(e), true) = (&event, self.visible) {
                    if e.count == 0 {
                        let _ = self.paint();
                    }
                }
            }
        }
    }

    /// The monitor under the pointer as (x, y, width, height); the whole screen if RandR says nothing.
    fn monitor(&self) -> (i32, i32, i32, i32) {
        let screen = &self.d.conn.setup().roots[self.d.screen];
        let whole = (0, 0, screen.width_in_pixels as i32, screen.height_in_pixels as i32);
        let pointer = self.d.conn.query_pointer(self.d.root).ok().and_then(|c| c.reply().ok());
        let (px, py) = pointer.map_or((0, 0), |p| (p.root_x as i32, p.root_y as i32));
        let monitors = self.d.conn.randr_get_monitors(self.d.root, true).ok().and_then(|c| c.reply().ok()).map(|r| r.monitors);
        let rect = |m: &x11rb::protocol::randr::MonitorInfo| (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
        monitors
            .and_then(|ms| {
                let under = ms.iter().find(|m| {
                    let (x, y, w, h) = rect(m);
                    (x..x + w).contains(&px) && (y..y + h).contains(&py)
                });
                under.or_else(|| ms.iter().find(|m| m.primary)).or(ms.first()).map(rect)
            })
            .unwrap_or(whole)
    }

    fn show(&mut self, kind: Kind, text: &str, hint: Option<&str>) -> Result<()> {
        let (mx, my, mw, mh) = self.monitor();
        let img = render(&self.text, kind, text, hint, self.scale, mw);
        let x = mx + (mw - img.w) / 2;
        let y = my + mh - img.h - (28.0 * self.scale) as i32;
        let aux = xproto::ConfigureWindowAux::new()
            .x(x)
            .y(y)
            .width(img.w as u32)
            .height(img.h as u32)
            .stack_mode(StackMode::ABOVE);
        self.d.conn.configure_window(self.window, &aux)?;
        if !self.argb && self.shaped {
            self.d.conn.shape_rectangles(
                shape::SO::SET,
                shape::SK::BOUNDING,
                xproto::ClipOrdering::UNSORTED,
                self.window,
                0,
                0,
                &outline(&img),
            )?;
        }
        self.image = Some(img);
        self.visible = true;
        self.d.conn.map_window(self.window)?;
        self.paint()
    }

    /// Draw the pill into the window, a band of rows per request (the X protocol caps request sizes).
    fn paint(&self) -> Result<()> {
        let Some(img) = &self.image else { return Ok(()) };
        let rows_per_request = (60_000 / (img.w as usize * 4)).max(1);
        for (n, rows) in img.pixels.chunks(img.w as usize * rows_per_request).enumerate() {
            let data: Vec<u8> = rows.iter().flat_map(|px| px.to_le_bytes()).collect();
            let height = rows.len() / img.w as usize;
            let y = n * rows_per_request;
            self.d.conn.put_image(ImageFormat::Z_PIXMAP, self.window, self.gc, img.w as u16, height as u16, 0, y as i16, 0, self.depth, &data)?;
        }
        self.d.conn.flush()?;
        Ok(())
    }
}

/// The pill's outline as rectangles: for every row, the runs of pixels that are mostly opaque.
fn outline(img: &Image) -> Vec<xproto::Rectangle> {
    let mut rects = Vec::new();
    for (y, row) in img.pixels.chunks(img.w as usize).enumerate() {
        let mut start = None;
        for (x, px) in row.iter().chain(&[0]).enumerate() {
            let solid = x < row.len() && px >> 24 >= 128;
            match (solid, start) {
                (true, None) => start = Some(x),
                (false, Some(from)) => {
                    rects.push(xproto::Rectangle { x: from as i16, y: y as i16, width: (x - from) as u16, height: 1 });
                    start = None;
                }
                _ => {}
            }
        }
    }
    rects
}

/// Device pixels per logical pixel from `Xft.dpi` (96 dpi = 1), as desktops set it for HiDPI screens.
fn dpi_scale(d: &Display) -> f32 {
    let resources = d
        .conn
        .get_property(false, d.root, xproto::AtomEnum::RESOURCE_MANAGER, xproto::AtomEnum::STRING, 0, 100_000)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
        .unwrap_or_default();
    let dpi = resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))
        .and_then(|v| v.trim().parse::<f32>().ok())
        .unwrap_or(96.0);
    (dpi / 96.0).round().max(1.0)
}

#[cfg(test)]
mod tests {
    use x11rb::protocol::xproto::MapState;

    use super::*;

    #[test]
    fn the_indicator_window_shows_and_hides() {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        let Ok(mut overlay) = XOverlay::connect() else { return }; // no X server or no font
        let state = |o: &XOverlay| o.d.conn.get_window_attributes(o.window).unwrap().reply().unwrap().map_state;
        overlay.show(Kind::Recording, "Hello from the indicator", Some("a hint")).unwrap();
        overlay.d.conn.get_input_focus().unwrap().reply().unwrap();
        assert_eq!(state(&overlay), MapState::VIEWABLE);
        let geometry = overlay.d.conn.get_geometry(overlay.window).unwrap().reply().unwrap();
        let img = overlay.image.as_ref().unwrap();
        assert_eq!((geometry.width as i32, geometry.height as i32), (img.w, img.h));
        assert!(!outline(img).is_empty());
        overlay.visible = false;
        overlay.d.conn.unmap_window(overlay.window).unwrap();
        overlay.d.conn.get_input_focus().unwrap().reply().unwrap();
        assert_eq!(state(&overlay), MapState::UNMAPPED);
    }
}
