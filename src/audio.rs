//! Low-latency audio output.
//!
//! A dedicated thread owns the cpal stream (streams are not `Send`). The
//! stream stays open and plays silence when idle, so starting an utterance
//! never pays device start-up latency. Synthesized 24 kHz mono chunks are
//! pushed into a queue that the audio callback drains while resampling to the
//! device rate. The queue holds the chunks as pushed, so the callback only reads
//! and pops, and pushing never grows a big buffer while the callback waits.
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
    /// Audio not yet played, in the chunks it was pushed in.
    chunks: VecDeque<Vec<f32>>,
    /// Samples of the front chunk already played.
    pos: usize,
    /// Samples not yet played, in total.
    len: usize,
    epoch: u64,
}

impl Queue {
    /// The `i`-th sample not yet played.
    fn get(&self, i: usize) -> Option<f32> {
        let mut i = self.pos + i;
        for c in &self.chunks {
            if i < c.len() {
                return Some(c[i]);
            }
            i -= c.len();
        }
        None
    }

    /// Drop the next sample.
    fn pop(&mut self) {
        let Some(front) = self.chunks.front() else { return };
        self.pos += 1;
        self.len -= 1;
        if self.pos >= front.len() {
            self.chunks.pop_front();
            self.pos = 0;
        }
    }

    /// Empty the queue; the caller frees what it returns after releasing the lock.
    fn take(&mut self) -> VecDeque<Vec<f32>> {
        self.pos = 0;
        self.len = 0;
        std::mem::take(&mut self.chunks)
    }
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
            queue: Mutex::new(Queue { chunks: VecDeque::new(), pos: 0, len: 0, epoch: 0 }),
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
        let old = q.take();
        q.epoch += 1;
        let epoch = q.epoch;
        drop(q);
        drop(old);
        epoch
    }

    pub fn epoch(&self) -> u64 {
        self.shared.queue.lock().epoch
    }

    /// Queue samples for `epoch`; returns false (and drops them) if it is stale.
    pub fn push(&self, epoch: u64, samples: &[f32]) -> bool {
        // Copy outside the lock: the audio callback waits for it.
        let output = self.shared.output.load(Ordering::Relaxed);
        let chunk = if output && !samples.is_empty() { samples.to_vec() } else { Vec::new() };
        let mut q = self.shared.queue.lock();
        if q.epoch != epoch {
            return false;
        }
        if !chunk.is_empty() {
            q.len += chunk.len();
            q.chunks.push_back(chunk);
        }
        true
    }

    pub fn buffered_secs(&self) -> f32 {
        self.shared.queue.lock().len as f32 / SAMPLE_RATE as f32
    }

    pub fn is_playing(&self) -> bool {
        self.shared.queue.lock().len > 1
    }
}

fn audio_thread(shared: Arc<Shared>, rx: mpsc::Receiver<Cmd>, mut device_id: String) {
    // The open stream and its own flag for "the device is gone, build a new stream".
    let mut stream: Option<(Stream, Arc<AtomicBool>)> = None;
    let mut last_error = String::new();
    loop {
        if stream.as_ref().is_none_or(|(_, broken)| broken.load(Ordering::Relaxed)) {
            stream = None;
            let built = build_stream(&device_id, shared.clone());
            shared.output.store(built.is_ok(), Ordering::Relaxed);
            match built {
                Ok(s) => {
                    last_error.clear();
                    stream = Some(s);
                }
                Err(e) => {
                    let dropped = shared.queue.lock().take();
                    drop(dropped);
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

/// Open and start the output stream; the flag is set when it has to be rebuilt.
fn build_stream(device_id: &str, shared: Arc<Shared>) -> anyhow::Result<(Stream, Arc<AtomicBool>)> {
    let device = find_device(device_id, cpal::Host::default_output_device).context("no audio output device")?;

    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    log::info!("audio output: {:?} {:?}", device.description().map(|d| d.name().to_owned()), config);

    let broken = Arc::new(AtomicBool::new(false));
    let b = broken.clone();
    let stream = match format {
        SampleFormat::F32 => make_stream::<f32>(&device, config, shared, b)?,
        SampleFormat::I16 => make_stream::<i16>(&device, config, shared, b)?,
        SampleFormat::I32 => make_stream::<i32>(&device, config, shared, b)?,
        SampleFormat::U16 => make_stream::<u16>(&device, config, shared, b)?,
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    Ok((stream, broken))
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
                // Linear-interpolating resampler from 24 kHz to the device rate. It has no
                // low-pass filter, so a device below 24 kHz (e.g. a headset in call mode) gets
                // some aliasing; speech stays intelligible. Always keep the sample being
                // interpolated from, so playback resumes seamlessly after running dry.
                while frac >= 1.0 && q.len > 1 {
                    q.pop();
                    frac -= 1.0;
                }
                let s = match (q.get(0), q.get(1)) {
                    (Some(a), Some(b)) if frac < 1.0 => {
                        let v = a + (b - a) * frac as f32;
                        frac += step;
                        v * vol
                    }
                    // Underrun (or the end of the audio): silence until more arrives.
                    _ => 0.0,
                };
                let out: T = T::from_sample(s);
                for o in frame.iter_mut() {
                    *o = out;
                }
            }
        },
        move |err: cpal::Error| match err.kind() {
            // The device is gone (unplugged, or the system output changed): open it again.
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
                log::warn!("audio output lost: {err}");
                broken.store(true, Ordering::Relaxed);
            }
            cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged => log::debug!("audio stream: {err}"),
            _ => log::warn!("audio stream error: {err}"),
        },
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_reads_across_chunks() {
        let mut q = Queue { chunks: VecDeque::new(), pos: 0, len: 0, epoch: 0 };
        for c in [vec![1.0, 2.0], vec![3.0], vec![4.0, 5.0]] {
            q.len += c.len();
            q.chunks.push_back(c);
        }
        let mut played = Vec::new();
        while let Some(s) = q.get(0) {
            assert_eq!(q.get(1).is_some(), q.len > 1);
            played.push(s);
            q.pop();
        }
        assert_eq!(played, [1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!((q.len, q.chunks.len()), (0, 0));
    }
}
