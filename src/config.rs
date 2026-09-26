//! On-disk state: the identity key, the friends list, and settings.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

/// The directory pstream keeps its state in.
///
/// `--home`/`$PSTREAM_HOME` when given, else `$XDG_CONFIG_HOME/pstream`, else
/// `~/.config/pstream`. Two homes are two identities, which is how one machine
/// plays both ends in tests.
#[derive(Clone, Debug)]
pub struct Home(PathBuf);

impl Home {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let dir = match explicit {
            Some(dir) => dir,
            None => match std::env::var_os("XDG_CONFIG_HOME") {
                Some(base) if !base.is_empty() => PathBuf::from(base).join("pstream"),
                _ => PathBuf::from(std::env::var_os("HOME").context("$HOME is not set")?)
                    .join(".config/pstream"),
            },
        };
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    pub fn dir(&self) -> &Path {
        &self.0
    }

    /// The control socket `pstream up` listens on.
    ///
    /// In `$XDG_RUNTIME_DIR`, named by a hash of the home, because Unix socket
    /// paths cap at 108 bytes and a home can be arbitrarily deep.
    pub fn socket_path(&self) -> PathBuf {
        use std::hash::{Hash, Hasher};
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) if !runtime.is_empty() => {
                let home = fs::canonicalize(&self.0).unwrap_or_else(|_| self.0.clone());
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                home.hash(&mut hasher);
                PathBuf::from(runtime).join(format!("pstream-{:016x}.sock", hasher.finish()))
            }
            _ => self.0.join("pstream.sock"),
        }
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
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
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

const CONFIG_HEADER: &str = "\
# pstream settings. `pstream friend ...` rewrites this file; comments you add are not kept.
#
# player:  receives the stream as MPEG-TS on stdin.
# capture: writes MPEG-TS (H.264/H.265 video, AAC audio) to stdout for `pstream live`.
# latency: default for watching: low | normal | smooth.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub player: Vec<String>,
    pub capture: Vec<String>,
    pub latency: Latency,
    #[serde(rename = "friend")]
    pub friends: Vec<Friend>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            player: ["mpv", "--force-window=immediate", "-"]
                .map(String::from)
                .to_vec(),
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
    /// Skip ahead after 100 ms of stall and tell mpv not to buffer.
    Low,
    /// Skip after 500 ms; mpv's low-latency profile.
    #[default]
    Normal,
    /// Ride out 2 s of stall and let mpv buffer normally.
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

    pub fn mpv_flags(self) -> &'static [&'static str] {
        match self {
            Self::Low => &["--profile=low-latency", "--cache=no"],
            Self::Normal => &["--profile=low-latency"],
            Self::Smooth => &[],
        }
    }
}
