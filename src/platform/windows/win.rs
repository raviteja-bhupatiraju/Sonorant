//! Small Win32 helpers shared by the clipboard capture, text insertion, overlay and self-tests.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId};

/// NUL-terminated UTF-16 string for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn key_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// Left/right modifier keys, with whether each is an extended key.
const MODIFIERS: [(u16, bool); 8] = [
    (VK_LCONTROL, false),
    (VK_RCONTROL, true),
    (VK_LMENU, false),
    (VK_RMENU, true),
    (VK_LSHIFT, false),
    (VK_RSHIFT, false),
    (VK_LWIN, true),
    (VK_RWIN, true),
];

/// An unassigned virtual key: pressing it while Alt or Win is down stops their release from
/// opening the menu bar / Start menu.
const VK_UNASSIGNED: u16 = 0xE8;

/// The hotkey's own modifiers are still held when it fires; sending Ctrl+C
/// then would combine with them (e.g. Ctrl+Alt+C). Wait for the user to let go; if they are
/// still held after `max`, release them (for the system) so the shortcut we send is not changed.
pub fn wait_for_modifier_release(max: Duration) {
    let start = Instant::now();
    let held = || MODIFIERS.into_iter().filter(|&(vk, _)| key_down(vk)).collect::<Vec<_>>();
    let mut down = held();
    while start.elapsed() < max && !down.is_empty() {
        std::thread::sleep(Duration::from_millis(5));
        down = held();
    }
    if down.is_empty() {
        return;
    }
    log::info!("modifier keys still held after {max:?}; releasing them before sending keystrokes");
    let mut inputs = Vec::new();
    if down.iter().any(|&(vk, _)| matches!(vk, VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN)) {
        inputs.extend([key_event(VK_UNASSIGNED, 0), key_event(VK_UNASSIGNED, KEYEVENTF_KEYUP)]);
    }
    for (vk, extended) in down {
        inputs.push(key_event(vk, KEYEVENTF_KEYUP | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }));
    }
    if let Err(e) = send_input(&inputs) {
        log::warn!("could not release the held modifier keys: {e:#}");
    }
}

pub fn foreground_class() -> String {
    unsafe {
        let mut buf = [0u16; 128];
        let n = GetClassNameW(GetForegroundWindow(), buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// A process handle, closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The process that owns the foreground window (opened for limited queries).
fn foreground_process() -> Option<Handle> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        (!h.is_null()).then_some(Handle(h))
    }
}

/// File name of the foreground window's executable, lower-case (empty if unknown).
pub fn foreground_exe() -> String {
    let Some(process) = foreground_process() else { return String::new() };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    if unsafe { QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) } == 0 {
        return String::new();
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().unwrap_or_default().to_lowercase()
}

/// Terminal emulators that do not use the console window classes.
const TERMINAL_EXES: &[&str] = &[
    "windowsterminal.exe",
    "openconsole.exe",
    "conhost.exe",
    "mintty.exe",
    "wezterm-gui.exe",
    "alacritty.exe",
    "tabby.exe",
    "kitty.exe",
    "hyper.exe",
    "putty.exe",
    "mobaxterm.exe",
    "conemu.exe",
    "conemu64.exe",
    "fluentterminal.app.exe",
    "rio.exe",
    "ghostty.exe",
    "warp.exe",
    "contour.exe",
];

/// Terminals whose paste shortcut is Ctrl+Shift+V (Shift+Insert does not paste the clipboard).
const CTRL_SHIFT_V_EXES: &[&str] = &["alacritty.exe", "hyper.exe"];

/// Consoles and terminals, where Ctrl+V may not paste. `exe` is the lower-case file name from
/// `foreground_exe`. (Copying does not depend on this: it always uses Ctrl+Insert, see `capture`.)
pub fn is_terminal(class: &str, exe: &str) -> bool {
    class == "ConsoleWindowClass" || class.contains("CASCADIA") || class == "mintty" || TERMINAL_EXES.contains(&exe)
}

/// The terminal pastes with Ctrl+Shift+V rather than Shift+Insert.
pub fn pastes_with_ctrl_shift_v(exe: &str) -> bool {
    CTRL_SHIFT_V_EXES.contains(&exe)
}

/// `Some(elevated)` for the process `h`, `None` if its token cannot be read.
unsafe fn is_elevated(h: HANDLE) -> Option<bool> {
    let mut token: HANDLE = std::ptr::null_mut();
    if OpenProcessToken(h, TOKEN_QUERY, &mut token) == 0 {
        return None;
    }
    let token = Handle(token);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = GetTokenInformation(
        token.0,
        TokenElevation,
        &mut elevation as *mut TOKEN_ELEVATION as *mut _,
        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut len,
    );
    (ok != 0).then_some(elevation.TokenIsElevated != 0)
}

/// Whether the foreground window belongs to an elevated (administrator) process while we are
/// not elevated. Windows (UIPI) then silently drops the keystrokes we send it.
pub fn foreground_is_elevated_above_us() -> bool {
    unsafe {
        if is_elevated(GetCurrentProcess()) != Some(false) {
            return false;
        }
        match foreground_process() {
            // A process whose token we may not even query runs at a higher integrity level.
            Some(p) => is_elevated(p.0).unwrap_or(true),
            None => false,
        }
    }
}

fn keyboard_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

pub fn key_event(vk: u16, flags: u32) -> INPUT {
    keyboard_input(vk, 0, flags)
}

/// A UTF-16 code unit typed directly, independent of the keyboard layout.
pub fn unicode_event(unit: u16, flags: u32) -> INPUT {
    keyboard_input(0, unit, KEYEVENTF_UNICODE | flags)
}

/// Inject `inputs`; an error if Windows did not accept all of them (e.g. another program's input
/// is blocking it). Input dropped by UIPI is not reported here: see `foreground_is_elevated_above_us`.
pub fn send_input(inputs: &[INPUT]) -> anyhow::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        anyhow::bail!("Windows accepted {sent} of {} simulated key events (error {})", inputs.len(), unsafe {
            GetLastError()
        });
    }
    Ok(())
}
