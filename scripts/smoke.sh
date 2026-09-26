#!/usr/bin/env bash
# End-to-end smoke test on one machine: three identities (alice streams, bob
# watches, carol is a stranger) talking over real iroh connections.
#
# Needs ffmpeg, ffprobe and mpv. Nothing pops up on screen: mpv runs with null
# outputs and a fake notify-send stands in for the desktop notification.
# Holepunching between two processes on one host is trivial, so this proves
# the plumbing (identity, friend gate, MoQ announce/subscribe, TS import and
# export, the control socket and handoff to `up`, auto-open and notification
# paths, the HTTP sink), not NAT traversal.
# Usage: scripts/smoke.sh [path/to/pstream]
set -uo pipefail

cd "$(dirname "$0")/.."
PSTREAM=${1:-./target/debug/pstream}
[[ $# -ge 1 ]] || cargo build --quiet || exit 1

# Honors $TMPDIR.
T=$(mktemp -d -t pstream-smoke.XXXX)
export NO_COLOR=1
PIDS=()
cleanup() {
    for pid in "${PIDS[@]}"; do kill -TERM "$pid" 2>/dev/null; done
    wait 2>/dev/null
}
trap cleanup EXIT

FAILED=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILED=1; }
pst() { "$PSTREAM" --home "$T/$1" "${@:2}"; }

# Waits up to $3 seconds for $2 to appear in file $1.
wait_for() {
    local deadline=$((SECONDS + $3))
    until grep -q "$2" "$1" 2>/dev/null; do
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

# Starts alice streaming the test pattern in the background. Called directly
# rather than through `pst`: a backgrounded function is a subshell, and the
# signal would go to the subshell instead of pstream.
start_alice() {
    "$PSTREAM" --home "$T/alice" live --source test >"$T/alice.out" 2>"$T/alice-$1.log" &
    PIDS+=($!)
    ALICE=$!
    wait_for "$T/alice-$1.log" 'live as' 10 || fail "alice didn't go live ($1)"
}
stop_alice() {
    kill -TERM "$ALICE" 2>/dev/null
    wait "$ALICE" 2>/dev/null
}

A=$(pst alice id)
B=$(pst bob id)
pst alice friend add bob "$B" >/dev/null
pst bob friend add alice "$A" >/dev/null

# 1. Record alice's stream to a file and check what arrived.
start_alice record
timeout 12 "$PSTREAM" --home "$T/bob" watch alice --output "$T/out.ts" 2>"$T/bob-record.log"
stop_alice
frames=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$T/out.ts" 2>/dev/null | head -1)
codecs=$(ffprobe -v error -show_entries stream=codec_name -of csv=p=0 "$T/out.ts" 2>/dev/null | sort -u | tr '\n' ' ')
if [[ ${frames:-0} -gt 150 && $codecs == *h264* && $codecs == *aac* ]]; then
    pass "recorded $frames video frames ($codecs) over iroh+MoQ"
else
    fail "recording: frames=${frames:-none} codecs=${codecs:-none} (see $T/bob-record.log)"
fi

# 2. Watch in the foreground with a player; mpv quits after 90 frames.
export PSTREAM_PLAYER="mpv --no-config --really-quiet --vo=null --ao=null --frames=90 -"
start_alice player
if timeout 30 "$PSTREAM" --home "$T/bob" watch alice 2>"$T/bob-player.log" \
    && grep -q 'player closed' "$T/bob-player.log"; then
    pass "foreground watch played 90 frames in mpv"
else
    fail "foreground watch (see $T/bob-player.log)"
fi
stop_alice

# 3. bob runs `up` with auto-open; alice going live opens bob's player on its own.
pst bob friend set alice --auto-open >/dev/null
"$PSTREAM" --home "$T/bob" up >"$T/bob-up.out" 2>"$T/bob-up.log" &
PIDS+=($!)
BOB_UP=$!
wait_for "$T/bob-up.out" 'pstream is up' 10 || fail "bob's node didn't start"
start_alice auto
if wait_for "$T/bob-up.log" 'player closed' 30; then
    pass "auto-open: bob's node opened the player when alice went live"
else
    fail "auto-open (see $T/bob-up.log)"
fi
status=$(pst bob status 2>&1)
if [[ $status == *alice*LIVE* ]]; then
    pass "status over the control socket shows alice LIVE"
else
    fail "status: $status"
fi
stop_alice

# 4. Notification path: auto-open off, a fake notify-send "clicks" Watch.
kill -TERM "$BOB_UP"; wait "$BOB_UP" 2>/dev/null
mkdir -p "$T/bin"
printf '#!/bin/sh\necho "$@" >> %q\necho watch\n' "$T/notified" >"$T/bin/notify-send"
chmod +x "$T/bin/notify-send"
pst bob friend set alice --no-auto-open >/dev/null
PATH="$T/bin:$PATH" "$PSTREAM" --home "$T/bob" up >"$T/bob-up2.out" 2>"$T/bob-up2.log" &
PIDS+=($!)
BOB_UP=$!
wait_for "$T/bob-up2.out" 'pstream is up' 10 || fail "bob's node didn't restart"
start_alice notify
if wait_for "$T/bob-up2.log" 'player closed' 30 && grep -q 'alice is live' "$T/notified"; then
    pass "notification: clicking Watch opened the player"
else
    fail "notification path (see $T/bob-up2.log)"
fi

# 5. carol isn't alice's friend, so alice refuses her.
C=$(pst carol id)
pst carol friend add alice "$A" >/dev/null
if timeout 30 "$PSTREAM" --home "$T/carol" watch alice --output "$T/carol.ts" 2>"$T/carol.log"; then
    fail "a stranger got alice's stream"
elif [[ -s $T/carol.ts ]]; then
    fail "a stranger received bytes ($T/carol.ts)"
else
    pass "stranger refused ($(grep -o 'Error: .*' "$T/carol.log" | head -1))"
fi
grep -q "refused a connection" "$T/alice-notify.log" && pass "alice logged the refusal" || fail "alice didn't log refusing carol ($C)"
stop_alice

# 6. alice runs `up` too, so `live` and `live --stop` are handed to her node
#    over the control socket. bob's node from check 4 is still watching for her.
"$PSTREAM" --home "$T/alice" up >"$T/alice-up.out" 2>"$T/alice-up.log" &
PIDS+=($!)
wait_for "$T/alice-up.out" 'pstream is up' 10 || fail "alice's node didn't start"
handoff=$(pst alice live --source test 2>&1)
if [[ $handoff == live* ]] && wait_for "$T/alice-up.log" 'live as' 10; then
    pass "live handed off to alice's running node"
else
    fail "handoff: $handoff (see $T/alice-up.log)"
fi
# Waits up to $1 seconds for bob's status to match the glob $2.
bob_status_matches() {
    local deadline=$((SECONDS + $1))
    until [[ $(pst bob status 2>&1) == $2 ]]; do
        ((SECONDS < deadline)) || return 1
        sleep 0.5
    done
}
if bob_status_matches 15 '*alice*LIVE*direct,*ms*'; then
    pass "bob's status shows alice LIVE over a direct path"
else
    fail "status path: $(pst bob status 2>&1)"
fi
stopped=$(pst alice live --stop 2>&1)
if [[ $stopped == stopped ]] && bob_status_matches 10 '*alice*online*'; then
    pass "live --stop through the control socket ended the stream for bob"
else
    fail "stop: $stopped / $(pst bob status 2>&1)"
fi

# 7. --serve hands the stream to one HTTP player. It needs bob's own node, so
#    his `up` goes first.
kill -TERM "$BOB_UP"; wait "$BOB_UP" 2>/dev/null
pst alice live --source test >/dev/null
PORT=$((20000 + RANDOM % 20000))
"$PSTREAM" --home "$T/bob" watch alice --serve "127.0.0.1:$PORT" 2>"$T/bob-serve.log" &
PIDS+=($!)
BOB_SERVE=$!
if wait_for "$T/bob-serve.log" 'open http' 20; then
    timeout 20 ffmpeg -v error -i "http://127.0.0.1:$PORT/" -t 3 -c copy -f mpegts "$T/served.ts"
    served=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$T/served.ts" 2>/dev/null | head -1)
    # The client leaving is the player closing, so bob's watch ends by itself.
    if [[ ${served:-0} -gt 60 ]] && wait_for "$T/bob-serve.log" 'player closed' 10; then
        pass "--serve streamed $served frames to an HTTP client"
    else
        fail "--serve: frames=${served:-none} (see $T/bob-serve.log)"
    fi
else
    fail "--serve never listened (see $T/bob-serve.log)"
fi
wait "$BOB_SERVE" 2>/dev/null

# 8. A player command with {url} (how VLC is run) gets a one-time local URL
#    instead of stdin. ffmpeg stands in, recording 3 s of what it's served.
if PSTREAM_PLAYER="ffmpeg -v error -i {url} -t 3 -c copy -f mpegts $T/url.ts" \
    timeout 30 "$PSTREAM" --home "$T/bob" watch alice 2>"$T/bob-url.log" \
    && grep -q 'player closed' "$T/bob-url.log"; then
    n=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$T/url.ts" 2>/dev/null | head -1)
    if [[ ${n:-0} -gt 60 ]]; then
        pass "a {url} player got $n frames from its one-time URL"
    else
        fail "{url} player: frames=${n:-none} (see $T/bob-url.log)"
    fi
else
    fail "{url} player (see $T/bob-url.log)"
fi
pst alice live --stop >/dev/null

if ((FAILED)); then
    echo "logs kept in $T"
    exit 1
fi
rm -rf "$T"
echo "all passed"
