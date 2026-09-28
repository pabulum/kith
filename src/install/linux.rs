//! Linux: the program in `~/.local/bin`, and XDG desktop entries and icons,
//! which every desktop reads.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::Installed;

/// The icon at the sizes icon themes hold, and scalable.
const ICONS: [(u32, &[u8]); 9] = [
    (16, include_bytes!("../../assets/icon/kith-16.png")),
    (22, include_bytes!("../../assets/icon/kith-22.png")),
    (24, include_bytes!("../../assets/icon/kith-24.png")),
    (32, include_bytes!("../../assets/icon/kith-32.png")),
    (48, include_bytes!("../../assets/icon/kith-48.png")),
    (64, include_bytes!("../../assets/icon/kith-64.png")),
    (128, include_bytes!("../../assets/icon/kith-128.png")),
    (256, include_bytes!("../../assets/icon/kith-256.png")),
    (512, include_bytes!("../../assets/icon/kith-512.png")),
];
const SVG: &[u8] = include_bytes!("../../assets/icon/kith.svg");

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_{name}_HOME`, or `~/{fallback}`.
fn xdg(name: &str, fallback: &str) -> Option<PathBuf> {
    match std::env::var_os(format!("XDG_{name}_HOME")) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => Some(home()?.join(fallback)),
    }
}

/// `$XDG_CONFIG_HOME/autostart/kith.desktop`.
fn autostart_file() -> Option<PathBuf> {
    Some(xdg("CONFIG", ".config")?.join("autostart/kith.desktop"))
}

/// Where the installed program goes: the one user-level place distributions
/// put on PATH (systemd's file-hierarchy).
fn installed_exe() -> Option<PathBuf> {
    Some(home()?.join(".local/bin/kith"))
}

fn applications() -> Option<PathBuf> {
    Some(xdg("DATA", ".local/share")?.join("applications"))
}

fn desktop_file() -> Option<PathBuf> {
    Some(applications()?.join("kith.desktop"))
}

fn hicolor() -> Option<PathBuf> {
    Some(xdg("DATA", ".local/share")?.join("icons/hicolor"))
}

fn icon_files() -> Vec<PathBuf> {
    let Some(hicolor) = hicolor() else {
        return Vec::new();
    };
    ICONS
        .iter()
        .map(|(size, _)| hicolor.join(format!("{size}x{size}/apps/kith.png")))
        .chain([hicolor.join("scalable/apps/kith.svg")])
        .collect()
}

/// Installed means the app-menu entry is there, and the program it starts.
pub fn installed() -> Option<Installed> {
    let exe = installed_exe()?;
    if !desktop_file()?.is_file() || !exe.is_file() {
        return None;
    }
    let version = Command::new(&exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().strip_prefix("kith ")?.parse().ok());
    Some(Installed { exe, version })
}

/// Copies `exe` to `~/.local/bin/kith`, unless it's that already (a
/// symlink counts), and adds Kith to the app menu, with its icon and
/// `kith://` links.
pub fn install(exe: &Path, home_args: &[String]) -> Result<PathBuf> {
    let installed = installed_exe().context("$HOME is not set")?;
    if !super::same_file(exe, &installed) {
        let bin = installed.parent().expect("a file in ~/.local/bin");
        fs::create_dir_all(bin).with_context(|| format!("creating {}", bin.display()))?;
        // Renamed into place, so a Kith running from the old copy keeps its program.
        let fresh = bin.join(".kith.new");
        fs::copy(exe, &fresh).with_context(|| format!("copying Kith to {}", bin.display()))?;
        fs::rename(&fresh, &installed)
            .with_context(|| format!("replacing {}", installed.display()))?;
    }

    let hicolor = hicolor().context("$HOME is not set")?;
    for (file, bytes) in icon_files()
        .iter()
        .zip(ICONS.iter().map(|(_, png)| *png).chain([SVG]))
    {
        write(file, bytes)?;
    }
    // Desktops notice new icons by the theme folder's modification time.
    if let Ok(dir) = fs::File::open(&hicolor) {
        let _ = dir.set_modified(std::time::SystemTime::now());
    }
    if hicolor.join("icon-theme.cache").exists() {
        quietly(
            "gtk-update-icon-cache",
            &["-f", "-t", &hicolor.display().to_string()],
        );
    }

    let mut open = home_args.to_vec();
    open.push("open".into());
    let mut quit = home_args.to_vec();
    quit.push("quit".into());
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Kith\n\
         GenericName=Screen streaming\n\
         Comment=Stream your screen to friends, peer to peer\n\
         Exec={} %u\n\
         Icon=kith\n\
         Terminal=false\n\
         Categories=Network;AudioVideo;Video;\n\
         Keywords=stream;screen;share;friends;watch;live;\n\
         MimeType=x-scheme-handler/kith;\n\
         StartupWMClass=kith\n\
         Actions=quit;\n\
         \n\
         [Desktop Action quit]\n\
         Name=Quit Kith\n\
         Exec={}\n",
        exec_line(&installed, &open),
        exec_line(&installed, &quit),
    );
    let desktop_file = desktop_file().context("$HOME is not set")?;
    write(&desktop_file, entry.as_bytes())?;
    let applications = applications().context("$HOME is not set")?;
    quietly(
        "xdg-mime",
        &["default", "kith.desktop", "x-scheme-handler/kith"],
    );
    quietly(
        "update-desktop-database",
        &[&applications.display().to_string()],
    );
    Ok(installed)
}

