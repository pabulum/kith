//! Kith keeps itself up to date from its GitHub releases. That way a friend
//! clicks through Windows' warning about programs from the internet once,
//! when they first run Kith: browsers mark what they download, and Windows
//! asks before starting a marked program, but what Kith downloads itself
//! carries no mark.
//!
//! A release's SHA256SUMS lists the hashes of its files, and SHA256SUMS.sig
//! signs it with an Ed25519 key that stays on the maintainer's PC
//! (`scripts/publish.sh`), never on GitHub. Kith checks that signature
//! against [`KEY`], so taking over the GitHub repository isn't enough to push
//! an update to every friend's Kith.
//!
//! The installed Kith looks for a new release every few hours and downloads
//! it in the background. The new version then installs itself, with its own
//! `kith install`: the next time Kith starts, or when the window's Restart to
//! update is clicked. Never in the middle of a stream.

use std::{
    ffi::OsStr,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use crate::{
    config::Home,
    install::{self, Version},
};

/// The public half of the key `scripts/publish.sh` signs releases with.
/// Replacing it takes a release that carries the new one, signed with the
/// old one.
const KEY: &str = "1af35908082f9ab97f77297d73d41a904a14fec8ccfd3cfb13979ab9a41dae21";

/// How often a running Kith looks for a new release.
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How soon it looks again after failing to, offline say.
const RETRY: Duration = Duration::from_secs(30 * 60);
/// The first look, once Kith has connected after starting.
const FIRST: Duration = Duration::from_secs(60);

/// This system's archive in a release is `kith-<version>-<system><extension>`,
/// and holds one folder, named like the archive without the extension.
const ARCHIVE: Option<(&str, &str)> = if cfg!(all(windows, target_arch = "x86_64")) {
    Some(("windows-x86_64", ".zip"))
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some(("linux-x86_64", ".tar.gz"))
} else {
    None
};

/// The program in that folder.
const PROGRAM: &str = if cfg!(windows) { "kith.exe" } else { "kith" };

/// Where releases come from, and the key their SHA256SUMS is signed with.
pub struct Source {
    url: String,
    key: PublicKey,
}

impl Source {
    /// Releases from `scripts/release.sh`, which sets `KITH_RELEASE_BUILD`,
    /// update from this repository's releases. A Kith built from source
    /// doesn't, since whoever built it has the source. Debug builds take a
    /// stand-in from `KITH_UPDATE_URL` and `KITH_UPDATE_KEY`, which is how the
    /// smoke tests update.
    pub fn get() -> Option<Self> {
        ARCHIVE?;
        #[cfg(debug_assertions)]
        if let (Ok(url), Ok(key)) = (
            std::env::var("KITH_UPDATE_URL"),
            std::env::var("KITH_UPDATE_KEY"),
        ) {
            return Some(Self {
                url,
                key: key.parse().ok()?,
            });
        }
        option_env!("KITH_RELEASE_BUILD").map(|_| Self {
            url: env!("CARGO_PKG_REPOSITORY").to_string(),
            key: KEY.parse().expect("KEY is a public key"),
        })
    }
}

/// Whether this Kith keeps itself up to date: a release (or the tests'
/// stand-in) that is the installed Kith. A copy run from anywhere else
/// offers to install itself instead.
pub fn here() -> bool {
    Source::get().is_some() && install::running_installed()
}

/// Where to read what's new in `version`.
pub fn notes(version: Version) -> String {
    format!("{}/releases/tag/v{version}", env!("CARGO_PKG_REPOSITORY"))
}

/// A release, as its signed SHA256SUMS describes it.
pub struct Release {
    pub version: Version,
    /// Its tag, `v` and the version as SHA256SUMS spells it.
    tag: String,
    /// This system's archive, the folder in it, and the archive's SHA-256.
    archive: String,
    folder: String,
    sha256: Vec<u8>,
}

/// The newest release, when it's newer than this Kith.
pub async fn check(source: &Source) -> Result<Option<Release>> {
    const SMALL: usize = 64 << 10;
    let client = client()?;
    let latest = format!("{}/releases/latest/download", source.url);
    let sums = get(&client, &format!("{latest}/SHA256SUMS"), SMALL).await?;
    let signature = get(&client, &format!("{latest}/SHA256SUMS.sig"), SMALL).await?;
    let release = read_sums(&sums, &signature, &source.key)?;
    Ok(Some(release).filter(|release| release.version > install::version()))
}

/// Downloads `release`, checks it against its SHA256SUMS, and unpacks it,
/// returning the folder it's in. Downloaded already is fine.
pub async fn download(source: &Source, release: &Release) -> Result<PathBuf> {
    let root = install::update_dir().context("finding a folder to download Kith to")?;
    let done = root.join(release.version.to_string());
    if done.join(PROGRAM).is_file() {
        return Ok(done);
    }
    // Something emptied it, which would stop the new download taking its place.
    let _ = fs::remove_dir_all(&done);
    let url = format!(
        "{}/releases/download/{}/{}",
        source.url, release.tag, release.archive
    );
    let archive = get(&client()?, &url, 512 << 20).await?;
    if Sha256::digest(&archive).as_slice() != release.sha256 {
        bail!("{} doesn't match the release's SHA256SUMS", release.archive);
    }

    // Unpacked beside the finished downloads, and renamed in among them once
    // whole: a folder named by a version is complete.
    let partial = root.join(format!(".{}-{}", release.version, std::process::id()));
    let _ = fs::remove_dir_all(&partial);
    fs::create_dir_all(&partial).with_context(|| format!("creating {}", partial.display()))?;
    let (folder, into) = (release.folder.clone(), partial.clone());
    let unpacked = tokio::task::spawn_blocking(move || {
        unpack(&archive, &folder, &into)?;
        anyhow::ensure!(into.join(PROGRAM).is_file(), "it has no {PROGRAM}");
        Ok(())
    })
    .await?;
    if let Err(err) = unpacked {
        let _ = fs::remove_dir_all(&partial);
        return Err(err.context(format!("unpacking {}", release.archive)));
    }
    if let Err(err) = fs::rename(&partial, &done) {
        let _ = fs::remove_dir_all(&partial);
        // Another Kith on this PC, for another identity, got there first.
        if !done.join(PROGRAM).is_file() {
            return Err(err).with_context(|| format!("moving the download to {}", done.display()));
        }
    }
    remove_downloads(|version| version < release.version);
    Ok(done)
}

/// The newest download that's newer than this Kith: its version and folder.
pub fn downloaded() -> Option<(Version, PathBuf)> {
    fs::read_dir(install::update_dir()?)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let version: Version = entry.file_name().to_str()?.parse().ok()?;
            let newer = version > install::version() && entry.path().join(PROGRAM).is_file();
            newer.then(|| (version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
}

/// Installs a downloaded version by running its own `kith install`, which
/// knows what that version is made of, and returns the installed program to
/// start once this Kith quits.
///
/// The download goes either way. Installed, it has nothing left to do, and
/// left in place a download whose program isn't the version it's named for
/// would install again at every start. Failed, the next look fetches it
/// afresh rather than retrying the same files.
pub fn install(download: &(Version, PathBuf), home: &Home) -> Result<PathBuf> {
    let installed = install_from(download, home);
    let _ = fs::remove_dir_all(&download.1);
    installed
}

fn install_from((version, folder): &(Version, PathBuf), home: &Home) -> Result<PathBuf> {
    // Another Kith on this PC, for another identity, installed it already.
    if let Some(installed) = install::installed()
        && installed.version >= Some(*version)
    {
        return Ok(installed.exe);
    }
    let program = folder.join(PROGRAM);
    let mut command = Command::new(&program);
    command
        .args(home.args())
        .arg("install")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
    }
    let output = command
        .output()
        .with_context(|| format!("starting {}", program.display()))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let reason = said
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("it failed");
        bail!(
            "Kith {version} couldn't install itself: {}",
            reason.trim_start_matches("Error: ")
        );
    }
    install::installed()
        .map(|installed| installed.exe)
        .with_context(|| format!("Kith {version} said it installed, but it isn't there"))
}

/// Installs a version downloaded while Kith last ran, and returns the
/// installed program to start instead of this one.
pub fn install_downloaded(home: &Home) -> Option<PathBuf> {
    if home.config().is_ok_and(|config| !config.updates) {
        return None;
    }
    let download = downloaded()?;
    tracing::info!("installing Kith {}, downloaded earlier", download.0);
    install(&download, home)
        .inspect_err(|err| tracing::warn!("{err:#}"))
        .ok()
}

/// Clears up after an update: downloads no newer than this Kith, and on
/// Windows the programs it moved aside.
pub fn tidy() {
    remove_downloads(|version| version <= install::version());
    install::tidy();
}

/// Looks for a new release every few hours while Kith runs, downloading one
/// when there is, and calls `ready` with the newest version downloaded.
/// `updates = false` in config.toml stops it looking.
pub async fn keep_checking(home: Home, ready: impl Fn(Version) + Send + 'static) {
    let Some(source) = Source::get() else { return };
    let mut wait = FIRST;
    loop {
        tokio::time::sleep(wait).await;
        wait = EVERY;
        if home.config().is_ok_and(|config| !config.updates) {
            continue;
        }
        if let Err(err) = fetch(&source).await {
            tracing::info!("looking for a new Kith: {err:#}");
            wait = RETRY;
        }
        if let Some((version, _)) = downloaded() {
            ready(version);
        }
    }
}

/// Downloads the newest release, when it's newer than this Kith and than
/// what's downloaded already.
async fn fetch(source: &Source) -> Result<()> {
    let Some(release) = check(source).await? else {
        return Ok(());
    };
    if downloaded().is_some_and(|(version, _)| version >= release.version) {
        return Ok(());
    }
    tracing::info!("Kith {} is out; downloading it", release.version);
    download(source, &release).await?;
    tracing::info!(
        "Kith {} is downloaded, and installs itself when Kith next starts",
        release.version
    );
    Ok(())
}

/// Checks SHA256SUMS against its signature, then finds this system's
/// archive in it.
fn read_sums(sums: &[u8], signature: &[u8], key: &PublicKey) -> Result<Release> {
    let signature = <&[u8; Signature::LENGTH]>::try_from(signature)
        .map_err(|_| anyhow!("SHA256SUMS.sig isn't a signature"))?;
    key.verify(sums, &Signature::from_bytes(signature))
        .map_err(|_| anyhow!("SHA256SUMS isn't signed with Kith's release key"))?;
    let (system, extension) = ARCHIVE.context("Kith isn't released for this system")?;
    let text = std::str::from_utf8(sums).context("SHA256SUMS isn't text")?;
    let mut found = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (hash, name) = line
            .split_once(' ')
            .with_context(|| format!("SHA256SUMS has a line without a file: {line:?}"))?;
        // sha256sum marks a file it read as binary with a *.
        let name = name.trim_start_matches([' ', '*']);
        let Some(folder) = name.strip_suffix(extension) else {
            continue;
        };
        let Some(version) = folder
            .strip_prefix("kith-")
            .and_then(|rest| rest.strip_suffix(system)?.strip_suffix('-'))
        else {
            continue;
        };
        let sha256 = hex::decode(hash)
            .ok()
            .filter(|sha256| sha256.len() == 32)
            .with_context(|| format!("SHA256SUMS has no proper hash for {name}"))?;
        if found.is_some() {
            bail!("SHA256SUMS lists two archives for this system");
        }
        found = Some(Release {
            version: version.parse()?,
            tag: format!("v{version}"),
            archive: name.to_string(),
            folder: folder.to_string(),
            sha256,
        });
    }
    found.with_context(|| format!("the release has no archive for {system}"))
}

