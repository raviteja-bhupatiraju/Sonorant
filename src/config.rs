//! Persistent user settings and well-known paths.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::hotkey::HotkeySpec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelVariant {
    /// 8-bit quantised model: ~92 MB. Smaller download, but ONNX Runtime's CPU
    /// kernels for it are often *slower* than fp32 (3-4x on a Core i9-13900K).
    Int8,
    /// Full precision model: ~326 MB, best quality and fastest on most CPUs.
    Fp32,
}

impl ModelVariant {
    pub const ALL: [ModelVariant; 2] = [ModelVariant::Fp32, ModelVariant::Int8];

    pub fn label(self) -> &'static str {
        match self {
            ModelVariant::Int8 => "Kokoro v1.0 - int8 (92 MB, smaller, slower)",
            ModelVariant::Fp32 => "Kokoro v1.0 - fp32 (326 MB, best quality and speed)",
        }
    }
}

/// Speech recognition model: Cactus Whistle (run by the Needle engine) or GGUF files from Hugging
/// Face run by transcribe.cpp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttModel {
    /// Cactus Whistle: 17 MB, seven languages, runs on the CPU.
    Whistle,
    /// NVIDIA Parakeet Unified EN 0.6B: English, best accuracy.
    ParakeetUnified,
    /// NVIDIA Canary 180M Flash: small and fast; English, German, Spanish, French.
    Canary180M,
}

impl SttModel {
    pub const ALL: [SttModel; 3] = [SttModel::Whistle, SttModel::ParakeetUnified, SttModel::Canary180M];

    pub fn repo(self) -> &'static str {
        match self {
            SttModel::Whistle => crate::stt::whistle::REPO,
            SttModel::ParakeetUnified => "handy-computer/parakeet-unified-en-0.6b-gguf",
            SttModel::Canary180M => "handy-computer/canary-180m-flash-gguf",
        }
    }

    pub fn file(self) -> &'static str {
        match self {
            SttModel::Whistle => crate::stt::whistle::FILE,
            SttModel::ParakeetUnified => "parakeet-unified-en-0.6b-Q8_0.gguf",
            SttModel::Canary180M => "canary-180m-flash-Q8_0.gguf",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SttModel::Whistle => "Cactus Whistle (7 languages, 17 MB, fast on any CPU)",
            SttModel::ParakeetUnified => "NVIDIA Parakeet Unified EN 0.6B (English, 731 MB)",
            SttModel::Canary180M => "NVIDIA Canary 180M Flash (EN/DE/ES/FR, 218 MB)",
        }
    }

    /// Longest audio (seconds) to give the model in one run.
    pub fn max_chunk_secs(self) -> usize {
        match self {
            SttModel::Whistle => crate::stt::whistle::MAX_CHUNK_SECS,
            SttModel::ParakeetUnified => 120,
            SttModel::Canary180M => 30,
        }
    }

    /// Languages to choose from; the first is the default. `auto` lets the model detect it.
    pub fn languages(self) -> &'static [(&'static str, &'static str)] {
        match self {
            SttModel::Whistle => &crate::stt::whistle::LANGUAGES,
            SttModel::ParakeetUnified => &[("en", "English")],
            SttModel::Canary180M => &[("en", "English"), ("de", "German"), ("es", "Spanish"), ("fr", "French")],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttMode {
    /// Hold the hotkey to record, release to transcribe.
    PushToTalk,
    /// Press once to start, press again to stop.
    Toggle,
}

impl SttMode {
    pub const ALL: [SttMode; 2] = [SttMode::PushToTalk, SttMode::Toggle];

    pub fn label(self) -> &'static str {
        match self {
            SttMode::PushToTalk => "Push to talk (hold the hotkey)",
            SttMode::Toggle => "Toggle (press to start, press to stop)",
        }
    }
}

/// How the transcribed text is inserted into the active application.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PasteMethod {
    /// Ctrl+V, or Shift+Insert in terminals.
    Auto,
    CtrlV,
    ShiftInsert,
    CtrlShiftV,
    /// Type the characters directly (works with any keyboard layout, no clipboard).
    Typing,
    /// Only put the text on the clipboard.
    ClipboardOnly,
}

