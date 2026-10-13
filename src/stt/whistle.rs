//! Cactus Whistle speech recognition (<https://huggingface.co/Cactus-Compute/whistle>, Apache 2.0),
//! run by Cactus's Needle engine. The engine is a small shared library (`libneedle3.dll` /
//! `libneedle3.so`, fetched by `scripts/fetch-needle.*`) that is loaded at run time, like ONNX Runtime.
//!
//! The engine keeps one process-global model and is not thread-safe, so every call is serialised
//! here. Audio is 16 kHz mono `f32`, at most 30 s per call.

use std::ffi::{c_char, c_int, c_uchar, c_ulonglong, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;
use parking_lot::Mutex;
use serde::Deserialize;

pub const REPO: &str = "Cactus-Compute/whistle";
pub const FILE: &str = "whistle.cact";
/// Longest audio (seconds) to give the engine in one call (its limit is 30 s).
pub const MAX_CHUNK_SECS: usize = 29;

/// Spoken languages to choose from (ISO code, name); the first is the default, `auto` detects it.
pub const LANGUAGES: [(&str, &str); 8] = [
    ("en", "English"),
    ("auto", "Detect automatically"),
    ("de", "German"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
];

#[cfg(windows)]
const LIB: &str = "libneedle3.dll";
#[cfg(target_os = "linux")]
const LIB: &str = "libneedle3.so";

/// `needle_models` bit for a loaded speech model.
const SPEECH: c_int = 2;
/// Room for the reply JSON (a 30 s transcript with word times is well below this).
const OUT_CAPACITY: usize = 1 << 16;

type Load = unsafe extern "C" fn(*const c_uchar, c_ulonglong) -> c_int;
type Models = unsafe extern "C" fn() -> c_int;
type LastError = unsafe extern "C" fn() -> *const c_char;
type Transcribe =
    unsafe extern "C" fn(*const f32, c_int, *const c_char, *const c_char, c_int, *mut c_char, c_int) -> c_int;
type StreamProcess = unsafe extern "C" fn(*const f32, c_int, *const c_char, *const c_char, *mut c_char, c_int) -> c_int;
type StreamStop = unsafe extern "C" fn(*mut c_char, c_int) -> c_int;

struct Api {
    load: Load,
    models: Models,
    last_error: LastError,
    transcribe: Transcribe,
    stream_process: StreamProcess,
    stream_stop: StreamStop,
    /// Keeps the library mapped for as long as the function pointers above are used.
    _lib: Library,
}

impl Api {
    fn open() -> Result<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            candidates.push(dir.join(LIB));
        }
        candidates.push(crate::config::data_dir().join(LIB));
        let path = candidates
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow!("{LIB} (the Whistle engine) not found next to Sonorant (see README)"))?;
        log::info!("using the Whistle engine at {}", path.display());
        // SAFETY: loading a library runs its initialisers; this one is the engine we ship. The
        // function types match `needle.h`.
        unsafe {
            let lib = Library::new(&path).with_context(|| format!("load {}", path.display()))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).with_context(|| format!("{LIB} lacks {}", $name))?
                };
            }
            Ok(Self {
                load: sym!("needle_load"),
                models: sym!("needle_models"),
                last_error: sym!("needle_last_error"),
                transcribe: sym!("needle_transcribe"),
                stream_process: sym!("needle_stream_transcribe_process"),
                stream_stop: sym!("needle_stream_transcribe_stop"),
                _lib: lib,
            })
        }
    }

    fn error(&self) -> String {
        // SAFETY: the engine returns a NUL-terminated string that stays valid until its next call.
        let p = unsafe { (self.last_error)() };
        if p.is_null() {
            return "unknown error".into();
        }
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// Check a return code (negative = failure).
    fn check(&self, rc: c_int) -> Result<c_int> {
        if rc < 0 {
            bail!("{}", self.error());
        }
        Ok(rc)
    }
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| Api::open().map_err(|e| format!("{e:#}"))).as_ref().map_err(|e| anyhow!(e.clone()))
}

/// Every engine call takes this: the model and its streaming state are process-global.
static CALLS: Mutex<()> = Mutex::new(());

/// Reply JSON fields that matter here.
#[derive(Deserialize, Default)]
struct Reply {
    #[serde(default)]
    text: String,
    /// Streaming: words not yet confirmed by a second pass.
    #[serde(default)]
    pending: String,
    /// Streaming: seconds of audio received so far.
    #[serde(default)]
    received: f32,
    /// Milliseconds to the first token, and decoder tokens per second after it.
    #[serde(default)]
    ttft_ms: f32,
    #[serde(default)]
    decode_tps: f32,
}

fn parse(out: &[u8]) -> Result<Reply> {
    let end = out.iter().position(|&b| b == 0).unwrap_or(out.len());
    serde_json::from_slice(&out[..end]).context("unreadable reply from the Whistle engine")
}

fn cstring(s: &str) -> Option<CString> {
    (!s.is_empty()).then(|| CString::new(s.replace('\0', "")).expect("NUL removed"))
}

