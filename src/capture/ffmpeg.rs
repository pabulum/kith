//! ffmpeg, which records the screen on Windows.
//!
//! `ddagrab` (DXGI Desktop Duplication) captures on the graphics card, and the
//! card's own encoder compresses without the frames leaving it. ffmpeg has no
//! `--info` to ask, so [`detect`] test-encodes a few frames of the real capture
//! with each encoder this build has, and offers the ones that worked. ffmpeg
//! can't record desktop sound on Windows, so Kith does, and hands it over on
//! stdin as [`PCM`].

use super::{Codec, Device, Encoder, Share};

const PROGRAM: &str = "ffmpeg";

/// What [`AUTO`](super::AUTO) tries, in order: each maker's own H.264 encoder,
/// then Windows' generic one, then HEVC (every browser plays H.264), and the
/// processor last.
pub(super) const AUTO_ORDER: [&str; 10] = [
    "h264_nvenc",
    "h264_amf",
    "h264_qsv",
    "h264_mf",
    "hevc_nvenc",
    "hevc_amf",
    "hevc_qsv",
    "hevc_mf",
    "libx264",
    "libopenh264",
];

/// The sound ffmpeg reads on stdin, as its input options: the format
/// [`loopback`](super::loopback) writes.
pub const PCM: [&str; 6] = ["-f", "f32le", "-ar", "48000", "-ac", "2"];

/// How an encoder wants its frames, which decides the filters in front of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// NVIDIA's NVENC and AMD's AMF take Direct3D 11 frames as they are.
    Direct,
    /// Intel's Quick Sync takes them once they're mapped to its own frames.
    Qsv,
    /// Windows' Media Foundation encoders, through whatever graphics card has one.
    MediaFoundation,
    /// Encoders on the processor, which need the frames downloaded first.
    Software,
}

fn family(name: &str) -> Option<(Codec, Family, Device)> {
    let gpu = |maker: &str| Device::Gpu(Some(maker.to_string()));
    Some(match name {
        "h264_nvenc" => (Codec::H264, Family::Direct, gpu("NVIDIA")),
        "hevc_nvenc" => (Codec::Hevc, Family::Direct, gpu("NVIDIA")),
        "h264_amf" => (Codec::H264, Family::Direct, gpu("AMD")),
        "hevc_amf" => (Codec::Hevc, Family::Direct, gpu("AMD")),
        "h264_qsv" => (Codec::H264, Family::Qsv, gpu("Intel")),
        "hevc_qsv" => (Codec::Hevc, Family::Qsv, gpu("Intel")),
        "h264_mf" => (Codec::H264, Family::MediaFoundation, Device::Gpu(None)),
        "hevc_mf" => (Codec::Hevc, Family::MediaFoundation, Device::Gpu(None)),
        "libx264" | "libopenh264" => (Codec::H264, Family::Software, Device::Cpu),
        _ => return None,
    })
}

fn encoder(name: &str) -> Option<Encoder> {
    let (codec, family, device) = family(name)?;
    Some(Encoder {
        name: name.to_string(),
        codec,
        device,
        route: (family == Family::MediaFoundation).then_some("Windows' Media Foundation"),
    })
}

/// Scales to fit 1080p, keeping the aspect ratio and never enlarging, because
/// upload is the budget: at 8 Mbps a 4K desktop would be mush.
const FIT: &str = "width='trunc(iw*min(1,min(1920/iw,1080/ih))/2)*2':\
                   height='trunc(ih*min(1,min(1920/iw,1080/ih))/2)*2'";

/// The filters between the capture and `encoder`. With `scale_d3d11` the
/// graphics card scales and converts to NV12 too, so the frames never leave it
/// on their way to a hardware encoder.
fn filters(family: Family, scale_d3d11: bool) -> Option<String> {
    match (family, scale_d3d11) {
        (Family::Direct | Family::MediaFoundation, true) => {
            Some(format!("scale_d3d11={FIT}:format=nv12"))
        }
        // NVENC and AMF take the captured frames at the desktop's own size.
        (Family::Direct, false) => None,
        (Family::Qsv, true) => Some(format!(
            "scale_d3d11={FIT}:format=nv12,hwmap=derive_device=qsv,format=qsv"
        )),
        (Family::Qsv, false) => Some(format!(
            "hwmap=derive_device=qsv,format=qsv,vpp_qsv={FIT}:format=nv12"
        )),
        (Family::MediaFoundation, false) => {
            Some(format!("hwdownload,format=bgra,scale={FIT},format=nv12"))
        }
        (Family::Software, _) => Some(format!("hwdownload,format=bgra,scale={FIT},format=yuv420p")),
    }
}

