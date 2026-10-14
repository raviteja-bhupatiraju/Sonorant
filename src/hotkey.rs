//! Hotkey description that can be stored in the config file, shown in the UI, recorded from
//! winit key events and converted to a `global-hotkey` hotkey (Windows) or an XDG shortcut
//! trigger for the GlobalShortcuts portal (Wayland).

use serde::{Deserialize, Serialize};
use winit::keyboard::KeyCode;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeySpec {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// W3C key code name, e.g. `KeyS`, `Digit1`, `F5`, `Space`.
    pub key: String,
}

impl HotkeySpec {
    pub fn new(ctrl: bool, alt: bool, shift: bool, key: &str) -> Self {
        Self { ctrl, alt, shift, meta: false, key: key.to_owned() }
    }

    /// `key` without any modifier.
    pub fn plain(key: &str) -> Self {
        Self::new(false, false, false, key)
    }

    /// Plain Esc: only ever claimed temporarily, since a global Esc would break every other application.
    pub fn is_plain_escape(&self) -> bool {
        *self == Self::plain("Escape")
    }

    /// The held modifiers as (label prefix, XDG trigger name).
    fn modifiers(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        [(self.ctrl, "Ctrl+", "CTRL"), (self.alt, "Alt+", "ALT"), (self.shift, "Shift+", "SHIFT"), (self.meta, META_LABEL, "LOGO")]
            .into_iter()
            .filter_map(|(held, label, xdg)| held.then_some((label, xdg)))
    }

    #[cfg(windows)]
    pub fn to_hotkey(&self) -> Option<global_hotkey::hotkey::HotKey> {
        use global_hotkey::hotkey::{Code, HotKey, Modifiers};
        let code: Code = self.key.parse().ok()?;
        let flags = [(self.ctrl, Modifiers::CONTROL), (self.alt, Modifiers::ALT), (self.shift, Modifiers::SHIFT), (self.meta, Modifiers::SUPER)];
        let m = flags.into_iter().filter(|f| f.0).fold(Modifiers::empty(), |acc, f| acc | f.1);
        Some(HotKey::new((!m.is_empty()).then_some(m), code))
    }

    /// The shortcut in the XDG shortcuts format the GlobalShortcuts portal takes, e.g. `CTRL+grave`.
    #[cfg(target_os = "linux")]
    pub fn xdg_trigger(&self) -> Option<String> {
        let key = xdg_key(&self.key)?;
        Some(self.modifiers().map(|(_, xdg)| format!("{xdg}+")).collect::<String>() + &key)
    }

    /// The X11 keysym of the key and the X modifier mask of the held modifiers (X11 key grabs).
    #[cfg(target_os = "linux")]
    pub fn x11_grab(&self) -> Option<(u32, u16)> {
        const SHIFT: u16 = 1;
        const CONTROL: u16 = 4;
        const MOD1: u16 = 8; // Alt
        const MOD4: u16 = 64; // Super
        let mods = [(self.ctrl, CONTROL), (self.alt, MOD1), (self.shift, SHIFT), (self.meta, MOD4)]
            .into_iter()
            .filter(|m| m.0)
            .fold(0, |acc, m| acc | m.1);
        Some((keysym_value(&xdg_key(&self.key)?)?, mods))
    }

    /// Human readable form such as `Ctrl+Alt+S`.
    pub fn label(&self) -> String {
        let s: String = self.modifiers().map(|(label, _)| label).collect();
        let k = self.key.as_str();
        let punct = match k {
            "Backquote" => Some("`"),
            "Minus" => Some("-"),
            "Equal" => Some("="),
            "Comma" => Some(","),
            "Period" => Some("."),
            "Slash" => Some("/"),
            "Semicolon" => Some(";"),
            "Quote" => Some("'"),
            "BracketLeft" => Some("["),
            "BracketRight" => Some("]"),
            "Backslash" => Some("\\"),
            _ => None,
        };
        let pretty = punct.unwrap_or_else(|| {
            k.strip_prefix("Key")
                .or_else(|| k.strip_prefix("Digit"))
                .or_else(|| k.strip_prefix("Arrow"))
                .unwrap_or(k)
        });
        s + pretty
    }

