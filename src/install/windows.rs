//! Windows: a folder under %LOCALAPPDATA%\Programs, a Start menu shortcut,
//! and per-user registry keys under HKEY_CURRENT_USER.

use std::{
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegDeleteTreeW,
    RegGetValueW, RegSetKeyValueW,
};

use super::Installed;

/// Notifications name their app by this, and the registry gives it Kith's
/// name and icon. The Start menu shortcut carries it too.
pub const APP_ID: &str = "Kith.App";

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Kith";
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Kith";
const LINKS: &str = r"Software\Classes\kith";

/// What a Kith folder holds, as the release zip lays it out: the program,
/// the ffmpeg it streams with, and their licenses.
const FILES: [&str; 7] = [
    "kith.exe",
    "ffmpeg.exe",
    "README.md",
    "LICENSE.txt",
    "THIRD-PARTY-LICENSES.txt",
    "FFMPEG-LICENSE.txt",
    "FFMPEG-SOURCE.txt",
];

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

/// Where per-user programs go: `%LOCALAPPDATA%\Programs\Kith`.
fn install_dir() -> Option<PathBuf> {
    Some(env_dir("LOCALAPPDATA")?.join(r"Programs\Kith"))
}

fn shortcut() -> Option<PathBuf> {
    Some(env_dir("APPDATA")?.join(r"Microsoft\Windows\Start Menu\Programs\Kith.lnk"))
}

pub fn installed() -> Option<Installed> {
    let location = read(UNINSTALL, Some("InstallLocation"))?;
    let exe = PathBuf::from(location).join("kith.exe");
    if !exe.is_file() {
        return None;
    }
    let version = read(UNINSTALL, Some("DisplayVersion")).and_then(|text| text.parse().ok());
    Some(Installed { exe, version })
}

/// Copies Kith's folder into place, adds the Start menu shortcut and the
/// uninstall entry, and points `kith://` links at the installed program.
pub fn install(exe: &Path, home_args: &[String]) -> Result<PathBuf> {
    let dir = install_dir().context("%LOCALAPPDATA% is not set")?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let source = exe.parent().context("finding Kith's own folder")?;
    for name in FILES {
        let from = source.join(name);
        if from.is_file() {
            put(&from, &dir.join(name))?;
        }
    }
    let installed = dir.join("kith.exe");

    if let Some(link) = shortcut() {
        let args = home_args
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        shortcut::create(&link, &installed, &args)
            .with_context(|| format!("creating the Start menu shortcut {}", link.display()))?;
    }

    let mut uninstall = home_args.to_vec();
    uninstall.push("uninstall".into());
    let size: u64 = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum();
    write(UNINSTALL, Some("DisplayName"), "Kith")?;
    write(UNINSTALL, Some("DisplayVersion"), env!("CARGO_PKG_VERSION"))?;
    write(
        UNINSTALL,
        Some("DisplayIcon"),
        &format!("{},0", installed.display()),
    )?;
    write(UNINSTALL, Some("Publisher"), "Kith")?;
    write(
        UNINSTALL,
        Some("URLInfoAbout"),
        env!("CARGO_PKG_REPOSITORY"),
    )?;
    write(
        UNINSTALL,
        Some("InstallLocation"),
        &dir.display().to_string(),
    )?;
    write(
        UNINSTALL,
        Some("UninstallString"),
        &command_line(&installed, &uninstall),
    )?;
    write_number(UNINSTALL, "EstimatedSize", (size / 1024) as u32)?;
    write_number(UNINSTALL, "NoModify", 1)?;
    write_number(UNINSTALL, "NoRepair", 1)?;

    let mut open = home_args.to_vec();
    open.extend(["open".to_string(), "%1".to_string()]);
    register(&installed, &open)?;
    Ok(installed)
}

