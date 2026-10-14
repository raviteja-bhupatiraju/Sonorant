//! Kokoro ONNX inference and voice (style vector) loading.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ort::session::{builder::GraphOptimizationLevel, RunOptions, Session};
use ort::value::Tensor;

use crate::models;

pub const SAMPLE_RATE: u32 = 24_000;
const STYLE_DIM: usize = 256;
const STYLE_ROWS: usize = 510;

/// Voice style tables: one raw little-endian f32 file of `[510, 1, 256]` per voice.
pub struct Voices {
    map: BTreeMap<String, Vec<f32>>,
}

impl Voices {
    /// Damaged voice files are evicted from the cache so a retry downloads them again.
    pub fn load(files: &[(String, PathBuf)]) -> Result<Self> {
        let mut map = BTreeMap::new();
        for (name, path) in files {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()));
            let bytes = bytes.inspect_err(|_| models::hub::evict_if_damaged(path))?;
            let v: Vec<f32> = bytes.as_chunks().0.iter().copied().map(f32::from_le_bytes).collect();
            if v.len() != STYLE_ROWS * STYLE_DIM {
                models::hub::evict_if_damaged(path);
                bail!("unexpected voice shape in {} ({} floats)", path.display(), v.len());
            }
            map.insert(name.clone(), v);
        }
        if map.is_empty() {
            bail!("no voices found");
        }
        Ok(Self { map })
    }

    pub fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// Style vector for an utterance of `n_tokens` phoneme tokens (without the pad tokens).
    ///
    /// Matches the reference implementation (hexgrad `KPipeline`: `pack[len(ps) - 1]`, where `ps`
    /// is the phoneme string, one token per phoneme, before padding). Some ports (kokoro-onnx)
    /// index with `len(tokens)` instead, one row further.
    pub fn style(&self, name: &str, n_tokens: usize) -> Option<&[f32]> {
        let v = self.map.get(name)?;
        let row = n_tokens.saturating_sub(1).min(STYLE_ROWS - 1);
        Some(&v[row * STYLE_DIM..(row + 1) * STYLE_DIM])
    }
}

pub struct Engine {
    session: Session,
    pub voices: Voices,
    /// Input / output tensor names (they differ between ONNX exports).
    names: TensorNames,
}

struct TensorNames {
    tokens: String,
    style: String,
    speed: String,
    audio: String,
}

#[cfg(windows)]
const ORT_LIB: &str = "onnxruntime.dll";
#[cfg(target_os = "linux")]
const ORT_LIB: &str = "libonnxruntime.so";

/// Locate and load the ONNX Runtime shared library (must happen once, before any session).
pub fn init_runtime() -> Result<()> {
    // Only success is remembered, so a retry after installing the missing library works without a restart.
    static INIT: parking_lot::Mutex<bool> = parking_lot::Mutex::new(false);
    let mut done = INIT.lock();
    if *done {
        return Ok(());
    }
    (|| -> Result<(), String> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("ORT_DYLIB_PATH") {
            candidates.push(p.into());
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            candidates.push(dir.join(ORT_LIB));
        }
        // Distribution packages (e.g. Arch's onnxruntime-cpu) install it system-wide.
        #[cfg(target_os = "linux")]
        candidates.extend(["/usr/lib", "/usr/local/lib", "/usr/lib/x86_64-linux-gnu"].map(|d| Path::new(d).join(ORT_LIB)));
        let Some(path) = candidates.into_iter().find(|p| p.is_file()) else {
            return Err(format!("{ORT_LIB} (ONNX Runtime) not found next to Sonorant or installed (see README)"));
        };
        log::info!("using ONNX Runtime at {}", path.display());
        ort::init_from(&path).map_err(|e| e.to_string())?.commit();
        Ok(())
    })()
    .map_err(|e| anyhow!(e))?;
    *done = true;
    Ok(())
}

impl Engine {
    pub fn load(files: &models::KokoroFiles, threads: usize) -> Result<Self> {
        init_runtime()?;
        let voices = Voices::load(&files.voices)?;
        // Kokoro runs on the CPU: ONNX Runtime's DirectML provider fails on this model's ConvTranspose
        // nodes (tested), and the CPU already reaches the first audio in about 200 ms.
        let mut builder = (|| -> ort::Result<_> {
            Ok(Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::Level3)?
                .with_intra_threads(threads)?
                .with_inter_threads(1)?
                .with_parallel_execution(false)?
                // Input length changes with every chunk; memory-pattern reuse only hurts.
                .with_memory_pattern(false)?)
        })()?;
        // A model file ONNX Runtime cannot load may be damaged: evict it (if it really is) so a
        // retry downloads it again.
        let session = builder.commit_from_file(&files.model).inspect_err(|_| models::hub::evict_if_damaged(&files.model))?;
        // Fail early with a clear message if espeak-ng's data folder is missing
        // (it must sit next to the exe as `espeak-ng-data`).
        crate::tts::text::phonemize("test", crate::tts::text::LANG)
            .context("speech data (espeak-ng-data folder) not found or unusable")?;
        let inputs: Vec<String> = session.inputs().iter().map(|i| i.name().to_owned()).collect();
        let Ok([tokens, style, speed]) = <[String; 3]>::try_from(inputs) else {
            bail!("unexpected Kokoro model inputs: {:?}", session.inputs());
        };
        let audio = session.outputs().first().map(|o| o.name().to_owned()).unwrap_or_default();
        let names = TensorNames { tokens, style, speed, audio };
        let mut engine = Self { session, voices, names };
        engine.warm_up();
        Ok(engine)
    }

    /// Run a couple of throw-away inferences so the first real utterance does
    /// not pay for kernel selection and allocator growth.
    fn warm_up(&mut self) {
        let Some(voice) = self.voices.names().into_iter().next() else { return };
        let Ok(run) = RunOptions::new() else { return };
        for n in [12usize, 40] {
            let tokens: Vec<i64> = (0..n).map(|i| 43 + (i % 20) as i64).collect();
            let _ = self.synth(&tokens, &voice, 1.0, &run);
        }
    }

    /// Synthesize one chunk of tokens (without pad tokens) into 24 kHz mono samples.
    /// `run.terminate()` from another thread aborts the inference with an error.
    pub fn synth(&mut self, tokens: &[i64], voice: &str, speed: f32, run: &RunOptions) -> Result<Vec<f32>> {
        let style = self.voices.style(voice, tokens.len()).ok_or_else(|| anyhow!("unknown voice {voice}"))?;
        let padded: Vec<i64> = [0].into_iter().chain(tokens.iter().copied()).chain([0]).collect();
        let n = padded.len();

        let tokens = Tensor::from_array(([1usize, n], padded))?;
        let style = Tensor::from_array(([1usize, STYLE_DIM], style.to_vec()))?;
        let speed = Tensor::from_array(([1usize], vec![speed]))?;
        let names = &self.names;
        let outputs = self.session.run_with_options(
            ort::inputs![
                names.tokens.as_str() => tokens,
                names.style.as_str() => style,
                names.speed.as_str() => speed,
            ],
            run,
        )?;
        let (_, audio) = outputs[names.audio.as_str()].try_extract_tensor::<f32>()?;
        Ok(audio.to_vec())
    }
}
