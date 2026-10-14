//! Grab the currently selected text by simulating a copy shortcut and
//! reading the clipboard, then put the previous clipboard content back.

use std::time::{Duration, Instant};

use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_CONTROL, VK_INSERT};

use super::clipboard;
use super::win::{key_event, send_input, wait_for_modifier_release};

/// Ctrl+Insert copies in every Windows toolkit, and in terminals (where Ctrl+C would interrupt
/// the running program when nothing is selected) it is the copy shortcut. It is used everywhere,
/// so that a terminal Sonorant does not know about cannot be interrupted.
fn send_copy_shortcut() {
    let sent = send_input(&[
        key_event(VK_CONTROL, 0),
        key_event(VK_INSERT, KEYEVENTF_EXTENDEDKEY),
        key_event(VK_INSERT, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
        key_event(VK_CONTROL, KEYEVENTF_KEYUP),
    ]);
    if let Err(e) = sent {
        log::warn!("copy shortcut: {e:#}");
    }
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
    let saved = restore.then(clipboard::save);
    if matches!(saved, Some(clipboard::Saved::Failed)) {
        // The copy shortcut would overwrite a clipboard that cannot be backed up.
        log::warn!("selection: the clipboard cannot be saved, so the selection is not read");
        return None;
    }

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
        if let Some(saved) = saved {
            clipboard::restore(saved);
        }
        text
    } else if clipboard_fallback {
        // Nothing was copied, so the clipboard still holds what it held before.
        clipboard.get_text().ok()
    } else {
        None
    };
    result.filter(|t| has_words(t)).map(|t| (t, t0))
}

/// Text currently on the clipboard (for the "speak clipboard" menu item).
pub fn clipboard_text() -> Option<String> {
    open_clipboard()?.get_text().ok().filter(|t| has_words(t))
}
