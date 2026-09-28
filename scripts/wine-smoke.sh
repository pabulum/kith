#!/usr/bin/env bash
# The Windows build, run under Wine, watching a Linux stream: proves the
# Windows side of the portable code (named-pipe control socket, %APPDATA%
# home, signals) without a Windows machine. Wine networking is the host's, so
# like smoke.sh this proves plumbing, not NAT traversal.
#
# Needs wine, the x86_64-pc-windows-gnu rustup target, mingw-w64-gcc, ffmpeg
# and ffprobe. Uses a throwaway Wine prefix, so ~/.wine is never touched.
# Given a Windows ffmpeg.exe, it also streams from the Windows build.
# Usage: scripts/wine-smoke.sh [path/to/ffmpeg.exe]
set -uo pipefail
WIN_FFMPEG=${1:-}

cd "$(dirname "$0")/.."
cargo build --quiet || exit 1
cargo build --quiet --target x86_64-pc-windows-gnu || exit 1
KITH=./target/debug/kith
EXE=./target/x86_64-pc-windows-gnu/debug/kith.exe

T=$(mktemp -d -t kith-wine.XXXX)
# winemenubuilder would copy the prefix's Start menu into this machine's app menu.
export WINEPREFIX=$T/wine WINEDEBUG=-all WINEDLLOVERRIDES="mscoree,mshtml=;winemenubuilder.exe=d" NO_COLOR=1
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
A=$("$KITH" --home "$T/alice" id)
"$KITH" --home "$T/alice" friend add win "$W" >/dev/null
wine "$EXE" friend add alice "$A" >/dev/null 2>&1
[[ -f $WINEPREFIX/drive_c/users/$USER/AppData/Roaming/kith/secret.key ]] \
    && pass "identity created under %APPDATA%" || fail "no identity under %APPDATA%"

"$KITH" --home "$T/alice" up >"$T/alice.out" 2>"$T/alice.log" &
PIDS+=($!)
ALICE_UP=$!
wait_for "$T/alice.out" 'Kith is up' 20 || fail "alice's node didn't start"
"$KITH" --home "$T/alice" live --source test >/dev/null

# 1. Record over iroh+MoQ into a file (a Z: path is the Linux filesystem).
#    The first network use in a fresh prefix starts Wine's networking services,
#    which can take ten seconds, so this allows for them.
timeout 25 wine "$EXE" watch alice --output "Z:${T//\//\\}\\win.ts" 2>"$T/win-record.log"
n=$(frames "$T/win.ts")
[[ ${n:-0} -gt 150 ]] && pass "Windows build recorded $n frames" \
    || fail "recording: frames=${n:-none} (see $T/win-record.log)"

# 2. `up` serves the control socket as a named pipe.
wine "$EXE" up >"$T/win-up.out" 2>"$T/win-up.log" &
PIDS+=($!)
WIN_UP=$!
wait_for "$T/win-up.out" 'Kith is up' 30 || fail "the Windows node didn't start"
deadline=$((SECONDS + 15))
until [[ $(wine "$EXE" status 2>/dev/null) == *alice*LIVE* ]] || ((SECONDS > deadline)); do sleep 0.5; done
status=$(wine "$EXE" status 2>/dev/null)
[[ $status == *alice*LIVE*ms* ]] && pass "status over the named pipe shows alice LIVE with a path" \
    || fail "status: $status"
second=$(timeout 30 wine "$EXE" up 2>&1)
[[ $second == *"already running"* ]] && pass "a second up is refused" || fail "second up: $second"
handler=$(wine reg query 'HKCU\Software\Classes\kith\shell\open\command' 2>/dev/null | tr -d '\r')
[[ $handler == *'kith.exe" "open" "%1"'* ]] && pass "up registered the kith:// link handler" \
    || fail "link handler: $handler"
timeout 30 wine "$EXE" quit >/dev/null 2>&1
deadline=$((SECONDS + 15))
while kill -0 "$WIN_UP" 2>/dev/null && ((SECONDS < deadline)); do sleep 0.3; done
kill -0 "$WIN_UP" 2>/dev/null && fail "quit didn't stop the Windows node" \
    || pass "quit over the named pipe stopped the Windows node"
kill -TERM "$WIN_UP" 2>/dev/null; wait "$WIN_UP" 2>/dev/null
wineserver -k 2>/dev/null

