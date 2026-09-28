#!/usr/bin/env bash
# The Windows build on real Windows, for what Wine can't stand in for: the
# named-pipe control socket and ffmpeg.exe on a real system, toasts, the
# registry and Start menu shortcuts, and whatever screen capture and
# encoders this machine has. Two identities on this PC stream to each other,
# and nobody needs to be at the keyboard. CI runs it on a GitHub Actions
# runner; it runs anywhere Git for Windows' bash does.
#
# It installs Kith for this user and uninstalls it again, so on your own PC
# it replaces an installed Kith.
# Usage: scripts/windows-smoke.sh path/to/ffmpeg.exe [path/to/kith.exe]
set -uo pipefail
cd "$(dirname "$0")/.."
FFMPEG=${1:?usage: scripts/windows-smoke.sh path/to/ffmpeg.exe [path/to/kith.exe]}
if [[ $# -ge 2 ]]; then
    BUILT=$2
else
    cargo build --quiet || exit 1
    BUILT=target/debug/kith.exe
fi

T=$(mktemp -d)
APP=$T/app
mkdir -p "$APP"
cp "$BUILT" "$APP/kith.exe"
cp "$FFMPEG" "$APP/ffmpeg.exe"
KITH=$APP/kith.exe
export NO_COLOR=1
PIDS=()
cleanup() {
    kith alice quit >/dev/null 2>&1
    kith bob quit >/dev/null 2>&1
    sleep 2
    for pid in "${PIDS[@]}"; do kill -9 "$pid" 2>/dev/null; done
}
trap cleanup EXIT

FAILED=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILED=1; }
home() { cygpath -w "$T/$1"; }
kith() { "$KITH" --home "$(home "$1")" "${@:2}" | tr -d '\r'; }
# Starts `up` for $1 in the background; the binary itself, so it stays ours.
up() {
    "$KITH" --home "$(home "$1")" up >"$T/$1-up.out" 2>"$T/$1-up.log" &
    PIDS+=($!)
    wait_for "$T/$1-up.out" 'Kith is up' 60 || fail "$1's node didn't start"
}
wait_for() {
    local deadline=$((SECONDS + $3))
    until grep -q "$2" "$1" 2>/dev/null; do
        ((SECONDS < deadline)) || return 1
        sleep 0.5
    done
}
# Records $2 seconds of $1's stream as bob, to $3.
record() {
    timeout "$2" "$KITH" --home "$(home bob)" watch "$1" --output "$(cygpath -w "$3")" \
        2>"$3.log"
}
# Whether $1 is at least $2 bytes of MPEG-TS: every 188-byte packet starts 0x47.
is_ts() {
    local size
    size=$(stat -c %s "$1" 2>/dev/null || echo 0)
    ((size >= $2)) || return 1
    [[ $(head -c 188000 "$1" | od -An -tx1 -v -w188 | awk '{print $1}' | sort -u) == 47 ]]
}

A=$(kith alice id)
B=$(kith bob id)
kith alice friend add bob "$B" >/dev/null
kith bob friend add alice "$A" >/dev/null

# 1. alice streams the test pattern through ffmpeg.exe, handed to her node
#    over the named pipe, and bob records it.
up alice
started=$(kith alice live --source test 2>&1)
[[ $started == live* ]] || fail "live: $started"
record alice 25 "$T/test.ts"
is_ts "$T/test.ts" 500000 && pass "bob recorded $(stat -c %s "$T/test.ts") bytes of alice's test pattern" \
    || fail "recording the test pattern (see $T/test.ts.log)"

# 2. bob's node hears alice go live and shows a toast, whose Watch button
#    is a kith:// link.
kith alice live --stop >/dev/null
up bob
kith alice live --source test >/dev/null
if wait_for "$T/bob-up.log" 'is live' 30; then
    sleep 2
    if grep -q "opens the stream (" "$T/bob-up.log"; then
        fail "the toast: $(grep 'opens the stream (' "$T/bob-up.log")"
    else
        pass "a toast told bob that alice is live"
    fi
else
    fail "bob didn't hear alice go live (see $T/bob-up.log)"
fi
handler=$(reg query 'HKCU\Software\Classes\kith\shell\open\command' 2>/dev/null | tr -d '\r')
[[ $handler == *'"open" "%1"'* ]] && pass "kith:// links open Kith" || fail "link handler: $handler"
kith alice live --stop >/dev/null
kith bob quit >/dev/null

# 3. What this PC can capture and encode, and a stream of its screen when it can.
encoders=$(kith alice encoders 2>&1)
echo "$encoders" | sed 's/^/      /'
if [[ $encoders == *" auto "*picks* ]]; then
    started=$(kith alice live 2>&1)
    record alice 25 "$T/screen.ts"
    is_ts "$T/screen.ts" 200000 && pass "bob recorded $(stat -c %s "$T/screen.ts") bytes of alice's screen" \
        || fail "streaming the screen: $started (see $T/screen.ts.log and $T/alice-up.log)"
    kith alice live --stop >/dev/null
else
    echo "SKIP  streaming the screen: nothing here can capture and encode it"
fi
kith alice quit >/dev/null

# 4. Installing: the program and ffmpeg.exe into %LOCALAPPDATA%\Programs\Kith,
#    a Start menu shortcut and an uninstall entry. Uninstalling removes them.
PROGRAMS=$(cygpath "$LOCALAPPDATA")/Programs/Kith
LINK="$(cygpath "$APPDATA")/Microsoft/Windows/Start Menu/Programs/Kith.lnk"
"$KITH" install >/dev/null
entry=$(reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kith' 2>/dev/null | tr -d '\r')
if [[ -f $PROGRAMS/kith.exe && -f $PROGRAMS/ffmpeg.exe && -f $LINK && $entry == *uninstall* ]] \
    && [[ $("$PROGRAMS/kith.exe" --version | tr -d '\r') == "kith "* ]]; then
    pass "install: kith.exe and ffmpeg.exe, a Start menu shortcut and an uninstall entry"
else
    fail "install: $(ls "$PROGRAMS" 2>&1 | tr '\n' ' ') shortcut=$([[ -f $LINK ]] && echo yes) entry=$entry"
fi
"$PROGRAMS/kith.exe" uninstall >/dev/null
deadline=$((SECONDS + 30))
while [[ -e $PROGRAMS/kith.exe ]] && ((SECONDS < deadline)); do sleep 1; done
if [[ ! -e $PROGRAMS/kith.exe && ! -e $LINK ]] \
    && ! reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kith' >/dev/null 2>&1; then
    pass "uninstall removed them"
else
    fail "uninstall left: $(ls "$PROGRAMS" 2>&1 | tr '\n' ' ') shortcut=$([[ -e $LINK ]] && echo yes)"
fi

if ((FAILED)); then
    echo "logs kept in $T"
    for log in "$T"/*.log; do
        echo "== $log"
        tail -20 "$log"
    done
    exit 1
fi
rm -rf "$T"
echo "all passed"