/// Constant 8 Mbps, a keyframe every second at 60 fps (one MoQ group), and no
/// B-frames, which would hold frames back.
fn encoder_options(name: &str) -> &'static [&'static str] {
    match name {
        "h264_nvenc" | "hevc_nvenc" => &[
            "-preset", "p4", "-tune", "ll", "-rc", "cbr", "-b:v", "8M", "-maxrate", "8M",
            "-bufsize", "8M", "-g", "60", "-bf", "0",
        ],
        "h264_amf" | "hevc_amf" => &[
            "-usage",
            "lowlatency",
            "-rc",
            "cbr",
            "-b:v",
            "8M",
            "-maxrate",
            "8M",
            "-bufsize",
            "8M",
            "-g",
            "60",
            "-bf",
            "0",
        ],
        "h264_qsv" | "hevc_qsv" => &[
            "-preset", "veryfast", "-b:v", "8M", "-maxrate", "8M", "-bufsize", "8M", "-g", "60",
            "-bf", "0",
        ],
        "h264_mf" | "hevc_mf" => &[
            "-hw_encoding",
            "1",
            "-rate_control",
            "cbr",
            "-scenario",
            "display_remoting",
            "-b:v",
            "8M",
            "-g",
            "60",
            "-bf",
            "0",
        ],
        "libx264" => &[
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-b:v",
            "8M",
            "-maxrate",
            "8M",
            "-bufsize",
            "8M",
            "-g",
            "60",
        ],
        _ => &["-b:v", "8M", "-g", "60"],
    }
}

/// `$KITH_SCREEN`: a lavfi source to record instead of the screen, such as
/// `testsrc2=size=1280x720:rate=30`, for testing where Windows can't capture
/// (Wine). Its frames aren't on the graphics card, so only processor encoders
/// are offered.
fn stand_in() -> Option<String> {
    std::env::var("KITH_SCREEN")
        .ok()
        .filter(|source| !source.is_empty())
}

/// The capture: with gfxcapture (Windows.Graphics.Capture, ffmpeg 8.0 on), the
/// monitor or window picked, by handle; without it, ddagrab's first monitor.
/// Either draws the mouse pointer in.
///
/// A window that's resized keeps the stream's size (`scale_aspect`), because
/// the encoder can't change size mid-stream.
fn source(share: &Share, gfxcapture: bool, framerate: u32) -> String {
    if !gfxcapture {
        return format!("ddagrab=framerate={framerate}:draw_mouse=1");
    }
    let target = match share {
        Share::Window { handle, .. } => format!("hwnd={handle}:resize_mode=scale_aspect"),
        Share::Screen { handle, .. } => format!("hmonitor={handle}"),
        Share::MainScreen => format!("hmonitor={}", main_monitor()),
    };
    format!("gfxcapture={target}:max_framerate={framerate}:capture_cursor=1")
}

#[cfg(windows)]
fn main_monitor() -> u64 {
    super::share::main_monitor()
}

/// Tests can't ask Windows, so they get a stand-in handle.
#[cfg(not(windows))]
fn main_monitor() -> u64 {
    1
}

/// The video half of a command: capture, filters, encoder.
fn video(encoder: &str, scale_d3d11: bool, source: String) -> Vec<String> {
    let stand_in = stand_in();
    let source = stand_in.clone().unwrap_or(source);
    let mut argv: Vec<String> = [
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        source,
    ]
    .into();
    let (_, family, _) = family(encoder).expect("only known encoders are offered");
    let filters = match stand_in {
        Some(_) => Some(format!("scale={FIT},format=yuv420p")),
        None => filters(family, scale_d3d11),
    };
    if let Some(filters) = filters {
        argv.extend(["-vf".to_string(), filters]);
    }
    argv.extend(["-c:v".to_string(), encoder.to_string()]);
    argv.extend(encoder_options(encoder).iter().map(|arg| arg.to_string()));
    argv
}

