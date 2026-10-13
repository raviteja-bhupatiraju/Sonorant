//! X11 plumbing shared by the hotkeys, the key injection and the indicator: connecting, the keyboard
//! mapping (keysym <-> keycode), typing through the XTest extension, and a few queries about the
//! focused window.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Modifier bits of an X key event's state / of a key grab.
pub const SHIFT: u16 = 1;
pub const LOCK: u16 = 2;
pub const CONTROL: u16 = 4;
pub const MOD1: u16 = 8;
pub const MOD2: u16 = 16;
pub const MOD4: u16 = 64;

/// The modifiers that make up a shortcut (Caps Lock and Num Lock are ignored).
pub const SHORTCUT_MODS: u16 = SHIFT | CONTROL | MOD1 | MOD4;

pub struct Display {
    pub conn: RustConnection,
    pub root: Window,
    pub screen: usize,
}

pub fn connect() -> Result<Display> {
    let (conn, screen) = RustConnection::connect(None).context("cannot connect to the X server")?;
    let root = conn.setup().roots[screen].root;
    Ok(Display { conn, root, screen })
}

/// The keyboard mapping: for every keycode the keysyms it produces (column 0 plain, column 1 with Shift).
pub struct Keymap {
    first: u8,
    last: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn load(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(first, last - first + 1)?.reply()?;
        Ok(Self { first, last, per: reply.keysyms_per_keycode.max(1) as usize, syms: reply.keysyms })
    }

    fn row(&self, keycode: u8) -> &[u32] {
        let i = (keycode - self.first) as usize * self.per;
        &self.syms[i..i + self.per]
    }

    /// A keycode that produces `sym`, and whether Shift must be held for it. Unshifted keys win.
    pub fn find(&self, sym: u32) -> Option<(u8, bool)> {
        let codes = self.first..=self.last;
        codes
            .clone()
            .find(|&k| self.row(k).first() == Some(&sym))
            .map(|k| (k, false))
            .or_else(|| codes.into_iter().find(|&k| self.row(k).get(1) == Some(&sym)).map(|k| (k, true)))
    }

    /// A keycode without any keysym, which can be borrowed to type characters the layout lacks.
    pub fn spare(&self) -> Option<u8> {
        (self.first..=self.last).rev().find(|&k| self.row(k).iter().all(|&s| s == 0))
    }

    pub fn per_keycode(&self) -> usize {
        self.per
    }
}

/// Types and presses keys with the XTest extension.
pub struct Keys {
    d: Display,
}

const KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
const KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
const SHIFT_L: u32 = 0xffe1;

impl Keys {
    pub fn connect() -> Result<Self> {
        let d = connect()?;
        d.conn.xtest_get_version(2, 1)?.reply().context("the X server has no XTest extension")?;
        Ok(Self { d })
    }

    fn fake(&self, down: bool, keycode: u8) -> Result<()> {
        self.d.conn.xtest_fake_input(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, self.d.root, 0, 0, 0)?;
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.d.conn.get_input_focus()?.reply()?; // a round trip: everything before it was processed
        Ok(())
    }

    /// Press `mods` (keysyms), tap `key`, release `mods`.
    pub fn chord(&self, mods: &[u32], key: u32) -> Result<()> {
        let map = Keymap::load(&self.d.conn)?;
        let code = |sym: u32| map.find(sym).map(|(k, _)| k).with_context(|| format!("no key for keysym {sym:#x}"));
        let mods: Vec<u8> = mods.iter().map(|&m| code(m)).collect::<Result<_>>()?;
        let key = code(key)?;
        for &m in &mods {
            self.fake(true, m)?;
        }
        self.fake(true, key)?;
        self.fake(false, key)?;
        for &m in mods.iter().rev() {
            self.fake(false, m)?;
        }
        self.sync()
    }

    /// Type `text`, character by character. Characters the keyboard layout cannot produce borrow
    /// an unused keycode for the moment.
    pub fn type_text(&self, text: &str, keysym: impl Fn(char) -> u32) -> Result<()> {
        let map = Keymap::load(&self.d.conn)?;
        let shift = map.find(SHIFT_L).map(|(k, _)| k);
        let spare = map.spare();
        let mut borrowed = false;
        for c in text.chars() {
            let sym = keysym(c);
            let (code, with_shift) = match map.find(sym) {
                Some(found) => found,
                None => {
                    let code = spare.context("no free key to type a character the layout lacks")?;
                    self.remap(code, sym, map.per_keycode())?;
                    borrowed = true;
                    (code, false)
                }
            };
            let shift = with_shift.then_some(shift).flatten();
            if let Some(s) = shift {
                self.fake(true, s)?;
            }
            self.fake(true, code)?;
            self.fake(false, code)?;
            if let Some(s) = shift {
                self.fake(false, s)?;
            }
            if borrowed {
                // Applications reload the keyboard mapping when it changes: let them catch up.
                self.sync()?;
                std::thread::sleep(Duration::from_millis(6));
            }
        }
        if let (true, Some(code)) = (borrowed, spare) {
            self.remap(code, 0, map.per_keycode())?;
        }
        self.sync()
    }

