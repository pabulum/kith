//! Which video player gets a stream, and how it gets it.
//!
//! mpv reads MPEG-TS on stdin. VLC is handed a local HTTP URL instead, because
//! on Windows it can't read stdin. A custom command gets that URL wherever an
//! argument says `{url}`, and stdin otherwise. With neither player installed,
//! the stream opens in the browser, in a page Kith serves locally.

use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Latency;

/// The `player` setting in config.toml.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlayerSetting {
    /// `"auto"` (mpv, else VLC, else the browser), `"mpv"`, `"vlc"` or
    /// `"browser"`.
    Named(String),
    /// A command, which gets a URL wherever an argument says `{url}` and the
    /// stream on stdin otherwise.
    Command(Vec<String>),
}

impl Default for PlayerSetting {
    fn default() -> Self {
        Self::Named("auto".into())
    }
}

/// A player command, ready to start.
#[derive(Debug, PartialEq, Eq)]
pub enum Player {
    /// The stream goes to the command's stdin.
    Stdin(Vec<String>),
    /// The command opens a local URL, put in place of `{url}`.
    Url(Vec<String>),
    /// The default browser opens a local page that plays the stream.
    Browser(Latency),
}

// The app and the browser page link to it; neither exists on Android.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub const GET_VLC: &str = "https://www.videolan.org/vlc/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Mpv,
    Vlc,
}

/// Picks the player for watching `title`.
///
/// `$KITH_PLAYER` overrides config.toml: one of the names, or a command
/// split on whitespace. That's how the smoke tests swap in a headless mpv.
pub fn resolve(setting: &PlayerSetting, latency: Latency, title: &str) -> Result<Player> {
    let setting = match env::var("KITH_PLAYER") {
        Ok(env) if !env.trim().is_empty() => from_env(&env),
        _ => setting.clone(),
    };
    resolve_with(&setting, latency, title, &find)
}

fn from_env(env: &str) -> PlayerSetting {
    let words: Vec<String> = env.split_whitespace().map(String::from).collect();
    match words.as_slice() {
        [name] if ["auto", "mpv", "vlc", "browser"].contains(&name.as_str()) => {
            PlayerSetting::Named(name.clone())
        }
        _ => PlayerSetting::Command(words),
    }
}

/// True when watching would fall back to the browser, which works but lags
/// a real player.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn uses_browser(setting: &PlayerSetting) -> bool {
    matches!(
        resolve(setting, Latency::default(), ""),
        Ok(Player::Browser(_))
    )
}

fn resolve_with(
    setting: &PlayerSetting,
    latency: Latency,
    title: &str,
    find: &dyn Fn(Kind) -> Option<PathBuf>,
) -> Result<Player> {
    let default_mpv = ["--force-window=immediate".to_string(), "-".to_string()];
    match setting {
        PlayerSetting::Named(name) => match name.as_str() {
            "auto" => match (find(Kind::Mpv), find(Kind::Vlc)) {
                (Some(mpv), _) => Ok(mpv_player(&mpv, latency, title, &default_mpv)),
                (None, Some(vlc)) => Ok(vlc_player(&vlc, latency, title)),
                (None, None) if cfg!(target_os = "android") => bail!(
                    "no video player found: use `kith watch --serve 127.0.0.1:8080` and \
                     open that URL in a player app"
                ),
                (None, None) => Ok(Player::Browser(latency)),
            },
            "browser" => Ok(Player::Browser(latency)),
            "mpv" => {
                let mpv = find(Kind::Mpv).unwrap_or_else(|| "mpv".into());
                Ok(mpv_player(&mpv, latency, title, &default_mpv))
            }
            "vlc" => {
                let vlc = find(Kind::Vlc).unwrap_or_else(|| "vlc".into());
                Ok(vlc_player(&vlc, latency, title))
            }
            other => bail!(
                "config.toml has player = {other:?}: use \"auto\", \"mpv\", \"vlc\", \
                 \"browser\", or a command such as [\"mpv\", \"-\"]"
            ),
        },
        PlayerSetting::Command(argv) => {
            let (program, rest) = argv.split_first().context("the player command is empty")?;
            Ok(if argv.iter().any(|arg| arg.contains("{url}")) {
                Player::Url(argv.clone())
            } else if is_mpv(program) {
                mpv_player(Path::new(program), latency, title, rest)
            } else {
                Player::Stdin(argv.clone())
            })
        }
    }
}

fn mpv_player(program: &Path, latency: Latency, title: &str, rest: &[String]) -> Player {
    let flags: &[&str] = match latency {
        // Skip ahead after 100 ms of stall and tell mpv not to buffer.
        Latency::Low => &["--profile=low-latency", "--cache=no"],
        Latency::Normal => &["--profile=low-latency"],
        Latency::Smooth => &[],
    };
    let mut argv = vec![program.to_string_lossy().into_owned()];
    argv.extend(flags.iter().map(|flag| flag.to_string()));
    argv.push(format!("--title=Kith: {title}"));
    argv.extend(rest.iter().cloned());
    Player::Stdin(argv)
}

