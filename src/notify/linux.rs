//! Notifications over D-Bus (org.freedesktop.Notifications), which every
//! Linux desktop's notification server speaks.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::OnceCell;
use zbus::zvariant::{StructureBuilder, Value};

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// One session bus connection for the whole process, or why there isn't one.
async fn session() -> Result<&'static zbus::Connection> {
    static SESSION: OnceCell<Result<zbus::Connection, String>> = OnceCell::const_new();
    SESSION
        .get_or_init(|| async {
            zbus::Connection::session()
                .await
                .map_err(|err| err.to_string())
        })
        .await
        .as_ref()
        .map_err(|err| anyhow::anyhow!("no session bus: {err}"))
}

/// Shows a notification, and with `watch` a Watch button. Waits for it to
/// be clicked or dismissed then, and says whether it was Watch (or the
/// notification itself).
pub async fn notify(title: &str, body: &str, watch: bool) -> Result<bool> {
    let proxy = NotificationsProxy::new(session().await?)
        .await
        .context("reaching the notification server")?;
    // Subscribed before the notification exists, so a quick click isn't missed.
    let mut invoked = proxy.receive_action_invoked().await?;
    let mut closed = proxy.receive_notification_closed().await?;

    let mut hints = HashMap::new();
    // KDE groups by it and takes the app's name and icon from it.
    hints.insert("desktop-entry", Value::from("kith"));
    if let Some(image) = image() {
        hints.insert("image-data", image);
    }
    let actions: &[&str] = if watch {
        &["default", "Watch", "watch", "Watch"]
    } else {
        &[]
    };
    let id = proxy
        .notify("Kith", 0, "", title, body, actions, hints, -1)
        .await
        .context("showing a notification")?;
    if !watch {
        return Ok(false);
    }
    loop {
        tokio::select! {
            Some(signal) = invoked.next() => {
                let args = signal.args()?;
                if args.id == id {
                    return Ok(matches!(args.action_key.as_str(), "watch" | "default"));
                }
            }
            Some(signal) = closed.next() => {
                if signal.args()?.id == id {
                    return Ok(false);
                }
            }
            else => return Ok(false),
        }
    }
}

/// Kith's icon as raw pixels (the `(iiibiiay)` of the spec), so it shows
/// even when no icon theme has it.
fn image() -> Option<Value<'static>> {
    let icon =
        eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon/kith-64.png")).ok()?;
    let structure = StructureBuilder::new()
        .add_field(icon.width as i32)
        .add_field(icon.height as i32)
        .add_field(icon.width as i32 * 4)
        .add_field(true)
        .add_field(8i32)
        .add_field(4i32)
        .add_field(icon.rgba)
        .build()
        .ok()?;
    Some(Value::from(structure))
}
