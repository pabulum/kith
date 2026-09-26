# pstream

Stream your screen to friends, peer to peer, from the terminal. Built on
[iroh](https://github.com/n0-computer/iroh) for connectivity and
[Media over QUIC](https://github.com/moq-dev/moq) for the media. Proof of
concept; the plan lives in [MANIFEST.md](MANIFEST.md).

## Needs

- Rust (edition 2024)
- `mpv` to watch
- `gpu-screen-recorder` to stream your screen (Linux), or `ffmpeg` for `--source test`
- `notify-send` for "friend is live" notifications (optional)

## Use

```sh
cargo build --release
alias pstream=./target/release/pstream

pstream id                               # your code; send it to your friend
pstream friend add sam <sam's code>      # they add yours too, or neither side connects
pstream friend set sam --auto-open       # open mpv when sam goes live instead of notifying

pstream up                               # stay reachable (leave it running)
pstream live                             # stream your screen (portal picker the first time)
pstream live --source test               # ...or a test pattern
pstream live --stop
pstream watch sam [--latency low|normal|smooth]
pstream status
```

Without `pstream up` running, `live` and `watch` run in the foreground (Ctrl-C
stops them). With it running, they hand the request to it.

Settings (player command, capture command, default latency, friends) live in
`~/.config/pstream/config.toml`. The identity key is `secret.key` next to it;
lose it and friends have to re-add you. `--home <dir>` or `$PSTREAM_HOME` picks
another state directory, which is how one machine can be several people.

## Test

```sh
scripts/smoke.sh    # three identities on this machine, headless mpv, fake notify-send
```
