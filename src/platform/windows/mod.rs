//! Windows: Win32 hotkeys, SendInput, a GDI layered window for the indicator, the registry.

mod capture;
mod clipboard;
mod hotkeys;
mod overlay;
mod paste;
mod tray;
pub(crate) mod win;

use anyhow::Result;

pub use capture::{clipboard_text, copy_text, open_clipboard, selected_text};
pub use hotkeys::Hotkeys;
pub use overlay::Overlay;
pub use paste::{insert, send_enter};
pub use tray::Tray;

use super::Command;

/// Whether the selection stays readable after the user deselects it (Linux's primary selection).
/// Windows reads it with a copy shortcut, so an empty selection means "nothing selected".
pub const SELECTION_PERSISTS: bool = false;

/// Whether the selection cannot be read right now: Windows silently drops the copy shortcut sent
/// to a window that runs as administrator when Sonorant does not.
pub fn selection_blocked() -> bool {
    win::foreground_is_elevated_above_us()
}

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
    "Automatic (Ctrl+V, Shift+Insert or Ctrl+Shift+V in terminals)"
}

/// With the GUI subsystem there is no console; attach to the parent's so CLI modes can print.
pub fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Only one instance may own the hotkeys and the tray icon. Held for the life of the process.
pub struct Instance {
    #[allow(dead_code)]
    mutex: windows_sys::Win32::Foundation::HANDLE,
    /// Signalled by a second `sonorant --settings`, which asks this instance to open its window.
    /// (Windows has no command channel like Linux's socket: this one event is all it supports.)
    open_settings: windows_sys::Win32::Foundation::HANDLE,
}

const EVENT_NAME: &str = r"Local\Sonorant.OpenSettings";

impl Instance {
    /// Claim the single instance; `Err` if Sonorant is already running (a `Settings` command is
    /// handed to it first). Other commands cannot be forwarded on Windows.
    pub fn claim(forward: Option<Command>) -> Result<Self, ()> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, OpenEventW, SetEvent, EVENT_MODIFY_STATE};
        let name = win::wide(r"Local\Sonorant.SingleInstance");
        let event_name = win::wide(EVENT_NAME);
        unsafe {
            let mutex = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if mutex.is_null() {
                return Err(());
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                if forward == Some(Command::Settings) {
                    let event = OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr());
                    if !event.is_null() {
                        SetEvent(event);
                        CloseHandle(event);
                    }
                }
                CloseHandle(mutex);
                return Err(());
            }
            let open_settings = CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()); // auto-reset
            Ok(Self { mutex, open_settings })
        }
    }

    /// Deliver the "open the settings" request of a second launch.
    pub fn listen(&self, sink: super::CommandSink) {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
        if self.open_settings.is_null() {
            return;
        }
        let event = self.open_settings as usize; // the handle lives as long as the process
        std::thread::spawn(move || {
            while unsafe { WaitForSingleObject(event as _, INFINITE) } == WAIT_OBJECT_0 {
                sink(Command::Settings);
            }
        });
    }
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