/// An HTTPS client, with iroh's TLS setup: the web's root certificates are
/// built in, so it's the same on every system.
fn client() -> Result<reqwest::Client> {
    let tls = iroh::tls::CaTlsConfig::embedded()
        .client_config(iroh::tls::default_provider())
        .context("setting up TLS")?;
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(concat!("kith/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("setting up downloads")
}

/// Fetches `url`, refusing more than `limit` bytes.
async fn get(client: &reqwest::Client, url: &str, limit: usize) -> Result<Vec<u8>> {
    let fetched = async {
        let mut response = client
            .get(url)
            .timeout(Duration::from_secs(15 * 60))
            .send()
            .await?
            .error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > limit {
                bail!("it's larger than {} MB", limit >> 20);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    };
    fetched.await.with_context(|| format!("downloading {url}"))
}

/// Removes the downloads `stale` picks by version, and ones that never
/// finished.
fn remove_downloads(stale: impl Fn(Version) -> bool) {
    let Some(root) = install::update_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let remove = match entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            Some(version) => stale(version),
            // Unfinished: one that's still going is minutes old.
            None => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(60 * 60)),
        };
        if remove {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Unpacks a release's zip into `into`: the files in its one `folder`.
#[cfg(windows)]
fn unpack(archive: &[u8], folder: &str, into: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if !entry.is_file() {
            bail!("{name} isn't a plain file");
        }
        let path = entry
            .enclosed_name()
            .with_context(|| format!("{name} would land outside its folder"))?;
        write(&mut entry, &place(into, folder, &path)?, false)?;
    }
    Ok(())
}

/// Unpacks a release's .tar.gz into `into`: the files in its one `folder`.
#[cfg(target_os = "linux")]
fn unpack(archive: &[u8], folder: &str, into: &Path) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_dir() || kind.is_pax_global_extensions() {
            continue;
        }
        let path = entry.path()?.into_owned();
        if !kind.is_file() {
            bail!("{} isn't a plain file", path.display());
        }
        let executable = entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0);
        write(&mut entry, &place(into, folder, &path)?, executable)?;
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn unpack(_: &[u8], _: &str, _: &Path) -> Result<()> {
    bail!("Kith isn't released for this system")
}

/// Where the archive's `path` goes in `into`: only files inside its one
/// folder, and only plain names below that, so nothing lands outside `into`.
fn place(into: &Path, folder: &str, path: &Path) -> Result<PathBuf> {
    let mut parts = path.components();
    let inside = parts.next() == Some(Component::Normal(OsStr::new(folder)));
    let rest = parts.as_path();
    let plain = rest
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    if !inside || !plain || rest.as_os_str().is_empty() {
        bail!(
            "the archive has {} outside its {folder} folder",
            path.display()
        );
    }
    Ok(into.join(rest))
}

/// Writes `contents` to a new `file`, making its folder first.
#[cfg_attr(not(unix), allow(unused_variables))]
fn write(contents: &mut impl Read, file: &Path, executable: bool) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, if executable { 0o755 } else { 0o644 });
    let mut out = options
        .open(file)
        .with_context(|| format!("creating {}", file.display()))?;
    std::io::copy(contents, &mut out).with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    #[test]
    fn the_release_key_parses() {
        KEY.parse::<PublicKey>().unwrap();
    }

    /// A SHA256SUMS like `scripts/release.sh` writes, for `version`.
    fn sums(version: &str) -> String {
        let (system, extension) = ARCHIVE.unwrap();
        format!(
            "{}  kith-{version}-linux-other.tar.gz\n\
             {}  kith-{version}-{system}{extension}\n\
             {}  kith-ffmpeg-8.1.3-source.tar\n",
            "11".repeat(32),
            "ab".repeat(32),
            "22".repeat(32),
        )
    }

    #[test]
    fn sha256sums_is_read_only_when_signed_with_the_key() {
        let secret = SecretKey::from_bytes(&[7; 32]);
        let key = secret.public();
        let text = sums("0.9.1");
        let signature = secret.sign(text.as_bytes()).to_bytes();

        let release = read_sums(text.as_bytes(), &signature, &key).unwrap();
        assert_eq!(release.version, "0.9.1".parse().unwrap());
        assert_eq!(release.tag, "v0.9.1");
        let (system, extension) = ARCHIVE.unwrap();
        assert_eq!(release.folder, format!("kith-0.9.1-{system}"));
        assert_eq!(release.archive, format!("kith-0.9.1-{system}{extension}"));
        assert_eq!(release.sha256, [0xab; 32]);

        let tampered = text.replace("abab", "abac");
        assert!(read_sums(tampered.as_bytes(), &signature, &key).is_err());
        let stranger = SecretKey::from_bytes(&[8; 32]).public();
        assert!(read_sums(text.as_bytes(), &signature, &stranger).is_err());
        assert!(read_sums(text.as_bytes(), &signature[..63], &key).is_err());
    }

    #[test]
    fn sha256sums_needs_exactly_one_archive_for_this_system() {
        let secret = SecretKey::from_bytes(&[7; 32]);
        let signed = |text: &str| {
            read_sums(
                text.as_bytes(),
                &secret.sign(text.as_bytes()).to_bytes(),
                &secret.public(),
            )
        };
        let (system, extension) = ARCHIVE.unwrap();
        assert!(signed(&format!("{}  kith-0.2.0-other.zip\n", "ab".repeat(32))).is_err());
        let twice = format!("{}{}", sums("0.2.0"), sums("0.3.0"));
        assert!(signed(&twice).is_err());
        let binary_mode = format!("{} *kith-0.2.0-{system}{extension}\n", "ab".repeat(32));
        assert_eq!(
            signed(&binary_mode).unwrap().version,
            "0.2.0".parse().unwrap()
        );
        let short_hash = format!("abab  kith-0.2.0-{system}{extension}\n");
        assert!(signed(&short_hash).is_err());
    }

    #[test]
    fn only_files_inside_the_folder_are_unpacked() {
        let into = Path::new("/staging");
        let place = |path: &str| place(into, "kith-1.0.0-x", Path::new(path));
        assert_eq!(place("kith-1.0.0-x/kith").unwrap(), into.join("kith"));
        assert_eq!(
            place("kith-1.0.0-x/docs/README.md").unwrap(),
            into.join("docs/README.md")
        );
        for outside in [
            "kith-1.0.0-x",
            "kith-1.0.0-x/../evil",
            "kith-1.0.0-x/docs/../../evil",
            "other/kith",
            "/kith-1.0.0-x/kith",
            "../kith-1.0.0-x/kith",
            "kith",
        ] {
            assert!(place(outside).is_err(), "{outside}");
        }
    }

    /// What a release's archive for this system unpacks to, and refuses.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn archives_unpack_their_folder() {
        let dir = std::env::temp_dir().join(format!("kith-unpack-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let archive = test_archive(&[
            ("kith-1.0.0-x/", None),
            ("kith-1.0.0-x/kith", Some(b"program".as_slice())),
            ("kith-1.0.0-x/licenses/LICENSE.txt", Some(b"MIT".as_slice())),
        ]);
        unpack(&archive, "kith-1.0.0-x", &dir).unwrap();
        assert_eq!(fs::read(dir.join("kith")).unwrap(), b"program");
        assert_eq!(fs::read(dir.join("licenses/LICENSE.txt")).unwrap(), b"MIT");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("kith")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the program is executable");
        }
        fs::remove_dir_all(&dir).unwrap();

        let escaping = test_archive(&[("kith-1.0.0-x/../../evil", Some(b"evil".as_slice()))]);
        assert!(unpack(&escaping, "kith-1.0.0-x", &dir).is_err());
        assert!(!dir.join("../../evil").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A zip with these entries (a directory when there are no contents).
    #[cfg(windows)]
    fn test_archive(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, contents) in entries {
            match contents {
                None => zip.add_directory(*name, options).unwrap(),
                Some(contents) => {
                    zip.start_file(*name, options).unwrap();
                    zip.write_all(contents).unwrap();
                }
            }
        }
        zip.finish().unwrap().into_inner()
    }

    /// A .tar.gz with these entries (a directory when there are no
    /// contents), written the way GNU tar writes, since the tar crate's
    /// builder refuses to write the `..` the tests need.
    #[cfg(target_os = "linux")]
    fn test_archive(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        use std::io::Write;
        let mut tar = Vec::new();
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_mode(0o755);
            header.set_size(contents.map_or(0, |contents| contents.len() as u64));
            if contents.is_none() {
                header.set_entry_type(tar::EntryType::Directory);
            }
            header.set_cksum();
            tar.extend_from_slice(header.as_bytes());
            if let Some(contents) = contents {
                tar.extend_from_slice(contents);
                tar.resize(tar.len().next_multiple_of(512), 0);
            }
        }
        tar.resize(tar.len() + 1024, 0);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }
}
