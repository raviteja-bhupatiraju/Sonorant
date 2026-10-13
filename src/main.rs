#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod config;
mod hotkey;
mod history;
mod models;
mod platform;
mod selftest;
mod stt;
mod tts;
mod ui;

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use config::Config;

struct Logger {
    file: Mutex<Option<std::fs::File>>,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || m.target().starts_with("sonorant")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("[{:>5}] {}\n", r.level(), r.args());
        eprint!("{line}");
        if let Some(f) = self.file.lock().as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {}
}

fn init_logging() {
    let dir = config::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("sonorant.log");
    // Keep the log from growing forever.
    if std::fs::metadata(&log_path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&log_path);
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(log_path).ok();
    let logger: &'static Logger = Box::leak(Box::new(Logger { file: Mutex::new(file) }));
    let _ = log::set_logger(logger).map(|()| log::set_max_level(log::LevelFilter::Info));
}

/// `sonorant --say "text" [--voice ID] [--speed X] [--wav FILE] [--no-stream] [--mute]`
/// Speaks without the tray UI and prints latency numbers; used for testing and benchmarking.
fn run_cli(args: &[String]) -> anyhow::Result<()> {
    let mut cfg = Config::load();
    let mut text = String::new();
    let mut wav = None;
    let mut mute = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--say" => text = it.next().cloned().unwrap_or_default(),
            "--voice" => cfg.voice = it.next().cloned().unwrap_or_default(),
            "--speed" => cfg.speed = it.next().and_then(|s| s.parse().ok()).unwrap_or(1.0),
            "--wav" => wav = it.next().cloned(),
            "--no-stream" => cfg.streaming = false,
            "--fp32" => cfg.model = config::ModelVariant::Fp32,
            "--int8" => cfg.model = config::ModelVariant::Int8,
            "--threads" => cfg.threads = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            "--mute" => mute = true,
            "--cpu" => cfg.use_gpu = false,
            "--gpu" => cfg.use_gpu = true,
            _ => {}
        }
    }
    anyhow::ensure!(!text.is_empty(), "usage: sonorant --say \"text\" [--voice ID] [--speed X] [--wav FILE] [--no-stream] [--mute]");

    let cfg = Arc::new(RwLock::new(cfg));
    let (device, volume) = {
        let c = cfg.read();
        (c.output_device.clone(), if mute { 0.0 } else { c.volume })
    };
    let audio = audio::AudioHandle::spawn(device, volume);
    let tts = tts::Tts::spawn(cfg.clone(), audio, Arc::new(|| {}));
    let t_load = Instant::now();
    loop {
        match tts.status.lock().state.clone() {
            tts::EngineState::Ready => break,
            tts::EngineState::Error(e) => anyhow::bail!("engine failed: {e}"),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("model ready after {:.2}s", t_load.elapsed().as_secs_f32());

    for run in 1..=2 {
        println!("--- run {run} ---");
        for c in tts::text::plan_chunks(&text, cfg.read().streaming) {
            println!("chunk ({} chars, pause {} ms): {}", c.text.chars().count(), c.pause_ms, c.text);
        }
        *tts.tap.lock() = wav.as_ref().map(|_| Vec::new());
        let t0 = Instant::now();
        tts.speak(text.clone(), t0);
        tts::wait_until_idle(&tts, Duration::from_secs(120));
        if let Some(m) = tts.status.lock().last.clone() {
            println!(
                "first audio: {} ms (first inference {} ms) | audio {:.1}s, synth {:.2}s, RTF {:.2} | wall {:.2}s",
                m.first_audio_ms,
                m.first_synth_ms,
                m.audio_secs,
                m.synth_secs,
                m.rtf(),
                t0.elapsed().as_secs_f32()
            );
        }
    }

    if let (Some(path), Some(samples)) = (wav, tts.tap.lock().take()) {
        write_wav(&path, &samples)?;
        println!("wrote {path} ({} samples)", samples.len());
    }
    tts.shutdown();
    Ok(())
}

/// `sonorant.exe --transcribe FILE.wav [--stt whistle|canary|parakeet] [--lang en] [--no-vad] [--live]`
/// Transcribes a WAV file with the configured speech model and prints the text. With `--live` the
/// file is first played into the live transcript in real time, printing each partial result.
fn run_transcribe_cli(args: &[String]) -> anyhow::Result<()> {
    let mut cfg = Config::load();
    cfg.stt_enabled = true;
    let mut file = String::new();
    let mut live = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--transcribe" => file = it.next().cloned().unwrap_or_default(),
            "--live" => live = true,
            "--stt" => {
                cfg.stt_model = match it.next().map(String::as_str) {
                    Some("canary") => config::SttModel::Canary180M,
                    Some("parakeet") => config::SttModel::ParakeetUnified,
                    _ => config::SttModel::Whistle,
                }
            }
            "--cpu" => cfg.use_gpu = false,
            "--lang" => cfg.stt_language = it.next().cloned().unwrap_or_else(|| "en".into()),
            "--no-vad" => cfg.use_vad = false,
            "--threads" => cfg.threads = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    anyhow::ensure!(!file.is_empty(), "usage: sonorant --transcribe FILE.wav [--stt whistle|canary|parakeet] [--lang en] [--no-vad]");
    let pcm = stt::recorder::read_wav_16k(&file)?;
    println!("{file}: {:.1}s of audio", pcm.len() as f32 / 16_000.0);

    let cfg = Arc::new(RwLock::new(cfg));
    let t0 = Instant::now();
    let stt = stt::Stt::spawn(
        cfg.clone(),
        Arc::new(move |e| {
            if let stt::SttEvent::Partial(text) = e {
                println!("  {:>5} ms  {text}", t0.elapsed().as_millis());
            }
        }),
    );
    let t = Instant::now();
    stt.preload();
    loop {
        match stt.status.lock().state.clone() {
            stt::SttState::Ready => break,
            stt::SttState::Error(e) => anyhow::bail!("{e}"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    println!("model ready after {:.2}s", t.elapsed().as_secs_f32());
    if live {
        let (tx, audio) = std::sync::mpsc::channel();
        stt.live(audio);
        for piece in pcm.chunks(stt::recorder::STT_RATE as usize / 10) {
            let _ = tx.send(piece.to_vec());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    for run in 1..=2 {
        let t = Instant::now();
        let text = stt.transcribe_to_text(pcm.clone()).map_err(|e| anyhow::anyhow!(e))?;
        println!("run {run}: {} ms -> {text:?}", t.elapsed().as_millis());
    }
    stt.shutdown();
    Ok(())
}

fn write_wav(path: &str, samples: &[f32]) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: tts::engine::SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

/// Run a command-line mode with a console attached; exit with status 1 on error.
fn run_console(mode: impl FnOnce() -> anyhow::Result<()>) {
    platform::attach_console();
    init_logging();
    if let Err(e) = mode() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if let Some(test) = args.iter().find(|a| a.starts_with("--selftest-")) {
        return run_console(|| match test.as_str() {
            #[cfg(windows)]
            "--selftest-paste" => selftest::paste(),
            "--selftest-overlay" => selftest::overlay(),
            "--selftest-mic" => selftest::mic(),
            "--selftest-vram" => selftest::vram(),
            other => anyhow::bail!("unknown self-test {other}"),
        });
    }
    if has("--transcribe") {
        return run_console(|| run_transcribe_cli(&args));
    }
    if has("--say") {
        return run_console(|| run_cli(&args));
    }
    // `--cmd NAME` sends a command to the running instance (bind these to keys on Wayland desktops
    // without the GlobalShortcuts portal).
    let cmd = args.iter().skip_while(|a| *a != "--cmd").nth(1);
    let forward = match cmd.map(|c| platform::Command::parse(c).ok_or(c)) {
        Some(Ok(c)) => Some(c),
        Some(Err(c)) => {
            let names: Vec<_> = platform::Command::NAMES.iter().map(|(n, _)| *n).collect();
            return run_console(|| anyhow::bail!("unknown command {c:?}; one of: {}", names.join(", ")));
        }
        None => has("--settings").then_some(platform::Command::Settings),
    };
    init_logging();

    // Only one instance may own the hotkeys and the tray icon.
    let Ok(instance) = platform::Instance::claim(forward) else {
        log::warn!("Sonorant is already running");
        return;
    };
    if cmd.is_some() {
        return log::warn!("Sonorant is not running");
    }

    if let Err(e) = run_gui(instance, has("--settings")) {
        log::error!("fatal: {e:#}");
        std::process::exit(1);
    }
}

fn run_gui(instance: platform::Instance, open_settings: bool) -> anyhow::Result<()> {
    let event_loop = winit::event_loop::EventLoop::<app::UserEvent>::with_user_event().build()?;
    let mut app = app::App::new(event_loop.create_proxy(), instance, open_settings);
    event_loop.run_app(&mut app)?;
    Ok(())
}
