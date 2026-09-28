//! Linux: XDG desktop entries, which every desktop reads.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

/// `$XDG_CONFIG_HOME/autostart/kith.desktop`.
fn autostart_file() -> Option<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(config.join("autostart/kith.desktop"))
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
