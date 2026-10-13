//! Tray icon through the StatusNotifierItem D-Bus protocol (KDE Plasma, most panels; GNOME with
//! the AppIndicator extension).

use std::time::Duration;

use anyhow::{Context, Result};
use ksni::menu::StandardItem;
use ksni::TrayMethods;

use super::{block_on, runtime};
use crate::ui::icon::icon_rgba;
use crate::platform::{Command, CommandSink};

struct Item {
    status: String,
    active: bool,
    sink: CommandSink,
}

/// The app icon as ARGB32 in network byte order.
fn icon(size: u32, active: bool) -> ksni::Icon {
    let data = icon_rgba(size, active).as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| [a, r, g, b]).collect();
    ksni::Icon { width: size as i32, height: size as i32, data }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "sonorant".into()
    }

    fn title(&self) -> String {
        "Sonorant".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        [22, 32, 48].map(|s| icon(s, self.active)).into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "Sonorant".into(), description: self.status.clone(), ..Default::default() }
    }

    /// Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        (self.sink)(Command::Settings);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let item = |label: &str, cmd: Command| -> ksni::MenuItem<Self> {
            StandardItem { label: label.into(), activate: Box::new(move |t: &mut Self| (t.sink)(cmd)), ..Default::default() }.into()
        };
        vec![
            StandardItem { label: self.status.clone(), enabled: false, ..Default::default() }.into(),
            ksni::MenuItem::Separator,
            item("Settings…", Command::Settings),
            item("Speak clipboard", Command::SpeakClipboard),
            item("Stop speaking", Command::Stop),
            ksni::MenuItem::Separator,
            item("Quit Sonorant", Command::Quit),
        ]
    }
}

pub struct Tray {
    handle: ksni::Handle<Item>,
}

impl Tray {
    /// Create the tray icon; menu choices and a left click are delivered to `sink`.
    pub fn new(tooltip: &str, sink: CommandSink) -> Result<Self> {
        let item = Item { status: tooltip.into(), active: false, sink };
        let handle = block_on(Duration::from_secs(5), item.spawn()).context("the tray did not start")??;
        Ok(Self { handle })
    }

    /// Update the status line in the menu, the tooltip and the icon colour.
    pub fn set_status(&mut self, text: &str, active: bool) {
        let text = text.to_owned();
        let handle = self.handle.clone();
        // The update is sent on the runtime; nothing here needs to wait for it.
        runtime().spawn(async move {
            handle
                .update(|t| {
                    t.status = text;
                    t.active = active;
                })
                .await
        });
    }
}
