//! The synthesis worker: owns the Kokoro engine and runs the streaming
//! pipeline (chunk -> phonemes -> inference -> trim -> audio queue).
//!
//! Playback of chunk N overlaps with synthesis of chunk N+1, so the time to
//! first audio is the cost of synthesizing only the (short) first chunk.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

pub mod engine;
pub mod text;
pub mod vocab;

use crate::audio::AudioHandle;
use crate::config::Config;
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

enum Cmd {
    Speak { text: String, t0: Instant, epoch: u64 },
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
        let worker = Worker {
            rx,
            cfg,
            audio: audio.clone(),
            status: status.clone(),
            download: download.clone(),
            tap: tap.clone(),
            notify,
            engine: None,
        };
        std::thread::Builder::new()
            .name("tts".into())
            .spawn(move || worker.run())
            .expect("spawn tts thread");
        Self { tx, audio, status, download, tap }
    }

    /// Speak `text`, interrupting anything currently playing. `t0` is when the
    /// user asked for it (hotkey press), used for latency measurement.
    pub fn speak(&self, text: String, t0: Instant) {
        let epoch = self.audio.begin();
        let _ = self.tx.send(Cmd::Speak { text, t0, epoch });
    }

    pub fn stop(&self) {
        self.audio.begin();
    }

    /// Reload the model (after a model / thread-count change).
    pub fn reload(&self) {
        self.stop();
        let _ = self.tx.send(Cmd::Reload);
    }

    pub fn is_busy(&self) -> bool {
        self.status.lock().synthesizing || self.audio.is_playing()
    }

    pub fn shutdown(&self) {
        self.audio.begin();
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
    engine: Option<Engine>,
}

impl Worker {
    fn set_state(&self, state: EngineState) {
        self.status.lock().state = state;
        (self.notify)();
    }

    fn run(mut self) {
        self.load();
        while let Ok(cmd) = self.rx.recv() {
            match cmd {
                Cmd::Quit => return,
                Cmd::Reload => self.load(),
                Cmd::Speak { text, t0, epoch } => {
                    // Skip to the newest request if several piled up while we were busy.
                    let (mut text, mut t0, mut epoch) = (text, t0, epoch);
                    let mut reload = false;
                    while let Ok(next) = self.rx.try_recv() {
                        match next {
                            Cmd::Speak { text: t, t0: s, epoch: e } => (text, t0, epoch) = (t, s, e),
                            Cmd::Reload => reload = true,
                            Cmd::Quit => return,
                        }
                    }
                    if reload {
                        self.load();
                    }
                    self.speak(&text, t0, epoch);
                }
            }
        }
    }

    fn load(&mut self) {
        self.engine = None;
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
                self.set_state(EngineState::Ready);
            }
            Err(e) => {
                log::error!("model load failed: {e:#}");
                self.set_state(EngineState::Error(format!("{e:#}")));
            }
        }
    }

    fn speak(&mut self, input: &str, t0: Instant, epoch: u64) {
        let Some(engine) = self.engine.as_mut() else {
            log::warn!("speak requested but the model is not ready");
            return;
        };
        let cfg = self.cfg.read().clone();
        let voice = if engine.voices.contains(&cfg.voice) { cfg.voice.as_str() } else { "af_heart" };

        self.status.lock().synthesizing = true;
        (self.notify)();

        let chunks = text::plan_chunks(input, cfg.streaming);
        let mut metrics = Metrics::default();
        let mut first = true;
        for chunk in &chunks {
            if self.audio.epoch() != epoch {
                break;
            }
            // Don't run far ahead of playback: wastes CPU if the user interrupts.
            while self.audio.buffered_secs() > 6.0 && self.audio.epoch() == epoch {
                std::thread::sleep(Duration::from_millis(25));
            }
            let phonemes = match text::phonemize(&chunk.text, text::LANG) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("phonemizer failed for {:?}: {e:#}", chunk.text);
                    continue;
                }
            };
            let tokens = text::tokenize(&phonemes);
            if tokens.is_empty() {
                continue;
            }
            let t_synth = Instant::now();
            let samples = match engine.synth(&tokens, voice, cfg.speed) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("inference failed: {e:#}");
                    continue;
                }
            };
            let synth_time = t_synth.elapsed();
            let out = finish_chunk(&samples, chunk.pause_ms);
            metrics.synth_secs += synth_time.as_secs_f32();
            metrics.audio_secs += out.len() as f32 / SAMPLE_RATE as f32;
            if let Some(tap) = self.tap.lock().as_mut() {
                tap.extend_from_slice(&out);
            }
            if !self.audio.push(epoch, &out) {
                break;
            }
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
    // Give the worker a moment to flag `synthesizing` after a speak() call.
    std::thread::sleep(Duration::from_millis(50));
    while tts.is_busy() && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(20));
    }
}