    pub fn is_modifier(key: KeyCode) -> bool {
        matches!(
            key,
            KeyCode::ControlLeft
                | KeyCode::ControlRight
                | KeyCode::ShiftLeft
                | KeyCode::ShiftRight
                | KeyCode::AltLeft
                | KeyCode::AltRight
                | KeyCode::SuperLeft
                | KeyCode::SuperRight
                | KeyCode::Meta
                | KeyCode::Hyper
        )
    }

    /// Build a spec from the modifier state and the pressed physical key.
    /// Returns `None` for bare modifier presses, keys the platform cannot bind, and combinations
    /// that would hijack normal typing (no Ctrl/Alt/Super and not a function key: Shift alone
    /// only types capitals and symbols).
    pub fn from_winit(ctrl: bool, alt: bool, shift: bool, meta: bool, key: KeyCode) -> Option<Self> {
        let name = format!("{key:?}");
        if Self::is_modifier(key) || !key_supported(&name) {
            return None;
        }
        let is_fkey = name.len() >= 2 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit());
        if !(ctrl || alt || meta || is_fkey) && key != KeyCode::Escape {
            return None;
        }
        Some(Self { ctrl, alt, shift, meta, key: name })
    }
}

#[cfg(windows)]
const META_LABEL: &str = "Win+";
#[cfg(not(windows))]
const META_LABEL: &str = "Super+";

#[cfg(windows)]
fn key_supported(name: &str) -> bool {
    name.parse::<global_hotkey::hotkey::Code>().is_ok()
}

#[cfg(target_os = "linux")]
fn key_supported(name: &str) -> bool {
    xdg_key(name).is_some()
}

/// The xkb keysym name for a W3C key code name.
#[cfg(target_os = "linux")]
fn xdg_key(code: &str) -> Option<String> {
    if let Some(c) = code.strip_prefix("Key").filter(|c| c.len() == 1) {
        return Some(c.to_lowercase());
    }
    if let Some(d) = code.strip_prefix("Digit").filter(|d| d.len() == 1) {
        return Some(d.to_owned());
    }
    if let Some(d) = code.strip_prefix("Numpad").filter(|d| d.len() == 1) {
        return Some(format!("KP_{d}"));
    }
    if code.len() >= 2 && code.starts_with('F') && code[1..].chars().all(|c| c.is_ascii_digit()) {
        return Some(code.to_owned());
    }
    let name = match code {
        "Backquote" => "grave",
        "Minus" => "minus",
        "Equal" => "equal",
        "BracketLeft" => "bracketleft",
        "BracketRight" => "bracketright",
        "Backslash" => "backslash",
        "Semicolon" => "semicolon",
        "Quote" => "apostrophe",
        "Comma" => "comma",
        "Period" => "period",
        "Slash" => "slash",
        "Space" => "space",
        "Enter" => "Return",
        "Tab" => "Tab",
        "Escape" => "Escape",
        "Backspace" => "BackSpace",
        "Insert" => "Insert",
        "Delete" => "Delete",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Page_Up",
        "PageDown" => "Page_Down",
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "Pause" => "Pause",
        "ScrollLock" => "Scroll_Lock",
        "PrintScreen" => "Print",
        "NumpadAdd" => "KP_Add",
        "NumpadSubtract" => "KP_Subtract",
        "NumpadMultiply" => "KP_Multiply",
        "NumpadDivide" => "KP_Divide",
        "NumpadDecimal" => "KP_Decimal",
        "NumpadEnter" => "KP_Enter",
        _ => return None,
    };
    Some(name.to_owned())
}