/// Copies `from` over `to`. The program being replaced may still be
/// running, which Windows allows renaming but not overwriting, so it moves
/// aside first, and goes for good once it can.
fn put(from: &Path, to: &Path) -> Result<()> {
    if super::same_file(from, to) {
        return Ok(());
    }
    let aside = to.with_file_name(format!(
        "{}.old",
        to.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = fs::remove_file(&aside);
    if to.exists() {
        fs::rename(to, &aside).with_context(|| format!("moving {} aside", to.display()))?;
    }
    fs::copy(from, to).with_context(|| format!("copying {}", to.display()))?;
    let _ = fs::remove_file(&aside);
    Ok(())
}

pub fn uninstall() -> Result<Vec<String>> {
    let mut removed = Vec::new();
    if let Some(link) = shortcut()
        && fs::remove_file(link).is_ok()
    {
        removed.push("the Start menu entry".to_string());
    }
    if read(RUN, Some(RUN_VALUE)).is_some() {
        delete_value(RUN, RUN_VALUE)?;
        removed.push("starting at login".to_string());
    }
    let app = format!(r"Software\Classes\AppUserModelId\{APP_ID}");
    for key in [LINKS, app.as_str(), UNINSTALL] {
        delete_tree(key)?;
    }
    removed.extend([
        "kith:// links".to_string(),
        "the uninstall entry".to_string(),
    ]);
    if let Some(dir) = env_dir("LOCALAPPDATA") {
        let _ = fs::remove_dir_all(dir.join("Kith"));
    }
    if let Some(dir) = install_dir()
        && dir.exists()
    {
        remove_later(&dir)?;
        removed.push(dir.display().to_string());
    }
    Ok(removed)
}

/// Removes `dir` a few seconds from now: Kith runs from it, this program
/// included, and Windows won't delete a program that's running.
fn remove_later(dir: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd.exe")
        .arg("/C")
        .raw_arg(format!(
            "ping -n 6 127.0.0.1 >nul & rmdir /s /q \"{}\"",
            dir.display()
        ))
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("removing {}", dir.display()))?;
    Ok(())
}

pub fn autostart() -> bool {
    read(RUN, Some(RUN_VALUE)).is_some()
}

pub fn set_autostart(exe: &Path, args: &[String], on: bool) -> Result<()> {
    if on {
        write(RUN, Some(RUN_VALUE), &command_line(exe, args))
    } else {
        delete_value(RUN, RUN_VALUE)
    }
}

/// `kith://` opens `kith.exe open <link>`, and notifications from
/// [`APP_ID`] show Kith's name and icon.
pub fn register(exe: &Path, args: &[String]) -> Result<()> {
    write(LINKS, None, "URL:Kith")?;
    write(LINKS, Some("URL Protocol"), "")?;
    write(
        &format!(r"{LINKS}\DefaultIcon"),
        None,
        &format!("\"{}\",0", exe.display()),
    )?;
    write(
        &format!(r"{LINKS}\shell\open\command"),
        None,
        &command_line(exe, args),
    )?;

    let app = format!(r"Software\Classes\AppUserModelId\{APP_ID}");
    write(&app, Some("DisplayName"), "Kith")?;
    if let Some(icon) = icon_file() {
        write(&app, Some("IconUri"), &icon.display().to_string())?;
    }
    Ok(())
}

/// The icon notifications show, which has to be a file: Kith writes its own
/// to %LOCALAPPDATA%\Kith.
fn icon_file() -> Option<std::path::PathBuf> {
    const ICON: &[u8] = include_bytes!("../../assets/icon/kith-256.png");
    let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Kith");
    let file = dir.join("kith.png");
    if std::fs::read(&file).ok().as_deref() != Some(ICON) {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&file, ICON).ok()?;
    }
    Some(file)
}

/// A command line Windows splits back into `exe` and `args`: each one quoted,
/// with backslashes doubled where they come before a quote.
fn command_line(exe: &Path, args: &[String]) -> String {
    std::iter::once(exe.display().to_string())
        .chain(args.iter().cloned())
        .map(|arg| quote(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote(arg: &str) -> String {
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(c);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn write(key: &str, value: Option<&str>, data: &str) -> Result<()> {
    let (key_w, value_w, data_w) = (wide(key), value.map(wide), wide(data));
    // SAFETY: every pointer is to a NUL-terminated buffer that outlives the
    // call, and the size is the data's, in bytes, NUL included.
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            value_w
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            REG_SZ,
            data_w.as_ptr() as *const c_void,
            (data_w.len() * 2) as u32,
        )
    };
    if status != 0 {
        bail!("writing HKEY_CURRENT_USER\\{key} (error {status})");
    }
    Ok(())
}

fn write_number(key: &str, value: &str, number: u32) -> Result<()> {
    let (key_w, value_w) = (wide(key), wide(value));
    // SAFETY: the strings are NUL-terminated, and the data is the u32's four bytes.
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            value_w.as_ptr(),
            REG_DWORD,
            &number as *const u32 as *const c_void,
            4,
        )
    };
    if status != 0 {
        bail!("writing HKEY_CURRENT_USER\\{key}\\{value} (error {status})");
    }
    Ok(())
}

