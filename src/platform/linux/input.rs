//! Reading the selection and typing into other applications (Wayland and X11).
//!
//! "Speak selection" reads the primary selection (the text currently selected anywhere), so no
//! copy shortcut has to be simulated. Keystrokes (paste shortcuts, typed text, Enter) go through
//! the RemoteDesktop portal, which asks the user once and is remembered; compositors without it
//! (wlroots) are served by `wtype` when installed. On X11, keystrokes are sent with XTest.

use std::process::Command as Process;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use arboard::{Clipboard, GetExtLinux, LinuxClipboardKind};
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, Session};
use parking_lot::{Mutex, MutexGuard};

use super::{block_on, register_app};
use crate::config::PasteMethod;
use crate::platform::InsertOptions;

/// The clipboard. Wayland and X11 serve clipboard contents from the process that set them, so one
/// instance lives for the whole run and copied text stays available after the call that set it.
fn clipboard() -> Result<MutexGuard<'static, Clipboard>> {
    static CLIPBOARD: LazyLock<Option<Mutex<Clipboard>>> = LazyLock::new(|| {
        Clipboard::new().inspect_err(|e| log::error!("clipboard unavailable: {e}")).ok().map(Mutex::new)
    });
    CLIPBOARD.as_ref().map(Mutex::lock).context("clipboard unavailable")
}

fn has_words(t: &str) -> bool {
    t.chars().any(char::is_alphanumeric)
}

/// The selected text (primary selection), else with `clipboard_fallback` the clipboard text.
/// The clipboard is never touched, so there is nothing to restore.
pub fn selected_text(_restore: bool, clipboard_fallback: bool) -> Option<(String, Instant)> {
    let t0 = Instant::now();
    let mut cb = clipboard().ok()?;
    let primary = cb.get().clipboard(LinuxClipboardKind::Primary).text().ok().filter(|t| has_words(t));
    let text = primary.or_else(|| clipboard_fallback.then(|| cb.get_text().ok()).flatten().filter(|t| has_words(t)))?;
    Some((text, t0))
}

/// Text currently on the clipboard (for the "speak clipboard" menu item).
pub fn clipboard_text() -> Option<String> {
    clipboard().ok()?.get_text().ok().filter(|t| has_words(t))
}

/// Put `text` on the clipboard.
pub fn copy_text(text: String) {
    if let Ok(mut cb) = clipboard() {
        let _ = cb.set_text(text);
    }
}

// ---- keystrokes ------------------------------------------------------------------------------

/// A key by X keysym and by its name for `wtype`.
#[derive(Clone, Copy)]
struct Key(u32, &'static str);

const CTRL: Key = Key(0xffe3, "ctrl");
const SHIFT: Key = Key(0xffe1, "shift");
const INSERT: Key = Key(0xff63, "Insert");
const RETURN: Key = Key(0xff0d, "Return");
const V: Key = Key(0x76, "v");

/// The keysym that types `c` (Latin-1 keysyms equal the code point; others are offset).
fn keysym(c: char) -> u32 {
    match c {
        '\n' => RETURN.0,
        '\t' => 0xff09,
        ' '..='~' | '\u{a0}'..='\u{ff}' => c as u32,
        c => 0x0100_0000 | c as u32,
    }
}

enum Keyboard {
    Portal { proxy: RemoteDesktop, session: Session<RemoteDesktop> },
    /// XTest on an X11 desktop.
    X11(Box<super::x11::Keys>),
    /// The `wtype` tool (wlroots compositors: Sway, Hyprland, river...).
    Wtype,
}

/// Remembers the user's permission so the RemoteDesktop dialog appears only once.
fn token_path() -> std::path::PathBuf {
    crate::config::data_dir().join("remote-desktop.token")
}

/// Whether the compositor offers the virtual-keyboard protocol that `wtype` types through (wlroots
/// compositors do; GNOME and KDE do not, so `wtype` cannot stand in for the portal there).
fn virtual_keyboard_available() -> bool {
    use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
    use smithay_client_toolkit::reexports::client::protocol::wl_registry::{self, WlRegistry};
    use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};

    struct Probe;
    impl Dispatch<WlRegistry, GlobalListContents> for Probe {
        fn event(_: &mut Self, _: &WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, _queue)) = registry_queue_init::<Probe>(&conn) else { return false };
    globals.contents().with_list(|list| list.iter().any(|g| g.interface == "zwp_virtual_keyboard_manager_v1"))
}

