//! Everything that differs between Windows and Linux (Wayland and X11): the tray icon, global hotkeys,
//! reading the selection and typing into other applications, the dictation indicator, single
//! instance handling and a few desktop settings. Both implementations expose the same API, so
//! the rest of Sonorant is platform independent.

use std::sync::Arc;

use crate::config::PasteMethod;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

/// What the user asked for, through the tray menu, a global hotkey or (on Linux) `sonorant --cmd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Settings,
    /// Read the selected text aloud.
    Speak,
    SpeakClipboard,
    /// Stop speaking (and cancel a dictation in progress).
    Stop,
    /// The dictation hotkey went down (`true`) or up (`false`).
    Dictate(bool),
    /// Cancel the dictation in progress.
    Cancel,
    Quit,
}

impl Command {
    /// Names accepted by `sonorant --cmd NAME`.
    pub const NAMES: [(&'static str, Command); 9] = [
        ("settings", Command::Settings),
        ("speak", Command::Speak),
        ("clipboard", Command::SpeakClipboard),
        ("stop", Command::Stop),
        // A toggle-mode press; push-to-talk bindings send the press and the release.
        ("dictate", Command::Dictate(true)),
        ("dictate-press", Command::Dictate(true)),
        ("dictate-release", Command::Dictate(false)),
        ("cancel", Command::Cancel),
        ("quit", Command::Quit),
    ];

    pub fn parse(name: &str) -> Option<Command> {
        Self::NAMES.iter().find(|(n, _)| *n == name).map(|&(_, c)| c)
    }

    #[cfg_attr(windows, allow(dead_code))] // used to forward commands on Linux
    pub fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(_, c)| *c == self).map_or("", |(n, _)| n)
    }
}

/// Where commands are delivered (the event loop).
pub type CommandSink = Arc<dyn Fn(Command) + Send + Sync>;

/// How transcribed text is put into the focused application.
pub struct InsertOptions {
    pub method: PasteMethod,
    pub keep_clipboard: bool,
    pub auto_submit: bool,
    pub trailing_space: bool,
}

/// Registration errors for the [speak, stop, dictate] hotkeys, shown in the settings dialog.
pub type HotkeyErrors = [Option<String>; 3];
