//! Desktop sound on Windows, without one app, for ffmpeg's stdin.
//!
//! ffmpeg can't record what the speakers play on Windows, so Kith does, with
//! WASAPI process loopback: everything except one app's process tree, which
//! works from Windows 10 2004 on. Without an app to leave out, or where process
//! loopback fails, it records the whole default output instead.
//!
//! Loopback delivers nothing while nothing plays, and ffmpeg holds the video
//! back waiting for sound to interleave with, so gaps are filled with silence,
//! keeping the sound in step with the clock.

use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{info, warn};
use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

/// 48 kHz stereo f32, as ffmpeg's `PCM` options tell it to expect.
const RATE: usize = 48_000;
const CHANNELS: usize = 2;
const FRAME: usize = CHANNELS * 4;
/// Sound goes out in chunks of about 10 ms.
const CHUNK: usize = RATE / 100 * FRAME;
/// How far the sound may drift from the clock. Silence fills in only this far
/// behind, so sound that starts playing lands close to on time, and sound
/// running further ahead is dropped.
const SLACK: Duration = Duration::from_millis(60);

/// Desktop sound being recorded, until this is dropped.
pub struct Loopback {
    pub pcm: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts recording the desktop sound without the first app in `silence`
/// that's running. If recording fails, the stream carries silence instead.
pub fn start(silence: &[String]) -> Loopback {
    let (tx, pcm) = mpsc::channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let (silence, stopping) = (silence.to_vec(), stop.clone());
    std::thread::Builder::new()
        .name("kith-sound".into())
        .spawn(move || {
            let mut clock = Clock::new();
            if let Err(err) = record(&silence, &tx, &stopping, &mut clock) {
                warn!("recording desktop sound: {err:#}; the stream's sound is silent");
                while !stopping.load(Ordering::Relaxed) && clock.send(&tx) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        })
        .expect("starting the sound thread");
    Loopback { pcm, stop }
}

fn record(
    silence: &[String],
    tx: &mpsc::Sender<Vec<u8>>,
    stop: &AtomicBool,
    clock: &mut Clock,
) -> Result<()> {
    wasapi::initialize_mta().ok().context("starting COM")?;
    let mut client = open(silence)?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, RATE, CHANNELS, None);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: 0,
    };
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .context("setting up the recording")?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    while !stop.load(Ordering::Relaxed) {
        // Timing out only means nothing is playing.
        let _ = event.wait_for_event(10);
        while capture.get_next_packet_size()?.unwrap_or(0) > 0 {
            capture.read_from_device_to_deque(&mut clock.queue)?;
        }
        if !clock.send(tx) {
            break;
        }
    }
    client.stop_stream().ok();
    Ok(())
}

/// A loopback client: the desktop without the first silenced app that's
/// running, or else the whole default output.
fn open(silence: &[String]) -> Result<AudioClient> {
    if let Some((app, pid)) = running(silence) {
        // `false` excludes the process tree
        // (PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE), whatever the
        // parameter's name suggests.
        match AudioClient::new_application_loopback_client(pid, false) {
            Ok(client) => {
                info!("recording desktop sound without {app}");
                return Ok(client);
            }
            Err(err) => warn!(
                "this Windows can't leave {app} out of the sound ({err}); friends in a \
                 call with you will hear themselves"
            ),
        }
    }
    let device = DeviceEnumerator::new()?
        .get_default_device(&Direction::Render)
        .context("finding the default sound output")?;
    Ok(device.get_iaudioclient()?)
}

/// Keeps the sound in step with the wall clock.
struct Clock {
    start: Instant,
    /// Frames handed to ffmpeg so far.
    sent: u64,
    queue: VecDeque<u8>,
}

impl Clock {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            sent: 0,
            queue: VecDeque::new(),
        }
    }

    /// Sends what's queued once there's a chunk, after filling silence in for
    /// time nothing played, or dropping sound that ran ahead. False once
    /// ffmpeg is gone.
    fn send(&mut self, tx: &mpsc::Sender<Vec<u8>>) -> bool {
        let due = (self.start.elapsed().as_secs_f64() * RATE as f64) as u64;
        let slack = (SLACK.as_secs_f64() * RATE as f64) as u64;
        let queued = (self.queue.len() / FRAME) as u64;
        let have = self.sent + queued;
        if have + slack < due {
            let missing = (due - slack - have) as usize;
            self.queue.extend(std::iter::repeat_n(0, missing * FRAME));
        } else if have > due + slack {
            let excess = (have - due).min(queued) as usize;
            self.queue.drain(..excess * FRAME);
        }
        if self.queue.len() < CHUNK {
            return true;
        }
        let whole = self.queue.len() / FRAME * FRAME;
        let chunk: Vec<u8> = self.queue.drain(..whole).collect();
        self.sent += (chunk.len() / FRAME) as u64;
        tx.blocking_send(chunk).is_ok()
    }
}

/// The first app in `silence` that's running, as the process the rest of its
/// processes descend from: excluding a process tree only works from its root.
fn running(silence: &[String]) -> Option<(String, u32)> {
    let processes = processes();
    silence.iter().find_map(|app| {
        let names = executables(app);
        let own: Vec<&(u32, u32, String)> = processes
            .iter()
            .filter(|(_, _, exe)| names.contains(exe))
            .collect();
        let pids: HashSet<u32> = own.iter().map(|(pid, _, _)| *pid).collect();
        own.iter()
            .find(|(_, parent, _)| !pids.contains(parent))
            .map(|(pid, _, _)| (app.clone(), *pid))
    })
}

/// What an app's processes are called, lowercased. Discord comes in three builds.
fn executables(app: &str) -> Vec<String> {
    if app.eq_ignore_ascii_case(super::DISCORD) {
        return ["discord.exe", "discordptb.exe", "discordcanary.exe"]
            .map(String::from)
            .into();
    }
    vec![format!("{}.exe", app.to_lowercase())]
}

/// Every running process: its id, its parent's id, and its lowercased exe name.
fn processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    let mut list = Vec::new();
    // SAFETY: the snapshot handle is checked and closed, and `entry` is a
    // zeroed PROCESSENTRY32W with dwSize set, as Process32FirstW requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return list;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            list.push((entry.th32ProcessID, entry.th32ParentProcessID, exe));
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    list
}
