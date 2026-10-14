//! X11: global hotkeys as passive key grabs on the root window. Press and release both arrive
//! (push-to-talk works), and a grabbed key is not delivered to the focused application.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, Mapping, ModMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::x11::{self, Keymap, LOCK, MOD2, SHORTCUT_MODS};
use crate::config::Config;
use crate::hotkey::HotkeySpec;
use crate::platform::{Command, CommandSink, HotkeyErrors};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Speak,
    Stop,
    Dictate,
    DictateSubmit,
    /// Esc during a dictation.
    Cancel,
}

impl Act {
    /// The slot of the settings dialog's error list for this hotkey (Esc to cancel has none).
    fn slot(self) -> Option<usize> {
        match self {
            Act::Speak => Some(0),
            Act::Stop => Some(1),
            Act::Dictate => Some(2),
            Act::DictateSubmit => Some(3),
            Act::Cancel => None,
        }
    }
}

struct Bind {
    sym: u32,
    mods: u16,
    act: Act,
    /// The keycode `sym` has in the current layout (0: not on this keyboard).
    keycode: u8,
}

struct State {
    binds: Vec<Bind>,
    keymap: Keymap,
    /// Keys whose press was delivered and whose release is still to come, by keycode.
    held: HashMap<u8, Act>,
}

pub struct X11Hotkeys {
    conn: Arc<RustConnection>,
    root: u32,
    state: Arc<Mutex<State>>,
    /// Shared with the event thread, which grabs the keys again when the keyboard layout changes.
    errors: Arc<Mutex<HotkeyErrors>>,
    /// Esc (plain, and with the dictation hotkey's modifiers), claimed only while dictating, to cancel.
    esc: Vec<(u32, u16)>,
    /// A "stop" hotkey that is plain Esc is claimed only while something is speaking or being dictated.
    transient_stop: Option<(u32, u16)>,
    transient_on: bool,
}

/// Caps Lock and Num Lock must not stop a shortcut from working: grab every combination of them.
const LOCK_COMBOS: [u16; 4] = [0, LOCK, MOD2, LOCK | MOD2];

fn grab(conn: &RustConnection, root: u32, keycode: u8, mods: u16) -> Result<()> {
    for (n, extra) in LOCK_COMBOS.iter().enumerate() {
        let cookie = conn.grab_key(true, root, ModMask::from(mods | extra), keycode, GrabMode::ASYNC, GrabMode::ASYNC)?;
        if let Err(e) = x11::checked(cookie) {
            ungrab(conn, root, keycode, mods, n);
            return Err(e);
        }
    }
    Ok(())
}

/// Release the first `combos` grabs of a key.
fn ungrab(conn: &RustConnection, root: u32, keycode: u8, mods: u16, combos: usize) {
    for extra in &LOCK_COMBOS[..combos] {
        let _ = conn.ungrab_key(keycode, root, ModMask::from(mods | extra));
    }
}

impl State {
    /// Grab every bound key again (after a keyboard mapping change), reporting into `errors` the
    /// ones that are no longer on the keyboard or that another program holds.
    fn grab_all(&mut self, conn: &RustConnection, root: u32, errors: &Mutex<HotkeyErrors>) {
        for b in &mut self.binds {
            b.keycode = self.keymap.find(b.sym).map_or(0, |(k, _)| k);
            let failed = match b.keycode {
                0 => Some("is not on the keyboard (layout changed)"),
                k => grab(conn, root, k, b.mods).is_err().then_some("is unavailable (already in use by another program?)"),
            };
            if failed.is_some() {
                b.keycode = 0;
            }
            if let Some(slot) = b.act.slot() {
                errors.lock()[slot] = failed.map(|why| format!("The {:?} hotkey {why}", b.act));
            }
            if let Some(why) = failed {
                log::warn!("could not grab the {:?} hotkey again: it {why}", b.act);
            }
        }
    }

    fn ungrab_all(&mut self, conn: &RustConnection, root: u32) {
        for b in self.binds.iter().filter(|b| b.keycode != 0) {
            ungrab(conn, root, b.keycode, b.mods, LOCK_COMBOS.len());
        }
        let _ = conn.flush();
    }
}