# 3. Installing: the program into %LOCALAPPDATA%\Programs\Kith, a Start menu
#    shortcut, an uninstall entry; and uninstalling takes it all away again.
U=$WINEPREFIX/drive_c/users/$USER
timeout 60 wine "$EXE" install >/dev/null 2>&1
INSTALLED=$U/AppData/Local/Programs/Kith/kith.exe
LINK="$U/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Kith.lnk"
entry=$(wine reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kith' 2>/dev/null | tr -d '\r')
if [[ -f $INSTALLED && -f $LINK && $entry == *'kith.exe" "uninstall"'* ]] \
    && [[ $(timeout 60 wine "$INSTALLED" --version 2>/dev/null | tr -d '\r') == "kith "* ]]; then
    pass "install: program, Start menu shortcut and uninstall entry"
else
    fail "install: program=$([[ -f $INSTALLED ]] && echo yes) shortcut=$([[ -f $LINK ]] && echo yes) entry=$entry"
fi
timeout 60 wine "$INSTALLED" uninstall >/dev/null 2>&1
# The folder goes a few seconds after the uninstaller (running from it) exits.
deadline=$((SECONDS + 20))
while [[ -e $INSTALLED ]] && ((SECONDS < deadline)); do sleep 0.5; done
if [[ ! -e $INSTALLED && ! -e $LINK ]] \
    && ! wine reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kith' >/dev/null 2>&1; then
    pass "uninstall removed the program, the shortcut and the entry"
else
    fail "uninstall left: program=$([[ -e $INSTALLED ]] && echo yes) shortcut=$([[ -e $LINK ]] && echo yes)"
fi
wineserver -k 2>/dev/null

# 4. --serve, with Linux ffmpeg as the player.
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

# 5. Streaming from Windows: the test pattern through ffmpeg.exe, then the
# screen path with KITH_SCREEN standing in for Windows' screen capture, which
# Wine lacks. Wine's sound drivers are off, so the stream's sound is Kith's
# filled-in silence rather than whatever this machine is playing.
if [[ -n $WIN_FFMPEG ]]; then
    kill -TERM "$ALICE_UP"; wait "$ALICE_UP" 2>/dev/null
    wineserver -k 2>/dev/null
    APP=$T/app
    mkdir -p "$APP" && cp "$EXE" "$APP/" && ln -s "$(realpath "$WIN_FFMPEG")" "$APP/ffmpeg.exe"
    export WINEDLLOVERRIDES="mscoree,mshtml,winepulse.drv,winealsa.drv=;winemenubuilder.exe=d"
    why=$(timeout 120 wine "$APP/kith.exe" encoders 2>&1 | tr -d '\r')
    [[ $why == *"wouldn't let ffmpeg capture the screen"* ]] \
        && pass "without screen capture, encoders says why" || fail "encoders: $why"

    KITH_SCREEN="testsrc2=size=1280x720:rate=30" wine "$APP/kith.exe" live \
        >"$T/win-live.out" 2>"$T/win-live.log" &
    PIDS+=($!)
    WIN_LIVE=$!
    if wait_for "$T/win-live.log" 'live as' 60; then
        timeout 12 "$KITH" --home "$T/alice" watch win --output "$T/from-win.ts" 2>"$T/from-win.log"
        n=$(frames "$T/from-win.ts")
        audio=$(ffprobe -v error -select_streams a:0 -count_packets \
            -show_entries stream=nb_read_packets -of csv=p=0 "$T/from-win.ts" 2>/dev/null | head -1)
        [[ ${n:-0} -gt 150 && ${audio:-0} -gt 100 ]] \
            && pass "Windows streamed $n frames and $audio sound packets through ffmpeg" \
            || fail "streaming from Windows: frames=${n:-none} sound=${audio:-none} (see $T/win-live.log)"
    else
        fail "the Windows build didn't go live (see $T/win-live.log)"
    fi
    kill -TERM "$WIN_LIVE" 2>/dev/null; wait "$WIN_LIVE" 2>/dev/null
else
    echo "SKIP  streaming from Windows (pass a Windows ffmpeg.exe to run it)"
fi

if ((FAILED)); then
    echo "logs kept in $T"
    exit 1
fi
rm -rf "$T"
echo "all passed"
