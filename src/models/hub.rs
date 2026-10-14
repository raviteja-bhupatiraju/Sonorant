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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure as check, Context, Result};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport};

use crate::models::{pins, Progress};

/// A download that receives nothing for this long fails (instead of blocking its worker forever).
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Set at shutdown: downloads in progress stop at the next read.
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Abort all downloads in progress (and any started later); used when the application exits.
pub fn cancel_downloads() {
    CANCELLED.store(true, Ordering::SeqCst);
}

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
    // A pinned file counts only at its pinned revision: whatever another tool cached at another
    // revision is not what Sonorant was tested with (and is not verified).
    if let Some(&(_, _, rev, ..)) = pins::PINS.iter().find(|p| p.0 == repo && p.1 == file) {
        return Some(dir.join("snapshots").join(rev).join(file)).filter(|p| p.is_file());
    }
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

/// Remove a cached file that turned out to be unusable (e.g. a model that fails to load), so the
/// next `ensure` downloads it again: the snapshot entry and, if it is a symlink, the blob behind it.
/// A hard-linked or copied entry leaves its blob behind, but `ensure` verifies a blob before reusing it.
pub fn evict(path: &Path) {
    if let Ok(target) = std::fs::read_link(path) {
        let blob = path.parent().map_or(target.clone(), |dir| dir.join(&target));
        // Only ever delete inside a `blobs` folder of the cache.
        if blob.parent().is_some_and(|p| p.ends_with("blobs")) {
            if let Err(e) = std::fs::remove_file(&blob) {
                log::warn!("could not remove {}: {e}", blob.display());
            }
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => log::info!("removed bad cached file {}", path.display()),
        Err(e) => log::warn!("could not remove {}: {e}", path.display()),
    }
}

/// Like `evict`, but only when the file is really damaged: one whose SHA-256 is that of a pinned
/// file was downloaded intact, so a failure to load it is the environment's fault (a runtime
/// version, memory, ...) and deleting the (large, shared) file would only cost another download.
pub fn evict_if_damaged(path: &Path) {
    let Ok(mut file) = std::fs::File::open(path) else { return };
    let mut hasher = Sha256::new();
    if std::io::copy(&mut file, &mut hasher).is_err() {
        return; // unreadable right now: not evidence of damage
    }
    let sha = hex(&hasher.finalize());
    if pins::PINS.iter().any(|p| p.4.eq_ignore_ascii_case(&sha)) {
        log::info!("{} is intact; keeping it", path.display());
    } else {
        evict(path);
    }
}

/// Caps every socket wait at `IDLE_TIMEOUT`. ureq 3 only has budgets for whole phases (such as the
/// entire response body), which a large download on a slow line could legitimately exceed.
#[derive(Debug)]
struct IdleTimeout;

impl Connector<Box<dyn Transport>> for IdleTimeout {
    type Out = IdleTransport;

    fn connect(&self, _: &ConnectionDetails, chained: Option<Box<dyn Transport>>) -> Result<Option<IdleTransport>, ureq::Error> {
        Ok(chained.map(IdleTransport))
    }
}

#[derive(Debug)]
struct IdleTransport(Box<dyn Transport>);

impl Transport for IdleTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.0.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, mut timeout: NextTimeout) -> Result<bool, ureq::Error> {
        if *timeout.after > IDLE_TIMEOUT {
            timeout.after = ureq::unversioned::transport::time::Duration::Exact(IDLE_TIMEOUT);
        }
        self.0.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }

    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

fn agent() -> ureq::Agent {
    // Windows: the system TLS stack (SChannel) instead of bundling a TLS library.
    #[cfg(windows)]
    let provider = ureq::tls::TlsProvider::NativeTls;
    // Linux: rustls with built-in root certificates, so packages do not depend on the OpenSSL version.
    #[cfg(not(windows))]
    let provider = ureq::tls::TlsProvider::Rustls;
    let config = ureq::Agent::config_builder()
        .tls_config(ureq::tls::TlsConfig::builder().provider(provider).build())
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build();
    let connector = DefaultConnector::default().chain(IdleTimeout);
    ureq::Agent::with_parts(config, connector, ureq::unversioned::resolver::DefaultResolver::default())
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<serde_json::Value> {
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let text = resp.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&text)?)
}

/// Commit hashes and blob ids end up as path components: accept nothing but hex digits.
fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What a blob must look like: its size and, for LFS files, the SHA-256 of its contents.
struct Expected {
    size: Option<u64>,
    sha256: Option<String>,
}

