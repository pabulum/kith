//! gpu-screen-recorder, which records the screen on Linux: installed, or
//! else its Flatpak, which is how SteamOS and other immutable systems get it.

use std::process::{Command, Output, Stdio};

use anyhow::{Result, anyhow, bail};

use super::{Backend, Codec, Device, Encoder, Recorder};

const PROGRAM: &str = "gpu-screen-recorder";

/// gpu-screen-recorder from Flathub. Portal capture and app audio work from
/// inside its sandbox, and `flatpak run` passes the stream through stdout.
const FLATPAK: [&str; 4] = [
    "flatpak",
    "run",
    "--command=gpu-screen-recorder",
    "com.dec05eba.gpu_screen_recorder",
];

/// What [`AUTO`](super::AUTO) picks from, in order. The graphics card comes
/// before the processor, and H.264 before HEVC, because a friend with no video
/// player installed watches in the browser, and every browser plays H.264.
pub(super) const AUTO_ORDER: [&str; 3] = ["h264", "hevc", "h264_software"];

/// What Discord's voice chat is called in PipeWire: it plays through its own
/// voice engine, which names itself, not Discord.
const DISCORD_VOICE: &str = "WEBRTC VoiceEngine";

// Windows test builds compile this module for its tests, but record with ffmpeg.
#[cfg_attr(windows, allow(dead_code))]
pub(super) fn detect() -> Result<Recorder> {
    let (output, flatpak) = match info(&[PROGRAM]) {
        Ok(output) => (output, false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => match info(&FLATPAK) {
            Ok(output) if output.status.success() => (output, true),
            _ => {
                return Err(anyhow!(
                    "{PROGRAM} (the screen recorder) isn't installed: get it from your \
                     distribution, or from Flathub (com.dec05eba.gpu_screen_recorder)"
                ));
            }
        },
        Err(err) => return Err(crate::node::spawn_error(PROGRAM, "capture", err)),
    };
    if !output.status.success() {
        bail!(
            "`{PROGRAM} --info` failed ({}), so Kith can't tell which encoders work \
             here; run it in a terminal to see why",
            output.status
        );
    }
    let mut recorder = parse(&String::from_utf8_lossy(&output.stdout));
    recorder.backend = Backend::Gsr { flatpak };
    Ok(recorder)
}

#[cfg_attr(windows, allow(dead_code))]
fn info(launcher: &[&str]) -> std::io::Result<Output> {
    Command::new(launcher[0])
        .args(&launcher[1..])
        .arg("--info")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
}

/// Reads `--info`: the graphics card's maker, whether it can record apps
/// separately (it needs PipeWire), and its codecs.
pub(super) fn parse(info: &str) -> Recorder {
    let mut section = "";
    let mut maker = None;
    let mut can_silence = false;
    let mut codecs = Vec::new();
    for line in info.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("section=") {
            section = name;
        } else if section == "system_info" && line == "supports_app_audio|yes" {
            can_silence = true;
        } else if section == "gpu_info"
            && let Some(vendor) = line.strip_prefix("vendor|")
        {
            maker = Some(match vendor {
                "amd" => "AMD".to_string(),
                "intel" => "Intel".to_string(),
                "nvidia" => "NVIDIA".to_string(),
                other => other.to_string(),
            });
        } else if section == "video_codecs" {
            codecs.push(line);
        }
    }
    let mut encoders: Vec<Encoder> = codecs
        .into_iter()
        .filter_map(|name| encoder(name, &maker))
        .collect();
    encoders.sort_by_key(Encoder::rank);
    Recorder {
        encoders,
        can_silence,
        backend: Backend::Gsr { flatpak: false },
    }
}

