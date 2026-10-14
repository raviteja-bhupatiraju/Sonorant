//! Wayland: global hotkeys through the XDG GlobalShortcuts portal (KDE Plasma, GNOME 48+, Hyprland...).
//!
//! The app only *suggests* key combinations: the desktop shows them to the user, who confirms or
//! changes them, and the desktop delivers press / release signals. Desktops without the portal
//! can bind keys to `sonorant --cmd speak` etc. in their own settings instead.

use std::sync::Arc;

use anyhow::Result;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::Session;
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{register_app, runtime};
use crate::config::Config;
use crate::hotkey::HotkeySpec;
use crate::platform::{Command, CommandSink, HotkeyErrors};

/// Shortcut ids, by `HotkeyErrors` slot.
const IDS: [&str; 4] = ["speak", "stop", "dictate", "dictate-submit"];

/// One shortcut to bind: (errors slot, description, preferred trigger).
type Wanted = (usize, &'static str, Option<String>);

pub struct PortalHotkeys {
    tx: mpsc::UnboundedSender<Vec<Wanted>>,
    errors: Arc<Mutex<HotkeyErrors>>,
}

impl PortalHotkeys {
    pub fn new(sink: CommandSink) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let errors: Arc<Mutex<HotkeyErrors>> = Arc::default();
        runtime().spawn(run(rx, sink, errors.clone()));
        Ok(Self { tx, errors })
    }

    /// (Re)bind the speak, stop and dictate hotkeys from `cfg`.
    pub fn register(&mut self, cfg: &Config) {
        let mut errors = HotkeyErrors::default();
        let specs = [
            (0, "Read the selected text aloud", Some(&cfg.hotkey)),
            (1, "Stop speaking or cancel dictation", cfg.stop_hotkey.as_ref()),
            (2, "Start or stop dictation (hold for push-to-talk)", cfg.stt_enabled.then_some(&cfg.stt_hotkey)),
            (3, "Finish dictation and press Enter", cfg.stt_submit_hotkey.as_ref().filter(|_| cfg.stt_enabled)),
        ];
        let mut used: Vec<&HotkeySpec> = Vec::new();
        let mut wanted = Vec::new();
        for (slot, description, spec) in specs {
            let Some(spec) = spec else { continue };
            if used.contains(&spec) {
                errors[slot] = Some(format!("{} is already used by another Sonorant shortcut", spec.label()));
            } else if spec.is_plain_escape() {
                // Wayland cannot claim Esc only while speaking; a permanent global Esc would break it everywhere.
                errors[slot] = Some("Plain Esc cannot be a global shortcut on Wayland; choose another key".into());
            } else {
                used.push(spec);
                wanted.push((slot, description, spec.xdg_trigger()));
            }
        }
        *self.errors.lock() = errors;
        let _ = self.tx.send(wanted);
    }

    pub fn errors(&self) -> HotkeyErrors {
        self.errors.lock().clone()
    }

    /// Esc cannot be claimed temporarily on Wayland: cancel with the stop key instead.
    pub fn set_dictating(&mut self, _on: bool, _dictate: &HotkeySpec) {}

    /// Nothing to claim temporarily; never needs polling.
    pub fn set_busy(&mut self, _busy: bool) -> bool {
        false
    }
}

const NO_PORTAL: &str = "This desktop has no global shortcuts portal: bind `sonorant --cmd speak` (or stop, dictate) \
                         to a key in its keyboard settings";

async fn run(mut rx: mpsc::UnboundedReceiver<Vec<Wanted>>, sink: CommandSink, errors: Arc<Mutex<HotkeyErrors>>) {
    register_app().await;
    let portal = async {
        let portal = GlobalShortcuts::new().await?;
        let activated = portal.receive_activated().await?;
        let deactivated = portal.receive_deactivated().await?;
        ashpd::Result::Ok((portal, activated, deactivated))
    };
    let (portal, mut activated, mut deactivated) = match portal.await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("global shortcuts portal unavailable: {e}");
            // Report the problem on every binding attempt.
            while let Some(wanted) = rx.recv().await {
                let mut errs = errors.lock();
                for (slot, ..) in wanted {
                    errs[slot].get_or_insert_with(|| NO_PORTAL.into());
                }
            }
            return;
        }
    };

    let mut session: Option<Session<GlobalShortcuts>> = None;
    let id_command = |id: &str, pressed: bool| match IDS.iter().position(|&i| i == id)? {
        0 => pressed.then_some(Command::Speak),
        1 => pressed.then_some(Command::Stop),
        2 => Some(Command::Dictate(pressed)),
        _ => pressed.then_some(Command::DictateSubmit),
    };
    loop {
        tokio::select! {
            wanted = rx.recv() => {
                let Some(mut wanted) = wanted else { return };
                // Only the latest request matters.
                while let Ok(newer) = rx.try_recv() {
                    wanted = newer;
                }
                if let Some(old) = session.take() {
                    let _ = old.close().await;
                }
                match bind(&portal, &wanted).await {
                    Ok((s, bound)) => {
                        session = Some(s);
                        let mut errs = errors.lock();
                        for (slot, ..) in &wanted {
                            if !bound.iter().any(|b| b == IDS[*slot]) {
                                errs[*slot].get_or_insert_with(|| "Not assigned in the desktop's shortcut dialog".into());
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("binding global shortcuts failed: {e}");
                        let mut errs = errors.lock();
                        for (slot, ..) in &wanted {
                            errs[*slot].get_or_insert_with(|| format!("The desktop refused the shortcut ({e})"));
                        }
                    }
                }
            }
            Some(a) = activated.next() => {
                if let Some(cmd) = id_command(a.shortcut_id(), true) {
                    sink(cmd);
                }
            }
            Some(d) = deactivated.next() => {
                if let Some(cmd) = id_command(d.shortcut_id(), false) {
                    sink(cmd);
                }
            }
        }
    }
}

/// Open a session and bind `wanted`; returns the session and the ids the desktop bound.
async fn bind(portal: &GlobalShortcuts, wanted: &[Wanted]) -> ashpd::Result<(Session<GlobalShortcuts>, Vec<String>)> {
    let session = portal.create_session(Default::default()).await?;
    let shortcuts: Vec<NewShortcut> = wanted
        .iter()
        .map(|(slot, description, trigger)| NewShortcut::new(IDS[*slot], *description).preferred_trigger(trigger.as_deref()))
        .collect();
    let response = async { portal.bind_shortcuts(&session, &shortcuts, None, Default::default()).await?.response() };
    let response = match response.await {
        Ok(r) => r,
        Err(e) => {
            let _ = session.close().await; // ashpd does not close a dropped session
            return Err(e);
        }
    };
    let bound = response.shortcuts().iter().map(|s| {
        log::info!("global shortcut {} -> {}", s.id(), s.trigger_description());
        s.id().to_owned()
    });
    let bound = bound.collect();
    Ok((session, bound))
}
