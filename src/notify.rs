//! Desktop notifications: "sam is live", with a Watch button.
//!
//! Linux talks to the desktop's notification server over D-Bus, and falls
//! back to `notify-send` without a session bus. Windows shows a toast, whose
//! Watch button is a `kith://watch/<code>` link that comes back through
//! `kith open`.

use iroh::EndpointId;
use tracing::{debug, info};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Tells the user `name` just went live. True if they clicked Watch, when
/// that comes back here rather than as a link.
pub async fn live(name: &str, code: EndpointId) -> bool {
    let title = format!("{name} is live");
    let body = "Watch opens the stream.";
    #[cfg(windows)]
    {
        let link = crate::link::Link::Watch(code);
        if let Err(err) = windows::toast(&title, body, Some(&link)).await {
            info!("{name} is live; `kith watch {name}` opens the stream ({err:#})");
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = code;
        #[cfg(target_os = "linux")]
        match linux::notify(&title, body, true).await {
            Ok(clicked) => return clicked,
            Err(err) => debug!("no notification over D-Bus ({err:#}); trying notify-send"),
        }
        notify_send(&title, body, true).await.unwrap_or_else(|| {
            info!("{name} is live; `kith watch {name}` opens the stream");
            false
        })
    }
}

/// A notification with nothing to click.
pub async fn tell(title: &str, body: &str) {
    #[cfg(windows)]
    if let Err(err) = windows::toast(title, body, None).await {
        debug!("couldn't show a notification ({err:#}): {title}");
    }
    #[cfg(target_os = "linux")]
    if linux::notify(title, body, false).await.is_err() {
        notify_send(title, body, false).await;
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    info!("{title}. {body}");
}

/// `notify-send`, for desktops without a session bus Kith can reach (and
/// the smoke test, whose fake one clicks Watch). `None` when it can't run.
#[cfg(not(windows))]
async fn notify_send(title: &str, body: &str, watch: bool) -> Option<bool> {
    let mut command = tokio::process::Command::new("notify-send");
    command.args(["--app-name=Kith", "--icon=kith"]);
    if watch {
        command.args(["--action=watch=Watch", "--wait"]);
    }
    let output = command.arg(title).arg(body).output().await;
    match output {
        Ok(output) => Some(String::from_utf8_lossy(&output.stdout).trim() == "watch"),
        Err(err) => {
            tracing::warn!("notify-send failed ({err})");
            None
        }
    }
}
