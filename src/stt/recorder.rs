//! Microphone capture for dictation. A dedicated thread owns the cpal input stream (streams are
//! not `Send`); `start` opens it, `stop` closes it and returns the audio as 16 kHz mono f32.
//! While recording, the audio can also be streamed out in small pieces for a live transcript.

use std::f64::consts::PI;
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream};

use crate::audio::{describe_devices, find_device};

pub const STT_RATE: u32 = 16_000;
/// Hard cap on a single recording (seconds of audio at the device rate).
const MAX_SECS: usize = 600;
/// How often captured audio is collected from the input callback (and handed to the live transcript).
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);
/// Capacity of the ring between the input callback and the recorder thread, in seconds of audio:
/// far more than `DRAIN_INTERVAL`, so only a stalled recorder thread loses audio.
const RING_SECS: usize = 2;

/// Receives the recording as it happens (16 kHz mono pieces); disconnects when it stops.
pub type LiveAudio = Receiver<Vec<f32>>;

/// Called on the recorder thread when the input device has been opened (or could not be).
pub type Started = Box<dyn FnOnce(Result<(), String>) + Send>;
/// Called on the recorder thread with the finished recording (16 kHz mono) and whether it was cut
/// off at the length limit.
pub type Stopped = Box<dyn FnOnce(Vec<f32>, bool) + Send>;
/// Called (once per recording) when the input device disappears while recording.
pub type OnLost = Arc<dyn Fn() + Send + Sync>;

enum Cmd {
    Start { device: String, live: Option<Sender<Vec<f32>>>, done: Started },
    Stop { done: Stopped },
}

/// Opening and closing a device can take seconds (Bluetooth, exclusive-mode apps), so neither call
/// waits for it: the result arrives through a callback, and commands run in order.
#[derive(Clone)]
pub struct Recorder {
    tx: Sender<Cmd>,
}

/// Test hook: when `SONORANT_TEST_WAV` names a WAV file, recordings return that audio instead of
/// using the microphone, so the whole dictation pipeline can be exercised without speaking.
/// Only with the `test-hooks` feature. The file is read once.
fn test_wav() -> Option<Vec<f32>> {
    #[cfg(feature = "test-hooks")]
    {
        static WAV: std::sync::OnceLock<Option<Vec<f32>>> = std::sync::OnceLock::new();
        return WAV
            .get_or_init(|| {
                let path = std::env::var_os("SONORANT_TEST_WAV")?;
                read_wav_16k(&path.to_string_lossy()).inspect_err(|e| log::error!("SONORANT_TEST_WAV: {e:#}")).ok()
            })
            .clone();
    }
    #[cfg(not(feature = "test-hooks"))]
    None
}

fn test_wav_set() -> bool {
    test_wav().is_some()
}

/// Input devices as (id, label) pairs for the settings dialog.
pub fn list_input_devices() -> Vec<(String, String)> {
    describe_devices(cpal::default_host().input_devices())
}

impl Recorder {
    pub fn spawn(on_lost: OnLost) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("recorder".into()).spawn(move || run(rx, on_lost)).expect("spawn recorder thread");
        Self { tx }
    }

    /// Begin recording from `device` (empty = default microphone). Returns the live audio feed at
    /// once; `done` reports whether the device could be opened.
    pub fn start(&self, device: &str, done: Started) -> LiveAudio {
        let (live, live_rx) = mpsc::channel();
        if test_wav_set() {
            done(Ok(())); // no live feed: `live` is dropped, so it ends at once
            return live_rx;
        }
        let cmd = Cmd::Start { device: device.to_owned(), live: Some(live), done };
        if let Err(mpsc::SendError(Cmd::Start { done, .. })) = self.tx.send(cmd) {
            done(Err("the recorder thread is gone".into()));
        }
        live_rx
    }

    /// Stop recording; `done` gets the audio (16 kHz mono, empty if nothing was recording).
    pub fn stop(&self, done: Stopped) {
        if let Some(pcm) = test_wav() {
            return done(pcm, false);
        }
        if let Err(mpsc::SendError(Cmd::Stop { done })) = self.tx.send(Cmd::Stop { done }) {
            done(Vec::new(), false);
        }
    }

    /// Stop recording and wait for the audio (command-line self-tests only).
    pub fn stop_blocking(&self) -> Vec<f32> {
        let (tx, rx) = mpsc::channel();
        self.stop(Box::new(move |pcm, _| {
            let _ = tx.send(pcm);
        }));
        rx.recv().unwrap_or_default()
    }
}

