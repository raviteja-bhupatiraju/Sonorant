//! Microphone capture for dictation. A dedicated thread owns the cpal input stream (streams are
//! not `Send`); `start` opens it, `stop` closes it and returns the audio as 16 kHz mono f32.
//! While recording, the audio can also be streamed out in small pieces for a live transcript.

use std::f64::consts::PI;
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream};
use parking_lot::Mutex;

use crate::audio::{describe_devices, find_device};

pub const STT_RATE: u32 = 16_000;
/// Hard cap on a single recording (seconds of audio at the device rate).
const MAX_SECS: usize = 600;
/// How often new audio is handed to the live transcript.
const LIVE_INTERVAL: Duration = Duration::from_millis(100);

/// Receives the recording as it happens (16 kHz mono pieces); disconnects when it stops.
pub type LiveAudio = Receiver<Vec<f32>>;

enum Cmd {
    Start { device: String, live: Option<Sender<Vec<f32>>>, reply: Sender<Result<(), String>> },
    Stop { reply: Sender<Vec<f32>> },
}

#[derive(Clone)]
pub struct Recorder {
    tx: Sender<Cmd>,
}

/// Test hook: when `SONORANT_TEST_WAV` names a WAV file, recordings return that audio instead of
/// using the microphone, so the whole dictation pipeline can be exercised without speaking.
fn test_wav() -> Option<Vec<f32>> {
    let path = std::env::var_os("SONORANT_TEST_WAV")?;
    read_wav_16k(&path.to_string_lossy()).ok()
}

/// Input devices as (id, label) pairs for the settings dialog.
pub fn list_input_devices() -> Vec<(String, String)> {
    describe_devices(cpal::default_host().input_devices())
}

impl Recorder {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("recorder".into()).spawn(move || run(rx)).expect("spawn recorder thread");
        Self { tx }
    }

    /// Begin recording from `device` (empty = default microphone). Returns the live audio feed.
    pub fn start(&self, device: &str) -> Result<LiveAudio, String> {
        let (live, live_rx) = mpsc::channel();
        if test_wav().is_some() {
            return Ok(live_rx); // no live feed: `live` is dropped, so it ends at once
        }
        let (reply, rx) = mpsc::channel();
        self.tx.send(Cmd::Start { device: device.to_owned(), live: Some(live), reply }).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())??;
        Ok(live_rx)
    }

    /// Stop recording and return the audio (16 kHz mono); empty if nothing was recording.
    pub fn stop(&self) -> Vec<f32> {
        if let Some(pcm) = test_wav() {
            return pcm;
        }
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Stop { reply }).is_err() {
            return Vec::new();
        }
        rx.recv().unwrap_or_default()
    }
}

struct Active {
    _stream: Stream,
    buf: Arc<Mutex<Vec<f32>>>,
    resampler: Resampler,
    live: Option<Live>,
}

struct Live {
    tx: Sender<Vec<f32>>,
    /// 16 kHz samples already sent.
    sent: usize,
}

impl Active {
    /// Send the audio recorded since the last call to the live feed.
    fn feed_live(&mut self) {
        let Some(live) = &mut self.live else { return };
        let piece = {
            let buf = self.buf.lock();
            let ready = self.resampler.ready_len(buf.len());
            if ready <= live.sent {
                return;
            }
            self.resampler.process(&buf, live.sent..ready)
        };
        live.sent += piece.len();
        if live.tx.send(piece).is_err() {
            self.live = None; // nobody is listening
        }
    }
}

fn run(rx: Receiver<Cmd>) {
    let mut active: Option<Active> = None;
    loop {
        let cmd = match &active {
            Some(a) if a.live.is_some() => rx.recv_timeout(LIVE_INTERVAL),
            _ => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match cmd {
            Ok(Cmd::Start { device, live, reply }) => {
                active = None; // restart cleanly if a previous recording was left open
                let result = open(&device, live).map(|a| active = Some(a));
                let _ = reply.send(result.map_err(|e| format!("{e:#}")));
            }
            Ok(Cmd::Stop { reply }) => {
                // Dropping the recording also ends its live feed.
                let samples = active.take().map(|a| {
                    drop(a._stream);
                    let buf = a.buf.lock();
                    a.resampler.process(&buf, 0..a.resampler.out_len(buf.len()))
                });
                let _ = reply.send(samples.unwrap_or_default());
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(a) = &mut active {
                    a.feed_live();
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
    let buf = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 10)));
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, buf.clone())?,
        SampleFormat::I16 => build::<i16>(&device, config, buf.clone())?,
        SampleFormat::I32 => build::<i32>(&device, config, buf.clone())?,
        SampleFormat::U16 => build::<u16>(&device, config, buf.clone())?,
        other => anyhow::bail!("unsupported microphone sample format {other:?}"),
    };
    stream.play()?;
    let live = live.map(|tx| Live { tx, sent: 0 });
    Ok(Active { _stream: stream, buf, resampler: Resampler::new(rate, STT_RATE), live })
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, buf: Arc<Mutex<Vec<f32>>>) -> anyhow::Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let cap = config.sample_rate as usize * MAX_SECS;
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            let mut b = buf.lock();
            if b.len() >= cap {
                return;
            }
            // Downmix to mono.
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
                b.push(sum / channels as f32);
            }
        },
        |err| log::warn!("microphone stream error: {err}"),
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
