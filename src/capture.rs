//! Screen capture: what records the screen here, the video encoders it
//! offers, and the command that records with one of them.
//!
//! Linux records with gpu-screen-recorder ([`gsr`]), Windows with ffmpeg
//! ([`ffmpeg`]) and Kith's own desktop sound ([`loopback`]). Only H.264 and
//! HEVC are offered: the stream travels as MPEG-TS, and moq-mux's TS export
//! carries no other video codec, so AV1, VP8 and VP9 are out whatever the
//! graphics card can do.

use anyhow::{Result, bail};

#[cfg(any(windows, test))]
mod ffmpeg;
#[cfg(any(not(windows), test))]
mod gsr;
#[cfg(windows)]
pub mod loopback;
pub mod share;

pub use share::Share;

/// The `encoder` setting that lets Kith pick.
pub const AUTO: &str = "auto";

/// Hover text for [`AUTO`] in the app, which Android builds don't have.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub const AUTO_HINT: &str = "Kith picks: the graphics card when it can encode, \
                             and H.264, which every friend can play.";

/// The app `silence` holds by default: friends in a voice call with you would
/// otherwise hear themselves come back through your stream.
pub const DISCORD: &str = "Discord";

/// A video encoder the recorder offers here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoder {
    /// The recorder's name for it, which is also what config.toml's `encoder`
    /// holds: gpu-screen-recorder's `--info` name (`hevc`, `h264_software`).
    pub name: String,
    codec: Codec,
    device: Device,
    /// A way of reaching the encoder that the label names, such as Vulkan,
    /// which gpu-screen-recorder calls experimental.
    route: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Codec {
    H264,
    Hevc,
    /// Only gpu-screen-recorder offers it.
    #[cfg_attr(windows, allow(dead_code))]
    Hevc10,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Device {
    /// The graphics card, by its maker when the recorder names one.
    Gpu(Option<String>),
    Cpu,
}

impl Encoder {
    /// What the app's picker shows, e.g. "HEVC on the AMD graphics card".
    pub fn label(&self) -> String {
        let codec = match self.codec {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Hevc10 => "HEVC 10-bit",
        };
        let device = match &self.device {
            Device::Cpu => "the processor".to_string(),
            Device::Gpu(Some(maker)) => format!("the {maker} graphics card"),
            Device::Gpu(None) => "the graphics card".to_string(),
        };
        match self.route {
            Some(route) => format!("{codec} on {device}, through {route}"),
            None => format!("{codec} on {device}"),
        }
    }

    /// What picking it means for the stream, and for the friends watching.
    /// The app shows it on hover.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn hint(&self) -> String {
        let codec = match self.codec {
            Codec::H264 => "Plays in every video player and browser.",
            Codec::Hevc => {
                "Sharper than H.264 at the same bitrate, but some browsers can't play \
                 it. VLC and mpv can."
            }
            Codec::Hevc10 => {
                "HEVC with smoother gradients in dark scenes. Fewer browsers play it \
                 than HEVC; VLC and mpv do."
            }
        };
        let device = match (&self.device, self.route) {
            (Device::Cpu, _) => {
                " Encoding on the processor keeps it busy, which can slow games down."
            }
            (_, Some(_)) => {
                " Another way to reach the same encoder: try it if the usual one misbehaves."
            }
            _ => "",
        };
        format!("{codec}{device}")
    }

    /// Orders a list the way the picker shows it: the plain graphics card
    /// routes first, then the processor, then the other routes.
    fn rank(&self) -> (bool, bool, Codec) {
        (self.route.is_some(), self.device == Device::Cpu, self.codec)
    }
}

/// What records the screen here, and what it can do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorder {
    /// In the order the picker shows them.
    pub encoders: Vec<Encoder>,
    /// Whether it can leave the apps in `silence` out of the stream's sound.
    pub can_silence: bool,
    backend: Backend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    #[cfg(any(not(windows), test))]
    Gsr,
    #[cfg(any(windows, test))]
    Ffmpeg {
        /// The build can scale on the graphics card.
        scale_d3d11: bool,
        /// The build can capture one monitor or window (Windows.Graphics.Capture).
        gfxcapture: bool,
    },
}

/// A screen recording to start.
pub struct Recording {
    pub argv: Vec<String>,
    /// The apps to leave out of the desktop sound Kith records and writes to
    /// the command's stdin, in the format of ffmpeg's `PCM` options. `None`
    /// when the command records the sound itself.
    pub sound: Option<Vec<String>>,
}

impl Recorder {
    /// Asks the recorder what this machine can do. On Linux that takes a
    /// fraction of a second; Windows test-encodes once per process, for a
    /// second or two.
    pub fn detect() -> Result<Self> {
        #[cfg(windows)]
        return ffmpeg::detect();
        #[cfg(not(windows))]
        gsr::detect()
    }

    fn find(&self, name: &str) -> Option<&Encoder> {
        self.encoders.iter().find(|encoder| encoder.name == name)
    }

    /// The encoder an `encoder` setting means here: [`AUTO`], or a name.
    pub fn choose(&self, setting: &str) -> Result<&Encoder> {
        if setting == AUTO {
            let order: &[&str] = match self.backend {
                #[cfg(any(not(windows), test))]
                Backend::Gsr => &gsr::AUTO_ORDER,
                #[cfg(any(windows, test))]
                Backend::Ffmpeg { .. } => &ffmpeg::AUTO_ORDER,
            };
            return match order.iter().find_map(|name| self.find(name)) {
                Some(encoder) => Ok(encoder),
                None => bail!(
                    "the screen recorder found no H.264 or HEVC encoder on this machine; \
                     `gpu-screen-recorder --info` shows what it did find"
                ),
            };
        }
        match self.find(setting) {
            Some(encoder) => Ok(encoder),
            None => bail!(
                "the {setting:?} encoder isn't available on this machine; pick another \
                 in the app, or see `kith encoders`"
            ),
        }
    }