/// Only options VLC 3 and 4 share: VLC exits on an option it doesn't know.
fn vlc_player(program: &Path, latency: Latency, title: &str) -> Player {
    // VLC's own default is 1000 ms.
    let caching = match latency {
        Latency::Low => 100,
        Latency::Normal => 300,
        Latency::Smooth => 1000,
    };
    Player::Url(vec![
        program.to_string_lossy().into_owned(),
        format!("--network-caching={caching}"),
        format!("--meta-title=Kith: {title}"),
        "--no-video-title-show".into(),
        "--play-and-exit".into(),
        "{url}".into(),
    ])
}

/// `file_stem`, so `mpv.exe` and a full path count, plus mpv's Flatpak.
fn is_mpv(program: &str) -> bool {
    Path::new(program)
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("mpv") || stem == "io.mpv")
}

/// Where a player is installed, if anywhere.
///
/// Next to Kith's own executable comes first, so a portable folder can
/// carry one. Then PATH, including the names Flatpak exports (how SteamOS and
/// other immutable distros install apps). VLC's Windows installer doesn't add
/// itself to PATH, so its default folders are checked too.
fn find(kind: Kind) -> Option<PathBuf> {
    let names: &[&str] = match kind {
        Kind::Mpv => &["mpv", "io.mpv.Mpv"],
        Kind::Vlc => &["vlc", "org.videolan.VLC"],
    };
    let mut dirs: Vec<PathBuf> = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .into_iter()
        .collect();
    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path));
    }
    let mut candidates: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|dir| {
            names
                .iter()
                .map(move |name| dir.join(format!("{name}{}", env::consts::EXE_SUFFIX)))
        })
        .collect();
    if cfg!(windows) && kind == Kind::Vlc {
        for base in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = env::var_os(base) {
                candidates.push(PathBuf::from(base).join(r"VideoLAN\VLC\vlc.exe"));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(kinds: &'static [Kind]) -> impl Fn(Kind) -> Option<PathBuf> {
        move |kind| {
            kinds
                .contains(&kind)
                .then(|| PathBuf::from(format!("/bin/{kind:?}").to_lowercase()))
        }
    }

    fn auto(kinds: &'static [Kind]) -> Result<Player> {
        resolve_with(
            &PlayerSetting::default(),
            Latency::Normal,
            "sam",
            &installed(kinds),
        )
    }

    #[test]
    fn auto_prefers_mpv_on_stdin() {
        let Player::Stdin(argv) = auto(&[Kind::Mpv, Kind::Vlc]).unwrap() else {
            panic!("mpv reads stdin");
        };
        assert_eq!(argv[0], "/bin/mpv");
        assert!(argv.contains(&"--profile=low-latency".to_string()));
        assert!(argv.contains(&"--title=Kith: sam".to_string()));
        assert_eq!(argv.last().unwrap(), "-");
    }

    #[test]
    fn auto_falls_back_to_vlc_on_a_url() {
        let Player::Url(argv) = auto(&[Kind::Vlc]).unwrap() else {
            panic!("VLC gets a URL");
        };
        assert_eq!(argv[0], "/bin/vlc");
        assert!(argv.contains(&"--network-caching=300".to_string()));
        assert_eq!(argv.last().unwrap(), "{url}");
    }

    #[test]
    fn auto_with_nothing_installed_uses_the_browser() {
        assert_eq!(auto(&[]).unwrap(), Player::Browser(Latency::Normal));
    }

    #[test]
    fn a_command_with_a_url_placeholder_gets_a_url() {
        let setting = PlayerSetting::Command(vec!["ffplay".into(), "{url}".into()]);
        let player = resolve_with(&setting, Latency::Low, "sam", &installed(&[])).unwrap();
        assert_eq!(player, Player::Url(vec!["ffplay".into(), "{url}".into()]));
    }

    #[test]
    fn the_old_default_mpv_command_still_gets_latency_flags() {
        let setting = PlayerSetting::Command(vec![
            "mpv".into(),
            "--force-window=immediate".into(),
            "-".into(),
        ]);
        let player = resolve_with(&setting, Latency::Low, "sam", &installed(&[])).unwrap();
        let Player::Stdin(argv) = player else {
            panic!("mpv reads stdin");
        };
        assert_eq!(
            argv,
            [
                "mpv",
                "--profile=low-latency",
                "--cache=no",
                "--title=Kith: sam",
                "--force-window=immediate",
                "-"
            ]
        );
    }

    #[test]
    fn the_environment_takes_names_and_commands() {
        assert_eq!(from_env("vlc"), PlayerSetting::Named("vlc".into()));
        assert_eq!(
            from_env("mpv --vo=null -"),
            PlayerSetting::Command(vec!["mpv".into(), "--vo=null".into(), "-".into()])
        );
    }

    #[test]
    fn an_unknown_name_is_an_error() {
        let setting = PlayerSetting::Named("quicktime".into());
        assert!(resolve_with(&setting, Latency::Normal, "sam", &installed(&[])).is_err());
    }

    #[test]
    fn config_toml_round_trips_both_forms() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Doc {
            player: PlayerSetting,
        }
        for player in [
            PlayerSetting::default(),
            PlayerSetting::Command(vec!["vlc".into(), "{url}".into()]),
        ] {
            let doc = Doc { player };
            let text = toml::to_string(&doc).unwrap();
            assert_eq!(toml::from_str::<Doc>(&text).unwrap(), doc, "{text}");
        }
    }
}
