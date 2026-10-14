//! X11 plumbing shared by the hotkeys, the key injection and the indicator: connecting, the keyboard
//! mapping (keysym <-> keycode), typing through the XTest extension, and a few queries about the
//! focused window.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ModMask, Window};
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

    /// Up to `n` keycodes without any keysym, which can be borrowed to type characters the layout lacks.
    pub fn spares(&self, n: usize) -> Vec<u8> {
        (self.first..=self.last).rev().filter(|&k| self.row(k).iter().all(|&s| s == 0)).take(n).collect()
    }

    pub fn per_keycode(&self) -> usize {
        self.per
    }
}

/// Types and presses keys with the XTest extension.
pub struct Keys {
    d: Display,
    /// Whether the X server has the XKB extension (to read and set Caps Lock and the layout group).
    xkb: bool,
}

const KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
const KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
const SHIFT_L: u32 = 0xffe1;

/// How many unused keycodes typing may borrow at once (in turn, see `Borrowed`).
const SPARES: usize = 8;
/// How long applications get to catch up with a keyboard mapping change.
const SETTLE: Duration = Duration::from_millis(10);

/// Keycodes without keysyms, lent to characters the layout lacks. They are cleared again when this
/// is dropped, also after an error.
struct Borrowed<'a> {
    keys: &'a Keys,
    per: usize,
    spares: Vec<u8>,
    /// The keysym each spare keycode types now (0: none).
    mapped: Vec<u32>,
    next: usize,
}

impl Borrowed<'_> {
    /// A keycode that types `sym`. Spares are remapped in turn, so a slow application still
    /// reading the mapping of one used a moment ago does not see it change under it.
    fn key_for(&mut self, sym: u32) -> Result<u8> {
        if let Some(i) = self.mapped.iter().position(|&s| s == sym) {
            return Ok(self.spares[i]);
        }
        anyhow::ensure!(!self.spares.is_empty(), "no free key to type a character the layout lacks");
        let i = self.next;
        self.next = (i + 1) % self.spares.len();
        self.mapped[i] = sym; // first, so that even a failed remap is undone
        self.keys.remap(self.spares[i], sym, self.per)?;
        Ok(self.spares[i])
    }

    fn any(&self) -> bool {
        self.mapped.iter().any(|&s| s != 0)
    }
}

impl Drop for Borrowed<'_> {
    fn drop(&mut self) {
        if !self.any() {
            return;
        }
        std::thread::sleep(SETTLE * 5); // the last borrowed key may still be being read
        for (&code, &sym) in self.spares.iter().zip(&self.mapped) {
            if sym != 0 {
                if let Err(e) = self.keys.remap(code, 0, self.per) {
                    log::warn!("could not clear borrowed keycode {code}: {e:#}");
                }
            }
        }
    }
}

/// Caps Lock off and the first layout (XKB group) active while typing, since the keycodes are
/// looked up for that state; the previous state comes back when this is dropped, also after an error.
struct PlainLayout<'a> {
    keys: &'a Keys,
    caps: bool,
    /// The locked group to return to, if it was changed.
    group: Option<xkb::Group>,
}

impl PlainLayout<'_> {
    fn set(&self, caps: bool, group: Option<xkb::Group>) -> Result<()> {
        let lock = ModMask::from(if self.caps { LOCK } else { 0 });
        let mods = ModMask::from(if caps { LOCK } else { 0 });
        let conn = &self.keys.d.conn;
        let device = xkb::ID::USE_CORE_KBD.into();
        checked(conn.xkb_latch_lock_state(device, lock, mods, group.is_some(), group.unwrap_or(xkb::Group::M1), ModMask::from(0u16), false, 0)?)
    }
}

impl Drop for PlainLayout<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.set(self.caps, self.group) {
            log::warn!("could not restore Caps Lock / the keyboard layout: {e:#}");
        }
    }
}

impl Keys {
    pub fn connect() -> Result<Self> {
        let d = connect()?;
        d.conn.xtest_get_version(2, 1)?.reply().context("the X server has no XTest extension")?;
        let xkb = d.conn.xkb_use_extension(1, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.supported);
        if !xkb {
            log::debug!("no XKB: typing does not account for Caps Lock or the active layout");
        }
        Ok(Self { d, xkb })
    }

