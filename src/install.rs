//! Installing Kith for this user, without admin rights: the program copies
//! itself into place and registers everything itself, so nothing needs a
//! package or an installer.
//!
//! Installed, it's in the Start menu (the app menu on Linux), `kith://`
//! links open it, and it can start at login. Running a newer Kith, say one
//! just downloaded, offers to update the installed one.

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Context, Result, bail};

use crate::config::Home;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as system;
#[cfg(windows)]
use windows as system;

#[cfg(windows)]
pub use windows::APP_ID;

/// What Kith is started with at login: the tray, without the window.
pub const BACKGROUND: &str = "--background";

/// This program's version.
pub fn version() -> Version {
    env!("CARGO_PKG_VERSION")
        .parse()
        .expect("Cargo.toml's version is x.y.z")
}

/// A release's version: three numbers, compared in order. Anything after
/// them (`-beta`) is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        let numbers = text.split(['-', '+']).next().unwrap_or_default();
        let parts: Vec<u64> = numbers
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .with_context(|| format!("{text:?} isn't a version"))?;
        match parts[..] {
            [major, minor, patch] => Ok(Self(major, minor, patch)),
            _ => bail!("{text:?} isn't a version"),
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Kith as installed for this user.
pub struct Installed {
    /// The installed program.
    pub exe: PathBuf,
    /// Its version, when it says.
    pub version: Option<Version>,
}

/// This user's installed Kith, if there is one.
pub fn installed() -> Option<Installed> {
    #[cfg(any(target_os = "linux", windows))]
    return system::installed();
    #[allow(unreachable_code)]
    None
}

/// What installing would do from this program, if it would do anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    /// Kith isn't installed.
    Install,
    /// An older Kith is installed (or one too old to say which it is).
    Update(Option<Version>),
}

/// Whether to offer installing this program: not when it's the installed
/// one, or older than it.
pub fn offer() -> Option<Offer> {
    if !cfg!(any(target_os = "linux", windows)) {
        return None;
    }
    let Some(installed) = installed() else {
        return Some(Offer::Install);
    };
    let exe = std::env::current_exe().ok()?;
    if same_file(&exe, &installed.exe) {
        return None;
    }
    match installed.version {
        Some(theirs) if theirs >= version() => None,
        theirs => Some(Offer::Update(theirs)),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Installs this program for the user, or updates the installed one to it,
/// and returns the installed program: what to start once this one quits.
pub fn install(home: &Home) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding Kith's own program")?;
    let installed = system::install(&exe, &home.args())?;
    if autostart() {
        // Start at login now means the installed program.
        let mut args = home.args();
        args.push(BACKGROUND.into());
        system::set_autostart(&installed, &args, true)?;
    }
    Ok(installed)
}

/// Undoes [`install`] and start at login, and says what it removed. Your
/// identity, friends and settings stay.
pub fn uninstall() -> Result<Vec<String>> {
    #[cfg(any(target_os = "linux", windows))]
    return system::uninstall();
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

/// Whether Kith starts when you log in.
pub fn autostart() -> bool {
    #[cfg(any(target_os = "linux", windows))]
    return system::autostart();
    #[allow(unreachable_code)]
    false
}

/// Starts Kith in the tray when you log in, or stops doing that.
pub fn set_autostart(home: &Home, on: bool) -> Result<()> {
    let (exe, args) = command(home, &[BACKGROUND])?;
    #[cfg(any(target_os = "linux", windows))]
    return system::set_autostart(&exe, &args, on);
    #[allow(unreachable_code)]
    {
        let _ = (exe, args, on);
        bail!("Kith can't start at login on this system")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        let v = |text: &str| text.parse::<Version>().unwrap();
        assert!(v("0.10.0") > v("0.9.3"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.2.0-beta.1"), v("0.2.0"));
        assert_eq!(v(" 0.1.0\n"), v("0.1.0"));
        assert!("0.1".parse::<Version>().is_err());
        assert!("kith 0.1.0".parse::<Version>().is_err());
        assert_eq!(version().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
