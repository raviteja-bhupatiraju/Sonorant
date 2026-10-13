//! Application glue: the winit event loop, commands from the tray / hotkeys (see `platform`),
//! dictation, and the on-demand settings window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::WindowId;

use crate::audio::AudioHandle;
use crate::config::{Config, PasteMethod, SttMode};
use crate::models;
use crate::ui::overlay;
use crate::platform::{self, Command, Hotkeys, Instance, Overlay, Tray};
use crate::stt::recorder::{Recorder, STT_RATE};
use crate::stt::{Outcome, Stt, SttEvent, SttState};
use crate::tts::{EngineState, Tts};
use crate::ui::{Action, SettingsWindow, Snapshot};

pub enum UserEvent {
    /// From the tray menu, a hotkey or another process.
    Command(Command),
    /// The TTS worker changed state (loading finished, error, ...).
    StatusChanged,
    /// The speech recognition worker changed state or finished a transcription.
    Stt(SttEvent),
    /// Hide the overlay if it still shows the notice with this serial number.
    HideOverlay(u64),
}

const SAMPLE_SENTENCE: &str = "Hello! This is how I sound. Press your hotkey on any selected text, and I will read it aloud.";

#[derive(Clone, Copy, PartialEq)]
enum Dictation {
    Idle,
    Recording { since: Instant },
    Processing,
}

pub struct App {
    cfg: Arc<RwLock<Config>>,
    /// Last persisted configuration (what the dialog reverts to on Cancel).
    saved: Config,
    tts: Tts,
    audio: AudioHandle,
    tray: Option<Tray>,
    hotkeys: Option<Hotkeys>,
    /// Keeps the single-instance claim (and on Linux the command socket) alive.
    instance: Instance,
    window: Option<SettingsWindow>,
    capturing: Arc<AtomicBool>,
    /// The last text read by "speak selection".
    last_spoken: Arc<parking_lot::Mutex<String>>,
    last_hotkey: Instant,
    open_settings_on_start: bool,
    // ---- speech recognition ----
    proxy: EventLoopProxy<UserEvent>,
    stt: Stt,
    recorder: Recorder,
    overlay: Option<Overlay>,
    dictation: Dictation,
    overlay_serial: u64,
}