impl X11Hotkeys {
    pub fn new(sink: CommandSink) -> Result<Self> {
        let d = x11::connect()?;
        // Report a held key once instead of as a stream of release / press pairs (auto-repeat).
        let detectable = || -> Result<()> {
            d.conn.xkb_use_extension(1, 0)?.reply()?;
            let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
            d.conn.xkb_per_client_flags(xkb::ID::USE_CORE_KBD.into(), flag, flag, xkb::BoolCtrl::from(0u32), xkb::BoolCtrl::from(0u32), xkb::BoolCtrl::from(0u32))?.reply()?;
            Ok(())
        };
        if let Err(e) = detectable() {
            log::warn!("no XKB detectable auto-repeat ({e:#}); a held push-to-talk key may stutter");
        }
        let keymap = Keymap::load(&d.conn)?;
        let (conn, root) = (Arc::new(d.conn), d.root);
        let state = Arc::new(Mutex::new(State { binds: Vec::new(), keymap, held: HashMap::new() }));
        let errors = Arc::new(Mutex::new(HotkeyErrors::default()));
        {
            let (conn, state, errors) = (conn.clone(), state.clone(), errors.clone());
            std::thread::Builder::new()
                .name("x11-hotkeys".into())
                .spawn(move || event_loop(&conn, root, &state, &errors, &sink))?;
        }
        Ok(Self { conn, root, state, errors, esc: Vec::new(), transient_stop: None, transient_on: false })
    }

    /// Grab `sym` + `mods` for `act`.
    fn bind(&mut self, (sym, mods): (u32, u16), act: Act) -> Result<(), String> {
        let mut st = self.state.lock();
        let keycode = st.keymap.find(sym).map(|(k, _)| k).ok_or("this key is not on the keyboard")?;
        if st.binds.iter().any(|b| b.keycode == keycode && b.mods == mods) {
            return Err("already claimed".into());
        }
        grab(&self.conn, self.root, keycode, mods).map_err(|e| e.to_string())?;
        let _ = self.conn.flush();
        st.binds.push(Bind { sym, mods, act, keycode });
        Ok(())
    }

    fn unbind(&mut self, (sym, mods): (u32, u16)) {
        let mut st = self.state.lock();
        if let Some(i) = st.binds.iter().position(|b| b.sym == sym && b.mods == mods) {
            let b = st.binds.remove(i);
            if b.keycode != 0 {
                ungrab(&self.conn, self.root, b.keycode, b.mods, LOCK_COMBOS.len());
                let _ = self.conn.flush();
            }
            st.held.remove(&b.keycode);
        }
    }

    /// (Re)bind the speak, stop and dictate hotkeys from `cfg`.
    pub fn register(&mut self, cfg: &Config) {
        {
            let mut st = self.state.lock();
            st.ungrab_all(&self.conn, self.root);
            st.binds.clear();
            st.held.clear();
        }
        self.esc.clear();
        self.transient_stop = None;
        self.transient_on = false;
        let mut errors = HotkeyErrors::default();

        // Slots match `errors`: [speak, stop, dictate, dictate and send].
        let wanted = [
            Some((Act::Speak, &cfg.hotkey)),
            cfg.stop_hotkey.as_ref().map(|k| (Act::Stop, k)),
            cfg.stt_enabled.then_some((Act::Dictate, &cfg.stt_hotkey)),
            cfg.stt_submit_hotkey.as_ref().filter(|_| cfg.stt_enabled).map(|k| (Act::DictateSubmit, k)),
        ];
        let mut used: Vec<&HotkeySpec> = Vec::new();
        for (slot, (act, spec)) in wanted.into_iter().enumerate().filter_map(|(i, w)| Some((i, w?))) {
            if used.contains(&spec) {
                errors[slot] = Some(format!("{} is already used by another Sonorant shortcut", spec.label()));
                continue;
            }
            used.push(spec);
            let Some(grab) = spec.x11_grab() else {
                errors[slot] = Some(format!("{} cannot be used as a hotkey on X11", spec.label()));
                continue;
            };
            if act == Act::Stop && spec.is_plain_escape() {
                self.transient_stop = Some(grab); // claimed on demand, see `set_busy`
                continue;
            }
            if let Err(e) = self.bind(grab, act) {
                log::warn!("could not grab {}: {e}", spec.label());
                errors[slot] = Some(format!("{} is unavailable (already in use by another program?)", spec.label()));
            }
        }
        *self.errors.lock() = errors;
    }

    pub fn errors(&self) -> HotkeyErrors {
        self.errors.lock().clone()
    }

    /// Claim Esc to cancel while a dictation is in progress (`dictate` is the dictation hotkey).
    pub fn set_dictating(&mut self, on: bool, dictate: &HotkeySpec) {
        for k in std::mem::take(&mut self.esc) {
            self.unbind(k);
        }
        if !on {
            return;
        }
        // The grab matches modifiers exactly, so Esc while the dictation hotkey's modifiers are
        // still held (push-to-talk) is a different shortcut from a bare Esc: claim both.
        let with_mods = HotkeySpec { key: "Escape".into(), ..dictate.clone() };
        for k in [HotkeySpec::plain("Escape"), with_mods].iter().filter_map(HotkeySpec::x11_grab) {
            if !self.esc.contains(&k) && self.bind(k, Act::Cancel).is_ok() {
                self.esc.push(k);
            }
        }
    }