/// One line of `--info`'s `video_codecs` section, if the stream can carry it.
///
/// HDR variants are left out: they need direct monitor capture, and Kith
/// records through the desktop portal.
fn encoder(name: &str, maker: &Option<String>) -> Option<Encoder> {
    let (rest, vulkan) = match name.strip_suffix("_vulkan") {
        Some(rest) => (rest, true),
        None => (name, false),
    };
    let (rest, cpu) = match rest.strip_suffix("_software") {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let codec = match rest {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "hevc_10bit" => Codec::Hevc10,
        _ => return None,
    };
    (!(cpu && vulkan)).then(|| Encoder {
        name: name.to_string(),
        codec,
        device: if cpu {
            Device::Cpu
        } else {
            Device::Gpu(maker.clone())
        },
        route: vulkan.then_some("Vulkan (experimental)"),
    })
}

/// The command that streams with `encoder`, leaving `silence` out of the sound.
///
/// It records through the desktop portal (KDE and GNOME show a picker the
/// first time; `-restore-portal-session` reuses the choice afterwards). It
/// scales to fit 1080p because upload is the budget: 1080p60 at 8 Mbps is four
/// viewers on a 40 Mbps uplink, where 4K would be one. `-keyint 1` makes every
/// MoQ group one second long, which bounds how long a joining or skipping
/// viewer waits for a keyframe, at some bitrate cost. AAC because every player
/// plays it, the browser page included.
pub(super) fn command(encoder: &Encoder, silence: &[String], flatpak: bool) -> Vec<String> {
    let cpu = encoder.device == Device::Cpu;
    // Processor encoding is `-k h264 -encoder cpu`, not a codec name of its own.
    let codec = match encoder.name.strip_suffix("_software") {
        Some(codec) if cpu => codec,
        _ => &encoder.name,
    };
    let audio = audio(silence);
    let launcher: &[&str] = if flatpak { &FLATPAK } else { &[PROGRAM] };
    let mut argv = launcher.to_vec();
    argv.extend([
        "-w",
        "portal",
        "-restore-portal-session",
        "yes",
        "-c",
        "mpegts",
        "-k",
        codec,
    ]);
    if cpu {
        argv.extend(["-encoder", "cpu"]);
    }
    argv.extend([
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
        &audio,
    ]);
    argv.into_iter().map(String::from).collect()
}

/// `-a`: the default output, or every app except the silenced ones. An app
/// that isn't running is simply not there to leave out.
fn audio(silence: &[String]) -> String {
    if silence.is_empty() {
        return "default_output".to_string();
    }
    let mut apps: Vec<&str> = silence.iter().map(String::as_str).collect();
    if super::silences(silence, super::DISCORD) {
        apps.push(DISCORD_VOICE);
    }
    apps.iter()
        .map(|app| format!("app-inverse:{app}"))
        .collect::<Vec<_>>()
        .join("|")
}

/// `gpu-screen-recorder --info` 6.1.3 on an RX 6950 XT under KDE Wayland.
#[cfg(test)]
pub(super) const AMD_INFO: &str = "\
section=system_info
display_server|wayland
supports_app_audio|yes
is_steam_deck|no
gsr_version|6.1.3
section=gpu_info
vendor|amd
card_path|/dev/dri/card1
section=video_codecs
h264
h264_software
hevc
hevc_hdr
hevc_10bit
h264_vulkan
hevc_vulkan
hevc_hdr_vulkan
hevc_10bit_vulkan
section=image_formats
jpeg
png
section=capture_options
DP-1|1920x1080
portal
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Share;

    fn names(recorder: &Recorder) -> Vec<&str> {
        recorder.encoders.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn lists_what_the_stream_can_carry() {
        let recorder = parse(AMD_INFO);
        assert!(recorder.can_silence);
        assert_eq!(
            names(&recorder),
            [
                "h264",
                "hevc",
                "hevc_10bit",
                "h264_software",
                "h264_vulkan",
                "hevc_vulkan",
                "hevc_10bit_vulkan",
            ]
        );
    }

    #[test]
    fn leaves_out_codecs_mpeg_ts_cant_carry() {
        let info = "section=gpu_info\nvendor|nvidia\nsection=video_codecs\nh264\nav1\nav1_10bit\nvp9\nav1_vulkan\n";
        let recorder = parse(info);
        assert!(!recorder.can_silence);
        assert_eq!(names(&recorder), ["h264"]);
        assert_eq!(
            recorder.encoders[0].label(),
            "H.264 on the NVIDIA graphics card"
        );
    }

    fn flag(argv: &[String], flag: &str) -> Option<String> {
        let at = argv.iter().position(|arg| arg == flag)?;
        argv.get(at + 1).cloned()
    }

    #[test]
    fn commands_select_the_encoder() {
        let recorder = parse(AMD_INFO);
        let argv = |name: &str| command(recorder.choose(name).unwrap(), &[], false);
        assert_eq!(flag(&argv("hevc"), "-k").as_deref(), Some("hevc"));
        assert_eq!(flag(&argv("hevc"), "-encoder"), None);
        assert_eq!(flag(&argv("h264_software"), "-k").as_deref(), Some("h264"));
        assert_eq!(
            flag(&argv("h264_software"), "-encoder").as_deref(),
            Some("cpu")
        );
        assert_eq!(
            flag(&argv("hevc_10bit_vulkan"), "-k").as_deref(),
            Some("hevc_10bit_vulkan")
        );
    }

    #[test]
    fn the_flatpak_records_the_same_way() {
        let recorder = parse(AMD_INFO);
        let h264 = recorder.choose("h264").unwrap();
        let installed = command(h264, &[], false);
        let flatpak = command(h264, &[], true);
        assert_eq!(installed[0], "gpu-screen-recorder");
        assert_eq!(flatpak[..4], FLATPAK);
        assert_eq!(installed[1..], flatpak[4..]);
    }

    #[test]
    fn silencing_discord_covers_its_voice_engine() {
        let recorder = parse(AMD_INFO);
        let h264 = recorder.choose("h264").unwrap();
        let discord = [super::super::DISCORD.to_string()];
        assert_eq!(
            flag(
                &recorder.command(h264, &discord, &Share::MainScreen).argv,
                "-a"
            )
            .as_deref(),
            Some("app-inverse:Discord|app-inverse:WEBRTC VoiceEngine")
        );
        assert_eq!(
            flag(&recorder.command(h264, &[], &Share::MainScreen).argv, "-a").as_deref(),
            Some("default_output")
        );
        let without_app_audio = Recorder {
            can_silence: false,
            ..recorder.clone()
        };
        assert_eq!(
            flag(
                &without_app_audio
                    .command(h264, &discord, &Share::MainScreen)
                    .argv,
                "-a"
            )
            .as_deref(),
            Some("default_output")
        );
    }
}
