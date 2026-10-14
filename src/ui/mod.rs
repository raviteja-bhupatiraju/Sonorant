//! The Dear ImGui settings dialog: a winit window with an OpenGL context,
//! created on demand from the tray menu and destroyed when closed.

use std::num::NonZeroU32;
use std::time::Instant;

use anyhow::{anyhow, Result};
use glutin::config::{Config as GlConfig, ConfigTemplateBuilder, GlConfig as _};
use glutin::context::{ContextAttributesBuilder, NotCurrentGlContext, PossiblyCurrentContext};
use glutin::display::{Display, DisplayApiPreference, GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SurfaceAttributesBuilder, SwapInterval, WindowSurface};
use imgui::{Condition, FontConfig, FontSource, StyleColor, Ui, WindowFlags};
use imgui_glow_renderer::glow::{self, HasContext};
use imgui_glow_renderer::AutoRenderer;
use imgui_winit_support::{HiDpiMode, WinitPlatform};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

pub mod icon;
pub mod overlay;

use crate::config::{Config, ModelVariant, PasteMethod, SttMode, SttModel};
use crate::hotkey::HotkeySpec;
use crate::history::Entry;
use crate::models::Progress;
use crate::stt::{SttState, SttStatus};
use crate::tts::{EngineState, Metrics};

/// Read-only data the dialog shows, refreshed every frame by the app.
pub struct Snapshot {
    pub state: EngineState,
    pub voices: Vec<String>,
    pub last: Option<Metrics>,
    pub download: Progress,
    /// Registration errors for [speak, stop, dictate] hotkeys.
    pub hotkey_errors: [Option<String>; 4],
    pub stt: SttStatus,
    pub stt_download: Progress,
    pub history: std::sync::Arc<Vec<Entry>>,
}

