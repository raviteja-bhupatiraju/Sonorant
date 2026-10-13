//! Dictation history, kept in `history.json` in the data folder (newest first).

use std::sync::LazyLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Local time, `YYYY-MM-DD HH:MM`.
    pub when: String,
    pub text: String,
}

fn path() -> std::path::PathBuf {
    crate::config::data_dir().join("history.json")
}

static STORE: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(|| {
    Mutex::new(std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
});

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

fn save(v: &[Entry]) {
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = std::fs::write(path(), s);
    }
}

/// Add a transcript, keeping at most `limit` entries (0 = history disabled, nothing stored).
pub fn add(text: &str, limit: u32) {
    if limit == 0 {
        return;
    }
    let mut v = STORE.lock();
    v.insert(0, Entry { when: now(), text: text.to_owned() });
    v.truncate(limit as usize);
    save(&v);
}

pub fn entries() -> Vec<Entry> {
    STORE.lock().clone()
}

pub fn clear() {
    let mut v = STORE.lock();
    v.clear();
    save(&v);
}

/// Apply a (possibly lowered) limit to what is stored.
pub fn trim_to(limit: u32) {
    let mut v = STORE.lock();
    if v.len() > limit as usize {
        v.truncate(limit as usize);
        save(&v);
    }
}
