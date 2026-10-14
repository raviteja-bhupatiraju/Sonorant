//! Insert dictated text into whatever application has the focus.

use std::time::Duration;

use anyhow::Context;
use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VK_CONTROL, VK_INSERT, VK_RETURN, VK_SHIFT,
};

use super::capture::open_clipboard;
use super::clipboard;
use super::win::{
    foreground_class, foreground_exe, foreground_is_elevated_above_us, is_terminal, key_event, pastes_with_ctrl_shift_v,
    send_input, unicode_event, wait_for_modifier_release,
};
use crate::config::PasteMethod;
use crate::platform::InsertOptions;

const VK_V: u16 = 0x56;

fn press_enter() -> [INPUT; 2] {
    [key_event(VK_RETURN, 0), key_event(VK_RETURN, KEYEVENTF_KEYUP)]
}

/// Type `text` as Unicode characters. Independent of the keyboard layout and of the clipboard.
fn type_text(text: &str) -> anyhow::Result<()> {
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
        send_input(batch).context("typing the text failed")?;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn paste_keys(method: PasteMethod) -> anyhow::Result<()> {
    let auto = || {
        let exe = foreground_exe();
        if !is_terminal(&foreground_class(), &exe) {
            PasteMethod::CtrlV
        } else if pastes_with_ctrl_shift_v(&exe) {
            PasteMethod::CtrlShiftV
        } else {
            PasteMethod::ShiftInsert
        }
    };
    let method = if method == PasteMethod::Auto { auto() } else { method };
    let (mods, key, extra): (&[u16], u16, u32) = match method {
        PasteMethod::ShiftInsert => (&[VK_SHIFT], VK_INSERT, KEYEVENTF_EXTENDEDKEY),
        PasteMethod::CtrlShiftV => (&[VK_CONTROL, VK_SHIFT], VK_V, 0),
        _ => (&[VK_CONTROL], VK_V, 0),
    };
    let inputs: Vec<INPUT> = mods
        .iter()
        .map(|&m| key_event(m, 0))
        .chain([key_event(key, extra), key_event(key, extra | KEYEVENTF_KEYUP)])
        .chain(mods.iter().rev().map(|&m| key_event(m, KEYEVENTF_KEYUP)))
        .collect();
    send_input(&inputs).context("sending the paste shortcut failed")
}

/// How long the target application gets to read the clipboard before it is restored. Large
/// pastes and slow applications (Office, remote desktops) need longer.
fn restore_delay(text: &str) -> Duration {
    Duration::from_millis(500 + (text.len() as u64 / 100).min(1500))
}

/// Insert `text` into the focused application.
pub fn insert(text: &str, opts: &InsertOptions) -> anyhow::Result<()> {
    let text = if opts.trailing_space { format!("{text} ") } else { text.to_owned() };
    // The dictation hotkey's modifiers may still be down; they would turn typed text or
    // Ctrl+V into other shortcuts.
    wait_for_modifier_release(Duration::from_millis(1500));
    if (opts.cancelled)() {
        anyhow::bail!("cancelled");
    }

    if opts.method != PasteMethod::ClipboardOnly && foreground_is_elevated_above_us() {
        // Windows drops simulated keystrokes into administrator windows without telling us, so
        // leave the text on the clipboard for the user to paste.
        clipboard::set_text(&text, false)?;
        anyhow::bail!(
            "the focused window runs as administrator, which blocks simulated keystrokes. \
             The text was copied to the clipboard: press Ctrl+V to paste it"
        );
    }

    match opts.method {
        PasteMethod::Typing => type_text(&text)?,
        PasteMethod::ClipboardOnly => {
            open_clipboard().context("clipboard is busy")?.set_text(text)?;
            return Ok(());
        }
        method => {
            let saved = opts.keep_clipboard.then(clipboard::save);
            if matches!(saved, Some(clipboard::Saved::Failed)) {
                // The clipboard cannot be backed up, so it must not be overwritten: type instead.
                log::warn!("paste: the clipboard cannot be saved; typing the text instead");
                type_text(&text)?;
                return finish(opts);
            }
            // A temporary copy is kept out of clipboard history and cloud sync.
            let ours = clipboard::set_text(&text, saved.is_some())?;
            std::thread::sleep(Duration::from_millis(40));
            if let Err(e) = paste_keys(method) {
                // Leave the text on the clipboard so the user can paste it by hand.
                return Err(e.context("the text is on the clipboard: press Ctrl+V to paste it"));
            }
            if let Some(saved) = saved {
                // Give the target application time to read the clipboard before restoring it.
                std::thread::sleep(restore_delay(&text));
                if unsafe { GetClipboardSequenceNumber() } == ours {
                    clipboard::restore(saved);
                } else {
                    log::info!("paste: the clipboard changed after pasting; not restoring it");
                }
            }
        }
    }
    finish(opts)
}

/// Press Enter in the focused application (the "send" key pressed without any dictation).
pub fn send_enter() -> anyhow::Result<()> {
    // The hotkey's modifiers, if it has any, would turn Enter into another shortcut.
    wait_for_modifier_release(Duration::from_millis(800));
    if foreground_is_elevated_above_us() {
        anyhow::bail!("the focused window runs as administrator, which blocks simulated keystrokes");
    }
    send_input(&press_enter()).context("pressing Enter failed")
}

/// Press Enter after the text, if asked to.
fn finish(opts: &InsertOptions) -> anyhow::Result<()> {
    if opts.auto_submit {
        std::thread::sleep(Duration::from_millis(60));
        send_input(&press_enter()).context("pressing Enter failed")?;
    }
    Ok(())
}
