//! System-wide hotkeys (RegisterHotKey, via `global-hotkey`).

use std::sync::Arc;

use anyhow::Result;
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use parking_lot::Mutex;

use crate::config::Config;
use crate::hotkey::HotkeySpec;
use crate::platform::{Command, CommandSink, HotkeyErrors};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Speak,
    Stop,
    Dictate,
    /// Esc during a dictation.
    Cancel,
}

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    /// Registered hotkeys and what they do (shared with the event handler).
    bindings: Arc<Mutex<Vec<(HotKey, Act)>>>,
    errors: HotkeyErrors,
    /// Esc (plain, and with the dictation hotkey's modifiers, since push-to-talk keeps them held),
    /// registered only while a dictation is in progress, to cancel it.
    esc: Vec<HotKey>,
    /// A "stop" hotkey that is plain Esc is claimed only while something is speaking or being
    /// dictated (a permanent global Esc would break Esc in every other application).
    transient_stop: Option<HotKey>,
    transient_on: bool,
}

impl Hotkeys {
    pub fn new(sink: CommandSink) -> Result<Self> {
        let manager = GlobalHotKeyManager::new()?;
        let bindings: Arc<Mutex<Vec<(HotKey, Act)>>> = Arc::default();
        let table = bindings.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            let Some(act) = table.lock().iter().find(|(k, _)| k.id() == e.id).map(|&(_, act)| act) else { return };
            let pressed = e.state == HotKeyState::Pressed;
            let cmd = match act {
                Act::Dictate => Command::Dictate(pressed),
                _ if !pressed => return,
                Act::Speak => Command::Speak,
                Act::Stop => Command::Stop,
                Act::Cancel => Command::Cancel,
            };
            sink(cmd);
        }));
        Ok(Self { manager, bindings, errors: Default::default(), esc: Vec::new(), transient_stop: None, transient_on: false })
    }

    fn bind(&mut self, key: HotKey, act: Act) -> Result<(), global_hotkey::Error> {
        self.manager.register(key)?;
        self.bindings.lock().push((key, act));
        Ok(())
    }

    fn unbind(&mut self, key: HotKey) {
        let _ = self.manager.unregister(key);
        self.bindings.lock().retain(|(k, _)| k.id() != key.id());
    }

    /// (Re)register the speak, stop and dictate hotkeys from `cfg`.
    pub fn register(&mut self, cfg: &Config) {
        for (key, _) in std::mem::take(&mut *self.bindings.lock()) {
            let _ = self.manager.unregister(key);
        }
        self.esc.clear();
        self.transient_stop = None;
        self.transient_on = false;
        self.errors = Default::default();

        // Slots match `errors`: [speak, stop, dictate].
        let wanted = [
            Some((Act::Speak, &cfg.hotkey)),
            cfg.stop_hotkey.as_ref().map(|k| (Act::Stop, k)),
            cfg.stt_enabled.then_some((Act::Dictate, &cfg.stt_hotkey)),
        ];
        let mut used: Vec<&HotkeySpec> = Vec::new();
        for (slot, (act, spec)) in wanted.into_iter().enumerate().filter_map(|(i, w)| Some((i, w?))) {
            if used.contains(&spec) {
                self.errors[slot] = Some(format!("{} is already used by another Sonorant shortcut", spec.label()));
                continue;
            }
            used.push(spec);
            let Some(key) = spec.to_hotkey() else { continue };
            if act == Act::Stop && spec.is_plain_escape() {
                self.transient_stop = Some(key); // claimed on demand, see `set_busy`
                continue;
            }
            if let Err(e) = self.bind(key, act) {
                log::warn!("could not register {}: {e}", spec.label());
                self.errors[slot] = Some(format!("{} is unavailable (already in use?)", spec.label()));
            }
        }
    }

    pub fn errors(&self) -> HotkeyErrors {
        self.errors.clone()
    }

    /// Claim Esc to cancel while a dictation is in progress (`dictate` is the dictation hotkey).
    pub fn set_dictating(&mut self, on: bool, dictate: &HotkeySpec) {
        for k in std::mem::take(&mut self.esc) {
            self.unbind(k);
        }
        if !on {
            return;
        }
        // Windows matches modifiers exactly, so Esc while Ctrl+Alt are still held (push-to-talk)
        // is a different shortcut from a bare Esc: register both.
        let with_mods = HotkeySpec { key: "Escape".into(), ..dictate.clone() };
        for k in [HotkeySpec::plain("Escape"), with_mods].iter().filter_map(HotkeySpec::to_hotkey) {
            if self.esc.iter().all(|x| x.id() != k.id()) && self.bind(k, Act::Cancel).is_ok() {
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
