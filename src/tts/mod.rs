//! The synthesis worker: owns the Kokoro engine and runs the streaming
//! pipeline (chunk -> phonemes -> inference -> trim -> audio queue).
//!
//! Playback of chunk N overlaps with synthesis of chunk N+1, so the time to
//! first audio is the cost of synthesizing only the (short) first chunk.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ort::session::RunOptions;
use parking_lot::{Mutex, RwLock};

pub mod engine;
pub mod text;
pub mod vocab;

use crate::audio::AudioHandle;
use crate::config::{Config, ModelVariant};
use self::engine::{Engine, SAMPLE_RATE};
use crate::models::{self, Progress};

#[derive(Clone, Debug, PartialEq)]
pub enum EngineState {
    Starting,
    Downloading,
    Loading,
    Ready,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    /// Hotkey press (or request) to first audio queued.
    pub first_audio_ms: u32,
    /// Duration of the first synthesized chunk's inference alone.
    pub first_synth_ms: u32,
    pub audio_secs: f32,
    pub synth_secs: f32,
}

impl Metrics {
    /// Real-time factor: synthesis time / audio duration (lower is faster).
    pub fn rtf(&self) -> f32 {
        if self.audio_secs > 0.0 {
            self.synth_secs / self.audio_secs
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug)]
pub struct Status {
    pub state: EngineState,
    pub voices: Vec<String>,
    pub last: Option<Metrics>,
    /// Set while a synthesis job is running.
    pub synthesizing: bool,
}

/// Per-request settings that take precedence over the saved config (e.g. the settings
/// dialog's preview of unsaved changes).
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub voice: Option<String>,
    pub speed: Option<f32>,
}

struct Request {
    text: String,
    t0: Instant,
    epoch: u64,
    overrides: Overrides,
}

enum Cmd {
    Speak(Request),
    Reload,
    Quit,
}

pub type Notify = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct Tts {
    tx: Sender<Cmd>,
    audio: AudioHandle,
    pub status: Arc<Mutex<Status>>,
    pub download: Arc<Mutex<Progress>>,
    /// When set, every chunk sent to the speakers is also appended here (used for `--wav`).
    pub tap: Arc<Mutex<Option<Vec<f32>>>>,
    shared: Arc<Shared>,
}

/// State shared between `Tts` handles and the worker thread.
#[derive(Default)]
struct Shared {
    /// Audio epoch of the newest speak request the worker has not finished yet (0: none).
    /// Set before the request is queued, so `is_busy` never misses it.
    pending: AtomicU64,
    /// Run options of the utterance being synthesized; terminating them aborts its inference.
    run: Mutex<Option<Arc<RunOptions>>>,
}

impl Shared {
    /// The worker is done with the request for `epoch` (spoken or dropped).
    fn finish(&self, epoch: u64) {
        let _ = self.pending.compare_exchange(epoch, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Abort the inference in progress (if any).
    fn terminate(&self) {
        if let Some(run) = self.run.lock().as_ref() {
            let _ = run.terminate();
        }
    }
}

impl Tts {
    pub fn spawn(cfg: Arc<RwLock<Config>>, audio: AudioHandle, notify: Notify) -> Self {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status {
            state: EngineState::Starting,
            voices: Vec::new(),
            last: None,
            synthesizing: false,
        }));
        let download = Arc::new(Mutex::new(Progress::default()));
        let tap = Arc::new(Mutex::new(None));
        let shared = Arc::new(Shared::default());
        let worker = Worker {
            rx,
            cfg,
            audio: audio.clone(),
            status: status.clone(),
            download: download.clone(),
            tap: tap.clone(),
            notify,
            shared: shared.clone(),
            engine: None,
            loaded: None,
            deferred: None,
        };
        std::thread::Builder::new()
            .name("tts".into())
            .spawn(move || worker.run())
            .expect("spawn tts thread");
        Self { tx, audio, status, download, tap, shared }
    }

