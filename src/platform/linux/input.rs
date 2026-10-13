//! Reading the selection and typing into other applications (Wayland and X11).
//!
//! "Speak selection" reads the primary selection (the text currently selected anywhere), so no
//! copy shortcut has to be simulated. Keystrokes (paste shortcuts, typed text, Enter) go through
//! the RemoteDesktop portal, which asks the user once and is remembered; compositors without it
//! (wlroots) are served by `wtype` when installed. On X11, keystrokes are sent with XTest.

use std::process::Command as Process;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
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

impl Keyboard {
    fn connect() -> Result<Self> {
        if super::use_x11() {
            return Ok(Keyboard::X11(Box::new(super::x11::Keys::connect()?)));
        }
        let portal = block_on(Duration::from_secs(120), async {
            register_app().await;
            let proxy = RemoteDesktop::new().await?;
            let session = proxy.create_session(Default::default()).await?;
            let token = std::fs::read_to_string(token_path()).ok();
            let options = SelectDevicesOptions::default()
                .set_devices(ashpd::enumflags2::BitFlags::from_flag(DeviceType::Keyboard))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref().map(str::trim));
            proxy.select_devices(&session, options).await?.response()?;
            // Shows the permission dialog unless the restore token is still valid.
            let started = proxy.start(&session, None, Default::default()).await?.response()?;
            if let Some(token) = started.restore_token() {
                let _ = std::fs::create_dir_all(crate::config::data_dir());
                let _ = std::fs::write(token_path(), token);
            }
            ashpd::Result::Ok(Keyboard::Portal { proxy, session })
        });
        match portal {
            Some(Ok(k)) => Ok(k),
            other => {
                let why = match other {
                    Some(Err(e)) => e.to_string(),
                    _ => "no answer".into(),
                };
                log::warn!("RemoteDesktop portal unavailable ({why}); trying wtype");
                if Process::new("wtype").arg("--help").output().is_ok() {
                    Ok(Keyboard::Wtype)
                } else {
                    bail!("cannot send keystrokes: the desktop's RemoteDesktop portal is unavailable ({why}) and wtype is not installed")
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

    fn keysyms(&self, events: Vec<(u32, bool)>) -> Result<()> {
        let Keyboard::Portal { proxy, session } = self else { unreachable!("only the portal sends keysyms") };
        let sent = block_on(Duration::from_secs(10), async {
            for (sym, down) in events {
                let state = if down { KeyState::Pressed } else { KeyState::Released };
                proxy.notify_keyboard_keysym(session, sym as i32, state, Default::default()).await?;
            }
            ashpd::Result::Ok(())
        });
        match sent {
            Some(r) => Ok(r?),
            None => bail!("the RemoteDesktop portal did not respond"),
        }
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

/// Insert `text` into the focused application.
pub fn insert(text: &str, opts: &InsertOptions) -> Result<()> {
    let text = if opts.trailing_space { format!("{text} ") } else { text.to_owned() };
    // Give the user a moment to release the hotkey, whose modifiers would otherwise combine with
    // the keys sent below. Wayland cannot report which keys are held; X11 can.
    if super::use_x11() {
        super::x11::wait_modifiers_released(Duration::from_millis(800));
    }
    std::thread::sleep(Duration::from_millis(80));

    match opts.method {
        PasteMethod::Typing => keyboard()?.type_text(&text)?,
        PasteMethod::ClipboardOnly => {
            clipboard()?.set_text(text)?;
            return Ok(());
        }
        method => {
            let mut cb = clipboard()?;
            let saved = opts.keep_clipboard.then(|| cb.get_text().ok()).flatten();
            cb.set_text(text)?;
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
                cb.set_text(saved)?;
            }
        }
    }
    if opts.auto_submit {
        std::thread::sleep(Duration::from_millis(60));
        send_chord(&[], RETURN)?;
    }
    Ok(())
}