fn read(key: &str, value: Option<&str>) -> Option<String> {
    let (key_w, value_w) = (wide(key), value.map(wide));
    let value_ptr = value_w
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr());
    let mut size = 0u32;
    // SAFETY: the first call only asks for the size; the second fills a
    // buffer of that size.
    unsafe {
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            value_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        if status != 0 {
            return None;
        }
        let mut data = vec![0u16; (size as usize).div_ceil(2)];
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            value_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr() as *mut c_void,
            &mut size,
        );
        if status != 0 {
            return None;
        }
        let text = String::from_utf16_lossy(&data);
        Some(text.trim_end_matches('\0').to_string())
    }
}

fn delete_value(key: &str, value: &str) -> Result<()> {
    let (key_w, value_w) = (wide(key), wide(value));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key_w.as_ptr(), value_w.as_ptr()) };
    // 2 is ERROR_FILE_NOT_FOUND: it was never there.
    if status != 0 && status != 2 {
        bail!("removing HKEY_CURRENT_USER\\{key}\\{value} (error {status})");
    }
    Ok(())
}

/// Deletes a key and everything under it. Gone already is fine.
fn delete_tree(key: &str) -> Result<()> {
    let key_w = wide(key);
    // SAFETY: the key name is NUL-terminated and outlives the call.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_w.as_ptr()) };
    // 2 is ERROR_FILE_NOT_FOUND.
    if status != 0 && status != 2 {
        bail!("removing HKEY_CURRENT_USER\\{key} (error {status})");
    }
    Ok(())
}

/// The Start menu shortcut, through the shell's COM objects.
mod shortcut {
    use std::path::Path;

    use windows::{
        Win32::{
            Storage::EnhancedStorage::PKEY_AppUserModel_ID,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                IPersistFile, StructuredStorage::PROPVARIANT,
            },
            UI::Shell::{IShellLinkW, PropertiesSystem::IPropertyStore, ShellLink},
        },
        core::{HSTRING, Interface},
    };

    use super::APP_ID;

    /// A shortcut to `target` with `args`, carrying Kith's app id so the
    /// taskbar and notifications count it as Kith.
    pub fn create(link: &Path, target: &Path, args: &str) -> windows::core::Result<()> {
        // SAFETY: COM calls on interfaces this function created and holds;
        // initializing COM again on a thread that has it is harmless.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell_link.SetPath(&HSTRING::from(target.as_os_str()))?;
            shell_link.SetArguments(&HSTRING::from(args))?;
            shell_link.SetDescription(&HSTRING::from("Stream your screen to friends"))?;
            if let Some(dir) = target.parent() {
                shell_link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
            }
            // Without it the shortcut still works; notifications just group apart.
            if let Ok(store) = shell_link.cast::<IPropertyStore>() {
                let _ = store
                    .SetValue(&PKEY_AppUserModel_ID, &PROPVARIANT::from(APP_ID))
                    .and_then(|()| store.Commit());
            }
            shell_link
                .cast::<IPersistFile>()?
                .Save(&HSTRING::from(link.as_os_str()), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_survive_windows_splitting() {
        assert_eq!(
            quote(r"C:\Program Files\Kith\kith.exe"),
            r#""C:\Program Files\Kith\kith.exe""#
        );
        assert_eq!(quote(r"C:\dir\"), r#""C:\dir\\""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(
            command_line(
                Path::new(r"C:\Kith\kith.exe"),
                &["open".into(), "%1".into()]
            ),
            r#""C:\Kith\kith.exe" "open" "%1""#
        );
    }

    /// Runs under Wine or on Windows, against the real registry.
    #[test]
    fn the_registry_round_trips() {
        let key = r"Software\Kith\test";
        write(key, Some("value"), "C:\\Kith\\kith.exe --background").unwrap();
        assert_eq!(
            read(key, Some("value")).as_deref(),
            Some("C:\\Kith\\kith.exe --background")
        );
        delete_value(key, "value").unwrap();
        assert_eq!(read(key, Some("value")), None);
        delete_value(key, "value").unwrap();
    }
}
