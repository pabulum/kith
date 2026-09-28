//! Hooking Kith into the desktop, per user and without admin rights: the
//! program registers everything itself, so nothing needs a package.
//!
//! So far that's starting at login, plus on Windows the `kith://` link
//! handler and the app identity that notifications carry.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::Home;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::APP_ID;

/// What Kith is started with at login: the tray, without the window.
pub const BACKGROUND: &str = "--background";

/// Whether Kith starts when you log in.
pub fn autostart() -> bool {
    #[cfg(target_os = "linux")]
    return linux::autostart();
    #[cfg(windows)]
    return windows::autostart();
    #[allow(unreachable_code)]
    false
}

/// Starts Kith in the tray when you log in, or stops doing that.
pub fn set_autostart(home: &Home, on: bool) -> Result<()> {
    let (exe, args) = command(home, &[BACKGROUND])?;
    #[cfg(target_os = "linux")]
    return linux::set_autostart(&exe, &args, on);
    #[cfg(windows)]
    return windows::set_autostart(&exe, &args, on);
    #[allow(unreachable_code)]
    {
        let _ = (exe, args, on);
        anyhow::bail!("Kith can't start at login on this system")
    }
}

/// Points `kith://` links, and the Watch button on notifications, at this
/// program. Cheap, so it runs every time Kith starts, which keeps the links
/// working when the program moves.
#[cfg(windows)]
pub fn register(home: &Home) -> Result<()> {
    let (exe, args) = command(home, &["open", "%1"])?;
    windows::register(&exe, &args)
}

/// This program, and the arguments that start it on `home` with `args`.
fn command(home: &Home, args: &[&str]) -> Result<(PathBuf, Vec<String>)> {
    let exe = std::env::current_exe().context("finding Kith's own program")?;
    let mut all = home.args();
    all.extend(args.iter().map(|arg| arg.to_string()));
    Ok((exe, all))
}