struct Active {
    /// `None` once stopped.
    stream: Option<Stream>,
    /// Mono audio from the input callback, which must neither lock nor allocate.
    ring: rtrb::Consumer<f32>,
    /// Set by the callback when the ring was full and audio was lost.
    overflow: Arc<AtomicBool>,
    /// Set by the stream's error callback when the device is gone (unplugged, disabled).
    lost: Arc<AtomicBool>,
    lost_reported: bool,
    /// The recording so far, mono at the device rate (at most `MAX_SECS`).
    buf: Vec<f32>,
    cap: usize,
    truncated: bool,
    resampler: Resampler,
    /// The recording converted to 16 kHz, as far as no further input can change it.
    out: Vec<f32>,
    live: Option<Live>,
}

struct Live {
    tx: Sender<Vec<f32>>,
    /// 16 kHz samples already sent.
    sent: usize,
}

impl Active {
    /// Move the audio captured since the last call out of the ring and convert what is ready.
    fn drain(&mut self) {
        if self.overflow.swap(false, Ordering::Relaxed) {
            log::warn!("microphone audio lost: the recorder fell behind");
        }
        let Ok(chunk) = self.ring.read_chunk(self.ring.slots()) else { return };
        let (a, b) = chunk.as_slices();
        let room = self.cap - self.buf.len();
        if a.len() + b.len() > room && !self.truncated {
            self.truncated = true;
            log::warn!("recording reached the {MAX_SECS} s limit; the rest is dropped");
        }
        let a_take = a.len().min(room);
        self.buf.extend_from_slice(&a[..a_take]);
        self.buf.extend_from_slice(&b[..b.len().min(room - a_take)]);
        chunk.commit_all();
        let ready = self.resampler.ready_len(self.buf.len());
        if ready > self.out.len() {
            let piece = self.resampler.process(&self.buf, self.out.len()..ready);
            self.out.extend(piece);
        }
    }

    /// Send the audio recorded since the last call to the live feed.
    fn feed_live(&mut self) {
        let Some(live) = &mut self.live else { return };
        if self.out.len() <= live.sent {
            return;
        }
        let piece = self.out[live.sent..].to_vec();
        live.sent = self.out.len();
        if live.tx.send(piece).is_err() {
            self.live = None; // nobody is listening
        }
    }

    /// End the recording and return all of it at 16 kHz.
    fn finish(mut self) -> (Vec<f32>, bool) {
        self.stream = None; // no more audio after this
        self.drain();
        // Only the last few samples still need converting.
        let end = self.resampler.out_len(self.buf.len());
        if end > self.out.len() {
            let tail = self.resampler.process(&self.buf, self.out.len()..end);
            self.out.extend(tail);
        }
        (self.out, self.truncated)
    }
}