/// The command that streams `share` with `encoder`, plus the desktop sound
/// Kith writes to its stdin, as MPEG-TS on stdout.
pub(super) fn command(
    encoder: &Encoder,
    scale_d3d11: bool,
    gfxcapture: bool,
    share: &Share,
) -> Vec<String> {
    let mut argv: Vec<String> = [PROGRAM, "-hide_banner", "-loglevel", "error", "-nostats"]
        .map(String::from)
        .into();
    let video = video(&encoder.name, scale_d3d11, source(share, gfxcapture, 60));
    // Inputs first: the capture's `-f lavfi -i ...`, then the sound on stdin.
    let (capture, output) = video.split_at(4);
    argv.extend_from_slice(capture);
    argv.extend(PCM.map(String::from));
    argv.extend(["-i", "pipe:0", "-map", "0:v", "-map", "1:a"].map(String::from));
    argv.extend_from_slice(output);
    argv.extend(
        [
            "-c:a",
            "aac",
            "-b:a",
            "160k",
            // gfxcapture sends no frames while the picture is still, and the
            // muxer would hold the sound back for up to 10 s waiting for them.
            "-max_interleave_delta",
            "200000",
            "-f",
            "mpegts",
            "-flush_packets",
            "1",
            "pipe:1",
        ]
        .map(String::from),
    );
    argv
}

/// The encoder names in `ffmpeg -encoders` output.
fn listed_encoders(text: &str) -> Vec<&str> {
    text.lines()
        .skip_while(|line| !line.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect()
}

/// Whether `ffmpeg -filters` output lists `filter`.
fn has_filter(text: &str, filter: &str) -> bool {
    text.lines()
        .any(|line| line.split_whitespace().nth(1) == Some(filter))
}

/// Which of the encoders this build has, in [`AUTO_ORDER`], could be offered.
/// OpenH264 only stands in for x264, which is much better at the same bitrate.
fn candidates(listed: &[&str]) -> Vec<&'static str> {
    let has_x264 = listed.contains(&"libx264");
    AUTO_ORDER
        .into_iter()
        .filter(|name| listed.contains(name))
        .filter(|&name| !(name == "libopenh264" && has_x264))
        .collect()
}

#[cfg(windows)]
pub(super) use windows::detect;

#[cfg(windows)]
mod windows {
    use std::{
        process::{Command, Output, Stdio},
        sync::Mutex,
        time::{Duration, Instant},
    };

    use anyhow::{Result, bail};
    use tracing::{debug, info};

    use super::*;
    use crate::capture::{Backend, Recorder};

    /// How long one test encode may take; a stuck driver doesn't hold up the app.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Detection test-encodes with every encoder, which takes a second or two,
    /// so it runs once per process.
    static DETECTED: Mutex<Option<Recorder>> = Mutex::new(None);

    /// Holds the lock while probing, so the app's startup check and an early
    /// Go live don't both test-encode: the second waits for the first's result.
    pub(in crate::capture) fn detect() -> Result<Recorder> {
        let mut detected = DETECTED.lock().unwrap();
        if let Some(recorder) = detected.clone() {
            return Ok(recorder);
        }
        let recorder = probe()?;
        *detected = Some(recorder.clone());
        Ok(recorder)
    }