/// End a RemoteDesktop session (ashpd does not close it when dropped). Also withdraws a request
/// still waiting on it, such as an unanswered permission dialog.
fn close_session(session: &Session<RemoteDesktop>) {
    if tokio::runtime::Handle::try_current().is_ok() {
        return; // cannot wait on the runtime from inside it
    }
    if let Some(Err(e)) = block_on(Duration::from_secs(2), session.close()) {
        log::debug!("closing the RemoteDesktop session: {e}");
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        if let Keyboard::Portal { session, .. } = self {
            close_session(session);
        }
    }
}

impl Keyboard {
    fn connect() -> Result<Self> {
        if super::use_x11() {
            return Ok(Keyboard::X11(Box::new(super::x11::Keys::connect()?)));
        }
        let why = match Self::connect_portal() {
            Ok(k) => return Ok(k),
            Err(e) => format!("{e:#}"),
        };
        if !virtual_keyboard_available() {
            bail!("cannot send keystrokes: the desktop's RemoteDesktop portal is unavailable ({why})");
        }
        log::warn!("RemoteDesktop portal unavailable ({why}); trying wtype");
        if Process::new("wtype").arg("--help").output().is_ok() {
            Ok(Keyboard::Wtype)
        } else {
            bail!("cannot send keystrokes: the desktop's RemoteDesktop portal is unavailable ({why}) and wtype is not installed")
        }
    }

