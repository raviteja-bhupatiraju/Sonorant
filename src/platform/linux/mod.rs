//! Linux, on Wayland and on X11. The session decides (`use_x11`):
//!
//! * Wayland: hotkeys and key injection through XDG desktop portals, a wlr-layer-shell indicator
//!   (with desktop notifications on compositors without layer shell, such as GNOME);
//! * X11: key grabs on the root window, key injection through XTest, and an override-redirect
//!   indicator window.
//!
//! Both read "speak selection" from the primary selection and use a StatusNotifierItem tray icon.

mod hotkeys;
mod hotkeys_x11;
mod input;
mod overlay;
mod overlay_x11;
mod tray;
mod x11;

use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;

pub use input::{clipboard_text, copy_text, insert, prepare_insert, selected_text, send_enter};
pub use overlay::Overlay;
pub use tray::Tray;

use super::{Command, CommandSink};

/// The primary selection keeps the last selected text even after it is deselected, so a second
/// hotkey press on the same text cannot mean "nothing selected": it stops speech instead.
pub const SELECTION_PERSISTS: bool = true;

/// Whether the selection cannot be read right now (never on Linux: no keystroke is simulated).
pub fn selection_blocked() -> bool {
    false
}

/// Paste shortcut used by `PasteMethod::Auto`. Wayland does not reveal the focused application,
/// so terminals (Ctrl+Shift+V) can only be detected on X11.
pub fn auto_paste_label() -> &'static str {
    if use_x11() {
        "Automatic (Ctrl+V, Ctrl+Shift+V in terminals)"
    } else {
        "Automatic (Ctrl+V)"
    }
}

/// Whether this session is X11 (no Wayland display, but an X server) rather than Wayland.
/// `SONORANT_BACKEND=x11` or `wayland` overrides the detection.
pub fn use_x11() -> bool {
    static X11: LazyLock<bool> = LazyLock::new(|| {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        match std::env::var("SONORANT_BACKEND").as_deref() {
            Ok("x11") => true,
            Ok("wayland") => false,
            _ => !set("WAYLAND_DISPLAY") && set("DISPLAY"),
        }
    });
    *X11
}

/// Whether a plain-Esc stop key is claimed only while Sonorant is busy: possible with X11 key grabs,
/// not with the Wayland portal (which does not take Esc as a global shortcut at all).
pub fn esc_on_demand() -> bool {
    use_x11()
}

/// A note shown under the hotkey settings.
pub fn hotkey_note() -> Option<&'static str> {
    Some(if use_x11() {
        "These keys are grabbed system-wide. If one is already used by another program, Sonorant says so here;          `sonorant --cmd speak` (or stop, dictate) can also be bound in the desktop's keyboard settings."
    } else {
        "On Wayland these are suggestions: the desktop asks you to confirm them. Without a global          shortcuts portal, bind `sonorant --cmd speak` (or stop, dictate) to keys in the desktop's settings."
    })
}

/// The global hotkeys of the session: the GlobalShortcuts portal on Wayland, key grabs on X11.
pub enum Hotkeys {
    Portal(hotkeys::PortalHotkeys),
    X11(hotkeys_x11::X11Hotkeys),
}

impl Hotkeys {
    pub fn new(sink: CommandSink) -> Result<Self> {
        if use_x11() {
            install_desktop_entry(); // names the tray and menu icon
            Ok(Self::X11(hotkeys_x11::X11Hotkeys::new(sink)?))
        } else {
            Ok(Self::Portal(hotkeys::PortalHotkeys::new(sink)?))
        }
    }

    /// (Re)bind the speak, stop and dictate hotkeys from `cfg`.
    pub fn register(&mut self, cfg: &crate::config::Config) {
        match self {
            Self::Portal(h) => h.register(cfg),
            Self::X11(h) => h.register(cfg),
        }
    }

    pub fn errors(&self) -> super::HotkeyErrors {
        match self {
            Self::Portal(h) => h.errors(),
            Self::X11(h) => h.errors(),
        }
    }

    /// Claim Esc to cancel while a dictation is in progress (X11 only; a portal cannot).
    pub fn set_dictating(&mut self, on: bool, dictate: &crate::hotkey::HotkeySpec) {
        match self {
            Self::Portal(h) => h.set_dictating(on, dictate),
            Self::X11(h) => h.set_dictating(on, dictate),
        }
    }

    /// Claim / release a plain-Esc stop key while busy (X11 only). Returns whether the caller must
    /// keep calling this while busy.
    pub fn set_busy(&mut self, busy: bool) -> bool {
        match self {
            Self::Portal(h) => h.set_busy(busy),
            Self::X11(h) => h.set_busy(busy),
        }
    }
}

/// Application id: names the `.desktop` file through which portals (shortcut and remote-control
/// permission dialogs) identify Sonorant.
const APP_ID: &str = "org.sonorant.Sonorant";