/// Route callbacks from other threads (tray, hotkeys, workers) into the event loop.
fn forward<E: 'static>(proxy: &EventLoopProxy<UserEvent>, wrap: fn(E) -> UserEvent) -> impl Fn(E) + Send + Sync + 'static {
    let proxy = proxy.clone();
    move |e| {
        let _ = proxy.send_event(wrap(e));
    }
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>, instance: Instance, open_settings_on_start: bool) -> Self {
        let saved = Config::load();
        let cfg = Arc::new(RwLock::new(saved.clone()));

        let audio = AudioHandle::spawn(saved.output_device.clone(), saved.volume);
        let stt = Stt::spawn(cfg.clone(), Arc::new(forward(&proxy, UserEvent::Stt)));
        if saved.stt_enabled {
            stt.preload();
        }
        let status_changed = forward(&proxy, |()| UserEvent::StatusChanged);
        let tts = Tts::spawn(cfg.clone(), audio.clone(), Arc::new(move || status_changed(())));

        Self {
            cfg,
            saved,
            tts,
            audio,
            tray: None,
            hotkeys: None,
            instance,
            window: None,
            capturing: Arc::new(AtomicBool::new(false)),
            last_spoken: Arc::default(),
            last_hotkey: Instant::now() - Duration::from_secs(10),
            open_settings_on_start,
            proxy,
            stt,
            recorder: Recorder::spawn(),
            overlay: None,
            dictation: Dictation::Idle,
            overlay_serial: 0,
        }
    }

    // ---- hotkeys ------------------------------------------------------------

    fn register_hotkeys(&mut self) {
        if let Some(hk) = &mut self.hotkeys {
            hk.register(&self.cfg.read());
        }
    }

    fn on_command(&mut self, event_loop: &ActiveEventLoop, cmd: Command) {
        match cmd {
            Command::Settings => self.open_settings(event_loop),
            Command::Speak => {
                // Ignore key auto-repeat and re-entry while a capture is in flight.
                let repeat = self.last_hotkey.elapsed() < Duration::from_millis(350);
                self.last_hotkey = Instant::now();
                if !repeat {
                    self.speak_selection();
                }
            }
            Command::SpeakClipboard => {
                if let Some(text) = platform::clipboard_text() {
                    self.tts.speak(text, Instant::now());
                }
            }
            Command::Stop => {
                self.tts.stop();
                self.cancel_dictation();
            }
            Command::Dictate(pressed) => self.on_dictation_key(pressed),
            Command::Cancel => self.cancel_dictation(),
            Command::Quit => event_loop.exit(),
        }
    }

    fn speak_selection(&self) {
        if self.capturing.swap(true, Ordering::SeqCst) {
            return;
        }
        let tts = self.tts.clone();
        let capturing = self.capturing.clone();
        let last_spoken = self.last_spoken.clone();
        let (restore, fallback) = {
            let c = self.cfg.read();
            (c.restore_clipboard, c.clipboard_fallback)
        };
        std::thread::spawn(move || {
            match platform::selected_text(restore, fallback) {
                // Pressed again on the same (still selected) text while it is being read: stop.
                Some((text, _)) if platform::SELECTION_PERSISTS && tts.is_busy() && *last_spoken.lock() == text => {
                    tts.stop();
                }
                Some((text, t0)) => {
                    log::info!("speaking {} chars", text.chars().count());
                    *last_spoken.lock() = text.clone();
                    tts.speak(text, t0);
                }
                None => {
                    // Nothing selected: treat the hotkey as a toggle and stop any speech.
                    log::info!("no selection");
                    if tts.is_busy() {
                        tts.stop();
                    }
                }
            }
            capturing.store(false, Ordering::SeqCst);
        });
    }

    // ---- dictation ------------------------------------------------------------

    fn on_dictation_key(&mut self, pressed: bool) {
        let mode = self.cfg.read().stt_mode;
        match (mode, pressed, self.dictation) {
            (SttMode::PushToTalk, true, Dictation::Idle) => self.start_recording(),
            (SttMode::PushToTalk, false, Dictation::Recording { since }) => {
                if since.elapsed() < Duration::from_millis(250) {
                    // An accidental tap: nothing worth transcribing.
                    self.cancel_dictation();
                } else {
                    self.finish_recording();
                }
            }
            (SttMode::Toggle, true, Dictation::Idle) => self.start_recording(),
            (SttMode::Toggle, true, Dictation::Recording { .. }) => self.finish_recording(),
            _ => {}
        }
    }

    fn show_overlay(&mut self, kind: overlay::Kind, text: &str, hint: Option<&str>) {
        self.overlay_serial += 1; // a pending auto-hide must not hide newer content
        if !self.cfg.read().show_overlay {
            return;
        }
        if self.overlay.is_none() {
            self.overlay = Overlay::new();
        }
        if let Some(o) = &self.overlay {
            o.show(kind, text, hint);
        }
    }

    /// Show `text` for `millis`, then hide the overlay again (unless something newer replaced it).
    fn flash(&mut self, kind: overlay::Kind, text: &str, millis: u64) {
        self.show_overlay(kind, text, None);
        let (serial, proxy) = (self.overlay_serial, self.proxy.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(millis));
            let _ = proxy.send_event(UserEvent::HideOverlay(serial));
        });
    }

    /// Show a short message, then hide the overlay again.
    fn notice(&mut self, text: &str, secs: u64) {
        self.flash(overlay::Kind::Notice, text, secs * 1000);
    }

    fn hide_overlay(&mut self) {
        self.overlay_serial += 1;
        if let Some(o) = &self.overlay {
            o.hide();
        }
    }

    /// Claim the cancel key (Esc) while a dictation is in progress.
    fn set_esc(&mut self, on: bool) {
        if let Some(hk) = &mut self.hotkeys {
            hk.set_dictating(on, &self.cfg.read().stt_hotkey);
        }
    }

    fn start_recording(&mut self) {
        let state = self.stt.status.lock().state.clone();
        match state {
            SttState::Disabled => return,
            SttState::Error(_) => {
                self.notice("Speech model unavailable - see Settings", 3);
                return;
            }
            _ => {}
        }
        // Keep Sonorant speech out of the microphone.
        self.tts.stop();
        let (device, live, typing) = {
            let c = self.cfg.read();
            (c.input_device.clone(), c.show_overlay && c.live_preview, c.paste_method != PasteMethod::ClipboardOnly)
        };
        let audio = match self.recorder.start(&device) {
            Ok(audio) => audio,
            Err(e) => {
                log::error!("could not start recording: {e}");
                self.notice("Microphone unavailable", 3);
                return;
            }
        };
        self.dictation = Dictation::Recording { since: Instant::now() };
        // Load (or download) the model while the user is still speaking.
        self.stt.preload();
        if live {
            self.stt.live(audio);
        }
        if typing {
            platform::prepare_insert(); // may need the user's permission once (Wayland)
        }
        // If the model is still loading, say so quietly: the transcription will wait for it.
        let hint = match state {
            SttState::Idle | SttState::Loading | SttState::Downloading => Some("Loading the speech model..."),
            _ => None,
        };
        self.show_overlay(overlay::Kind::Recording, "Recording", hint);
        self.set_esc(true);
    }

    fn finish_recording(&mut self) {
        let pcm = self.recorder.stop();
        if pcm.len() < STT_RATE as usize / 4 {
            self.dictation = Dictation::Idle;
            self.set_esc(false);
            self.notice("Too short", 1);
            return;
        }
        log::info!("recorded {:.1}s", pcm.len() as f32 / STT_RATE as f32);
        self.dictation = Dictation::Processing;
        // The first transcription after a model load can be slow (GPU pipelines are prepared on first use).
        let first = self.stt.status.lock().runs_since_load == 0;
        let hint = first.then_some("The first transcription after loading can be slow");
        self.show_overlay(overlay::Kind::Working, "Transcribing...", hint);
        self.stt.transcribe(pcm);
    }

    fn cancel_dictation(&mut self) {
        if self.dictation == Dictation::Idle {
            return;
        }
        let _ = self.recorder.stop();
        self.stt.cancel();
        self.dictation = Dictation::Idle;
        self.set_esc(false);
        self.hide_overlay();
        log::info!("dictation cancelled");
    }

    fn on_stt_event(&mut self, event: SttEvent) {
        match event {
            SttEvent::StateChanged => self.refresh_tray(),
            SttEvent::Partial(text) => {
                if matches!(self.dictation, Dictation::Recording { .. }) && !text.is_empty() {
                    self.show_overlay(overlay::Kind::Recording, &text, None);
                }
            }
            SttEvent::Finished(outcome) => {
                if self.dictation != Dictation::Processing {
                    return; // cancelled in the meantime
                }
                self.dictation = Dictation::Idle;
                self.set_esc(false);
                match outcome {
                    Outcome::Cancelled => self.hide_overlay(),
                    Outcome::Inserted(text) => {
                        // The text is now in the application: get out of the way at once. Only when it
                        // was merely copied to the clipboard is it worth showing what was recognised.
                        if self.cfg.read().paste_method == PasteMethod::ClipboardOnly {
                            let millis = (1800 + text.chars().count() as u64 * 30).clamp(2200, 7000);
                            self.flash(overlay::Kind::Done, &text, millis);
                        } else {
                            self.hide_overlay();
                        }
                    }
                    Outcome::NoSpeech => self.notice("No speech detected", 2),
                    Outcome::Failed(e) => {
                        log::error!("dictation failed: {e}");
                        self.notice("Dictation failed - see Settings", 3);
                    }
                }
            }
        }
    }

    // ---- tray ---------------------------------------------------------------

    fn refresh_tray(&mut self) {
        let Some(tray) = self.tray.as_mut() else { return };
        let state = self.tts.status.lock().state.clone();
        let text = match state {
            EngineState::Ready => &format!("Ready - {} reads the selection", self.cfg.read().hotkey.label()),
            EngineState::Starting | EngineState::Loading => "Loading model...",
            EngineState::Downloading => "Downloading model files...",
            EngineState::Error(_) => "Model failed to load - open Settings",
        };
        tray.set_status(text, state == EngineState::Ready);
    }

    // ---- settings window ------------------------------------------------------

    fn open_settings(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.focus();
            return;
        }
        match SettingsWindow::new(event_loop, self.saved.clone()) {
            Ok(w) => {
                w.focus();
                self.window = Some(w);
            }
            Err(e) => log::error!("could not open settings window: {e:#}"),
        }
    }

    fn snapshot(&self) -> Snapshot {
        let st = self.tts.status.lock().clone();
        let errors = self.hotkeys.as_ref().map(Hotkeys::errors).unwrap_or_default();
        Snapshot {
            state: st.state,
            voices: st.voices,
            last: st.last,
            download: self.tts.download.lock().clone(),
            hotkey_errors: errors,
            stt: self.stt.status.lock().clone(),
            stt_download: self.stt.download.lock().clone(),
            history: crate::history::entries(),
        }
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Preview(draft) => {
                // Use the draft voice/speed/volume for the preview only.
                let Config { voice, speed, volume, streaming, .. } = draft;
                *self.cfg.write() = Config { voice, speed, volume, streaming, ..self.saved.clone() };
                self.audio.set_volume(volume);
                self.tts.speak(SAMPLE_SENTENCE.to_string(), Instant::now());
            }
            Action::Apply(c) => self.apply(c),
            Action::Save(c) => {
                self.apply(c);
                self.window = None;
            }
            Action::Cancel => {
                // Undo anything a preview changed.
                *self.cfg.write() = self.saved.clone();
                self.audio.set_volume(self.saved.volume);
                self.window = None;
            }
            Action::Retry => self.tts.reload(),
            Action::RetryStt => {
                self.stt.unload();
                self.stt.preload();
            }
            Action::ClearHistory => crate::history::clear(),
            Action::Copy(text) => platform::copy_text(text),
        }
    }

    fn apply(&mut self, new: Config) {
        let old = std::mem::replace(&mut self.saved, new.clone());
        *self.cfg.write() = new.clone();
        self.audio.set_volume(new.volume);
        if new.output_device != old.output_device {
            self.audio.set_device(new.output_device.clone());
        }
        let hotkeys = |c: &Config| (c.hotkey.clone(), c.stop_hotkey.clone(), c.stt_hotkey.clone(), c.stt_enabled);
        if hotkeys(&new) != hotkeys(&old) {
            self.register_hotkeys();
        }
        if !new.stt_enabled {
            self.cancel_dictation();
        }
        // Thread count and GPU use only apply to a freshly loaded model.
        let engine_changed = (new.threads, new.use_gpu) != (old.threads, old.use_gpu);
        if engine_changed {
            self.stt.unload();
        }
        if engine_changed || (new.stt_enabled, new.stt_model) != (old.stt_enabled, old.stt_model) {
            self.stt.preload(); // loads when enabled, unloads when disabled
        }
        crate::history::trim_to(new.history_limit);
        if new.start_with_windows != old.start_with_windows {
            if let Err(e) = platform::set_autostart(new.start_with_windows) {
                log::warn!("could not update autostart: {e:#}");
            }
        }
        if new.model != old.model || new.threads != old.threads {
            self.tts.reload();
        }
        if let Err(e) = new.save() {
            log::error!("could not save config: {e:#}");
        }
        self.refresh_tray();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _: &ActiveEventLoop, _: StartCause) {}

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        let sink: platform::CommandSink = Arc::new(forward(&self.proxy, UserEvent::Command));
        self.instance.listen(sink.clone());
        match Tray::new("Sonorant - starting", sink.clone()) {
            Ok(t) => self.tray = Some(t),
            Err(e) => log::error!("could not create tray icon: {e:#}"),
        }
        match Hotkeys::new(sink) {
            Ok(hk) => {
                self.hotkeys = Some(hk);
                self.register_hotkeys();
            }
            Err(e) => log::error!("could not set up global hotkeys: {e:#}"),
        }
        // Test hook: speak this text once the voice model is ready (used to test the stop key).
        if let Ok(text) = std::env::var("SONORANT_TEST_SPEAK") {
            let tts = self.tts.clone();
            std::thread::spawn(move || {
                while tts.status.lock().state != EngineState::Ready {
                    std::thread::sleep(Duration::from_millis(200));
                }
                std::thread::sleep(Duration::from_secs(3));
                tts.speak(text, Instant::now());
            });
        }
        self.refresh_tray();
        // First run: show the dialog so the model download is visible.
        if self.open_settings_on_start || !models::kokoro_ready(self.saved.model) {
            self.open_settings(event_loop);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Command(cmd) => self.on_command(event_loop, cmd),
            UserEvent::StatusChanged => self.refresh_tray(),
            UserEvent::Stt(e) => self.on_stt_event(e),
            UserEvent::HideOverlay(serial) => {
                if serial == self.overlay_serial {
                    self.hide_overlay();
                }
            }
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(win) = self.window.as_mut() else { return };
        if win.id() != id {
            return;
        }
        if matches!(event, WindowEvent::RedrawRequested) {
            let snap = self.snapshot();
            let action = self.window.as_mut().unwrap().redraw(&snap);
            self.handle_action(action);
            return;
        }
        win.handle_event(event);
        if win.wants_close() {
            self.handle_action(Action::Cancel);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let busy = self.tts.is_busy() || self.dictation != Dictation::Idle;
        let poll = self.hotkeys.as_mut().is_some_and(|h| h.set_busy(busy));
        match &self.window {
            // Check again soon, so a temporarily claimed stop key is released once speech ends.
            None if poll => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250)))
            }
            Some(w) => {
                // Live status needs periodic redraws, but ~30 fps is plenty; input events
                // wake the loop immediately, so the dialog still feels responsive.
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(33)));
                w.request_redraw();
            }
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.cancel_dictation();
        self.stt.shutdown();
        self.tts.shutdown();
    }
}
