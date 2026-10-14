//! Silence removal with Silero VAD (v5, ONNX), run on the shared ONNX Runtime.
//!
//! The model scores 512-sample frames (32 ms at 16 kHz), each preceded by 64 samples of context.
//! Frames judged to be speech, plus a little padding either side, are kept; the rest is dropped.
//! If no speech is found the result is empty, which also avoids the hallucinated text speech
//! models produce on silence.

use std::path::Path;

use anyhow::Result;
use ort::session::Session;
use ort::value::Tensor;

const FRAME: usize = 512;
const CONTEXT: usize = 64;
const THRESHOLD: f32 = 0.5;
/// Frames kept before / after speech (~190 ms / ~290 ms).
const PAD_BEFORE: usize = 6;
const PAD_AFTER: usize = 9;
/// Minimum total speech (frames) worth transcribing (~160 ms).
const MIN_SPEECH_FRAMES: usize = 5;

pub struct Vad {
    session: Session,
}

impl Vad {
    /// Load the model. A file ONNX Runtime cannot read is removed from the cache, so a retry
    /// downloads it again.
    pub fn load(path: &Path) -> Result<Self> {
        crate::tts::engine::init_runtime()?;
        let mut builder = (|| -> ort::Result<_> { Ok(Session::builder()?.with_intra_threads(1)?.with_inter_threads(1)?) })()?;
        let session = builder.commit_from_file(path).inspect_err(|_| crate::models::hub::evict_if_damaged(path))?;
        Ok(Self { session })
    }

    /// Speech probability for every 512-sample frame.
    fn probabilities(&mut self, pcm: &[f32]) -> Result<Vec<f32>> {
        let mut state = vec![0f32; 2 * 128];
        let mut context = vec![0f32; CONTEXT];
        let mut probs = Vec::with_capacity(pcm.len() / FRAME);
        for frame in pcm.as_chunks::<FRAME>().0 {
            let mut input = Vec::with_capacity(CONTEXT + FRAME);
            input.extend_from_slice(&context);
            input.extend_from_slice(frame);
            context.copy_from_slice(&frame[FRAME - CONTEXT..]);

            let input = Tensor::from_array(([1usize, CONTEXT + FRAME], input))?;
            let st = Tensor::from_array(([2usize, 1, 128], state.clone()))?;
            let sr = Tensor::from_array(((), vec![16_000i64]))?;
            let out = self.session.run(ort::inputs!["input" => input, "state" => st, "sr" => sr])?;
            let (_, p) = out["output"].try_extract_tensor::<f32>()?;
            probs.push(p[0]);
            let (_, s) = out["stateN"].try_extract_tensor::<f32>()?;
            state.copy_from_slice(s);
        }
        Ok(probs)
    }

    /// Keep only the speech (with padding). Empty if there is none.
    pub fn trim(&mut self, pcm: &[f32]) -> Result<Vec<f32>> {
        let probs = self.probabilities(pcm)?;
        let speech: Vec<bool> = probs.iter().map(|&p| p >= THRESHOLD).collect();
        if speech.iter().filter(|&&s| s).count() < MIN_SPEECH_FRAMES {
            return Ok(Vec::new());
        }
        let n = speech.len();
        let mut keep = vec![false; n];
        for i in speech.iter().enumerate().filter_map(|(i, &s)| s.then_some(i)) {
            keep[i.saturating_sub(PAD_BEFORE)..=(i + PAD_AFTER).min(n - 1)].fill(true);
        }
        let frames = pcm.as_chunks::<FRAME>().0;
        Ok(frames.iter().zip(keep).filter(|(_, k)| *k).flat_map(|(f, _)| f).copied().collect())
    }
}
