//! Insert dictated text into whatever application has the focus.

use std::time::Duration;

use anyhow::Context;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_CONTROL, VK_INSERT, VK_RETURN, VK_SHIFT,
};

use super::capture::{open_clipboard, restore_clipboard, save_clipboard};
use super::win::{foreground_class, is_terminal, key_event, send_input, unicode_event, wait_for_modifier_release};
use crate::config::PasteMethod;
use crate::platform::InsertOptions;

const VK_V: u16 = 0x56;

fn press_enter() -> [INPUT; 2] {
    [key_event(VK_RETURN, 0), key_event(VK_RETURN, KEYEVENTF_KEYUP)]
}

/// Type `text` as Unicode characters. Independent of the keyboard layout and of the clipboard.
fn type_text(text: &str) {
    let inputs: Vec<INPUT> = text
        .encode_utf16()
        .flat_map(|unit| {
            if unit == u16::from(b'\n') {
                press_enter()
            } else {
                [unicode_event(unit, 0), unicode_event(unit, KEYEVENTF_KEYUP)]
            }
        })
        .collect();
    // Send in batches so very long transcripts do not overflow the input queue.
    for batch in inputs.chunks(200) {
        send_input(batch);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn paste_keys(method: PasteMethod, class: &str) {
    let (mods, key, extra): (&[u16], u16, u32) = match method {
        PasteMethod::ShiftInsert => (&[VK_SHIFT], VK_INSERT, KEYEVENTF_EXTENDEDKEY),
        PasteMethod::CtrlShiftV => (&[VK_CONTROL, VK_SHIFT], VK_V, 0),
        PasteMethod::Auto if is_terminal(class) => (&[VK_SHIFT], VK_INSERT, KEYEVENTF_EXTENDEDKEY),
        _ => (&[VK_CONTROL], VK_V, 0),
    };
    let inputs: Vec<INPUT> = mods
        .iter()
        .map(|&m| key_event(m, 0))
        .chain([key_event(key, extra), key_event(key, extra | KEYEVENTF_KEYUP)])
        .chain(mods.iter().rev().map(|&m| key_event(m, KEYEVENTF_KEYUP)))
        .collect();
    send_input(&inputs);
}

/// Insert `text` into the focused application.
pub fn insert(text: &str, opts: &InsertOptions) -> anyhow::Result<()> {
    let text = if opts.trailing_space { format!("{text} ") } else { text.to_owned() };
    // The dictation hotkey's modifiers may still be down; they would turn typed text or
    // Ctrl+V into other shortcuts.
    wait_for_modifier_release(Duration::from_millis(1500));

    match opts.method {
        PasteMethod::Typing => type_text(&text),
        PasteMethod::ClipboardOnly => {
            open_clipboard().context("clipboard is busy")?.set_text(text)?;
            return Ok(());
        }
        method => {
            let mut cb = open_clipboard().context("clipboard is busy")?;
            let saved = opts.keep_clipboard.then(|| save_clipboard(&mut cb));
            cb.set_text(text)?;
            std::thread::sleep(Duration::from_millis(40));
            paste_keys(method, &foreground_class());
            if let Some(saved) = saved {
                // Give the target application time to read the clipboard before restoring it.
                std::thread::sleep(Duration::from_millis(300));
                restore_clipboard(&mut cb, saved);
            }
        }
    }
    if opts.auto_submit {
        std::thread::sleep(Duration::from_millis(60));
        send_input(&press_enter());
    }
    Ok(())
}