pub fn uninstall() -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let mut remove = |file: Option<PathBuf>, what: &str| {
        if let Some(file) = file
            && fs::remove_file(&file).is_ok()
        {
            removed.push(what.to_string());
        }
    };
    remove(desktop_file(), "the app menu entry");
    remove(autostart_file(), "starting at login");
    // Only a copy: a symlink there is someone's own setup.
    let installed =
        installed_exe().filter(|exe| fs::symlink_metadata(exe).is_ok_and(|meta| meta.is_file()));
    let program = installed
        .as_ref()
        .map(|exe| format!("the program ({})", exe.display()));
    remove(installed, program.as_deref().unwrap_or_default());
    let icons: Vec<PathBuf> = icon_files()
        .into_iter()
        .filter(|file| fs::remove_file(file).is_ok())
        .collect();
    if !icons.is_empty() {
        removed.push("the icon".into());
    }
    if let Some(applications) = applications() {
        quietly(
            "update-desktop-database",
            &[&applications.display().to_string()],
        );
    }
    Ok(removed)
}

fn write(file: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(file, bytes).with_context(|| format!("writing {}", file.display()))
}

/// Runs a desktop helper that may not be installed, and doesn't matter much.
fn quietly(program: &str, args: &[&str]) {
    let _ = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn autostart() -> bool {
    // Desktops "turn off" an entry by adding Hidden=true rather than deleting it.
    autostart_file()
        .and_then(|file| fs::read_to_string(file).ok())
        .is_some_and(|entry| !entry.lines().any(|line| line.trim() == "Hidden=true"))
}

pub fn set_autostart(exe: &Path, args: &[String], on: bool) -> Result<()> {
    let file = autostart_file().context("$HOME is not set")?;
    if !on {
        return match fs::remove_file(&file) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).with_context(|| format!("removing {}", file.display()))
            }
            _ => Ok(()),
        };
    }
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Kith\n\
         Comment=Keeps you reachable by friends, from the tray\n\
         Exec={}\n\
         Icon=kith\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_line(exe, args)
    );
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&file, entry).with_context(|| format!("writing {}", file.display()))
}

/// A desktop entry's `Exec` value: arguments quoted where the spec says to,
/// `%` doubled (it starts field codes), and backslashes escaped once more
/// for the file itself.
fn exec_line(exe: &Path, args: &[String]) -> String {
    std::iter::once(exe.display().to_string())
        .chain(args.iter().cloned())
        .map(|arg| exec_arg(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn exec_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    let reserved = |c: char| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c);
    let quoted = if arg.is_empty() || arg.chars().any(reserved) {
        let mut quoted = String::from('"');
        for c in arg.chars() {
            if "\"`$\\".contains(c) {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    } else {
        arg
    };
    quoted.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_lines_quote_what_the_spec_reserves() {
        let exe = Path::new("/home/sam/Kith 0.2/kith");
        let args = [
            "--home".to_string(),
            "/tmp/50%".to_string(),
            "--background".to_string(),
        ];
        assert_eq!(
            exec_line(exe, &args),
            r#""/home/sam/Kith 0.2/kith" --home /tmp/50%% --background"#
        );
        assert_eq!(exec_arg(r#"a"b"#), r#""a\\"b""#);
        assert_eq!(exec_arg("$HOME"), r#""\\$HOME""#);
        assert_eq!(exec_arg(""), r#""""#);
    }
}