    /// Switch to `PlainLayout` if Caps Lock is on or another layout is active.
    fn plain_layout(&self) -> Result<Option<PlainLayout<'_>>> {
        if !self.xkb {
            return Ok(None);
        }
        let state = self.d.conn.xkb_get_state(xkb::ID::USE_CORE_KBD.into())?.reply()?;
        let caps = u16::from(state.locked_mods) & LOCK != 0;
        let group = (state.group != xkb::Group::M1).then_some(state.locked_group);
        if !caps && group.is_none() {
            return Ok(None);
        }
        let plain = PlainLayout { keys: self, caps, group };
        // A failure here still restores (on drop) whatever part took effect.
        plain.set(false, group.map(|_| xkb::Group::M1))?;
        Ok(Some(plain))
    }

    fn fake(&self, down: bool, keycode: u8) -> Result<()> {
        self.d.conn.xtest_fake_input(if down { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, self.d.root, 0, 0, 0)?;
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.d.conn.get_input_focus()?.reply()?; // a round trip: everything before it was processed
        // Every client receives MappingNotify (borrowed keycodes cause them): nobody else reads them here.
        while self.d.conn.poll_for_event()?.is_some() {}
        Ok(())
    }

    /// Release the Shift, Ctrl, Alt and Super keys that are down.
    fn release_modifiers(&self) -> Result<()> {
        let pressed = self.d.conn.query_keymap()?.reply()?.keys;
        let map = self.d.conn.get_modifier_mapping()?.reply()?;
        let per = usize::from(map.keycodes_per_modifier()).max(1);
        for (bit, codes) in map.keycodes.chunks(per).enumerate() {
            if SHORTCUT_MODS & (1 << bit) == 0 {
                continue;
            }
            for &k in codes.iter().filter(|&&k| k != 0 && pressed[usize::from(k / 8)] & (1 << (k % 8)) != 0) {
                self.fake(false, k)?;
            }
        }
        self.sync()
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
        // Dropped in reverse order: the borrowed keys are cleared, then the layout state comes back.
        let _layout = self.plain_layout()?;
        let spares = map.spares(SPARES);
        let mut borrowed = Borrowed { keys: self, per: map.per_keycode(), mapped: vec![0; spares.len()], spares, next: 0 };
        for c in text.chars() {
            let sym = keysym(c);
            let (code, with_shift) = match map.find(sym) {
                Some(found) => found,
                None => (borrowed.key_for(sym)?, false),
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
            if borrowed.any() {
                // Applications reload the keyboard mapping when it changes: let them catch up.
                self.sync()?;
                std::thread::sleep(SETTLE);
            }
        }
        self.sync()
    }

    /// Make `keycode` produce `sym` (0 clears it).
    fn remap(&self, keycode: u8, sym: u32, per: usize) -> Result<()> {
        self.d.conn.change_keyboard_mapping(1, keycode, per as u8, &vec![sym; per])?;
        self.sync()?;
        std::thread::sleep(SETTLE);
        Ok(())
    }
}

/// Wait (at most `limit`) until the user has let go of Ctrl, Alt, Shift and Super, which would
/// otherwise combine with the keys sent next. Keys still down then are released with XTest: the
/// dictated text is typed either way, and not as a string of shortcuts.
pub fn wait_modifiers_released(limit: Duration) {
    let Ok(keys) = Keys::connect() else { return };
    let d = &keys.d;
    let start = Instant::now();
    loop {
        let held = d.conn.query_pointer(d.root).ok().and_then(|c| c.reply().ok()).map_or(0, |r| u16::from(r.mask));
        if held & SHORTCUT_MODS == 0 {
            return;
        }
        if start.elapsed() >= limit {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    log::warn!("modifier keys still held after {limit:?}; releasing them");
    if let Err(e) = keys.release_modifiers() {
        log::warn!("could not release the modifier keys: {e:#}");
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
pub(super) mod tests {
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass};
    use x11rb::protocol::Event;

    use super::*;

    /// The tests change the server's focus, keyboard mapping and Caps Lock: one at a time.
    pub(in crate::platform::linux) static SERVER: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Whether an X server is reachable (the tests are skipped otherwise).
    fn server() -> bool {
        std::env::var_os("DISPLAY").is_some() && connect().is_ok()
    }

    /// A connection with a focused window that receives key events.
    fn focused_window() -> Display {
        let d = connect().unwrap();
        let win = d.conn.generate_id().unwrap();
        let aux = CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE);
        d.conn.create_window(0, win, d.root, 0, 0, 50, 50, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        d.conn.map_window(win).unwrap();
        d.conn.set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME).unwrap();
        d.conn.get_input_focus().unwrap().reply().unwrap();
        d
    }

    /// The (keycode, event state) of every key press `d` received, apart from modifier keys.
    fn presses(d: &Display) -> Vec<(u8, u16)> {
        let modifiers = d.conn.get_modifier_mapping().unwrap().reply().unwrap().keycodes;
        let mut got = Vec::new();
        while let Some(event) = d.conn.poll_for_event().unwrap() {
            if let Event::KeyPress(e) = event {
                if !modifiers.contains(&e.detail) {
                    got.push((e.detail, u16::from(e.state)));
                }
            }
        }
        got
    }

    fn keysym(c: char) -> u32 {
        if c.is_ascii() {
            c as u32
        } else {
            0x0100_0000 | c as u32
        }
    }

    #[test]
    fn typed_text_arrives_key_by_key() {
        if !server() {
            return;
        }
        let _server = SERVER.lock();
        let d = focused_window();
        let before = Keymap::load(&d.conn).unwrap();

        let keys = Keys::connect().unwrap();
        let text = "Hi, Zed! \u{fc}\u{20ac}\u{e9}\u{fc}";
        keys.type_text(text, keysym).unwrap();

        let got = presses(&d);
        assert_eq!(got.len(), text.chars().count(), "one key press per character, got {got:?}");
        // The ASCII characters come from the layout, with Shift where the layout needs it.
        for (c, (code, state)) in text.chars().zip(&got).filter(|(c, _)| c.is_ascii()) {
            let (want_code, want_shift) = before.find(c as u32).unwrap();
            assert_eq!((*code, state & SHIFT != 0), (want_code, want_shift), "character {c:?}");
        }
        // The mapping is back to normal afterwards: the borrowed keys have no keysym again.
        assert_eq!(Keymap::load(&d.conn).unwrap().spares(SPARES), before.spares(SPARES));
    }

    /// Also covers the active layout when the server has a second one (`setxkbmap -layout us,ru`).
    #[test]
    fn caps_lock_and_the_layout_do_not_change_typed_text() {
        if !server() {
            return;
        }
        let _server = SERVER.lock();
        let d = focused_window();
        let keys = Keys::connect().unwrap();
        if !keys.xkb {
            return;
        }
        let device = xkb::ID::USE_CORE_KBD.into();
        let set = |on: bool, group: xkb::Group| {
            let mods = ModMask::from(if on { LOCK } else { 0 });
            let cookie = d.conn.xkb_latch_lock_state(device, ModMask::from(LOCK), mods, true, group, ModMask::from(0u16), false, 0);
            checked(cookie.unwrap()).unwrap();
        };
        let state = || d.conn.xkb_get_state(device).unwrap().reply().unwrap();
        d.conn.xkb_use_extension(1, 0).unwrap().reply().unwrap();
        set(true, xkb::Group::M2);
        let second_layout = state().group == xkb::Group::M2; // a single layout wraps back to the first

        keys.type_text("Hi", keysym).unwrap();
        let got = presses(&d);
        let after = state();
        set(false, xkb::Group::M1);

        // Typed without Caps Lock (Shift only for the capital) in the first layout (group bits 13-14
        // of the event state); both are back afterwards.
        let typed: Vec<_> = got.iter().map(|&(_, state)| (state & SHIFT != 0, state & LOCK != 0, state >> 13 & 3)).collect();
        assert_eq!(typed, [(true, false, 0), (false, false, 0)], "got {got:?}");
        assert!(u16::from(after.locked_mods) & LOCK != 0, "Caps Lock was not restored");
        if second_layout {
            assert_eq!(after.group, xkb::Group::M2, "the layout was not restored");
        }
    }

    #[test]
    fn the_modifier_wait_returns_when_nothing_is_held() {
        if !server() {
            return;
        }
        let _server = SERVER.lock();
        let t = Instant::now();
        wait_modifiers_released(Duration::from_secs(2));
        assert!(t.elapsed() < Duration::from_secs(1));
        // No window manager here, so no terminal is focused.
        assert!(!terminal_focused());
    }

    #[test]
    fn held_modifiers_are_released_after_the_wait() {
        if !server() {
            return;
        }
        let _server = SERVER.lock();
        let keys = Keys::connect().unwrap();
        let ctrl = Keymap::load(&keys.d.conn).unwrap().find(0xffe3).unwrap().0;
        keys.fake(true, ctrl).unwrap();
        keys.sync().unwrap();
        wait_modifiers_released(Duration::from_millis(100));
        let mask = u16::from(keys.d.conn.query_pointer(keys.d.root).unwrap().reply().unwrap().mask);
        assert_eq!(mask & CONTROL, 0, "Ctrl is still down");
    }
}
