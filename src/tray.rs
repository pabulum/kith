//! The tray icon, which keeps Kith reachable with its window closed.
//!
//! Windows draws it with `Shell_NotifyIcon`; Linux desktops through the
//! StatusNotifierItem D-Bus protocol (KDE's panel, and GNOME's with the
//! AppIndicator extension). With no tray to show it in, closing the window
//! quits Kith as before.

use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// What the tray asks the app for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Show the window: a click on the icon, or Open in its menu.
    Open,
    /// Quit Kith altogether.
    Quit,
}

/// Called on the tray's own thread, so it mustn't block.
pub type OnAction = Arc<dyn Fn(Action) + Send + Sync>;

/// The icon, for as long as this lives.
pub struct Tray {
    #[cfg(target_os = "linux")]
    _tray: linux::Tray,
    #[cfg(windows)]
    _tray: windows::Tray,
}

/// Puts Kith in the tray. `None` where there's no tray.
///
/// A tray that isn't up yet, as at login, is waited for when `wait` is set;
/// otherwise its absence counts as having none.
#[allow(unused_variables)]
pub fn spawn(on_action: OnAction, wait: bool) -> Option<Tray> {
    #[cfg(target_os = "linux")]
    return linux::spawn(on_action, wait).map(|tray| Tray { _tray: tray });
    #[cfg(windows)]
    return windows::spawn(on_action).map(|tray| Tray { _tray: tray });
    #[allow(unreachable_code)]
    None
}
