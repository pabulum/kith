#!/usr/bin/env bash
# The Windows build, run under Wine, watching a Linux stream: proves the
# Windows side of the portable code (named-pipe control socket, %APPDATA%
# home, signals) without a Windows machine. Wine networking is the host's, so
# like smoke.sh this proves plumbing, not NAT traversal.
#
# Needs wine, the x86_64-pc-windows-gnu rustup target, mingw-w64-gcc, ffmpeg
# and ffprobe. Uses a throwaway Wine prefix, so ~/.wine is never touched.
# Usage: scripts/wine-smoke.sh
set -uo pipefail

cd "$(dirname "$0")/.."
cargo build --quiet || exit 1
cargo build --quiet --target x86_64-pc-windows-gnu || exit 1
PSTREAM=./target/debug/pstream
EXE=./target/x86_64-pc-windows-gnu/debug/pstream.exe

T=$(mktemp -d -t pstream-wine.XXXX)
export WINEPREFIX=$T/wine WINEDEBUG=-all WINEDLLOVERRIDES="mscoree,mshtml=" NO_COLOR=1
# Headless: nothing here opens a window, and Wine skips its setup dialogs.
unset DISPLAY WAYLAND_DISPLAY
PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill -TERM "$pid" 2>/dev/null; done
    wineserver -k 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

FAILED=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILED=1; }
wait_for() {
    local deadline=$((SECONDS + $3))
    until grep -q "$2" "$1" 2>/dev/null; do
        ((SECONDS < deadline)) || return 1
        sleep 0.3
    done
}
frames() {
    ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames \
        -of csv=p=0 "$1" 2>/dev/null | head -1
}

wineboot -i >/dev/null 2>&1
W=$(wine "$EXE" id 2>/dev/null | tr -d '\r')
A=$("$PSTREAM" --home "$T/alice" id)
"$PSTREAM" --home "$T/alice" friend add win "$W" >/dev/null
wine "$EXE" friend add alice "$A" >/dev/null 2>&1
[[ -f $WINEPREFIX/drive_c/users/$USER/AppData/Roaming/pstream/secret.key ]] \
    && pass "identity created under %APPDATA%" || fail "no identity under %APPDATA%"

"$PSTREAM" --home "$T/alice" up >"$T/alice.out" 2>"$T/alice.log" &
PIDS+=($!)
wait_for "$T/alice.out" 'pstream is up' 20 || fail "alice's node didn't start"
"$PSTREAM" --home "$T/alice" live --source test >/dev/null

# 1. Record over iroh+MoQ into a file (a Z: path is the Linux filesystem).
timeout 15 wine "$EXE" watch alice --output "Z:${T//\//\\}\\win.ts" 2>"$T/win-record.log"
n=$(frames "$T/win.ts")
[[ ${n:-0} -gt 150 ]] && pass "Windows build recorded $n frames" \
    || fail "recording: frames=${n:-none} (see $T/win-record.log)"

# 2. `up` serves the control socket as a named pipe.
wine "$EXE" up >"$T/win-up.out" 2>"$T/win-up.log" &
PIDS+=($!)
WIN_UP=$!
wait_for "$T/win-up.out" 'pstream is up' 30 || fail "the Windows node didn't start"
deadline=$((SECONDS + 15))
until [[ $(wine "$EXE" status 2>/dev/null) == *alice*LIVE* ]] || ((SECONDS > deadline)); do sleep 0.5; done
status=$(wine "$EXE" status 2>/dev/null)
[[ $status == *alice*LIVE*ms* ]] && pass "status over the named pipe shows alice LIVE with a path" \
    || fail "status: $status"
second=$(timeout 30 wine "$EXE" up 2>&1)
[[ $second == *"already running"* ]] && pass "a second up is refused" || fail "second up: $second"
kill -TERM "$WIN_UP"; wait "$WIN_UP" 2>/dev/null
wineserver -k 2>/dev/null

# 3. --serve, with Linux ffmpeg as the player.
PORT=$((20000 + RANDOM % 20000))
wine "$EXE" watch alice --serve "127.0.0.1:$PORT" 2>"$T/win-serve.log" &
PIDS+=($!)
if wait_for "$T/win-serve.log" 'open http' 30; then
    timeout 20 ffmpeg -v error -i "http://127.0.0.1:$PORT/" -t 3 -c copy -f mpegts "$T/served.ts"
    n=$(frames "$T/served.ts")
    [[ ${n:-0} -gt 60 ]] && pass "Windows build served $n frames over HTTP" \
        || fail "--serve: frames=${n:-none} (see $T/win-serve.log)"
else
    fail "--serve never listened (see $T/win-serve.log)"
fi

if ((FAILED)); then
    echo "logs kept in $T"
    exit 1
fi
rm -rf "$T"
echo "all passed"