fn run(rx: Receiver<Cmd>, on_lost: OnLost) {
    let mut active: Option<Active> = None;
    loop {
        let cmd = match &active {
            Some(_) => rx.recv_timeout(DRAIN_INTERVAL),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match cmd {
            Ok(Cmd::Start { device, live, done }) => {
                active = None; // restart cleanly if a previous recording was left open
                let result = open(&device, live).map(|a| active = Some(a));
                done(result.map_err(|e| format!("{e:#}")));
            }
            Ok(Cmd::Stop { done }) => {
                // Dropping the recording also ends its live feed.
                let (samples, cut) = active.take().map(Active::finish).unwrap_or_default();
                done(samples, cut);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(a) = &mut active {
                    a.drain();
                    a.feed_live();
                    if a.lost.load(Ordering::Relaxed) && !a.lost_reported {
                        a.lost_reported = true;
                        on_lost();
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn open(device_id: &str, live: Option<Sender<Vec<f32>>>) -> anyhow::Result<Active> {
    let device = find_device(device_id, cpal::Host::default_input_device).context("no microphone found")?;

    let supported = device.default_input_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate;
    let (producer, ring) = rtrb::RingBuffer::new(rate as usize * RING_SECS);
    let overflow = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicBool::new(false));
    let (o, l) = (overflow.clone(), lost.clone());
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, producer, o, l)?,
        SampleFormat::F64 => build::<f64>(&device, config, producer, o, l)?,
        SampleFormat::I8 => build::<i8>(&device, config, producer, o, l)?,
        SampleFormat::I16 => build::<i16>(&device, config, producer, o, l)?,
        SampleFormat::I24 => build::<cpal::I24>(&device, config, producer, o, l)?,
        SampleFormat::I32 => build::<i32>(&device, config, producer, o, l)?,
        SampleFormat::U8 => build::<u8>(&device, config, producer, o, l)?,
        SampleFormat::U16 => build::<u16>(&device, config, producer, o, l)?,
        other => anyhow::bail!("unsupported microphone sample format {other:?}"),
    };
    stream.play()?;
    let live = live.map(|tx| Live { tx, sent: 0 });
    Ok(Active {
        stream: Some(stream),
        ring,
        overflow,
        lost,
        lost_reported: false,
        buf: Vec::with_capacity(rate as usize * 10),
        cap: rate as usize * MAX_SECS,
        truncated: false,
        resampler: Resampler::new(rate, STT_RATE),
        out: Vec::with_capacity(STT_RATE as usize * 10),
        live,
    })
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut ring: rtrb::Producer<f32>,
    overflow: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
) -> anyhow::Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            // Downmix to mono. Runs on the audio thread: no locks, no allocation.
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
                if ring.push(sum / channels as f32).is_err() {
                    overflow.store(true, Ordering::Relaxed);
                    return;
                }
            }
        },
        move |err: cpal::Error| match err.kind() {
            // The device is gone (unplugged, or the system input changed): the app is told.
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
                log::warn!("microphone lost: {err}");
                lost.store(true, Ordering::Relaxed);
            }
            _ => log::warn!("microphone stream error: {err}"),
        },
        None,
    )?)
}

/// Read a WAV file (any integer or float sample format) as 16 kHz mono f32.
pub fn read_wav_16k(path: &str) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("reading {path}"))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|s| s as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    let channels = spec.channels.max(1) as usize;
    let mono: Vec<f32> = samples.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect();
    anyhow::ensure!(!mono.is_empty(), "no audio data in {path}");
    Ok(resample(&mono, spec.sample_rate, STT_RATE))
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Band-limited (windowed-sinc) resampler. Fine for speech; runs once per recording.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let r = Resampler::new(from, to);
    r.process(input, 0..r.out_len(input.len()))
}

