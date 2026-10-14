//! Dictation history, kept in `history.json` in the data folder (newest first).

use std::sync::{Arc, LazyLock};

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

/// The entries, and a counter bumped on every change (so writes that race can tell which is newer).
/// Shared with the settings window, which redraws often: reading the entries does not copy them.
static STORE: LazyLock<Mutex<(Arc<Vec<Entry>>, u64)>> = LazyLock::new(|| Mutex::new((Arc::new(load()), 0)));

/// The version last written to disk; held while writing, so writes happen one at a time.
static WRITTEN: Mutex<u64> = Mutex::new(0);

fn load() -> Vec<Entry> {
    let path = path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::error!("could not read {}: {e}", path.display());
            }
            return Vec::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        log::error!("{} is not valid ({e}); starting a new history", path.display());
        crate::config::keep_bad_file(&path);
        Vec::new()
    })
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// Serialize the entries (call with the store locked); `save` writes the result after unlocking.
fn snapshot(store: &mut (Arc<Vec<Entry>>, u64)) -> (u64, serde_json::Result<String>) {
    store.1 += 1;
    (store.1, serde_json::to_string_pretty(&*store.0))
}

fn save((version, json): (u64, serde_json::Result<String>)) {
    let mut written = WRITTEN.lock();
    if version <= *written {
        return; // a newer state is already on disk
    }
    let saved = json.map_err(std::io::Error::from).and_then(|s| {
        std::fs::create_dir_all(crate::config::data_dir())?;
        crate::config::write_atomic(&path(), s.as_bytes())
    });
    match saved {
        Ok(()) => *written = version,
        Err(e) => log::error!("could not save the dictation history: {e}"),
    }
}

/// Add a transcript, keeping at most `limit` entries (0 = history disabled, nothing stored).
pub fn add(text: &str, limit: u32) {
    if limit == 0 {
        return;
    }
    let snap = {
        let mut store = STORE.lock();
        let entries = Arc::make_mut(&mut store.0);
        entries.insert(0, Entry { when: now(), text: text.to_owned() });
        entries.truncate(limit as usize);
        snapshot(&mut store)
    };
    save(snap);
}

pub fn entries() -> Arc<Vec<Entry>> {
    STORE.lock().0.clone()
}

pub fn clear() {
    let snap = {
        let mut store = STORE.lock();
        store.0 = Arc::default();
        snapshot(&mut store)
    };
    save(snap);
}

/// Apply a (possibly lowered) limit to what is stored.
pub fn trim_to(limit: u32) {
    let snap = {
        let mut store = STORE.lock();
        if store.0.len() <= limit as usize {
            return;
        }
        Arc::make_mut(&mut store.0).truncate(limit as usize);
        snapshot(&mut store)
    };
    save(snap);
}