    fn run(args: &[&str]) -> Result<String> {
        let output = Command::new(PROGRAM)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|err| crate::node::spawn_error(PROGRAM, "capture", err))?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn probe() -> Result<Recorder> {
        if let Some(source) = stand_in() {
            info!("KITH_SCREEN is set: recording {source} instead of the screen");
            let listed = run(&["-hide_banner", "-encoders"])?;
            let encoders: Vec<Encoder> = candidates(&listed_encoders(&listed))
                .into_iter()
                .filter(|name| {
                    family(name).is_some_and(|(_, family, _)| family == Family::Software)
                })
                .filter_map(encoder)
                .collect();
            if encoders.is_empty() {
                bail!("KITH_SCREEN needs an ffmpeg with libx264 or libopenh264");
            }
            // The picker shows as usual, though the stand-in replaces what's picked.
            let filters = run(&["-hide_banner", "-filters"])?;
            return Ok(Recorder {
                encoders,
                can_silence: true,
                backend: Backend::Ffmpeg {
                    scale_d3d11: false,
                    gfxcapture: has_filter(&filters, "gfxcapture"),
                },
            });
        }
        let filters = run(&["-hide_banner", "-filters"])?;
        if !has_filter(&filters, "ddagrab") {
            bail!(
                "this ffmpeg can't capture the screen (it has no ddagrab filter); \
                 put a current Windows build of ffmpeg next to kith.exe"
            );
        }
        let scale_d3d11 = has_filter(&filters, "scale_d3d11");
        let gfxcapture = has_filter(&filters, "gfxcapture");
        let encoders = run(&["-hide_banner", "-encoders"])?;
        let names = candidates(&listed_encoders(&encoders));

        let started = Instant::now();
        let outcomes: Vec<(&str, Option<Output>)> = std::thread::scope(|scope| {
            let probes: Vec<_> = names
                .iter()
                .map(|&name| {
                    (
                        name,
                        scope.spawn(move || test_encode(name, scale_d3d11, gfxcapture)),
                    )
                })
                .collect();
            probes
                .into_iter()
                .map(|(name, probe)| (name, probe.join().ok().flatten()))
                .collect()
        });
        let mut working = Vec::new();
        let mut complaint = None;
        for (name, output) in outcomes {
            match output {
                Some(output) if output.status.success() => working.extend(encoder(name)),
                Some(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
                    debug!(
                        "{name} can't encode the capture here: {}",
                        last.unwrap_or("?")
                    );
                    complaint = complaint.or(last.map(str::to_string));
                }
                None => debug!("{name} timed out on a test encode"),
            }
        }
        info!(
            "test-encoded the screen with {} encoders in {:.1}s; {} work",
            names.len(),
            started.elapsed().as_secs_f64(),
            working.len()
        );
        if working.is_empty() {
            let complaint = complaint.unwrap_or_default();
            if complaint.contains("opening input") {
                bail!(
                    "Windows wouldn't let ffmpeg capture the screen ({complaint}). Screen \
                     capture needs a graphics driver, and doesn't work over Remote Desktop \
                     or in most virtual machines"
                );
            }
            bail!("ffmpeg couldn't encode the screen with any encoder it has ({complaint})");
        }
        working.sort_by_key(Encoder::rank);
        Ok(Recorder {
            encoders: working,
            can_silence: true,
            backend: Backend::Ffmpeg {
                scale_d3d11,
                gfxcapture,
            },
        })
    }