/// Windowed-sinc resampling from one fixed rate to another. Any range of output samples can be
/// computed on its own, so a growing recording can be converted piece by piece with exactly the
/// result of converting it in one go.
pub struct Resampler {
    /// The rate ratio in lowest terms: output sample i sits at input position i * p / q.
    p: usize,
    q: usize,
    /// Kernel half-width in input samples.
    half: usize,
    cutoff: f64,
    /// Kernels for each of the q distinct fractional positions (when q is small enough to cache):
    /// one for 48 -> 16 kHz, 160 for 44.1 -> 16 kHz.
    table: Vec<Vec<f64>>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let g = gcd(from, to);
        let (p, q) = ((from / g) as usize, (to / g) as usize);
        let cutoff = (to as f64 / from as f64).min(1.0) * 0.95; // low-pass below the new Nyquist when downsampling
        let half = (16.0 / cutoff).ceil() as usize;
        let mut r = Self { p, q, half, cutoff, table: Vec::new() };
        if p != q && q <= 4096 {
            r.table = (0..q).map(|i| r.kernel(i as f64 / q as f64)).collect();
        }
        r
    }

    /// Kernel taps for an output sample that sits `frac` (0..1) past an input sample.
    fn kernel(&self, frac: f64) -> Vec<f64> {
        let (half, cutoff) = (self.half as f64, self.cutoff);
        (0..=2 * self.half)
            .map(|k| {
                let x = k as f64 - half - frac;
                let sinc = if x.abs() < 1e-9 { 1.0 } else { (PI * cutoff * x).sin() / (PI * cutoff * x) };
                let window = 0.5 * (1.0 + (PI * x / (half + 1.0)).cos());
                sinc * window
            })
            .collect()
    }

    /// Output length for `input_len` input samples.
    pub fn out_len(&self, input_len: usize) -> usize {
        input_len * self.q / self.p
    }

    /// Output samples that no further input can change (their kernel lies entirely in `input_len`).
    pub fn ready_len(&self, input_len: usize) -> usize {
        if self.p == self.q { input_len } else { input_len.saturating_sub(self.half + 1) * self.q / self.p }
    }

    /// Output samples `range` (edge samples are renormalised to the taps inside the input).
    pub fn process(&self, input: &[f32], range: Range<usize>) -> Vec<f32> {
        if self.p == self.q {
            return input[range].to_vec();
        }
        let half = self.half;
        range
            .map(|i| {
                let (center, r) = (i * self.p / self.q, i * self.p % self.q);
                let computed;
                let taps: &[f64] = match self.table.get(r) {
                    Some(t) => t,
                    None => {
                        computed = self.kernel(r as f64 / self.q as f64);
                        &computed
                    }
                };
                let first = half.saturating_sub(center);
                let last = (input.len() + half - center).min(taps.len());
                let start = center + first - half;
                let (acc, norm) = taps[first..last]
                    .iter()
                    .zip(&input[start..])
                    .fold((0.0f64, 0.0f64), |(acc, norm), (&w, &s)| (acc + s as f64 * w, norm + w));
                if norm.abs() > 1e-9 { (acc / norm) as f32 } else { 0.0 }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_preserves_a_tone() {
        // 1 kHz tone at 48 kHz -> 16 kHz keeps its frequency and amplitude.
        let from = 48_000u32;
        let input: Vec<f32> = (0..from as usize / 2).map(|n| (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / from as f32).sin()).collect();
        let out = resample(&input, from, 16_000);
        assert_eq!(out.len(), 8000);
        let rms = (out[200..7800].iter().map(|x| x * x).sum::<f32>() / 7600.0).sqrt();
        assert!((rms - 0.707).abs() < 0.03, "rms {rms}");
        // A 12 kHz tone is above the new Nyquist (8 kHz) and must be strongly attenuated.
        let hi: Vec<f32> = (0..from as usize / 2).map(|n| (2.0 * std::f32::consts::PI * 12_000.0 * n as f32 / from as f32).sin()).collect();
        let o2 = resample(&hi, from, 16_000);
        let rms2 = (o2[200..7800].iter().map(|x| x * x).sum::<f32>() / 7600.0).sqrt();
        assert!(rms2 < 0.1, "alias rms {rms2}");
    }

    #[test]
    fn piecewise_resampling_matches_one_pass() {
        for from in [48_000u32, 44_100] {
            let input: Vec<f32> = (0..from as usize).map(|n| (n as f32 * 0.37).sin()).collect();
            let r = Resampler::new(from, 16_000);
            let whole = r.process(&input, 0..r.out_len(input.len()));
            // Feed it the way a recording grows, converting what is ready after every 10 ms.
            let mut pieces = Vec::new();
            for len in (0..input.len()).step_by(from as usize / 100) {
                let ready = r.ready_len(len);
                pieces.extend(r.process(&input[..len], pieces.len()..ready.max(pieces.len())));
            }
            let ready = r.ready_len(input.len());
            pieces.extend(r.process(&input, pieces.len()..ready));
            assert_eq!(pieces, whole[..ready], "{from} Hz");
        }
    }
}
