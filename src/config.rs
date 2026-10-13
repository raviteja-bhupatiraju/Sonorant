//! Persistent user settings and well-known paths.

use std::path::PathBuf;

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
            history_limit: 50,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("Sonorant")
}

pub fn data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("Sonorant")
}

impl Config {
    fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(config_dir())?;
        std::fs::write(Self::path(), serde_json::to_string_pretty(self)?)?;
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