impl PasteMethod {
    pub const ALL: [PasteMethod; 6] = [
        PasteMethod::Auto,
        PasteMethod::CtrlV,
        PasteMethod::ShiftInsert,
        PasteMethod::CtrlShiftV,
        PasteMethod::Typing,
        PasteMethod::ClipboardOnly,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PasteMethod::Auto => crate::platform::auto_paste_label(),
            PasteMethod::CtrlV => "Clipboard + Ctrl+V",
            PasteMethod::ShiftInsert => "Clipboard + Shift+Insert",
            PasteMethod::CtrlShiftV => "Clipboard + Ctrl+Shift+V",
            PasteMethod::Typing => "Type the text directly",
            PasteMethod::ClipboardOnly => "Copy to clipboard only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkey: HotkeySpec,
    pub stop_hotkey: Option<HotkeySpec>,
    pub voice: String,
    pub speed: f32,
    pub volume: f32,
    /// cpal device id; empty = system default output.
    pub output_device: String,
    pub model: ModelVariant,
    /// ONNX Runtime intra-op threads; 0 = automatic.
    pub threads: u32,
    /// Run the models on the GPU when one is available (CPU otherwise).
    pub use_gpu: bool,
    /// Synthesize and play sentence by sentence (low latency).
    pub streaming: bool,
    /// Read the clipboard when nothing is selected.
    pub clipboard_fallback: bool,
    /// Put the previous clipboard content back after grabbing the selection.
    pub restore_clipboard: bool,
    /// Start Sonorant at sign-in (the name predates the Linux version; kept for existing configs).
    pub start_with_windows: bool,

    // ---- Speech recognition (dictation) ----
    pub stt_enabled: bool,
    pub stt_model: SttModel,
    pub stt_hotkey: HotkeySpec,
    /// Ends a recording like `stt_hotkey`, but inserts the text without a trailing space and presses Enter.
    pub stt_submit_hotkey: Option<HotkeySpec>,
    pub stt_mode: SttMode,
    /// Spoken language (ISO code, or `auto`); see `SttModel::languages`.
    pub stt_language: String,
    /// cpal input device id; empty = system default microphone.
    pub input_device: String,
    pub paste_method: PasteMethod,
    /// Restore the previous clipboard after pasting.
    pub paste_keep_clipboard: bool,
    /// Press Enter after inserting the text.
    pub auto_submit: bool,
    pub trailing_space: bool,
    /// Drop silence (Silero VAD) before transcribing.
    pub use_vad: bool,
    pub show_overlay: bool,
    /// Show the recognised text in the indicator while speaking.
    pub live_preview: bool,
    /// Unload the speech model after this many idle minutes (0 = never).
    pub stt_unload_minutes: u32,
    /// Comma-separated custom words / names to favour.
    pub custom_words: String,
    /// Number of transcripts kept in the history (0 disables it).
    pub history_limit: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: HotkeySpec::new(true, false, false, "Backquote"),
            stop_hotkey: Some(HotkeySpec::new(true, false, false, "Digit1")),
            voice: "af_heart".into(),
            speed: 1.0,
            volume: 1.0,
            output_device: String::new(),
            model: ModelVariant::Fp32,
            threads: 0,
            use_gpu: true,
            streaming: true,
            clipboard_fallback: false,
            restore_clipboard: true,
            start_with_windows: false,
            stt_enabled: true,
            stt_model: SttModel::Whistle,
            stt_hotkey: HotkeySpec::new(false, false, false, "F1"),
            stt_submit_hotkey: None,
            stt_mode: SttMode::Toggle,
            stt_language: "en".into(),
            input_device: String::new(),
            paste_method: PasteMethod::Auto,
            paste_keep_clipboard: true,
            auto_submit: false,
            trailing_space: true,
            use_vad: true,
            show_overlay: true,
            live_preview: true,
            stt_unload_minutes: 10,
            custom_words: String::new(),
            history_limit: 3,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("Sonorant")
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("Sonorant")
}

/// Replace `path` with `data` so that a crash or full disk never leaves a half-written file: write
/// a temporary file next to it, flush it to disk, then rename it over the original.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Keep an unreadable settings file as `<name>.bad` for inspection before it gets overwritten.
pub fn keep_bad_file(path: &Path) {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    match std::fs::rename(path, &bad) {
        Ok(()) => log::warn!("kept the unreadable file as {}", Path::new(&bad).display()),
        Err(e) => log::warn!("could not back up {}: {e}", path.display()),
    }
}

impl Config {
    fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::error!("could not read {}: {e}; using the default settings", path.display());
                }
                return Self::default();
            }
        };
        Self::parse(&text).unwrap_or_else(|e| {
            log::error!("{} is not valid ({e}); using the default settings", path.display());
            keep_bad_file(&path);
            Self::default()
        })
    }

    /// Parse a stored configuration. Missing settings and settings this version does not understand
    /// (an unknown model name, a value of the wrong type, ...) take their defaults; only a file that
    /// is not a JSON object at all is an error.
    fn parse(text: &str) -> serde_json::Result<Self> {
        let stored: serde_json::Map<String, serde_json::Value> = serde_json::from_str(text)?;
        let serde_json::Value::Object(mut merged) = serde_json::to_value(Self::default())? else {
            unreachable!("Config serializes to an object")
        };
        // Take over the stored settings one by one, skipping any that would make the whole fail.
        for (key, value) in stored {
            let previous = merged.insert(key.clone(), value);
            if let Err(e) = Self::deserialize(&serde_json::Value::Object(merged.clone())) {
                log::warn!("config: ignoring setting {key:?} ({e})");
                match previous {
                    Some(v) => merged.insert(key, v),
                    None => merged.remove(&key),
                };
            }
        }
        Self::deserialize(serde_json::Value::Object(merged)).map(Self::sanitized)
    }

    /// Bring hand-edited values into the ranges the settings window offers: the models and the
    /// audio output take them as they are.
    pub(crate) fn sanitized(mut self) -> Self {
        let default = Self::default();
        let finite = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        self.speed = finite(self.speed, default.speed).clamp(0.5, 2.0);
        self.volume = finite(self.volume, default.volume).clamp(0.0, 1.5);
        self.threads = self.threads.min(64);
        let languages = self.stt_model.languages();
        if !languages.iter().any(|(code, _)| *code == self.stt_language) {
            self.stt_language = languages[0].0.to_owned();
        }
        self
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(config_dir())?;
        write_atomic(&Self::path(), serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }

    /// Inference threads to use: the configured count, or up to 8 when automatic.
    pub fn thread_count(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8),
            n => n as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_or_invalid_settings_fall_back_to_defaults() {
        let c = Config::parse(
            r#"{"voice":"am_adam","stt_model":"SomeFutureModel","speed":"fast","paste_method":"Typing",
                "hotkey":{"ctrl":true,"key":"KeyR"},"unknown_setting":1}"#,
        )
        .expect("parse");
        let d = Config::default();
        assert_eq!(c.voice, "am_adam");
        assert_eq!(c.paste_method, PasteMethod::Typing);
        assert_eq!(c.stt_model, d.stt_model);
        assert_eq!(c.speed, d.speed);
        assert_eq!(c.hotkey, HotkeySpec::new(true, false, false, "KeyR"));
        assert_eq!(c.history_limit, d.history_limit);
    }

    #[test]
    fn out_of_range_settings_are_brought_back() {
        let c = Config::parse(r#"{"speed":0,"volume":99,"threads":100000,"stt_model":"Canary180M","stt_language":"auto"}"#).expect("parse");
        assert_eq!((c.speed, c.volume, c.threads), (0.5, 1.5, 64));
        assert_eq!(c.stt_language, "en");
    }

    #[test]
    fn non_object_is_an_error() {
        assert!(Config::parse("not json").is_err());
        assert!(Config::parse("[1, 2]").is_err());
        assert_eq!(Config::parse("{}").expect("empty"), Config::default());
    }

    #[test]
    fn write_atomic_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("sonorant-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("config.json");
        write_atomic(&f, b"one").unwrap();
        write_atomic(&f, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