impl Expected {
    /// Check an existing file (a cached blob about to be reused).
    fn matches(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else { return false };
        if self.size.is_some_and(|s| s != meta.len()) {
            return false;
        }
        let Some(want) = &self.sha256 else { return true };
        let hash = || -> std::io::Result<String> {
            let mut hasher = Sha256::new();
            std::io::copy(&mut std::fs::File::open(path)?, &mut hasher)?;
            Ok(hex(&hasher.finalize()))
        };
        hash().is_ok_and(|h| h.eq_ignore_ascii_case(want))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The commit, blob id and expected content of `file` in `repo`: from the built-in pins when the
/// file has one, else from the latest commit on the Hub (which only protects against a damaged
/// download, since the hash comes from the same server).
fn resolve(agent: &ureq::Agent, repo: &str, file: &str) -> Result<(String, String, Expected)> {
    if let Some(&(_, _, rev, size, sha256)) = pins::PINS.iter().find(|p| p.0 == repo && p.1 == file) {
        return Ok((rev.to_owned(), sha256.to_owned(), Expected { size: Some(size), sha256: Some(sha256.to_owned()) }));
    }
    log::warn!("{repo}/{file} is not pinned: taking the latest revision from the Hub");
    // Latest commit and the blob id (LFS sha256 or git oid) of this file, all from that one commit.
    let info = get_json(agent, &format!("https://huggingface.co/api/models/{repo}"))?;
    let rev = info["sha"].as_str().filter(|s| is_hex(s)).with_context(|| format!("no commit sha for {repo}"))?.to_owned();
    let parent = file.rsplit_once('/').map_or("", |(parent, _)| parent);
    let mut tree_url = format!("https://huggingface.co/api/models/{repo}/tree/{rev}");
    if !parent.is_empty() {
        tree_url += &format!("/{parent}");
    }
    let tree = get_json(agent, &tree_url)?;
    let entry = tree
        .as_array()
        .and_then(|a| a.iter().find(|e| e["path"].as_str() == Some(file)))
        .with_context(|| format!("{file} not found in {repo}"))?;
    let lfs = &entry["lfs"];
    let oid = lfs["oid"].as_str().or_else(|| entry["oid"].as_str()).unwrap_or_default().to_owned();
    check!(is_hex(&oid), "{file}: unexpected blob id {oid:?}");
    let expected = Expected {
        size: lfs["size"].as_u64().or_else(|| entry["size"].as_u64()),
        sha256: lfs["oid"].as_str().map(str::to_owned),
    };
    Ok((rev, oid, expected))
}

/// Return the cached path, downloading `file` from `repo` into the HF cache first if needed.
pub fn ensure(repo: &str, file: &str, progress: &Arc<Mutex<Progress>>) -> Result<PathBuf> {
    if let Some(p) = find(repo, file) {
        return Ok(p);
    }
    let agent = agent();
    let dir = repo_dir(repo);
    let (rev, oid, expected) = resolve(&agent, repo, file)?;
    check!(is_hex(&rev) && is_hex(&oid), "{file}: unexpected revision or blob id");
    let name = file.rsplit('/').next().unwrap_or(file);

    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs)?;
    let blob = blobs.join(&oid);
    // The blob may already be there (another snapshot, another tool): link it instead of downloading.
    if !(blob.is_file() && expected.matches(&blob)) {
        let url = format!("https://huggingface.co/{repo}/resolve/{rev}/{file}");
        // The name the Hugging Face tools use too: an interrupted download is continued next time.
        let part = blobs.join(format!("{oid}.incomplete"));
        let fetched = download(&agent, &url, &part, name, &expected, progress);
        *progress.lock() = Progress::default();
        fetched?;
        std::fs::rename(&part, &blob)?;
    }

    let link = dir.join("snapshots").join(&rev).join(file);
    link_file(&blob, &link)?;
    std::fs::create_dir_all(dir.join("refs"))?;
    std::fs::write(dir.join("refs").join("main"), &rev)?;
    Ok(link)
}

/// Download `url` into `part` (continuing what is already there), checking it against `expected`.
/// A failed transfer leaves `part` for the next attempt; one that arrived wrong is deleted.
fn download(agent: &ureq::Agent, url: &str, part: &Path, name: &str, expected: &Expected, progress: &Arc<Mutex<Progress>>) -> Result<()> {
    let mut existing = std::fs::metadata(part).map_or(0, |m| m.len());
    if expected.size.is_some_and(|size| existing >= size) {
        let _ = std::fs::remove_file(part); // nothing sensible to continue
        existing = 0;
    }
    log::info!("downloading {url}{}", if existing > 0 { format!(" (continuing at {existing} bytes)") } else { String::new() });
    let mut request = agent.get(url);
    if existing > 0 {
        request = request.header("Range", format!("bytes={existing}-"));
    }
    let resp = request.call().with_context(|| format!("GET {url}"))?;

    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut out = if existing > 0 && resp.status().as_u16() == 206 {
        // The server continues where the file ends: hash what is already there first.
        let mut old = std::fs::File::open(part)?;
        done = std::io::copy(&mut old, &mut hasher)?;
        std::fs::OpenOptions::new().append(true).open(part)?
    } else {
        std::fs::File::create(part)? // not resumed: start over
    };
    let total = expected.size.or_else(|| resp.body().content_length().map(|n| n + done)).unwrap_or(0);
    *progress.lock() = Progress { file: name.to_owned(), done, total };

    let mut reader = resp.into_body().into_reader();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if CANCELLED.load(Ordering::SeqCst) {
            bail!("{name}: download cancelled");
        }
        let n = reader.read(&mut buf).with_context(|| format!("downloading {name}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress.lock().done = done;
    }
    out.sync_all()?;
    drop(out);
    let wrong = |what: String| {
        let _ = std::fs::remove_file(part);
        anyhow::anyhow!(what)
    };
    if let Some(size) = expected.size.or((total != 0).then_some(total)) {
        if done < size {
            bail!("{name}: incomplete download ({done} of {size} bytes)"); // kept: continued next time
        }
        if done > size {
            return Err(wrong(format!("{name}: too much data ({done} of {size} bytes)")));
        }
    }
    if let Some(want) = &expected.sha256 {
        let got = hex(&hasher.finalize());
        if !got.eq_ignore_ascii_case(want) {
            return Err(wrong(format!("{name}: checksum mismatch (expected {want}, got {got})")));
        }
    }
    Ok(())
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
    // Copy under a temporary name, so a half-written copy is never taken for the real file.
    let mut tmp = link.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let copied = std::fs::copy(blob, &tmp).and_then(|_| std::fs::rename(&tmp, link));
    if copied.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    copied?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hex_ids_are_path_safe() {
        assert!(is_hex("0123abcdEF"));
        assert!(!is_hex(""));
        assert!(!is_hex("../etc"));
        assert!(!is_hex("abc/def"));
    }

    #[test]
    fn expected_checks_size_and_sha256() {
        let dir = std::env::temp_dir().join(format!("sonorant-hub-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("blob");
        std::fs::write(&f, b"abc").unwrap();
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(Expected { size: Some(3), sha256: Some(sha.into()) }.matches(&f));
        assert!(Expected { size: Some(3), sha256: None }.matches(&f));
        assert!(!Expected { size: Some(4), sha256: None }.matches(&f));
        assert!(!Expected { size: None, sha256: Some("00".repeat(32)) }.matches(&f));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Needs the network: `cargo test -- --ignored pinned_download`.
    #[test]
    #[ignore]
    fn pinned_download_is_verified_and_resumed() {
        let dir = std::env::temp_dir().join(format!("sonorant-hub-net-{}", std::process::id()));
        std::env::set_var("HF_HUB_CACHE", &dir);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let (repo, file) = ("onnx-community/Kokoro-82M-v1.0-ONNX", "voices/af_heart.bin");
        let path = ensure(repo, file, &progress).expect("download");
        let whole = std::fs::read(&path).unwrap();
        assert_eq!(whole.len(), 522_240);

        // An interrupted download: half of the file is left under the `.incomplete` name.
        let (rev, oid, _) = resolve(&agent(), repo, file).unwrap();
        let blob = dir.join("models--onnx-community--Kokoro-82M-v1.0-ONNX").join("blobs").join(&oid);
        std::fs::remove_file(&blob).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(blob.with_file_name(format!("{oid}.incomplete")), &whole[..200_000]).unwrap();
        let again = ensure(repo, file, &progress).expect("resume");
        assert_eq!(std::fs::read(&again).unwrap(), whole);
        assert!(again.to_string_lossy().contains(&rev));

        // A part that is not the start of the file is caught by the checksum and removed.
        std::fs::remove_file(&blob).unwrap();
        std::fs::remove_file(&again).unwrap();
        std::fs::write(blob.with_file_name(format!("{oid}.incomplete")), vec![7u8; 100_000]).unwrap();
        assert!(ensure(repo, file, &progress).is_err());
        assert!(!blob.with_file_name(format!("{oid}.incomplete")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
