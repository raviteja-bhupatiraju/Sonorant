//! Windows: Win32 hotkeys, SendInput, a GDI layered window for the indicator, the registry.

mod capture;
mod hotkeys;
mod overlay;
mod paste;
mod tray;
pub(crate) mod win;

use anyhow::Result;

pub use capture::{clipboard_text, copy_text, open_clipboard, selected_text};
pub use hotkeys::Hotkeys;
pub use overlay::Overlay;
pub use paste::insert;
pub use tray::Tray;

use super::Command;

/// Whether the selection stays readable after the user deselects it (Linux's primary selection).
/// Windows reads it with a copy shortcut, so an empty selection means "nothing selected".
pub const SELECTION_PERSISTS: bool = false;

/// Nothing to prepare: SendInput needs no permission.
pub fn prepare_insert() {}

/// Whether a plain-Esc stop key is claimed only while Sonorant is busy (a permanent global Esc would break Esc everywhere).
pub fn esc_on_demand() -> bool {
    true
}

/// A note shown under the hotkey settings.
pub fn hotkey_note() -> Option<&'static str> {
    None
}

/// Paste shortcut used by `PasteMethod::Auto`.
pub fn auto_paste_label() -> &'static str {
    "Automatic (Ctrl+V, Shift+Insert in terminals)"
}

/// With the GUI subsystem there is no console; attach to the parent's so CLI modes can print.
pub fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Only one instance may own the hotkeys and the tray icon. Held for the life of the process.
pub struct Instance(#[allow(dead_code)] windows_sys::Win32::Foundation::HANDLE);

impl Instance {
    /// Claim the single instance; `Err` if Sonorant is already running. Windows has no command
    /// channel to the running instance, so `forward` is ignored.
    pub fn claim(forward: Option<Command>) -> Result<Self, ()> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let _ = forward;
        let name = win::wide(r"Local\Sonorant.SingleInstance");
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() || GetLastError() == ERROR_ALREADY_EXISTS {
                Err(())
            } else {
                Ok(Self(handle))
            }
        }
    }

    /// Deliver commands from other processes (`sonorant --cmd`); not available on Windows.
    pub fn listen(&self, _sink: super::CommandSink) {}
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn set_autostart(enable: bool) -> Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let key = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?.0;
    if enable {
        let exe = std::env::current_exe()?;
        key.set_value("Sonorant", &format!("\"{}\"", exe.display()))?;
    } else {
        let _ = key.delete_value("Sonorant");
    }
    Ok(())
}

/// Whether Windows apps are set to the light theme.
pub fn prefers_light_theme() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .is_ok_and(|v| v != 0)
}

/// The UI font (Segoe UI), if installed.
pub fn ui_font() -> Option<Vec<u8>> {
    std::fs::read(r"C:\Windows\Fonts\segoeui.ttf").ok()
}