/// The async runtime for D-Bus (portals, tray, notifications).
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sonorant-dbus")
            .enable_all()
            .build()
            .expect("start the async runtime")
    });
    &RT
}

/// Run `fut` on the runtime, giving up after `limit` (portals can hang when misconfigured).
pub(crate) fn block_on<T>(limit: Duration, fut: impl Future<Output = T>) -> Option<T> {
    runtime().block_on(async { tokio::time::timeout(limit, fut).await.ok() })
}

/// Terminals already have a console.
pub fn attach_console() {}

fn runtime_path(name: &str) -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join(name)
}

fn socket_path() -> PathBuf {
    runtime_path("sonorant.sock")
}

/// Single instance through a lock file and a Unix socket in `$XDG_RUNTIME_DIR`; the socket also
/// carries `sonorant --cmd` commands to the running instance.
pub struct Instance {
    listener: Option<UnixListener>,
    /// Holds the instance lock (released by the kernel when the process ends, even after a crash).
    _lock: Option<std::fs::File>,
}

impl Instance {
    /// Claim the single instance; `Err` if Sonorant is already running, after passing `forward` to it.
    pub fn claim(forward: Option<Command>) -> Result<Self, ()> {
        let path = socket_path();
        // The lock decides who runs, so two instances starting together cannot both bind the socket
        // (or remove the other one's fresh socket as a leftover).
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(runtime_path("sonorant.lock"))
            .inspect_err(|e| log::warn!("instance lock unavailable: {e}"))
            .ok();
        let owner = match &lock {
            Some(file) => match file.try_lock() {
                Ok(()) => true,
                Err(std::fs::TryLockError::WouldBlock) => false,
                Err(std::fs::TryLockError::Error(e)) => {
                    log::warn!("instance lock unavailable: {e}");
                    UnixStream::connect(&path).is_err()
                }
            },
            None => UnixStream::connect(&path).is_err(),
        };
        if !owner {
            // The running instance may still be starting up: give it a moment to open its socket.
            for _ in 0..20 {
                if let Ok(mut running) = UnixStream::connect(&path) {
                    if let Some(cmd) = forward {
                        let _ = writeln!(running, "{}", cmd.name());
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            return Err(());
        }
        let _ = std::fs::remove_file(&path); // left over from a crash
        let listener = UnixListener::bind(&path)
            .inspect_err(|e| log::warn!("command socket {} unavailable: {e}", path.display()))
            .ok();
        // Register with the portal frontend before anything uses a portal (every portal user awaits
        // `register_app`, which waits for this attempt), without holding up the start.
        runtime().spawn(register_app());
        Ok(Self { listener, _lock: lock })
    }

    /// Deliver commands from other processes (`sonorant --cmd NAME`) to `sink`.
    pub fn listen(&self, sink: CommandSink) {
        let Some(listener) = self.listener.as_ref().and_then(|l| l.try_clone().ok()) else { return };
        std::thread::Builder::new()
            .name("commands".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // One thread per client: one that never closes must not block the others.
                    let sink = sink.clone();
                    std::thread::spawn(move || {
                        for line in BufReader::new(stream).lines().map_while(Result::ok) {
                            match Command::parse(line.trim()) {
                                Some(cmd) => sink(cmd),
                                None => log::warn!("unknown command {line:?}"),
                            }
                        }
                    });
                }
            })
            .expect("spawn command thread");
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        if self.listener.is_some() {
            let _ = std::fs::remove_file(socket_path());
        }
    }
}

/// What starts Sonorant: the AppImage file when running from one (the executable itself then lives in
/// a temporary mount), else the executable.
fn launcher() -> Result<PathBuf> {
    match std::env::var_os("APPIMAGE") {
        Some(appimage) => Ok(appimage.into()),
        None => Ok(std::env::current_exe()?),
    }
}

/// Marks `.desktop` entries Sonorant wrote itself, so they can be replaced or removed safely.
const GENERATED: &str = "X-Sonorant-Generated=true";

/// A `.desktop` entry that starts Sonorant with `args`; `hidden` keeps it out of application menus.
fn desktop_entry(args: &str, hidden: bool) -> Result<String> {
    let exec = format!("{} {args}", exec_arg(&launcher()?.to_string_lossy()));
    Ok(format!(
        "[Desktop Entry]
Type=Application
Name=Sonorant
Comment=Read selected text aloud and dictate with hotkeys
Exec={}
Icon={APP_ID}
Terminal=false
Categories=Utility;Accessibility;
NoDisplay={hidden}
{GENERATED}
",
        exec.trim_end()
    ))
}

/// `arg` as a quoted argument of a `.desktop` entry's `Exec` key (Desktop Entry spec): `"`, `` ` ``,
/// `$` and `\` are backslash-escaped inside the quotes and `%` (field codes) is doubled; then the
/// value's own string escaping doubles every backslash once more.
fn exec_arg(arg: &str) -> String {
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

/// Make sure a `<APP_ID>.desktop` entry exists, which portals need to identify Sonorant (and which
/// names its icon). A package (deb / rpm) installs one system-wide; otherwise Sonorant writes its own
/// to `~/.local/share/applications`: listed in menus for an AppImage, hidden for a development build.
fn install_desktop_entry() {
    let Some(data) = dirs::data_dir() else { return };
    let name = format!("{APP_ID}.desktop");
    let local = data.join("applications").join(&name);
    let ours = std::fs::read_to_string(&local).map(|s| s.contains(GENERATED));
    let system = std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    if std::env::split_paths(&system).any(|d| d.join("applications").join(&name).is_file()) {
        // Installed by a package: an entry of ours would override (and hide) it.
        if ours.is_ok_and(|ours| ours) {
            let _ = std::fs::remove_file(&local);
        }
        return;
    }
    if ours.is_ok_and(|ours| !ours) {
        return; // the user's own entry
    }
    let Ok(entry) = desktop_entry("--settings", std::env::var_os("APPIMAGE").is_none()) else { return };
    if std::fs::read_to_string(&local).ok().as_deref() != Some(entry.as_str()) {
        let _ = std::fs::create_dir_all(data.join("applications")).and_then(|()| std::fs::write(&local, entry));
    }
    // The icon `build.rs` puts next to the binary.
    let icon = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("sonorant.png"))).filter(|p| p.is_file());
    if let Some(icon) = icon {
        let dest = data.join(format!("icons/hicolor/256x256/apps/{APP_ID}.png"));
        if std::fs::metadata(&dest).map(|m| m.len()).ok() != std::fs::metadata(&icon).map(|m| m.len()).ok() {
            let _ = std::fs::create_dir_all(dest.parent().expect("has a parent")).and_then(|()| std::fs::copy(&icon, &dest));
        }
    }
}

