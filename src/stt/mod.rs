//! Speech recognition worker. Owns the speech model (Cactus Whistle, or Parakeet Unified / Canary on
//! transcribe.cpp), loads it lazily, unloads it after an idle timeout, and turns recorded audio into
//! inserted text.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use transcribe_cpp::{
    Backend, Feature, Model, ModelOptions, ParakeetBufferedStreamOptions, RunOptions, Session, SessionOptions,
    StreamExtension, StreamOptions,
};

pub mod recorder;
pub mod vad;
pub mod whistle;

use crate::config::{Config, SttModel};
use crate::models::Progress;
use self::recorder::LiveAudio;
use crate::platform::{self, InsertOptions};
use self::whistle::Whistle;
use crate::history;
use crate::models::hub;
use self::vad::Vad;

const VAD_REPO: &str = "onnx-community/silero-vad";
const VAD_FILE: &str = "onnx/model.onnx";

#[derive(Clone, Debug, PartialEq)]
pub enum SttState {
    /// Speech recognition is switched off in the settings.
    Disabled,
    /// Enabled, model not in memory (not loaded yet, or unloaded after the idle timeout).
    Idle,
    Downloading,
    Loading,
    Ready,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct SttMetrics {
    pub audio_secs: f32,
    pub millis: u32,
}

#[derive(Clone, Debug)]
pub struct SttStatus {
    pub state: SttState,
    pub last: Option<SttMetrics>,
    /// Where the loaded model runs, e.g. "GPU: NVIDIA GeForce RTX 4090" or "CPU".
    pub device: String,
    /// Transcriptions finished since the model was last loaded (0 = the next one is the first).
    pub runs_since_load: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Inserted(String),
    NoSpeech,
    Failed(String),
    Cancelled,
}

pub enum SttEvent {
    StateChanged,
    /// The live transcript of the recording in progress.
    Partial(String),
    /// The transcription with this id (as returned by `Stt::transcribe`) is done.
    Finished(u64, Outcome),
}

pub type SttNotify = Arc<dyn Fn(SttEvent) + Send + Sync>;

enum Cmd {
    /// Load the model now (after enabling or changing the model).
    Preload,
    Unload,
    /// Transcribe a recording as it happens, reporting `SttEvent::Partial`s until it stops.
    Live(LiveAudio),
    Transcribe { pcm: Vec<f32>, id: u64, submit: bool, reply: Option<Sender<Result<String, String>>> },
    Quit,
}

#[derive(Clone)]
pub struct Stt {
    tx: Sender<Cmd>,
    pub status: Arc<Mutex<SttStatus>>,
    pub download: Arc<Mutex<Progress>>,
    /// Id of the transcription whose result may still be used; bumping it cancels pending ones.
    current: Arc<AtomicU64>,
}

impl Stt {
    pub fn spawn(cfg: Arc<RwLock<Config>>, notify: SttNotify) -> Self {
        let (tx, rx) = mpsc::channel();
        let enabled = cfg.read().stt_enabled;
        let status = Arc::new(Mutex::new(SttStatus {
            state: if enabled { SttState::Idle } else { SttState::Disabled },
            last: None,
            device: String::new(),
            runs_since_load: 0,
        }));
        let download = Arc::new(Mutex::new(Progress::default()));
        let current = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            rx,
            cfg,
            status: status.clone(),
            download: download.clone(),
            current: current.clone(),
            notify,
            loaded: None,
            vad: None,
            force_cpu: None,
        };
        std::thread::Builder::new().name("stt".into()).spawn(move || worker.run()).expect("spawn stt thread");
        Self { tx, status, download, current }
    }

    /// Load the model in the background (no-op if already loaded).
    pub fn preload(&self) {
        let _ = self.tx.send(Cmd::Preload);
    }

    pub fn unload(&self) {
        let _ = self.tx.send(Cmd::Unload);
    }

    fn send_transcribe(&self, pcm: Vec<f32>, submit: bool, reply: Option<Sender<Result<String, String>>>) -> u64 {
        let id = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(Cmd::Transcribe { pcm, id, submit, reply });
        id
    }

    /// Show a live transcript of the recording that `audio` delivers (preview only: the inserted
    /// text still comes from `transcribe` on the whole recording).
    pub fn live(&self, audio: LiveAudio) {
        let _ = self.tx.send(Cmd::Live(audio));
    }

