//! Locating and fetching model files. Everything lives in the Hugging Face hub cache
//! (see `hub`), so models are shared with other tools.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;

pub mod hub;

use crate::config::ModelVariant;

/// Kokoro-82M v1.0 ONNX export (Apache-2.0).
pub const KOKORO_REPO: &str = "onnx-community/Kokoro-82M-v1.0-ONNX";

/// The US English voices (`af_*` female, `am_*` male); the only ones Sonorant offers.
pub const US_VOICES: [&str; 20] = [
    "af_alloy", "af_aoede", "af_bella", "af_heart", "af_jessica", "af_kore", "af_nicole", "af_nova", "af_river",
    "af_sarah", "af_sky", "am_adam", "am_echo", "am_eric", "am_fenrir", "am_liam", "am_michael", "am_onyx",
    "am_puck", "am_santa",
];

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub file: String,
    pub done: u64,
    pub total: u64,
}

impl Progress {
    /// Completed share of the download (0 while the size is unknown).
    pub fn fraction(&self) -> f32 {
        if self.total > 0 { self.done as f32 / self.total as f32 } else { 0.0 }
    }
}

fn kokoro_model_file(v: ModelVariant) -> &'static str {
    match v {
        ModelVariant::Fp32 => "onnx/model.onnx",
        ModelVariant::Int8 => "onnx/model_quantized.onnx",
    }
}

fn voice_file(name: &str) -> String {
    format!("voices/{name}.bin")
}

/// True if the Kokoro model and all voices are already in the cache.
pub fn kokoro_ready(v: ModelVariant) -> bool {
    hub::find(KOKORO_REPO, kokoro_model_file(v)).is_some()
        && US_VOICES.iter().all(|n| hub::find(KOKORO_REPO, &voice_file(n)).is_some())
}

pub struct KokoroFiles {
    pub model: PathBuf,
    pub voices: Vec<(String, PathBuf)>,
}

/// Locate the Kokoro files, downloading any that are missing.
pub fn ensure_kokoro(v: ModelVariant, progress: &Arc<Mutex<Progress>>) -> Result<KokoroFiles> {
    let model = hub::ensure(KOKORO_REPO, kokoro_model_file(v), progress)?;
    let voices = US_VOICES
        .iter()
        .map(|&name| Ok((name.to_owned(), hub::ensure(KOKORO_REPO, &voice_file(name), progress)?)))
        .collect::<Result<_>>()?;
    Ok(KokoroFiles { model, voices })
}

/// Models used to live in `%LOCALAPPDATA%\Sonorant\models`; they are now in the HF cache.
/// Remove the old copies (they can always be fetched again).
pub fn remove_legacy_models() {
    let old = crate::config::data_dir().join("models"); // %LOCALAPPDATA%\Sonorant\models
    if old.is_dir() {
        match std::fs::remove_dir_all(&old) {
            Ok(()) => log::info!("removed legacy model folder {}", old.display()),
            Err(e) => log::warn!("could not remove legacy model folder: {e}"),
        }
    }
}
