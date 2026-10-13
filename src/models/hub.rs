//! Model files, fetched on first use into the standard Hugging Face cache, shared with other tools
//! such as Handy and the Python libraries:
//!
//! ```text
//! <cache>/models--<org>--<name>/refs/main                 (commit hash)
//! <cache>/models--<org>--<name>/blobs/<oid>               (file contents)
//! <cache>/models--<org>--<name>/snapshots/<commit>/<file> (link to the blob)
//! ```
//!
//! The cache root is `HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;

use crate::models::Progress;

pub fn cache_root() -> PathBuf {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(p) = env("HF_HUB_CACHE").or_else(|| env("HUGGINGFACE_HUB_CACHE")) {
        return p;
    }
    if let Some(p) = env("HF_HOME") {
        return p.join("hub");
    }
    if let Some(p) = env("XDG_CACHE_HOME") {
        return p.join("huggingface").join("hub");
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".cache").join("huggingface").join("hub")
}

fn repo_dir(repo: &str) -> PathBuf {
    cache_root().join(format!("models--{}", repo.replace('/', "--")))
}

/// Path of `file` from `repo` if it is already in the cache (any snapshot).
pub fn find(repo: &str, file: &str) -> Option<PathBuf> {
    let dir = repo_dir(repo);
    // Prefer the snapshot `refs/main` points at, then any other snapshot.
    if let Ok(rev) = std::fs::read_to_string(dir.join("refs").join("main")) {
        let p = dir.join("snapshots").join(rev.trim()).join(file);
        if p.is_file() {
            return Some(p);
        }
    }
    std::fs::read_dir(dir.join("snapshots"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().join(file))
        .find(|p| p.is_file())
}

fn agent() -> ureq::Agent {
    // Windows: the system TLS stack (SChannel) instead of bundling a TLS library.
    #[cfg(windows)]
    let provider = ureq::tls::TlsProvider::NativeTls;
    // Linux: rustls with built-in root certificates, so packages do not depend on the OpenSSL version.
    #[cfg(not(windows))]
    let provider = ureq::tls::TlsProvider::Rustls;
    ureq::Agent::config_builder().tls_config(ureq::tls::TlsConfig::builder().provider(provider).build()).build().into()
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<serde_json::Value> {
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let text = resp.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&text)?)
}

/// Return the cached path, downloading `file` from `repo` into the HF cache first if needed.
pub fn ensure(repo: &str, file: &str, progress: &Arc<Mutex<Progress>>) -> Result<PathBuf> {
    if let Some(p) = find(repo, file) {
        return Ok(p);
    }
    let agent = agent();
    let dir = repo_dir(repo);

    // Latest commit and the blob id (LFS sha256 or git oid) of this file.
    let info = get_json(&agent, &format!("https://huggingface.co/api/models/{repo}"))?;
    let latest = info["sha"].as_str().with_context(|| format!("no commit sha for {repo}"))?.to_owned();
    let (parent, name) = file.rsplit_once('/').unwrap_or(("", file));
    let mut tree_url = format!("https://huggingface.co/api/models/{repo}/tree/{latest}");
    if !parent.is_empty() {
        tree_url += &format!("/{parent}");
    }
    let tree = get_json(&agent, &tree_url)?;
    let entry = tree
        .as_array()
        .and_then(|a| a.iter().find(|e| e["path"].as_str() == Some(file)))
        .with_context(|| format!("{file} not found in {repo}"))?;
    let oid = entry["lfs"]["oid"].as_str().or_else(|| entry["oid"].as_str()).unwrap_or(name).to_owned();

    // Keep an existing `refs/main` so other tools still see their snapshot; add our file to it.
    let refs_main = dir.join("refs").join("main");
    let rev = std::fs::read_to_string(&refs_main)
        .ok()
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| latest.clone());

    let url = format!("https://huggingface.co/{repo}/resolve/{latest}/{file}");
    log::info!("downloading {url}");
    let resp = agent.get(&url).call().with_context(|| format!("GET {url}"))?;
    let total = resp.body().content_length().unwrap_or(0);
    *progress.lock() = Progress { file: name.to_owned(), done: 0, total };

    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs)?;
    let blob = blobs.join(&oid);
    let part = blobs.join(format!("{oid}.incomplete"));
    {
        let mut out = std::fs::File::create(&part)?;
        let mut reader = resp.into_body().into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        while let n @ 1.. = reader.read(&mut buf)? {
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress.lock().done = done;
        }
        out.flush()?;
        if total != 0 && done != total {
            bail!("{file}: incomplete download ({done} of {total} bytes)");
        }
    }
    std::fs::rename(&part, &blob)?;

    let link = dir.join("snapshots").join(&rev).join(file);
    link_file(&blob, &link)?;
    if !refs_main.exists() {
        std::fs::create_dir_all(dir.join("refs"))?;
        std::fs::write(&refs_main, &rev)?;
    }
    *progress.lock() = Progress::default();
    Ok(link)
}

/// Make `link` refer to `blob`: symlink if permitted, else hard link, else copy.
fn link_file(blob: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(link);
    // Relative symlink target, as huggingface_hub writes it: from `snapshots/<rev>/<file>` one `..` per
    // component of `<rev>/<file>` leads back to the repo folder (`../../blobs/<oid>` for a top-level file).
    let depth = link
        .ancestors()
        .find(|a| a.ends_with("snapshots"))
        .and_then(|snapshots| link.strip_prefix(snapshots).ok())
        .map_or(2, |p| p.components().count());
    let target = std::iter::repeat_n("..", depth).collect::<PathBuf>().join("blobs").join(blob.file_name().unwrap());
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&target, link);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, link);
    if linked.is_ok() && link.is_file() {
        return Ok(());
    }
    let _ = std::fs::remove_file(link);
    if std::fs::hard_link(blob, link).is_ok() {
        return Ok(());
    }
    std::fs::copy(blob, link)?;
    Ok(())
}