    /// Transcribe 16 kHz mono audio and insert the text.
    /// Returns the id its `SttEvent::Finished` will carry.
    /// With `submit` the text goes in without a trailing space and Enter is pressed after it.
    pub fn transcribe(&self, pcm: Vec<f32>, submit: bool) -> u64 {
        self.send_transcribe(pcm, submit, None)
    }

    /// Transcribe and return the text instead of inserting it (command-line testing).
    pub fn transcribe_to_text(&self, pcm: Vec<f32>) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        self.send_transcribe(pcm, false, Some(reply));
        rx.recv().unwrap_or_else(|e| Err(e.to_string()))
    }

    /// Discard any transcription in flight; its text will not be inserted.
    pub fn cancel(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
    }

    pub fn shutdown(&self) {
        self.cancel();
        let _ = self.tx.send(Cmd::Quit);
    }
}

/// The engine behind a loaded model.
enum Engine {
    /// transcribe.cpp (Parakeet, Canary): GPU or CPU.
    Transcribe { model: Model, session: Session },
    /// Cactus Whistle: CPU only.
    Whistle(Whistle),
}

struct Loaded {
    kind: SttModel,
    engine: Engine,
    /// Whether this model actually runs on the GPU.
    gpu: bool,
    /// Whether the GPU was requested when loading (it may have fallen back to the CPU).
    requested_gpu: bool,
}

/// The custom words (comma separated in the settings) as a list.
fn custom_words(cfg: &Config) -> Vec<String> {
    cfg.custom_words.split(',').map(str::trim).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// The spoken language to pass to Whistle (`None` = let it detect the language).
fn whistle_language(cfg: &Config) -> Option<&str> {
    Some(cfg.stt_language.as_str()).filter(|l| *l != "auto")
}

fn transcribe_options(kind: SttModel, model: &Model, cfg: &Config) -> RunOptions {
    let mut options = RunOptions::default();
    if kind == SttModel::Canary180M {
        options.language = Some(cfg.stt_language.clone());
    }
    if model.supports(Feature::Vocabulary) {
        options.vocabulary = custom_words(cfg);
    }
    options
}

impl Loaded {
    /// Transcribe one piece of audio (no longer than the model's `max_chunk_secs`).
    fn run(&mut self, pcm: &[f32], cfg: &Config) -> Result<String, String> {
        match &mut self.engine {
            Engine::Transcribe { model, session } => session
                .run(pcm, &transcribe_options(self.kind, model, cfg))
                .map(|t| t.text)
                .map_err(|e| e.to_string()),
            Engine::Whistle(w) => {
                w.transcribe(pcm, whistle_language(cfg), &custom_words(cfg)).map_err(|e| format!("{e:#}"))
            }
        }
    }

    /// Where the model runs, e.g. "GPU: NVIDIA GeForce RTX 4090" or "CPU".
    fn device(&self) -> String {
        match &self.engine {
            Engine::Transcribe { model, .. } if self.gpu => match model.device() {
                Ok(d) => format!("GPU: {}", d.description),
                Err(_) => "CPU".to_owned(),
            },
            _ => "CPU".to_owned(),
        }
    }
}

/// Load a transcribe.cpp model (Parakeet, Canary) and warm it up.
fn load_transcribe(kind: SttModel, path: &std::path::Path, n_threads: i32, want_gpu: bool) -> Result<Loaded, String> {
    init_backends();
    let load = || -> Result<Loaded, transcribe_cpp::Error> {
        // `Auto` picks the best device (the GPU when present) and falls back to the CPU.
        let backend = if want_gpu { Backend::Auto } else { Backend::Cpu };
        let model = Model::load_with(path, &ModelOptions { backend, ..Default::default() })?;
        let mut session = model.session_with(&SessionOptions { n_threads, ..Default::default() })?;
        let on_gpu = model.device().is_ok_and(|d| d.device_type != transcribe_cpp::DeviceType::Cpu);
        // Warm up the GPU (shader setup) so the first real dictation is quick. On a GPU, pipelines are created per input size class the first time they are
        // used (seconds, once per machine; the driver caches them), so cover short, medium
        // and long dictations. Noise rather than silence, so the whole network actually runs.
        // On the CPU there is nothing to prepare, so the passes would only delay the load.
        if on_gpu && want_gpu {
            for secs in [1.5f32, 5.0, 11.0] {
                let n = (recorder::STT_RATE as f32 * secs) as usize;
                let noise: Vec<f32> = (0..n).map(|i| ((i * 7919 % 2000) as f32 - 1000.0) / 40_000.0).collect();
                let _ = session.run(&noise, &RunOptions::default());
            }
        }
        Ok(Loaded {
            kind,
            engine: Engine::Transcribe { model, session },
            gpu: on_gpu && want_gpu,
            requested_gpu: want_gpu,
        })
    };
    load().map_err(|e| {
        // The file itself is unreadable (damaged download): remove it so a retry fetches it again.
        if matches!(e, transcribe_cpp::Error::ModelLoad(_)) {
            hub::evict_if_damaged(path);
        }
        format!("Could not load the speech model: {e}")
    })
}

/// Load the transcribe.cpp backend modules (Vulkan GPU, CPU variants) that sit next to the exe.
/// Must happen once before the first model load. A missing or unusable GPU module is not an
/// error: the CPU modules are always there.
fn init_backends() {
    #[cfg(feature = "gpu")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if let Err(e) = transcribe_cpp::init_backends_default() {
                log::error!("could not load the speech backends: {e}");
            }
        });
    }
}