    /// One frame of the main screen through `name` and its filters, thrown away.
    /// One is enough, and Windows.Graphics.Capture sends only the first while
    /// the picture is still.
    fn test_encode(name: &str, scale_d3d11: bool, gfxcapture: bool) -> Option<Output> {
        let mut argv = vec![
            "-hide_banner".to_string(),
            "-loglevel".into(),
            "error".into(),
        ];
        let source = source(&Share::MainScreen, gfxcapture, 30);
        argv.extend(video(name, scale_d3d11, source));
        argv.extend(["-frames:v", "1", "-f", "null", "-"].map(String::from));
        let mut child = Command::new(PROGRAM)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + PROBE_TIMEOUT;
        while Instant::now() < deadline {
            if child.try_wait().ok()?.is_some() {
                return child.wait_with_output().ok();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tail of `ffmpeg -encoders` from a Windows build.
    const ENCODERS: &str = "\
Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 V....D h264_amf             AMD AMF H.264 Encoder (codec h264)
 V....D h264_mf              H264 via MediaFoundation (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V....D h264_qsv             H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (Intel Quick Sync Video acceleration) (codec h264)
 V....D av1_nvenc            NVIDIA NVENC av1 encoder (codec av1)
 V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)
 A....D aac                  AAC (Advanced Audio Coding)
";

    const FILTERS: &str = "\
Filters:
  T.. = Timeline support
 ... ddagrab           |->V       Grab Windows Desktop images using Desktop Duplication API
 ... scale_d3d11       V->V       Scale video using Direct3D11
 T.. scale             V->V       Scale the input video size and/or convert the image format.
";

    #[test]
    fn candidates_follow_the_auto_order() {
        let listed = listed_encoders(ENCODERS);
        assert!(listed.contains(&"aac"));
        assert_eq!(
            candidates(&listed),
            [
                "h264_nvenc",
                "h264_amf",
                "h264_qsv",
                "h264_mf",
                "hevc_nvenc",
                "libx264"
            ]
        );
        assert_eq!(candidates(&["libopenh264", "libx264"]), ["libx264"]);
        assert_eq!(candidates(&["libopenh264"]), ["libopenh264"]);
        assert!(has_filter(FILTERS, "ddagrab"));
        assert!(has_filter(FILTERS, "scale_d3d11"));
        assert!(!has_filter(FILTERS, "vpp_qsv"));
    }

    #[test]
    fn labels_name_the_maker() {
        let label = |name: &str| encoder(name).unwrap().label();
        assert_eq!(label("h264_nvenc"), "H.264 on the NVIDIA graphics card");
        assert_eq!(label("hevc_amf"), "HEVC on the AMD graphics card");
        assert_eq!(
            label("h264_mf"),
            "H.264 on the graphics card, through Windows' Media Foundation"
        );
        assert_eq!(label("libx264"), "H.264 on the processor");
        assert!(encoder("av1_nvenc").is_none());
    }

    fn flag(argv: &[String], flag: &str) -> Option<String> {
        let at = argv.iter().position(|arg| arg == flag)?;
        argv.get(at + 1).cloned()
    }

    #[test]
    fn frames_stay_on_the_graphics_card() {
        let nvenc = command(
            &encoder("h264_nvenc").unwrap(),
            true,
            false,
            &Share::MainScreen,
        );
        let vf = flag(&nvenc, "-vf").unwrap();
        assert!(vf.starts_with("scale_d3d11=width='trunc("), "{vf}");
        assert!(vf.ends_with(":format=nv12"), "{vf}");
        assert!(!vf.contains("hwdownload"));
        // Without scale_d3d11, NVENC takes the desktop at its own size.
        assert_eq!(
            flag(
                &command(
                    &encoder("h264_nvenc").unwrap(),
                    false,
                    false,
                    &Share::MainScreen
                ),
                "-vf"
            ),
            None
        );
        let qsv = flag(
            &command(
                &encoder("h264_qsv").unwrap(),
                false,
                false,
                &Share::MainScreen,
            ),
            "-vf",
        )
        .unwrap();
        assert!(qsv.starts_with("hwmap=derive_device=qsv"), "{qsv}");
        let x264 = flag(
            &command(
                &encoder("libx264").unwrap(),
                true,
                false,
                &Share::MainScreen,
            ),
            "-vf",
        )
        .unwrap();
        assert!(
            x264.starts_with("hwdownload,format=bgra,scale=width='trunc("),
            "{x264}"
        );
    }

    #[test]
    fn auto_prefers_the_makers_h264() {
        let recorder = crate::capture::Recorder {
            encoders: ["libx264", "hevc_nvenc", "h264_mf", "h264_nvenc"]
                .into_iter()
                .filter_map(encoder)
                .collect(),
            can_silence: true,
            backend: crate::capture::Backend::Ffmpeg {
                scale_d3d11: true,
                gfxcapture: true,
            },
        };
        let auto = recorder.choose(crate::capture::AUTO).unwrap();
        assert_eq!(auto.name, "h264_nvenc");
        let recording = recorder.command(auto, &["Discord".to_string()], &Share::MainScreen);
        assert_eq!(recording.argv[0], "ffmpeg");
        assert_eq!(recording.sound, Some(vec!["Discord".to_string()]));
    }

    #[test]
    fn gfxcapture_takes_what_was_picked() {
        let h264 = encoder("h264_nvenc").unwrap();
        let input = |share: &Share| flag(&command(&h264, true, true, share), "-i").unwrap();
        let window = Share::Window {
            handle: 42,
            label: "Notes".into(),
        };
        assert_eq!(
            input(&window),
            "gfxcapture=hwnd=42:resize_mode=scale_aspect:max_framerate=60:capture_cursor=1"
        );
        let screen = Share::Screen {
            handle: 7,
            label: "Screen 2".into(),
        };
        assert_eq!(
            input(&screen),
            "gfxcapture=hmonitor=7:max_framerate=60:capture_cursor=1"
        );
        assert!(input(&Share::MainScreen).starts_with("gfxcapture=hmonitor="));
    }

    #[test]
    fn the_sound_comes_in_on_stdin() {
        let argv = command(
            &encoder("hevc_amf").unwrap(),
            true,
            false,
            &Share::MainScreen,
        );
        let inputs: Vec<&str> = argv
            .windows(2)
            .filter(|pair| pair[0] == "-i")
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(inputs, ["ddagrab=framerate=60:draw_mouse=1", "pipe:0"]);
        // The PCM format goes before its `-i`, the video options after both inputs.
        let pcm = argv.iter().position(|arg| arg == "f32le").unwrap();
        let stdin = argv.iter().position(|arg| arg == "pipe:0").unwrap();
        let codec = argv.iter().position(|arg| arg == "-c:v").unwrap();
        assert!(pcm < stdin && stdin < codec);
        assert_eq!(flag(&argv, "-c:v").as_deref(), Some("hevc_amf"));
        assert_eq!(flag(&argv, "-f").as_deref(), Some("lavfi"));
        assert_eq!(argv.last().map(String::as_str), Some("pipe:1"));
    }
}