    /// Make `keycode` produce `sym` (0 clears it).
    fn remap(&self, keycode: u8, sym: u32, per: usize) -> Result<()> {
        self.d.conn.change_keyboard_mapping(1, keycode, per as u8, &vec![sym; per])?;
        self.sync()?;
        std::thread::sleep(Duration::from_millis(6));
        Ok(())
    }
}

/// Wait (at most `limit`) until the user has let go of Ctrl, Alt, Shift and Super, which would
/// otherwise combine with the keys sent next.
pub fn wait_modifiers_released(limit: Duration) {
    let Ok(d) = connect() else { return };
    let start = Instant::now();
    while start.elapsed() < limit {
        let held = d.conn.query_pointer(d.root).ok().and_then(|c| c.reply().ok()).map_or(0, |r| u16::from(r.mask));
        if held & SHORTCUT_MODS == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Whether the focused window belongs to a terminal emulator (which pastes with Ctrl+Shift+V).
pub fn terminal_focused() -> bool {
    const TERMINALS: [&str; 18] = [
        "gnome-terminal", "mate-terminal", "xfce4-terminal", "konsole", "xterm", "uxterm", "urxvt", "rxvt", "kitty", "alacritty",
        "terminator", "tilix", "lxterminal", "st-256color", "wezterm", "foot", "terminology", "qterminal",
    ];
    let class = || -> Result<String> {
        let d = connect()?;
        let atom = |name: &str| -> Result<u32> { Ok(d.conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
        let active = d
            .conn
            .get_property(false, d.root, atom("_NET_ACTIVE_WINDOW")?, xproto::AtomEnum::WINDOW, 0, 1)?
            .reply()?
            .value32()
            .and_then(|mut v| v.next())
            .filter(|&w| w != 0);
        let window = match active {
            Some(w) => w,
            None => d.conn.get_input_focus()?.reply()?.focus,
        };
        let class = d.conn.get_property(false, window, xproto::AtomEnum::WM_CLASS, xproto::AtomEnum::STRING, 0, 256)?.reply()?;
        Ok(String::from_utf8_lossy(&class.value).to_lowercase())
    };
    match class() {
        Ok(class) => TERMINALS.iter().any(|t| class.contains(t)),
        Err(e) => {
            log::debug!("could not read the focused window's class: {e:#}");
            false
        }
    }
}

/// An error unless the X server accepts the request that `cookie` stands for.
pub fn checked<C: Connection>(cookie: x11rb::cookie::VoidCookie<'_, C>) -> Result<()> {
    match cookie.check() {
        Ok(()) => Ok(()),
        Err(e) => bail!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass};
    use x11rb::protocol::Event;

    use super::*;

    /// Whether an X server is reachable (the tests are skipped otherwise).
    fn server() -> bool {
        std::env::var_os("DISPLAY").is_some() && connect().is_ok()
    }

    #[test]
    fn typed_text_arrives_key_by_key() {
        if !server() {
            return;
        }
        let d = connect().unwrap();
        let win = d.conn.generate_id().unwrap();
        let aux = CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE);
        d.conn.create_window(0, win, d.root, 0, 0, 50, 50, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        d.conn.map_window(win).unwrap();
        d.conn.set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME).unwrap();
        d.conn.get_input_focus().unwrap().reply().unwrap();

        let keys = Keys::connect().unwrap();
        let keysym = |c: char| if c.is_ascii() { c as u32 } else { 0x0100_0000 | c as u32 };
        let text = "Hi, Zed! \u{fc}\u{20ac}";
        keys.type_text(text, keysym).unwrap();

        let map = Keymap::load(&d.conn).unwrap();
        let mut got = Vec::new();
        let mut shifted = false;
        while let Some(event) = d.conn.poll_for_event().unwrap() {
            match event {
                Event::KeyPress(e) => got.push((e.detail, u16::from(e.state) & SHIFT != 0)),
                Event::KeyRelease(_) => shifted = shifted || false,
                _ => {}
            }
        }
        assert_eq!(got.len(), text.chars().count(), "one key press per character, got {got:?}");
        // The ASCII characters come from the layout, with Shift where the layout needs it.
        for (c, (code, shift)) in text.chars().zip(&got).filter(|(c, _)| c.is_ascii()) {
            let (want_code, want_shift) = map.find(c as u32).unwrap();
            assert_eq!((*code, *shift), (want_code, want_shift), "character {c:?}");
        }
        // The mapping is back to normal afterwards: the borrowed key has no keysym again.
        assert!(map.spare().is_some());
    }

    #[test]
    fn the_modifier_wait_returns_when_nothing_is_held() {
        if !server() {
            return;
        }
        let t = Instant::now();
        wait_modifiers_released(Duration::from_secs(2));
        assert!(t.elapsed() < Duration::from_secs(1));
        // No window manager here, so no terminal is focused.
        assert!(!terminal_focused());
    }
}