    /// Speak `text`, interrupting anything currently playing. `t0` is when the
    /// user asked for it (hotkey press), used for latency measurement.
    pub fn speak(&self, text: String, t0: Instant) {
        self.speak_with(text, t0, Overrides::default());
    }

    /// Like `speak`, with `overrides` used instead of the config for this request only.
    pub fn speak_with(&self, text: String, t0: Instant, overrides: Overrides) {
        let epoch = self.audio.begin();
        self.shared.pending.store(epoch, Ordering::SeqCst);
        self.shared.terminate();
        let _ = self.tx.send(Cmd::Speak(Request { text, t0, epoch, overrides }));
    }

    pub fn stop(&self) {
        self.audio.begin();
        self.shared.terminate();
    }

    /// Reload the model (after a model / thread-count change).
    pub fn reload(&self) {
        self.stop();
        let _ = self.tx.send(Cmd::Reload);
    }

    pub fn is_busy(&self) -> bool {
        // A queued request counts until the worker is done with it, unless it was stopped or replaced.
        let pending = self.shared.pending.load(Ordering::SeqCst);
        (pending != 0 && pending == self.audio.epoch()) || self.status.lock().synthesizing || self.audio.is_playing()
    }

    pub fn shutdown(&self) {
        self.stop();
        let _ = self.tx.send(Cmd::Quit);
    }
}

struct Worker {
    rx: Receiver<Cmd>,
    cfg: Arc<RwLock<Config>>,
    audio: AudioHandle,
    status: Arc<Mutex<Status>>,
    download: Arc<Mutex<Progress>>,
    tap: Arc<Mutex<Option<Vec<f32>>>>,
    notify: Notify,
    shared: Arc<Shared>,
    engine: Option<Engine>,
    /// Model variant and thread count of `engine`.
    loaded: Option<(ModelVariant, usize)>,
    /// Newest speak request that arrived while no model was loaded; spoken once one is.
    deferred: Option<Request>,
}

impl Worker {
    fn set_state(&self, state: EngineState) {
        self.status.lock().state = state;
        (self.notify)();
    }

