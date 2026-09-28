//! The tray on Linux: a StatusNotifierItem, over D-Bus.

use ksni::{
    MenuItem,
    blocking::{Handle, TrayMethods},
    menu::StandardItem,
};
use tracing::info;

use super::{Action, OnAction};

pub struct Tray(Handle<KithTray>);

impl Drop for Tray {
    fn drop(&mut self) {
        self.0.shutdown().wait();
    }
}

pub fn spawn(on_action: OnAction, wait: bool) -> Option<Tray> {
    let tray = KithTray {
        on_action,
        icons: icons(),
    };
    match tray.assume_sni_available(wait).spawn() {
        Ok(handle) => Some(Tray(handle)),
        Err(err) => {
            info!("no tray to show Kith in ({err}), so closing the window quits it");
            None
        }
    }
}

pub struct KithTray {
    on_action: OnAction,
    icons: Vec<ksni::Icon>,
}

impl ksni::Tray for KithTray {
    fn id(&self) -> String {
        "kith".into()
    }

    fn title(&self) -> String {
        "Kith".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Kith".into(),
            description: "Friends can reach you while Kith runs".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_action)(Action::Open);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open Kith".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_action)(Action::Open)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Kith".into(),
                activate: Box::new(|tray: &mut Self| (tray.on_action)(Action::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    /// The panel restarting (or not being up yet at login) isn't a reason to
    /// give up: the icon comes back when it does.
    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        true
    }
}

/// The icon at the sizes panels draw, as the ARGB32 StatusNotifierItem takes.
fn icons() -> Vec<ksni::Icon> {
    let pngs: [&[u8]; 4] = [
        include_bytes!("../../assets/icon/kith-22.png"),
        include_bytes!("../../assets/icon/kith-32.png"),
        include_bytes!("../../assets/icon/kith-48.png"),
        include_bytes!("../../assets/icon/kith-64.png"),
    ];
    pngs.into_iter()
        .filter_map(|png| eframe::icon_data::from_png_bytes(png).ok())
        .map(|icon| ksni::Icon {
            width: icon.width as i32,
            height: icon.height as i32,
            data: icon
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|&[r, g, b, a]| [a, r, g, b])
                .collect(),
        })
        .collect()
}
