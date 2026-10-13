//! Small Win32 helpers shared by the clipboard capture, text insertion, overlay and self-tests.

use std::time::{Duration, Instant};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_UNICODE, VK_CONTROL, VK_LWIN,
    VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};

/// NUL-terminated UTF-16 string for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn key_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

/// The hotkey's own modifiers are still held when it fires; sending Ctrl+C
/// then would combine with them (e.g. Ctrl+Alt+C). Wait for the user to let go.
pub fn wait_for_modifier_release(max: Duration) {
    let start = Instant::now();
    while start.elapsed() < max && [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN].into_iter().any(key_down) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn foreground_class() -> String {
    unsafe {
        let mut buf = [0u16; 128];
        let n = GetClassNameW(GetForegroundWindow(), buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// Consoles and terminals, where Ctrl+C interrupts and Ctrl+V may not paste.
pub fn is_terminal(class: &str) -> bool {
    class == "ConsoleWindowClass" || class.contains("CASCADIA") || class == "mintty"
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

pub fn send_input(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}
