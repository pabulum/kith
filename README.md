# pstream

Stream your screen to friends, peer to peer, from the terminal. Built on
[iroh](https://github.com/n0-computer/iroh) for connectivity and
[Media over QUIC](https://github.com/moq-dev/moq) for the media. Proof of
concept; the plan lives in [MANIFEST.md](MANIFEST.md).

## Needs

- Rust (edition 2024)
- `mpv` to watch (on Windows, `mpv.exe` next to `pstream.exe` or on PATH)
- `gpu-screen-recorder` to stream your screen (Linux), or `ffmpeg` for `--source test`
- `notify-send` for "friend is live" notifications (optional)

Windows can watch but can't stream yet. Linux can do both.

## Use

`pstream` with no command opens a window. It shows your code, your friends and
who's live, and has buttons for adding friends, watching and going live. While
it's open, it does what `pstream up` does, and the commands below talk to it.
On Windows, double-click `pstream.exe`; if the window can't open, a dialog
names the log to send.

```sh
cargo build --release
alias pstream=./target/release/pstream

pstream                                  # the window
pstream id                               # your code; send it to your friend
pstream friend add sam <sam's code>      # they add yours too, or neither side connects
pstream friend set sam --auto-open       # open mpv when sam goes live instead of notifying

pstream up                               # stay reachable (leave it running)
pstream live                             # stream your screen (portal picker the first time)
pstream live --source test               # ...or a test pattern
pstream live --stop
pstream watch sam [--latency low|normal|smooth]
pstream watch sam --serve 127.0.0.1:8080 # ...for a player pstream can't start (open the URL in it)
pstream status                           # who's online or live, and whether the path is direct or relayed
```

Without `pstream up` running, `live` and `watch` run in the foreground (Ctrl-C
stops them). With it running, they hand the request to it.

Settings (player command, capture command, default latency, friends) live in
`~/.config/pstream/config.toml`, or `%APPDATA%\pstream\config.toml` on
Windows. The identity key is `secret.key` next to it; lose it and friends have
to re-add you. `--home <dir>` or `$PSTREAM_HOME` picks another state
directory, which is how one machine can be several people.

## Test

```sh
scripts/smoke.sh         # three identities on this machine, headless mpv, fake notify-send
scripts/wine-smoke.sh    # the Windows build under Wine, watching a Linux stream
```
