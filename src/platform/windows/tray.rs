//! System tray icon (drawn procedurally, no asset files) and its context menu.

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::ui::icon::icon_rgba;
use crate::platform::{Command, CommandSink};

fn make_icon(size: u32, active: bool) -> Icon {
    Icon::from_rgba(icon_rgba(size, active), size, size).expect("valid icon data")
}

pub struct Tray {
    icon: TrayIcon,
    status_item: MenuItem,
    active: bool,
}

impl Tray {
    /// Create the tray icon; menu choices and a left click are delivered to `sink`.
    pub fn new(tooltip: &str, sink: CommandSink) -> Result<Self> {
        let status_item = MenuItem::new("Sonorant", false, None);
        let items = [
            (MenuItem::new("Settings…", true, None), Command::Settings),
            (MenuItem::new("Speak clipboard", true, None), Command::SpeakClipboard),
            (MenuItem::new("Stop speaking", true, None), Command::Stop),
            (MenuItem::new("Quit Sonorant", true, None), Command::Quit),
        ];

        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        menu.append_items(&[&status_item, &separator, &items[0].0, &items[1].0, &items[2].0, &separator, &items[3].0])?;

        let ids: Vec<_> = items.iter().map(|(item, cmd)| (item.id().clone(), *cmd)).collect();
        let menu_sink = sink.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if let Some(&(_, cmd)) = ids.iter().find(|(id, _)| *id == e.id) {
                menu_sink(cmd);
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                sink(Command::Settings);
            }
        }));

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(tooltip)
            .with_icon(make_icon(32, false))
            .build()?;

        Ok(Self { icon, status_item, active: false })
    }

    /// Update the status line in the menu, the tooltip and (if it changed) the icon colour.
    pub fn set_status(&mut self, text: &str, active: bool) {
        self.status_item.set_text(text);
        let _ = self.icon.set_tooltip(Some(format!("Sonorant — {text}")));
        if active != self.active {
            self.active = active;
            let _ = self.icon.set_icon(Some(make_icon(32, active)));
        }
    }
}