/// The numeric X keysym for a keysym name as returned by `xdg_key`.
#[cfg(target_os = "linux")]
fn keysym_value(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return c.is_ascii_alphanumeric().then_some(c as u32);
    }
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=35).contains(n)) {
        return Some(0xffbd + n);
    }
    if let Some(d) = name.strip_prefix("KP_").and_then(|d| d.parse::<u32>().ok()).filter(|d| *d <= 9) {
        return Some(0xffb0 + d);
    }
    Some(match name {
        "grave" => 0x60,
        "minus" => 0x2d,
        "equal" => 0x3d,
        "bracketleft" => 0x5b,
        "bracketright" => 0x5d,
        "backslash" => 0x5c,
        "semicolon" => 0x3b,
        "apostrophe" => 0x27,
        "comma" => 0x2c,
        "period" => 0x2e,
        "slash" => 0x2f,
        "space" => 0x20,
        "Return" => 0xff0d,
        "Tab" => 0xff09,
        "Escape" => 0xff1b,
        "BackSpace" => 0xff08,
        "Insert" => 0xff63,
        "Delete" => 0xffff,
        "Home" => 0xff50,
        "End" => 0xff57,
        "Page_Up" => 0xff55,
        "Page_Down" => 0xff56,
        "Up" => 0xff52,
        "Down" => 0xff54,
        "Left" => 0xff51,
        "Right" => 0xff53,
        "Pause" => 0xff13,
        "Scroll_Lock" => 0xff14,
        "Print" => 0xff61,
        "KP_Add" => 0xffab,
        "KP_Subtract" => 0xffad,
        "KP_Multiply" => 0xffaa,
        "KP_Divide" => 0xffaf,
        "KP_Decimal" => 0xffae,
        "KP_Enter" => 0xff8d,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_escape_is_a_valid_stop_key_but_other_plain_keys_are_not() {
        let esc = HotkeySpec::from_winit(false, false, false, false, KeyCode::Escape).expect("Esc");
        assert_eq!(esc.key, "Escape");
        #[cfg(windows)]
        assert!(esc.to_hotkey().is_some());
        assert!(HotkeySpec::from_winit(false, false, false, false, KeyCode::KeyA).is_none());
        assert!(HotkeySpec::from_winit(false, false, false, false, KeyCode::F2).is_some());
        assert!(HotkeySpec::from_winit(true, false, false, false, KeyCode::ControlLeft).is_none());
    }

    #[test]
    fn shift_alone_does_not_make_a_typing_key_bindable() {
        assert!(HotkeySpec::from_winit(false, false, true, false, KeyCode::KeyA).is_none());
        assert!(HotkeySpec::from_winit(false, false, true, false, KeyCode::Digit1).is_none());
        assert!(HotkeySpec::from_winit(false, false, true, false, KeyCode::Space).is_none());
        let spec = HotkeySpec::from_winit(true, false, true, false, KeyCode::KeyA).expect("Ctrl+Shift+A");
        assert!(spec.ctrl && spec.shift);
        assert!(HotkeySpec::from_winit(false, true, false, false, KeyCode::KeyA).is_some());
        assert!(HotkeySpec::from_winit(false, false, false, true, KeyCode::KeyA).is_some());
        assert!(HotkeySpec::from_winit(false, false, true, false, KeyCode::F5).is_some());
    }

    #[test]
    fn missing_fields_take_defaults() {
        let spec: HotkeySpec = serde_json::from_str(r#"{"ctrl":true,"key":"KeyS"}"#).expect("parse");
        assert_eq!(spec, HotkeySpec::new(true, false, false, "KeyS"));
    }

    #[test]
    fn backquote_is_assignable() {
        let spec = HotkeySpec::from_winit(true, false, false, false, KeyCode::Backquote).expect("spec");
        assert_eq!(spec.key, "Backquote");
        #[cfg(windows)]
        assert!(spec.to_hotkey().is_some());
        #[cfg(target_os = "linux")]
        {
            assert_eq!(spec.xdg_trigger().as_deref(), Some("CTRL+grave"));
            assert_eq!(spec.x11_grab(), Some((0x60, 4)));
        }
        assert_eq!(spec.label(), "Ctrl+`");
    }
}
