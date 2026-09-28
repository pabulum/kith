# Kith

*kith* (n.): friends and acquaintances; the people you know.

Stream your screen to your friends, peer to peer. There are no accounts and
no servers of ours, and quality isn't a paid tier: your stream goes straight
to each friend over an encrypted connection.

> **Early days.** It works, but it has been tested on few machines, and on
> Windows only under Wine so far. Expect rough edges.

## How it works

- **Your code is your identity.** Kith makes a key pair the first time it
  runs, and your code is the public half. Send yours to a friend and add
  theirs. Kith only connects people who have added each other, so nobody
  else can put a stream on your screen.
- **Going live tells your friends.** Friends with Kith open get a
  notification, or their player opens by itself if they asked for that.
- **The video goes straight to them** when your networks allow a direct
  connection, which is most of the time, and through a public relay when they
  don't. [iroh](https://github.com/n0-computer/iroh) makes the connection and
  [Media over QUIC](https://github.com/moq-dev/moq) carries the video.

| | Watch | Stream |
| --- | --- | --- |
| Linux | yes | yes, through the desktop's screen-sharing portal |
| Windows 10 and 11 | yes | a monitor or a window, through ffmpeg; not yet tried on a real PC |
| Android | command line only; builds, untested | no |

## Install

There are no downloads yet. To build it, [install Rust](https://rustup.rs),
then:

```sh
git clone <this repository>
cd kith
cargo build --release
```

The program is `target/release/kith`. To build the Windows one on Linux,
run `rustup target add x86_64-pc-windows-gnu`, install mingw-w64
(`mingw-w64-gcc` on Arch), and run
`cargo build --release --target x86_64-pc-windows-gnu`.

You also need:

- **To watch:** mpv or VLC. With neither, streams open in your web browser,
  which lags a little more.
- **To stream (Linux):** [gpu-screen-recorder](https://git.dec05eba.com/gpu-screen-recorder/about/),
  and ffmpeg for the test pattern.
- **To stream (Windows):** `ffmpeg.exe` next to `kith.exe`, from one of the
  Windows builds [ffmpeg.org](https://ffmpeg.org/download.html) links to
  (ffmpeg 8.0 or later picks a monitor or a window; older ones stream the
  main screen).

## Use it

### The app

Run `kith` with no arguments (on Windows, double-click `kith.exe`).
The window shows your code, your friends, and who's live. The first time,
it offers to **Install** itself: into the Start menu on Windows (Settings →
Apps removes it again), or your app menu on Linux. Opening a newer Kith
later offers to **Update** the installed one.

1. **Copy** your code and send it to a friend. Put theirs in **Add a friend**.
2. When a friend goes live, a notification says so, with a **Watch**
   button, and they show as LIVE in the window. Tick **auto-open** to have
   their stream open by itself next time.
3. To stream, pick **Screen** or **Test pattern** and click **Go live**. The
   first time, your desktop asks which screen or window to share.
   On Windows, **Share** picks a monitor or one window. A window streams on
   its own even behind others, and closing it ends the stream.
   **Video** picks the encoder. **Automatic** uses your graphics card and
   H.264, which every friend can play. Hover over a choice to see what it
   means for the friends watching. **Keep Discord out of the sound**, on by
   default, stops friends in a Discord call with you hearing themselves.

Friends can reach you while Kith runs. Closing the window leaves it in the
tray (the notification area, on Windows): click its icon to open the window
again, or use its menu to quit. On a desktop without a tray (GNOME without
the AppIndicator extension), closing the window quits Kith. Under
**Settings**, **Start Kith when you log in** starts it in the tray.

### The command line

```sh
kith id                               # your code
kith friend add sam <sam's code>      # sam adds yours too, or neither of you connects
kith friend set sam --auto-open       # open sam's stream as soon as they go live
kith up                               # stay reachable without the window
kith live                             # stream your screen
kith live --source test               # ...or a test pattern
kith live --stop
kith encoders                         # the video encoders your screen can be streamed with
kith watch sam [--latency low|normal|smooth]
kith watch sam --serve 127.0.0.1:8080 # for a player you open yourself, at that URL
kith status                           # who's online or live, and whether it's direct or relayed
kith quit                             # stop Kith: the window, the tray icon, or `kith up`
kith install                          # into the Start menu or app menu (~/.local/bin on Linux)
kith uninstall                        # ...and out again; your code and friends stay
```

While Kith runs (the app, or `kith up`), the other commands go through it.
Without it, `live` and `watch` run until you press Ctrl-C. Opening Kith
again shows the window it already has, and `kith --background` starts it in
the tray without one.

## Players

`player` in the settings picks what plays a stream:

- `"auto"`, the default: mpv if it's installed, else VLC, else the browser.
- `"mpv"`, `"vlc"` or `"browser"`.
- A command, such as `["mpv", "--fs", "-"]`. It gets the stream on its
  standard input, or a local URL wherever an argument says `{url}`.

mpv has the least delay. The browser plays a page that Kith serves on your
own machine (127.0.0.1). Every browser plays H.264, which streams use unless
the streamer picks HEVC. Whether a browser can play HEVC depends on the
browser and the graphics card, and one that can't says so and links to VLC.

## Settings

The settings folder is `~/.config/kith` on Linux and `%APPDATA%\kith`
on Windows. It holds `config.toml`, your key (`secret.key`) and the log
(`kith.log`). Besides `player`, `config.toml` has:

- `encoder`: the video encoder for streaming your screen. `"auto"` (the
  default) picks the graphics card and H.264; `kith encoders` lists the
  others your computer has, such as `"hevc"` or `"hevc_10bit"`.
- `silence`: apps whose sound stays out of your stream, `["Discord"]` by
  default. Add a voice chat you use, such as `"TeamSpeak"`, or set `[]` to
  send everything. On Windows only the first one that's running is left out,
  and it takes Windows 10 version 2004 or later.
- `capture`: not set by default. A command to record your screen with
  instead of the built-in one, which writes MPEG-TS (H.264 or HEVC video,
  AAC audio) to standard output. `encoder` and `silence` don't apply to it.
- `latency`: how far behind live you watch by default.

`--home <dir>` or `KITH_HOME` uses another folder, which is how one
computer can be two people.

Keep `secret.key`. It *is* your identity: if you lose it, your friends have to
add your new code.

## Privacy

- **Your friends can see your IP address.** A direct connection needs it, as
  in most peer-to-peer games and calls.
- **So can anyone who has your code,** even if you haven't added them: setting
  up a connection exchanges addresses before Kith checks who's asking.
  Share your code the way you'd share your phone number. If it gets out,
  delete `secret.key` to get a new one, and send that to your friends.
- **Streams are encrypted end to end.** The relays that carry traffic when a
  direct connection isn't possible can't read it, but they can see which codes
  talk to each other, and when.
- **Kith uses public infrastructure run by [n0](https://n0.computer),**
  who make iroh: the relays, and a directory that maps your code to the relay
  you're using (not to your IP address). The relays are rate-limited, and
  Kith can't use one of your own yet.
- **The log** has your friends' names and codes. Read it before you share it.

## Troubleshooting

- **The log** is `kith.log` in the settings folder. The previous run's is
  `kith.log.old`.
- **The window doesn't open on Windows.** Kith tries again with OpenGL, and
  if that fails too, shows a dialog that says where the log is. Setting
  `WGPU_BACKEND=gl` goes straight to OpenGL.
- **A stream stutters.** `kith status` shows whether each friend's
  connection is `direct` or `relayed`. Relayed traffic goes through a
  rate-limited public server.
- **Windows warns about an unrecognized app.** Kith isn't code-signed yet.
  Choose **More info**, then **Run anyway**.
- **No notifications on Windows.** Windows holds them back during games
  and in Do Not Disturb, and they wait in the notification center.
- **No Kith in the tray on GNOME.** GNOME shows tray icons only with the
  AppIndicator extension (Ubuntu turns it on). Without it, closing the
  window quits Kith.

## Development

```sh
cargo test               # unit tests
scripts/smoke.sh         # end to end on one machine: three identities, headless players
scripts/wine-smoke.sh    # the Windows build under Wine, watching a Linux stream
scripts/wine-smoke.sh path/to/ffmpeg.exe   # ...and streaming from Windows too
scripts/release.sh       # release archives in dist/, with third-party licenses
```

## License

MIT, in [LICENSE](LICENSE). The browser player embeds
[mpegts.js](https://github.com/xqq/mpegts.js), which is Apache-2.0; see
`assets/mpegts.js`. Release archives list the licenses of everything
compiled in.