    /// Claim / release a plain-Esc stop key depending on whether anything is speaking or dictating.
    /// Returns whether the caller must keep calling this while busy (to release it promptly).
    pub fn set_busy(&mut self, busy: bool) -> bool {
        let Some(key) = self.transient_stop else { return false };
        if busy && !self.transient_on {
            match self.bind(key, Act::Stop) {
                Ok(()) => self.transient_on = true,
                // Already claimed (e.g. by the dictation cancel key): that handler covers it.
                Err(e) => log::debug!("stop key (Esc) not claimed: {e}"),
            }
        } else if !busy && self.transient_on {
            self.unbind(key);
            self.transient_on = false;
        }
        self.transient_on
    }
}

fn command(act: Act, pressed: bool) -> Option<Command> {
    match act {
        Act::Dictate => Some(Command::Dictate(pressed)),
        Act::DictateSubmit => pressed.then_some(Command::DictateSubmit),
        _ if !pressed => None,
        Act::Speak => Some(Command::Speak),
        Act::Stop => Some(Command::Stop),
        Act::Cancel => Some(Command::Cancel),
    }
}

fn event_loop(conn: &RustConnection, root: u32, state: &Mutex<State>, errors: &Mutex<HotkeyErrors>, sink: &CommandSink) {
    loop {
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => {
                log::warn!("lost the X connection, hotkeys stop working: {e}");
                return;
            }
        };
        match event {
            Event::KeyPress(e) => {
                let mods = u16::from(e.state) & SHORTCUT_MODS;
                let act = {
                    let mut st = state.lock();
                    if st.held.contains_key(&e.detail) {
                        continue; // auto-repeat
                    }
                    let Some(act) = st.binds.iter().find(|b| b.keycode == e.detail && b.mods == mods).map(|b| b.act) else {
                        continue;
                    };
                    st.held.insert(e.detail, act);
                    act
                };
                if let Some(cmd) = command(act, true) {
                    sink(cmd);
                }
            }
            Event::KeyRelease(e) => {
                // The grab stays active until the key is up, even if its modifiers were released first.
                let act = state.lock().held.remove(&e.detail);
                if let Some(cmd) = act.and_then(|a| command(a, false)) {
                    sink(cmd);
                }
            }
            Event::MappingNotify(m) if m.request == Mapping::KEYBOARD => {
                // Typing borrows keycodes (see `x11::Keys`), which also lands here: only a change
                // that moves one of our keys needs the grabs redone.
                let Ok(map) = Keymap::load(conn) else { continue };
                let mut st = state.lock();
                let moved = st.binds.iter().any(|b| map.find(b.sym).map_or(0, |(k, _)| k) != b.keycode);
                if moved {
                    st.ungrab_all(conn, root);
                    st.keymap = map;
                    st.grab_all(conn, root, errors);
                    st.held.clear();
                    let _ = conn.flush();
                } else {
                    st.keymap = map;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::platform::linux::x11::Keys;

    #[test]
    fn grabbed_keys_reach_the_sink_and_are_not_delivered_elsewhere() {
        if std::env::var_os("DISPLAY").is_none() || x11::connect().is_err() {
            return;
        }
        let _server = x11::tests::SERVER.lock();
        let (tx, rx) = mpsc::channel();
        let sink: CommandSink = Arc::new(move |c| {
            let _ = tx.send(c);
        });
        let mut hotkeys = X11Hotkeys::new(sink).unwrap();
        hotkeys.register(&Config::default()); // Ctrl+` speak, Ctrl+1 stop, F1 dictate
        assert_eq!(hotkeys.errors(), HotkeyErrors::default());

        let keys = Keys::connect().unwrap();
        let next = || rx.recv_timeout(Duration::from_secs(2)).ok();
        keys.chord(&[0xffe3], 0x60).unwrap(); // Ctrl+`
        assert_eq!(next(), Some(Command::Speak));
        keys.chord(&[0xffe3], 0x31).unwrap(); // Ctrl+1
        assert_eq!(next(), Some(Command::Stop));
        keys.chord(&[], 0xffbe).unwrap(); // F1: press, then release
        assert_eq!(next(), Some(Command::Dictate(true)));
        assert_eq!(next(), Some(Command::Dictate(false)));

        // The same key twice is refused, a key another program holds is reported.
        let mut cfg = Config::default();
        cfg.stt_hotkey = cfg.hotkey.clone();
        hotkeys.register(&cfg);
        assert!(hotkeys.errors()[2].is_some());

        // Esc is claimed on demand only.
        hotkeys.register(&Config::default());
        hotkeys.set_dictating(true, &Config::default().stt_hotkey);
        keys.chord(&[], 0xff1b).unwrap();
        assert_eq!(next(), Some(Command::Cancel));
        hotkeys.set_dictating(false, &Config::default().stt_hotkey);
        keys.chord(&[], 0xff1b).unwrap();
        assert_eq!(next(), None);
    }
}
