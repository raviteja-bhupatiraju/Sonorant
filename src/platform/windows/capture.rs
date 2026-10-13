//! Grab the currently selected text by simulating a copy shortcut and
//! reading the clipboard, then put the previous clipboard content back.

use std::time::{Duration, Instant};

use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_CONTROL, VK_INSERT};

use super::win::{foreground_class, is_terminal, key_event, send_input, wait_for_modifier_release};

const VK_C: u16 = 0x43;

fn send_copy_shortcut() {
    // In consoles / terminals Ctrl+C means "interrupt", so use Ctrl+Insert there.
    let (key, extra) = if is_terminal(&foreground_class()) { (VK_INSERT, KEYEVENTF_EXTENDEDKEY) } else { (VK_C, 0) };
    send_input(&[
        key_event(VK_CONTROL, 0),
        key_event(key, extra),
        key_event(key, extra | KEYEVENTF_KEYUP),
        key_event(VK_CONTROL, KEYEVENTF_KEYUP),
    ]);
}

pub(crate) enum Saved {
    Text(String),
    Image(arboard::ImageData<'static>),
    Empty,
}

pub(crate) fn save_clipboard(clipboard: &mut arboard::Clipboard) -> Saved {
    if let Ok(t) = clipboard.get_text() {
        Saved::Text(t)
    } else if let Ok(img) = clipboard.get_image() {
        Saved::Image(img.to_owned_img())
    } else {
        Saved::Empty
    }
}

pub(crate) fn restore_clipboard(clipboard: &mut arboard::Clipboard, saved: Saved) {
    let _ = match saved {
        Saved::Text(t) => clipboard.set_text(t),
        Saved::Image(img) => clipboard.set_image(img),
        Saved::Empty => clipboard.clear(),
    };
}

/// Call `f` up to `tries` times, 10 ms apart, until it succeeds.
fn retry<T, E>(tries: usize, mut f: impl FnMut() -> Result<T, E>) -> Option<T> {
    for i in 0..tries {
        if let Ok(v) = f() {
            return Some(v);
        }
        if i + 1 < tries {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    None
}

/// The clipboard is briefly locked by whichever application last wrote to it.
pub fn open_clipboard() -> Option<arboard::Clipboard> {
    retry(10, arboard::Clipboard::new)
}

/// Put `text` on the clipboard.
pub fn copy_text(text: String) {
    if let Some(mut cb) = open_clipboard() {
        let _ = cb.set_text(text);
    }
}

fn has_words(t: &str) -> bool {
    t.chars().any(char::is_alphanumeric)
}

/// Returns the selected text, or `None` if nothing was selected / copied.
/// With `clipboard_fallback`, the existing clipboard text is returned instead
/// when nothing is selected.
pub fn selected_text(restore: bool, clipboard_fallback: bool) -> Option<(String, Instant)> {
    wait_for_modifier_release(Duration::from_millis(1500));

    let mut clipboard = open_clipboard()?;
    let saved = save_clipboard(&mut clipboard);

    let before = unsafe { GetClipboardSequenceNumber() };
    let t0 = Instant::now();
    send_copy_shortcut();

    let changed = loop {
        if unsafe { GetClipboardSequenceNumber() } != before {
            break true;
        }
        if t0.elapsed() >= Duration::from_millis(500) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(4));
    };

    let result = if changed {
        // Give the source app a moment to finish writing all formats.
        std::thread::sleep(Duration::from_millis(15));
        let text = retry(10, || clipboard.get_text());
        if restore {
            restore_clipboard(&mut clipboard, saved);
        }
        text
    } else {
        match saved {
            Saved::Text(t) if clipboard_fallback => Some(t),
            _ => None,
        }
    };
    result.filter(|t| has_words(t)).map(|t| (t, t0))
}

/// Text currently on the clipboard (for the "speak clipboard" menu item).
pub fn clipboard_text() -> Option<String> {
    open_clipboard()?.get_text().ok().filter(|t| has_words(t))
}