    fn run(mut self) {
        self.load_guarded();
        while let Ok(cmd) = self.rx.recv() {
            // Coalesce everything queued meanwhile: the newest speak request wins and
            // reloads (e.g. several requested during a download) collapse into one.
            let (mut speak, mut reload) = (None, false);
            let mut next = Some(cmd);
            while let Some(cmd) = next {
                match cmd {
                    Cmd::Quit => return,
                    Cmd::Reload => reload = true,
                    Cmd::Speak(req) => {
                        if let Some(old) = speak.replace(req) {
                            self.shared.finish(old.epoch);
                        }
                    }
                }
                next = self.rx.try_recv().ok();
            }
            if reload {
                let wanted = {
                    let c = self.cfg.read();
                    (c.model, c.thread_count())
                };
                if self.engine.is_some() && self.loaded == Some(wanted) {
                    log::info!("model already loaded, skipping reload");
                } else {
                    self.load_guarded();
                }
            }
            if let Some(req) = speak {
                let epoch = req.epoch;
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.speak(req))).is_err() {
                    log::error!("the speech synthesis worker panicked");
                    self.recover(epoch);
                }
            }
        }
    }

    /// After a panic: no utterance is in progress any more, and nothing may wait for this one.
    fn recover(&mut self, epoch: u64) {
        *self.shared.run.lock() = None;
        self.shared.finish(epoch);
        self.status.lock().synthesizing = false;
        (self.notify)();
    }

    /// `load`, but a panic ends in the error state instead of killing the worker silently.
    fn load_guarded(&mut self) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.load())).is_err() {
            log::error!("the speech model loader panicked");
            (self.engine, self.loaded) = (None, None);
            self.recover(0);
            self.set_state(EngineState::Error("The voice model failed unexpectedly - try Retry".into()));
        }
    }

    fn load(&mut self) {
        self.engine = None;
        self.loaded = None;
        let (variant, threads) = {
            let c = self.cfg.read();
            (c.model, c.thread_count())
        };
        if !models::kokoro_ready(variant) {
            self.set_state(EngineState::Downloading);
        }
        let files = match models::ensure_kokoro(variant, &self.download) {
            Ok(f) => f,
            Err(e) => {
                log::error!("model download failed: {e:#}");
                *self.download.lock() = Progress::default();
                self.set_state(EngineState::Error(format!("Model download failed: {e:#}")));
                return;
            }
        };
        self.set_state(EngineState::Loading);
        let t = Instant::now();
        match Engine::load(&files, threads) {
            Ok(engine) => {
                log::info!("model loaded in {:.2}s", t.elapsed().as_secs_f32());
                models::remove_legacy_models();
                let voices: Vec<String> =
                    engine.voices.names().into_iter().filter(|v| text::is_supported_voice(v)).collect();
                self.status.lock().voices = voices;
                self.engine = Some(engine);
                self.loaded = Some((variant, threads));
                self.set_state(EngineState::Ready);
                // Speak what was asked for while the model was unavailable, unless it was
                // stopped or replaced since.
                if let Some(req) = self.deferred.take() {
                    if self.audio.epoch() == req.epoch {
                        self.speak(req);
                    }
                }
            }
            Err(e) => {
                log::error!("model load failed: {e:#}");
                self.set_state(EngineState::Error(format!("{e:#}")));
            }
        }
    }

    fn speak(&mut self, req: Request) {
        let Some(engine) = self.engine.as_mut() else {
            log::warn!("speak requested but the model is not ready; it will be spoken once it is");
            // Not busy meanwhile: the model may stay unavailable (e.g. after a failed download).
            self.shared.finish(req.epoch);
            self.deferred = Some(req);
            return;
        };
        let Request { text: input, t0, epoch, overrides } = req;
        let mut cfg = self.cfg.read().clone();
        if let Some(voice) = overrides.voice {
            cfg.voice = voice;
        }
        if let Some(speed) = overrides.speed {
            cfg.speed = speed;
        }
        let voice = if engine.voices.contains(&cfg.voice) {
            cfg.voice.clone()
        } else {
            let names = engine.voices.names();
            let fallback = names.iter().find(|v| text::is_supported_voice(v)).or(names.first());
            fallback.cloned().unwrap_or_default()
        };
        let run = match RunOptions::new() {
            Ok(r) => Arc::new(r),
            Err(e) => {
                log::error!("create run options: {e}");
                self.shared.finish(epoch);
                return;
            }
        };
        // Publish before the epoch check below, so a stop() racing with it either changes the
        // epoch first (caught by the check) or terminates this run.
        *self.shared.run.lock() = Some(run.clone());

        self.status.lock().synthesizing = true;
        self.shared.finish(epoch);
        (self.notify)();

        let chunks = text::plan_chunks(&input, cfg.streaming);
        let mut metrics = Metrics::default();
        let mut first = true;
        'chunks: for chunk in &chunks {
            if self.audio.epoch() != epoch {
                break;
            }
            let mut pieces = Vec::new();
            if let Err(e) = token_pieces(&chunk.text, chunk.pause_ms, &mut pieces) {
                // Not the text itself: the log is kept, and the text is whatever the user selected.
                log::warn!("phonemizer failed for a chunk of {} characters: {e:#}", chunk.text.chars().count());
                continue;
            }
            for (tokens, pause_ms) in pieces {
                // Don't run far ahead of playback: wastes CPU if the user interrupts.
                while self.audio.buffered_secs() > 6.0 && self.audio.epoch() == epoch {
                    std::thread::sleep(Duration::from_millis(25));
                }
                if self.audio.epoch() != epoch {
                    break 'chunks;
                }
                let t_synth = Instant::now();
                let samples = match engine.synth(&tokens, &voice, cfg.speed, &run) {
                    Ok(s) => s,
                    Err(_) if self.audio.epoch() != epoch => break 'chunks, // stopped mid-inference
                    Err(e) => {
                        log::error!("inference failed: {e:#}");
                        continue;
                    }
                };
                let synth_time = t_synth.elapsed();
                let out = finish_chunk(&samples, pause_ms);
                if !self.audio.push(epoch, &out) {
                    break 'chunks;
                }
                if let Some(tap) = self.tap.lock().as_mut() {
                    tap.extend_from_slice(&out);
                }
                metrics.synth_secs += synth_time.as_secs_f32();
                metrics.audio_secs += out.len() as f32 / SAMPLE_RATE as f32;
                if first {
                    first = false;
                    metrics.first_audio_ms = t0.elapsed().as_millis() as u32;
                    metrics.first_synth_ms = synth_time.as_millis() as u32;
                    log::info!(
                        "first audio after {} ms (inference {} ms, {} tokens)",
                        metrics.first_audio_ms,
                        metrics.first_synth_ms,
                        tokens.len()
                    );
                }
            }
        }
        *self.shared.run.lock() = None;
        log::info!(
            "utterance done: {:.1}s audio, synth {:.2}s, RTF {:.2}",
            metrics.audio_secs,
            metrics.synth_secs,
            metrics.rtf()
        );
        {
            let mut st = self.status.lock();
            st.synthesizing = false;
            if metrics.audio_secs > 0.0 {
                st.last = Some(metrics);
            }
        }
        (self.notify)();
    }
}

