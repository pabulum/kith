//! Which video player gets a stream, and how it gets it.
//!
//! mpv reads MPEG-TS on stdin. VLC is handed a local HTTP URL instead, because
//! on Windows it can't read stdin. A custom command gets that URL wherever an
//! argument says `{url}`, and stdin otherwise.

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
    /// `"auto"` (mpv if it's installed, else VLC), `"mpv"` or `"vlc"`.
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
}

pub const GET_VLC: &str = "https://www.videolan.org/vlc/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Mpv,
    Vlc,
}

/// Picks the player for watching `title`.
///
/// `$PSTREAM_PLAYER` (split on whitespace) overrides config.toml, which is how
/// the smoke test swaps in a headless mpv.
pub fn resolve(setting: &PlayerSetting, latency: Latency, title: &str) -> Result<Player> {
    let setting = match env::var("PSTREAM_PLAYER") {
        Ok(env) if !env.trim().is_empty() => {
            PlayerSetting::Command(env.split_whitespace().map(String::from).collect())
        }
        _ => setting.clone(),
    };
    resolve_with(&setting, latency, title, &find)
}

/// True when [`resolve`] would find something to play with.
pub fn available(setting: &PlayerSetting) -> bool {
    resolve(setting, Latency::default(), "").is_ok()
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
                (None, None) => bail!(
                    "no video player found: install VLC ({GET_VLC}) or mpv, or set `player` \
                     in config.toml"
                ),
            },
            "mpv" => {
                let mpv = find(Kind::Mpv).unwrap_or_else(|| "mpv".into());
                Ok(mpv_player(&mpv, latency, title, &default_mpv))
            }
            "vlc" => {
                let vlc = find(Kind::Vlc).unwrap_or_else(|| "vlc".into());
                Ok(vlc_player(&vlc, latency, title))
            }
            other => bail!(
                "config.toml has player = {other:?}: use \"auto\", \"mpv\", \"vlc\", or a \
                 command such as [\"mpv\", \"-\"]"
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
    argv.push(format!("--title=pstream: {title}"));
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
        format!("--meta-title=pstream: {title}"),
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
/// Next to pstream's own executable comes first, so a portable folder can
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
        assert!(argv.contains(&"--title=pstream: sam".to_string()));
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
    fn auto_with_nothing_installed_says_what_to_get() {
        let err = auto(&[]).unwrap_err().to_string();
        assert!(err.contains(GET_VLC), "{err}");
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
                "--title=pstream: sam",
                "--force-window=immediate",
                "-"
            ]
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