    fn connect_portal() -> Result<Self> {
        let (proxy, session) = block_on(Duration::from_secs(10), async {
            register_app().await;
            let proxy = RemoteDesktop::new().await?;
            let session = proxy.create_session(Default::default()).await?;
            ashpd::Result::Ok((proxy, session))
        })
        .context("no answer")??;
        let token = std::fs::read_to_string(token_path()).ok();
        // Shows the permission dialog unless the restore token is still valid: give the user time.
        let started = block_on(Duration::from_secs(120), async {
            let options = SelectDevicesOptions::default()
                .set_devices(ashpd::enumflags2::BitFlags::from_flag(DeviceType::Keyboard))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref().map(str::trim));
            proxy.select_devices(&session, options).await?.response()?;
            proxy.start(&session, None, Default::default()).await?.response()
        });
        match started {
            Some(Ok(started)) => {
                if let Some(token) = started.restore_token() {
                    let _ = std::fs::create_dir_all(crate::config::data_dir());
                    let _ = std::fs::write(token_path(), token);
                }
                Ok(Keyboard::Portal { proxy, session })
            }
            failed => {
                close_session(&session); // don't leave the session (and its dialog) behind
                match failed {
                    Some(Err(e)) => Err(e.into()),
                    _ => bail!("no answer"),
                }
            }
        }
    }

    /// Press `mods`, tap `key`, release `mods`.
    fn chord(&self, mods: &[Key], key: Key) -> Result<()> {
        match self {
            Keyboard::Portal { .. } => {
                let presses = mods.iter().map(|m| (m.0, true));
                let tap = [(key.0, true), (key.0, false)];
                let releases = mods.iter().rev().map(|m| (m.0, false));
                self.keysyms(presses.chain(tap).chain(releases).collect())
            }
            Keyboard::X11(keys) => keys.chord(&mods.iter().map(|m| m.0).collect::<Vec<_>>(), key.0),
            Keyboard::Wtype => {
                let mut args: Vec<&str> = mods.iter().flat_map(|m| ["-M", m.1]).collect();
                args.extend(["-k", key.1]);
                args.extend(mods.iter().rev().flat_map(|m| ["-m", m.1]));
                wtype(&args)
            }
        }
    }

    fn type_text(&self, text: &str) -> Result<()> {
        match self {
            Keyboard::Portal { .. } => self.keysyms(text.chars().flat_map(|c| [(keysym(c), true), (keysym(c), false)]).collect()),
            Keyboard::X11(keys) => keys.type_text(text, keysym),
            Keyboard::Wtype => wtype(&["--", text]),
        }
    }

    /// Send key presses (`true`) and releases through the portal, one D-Bus call each. They go in
    /// chunks with a time limit each, so a long text is not cut off by a fixed limit; after a failure
    /// every key that may still be down is released.
    fn keysyms(&self, events: Vec<(u32, bool)>) -> Result<()> {
        let Keyboard::Portal { proxy, session } = self else { unreachable!("only the portal sends keysyms") };
        let send = |sym: u32, down: bool| {
            let state = if down { KeyState::Pressed } else { KeyState::Released };
            proxy.notify_keyboard_keysym(session, sym as i32, state, Default::default())
        };
        let mut down: Vec<u32> = Vec::new();
        let mut result = Ok(());
        for chunk in events.chunks(64) {
            let sent = block_on(Duration::from_secs(5), async {
                for &(sym, press) in chunk {
                    if press {
                        down.push(sym); // before the call: a press cut off by the time limit may still arrive
                    }
                    send(sym, press).await?;
                    if !press {
                        if let Some(i) = down.iter().rposition(|&s| s == sym) {
                            down.remove(i);
                        }
                    }
                }
                ashpd::Result::Ok(())
            });
            match sent {
                Some(Ok(())) => continue,
                Some(Err(e)) => result = Err(e.into()),
                None => result = Err(anyhow!("the RemoteDesktop portal did not respond")),
            }
            break;
        }
        if result.is_err() && !down.is_empty() {
            let released = block_on(Duration::from_secs(2), async {
                for &sym in down.iter().rev() {
                    send(sym, false).await?;
                }
                ashpd::Result::Ok(())
            });
            if !matches!(released, Some(Ok(()))) {
                log::warn!("could not release the keys pressed before the portal failed");
            }
        }
        result
    }
}

fn wtype(args: &[&str]) -> Result<()> {
    let status = Process::new("wtype").args(args).status().context("running wtype")?;
    anyhow::ensure!(status.success(), "wtype failed ({status})");
    Ok(())
}

/// The shared keyboard connection, made on first use.
static KEYBOARD: Mutex<Option<Arc<Keyboard>>> = Mutex::new(None);

fn keyboard() -> Result<Arc<Keyboard>> {
    let mut k = KEYBOARD.lock(); // held while connecting: callers wait for the permission dialog
    if k.is_none() {
        *k = Some(Arc::new(Keyboard::connect()?));
    }
    Ok(k.clone().expect("connected"))
}

/// Send a key chord, reconnecting once if the portal session was closed meanwhile. (Typed text is
/// not retried: a partly typed text would be typed twice.)
fn send_chord(mods: &[Key], key: Key) -> Result<()> {
    keyboard()?.chord(mods, key).or_else(|e| {
        log::warn!("sending keys failed ({e:#}); reconnecting");
        *KEYBOARD.lock() = None;
        keyboard()?.chord(mods, key)
    })
}

/// Connect the keyboard in the background (called when a dictation starts), so a first-time
/// permission dialog appears while the user is still speaking rather than when the text is ready.
pub fn prepare_insert() {
    std::thread::spawn(|| {
        if let Err(e) = keyboard() {
            log::warn!("{e:#}");
        }
    });
}