pub enum Action {
    None,
    /// Apply and persist, then close.
    Save(Config),
    /// Apply and persist, keep the dialog open.
    Apply(Config),
    /// Discard changes and close.
    Cancel,
    /// Speak a sample using the (unsaved) voice / speed / volume.
    Preview(Config),
    /// Retry loading the model after an error.
    Retry,
    /// Retry loading the speech recognition model.
    RetryStt,
    ClearHistory,
    /// Put text on the clipboard.
    Copy(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Recording {
    Speak,
    Stop,
    Dictate,
    DictateSubmit,
}

struct UiState {
    draft: Config,
    recording: Option<Recording>,
    /// Feedback about the last recording attempt.
    note: Option<String>,
    devices: Vec<(String, String)>,
    inputs: Vec<(String, String)>,
    /// Tab to select on the first frame (command-line `--tab`).
    select_tab: Option<String>,
}

pub struct SettingsWindow {
    // Field order matters: GL resources must drop before the context/window.
    renderer: AutoRenderer,
    imgui: imgui::Context,
    platform: WinitPlatform,
    surface: Surface<WindowSurface>,
    context: PossiblyCurrentContext,
    window: Window,
    last_frame: Instant,
    modifiers: ModifiersState,
    state: UiState,
    close_requested: bool,
    clear: f32,
}

impl SettingsWindow {
    pub fn new(event_loop: &ActiveEventLoop, config: Config) -> Result<Self> {
        let icon = winit::window::Icon::from_rgba(crate::ui::icon::icon_rgba(64, true), 64, 64).ok();
        let attrs = Window::default_attributes()
            .with_title("Sonorant Settings")
            .with_inner_size(LogicalSize::new(560.0, 470.0))
            .with_resizable(false)
            .with_enabled_buttons(winit::window::WindowButtons::CLOSE)
            .with_window_icon(icon);
        let (window, gl_config) = create_gl_window(event_loop, attrs)?;
        let handle = window.window_handle()?.as_raw();

        let context_attribs = ContextAttributesBuilder::new().build(Some(handle));
        let context = unsafe { gl_config.display().create_context(&gl_config, &context_attribs)? };
        let size = window.inner_size();
        let surface_attribs = SurfaceAttributesBuilder::<WindowSurface>::new().with_srgb(Some(true)).build(
            handle,
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
        let surface = unsafe { gl_config.display().create_window_surface(&gl_config, &surface_attribs)? };
        let context = context.make_current(&surface)?;
        let _ = surface.set_swap_interval(&context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()));

        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| context.display().get_proc_address(s).cast()) };

        let mut imgui = imgui::Context::create();
        imgui.set_ini_filename(None);
        let mut platform = WinitPlatform::new(&mut imgui);
        platform.attach_window(imgui.io_mut(), &window, HiDpiMode::Default);
        let scale = platform.hidpi_factor() as f32;
        install_font(&mut imgui, 15.0 * scale);
        imgui.io_mut().font_global_scale = 1.0 / scale;
        let light = crate::platform::prefers_light_theme();
        style(&mut imgui, light);

        let renderer = AutoRenderer::new(gl, &mut imgui).map_err(|e| anyhow!("init renderer: {e}"))?;

        Ok(Self {
            renderer,
            imgui,
            platform,
            surface,
            context,
            window,
            last_frame: Instant::now(),
            modifiers: ModifiersState::empty(),
            state: UiState {
                draft: config,
                recording: None,
                note: None,
                devices: crate::audio::list_output_devices(),
                inputs: crate::stt::recorder::list_input_devices(),
                select_tab: std::env::args().skip_while(|a| a != "--tab").nth(1),
            },
            close_requested: false,
            clear: if light { 0.96 } else { 0.11 },
        })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn focus(&self) {
        self.window.set_minimized(false);
        self.window.focus_window();
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    pub fn wants_close(&self) -> bool {
        self.close_requested
    }

    /// Feed a winit window event to the dialog.
    pub fn handle_event(&mut self, event: WindowEvent) {
        match &event {
            WindowEvent::CloseRequested => self.close_requested = true,
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                self.surface.resize(
                    &self.context,
                    NonZeroU32::new(size.width).unwrap(),
                    NonZeroU32::new(size.height).unwrap(),
                );
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event: key, .. }
                if key.state == ElementState::Pressed && !key.repeat && self.state.recording.is_some() =>
            {
                if let PhysicalKey::Code(code) = key.physical_key {
                    self.record_key(code);
                }
                return; // don't let imgui see the keystroke while recording
            }
            _ => {}
        }
        let rescaled = matches!(event, WindowEvent::ScaleFactorChanged { .. });
        let wrapped = Event::<()>::WindowEvent { window_id: self.window.id(), event };
        self.platform.handle_event(self.imgui.io_mut(), &self.window, &wrapped);
        if rescaled {
            self.rebuild_font();
        }
    }

    /// Re-rasterize the font for the new DPI scale (e.g. the window moved to another monitor)
    /// and re-upload the atlas into the renderer's existing font texture.
    fn rebuild_font(&mut self) {
        let scale = self.platform.hidpi_factor() as f32;
        let tex_id = self.imgui.fonts().tex_id;
        self.imgui.fonts().clear();
        install_font(&mut self.imgui, 15.0 * scale);
        self.imgui.io_mut().font_global_scale = 1.0 / scale;
        let Some(texture) = self.renderer.renderer().font_atlas_texture else { return };
        let fonts = self.imgui.fonts();
        let atlas = fonts.build_rgba32_texture();
        unsafe {
            // Same format as imgui_glow_renderer's own atlas upload.
            let gl = self.renderer.gl_context();
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::SRGB8_ALPHA8 as _,
                atlas.width as _,
                atlas.height as _,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                Some(atlas.data),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        fonts.tex_id = tex_id;
        self.window.request_redraw();
    }

    fn record_key(&mut self, code: KeyCode) {
        let Some(target) = self.state.recording else { return };
        if code == KeyCode::Escape {
            // Esc cancels the capture, except for the stop key, where Esc itself is a valid choice
            // (clicking the button again cancels instead).
            if target == Recording::Stop {
                self.state.draft.stop_hotkey = Some(HotkeySpec::plain("Escape"));
            }
            self.state.recording = None;
            return;
        }
        let m = self.modifiers;
        let Some(spec) = HotkeySpec::from_winit(m.control_key(), m.alt_key(), m.shift_key(), m.super_key(), code) else {
            if !HotkeySpec::is_modifier(code) {
                // A bare modifier means we are still waiting for the actual key.
                log::info!("hotkey rejected: {code:?} with {m:?}");
                self.state.note = Some(format!("{code:?} needs at least one modifier (Ctrl/Alt/Shift/Win), or use an F-key."));
            }
            return;
        };
        self.state.note = None;
        match target {
            Recording::Speak => self.state.draft.hotkey = spec,
            Recording::Stop => self.state.draft.stop_hotkey = Some(spec),
            Recording::Dictate => self.state.draft.stt_hotkey = spec,
            Recording::DictateSubmit => self.state.draft.stt_submit_hotkey = Some(spec),
        }
        self.state.recording = None;
    }

    /// Render one frame and return what the user asked for.
    pub fn redraw(&mut self, snap: &Snapshot) -> Action {
        let now = Instant::now();
        self.imgui.io_mut().update_delta_time(now - self.last_frame);
        self.last_frame = now;
        let _ = self.platform.prepare_frame(self.imgui.io_mut(), &self.window);

        let ui = self.imgui.frame();
        let action = draw(ui, &mut self.state, snap);
        self.platform.prepare_render(ui, &self.window);
        let draw_data = self.imgui.render();

        unsafe {
            let gl = self.renderer.gl_context();
            gl.clear_color(self.clear, self.clear, self.clear, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        if let Err(e) = self.renderer.render(draw_data) {
            log::error!("imgui render failed: {e}");
        }
        let _ = self.surface.swap_buffers(&self.context);
        action
    }
}

/// Create the window and choose its GL config, like `glutin_winit::DisplayBuilder` (whose config
/// picker cannot fail, so an empty config list would panic), preferring an sRGB-capable config
/// to match the sRGB surface requested in `SettingsWindow::new`.
fn create_gl_window(event_loop: &ActiveEventLoop, attrs: WindowAttributes) -> Result<(Window, GlConfig)> {
    let display_handle = event_loop.display_handle()?.as_raw();
    let pick = |configs: Box<dyn Iterator<Item = GlConfig> + '_>| {
        configs
            .reduce(|a, b| if b.srgb_capable() && !a.srgb_capable() { b } else { a })
            .ok_or_else(|| anyhow!("no suitable OpenGL configuration found"))
    };
    #[cfg(windows)]
    {
        // WGL needs the window to choose a config.
        let window = event_loop.create_window(attrs).map_err(|e| anyhow!("create window: {e}"))?;
        let handle = window.window_handle()?.as_raw();
        let display = unsafe { Display::new(display_handle, DisplayApiPreference::Wgl(Some(handle)))? };
        let template = ConfigTemplateBuilder::new().compatible_with_native_window(handle).build();
        let config = pick(unsafe { display.find_configs(template)? })?;
        Ok((window, config))
    }
    #[cfg(not(windows))]
    {
        // Same preference as glutin_winit's default: GLX on X11, EGL otherwise (Wayland).
        let preference = DisplayApiPreference::GlxThenEgl(Box::new(winit::platform::x11::register_xlib_error_hook));
        let display = unsafe { Display::new(display_handle, preference)? };
        let config = pick(unsafe { display.find_configs(ConfigTemplateBuilder::new().build())? })?;
        let window = glutin_winit::finalize_window(event_loop, attrs, &config).map_err(|e| anyhow!("create window: {e}"))?;
        Ok((window, config))
    }
}

fn install_font(imgui: &mut imgui::Context, size_pixels: f32) {
    let font = crate::platform::ui_font();
    let sources = match &font {
        Some(data) => vec![FontSource::TtfData {
            data,
            size_pixels,
            config: Some(FontConfig { oversample_h: 2, oversample_v: 2, ..FontConfig::default() }),
        }],
        None => vec![FontSource::DefaultFontData {
            config: Some(FontConfig { size_pixels, ..FontConfig::default() }),
        }],
    };
    imgui.fonts().add_font(&sources);
}

/// Windows 11 style palette (light or dark, following the system setting).
fn style(imgui: &mut imgui::Context, light: bool) {
    let s = imgui.style_mut();
    if light {
        s.use_light_colors();
    } else {
        s.use_dark_colors();
    }
    s.window_rounding = 0.0;
    s.child_rounding = 0.0;
    s.frame_rounding = 4.0;
    s.grab_rounding = 3.0;
    s.popup_rounding = 4.0;
    s.scrollbar_rounding = 4.0;
    s.tab_rounding = 4.0;
    s.window_padding = [14.0, 12.0];
    s.frame_padding = [8.0, 3.0];
    s.item_spacing = [8.0, 5.0];
    s.item_inner_spacing = [6.0, 4.0];
    s.scrollbar_size = 10.0;
    s.frame_border_size = 1.0;
    s.popup_border_size = 1.0;

    // (page, field, field hover, button, button hover, button pressed, border, text, dim text, grab, tab)
    let p = if light {
        (0.953, 1.000, 0.975, 0.985, 0.925, 0.870, 0.780, 0.090, 0.420, 0.480, 0.905)
    } else {
        (0.125, 0.200, 0.240, 0.200, 0.245, 0.165, 0.310, 0.950, 0.600, 0.680, 0.165)
    };
    let g = |v: f32| [v, v, v, 1.0];
    let set = |s: &mut imgui::Style, c: StyleColor, col: [f32; 4]| s.colors[c as usize] = col;
    set(s, StyleColor::WindowBg, g(p.0));
    set(s, StyleColor::ChildBg, g(p.0));
    set(s, StyleColor::PopupBg, g(if light { 0.990 } else { 0.170 }));
    set(s, StyleColor::FrameBg, g(p.1));
    set(s, StyleColor::FrameBgHovered, g(p.2));
    set(s, StyleColor::FrameBgActive, g(p.2));
    set(s, StyleColor::Button, g(p.3));
    set(s, StyleColor::ButtonHovered, g(p.4));
    set(s, StyleColor::ButtonActive, g(p.5));
    set(s, StyleColor::Header, g(p.4));
    set(s, StyleColor::HeaderHovered, g(p.4));
    set(s, StyleColor::HeaderActive, g(p.5));
    set(s, StyleColor::Border, g(p.6));
    set(s, StyleColor::Separator, g(p.6));
    set(s, StyleColor::Text, g(p.7));
    set(s, StyleColor::TextDisabled, g(p.8));
    set(s, StyleColor::SliderGrab, g(p.9));
    set(s, StyleColor::SliderGrabActive, g(p.9 + if light { -0.15 } else { 0.15 }));
    set(s, StyleColor::CheckMark, g(p.7));
    set(s, StyleColor::ScrollbarBg, g(p.0));
    set(s, StyleColor::ScrollbarGrab, g(p.6));
    set(s, StyleColor::ScrollbarGrabHovered, g(p.9));
    set(s, StyleColor::ScrollbarGrabActive, g(p.9));
    set(s, StyleColor::PlotHistogram, g(p.9));
    set(s, StyleColor::TextSelectedBg, [p.9, p.9, p.9, 0.35]);
    set(s, StyleColor::Tab, g(p.10));
    set(s, StyleColor::TabHovered, g(p.4));
    set(s, StyleColor::TabActive, g(p.0));
    set(s, StyleColor::TabUnfocused, g(p.10));
    set(s, StyleColor::TabUnfocusedActive, g(p.0));
}

const GREEN: [f32; 4] = [0.28, 0.72, 0.38, 1.0];
const AMBER: [f32; 4] = [0.93, 0.68, 0.16, 1.0];
const RED: [f32; 4] = [0.88, 0.32, 0.32, 1.0];
const LABEL_W: f32 = 112.0;

fn dim(ui: &Ui) -> [f32; 4] {
    ui.style_color(StyleColor::TextDisabled)
}

/// Section heading with a rule, as in a Windows group box.
fn heading(ui: &Ui, title: &str) {
    ui.dummy([0.0, 2.0]);
    ui.text(title);
    ui.separator();
}

fn label(ui: &Ui, text: &str) {
    ui.align_text_to_frame_padding();
    ui.text(text);
    ui.same_line_with_pos(LABEL_W);
}

/// Coloured status dot followed by text.
fn status_line(ui: &Ui, color: [f32; 4], text: &str) {
    let p = ui.cursor_screen_pos();
    let r = 4.5;
    ui.get_window_draw_list()
        .add_circle([p[0] + r + 1.0, p[1] + ui.text_line_height() / 2.0 + 1.0], r, color)
        .filled(true)
        .build();
    let c = ui.cursor_pos();
    ui.set_cursor_pos([c[0] + 18.0, c[1]]);
    ui.text(text);
}

fn voice_name(id: &str) -> String {
    let name = id.split('_').nth(1).unwrap_or(id);
    let mut chars = name.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

fn voice_gender(id: &str) -> &'static str {
    if id.chars().nth(1) == Some('f') {
        "Female"
    } else {
        "Male"
    }
}

fn voice_display(id: &str) -> String {
    format!("{} ({})", voice_name(id), voice_gender(id).to_lowercase())
}

/// A button showing the shortcut for `target`; clicking it starts (or cancels) capturing a new one.
fn hotkey_button(ui: &Ui, st: &mut UiState, target: Recording, current: String) {
    let recording = st.recording == Some(target);
    // For the stop key, Esc itself is a valid choice, so cancelling is done by clicking again.
    let cancel = if target == Recording::Stop { "click to cancel" } else { "Esc cancels" };
    let text = if recording { format!("Press a shortcut... ({cancel})") } else { current };
    if ui.button_with_size(format!("{text}##{target:?}"), [220.0, 0.0]) {
        st.recording = (!recording).then_some(target);
        st.note = None;
    }
}

/// A line of text aligned with the controls (right of the labels).
fn indented_text(ui: &Ui, color: [f32; 4], text: &str) {
    ui.set_cursor_pos([LABEL_W, ui.cursor_pos()[1]]);
    ui.text_colored(color, text);
}

fn hotkey_error(ui: &Ui, error: &Option<String>) {
    if let Some(e) = error {
        indented_text(ui, RED, e);
    }
}

/// Device picker: "System default" (empty id) followed by `devices` as (id, name).
fn device_combo(ui: &Ui, id: &str, selected: &mut String, devices: &[(String, String)]) {
    ui.set_next_item_width(-1.0);
    let current = devices.iter().find(|(d, _)| d == selected).map_or("System default", |(_, n)| n.as_str());
    if let Some(_combo) = ui.begin_combo(id, current) {
        if ui.selectable_config("System default").selected(selected.is_empty()).build() {
            selected.clear();
        }
        for (d, name) in devices {
            if ui.selectable_config(format!("{name}##{d}")).selected(d == selected).build() {
                *selected = d.clone();
            }
        }
    }
}

/// An integer slider bound to a `u32` setting.
fn u32_slider(ui: &Ui, id: &str, max: i32, format: &str, value: &mut u32) {
    ui.set_next_item_width(-1.0);
    let mut v = *value as i32;
    if ui.slider_config(id, 0, max).display_format(format).build(&mut v) {
        *value = v.max(0) as u32;
    }
}

fn tab_general(ui: &Ui, st: &mut UiState, snap: &Snapshot, _: &mut Action) {
    heading(ui, "Hotkeys");
    if let Some(n) = &st.note {
        ui.text_colored(AMBER, n);
    }
    label(ui, "Speak selection");
    hotkey_button(ui, st, Recording::Speak, st.draft.hotkey.label());
    hotkey_error(ui, &snap.hotkey_errors[0]);
    label(ui, "Stop speaking");
    let stop = st.draft.stop_hotkey.as_ref().map_or_else(|| "Not set".into(), HotkeySpec::label);
    hotkey_button(ui, st, Recording::Stop, stop);
    ui.same_line();
    if ui.button("Clear") {
        st.draft.stop_hotkey = None;
        st.recording = None;
    }
    hotkey_error(ui, &snap.hotkey_errors[1]);
    if crate::platform::esc_on_demand() && st.draft.stop_hotkey.as_ref().is_some_and(HotkeySpec::is_plain_escape) {
        indented_text(ui, dim(ui), "Esc is only claimed while Sonorant is speaking or dictating.");
    }
    if let Some(note) = crate::platform::hotkey_note() {
        dim_wrapped(ui, note);
    }

    ui.dummy([0.0, 6.0]);
    heading(ui, "Behavior");
    ui.checkbox("Stream sentence by sentence (lowest latency)", &mut st.draft.streaming);
    ui.checkbox("Read the clipboard if nothing is selected", &mut st.draft.clipboard_fallback);
    // On Linux the selection is read directly (the primary selection), without the clipboard.
    if cfg!(windows) {
        ui.checkbox("Restore the clipboard after copying the selection", &mut st.draft.restore_clipboard);
    }
    ui.checkbox("Start Sonorant when I sign in", &mut st.draft.start_with_windows);
}

fn tab_voice(ui: &Ui, st: &mut UiState, snap: &Snapshot, action: &mut Action) {
    heading(ui, "Voice");
    label(ui, "Voice");
    ui.set_next_item_width(ui.content_region_avail()[0] - 80.0);
    let preview = if snap.voices.is_empty() { st.draft.voice.clone() } else { voice_display(&st.draft.voice) };
    if let Some(_combo) = ui.begin_combo("##voice", preview) {
        let mut group = "";
        for id in &snap.voices {
            let lang = voice_gender(id);
            if lang != group {
                group = lang;
                ui.text_colored(dim(ui), lang);
            }
            let selected = *id == st.draft.voice;
            if ui.selectable_config(format!("  {}##{id}", voice_name(id))).selected(selected).build() {
                st.draft.voice = id.clone();
            }
            if selected {
                ui.set_item_default_focus();
            }
        }
    }
    ui.same_line();
    if ui.button_with_size("Preview", [72.0, 0.0]) {
        *action = Action::Preview(st.draft.clone());
    }
    label(ui, "Speed");
    ui.set_next_item_width(-1.0);
    ui.slider_config("##speed", 0.5, 2.0).display_format("%.2fx").build(&mut st.draft.speed);
    label(ui, "Volume");
    ui.set_next_item_width(-1.0);
    let mut pct = st.draft.volume * 100.0;
    if ui.slider_config("##volume", 0.0, 150.0).display_format("%.0f%%").build(&mut pct) {
        st.draft.volume = pct / 100.0;
    }

    ui.dummy([0.0, 6.0]);
    heading(ui, "Audio output");
    label(ui, "Device");
    device_combo(ui, "##device", &mut st.draft.output_device, &st.devices);
}

fn tab_engine(ui: &Ui, st: &mut UiState, snap: &Snapshot, action: &mut Action) {
    heading(ui, "Kokoro-82M text-to-speech model");
    label(ui, "Kokoro model");
    combo_row(ui, "##model", &mut st.draft.model, &ModelVariant::ALL, ModelVariant::label);
    label(ui, "CPU threads");
    u32_slider(ui, "##threads", 16, "%d  (0 = automatic)", &mut st.draft.threads);
    indented_text(ui, dim(ui), "Changing these reloads the model when applied.");

    ui.dummy([0.0, 6.0]);
    heading(ui, "Status");
    match &snap.state {
        EngineState::Ready => status_line(ui, GREEN, "Model loaded"),
        EngineState::Starting | EngineState::Loading => status_line(ui, AMBER, "Loading model..."),
        EngineState::Downloading => status_line(ui, AMBER, "Downloading model files (first run only)"),
        EngineState::Error(e) => {
            status_line(ui, RED, "Model failed to load");
            ui.text_wrapped(e);
            if ui.button("Retry") {
                *action = Action::Retry;
            }
        }
    }
    if let Some(m) = &snap.last {
        ui.text_colored(
            dim(ui),
            format!("Last utterance: first audio in {} ms, real-time factor {:.2}", m.first_audio_ms, m.rtf()),
        );
    }
}

fn tab_about(ui: &Ui, _: &mut UiState, _: &Snapshot, _: &mut Action) {
    ui.set_window_font_scale(1.4);
    ui.text("Sonorant");
    ui.set_window_font_scale(1.0);
    ui.text_colored(dim(ui), format!("Version {}", env!("CARGO_PKG_VERSION")));
    ui.dummy([0.0, 4.0]);
    ui.text_wrapped("Reads your selected text aloud with a single hotkey, and types what you say into any application.");
    dim_wrapped(
        ui,
        "Everything runs on this computer: speech recognition and the voice models are downloaded once, then nothing is sent anywhere, and no account is needed.",
    );

    ui.dummy([0.0, 6.0]);
    heading(ui, "Author");
    ui.text("Ravi Teja Bhupatiraju");

    ui.dummy([0.0, 6.0]);
    heading(ui, "License");
    ui.text("MIT License, copyright (c) 2026 Ravi Teja Bhupatiraju");
    dim_wrapped(ui, "The components below keep their own licenses.");

    ui.dummy([0.0, 6.0]);
    heading(ui, "Built with");
    ui.text_wrapped("Text to speech: Kokoro-82M by hexgrad (Apache 2.0)");
    ui.text_wrapped("Speech recognition: Cactus Whistle model and Needle engine (Apache 2.0)");
    ui.text_wrapped("Speech recognition: NVIDIA Parakeet Unified and Canary models (CC-BY-4.0), run by transcribe.cpp and ggml (MIT)");
    ui.text_wrapped("Silence removal: Silero VAD (MIT)");
    ui.text_wrapped("Model runtime: ONNX Runtime by Microsoft (MIT)");
    ui.text_wrapped("Pronunciation: eSpeak NG (GPL v3)");
    ui.text_wrapped("Interface: Dear ImGui (MIT)");
}


fn dim_wrapped(ui: &Ui, text: &str) {
    let _c = ui.push_style_color(StyleColor::Text, dim(ui));
    ui.text_wrapped(text);
}

fn combo_row<T: Copy + PartialEq, L: AsRef<str>>(ui: &Ui, id: &str, current: &mut T, items: &[T], label_of: impl Fn(T) -> L) {
    ui.set_next_item_width(-1.0);
    if let Some(_c) = ui.begin_combo(id, label_of(*current)) {
        for (i, &item) in items.iter().enumerate() {
            // Never put three or more `#` in a row in a label: imgui reads `###` as "restart the ID
            // hash here", which would give every item the same ID (only the first stays clickable).
            if ui.selectable_config(format!("{}##opt{i}", label_of(item).as_ref())).selected(*current == item).build() {
                *current = item;
            }
        }
    }
}

fn tab_dictation(ui: &Ui, st: &mut UiState, snap: &Snapshot, action: &mut Action) {
    heading(ui, "Speech recognition");
    ui.checkbox("Enable speech recognition (dictation)", &mut st.draft.stt_enabled);
    dim_wrapped(ui, "Models download on first use (shared Hugging Face cache). The options below apply once it is enabled.");

    ui.dummy([0.0, 4.0]);
    label(ui, "Model");
    combo_row(ui, "##sttmodel", &mut st.draft.stt_model, &SttModel::ALL, |m| m.label());
    // Keep the language valid for the chosen model (and keep it if the new model supports it too).
    let langs = st.draft.stt_model.languages();
    if !langs.iter().any(|(c, _)| *c == st.draft.stt_language) {
        st.draft.stt_language = langs[0].0.to_owned();
    }
    if langs.len() > 1 {
        label(ui, "Language");
        ui.set_next_item_width(-1.0);
        let name = langs.iter().find(|(c, _)| *c == st.draft.stt_language).map(|(_, n)| *n).unwrap_or("English");
        if let Some(_c) = ui.begin_combo("##sttlang", name) {
            for (code, n) in langs {
                if ui.selectable_config(format!("{n}##lang")).selected(st.draft.stt_language == *code).build() {
                    st.draft.stt_language = (*code).to_owned();
                }
            }
        }
    }

    ui.dummy([0.0, 4.0]);
    heading(ui, "Recording");
    if let Some(n) = &st.note {
        ui.text_colored(AMBER, n);
    }
    label(ui, "Hotkey");
    hotkey_button(ui, st, Recording::Dictate, st.draft.stt_hotkey.label());
    hotkey_error(ui, &snap.hotkey_errors[2]);
    label(ui, "Finish and send");
    let submit = st.draft.stt_submit_hotkey.as_ref().map_or_else(|| "Not set".into(), HotkeySpec::label);
    hotkey_button(ui, st, Recording::DictateSubmit, submit);
    ui.same_line();
    if ui.button("Clear##submit") {
        st.draft.stt_submit_hotkey = None;
        st.recording = None;
    }
    hotkey_error(ui, &snap.hotkey_errors[3]);
    dim_wrapped(ui, "Ends the recording like the hotkey above, but adds no space after the text and presses Enter, to send a chat message. Starts a recording when none is running; pressed twice quickly it only presses Enter.");
    label(ui, "Mode");
    combo_row(ui, "##sttmode", &mut st.draft.stt_mode, &SttMode::ALL, |m| m.label());
    label(ui, "Microphone");
    device_combo(ui, "##mic", &mut st.draft.input_device, &st.inputs);

    ui.dummy([0.0, 2.0]);
    match &snap.stt.state {
        SttState::Disabled => dim_wrapped(ui, "Off"),
        SttState::Idle => status_line(ui, [0.55, 0.55, 0.6, 1.0], "Not loaded (loads when needed)"),
        SttState::Loading => status_line(ui, AMBER, "Loading model..."),
        SttState::Ready => {
            let device = if snap.stt.device.is_empty() { "Model loaded".to_owned() } else { format!("Model loaded ({})", snap.stt.device) };
            status_line(ui, GREEN, &device)
        }
        SttState::Downloading => {
            status_line(ui, AMBER, "Downloading the speech model");
            let d = &snap.stt_download;
            let frac = d.fraction();
            let overlay = format!("{}   {:.0} / {:.0} MB", d.file, d.done as f32 / 1e6, d.total as f32 / 1e6);
            imgui::ProgressBar::new(frac).size([-1.0, 0.0]).overlay_text(overlay).build(ui);
        }
        SttState::Error(e) => {
            status_line(ui, RED, "Speech model error");
            ui.text_wrapped(e);
            if ui.button("Retry") {
                *action = Action::RetryStt;
            }
        }
    }
    if let Some(m) = &snap.stt.last {
        dim_wrapped(ui, &format!("Last dictation: {:.1} s of audio transcribed in {} ms", m.audio_secs, m.millis));
    }
}

fn tab_insertion(ui: &Ui, st: &mut UiState, _: &Snapshot, _: &mut Action) {
    heading(ui, "Inserting the text");
    label(ui, "Method");
    combo_row(ui, "##paste", &mut st.draft.paste_method, &PasteMethod::ALL, |m| m.label());
    ui.checkbox("Restore the previous clipboard afterwards", &mut st.draft.paste_keep_clipboard);
    ui.checkbox("Add a space after the text", &mut st.draft.trailing_space);
    ui.checkbox("Press Enter after inserting", &mut st.draft.auto_submit);

    ui.dummy([0.0, 2.0]);
    heading(ui, "While dictating");
    ui.checkbox("Remove silence before transcribing (voice activity detection)", &mut st.draft.use_vad);
    ui.checkbox("Show the recording indicator", &mut st.draft.show_overlay);
    if st.draft.show_overlay {
        ui.checkbox("Show the text live while speaking", &mut st.draft.live_preview);
    }

    ui.dummy([0.0, 2.0]);
    heading(ui, "Advanced");
    label(ui, "Custom words");
    ui.set_next_item_width(-1.0);
    ui.input_text("##words", &mut st.draft.custom_words).hint("Names or terms to favour, separated by commas").build();
    label(ui, "Unload model");
    u32_slider(ui, "##unload", 60, "after %d min idle  (0 = never)", &mut st.draft.stt_unload_minutes);
}

fn tab_history(ui: &Ui, st: &mut UiState, snap: &Snapshot, action: &mut Action) {
    heading(ui, "Dictation history");
    label(ui, "Keep the last");
    ui.set_next_item_width(ui.content_region_avail()[0] - 96.0);
    let mut n = st.draft.history_limit as i32;
    if ui.slider_config("##histlimit", 0, 200).display_format("%d transcripts  (0 = off)").build(&mut n) {
        st.draft.history_limit = n.max(0) as u32;
    }
    ui.same_line();
    if ui.button_with_size("Clear", [86.0, 0.0]) {
        *action = Action::ClearHistory;
    }
    dim_wrapped(ui, "Transcripts are stored unencrypted in Sonorant's data folder. Set 0 to keep none.");
    ui.dummy([0.0, 2.0]);
    if snap.history.is_empty() {
        dim_wrapped(ui, "Nothing here yet. Dictated text appears here so you can copy it again.");
        return;
    }
    for (i, e) in snap.history.iter().enumerate() {
        let _id = ui.push_id_usize(i);
        ui.text_colored(dim(ui), &e.when);
        ui.same_line_with_pos(ui.window_content_region_max()[0] - 60.0);
        if ui.small_button("Copy") {
            *action = Action::Copy(e.text.clone());
        }
        ui.text_wrapped(&e.text);
        ui.separator();
    }
}

type TabFn = fn(&Ui, &mut UiState, &Snapshot, &mut Action);

const TABS: [(&str, TabFn); 7] = [
    ("General", tab_general),
    ("Voice", tab_voice),
    ("Dictation", tab_dictation),
    ("Insertion", tab_insertion),
    ("History", tab_history),
    ("Engine", tab_engine),
    ("About", tab_about),
];

fn draw(ui: &Ui, st: &mut UiState, snap: &Snapshot) -> Action {
    let mut action = Action::None;
    let [w, h] = ui.io().display_size;

    ui.window("##settings")
        .position([0.0, 0.0], Condition::Always)
        .size([w, h], Condition::Always)
        .flags(
            WindowFlags::NO_TITLE_BAR
                | WindowFlags::NO_RESIZE
                | WindowFlags::NO_MOVE
                | WindowFlags::NO_COLLAPSE
                | WindowFlags::NO_SAVED_SETTINGS
                | WindowFlags::NO_SCROLLBAR,
        )
        .build(|| {
            ui.child_window("##content").size([0.0, -42.0]).build(|| {
                let select = st.select_tab.take();
                let Some(_bar) = ui.tab_bar("##tabs") else { return };
                for (name, body) in TABS {
                    let mut item = imgui::TabItem::new(name);
                    if select.as_deref() == Some(name) {
                        item = item.flags(imgui::TabItemFlags::SET_SELECTED);
                    }
                    if let Some(_t) = item.begin(ui) {
                        ui.dummy([0.0, 2.0]);
                        body(ui, st, snap, &mut action);
                    }
                }
            });

            // ---- Footer: status on the left, standard OK / Cancel / Apply on the right ----
            ui.separator();
            let y = ui.cursor_pos()[1] + 3.0;
            ui.set_cursor_pos([ui.cursor_pos()[0], y + 2.0]);
            match &snap.state {
                EngineState::Ready => status_line(ui, GREEN, "Ready"),
                EngineState::Starting | EngineState::Loading => status_line(ui, AMBER, "Loading model..."),
                EngineState::Downloading => {
                    let d = &snap.download;
                    let frac = d.fraction();
                    ui.set_cursor_pos([14.0, y]);
                    imgui::ProgressBar::new(frac)
                        .size([240.0, 24.0])
                        .overlay_text(format!("{} {:.0}%", d.file, frac * 100.0))
                        .build(ui);
                }
                EngineState::Error(_) => status_line(ui, RED, "Model error (see Engine tab)"),
            }

            let (bw, gap) = (84.0, 8.0);
            ui.set_cursor_pos([w - 3.0 * bw - 2.0 * gap - 14.0, y]);
            {
                // OK is the default button: emphasised with a stronger border.
                let _bc = ui.push_style_color(StyleColor::Border, ui.style_color(StyleColor::Text));
                if ui.button_with_size("OK", [bw, 0.0]) {
                    action = Action::Save(st.draft.clone());
                }
            }
            ui.same_line_with_spacing(0.0, gap);
            if ui.button_with_size("Cancel", [bw, 0.0]) {
                action = Action::Cancel;
            }
            ui.same_line_with_spacing(0.0, gap);
            if ui.button_with_size("Apply", [bw, 0.0]) {
                action = Action::Apply(st.draft.clone());
            }
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::tts::EngineState;

    fn snapshot() -> Snapshot {
        Snapshot {
            state: EngineState::Ready,
            voices: vec!["af_heart".into(), "am_adam".into()],
            last: None,
            download: Progress::default(),
            hotkey_errors: [None, None, None],
            stt: SttStatus { state: SttState::Ready, last: None, device: String::new(), runs_since_load: 0 },
            stt_download: Progress::default(),
            history: Default::default(),
        }
    }

    fn state(tab: &str) -> UiState {
        UiState {
            draft: Config { stt_enabled: true, ..Config::default() },
            recording: None,
            note: None,
            devices: Vec::new(),
            inputs: Vec::new(),
            select_tab: Some(tab.to_owned()),
        }
    }

    /// Runs the real `draw` headlessly, feeding it simulated mouse input.
    struct Harness {
        ctx: imgui::Context,
        st: UiState,
        snap: Snapshot,
    }

    impl Harness {
        fn new(tab: &str) -> Self {
            let mut ctx = imgui::Context::create();
            ctx.set_ini_filename(None);
            ctx.io_mut().display_size = [560.0, 470.0];
            style(&mut ctx, true);
            let _ = ctx.fonts().build_rgba32_texture(); // the atlas must exist before a frame can run
            let mut h = Harness { ctx, st: state(tab), snap: snapshot() };
            for _ in 0..4 {
                h.frame();
            }
            h
        }

        fn frame(&mut self) {
            self.ctx.io_mut().update_delta_time(Duration::from_millis(16));
            let ui = self.ctx.frame();
            draw(ui, &mut self.st, &self.snap);
            self.ctx.render();
        }

        fn click(&mut self, x: f32, y: f32) {
            self.ctx.io_mut().add_mouse_pos_event([x, y]);
            self.frame();
            self.ctx.io_mut().add_mouse_button_event(imgui::MouseButton::Left, true);
            self.frame();
            self.ctx.io_mut().add_mouse_button_event(imgui::MouseButton::Left, false);
            self.frame();
            self.frame();
        }

        /// Whether any popup (combo list) is open. Safe between frames: with "any popup" flags
        /// imgui only looks at its popup stack.
        fn popup_open(&self, _id: &str) -> bool {
            const ANY_POPUP_ID_AND_LEVEL: i32 = (1 << 7) | (1 << 8);
            unsafe { imgui::sys::igIsPopupOpen(std::ptr::null(), ANY_POPUP_ID_AND_LEVEL) }
        }

        /// Open the dropdown `combo_id` (found by scanning for the row that opens it), then click at
        /// successive positions below it, re-opening it each time, until `changed` reports a
        /// different selection. Returns whether the selection changed.
        fn pick(&mut self, combo_id: &str, scan: std::ops::Range<i32>, changed: impl Fn(&UiState) -> bool) -> bool {
            let mut opened_at = None;
            for y in scan.step_by(3) {
                self.click(300.0, y as f32);
                if self.popup_open(combo_id) {
                    opened_at = Some(y as f32);
                    break;
                }
            }
            let y = opened_at.unwrap_or_else(|| panic!("could not open the dropdown {combo_id} by clicking"));
            for offset in (16..120).step_by(2) {
                if !self.popup_open(combo_id) {
                    self.click(300.0, y); // re-open
                    assert!(self.popup_open(combo_id), "dropdown {combo_id} would not re-open");
                }
                self.click(300.0, y + offset as f32);
                if changed(&self.st) {
                    return true;
                }
            }
            false
        }
    }

    /// imgui allows only one live context per process, so everything runs in a single test.
    #[test]
    fn dictation_dropdowns_change_their_selection() {
        let mut h = Harness::new("Dictation");
        assert_eq!(h.st.draft.stt_model, SttModel::Whistle);
        assert!(
            h.pick("##sttmodel", 100..200, |s| s.draft.stt_model != SttModel::Whistle),
            "Model dropdown did not change the model"
        );
        drop(h);

        let mut h = Harness::new("Dictation");
        assert_eq!(h.st.draft.stt_language, "en");
        assert!(
            h.pick("##sttlang", 165..260, |s| s.draft.stt_language != "en"),
            "Language dropdown did not change the language"
        );
        drop(h);

        let mut h = Harness::new("Dictation");
        let before = h.st.draft.stt_mode;
        assert!(
            h.pick("##sttmode", 200..300, |s| s.draft.stt_mode != before),
            "Mode dropdown did not change the mode"
        );
        drop(h);

        let mut h = Harness::new("Insertion");
        let before = h.st.draft.paste_method;
        assert!(
            h.pick("##paste", 60..140, |s| s.draft.paste_method != before),
            "Insertion method dropdown did not change the method"
        );
    }
}
