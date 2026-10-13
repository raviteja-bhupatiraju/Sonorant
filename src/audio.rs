//! Low-latency audio output.
//!
//! A dedicated thread owns the cpal stream (streams are not `Send`). The
//! stream stays open and plays silence when idle, so starting an utterance
//! never pays device start-up latency. Synthesized 24 kHz mono chunks are
//! pushed into a queue that the audio callback drains while resampling to the
//! device rate.
//!
//! Every utterance has an *epoch*. Starting a new utterance (or stopping)
//! bumps the epoch and clears the queue atomically, so audio from a stale
//! synthesis job can never leak into the next one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream};
use parking_lot::Mutex;

use crate::tts::engine::SAMPLE_RATE;

struct Queue {
    buf: VecDeque<f32>,
    epoch: u64,
}

struct Shared {
    queue: Mutex<Queue>,
    volume: AtomicU32,
    /// False while no output device could be opened: audio is then dropped instead of queued, so
    /// nothing waits for playback that can never happen.
    output: AtomicBool,
}

enum Cmd {
    SetDevice(String),
}

#[derive(Clone)]
pub struct AudioHandle {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
}

/// Devices as (id, label) pairs for the settings dialog.
pub fn describe_devices(devices: Result<impl Iterator<Item = Device>, cpal::Error>) -> Vec<(String, String)> {
    let Ok(devices) = devices else { return Vec::new() };
    devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let name = d.description().map(|x| x.name().to_owned()).unwrap_or_else(|_| id.clone());
            Some((id, name))
        })
        .collect()
}

pub fn list_output_devices() -> Vec<(String, String)> {
    describe_devices(cpal::default_host().output_devices())
}

/// The device with the stored `id` (empty = none chosen), else the system default from `default`.
pub fn find_device(id: &str, default: impl FnOnce(&cpal::Host) -> Option<Device>) -> Option<Device> {
    let host = cpal::default_host();
    (!id.is_empty())
        .then(|| id.parse().ok())
        .flatten()
        .and_then(|id| host.device_by_id(&id))
        .or_else(|| default(&host))
}

impl AudioHandle {
    pub fn spawn(device_id: String, volume: f32) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue { buf: VecDeque::new(), epoch: 0 }),
            volume: AtomicU32::new(volume.to_bits()),
            output: AtomicBool::new(true), // until the device fails to open
        });
        let (tx, rx) = mpsc::channel();
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || audio_thread(thread_shared, rx, device_id))
            .expect("spawn audio thread");
        Self { shared, tx }
    }

    pub fn set_device(&self, id: String) {
        let _ = self.tx.send(Cmd::SetDevice(id));
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Start a new utterance: drop anything queued and invalidate older epochs.
    pub fn begin(&self) -> u64 {
        let mut q = self.shared.queue.lock();
        q.buf.clear();
        q.epoch += 1;
        q.epoch
    }

    pub fn epoch(&self) -> u64 {
        self.shared.queue.lock().epoch
    }

    /// Queue samples for `epoch`; returns false (and drops them) if it is stale.
    pub fn push(&self, epoch: u64, samples: &[f32]) -> bool {
        let mut q = self.shared.queue.lock();
        if q.epoch != epoch {
            return false;
        }
        if self.shared.output.load(Ordering::Relaxed) {
            q.buf.extend(samples.iter().copied());
        }
        true
    }

    pub fn buffered_secs(&self) -> f32 {
        self.shared.queue.lock().buf.len() as f32 / SAMPLE_RATE as f32
    }

    pub fn is_playing(&self) -> bool {
        self.shared.queue.lock().buf.len() > 1
    }
}

fn audio_thread(shared: Arc<Shared>, rx: mpsc::Receiver<Cmd>, mut device_id: String) {
    let broken = Arc::new(AtomicBool::new(false));
    let mut stream: Option<Stream> = None;
    let mut last_error = String::new();
    loop {
        if stream.is_none() || broken.swap(false, Ordering::Relaxed) {
            stream = None;
            let built = build_stream(&device_id, shared.clone(), broken.clone());
            shared.output.store(built.is_ok(), Ordering::Relaxed);
            match built {
                Ok(s) => {
                    last_error.clear();
                    stream = Some(s);
                }
                Err(e) => {
                    shared.queue.lock().buf.clear();
                    let msg = format!("{e:#}");
                    if msg != last_error {
                        log::error!("audio output unavailable: {msg}");
                        last_error = msg;
                    }
                }
            }
        }
        let wait = if stream.is_some() { Duration::from_millis(500) } else { Duration::from_secs(2) };
        match rx.recv_timeout(wait) {
            Ok(Cmd::SetDevice(id)) => {
                device_id = id;
                stream = None;
            }
            // The last `AudioHandle` was dropped.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn build_stream(device_id: &str, shared: Arc<Shared>, broken: Arc<AtomicBool>) -> anyhow::Result<Stream> {
    let device = find_device(device_id, cpal::Host::default_output_device).context("no audio output device")?;

    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    log::info!("audio output: {:?} {:?}", device.description().map(|d| d.name().to_owned()), config);

    let stream = match format {
        SampleFormat::F32 => make_stream::<f32>(&device, config, shared, broken)?,
        SampleFormat::I16 => make_stream::<i16>(&device, config, shared, broken)?,
        SampleFormat::I32 => make_stream::<i32>(&device, config, shared, broken)?,
        SampleFormat::U16 => make_stream::<u16>(&device, config, shared, broken)?,
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    Ok(stream)
}

fn make_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
    broken: Arc<AtomicBool>,
) -> anyhow::Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let step = SAMPLE_RATE as f64 / config.sample_rate as f64;
    let mut frac = 0.0f64;
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let vol = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            let mut q = shared.queue.lock();
            for frame in data.chunks_mut(channels) {
                let s = if q.buf.len() >= 2 {
                    // Linear-interpolating resampler from 24 kHz to the device rate.
                    let (a, b) = (q.buf[0], q.buf[1]);
                    let v = a + (b - a) * frac as f32;
                    frac += step;
                    while frac >= 1.0 && !q.buf.is_empty() {
                        q.buf.pop_front();
                        frac -= 1.0;
                    }
                    v * vol
                } else {
                    frac = 0.0;
                    q.buf.clear();
                    0.0
                };
                let out: T = T::from_sample(s);
                for o in frame.iter_mut() {
                    *o = out;
                }
            }
        },
        move |err| {
            log::warn!("audio stream error: {err}");
            broken.store(true, Ordering::Relaxed);
        },
        None,
    )?)
}