fn ptr(s: &Option<CString>) -> *const c_char {
    s.as_ref().map_or(std::ptr::null(), |c| c.as_ptr())
}

/// Handle to the loaded Whistle model.
#[derive(Clone, Copy)]
pub struct Whistle {
    api: &'static Api,
}

impl Whistle {
    /// Load the engine and the model file (a no-op when the model is already loaded).
    pub fn load(model: &Path) -> Result<Self> {
        let api = api()?;
        let _calls = CALLS.lock();
        // SAFETY: plain calls into the engine, serialised by CALLS.
        if unsafe { (api.models)() } & SPEECH == 0 {
            let bytes = std::fs::read(model).with_context(|| format!("read {}", model.display()))?;
            // The engine may keep pointing into these bytes, and the model stays loaded for the
            // life of the process (17 MB), so never free them.
            let bytes: &'static [u8] = Vec::leak(bytes);
            api.check(unsafe { (api.load)(bytes.as_ptr(), bytes.len() as c_ulonglong) })?;
            if unsafe { (api.models)() } & SPEECH == 0 {
                bail!("{} is not a Whistle speech model", model.display());
            }
        }
        Ok(Self { api })
    }

    /// Transcribe up to 30 s of audio. `language` is an ISO code (None = detect it); `keywords`
    /// bias the recognition towards names and terms.
    pub fn transcribe(&self, pcm: &[f32], language: Option<&str>, keywords: &[String]) -> Result<String> {
        let (language, keywords) = (language.and_then(cstring), cstring(&keywords.join("\n")));
        let mut out = vec![0u8; OUT_CAPACITY];
        let _calls = CALLS.lock();
        let api = self.api;
        // SAFETY: the buffers outlive the call and the lengths passed match them.
        let rc = unsafe {
            (api.transcribe)(
                pcm.as_ptr(),
                pcm.len() as c_int,
                ptr(&language),
                ptr(&keywords),
                0,
                out.as_mut_ptr().cast(),
                OUT_CAPACITY as c_int,
            )
        };
        let tokens = api.check(rc)?;
        let reply = parse(&out)?;
        log::debug!("whistle: {tokens} tokens, first after {:.0} ms, then {:.0} tokens/s", reply.ttft_ms, reply.decode_tps);
        Ok(reply.text.trim().to_owned())
    }

    /// Start a live transcript (the engine keeps one stream at a time).
    pub fn stream(&self, language: Option<&str>, keywords: &[String]) -> Stream {
        Stream {
            api: self.api,
            language: language.and_then(cstring),
            keywords: cstring(&keywords.join("\n")),
            committed: String::new(),
            seconds: 0.0,
            active: false,
        }
    }
}

/// A live transcript: feed it audio about a second at a time. Dropping it ends the stream.
pub struct Stream {
    api: &'static Api,
    language: Option<CString>,
    keywords: Option<CString>,
    /// Words two consecutive passes agreed on.
    committed: String,
    seconds: f32,
    /// Whether the engine holds state for this stream (it needs a stop to start a new one).
    active: bool,
}

impl Stream {
    /// Add audio and return the transcript so far (confirmed words plus the tentative tail).
    pub fn feed(&mut self, pcm: &[f32]) -> Result<String> {
        let mut out = vec![0u8; OUT_CAPACITY];
        let _calls = CALLS.lock();
        let api = self.api;
        self.active = true;
        // SAFETY: as in `Whistle::transcribe`.
        let rc = unsafe {
            (api.stream_process)(
                pcm.as_ptr(),
                pcm.len() as c_int,
                ptr(&self.language),
                ptr(&self.keywords),
                out.as_mut_ptr().cast(),
                OUT_CAPACITY as c_int,
            )
        };
        api.check(rc)?;
        let reply = parse(&out)?;
        self.seconds = reply.received;
        append(&mut self.committed, &reply.text);
        let mut shown = self.committed.clone();
        append(&mut shown, &reply.pending);
        Ok(shown)
    }

    /// Seconds of audio the stream has received.
    pub fn seconds(&self) -> f32 {
        self.seconds
    }

    /// End the stream and return its complete transcript.
    pub fn finish(mut self) -> Result<String> {
        let reply = self.stop()?;
        append(&mut self.committed, &reply.text);
        Ok(std::mem::take(&mut self.committed))
    }

    fn stop(&mut self) -> Result<Reply> {
        let mut out = vec![0u8; OUT_CAPACITY];
        let _calls = CALLS.lock();
        self.active = false;
        // SAFETY: as in `Whistle::transcribe`.
        let rc = unsafe { (self.api.stream_stop)(out.as_mut_ptr().cast(), OUT_CAPACITY as c_int) };
        self.api.check(rc)?;
        parse(&out)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Leave the engine ready for the next stream.
        if self.active {
            let _ = self.stop();
        }
    }
}

/// Append `piece` to `text`, separated by a space.
fn append(text: &mut String, piece: &str) {
    let piece = piece.trim();
    if piece.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(piece);
}