/// Press Enter in the focused application (the "send" key pressed without any dictation).
pub fn send_enter() -> Result<()> {
    if super::use_x11() {
        super::x11::wait_modifiers_released(Duration::from_millis(800));
    }
    send_chord(&[], RETURN)
}

/// Insert `text` into the focused application.
pub fn insert(text: &str, opts: &InsertOptions) -> Result<()> {
    let text = if opts.trailing_space { format!("{text} ") } else { text.to_owned() };
    // Give the user a moment to release the hotkey, whose modifiers would otherwise combine with
    // the keys sent below. Wayland cannot report which keys are held; X11 can.
    if super::use_x11() {
        super::x11::wait_modifiers_released(Duration::from_millis(800));
    }
    std::thread::sleep(Duration::from_millis(80));
    if (opts.cancelled)() {
        anyhow::bail!("cancelled");
    }

    let mut method = opts.method;
    if method == PasteMethod::Typing {
        let keyboard = keyboard()?;
        if matches!(*keyboard, Keyboard::Portal { .. }) && !text.is_ascii() {
            // The portal types keysyms, which mutter and KWin can only type when the active layout
            // has them: characters such as é, € or curly quotes would be dropped. Paste instead.
            log::info!("the text has non-ASCII characters, which the portal may not type: pasting it");
            method = PasteMethod::Auto;
        } else if let Err(e) = keyboard.type_text(&text) {
            // The connection may have died: make the next insert reconnect. (Not retried here: a
            // partly typed text would be typed twice.)
            drop(keyboard);
            *KEYBOARD.lock() = None;
            return Err(e);
        }
    }
    match method {
        PasteMethod::Typing => {}
        PasteMethod::ClipboardOnly => {
            clipboard()?.set_text(text)?;
            return Ok(());
        }
        method => paste(text, method, opts.keep_clipboard)?,
    }
    if opts.auto_submit {
        std::thread::sleep(Duration::from_millis(60));
        send_chord(&[], RETURN)?;
    }
    Ok(())
}

/// What the clipboard held before a paste, to be put back afterwards.
enum Saved {
    Text(String),
    Image(arboard::ImageData<'static>),
}

/// Put `text` on the clipboard and send the paste shortcut for `method`; with `keep_clipboard`, put
/// the previous text or image back afterwards. (Other contents, such as copied files, cannot be read
/// here and are not restored.)
fn paste(text: String, method: PasteMethod, keep_clipboard: bool) -> Result<()> {
    let saved = {
        let mut cb = clipboard()?;
        let saved = keep_clipboard.then(|| cb.get_text().map(Saved::Text).or_else(|_| cb.get_image().map(Saved::Image)).ok()).flatten();
        cb.set_text(text.clone())?;
        saved
    }; // not locked while the keys go out: a first permission dialog can take minutes
    std::thread::sleep(Duration::from_millis(40));
    let (mods, key): (&[Key], Key) = match method {
        PasteMethod::ShiftInsert => (&[SHIFT], INSERT),
        PasteMethod::CtrlShiftV => (&[CTRL, SHIFT], V),
        // Terminals paste with Ctrl+Shift+V; only X11 reveals the focused application.
        PasteMethod::Auto if super::use_x11() && super::x11::terminal_focused() => (&[CTRL, SHIFT], V),
        _ => (&[CTRL], V),
    };
    send_chord(mods, key)?;
    if let Some(saved) = saved {
        // Give the target application time to read the clipboard before restoring it.
        std::thread::sleep(Duration::from_millis(300));
        let mut cb = clipboard()?;
        // Something copied meanwhile (by the user or another program) is not ours to replace.
        if cb.get_text().is_ok_and(|now| now == text) {
            match saved {
                Saved::Text(t) => cb.set_text(t)?,
                Saved::Image(img) => cb.set_image(img)?,
            }
        } else {
            log::debug!("the clipboard changed after pasting; not restoring it");
        }
    }
    Ok(())
}