    /// Whether Kith picks what's shared: on Windows, with gfxcapture. Linux's
    /// desktop portal asks for itself.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn can_pick(&self) -> bool {
        match self.backend {
            #[cfg(any(not(windows), test))]
            Backend::Gsr => false,
            #[cfg(any(windows, test))]
            Backend::Ffmpeg { gfxcapture, .. } => gfxcapture,
        }
    }

    /// The recording that streams `share` with `encoder`, leaving the apps in
    /// `silence` out of the sound where the recorder can.
    pub fn command(
        &self,
        encoder: &Encoder,
        silence: &[String],
        // Linux's desktop portal asks what to share instead.
        #[cfg_attr(not(windows), allow(unused_variables))] share: &Share,
    ) -> Recording {
        let silence = if self.can_silence { silence } else { &[] };
        match self.backend {
            #[cfg(any(not(windows), test))]
            Backend::Gsr => Recording {
                argv: gsr::command(encoder, silence),
                sound: None,
            },
            #[cfg(any(windows, test))]
            Backend::Ffmpeg {
                scale_d3d11,
                gfxcapture,
            } => Recording {
                argv: ffmpeg::command(encoder, scale_d3d11, gfxcapture, share),
                sound: Some(silence.to_vec()),
            },
        }
    }

    /// What `kith encoders` prints: a line per setting, `*` on the current one.
    pub fn table(&self, setting: &str) -> String {
        let auto = match self.choose(AUTO) {
            Ok(encoder) => format!("picks {}", encoder.label()),
            Err(_) => "finds nothing to use".to_string(),
        };
        let rows = std::iter::once((AUTO, auto)).chain(
            self.encoders
                .iter()
                .map(|encoder| (encoder.name.as_str(), encoder.label())),
        );
        let mut out = String::new();
        for (name, label) in rows {
            let mark = if name == setting { '*' } else { ' ' };
            out.push_str(&format!("{mark} {name:<18} {label}\n"));
        }
        if setting != AUTO && self.find(setting).is_none() {
            out.push_str(&format!(
                "config.toml asks for {setting:?}, which isn't available here.\n"
            ));
        }
        out
    }

    /// Shows the app's picker text for a setting, e.g. "Automatic: H.264 on
    /// the AMD graphics card".
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn describe(&self, setting: &str) -> String {
        match self.choose(setting) {
            Ok(encoder) if setting == AUTO => format!("Automatic: {}", encoder.label()),
            Ok(encoder) => encoder.label(),
            Err(_) if setting == AUTO => "Automatic (nothing available)".to_string(),
            Err(_) => format!("{setting} (not available here)"),
        }
    }
}

/// Whether `silence` includes `app`, however config.toml capitalizes it.
pub fn silences(silence: &[String], app: &str) -> bool {
    silence.iter().any(|name| name.eq_ignore_ascii_case(app))
}

/// What the sound will be, e.g. "desktop sound, without Discord".
pub fn sound(silence: &[String], can_silence: bool) -> String {
    if silence.is_empty() {
        "all desktop sound".to_string()
    } else if can_silence {
        format!("desktop sound, without {}", silence.join(", "))
    } else {
        format!(
            "all desktop sound: the recorder can't leave {} out here",
            silence.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_the_graphics_card_and_h264() {
        let recorder = gsr::parse(gsr::AMD_INFO);
        assert_eq!(recorder.choose(AUTO).unwrap().name, "h264");
        assert_eq!(recorder.choose("hevc_10bit").unwrap().name, "hevc_10bit");
        assert!(recorder.choose("av1").is_err());

        let hevc_only = gsr::parse("section=video_codecs\nhevc\nh264_software\n");
        assert_eq!(hevc_only.choose(AUTO).unwrap().name, "hevc");
        let cpu_only = gsr::parse("section=video_codecs\nh264_software\nh264_vulkan\n");
        assert_eq!(cpu_only.choose(AUTO).unwrap().name, "h264_software");
        let vulkan_only = gsr::parse("section=video_codecs\nhevc_vulkan\n");
        assert!(vulkan_only.choose(AUTO).is_err());
    }

    #[test]
    fn labels_say_where_it_encodes() {
        let recorder = gsr::parse(gsr::AMD_INFO);
        let label = |name: &str| recorder.choose(name).unwrap().label();
        assert_eq!(label("hevc"), "HEVC on the AMD graphics card");
        assert_eq!(label("h264_software"), "H.264 on the processor");
        assert_eq!(
            label("hevc_vulkan"),
            "HEVC on the AMD graphics card, through Vulkan (experimental)"
        );
        assert_eq!(
            recorder.describe(AUTO),
            "Automatic: H.264 on the AMD graphics card"
        );
        assert_eq!(recorder.describe("av1"), "av1 (not available here)");
    }

    #[test]
    fn sound_says_what_goes_out() {
        let discord = vec![DISCORD.to_string()];
        assert_eq!(sound(&discord, true), "desktop sound, without Discord");
        assert!(sound(&discord, false).starts_with("all desktop sound: "));
        assert_eq!(sound(&[], true), "all desktop sound");
        assert!(silences(&["discord".into()], DISCORD));
    }
}
