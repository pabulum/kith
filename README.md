# pstream

Stream your screen to your friends, peer to peer. There are no accounts and
no servers of ours, and quality isn't a paid tier: your stream goes straight
to each friend over an encrypted connection.

> **Early days.** It works, but it has been tested on few machines, and on
> Windows only under Wine so far. Expect rough edges.

## How it works

- **Your code is your identity.** pstream makes a key pair the first time it
  runs, and your code is the public half. Send yours to a friend and add
  theirs. pstream only connects people who have added each other, so nobody
  else can put a stream on your screen.
- **Going live tells your friends.** Friends with pstream open get a
  notification, or their player opens by itself if they asked for that.
- **The video goes straight to them** when your networks allow a direct
  connection, which is most of the time, and through a public relay when they
  don't. [iroh](https://github.com/n0-computer/iroh) makes the connection and
  [Media over QUIC](https://github.com/moq-dev/moq) carries the video.

| | Watch | Stream |
| --- | --- | --- |
| Linux | yes | yes, through the desktop's screen-sharing portal |
| Windows 10 and 11 | yes | not yet |
| Android | command line only; builds, untested | no |

## Install

There are no downloads yet. To build it, [install Rust](https://rustup.rs),
then:

```sh
git clone <this repository>
cd pstream
cargo build --release
```

The program is `target/release/pstream`. To build the Windows one on Linux,
run `rustup target add x86_64-pc-windows-gnu`, install mingw-w64
(`mingw-w64-gcc` on Arch), and run
`cargo build --release --target x86_64-pc-windows-gnu`.

You also need:

- **To watch:** mpv or VLC. With neither, streams open in your web browser,
  which lags a little more.
- **To stream (Linux):** [gpu-screen-recorder](https://git.dec05eba.com/gpu-screen-recorder/about/),
  and ffmpeg for the test pattern.
- **For notifications (Linux, optional):** `notify-send`.

## Use it

### The app

Run `pstream` with no arguments (on Windows, double-click `pstream.exe`).
The window shows your code, your friends, and who's live.

1. **Copy** your code and send it to a friend. Put theirs in **Add a friend**.
2. When a friend goes live, they show as LIVE and pstream's taskbar entry
   asks for attention. Click **Watch**. Tick **auto-open** to have their
   stream open by itself next time.
3. To stream, pick **Screen** or **Test pattern** and click **Go live**. The
   first time, your desktop asks which screen or window to share.

Friends can only reach you while the window (or `pstream up`) is running.

### The command line

```sh
pstream id                               # your code
pstream friend add sam <sam's code>      # sam adds yours too, or neither of you connects
pstream friend set sam --auto-open       # open sam's stream as soon as they go live
pstream up                               # stay reachable without the window
pstream live                             # stream your screen
pstream live --source test               # ...or a test pattern
pstream live --stop
pstream watch sam [--latency low|normal|smooth]
pstream watch sam --serve 127.0.0.1:8080 # for a player you open yourself, at that URL
pstream status                           # who's online or live, and whether it's direct or relayed
```

While the window or `pstream up` runs, the other commands go through it.
Without either, `live` and `watch` run until you press Ctrl-C.

## Players

`player` in the settings picks what plays a stream:

- `"auto"`, the default: mpv if it's installed, else VLC, else the browser.
- `"mpv"`, `"vlc"` or `"browser"`.
- A command, such as `["mpv", "--fs", "-"]`. It gets the stream on its
  standard input, or a local URL wherever an argument says `{url}`.

mpv has the least delay. The browser plays a page that pstream serves on your
own machine (127.0.0.1). Whether a browser can play a stream depends on the
browser and the graphics card: streams from Linux are H.265/HEVC, and a
browser that can't play that says so and links to VLC.

## Settings

The settings folder is `~/.config/pstream` on Linux and `%APPDATA%\pstream`
on Windows. It holds `config.toml`, your key (`secret.key`) and the log
(`pstream.log`). Besides `player`, `config.toml` has:

- `capture`: the command that records your screen. It writes MPEG-TS to
  standard output.
- `latency`: how far behind live you watch by default.

`--home <dir>` or `PSTREAM_HOME` uses another folder, which is how one
computer can be two people.

Keep `secret.key`. It *is* your identity: if you lose it, your friends have to
add your new code.

## Privacy

- **Your friends can see your IP address.** A direct connection needs it, as
  in most peer-to-peer games and calls.
- **So can anyone who has your code,** even if you haven't added them: setting
  up a connection exchanges addresses before pstream checks who's asking.
  Share your code the way you'd share your phone number. If it gets out,
  delete `secret.key` to get a new one, and send that to your friends.
- **Streams are encrypted end to end.** The relays that carry traffic when a
  direct connection isn't possible can't read it, but they can see which codes
  talk to each other, and when.
- **pstream uses public infrastructure run by [n0](https://n0.computer),**
  who make iroh: the relays, and a directory that maps your code to the relay
  you're using (not to your IP address). The relays are rate-limited, and
  pstream can't use one of your own yet.
- **The log** has your friends' names and codes. Read it before you share it.

## Troubleshooting

- **The log** is `pstream.log` in the settings folder. The previous run's is
  `pstream.log.old`.
- **The window doesn't open on Windows.** pstream tries again with OpenGL, and
  if that fails too, shows a dialog that says where the log is. Setting
  `WGPU_BACKEND=gl` goes straight to OpenGL.
- **A stream stutters.** `pstream status` shows whether each friend's
  connection is `direct` or `relayed`. Relayed traffic goes through a
  rate-limited public server.
- **Windows warns about an unrecognized app.** pstream isn't code-signed yet.
  Choose **More info**, then **Run anyway**.

## Development

```sh
cargo test               # unit tests
scripts/smoke.sh         # end to end on one machine: three identities, headless players
scripts/wine-smoke.sh    # the Windows build under Wine, watching a Linux stream
scripts/release.sh       # release archives in dist/, with third-party licenses
```

## License

MIT, in [LICENSE](LICENSE). The browser player embeds
[mpegts.js](https://github.com/xqq/mpegts.js), which is Apache-2.0; see
`assets/mpegts.js`. Release archives list the licenses of everything
compiled in.
