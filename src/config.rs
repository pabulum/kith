//! On-disk state: the identity key, the friends list, and settings.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

use crate::player::PlayerSetting;

/// The directory pstream keeps its state in.
///
/// `--home`/`$PSTREAM_HOME` when given, else `$XDG_CONFIG_HOME/pstream`, else
/// `~/.config/pstream` (`%APPDATA%\pstream` on Windows). Two homes are two
/// identities, which is how one machine plays both ends in tests.
#[derive(Clone, Debug)]
pub struct Home(PathBuf);

impl Home {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let dir = match explicit {
            Some(dir) => dir,
            None => default_dir()?,
        };
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    pub fn dir(&self) -> &Path {
        &self.0
    }

    /// The control socket `pstream up` listens on: a Unix socket, or a named
    /// pipe on Windows.
    ///
    /// Named by a hash of the home, because Unix socket paths cap at 108 bytes
    /// and a home can be arbitrarily deep, and pipe names can't hold a path.
    pub fn socket_path(&self) -> PathBuf {
        let name = format!("pstream-{:016x}", self.hash());
        if cfg!(windows) {
            return PathBuf::from(format!(r"\\.\pipe\{name}"));
        }
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) if !runtime.is_empty() => {
                PathBuf::from(runtime).join(format!("{name}.sock"))
            }
            _ => self.0.join("pstream.sock"),
        }
    }

    fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let home = fs::canonicalize(&self.0).unwrap_or_else(|_| self.0.clone());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        home.hash(&mut hasher);
        hasher.finish()
    }

    fn config_path(&self) -> PathBuf {
        self.0.join("config.toml")
    }

    /// Loads the identity key, creating one on first run.
    ///
    /// The key *is* the identity: friends store its public half, so losing or
    /// regenerating it means every friend has to re-add you.
    pub fn secret(&self) -> Result<SecretKey> {
        let path = self.0.join("secret.key");
        match fs::read_to_string(&path) {
            Ok(text) => SecretKey::from_str(text.trim())
                .with_context(|| format!("{} is not a valid key", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let secret = SecretKey::generate();
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                // Windows keeps %APPDATA% private to the user already.
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                let mut file = options
                    .open(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                file.write_all(hex::encode(secret.to_bytes()).as_bytes())?;
                Ok(secret)
            }
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn config(&self) -> Result<Config> {
        let path = self.config_path();
        match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Written out on first run so the defaults are discoverable and editable.
                let config = Config::default();
                self.save(&config)?;
                Ok(config)
            }
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        let path = self.config_path();
        let text = format!("{CONFIG_HEADER}\n{}", toml::to_string_pretty(config)?);
        // Write-then-rename so a crash never leaves a half-written friends list.
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(unix)]
fn default_dir() -> Result<PathBuf> {
    Ok(match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) if !base.is_empty() => PathBuf::from(base).join("pstream"),
        _ => PathBuf::from(std::env::var_os("HOME").context("$HOME is not set")?)
            .join(".config/pstream"),
    })
}

#[cfg(windows)]
fn default_dir() -> Result<PathBuf> {
    let appdata = std::env::var_os("APPDATA").context("%APPDATA% is not set")?;
    Ok(PathBuf::from(appdata).join("pstream"))
}

const CONFIG_HEADER: &str = "\
# pstream settings. `pstream friend ...` rewrites this file; comments you add are not kept.
#
# player:  \"auto\" (mpv if it's installed, else VLC), \"mpv\", \"vlc\", or a command.
#          A command gets the stream on stdin, or a local URL wherever an
#          argument says {url}, e.g. [\"vlc\", \"{url}\"].
# capture: writes MPEG-TS (H.264/H.265 video, AAC audio) to stdout for `pstream live`.
# latency: default for watching: low | normal | smooth.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub player: PlayerSetting,
    pub capture: Vec<String>,
    pub latency: Latency,
    #[serde(rename = "friend")]
    pub friends: Vec<Friend>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            player: PlayerSetting::default(),
            capture: DEFAULT_CAPTURE.map(String::from).to_vec(),
            latency: Latency::Normal,
            friends: Vec::new(),
        }
    }
}