/// Install the desktop entry and register Sonorant's id with the portal frontend. Must be awaited
/// before any portal call (the registration has to come first on the connection). Runs once; a
/// registration that timed out is tried again by the next caller.
pub(crate) async fn register_app() {
    static DONE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    let _ = DONE
        .get_or_try_init(|| async {
            install_desktop_entry();
            let registered = async { ashpd::register_host_app(APP_ID.parse()?).await };
            match tokio::time::timeout(Duration::from_secs(2), registered).await {
                Ok(Ok(())) => Ok(()),
                // An answer, if a negative one: older portals have no host registry.
                Ok(Err(e)) => {
                    log::debug!("portal app registration: {e}");
                    Ok(())
                }
                Err(_) => {
                    log::debug!("portal app registration timed out");
                    Err(())
                }
            }
        })
        .await;
}

pub fn set_autostart(enable: bool) -> Result<()> {
    let path = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config folder"))?.join("autostart/sonorant.desktop");
    if enable {
        std::fs::create_dir_all(path.parent().expect("has a parent"))?;
        std::fs::write(&path, desktop_entry("", false)?)?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// The desktop's light / dark preference (Settings portal); dark when unknown.
pub fn prefers_light_theme() -> bool {
    use ashpd::desktop::settings::{ColorScheme, Settings};
    let scheme = block_on(Duration::from_millis(800), async {
        register_app().await;
        Settings::new().await?.color_scheme().await
    });
    matches!(scheme, Some(Ok(ColorScheme::PreferLight)))
}

/// A regular sans-serif system font as (font file data, face index).
pub(crate) fn sans_font() -> Option<(Vec<u8>, u32)> {
    static FONT: LazyLock<Option<(Vec<u8>, u32)>> = LazyLock::new(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let names = ["Cantarell", "Inter", "Noto Sans", "Ubuntu", "DejaVu Sans", "Liberation Sans"];
        let id = names
            .map(fontdb::Family::Name)
            .into_iter()
            .chain([fontdb::Family::SansSerif])
            .find_map(|family| db.query(&fontdb::Query { families: &[family], ..Default::default() }))?;
        db.with_face_data(id, |data, index| (data.to_vec(), index))
    });
    FONT.clone()
}

/// The UI font for the settings dialog (only plain font files: imgui reads the first face).
pub fn ui_font() -> Option<Vec<u8>> {
    sans_font().filter(|(_, index)| *index == 0).map(|(data, _)| data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_arguments_are_escaped() {
        assert_eq!(exec_arg("/opt/Sonorant.AppImage"), r#""/opt/Sonorant.AppImage""#);
        // `\` and `"` get the quoting escape, then the string escape doubles the backslashes.
        assert_eq!(exec_arg(r#"/a b/100%/$x`y"z\w"#), r#""/a b/100%%/\\$x\\`y\\"z\\\\w""#);
    }
}