struct Worker {
    rx: Receiver<Cmd>,
    cfg: Arc<RwLock<Config>>,
    status: Arc<Mutex<SttStatus>>,
    download: Arc<Mutex<Progress>>,
    current: Arc<AtomicU64>,
    notify: SttNotify,
    loaded: Option<Loaded>,
    vad: Option<Vad>,
    /// Set after a GPU failure with this model: stay on the CPU until the next restart, retry or
    /// settings change (`Cmd::Unload`, or switching to another model).
    force_cpu: Option<SttModel>,
}

/// Set when the model refuses the small preview window, so it is not tried again.
static QUICK_REJECTED: AtomicBool = AtomicBool::new(false);

impl Worker {
    fn set_state(&self, state: SttState) {
        self.status.lock().state = state;
        (self.notify)(SttEvent::StateChanged);
    }

    fn run(mut self) {
        loop {
            let unload_after = self.cfg.read().stt_unload_minutes;
            // Whistle cannot be unloaded (the engine keeps it until exit), so it never times out.
            let unloadable = self.loaded.as_ref().is_some_and(|l| !matches!(l.engine, Engine::Whistle(_)));
            let cmd = if unloadable && unload_after > 0 {
                self.rx.recv_timeout(Duration::from_secs(unload_after as u64 * 60))
            } else {
                self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };
            let cmd = match cmd {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => {
                    self.unload();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if matches!(cmd, Cmd::Quit) {
                return;
            }
            // A panic (in the model code, say) must not leave the dictation waiting forever.
            let transcription = if let Cmd::Transcribe { id, .. } = &cmd { Some(*id) } else { None };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(cmd))).is_err() {
                log::error!("the speech recognition worker panicked");
                (self.loaded, self.vad) = (None, None);
                self.set_state(SttState::Error("Speech recognition failed unexpectedly - try Retry".into()));
                if let Some(id) = transcription {
                    (self.notify)(SttEvent::Finished(id, Outcome::Failed("internal error".into())));
                }
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Quit => {}
            Cmd::Unload => {
                // Sent on a retry and when the GPU setting changes: give the GPU another chance.
                self.force_cpu = None;
                self.unload();
            }
            Cmd::Preload => {
                if self.cfg.read().stt_enabled {
                    let _ = self.ensure_loaded();
                } else {
                    self.unload(); // also reports `Disabled`
                }
            }
            Cmd::Live(audio) => self.live(audio),
            Cmd::Transcribe { pcm, id, submit, reply } => self.handle_transcribe(pcm, id, submit, reply),
        }
    }

    fn unload(&mut self) {
        let (model, vad) = (self.loaded.take(), self.vad.take());
        if model.as_ref().is_some_and(|l| matches!(l.engine, Engine::Whistle(_))) {
            log::info!("speech model released (the Whistle engine keeps it in memory until exit)");
        } else if model.is_some() || vad.is_some() {
            log::info!("speech model unloaded");
        }
        let enabled = self.cfg.read().stt_enabled;
        self.set_state(if enabled { SttState::Idle } else { SttState::Disabled });
    }

    fn ensure_loaded(&mut self) -> Result<(), String> {
        let (kind, n_threads, want_gpu) = {
            let c = self.cfg.read();
            (c.stt_model, c.thread_count() as i32, c.use_gpu)
        };
        if self.force_cpu.is_some_and(|k| k != kind) {
            self.force_cpu = None; // the model was changed in the settings
        }
        let want_gpu = want_gpu && self.force_cpu != Some(kind);
        if self.loaded.as_ref().map(|l| (l.kind, l.requested_gpu)) == Some((kind, want_gpu)) {
            if self.status.lock().state != SttState::Ready {
                self.set_state(SttState::Ready);
            }
            return Ok(());
        }
        self.loaded = None;
        let path = match self.fetch(kind.repo(), kind.file()) {
            Ok(p) => p,
            Err(e) => return Err(self.fail(format!("Speech model download failed: {e:#}"))),
        };
        self.set_state(SttState::Loading);
        let t = Instant::now();
        let loaded = if kind == SttModel::Whistle {
            Whistle::load(&path)
                .map(|w| Loaded { kind, engine: Engine::Whistle(w), gpu: false, requested_gpu: want_gpu })
                .map_err(|e| format!("Could not load the speech model: {e:#}"))
        } else {
            load_transcribe(kind, &path, n_threads, want_gpu)
        };
        match loaded {
            Ok(l) => {
                let device = l.device();
                log::info!("speech model {} loaded on {device} in {:.2}s", kind.file(), t.elapsed().as_secs_f32());
                {
                    let mut st = self.status.lock();
                    st.device = device;
                    st.runs_since_load = 0;
                }
                self.loaded = Some(l);
                if self.cfg.read().use_vad {
                    if let Err(e) = self.ensure_vad() {
                        log::warn!("{e}");
                    }
                }
                self.set_state(SttState::Ready);
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    fn fail(&mut self, msg: String) -> String {
        log::error!("{msg}");
        *self.download.lock() = Progress::default();
        self.set_state(SttState::Error(msg.clone()));
        msg
    }

    /// Cached path of a model file, downloading it (with progress) if needed.
    fn fetch(&mut self, repo: &str, file: &str) -> anyhow::Result<PathBuf> {
        if let Some(p) = hub::find(repo, file) {
            return Ok(p);
        }
        self.set_state(SttState::Downloading);
        hub::ensure(repo, file, &self.download)
    }

    fn ensure_vad(&mut self) -> Result<(), String> {
        if self.vad.is_some() {
            return Ok(());
        }
        // The VAD may be fetched in the middle of a load or transcription: show `Downloading` only
        // while it lasts.
        let prev = self.status.lock().state.clone();
        let path = self.fetch(VAD_REPO, VAD_FILE);
        if self.status.lock().state != prev {
            self.set_state(prev);
        }
        let path = path.map_err(|e| {
            *self.download.lock() = Progress::default();
            format!("VAD download failed: {e:#}")
        })?;
        self.vad = Some(Vad::load(&path).map_err(|e| format!("VAD load failed: {e:#}"))?);
        Ok(())
    }

    /// Report a live transcript while the recording runs; returns when it stops.
    fn live(&mut self, audio: LiveAudio) {
        let cfg = self.cfg.read().clone();
        if !cfg.live_preview || self.ensure_loaded().is_err() {
            return;
        }
        let notify = self.notify.clone();
        let mut shown = String::new();
        let mut show = |text: &str| {
            let text = text.trim();
            if text != shown {
                shown = text.to_owned();
                notify(SttEvent::Partial(shown.clone()));
            }
        };
        let l = self.loaded.as_mut().expect("loaded");
        let kind = l.kind;
        match &mut l.engine {
            Engine::Whistle(w) => live_whistle(*w, &cfg, &audio, &mut show),
            Engine::Transcribe { model, session } => {
                let options = transcribe_options(kind, model, &cfg);
                if model.capabilities().supports_streaming {
                    // Parakeet's default window waits for 1 s of audio and 1 s of lookahead, so the text
                    // trails the speaker by about two seconds. The preview takes the smallest steps the
                    // model offers (80 ms frames; its menu has 1, 2, 7 and 13 for the chunk): it is only
                    // a preview, the final text comes from the whole recording.
                    let quick = StreamOptions {
                        family: (kind == SttModel::ParakeetUnified && !QUICK_REJECTED.load(Ordering::Relaxed)).then(|| {
                            StreamExtension::ParakeetBuffered(ParakeetBufferedStreamOptions {
                                left_ms: None,
                                chunk_ms: Some(160),
                                right_ms: Some(160),
                            })
                        }),
                        ..StreamOptions::default()
                    };
                    let mut stream = match session.stream(&options, &quick) {
                        Ok(s) => s,
                        Err(e) => {
                            // Not this recording's preview, but the next ones use the default window.
                            QUICK_REJECTED.store(true, Ordering::Relaxed);
                            return log::warn!("live transcript unavailable ({e}); using the default window from now on");
                        }
                    };
                    while let Some(pcm) = next_live(&audio) {
                        if let Err(e) = stream.feed(&pcm) {
                            return log::warn!("live transcript stopped: {e}");
                        }
                        show(&stream.text().display());
                    }
                    // Dropping the stream abandons it: the final text comes from the whole recording.
                } else {
                    // No streaming support: transcribe the latest audio again every quarter second or
                    // so (a slower pass simply delays the next one).
                    let window = kind.max_chunk_secs() * recorder::STT_RATE as usize;
                    let (mut pcm, mut last) = (Vec::new(), Instant::now());
                    while let Some(piece) = next_live(&audio) {
                        pcm.extend(piece);
                        if last.elapsed() < Duration::from_millis(250) {
                            continue;
                        }
                        last = Instant::now();
                        match session.run(&pcm[pcm.len().saturating_sub(window)..], &options) {
                            Ok(t) => show(&t.text),
                            Err(e) => return log::warn!("live transcript stopped: {e}"),
                        }
                    }
                }
            }
        }
    }

    fn handle_transcribe(&mut self, pcm: Vec<f32>, id: u64, submit: bool, reply: Option<Sender<Result<String, String>>>) {
        let mut outcome = self.transcribe(pcm, id);
        if let Some(r) = reply {
            // Command-line use: hand the text back instead of inserting it.
            let _ = r.send(match outcome {
                Outcome::Inserted(t) => Ok(t),
                Outcome::NoSpeech => Ok(String::new()),
                Outcome::Failed(e) => Err(e),
                Outcome::Cancelled => Err("cancelled".into()),
            });
            return;
        }
        if let Outcome::Inserted(text) = &outcome {
            let cfg = self.cfg.read().clone();
            let current = self.current.clone();
            let cancelled = move || current.load(Ordering::SeqCst) != id;
            if self.current.load(Ordering::SeqCst) != id {
                outcome = Outcome::Cancelled;
            } else if let Err(e) = deliver(text, &cfg, submit, &cancelled) {
                outcome = Outcome::Failed(format!("Could not insert the text: {e}"));
            }
        }
        (self.notify)(SttEvent::Finished(id, outcome));
    }

    fn transcribe(&mut self, mut pcm: Vec<f32>, id: u64) -> Outcome {
        let cfg = self.cfg.read().clone();
        if let Err(e) = self.ensure_loaded() {
            return Outcome::Failed(e);
        }
        if self.current.load(Ordering::SeqCst) != id {
            return Outcome::Cancelled;
        }
        let t0 = Instant::now();
        let audio_secs = pcm.len() as f32 / recorder::STT_RATE as f32;

        if cfg.use_vad {
            match self.ensure_vad().and_then(|()| {
                self.vad.as_mut().unwrap().trim(&pcm).map_err(|e| format!("VAD failed: {e:#}"))
            }) {
                Ok(trimmed) => pcm = trimmed,
                Err(e) => log::warn!("{e}; transcribing without silence removal"),
            }
        }
        if pcm.len() < recorder::STT_RATE as usize / 5 {
            log::info!("no speech detected ({audio_secs:.1}s recorded)");
            return Outcome::NoSpeech;
        }

        // Models have a maximum audio length per run: transcribe long recordings in pieces.
        let chunk = self.loaded.as_ref().expect("loaded").kind.max_chunk_secs() * recorder::STT_RATE as usize;
        let mut pieces: Vec<String> = Vec::new();
        for piece in split_at_quiet(&pcm, chunk) {
            if self.current.load(Ordering::SeqCst) != id {
                return Outcome::Cancelled;
            }
            let (first, on_gpu) = {
                let l = self.loaded.as_mut().expect("loaded");
                (l.run(piece, &cfg), l.gpu)
            };
            let result = match first {
                Err(e) if on_gpu => {
                    // The GPU failed (driver reset, out of memory, ...): carry on with the CPU.
                    log::warn!("GPU transcription failed ({e}); switching to the CPU");
                    self.force_cpu = self.loaded.as_ref().map(|l| l.kind);
                    self.loaded = None;
                    if let Err(e) = self.ensure_loaded() {
                        return Outcome::Failed(e);
                    }
                    self.loaded.as_mut().expect("loaded").run(piece, &cfg)
                }
                other => other,
            };
            match result {
                Ok(t) if !t.trim().is_empty() => pieces.push(t.trim().to_owned()),
                Ok(_) => {}
                Err(e) => return Outcome::Failed(format!("Transcription failed: {e}")),
            }
        }
        let text = pieces.join(" ");
        let millis = t0.elapsed().as_millis() as u32;
        // Not the text itself: the log is kept even when the history is off.
        log::info!("transcribed {audio_secs:.1}s of audio in {millis} ms ({} characters)", text.chars().count());
        {
            let mut st = self.status.lock();
            st.last = Some(SttMetrics { audio_secs, millis });
            st.runs_since_load += 1;
        }
        (self.notify)(SttEvent::StateChanged);

        if text.is_empty() {
            return Outcome::NoSpeech;
        }
        if self.current.load(Ordering::SeqCst) != id {
            return Outcome::Cancelled;
        }
        Outcome::Inserted(text)
    }
}

/// Live transcript with Whistle: its engine buffers the audio and re-transcribes all of it at every
/// step, so a pass gets slower as the stream grows (and a stream holds at most 30 s). Start a new
/// stream at a pause once it is a few seconds long, and keep what was said so far.
fn live_whistle(w: Whistle, cfg: &Config, audio: &LiveAudio, show: &mut impl FnMut(&str)) {
    // A short step keeps the text close behind the speaker; a pass that takes longer than a step just
    // makes `next_live` hand over everything that arrived meanwhile.
    const STEP: usize = recorder::STT_RATE as usize / 8;
    /// Restart at the next pause after this many seconds, or regardless after `RESTART_MAX_SECS`.
    const RESTART_SECS: f32 = 8.0;
    const RESTART_MAX_SECS: f32 = 14.0;
    /// RMS level of the last quarter second below which the speaker is taken to be pausing.
    const QUIET_RMS: f32 = 0.01;
    let (language, words) = (whistle_language(cfg), custom_words(cfg));
    let (mut earlier, mut stream, mut buffered) = (String::new(), w.stream(language, &words), Vec::new());
    let join = |earlier: &str, now: &str| [earlier, now].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
    while let Some(piece) = next_live(audio) {
        buffered.extend(piece);
        if buffered.len() < STEP {
            continue;
        }
        let fed = std::mem::take(&mut buffered);
        match stream.feed(&fed) {
            Ok(text) => show(&join(&earlier, &text)),
            Err(e) => return log::warn!("live transcript stopped: {e:#}"),
        }
        let tail = &fed[fed.len().saturating_sub(STEP / 2)..];
        let pausing = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt() < QUIET_RMS;
        if stream.seconds() > RESTART_MAX_SECS || (stream.seconds() > RESTART_SECS && pausing) {
            let done = std::mem::replace(&mut stream, w.stream(language, &words));
            match done.finish() {
                Ok(text) => earlier = join(&earlier, &text),
                Err(e) => return log::warn!("live transcript stopped: {e:#}"),
            }
        }
    }
}

/// The live audio that has arrived since the last call (waiting for some if there is none yet).
/// `None` once the recording has stopped: a preview pass over the last bit of audio would only
/// delay the real transcription, which takes the whole recording anyway. This also skips the
/// preview entirely when the model finished loading only after the recording was over.
fn next_live(audio: &LiveAudio) -> Option<Vec<f32>> {
    let mut pcm = audio.recv().ok()?;
    loop {
        match audio.try_recv() {
            Ok(piece) => pcm.extend(piece), // catch up after a slow pass
            Err(mpsc::TryRecvError::Empty) => return Some(pcm),
            Err(mpsc::TryRecvError::Disconnected) => return None,
        }
    }
}

/// Cut `pcm` into pieces of at most `max` samples, at the quietest moment of the last 3 s of each
/// piece so that a cut rarely lands inside a word. No piece after the first is shorter than
/// `MIN_PIECE` (a sliver of audio transcribes badly), as long as `max` is a few seconds.
fn split_at_quiet(pcm: &[f32], max: usize) -> Vec<&[f32]> {
    const WINDOW: usize = recorder::STT_RATE as usize / 50; // 20 ms
    const SEARCH: usize = 3 * recorder::STT_RATE as usize;
    const MIN_PIECE: usize = recorder::STT_RATE as usize / 2;
    let mut pieces = Vec::new();
    let mut rest = pcm;
    while rest.len() > max {
        // Cut early enough to leave at least `MIN_PIECE` for what follows.
        let end = max.min(rest.len().saturating_sub(MIN_PIECE)).max(WINDOW);
        let energy = |at: usize| rest[at..at + WINDOW].iter().map(|s| s * s).sum::<f32>();
        let quietest = (end.saturating_sub(SEARCH)..end - WINDOW).step_by(WINDOW).min_by(|&a, &b| energy(a).total_cmp(&energy(b)));
        let (piece, tail) = rest.split_at(quietest.map_or(end, |at| at + WINDOW / 2));
        pieces.push(piece);
        rest = tail;
    }
    pieces.push(rest);
    pieces
}

/// Insert a finished transcript and record it in the history. Runs on the worker side of the
/// app (never on the UI thread) because inserting waits for key releases and clipboard timing.
pub fn deliver(text: &str, cfg: &Config, submit: bool, cancelled: &(dyn Fn() -> bool + Sync)) -> Result<(), String> {
    if cancelled() {
        return Err("cancelled".into());
    }
    history::add(text, cfg.history_limit);
    // Test hook: run the whole pipeline but do not send keystrokes to the focused application.
    if cfg!(feature = "test-hooks") && std::env::var_os("SONORANT_TEST_NO_PASTE").is_some() {
        return Ok(());
    }
    platform::insert(
        text,
        &InsertOptions {
            method: cfg.paste_method,
            keep_clipboard: cfg.paste_keep_clipboard,
            auto_submit: cfg.auto_submit || submit,
            trailing_space: cfg.trailing_space && !submit,
            cancelled,
        },
    )
    .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_audio_is_cut_in_the_pause() {
        let rate = recorder::STT_RATE as usize;
        // 70 s of "speech" with a pause 27-27.5 s into it.
        let pcm: Vec<f32> = (0..70 * rate)
            .map(|i| if (27 * rate..27 * rate + rate / 2).contains(&i) { 0.0 } else { ((i % 50) as f32 - 25.0) / 100.0 })
            .collect();
        let pieces = split_at_quiet(&pcm, 29 * rate);
        assert!(pieces.iter().all(|p| p.len() <= 29 * rate));
        assert_eq!(pieces.iter().map(|p| p.len()).sum::<usize>(), pcm.len());
        let first_end = pieces[0].len();
        assert!((27 * rate..27 * rate + rate / 2).contains(&first_end), "cut at {first_end}");
        assert_eq!(split_at_quiet(&pcm[..rate], 29 * rate).len(), 1);
    }

    #[test]
    fn no_sliver_at_the_end() {
        let rate = recorder::STT_RATE as usize;
        let max = 29 * rate;
        for extra in [1, rate / 100, rate / 4, rate / 2, rate] {
            // Steady "speech", and the same with silence right at the end of the first piece.
            let speech: Vec<f32> = (0..max + extra).map(|i| ((i % 50) as f32 - 25.0) / 100.0).collect();
            let mut pause = speech.clone();
            pause[max - rate / 10..max].fill(0.0);
            for pcm in [&speech, &pause] {
                let pieces = split_at_quiet(pcm, max);
                assert_eq!(pieces.len(), 2);
                assert_eq!(pieces.iter().map(|p| p.len()).sum::<usize>(), pcm.len());
                assert!(pieces.iter().all(|p| (rate / 2..=max).contains(&p.len())), "{extra}: {:?}", pieces.iter().map(|p| p.len()).collect::<Vec<_>>());
            }
        }
    }
}
