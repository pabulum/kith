//! Windows: per-user registry keys under HKEY_CURRENT_USER.

use std::{ffi::c_void, path::Path};

use anyhow::{Result, bail};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};

/// Notifications name their app by this, and the registry gives it Kith's
/// name and icon.
pub const APP_ID: &str = "Kith.App";

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Kith";

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
    let classes = r"Software\Classes\kith";
    write(classes, None, "URL:Kith")?;
    write(classes, Some("URL Protocol"), "")?;
    write(
        &format!(r"{classes}\DefaultIcon"),
        None,
        &format!("\"{}\",0", exe.display()),
    )?;
    write(
        &format!(r"{classes}\shell\open\command"),
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