/// Phonemize `text` into token sequences that each fit the model, splitting the text in two
/// (at clause punctuation or a space) while its phonemes are too long: chunks are sized in
/// characters, and some text (numbers, symbols) expands to many phonemes. Each piece comes with
/// the pause to append after it; the last one gets `pause_ms`.
fn token_pieces(text: &str, pause_ms: u32, out: &mut Vec<(Vec<i64>, u32)>) -> anyhow::Result<()> {
    let mut tokens = text::tokenize(&text::phonemize(text, text::LANG)?);
    if tokens.len() > text::MAX_TOKENS {
        let (head, rest) = text::split_at_limit(text, text.chars().count() / 2);
        if !head.is_empty() && !rest.is_empty() {
            token_pieces(&head, text::pause_after(&head), out)?;
            return token_pieces(&rest, pause_ms, out);
        }
        log::warn!("truncating {} phoneme tokens to {}", tokens.len(), text::MAX_TOKENS);
        tokens.truncate(text::MAX_TOKENS);
    }
    if !tokens.is_empty() {
        out.push((tokens, pause_ms));
    }
    Ok(())
}

/// Trim the model's leading/trailing silence, add short fades to avoid clicks
/// and append a punctuation-dependent pause.
fn finish_chunk(samples: &[f32], pause_ms: u32) -> Vec<f32> {
    const THRESHOLD: f32 = 0.003;
    let rate = SAMPLE_RATE as usize;
    let first = samples.iter().position(|s| s.abs() > THRESHOLD).unwrap_or(0);
    let last = samples.iter().rposition(|s| s.abs() > THRESHOLD).unwrap_or(samples.len().saturating_sub(1));
    let start = first.saturating_sub(rate / 200); // keep 5 ms
    let end = (last + rate / 50).min(samples.len()); // keep 20 ms
    let mut out = samples.get(start..end).unwrap_or_default().to_vec();

    let fade = (rate / 500).min(out.len() / 2); // 2 ms
    let n = out.len();
    for i in 0..fade {
        let g = i as f32 / fade as f32;
        out[i] *= g;
        out[n - 1 - i] *= g;
    }
    out.resize(n + rate * pause_ms as usize / 1000, 0.0);
    out
}

pub fn wait_until_idle(tts: &Tts, timeout: Duration) {
    let start = Instant::now();
    // No startup grace period needed: `speak` marks the request pending before queuing it.
    while tts.is_busy() && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(20));
    }
}