/// gpu-screen-recorder through the desktop portal (KDE/GNOME show a picker the
/// first time; `-restore-portal-session` reuses the choice afterwards).
///
/// Scaled to fit 1080p because upload is the budget: 1080p60 HEVC at 8 Mbps is
/// four viewers on a 40 Mbps uplink, where 4K would be one. `-keyint 1` makes
/// every MoQ group one second long, which bounds how long a joining or
/// skipping viewer waits for a keyframe, at some bitrate cost. AAC because the
/// MPEG-TS importer takes AAC/MP2/AC-3, not Opus.
const DEFAULT_CAPTURE: [&str; 23] = [
    "gpu-screen-recorder",
    "-w",
    "portal",
    "-restore-portal-session",
    "yes",
    "-c",
    "mpegts",
    "-k",
    "hevc",
    "-s",
    "1920x1080",
    "-f",
    "60",
    "-keyint",
    "1",
    "-bm",
    "cbr",
    "-q",
    "8000",
    "-ac",
    "aac",
    "-a",
    "default_output",
];

impl Config {
    /// Adds a friend by the code their pstream shows them.
    pub fn add_friend(
        &mut self,
        name: &str,
        code: &str,
        auto_open: bool,
        own: EndpointId,
    ) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("give your friend a name");
        }
        let id = EndpointId::from_str(code.trim()).with_context(|| {
            format!(
                "{:?} isn't a pstream code: ask your friend for the 64-character code \
                 pstream shows them",
                code.trim()
            )
        })?;
        if id == own {
            bail!("that's your own code; ask your friend for theirs");
        }
        if let Some(existing) = self.friends.iter().find(|f| f.name == name) {
            bail!("you already have a friend named {:?}", existing.name);
        }
        if let Some(existing) = self.friends.iter().find(|f| f.id().ok() == Some(id)) {
            bail!("that code is already saved as {:?}", existing.name);
        }
        self.friends.push(Friend {
            name: name.to_string(),
            code: id.to_string(),
            auto_open,
        });
        Ok(())
    }

    pub fn remove_friend(&mut self, name: &str) -> Result<()> {
        let before = self.friends.len();
        self.friends.retain(|f| f.name != name);
        if self.friends.len() == before {
            bail!("no friend named {name:?}");
        }
        Ok(())
    }

    pub fn friend_mut(&mut self, name: &str) -> Result<&mut Friend> {
        self.friends
            .iter_mut()
            .find(|f| f.name == name)
            .with_context(|| format!("no friend named {name:?}"))
    }

    pub fn friend(&self, name_or_code: &str) -> Option<&Friend> {
        self.friends
            .iter()
            .find(|friend| friend.name == name_or_code || friend.code == name_or_code)
    }

    /// Resolves a friend's name, or accepts a raw code.
    pub fn resolve(&self, name_or_code: &str) -> Result<(String, EndpointId)> {
        if let Some(friend) = self.friend(name_or_code) {
            return Ok((friend.name.clone(), friend.id()?));
        }
        match EndpointId::from_str(name_or_code) {
            Ok(id) => Ok((id.fmt_short().to_string(), id)),
            Err(_) => bail!("no friend named {name_or_code:?} (see `pstream friend ls`)"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Friend {
    pub name: String,
    /// Their iroh endpoint id: the public half of their `secret.key`.
    pub code: String,
    /// Open the player as soon as they go live, instead of notifying.
    #[serde(default)]
    pub auto_open: bool,
}

impl Friend {
    pub fn id(&self) -> Result<EndpointId> {
        EndpointId::from_str(&self.code)
            .with_context(|| format!("friend {:?} has an invalid code", self.name))
    }
}

/// How far behind live a viewer sits, traded against smoothness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Latency {
    /// Skip ahead after 100 ms of stall, and have the player barely buffer.
    Low,
    /// Skip after 500 ms, with a low-latency player setup.
    #[default]
    Normal,
    /// Ride out 2 s of stall, and let the player buffer normally.
    Smooth,
}

impl Latency {
    /// How long the exporter waits on a stalled group before skipping to a newer one.
    pub fn max_age(self) -> Duration {
        match self {
            Self::Low => Duration::from_millis(100),
            Self::Normal => Duration::from_millis(500),
            Self::Smooth => Duration::from_secs(2),
        }
    }
}
