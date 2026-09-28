#!/usr/bin/env bash
# End-to-end smoke test on one machine: three identities (alice streams, bob
# watches, carol is a stranger) talking over real iroh connections.
#
# Needs ffmpeg, ffprobe and mpv, plus dbus-daemon and python3-gi for the D-Bus
# notification check. Nothing pops up on screen: mpv runs with null
# outputs, and fake notification servers stand in for the desktop's.
# Holepunching between two processes on one host is trivial, so this proves
# the plumbing (identity, friend gate, MoQ announce/subscribe, TS import and
# export, the control socket and handoff to `up`, auto-open and notification
# paths, the HTTP sink), not NAT traversal.
# Usage: scripts/smoke.sh [path/to/kith]
set -uo pipefail

cd "$(dirname "$0")/.."
KITH=${1:-./target/debug/kith}
[[ $# -ge 1 ]] || cargo build --quiet || exit 1

# Honors $TMPDIR.
T=$(mktemp -d -t kith-smoke.XXXX)
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
kith() { "$KITH" --home "$T/$1" "${@:2}"; }

# Waits up to $3 seconds for $2 to appear in file $1.
wait_for() {
    local deadline=$((SECONDS + $3))
    until grep -q "$2" "$1" 2>/dev/null; do
        ((SECONDS < deadline)) || return 1
        sleep 0.2
    done
}

# Starts alice streaming the test pattern in the background. Called directly
# rather than through the `kith` function: a backgrounded function is a
# subshell, and the signal would go to the subshell instead of the binary.
start_alice() {
    "$KITH" --home "$T/alice" live --source test >"$T/alice.out" 2>"$T/alice-$1.log" &
    PIDS+=($!)
    ALICE=$!
    wait_for "$T/alice-$1.log" 'live as' 10 || fail "alice didn't go live ($1)"
}
stop_alice() {
    kill -TERM "$ALICE" 2>/dev/null
    wait "$ALICE" 2>/dev/null
}

A=$(kith alice id)
B=$(kith bob id)
kith alice friend add bob "$B" >/dev/null
kith bob friend add alice "$A" >/dev/null

# 1. Record alice's stream to a file and check what arrived.
start_alice record
timeout 12 "$KITH" --home "$T/bob" watch alice --output "$T/out.ts" 2>"$T/bob-record.log"
stop_alice
frames=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$T/out.ts" 2>/dev/null | head -1)
codecs=$(ffprobe -v error -show_entries stream=codec_name -of csv=p=0 "$T/out.ts" 2>/dev/null | sort -u | tr '\n' ' ')
if [[ ${frames:-0} -gt 150 && $codecs == *h264* && $codecs == *aac* ]]; then
    pass "recorded $frames video frames ($codecs) over iroh+MoQ"
else
    fail "recording: frames=${frames:-none} codecs=${codecs:-none} (see $T/bob-record.log)"
fi

# 2. Watch in the foreground with a player; mpv quits after 90 frames.
export KITH_PLAYER="mpv --no-config --really-quiet --vo=null --ao=null --frames=90 -"
start_alice player
if timeout 30 "$KITH" --home "$T/bob" watch alice 2>"$T/bob-player.log" \
    && grep -q 'player closed' "$T/bob-player.log"; then
    pass "foreground watch played 90 frames in mpv"
else
    fail "foreground watch (see $T/bob-player.log)"
fi
stop_alice

# 3. bob runs `up` with auto-open; alice going live opens bob's player on its own.
kith bob friend set alice --auto-open >/dev/null
"$KITH" --home "$T/bob" up >"$T/bob-up.out" 2>"$T/bob-up.log" &
PIDS+=($!)
BOB_UP=$!
wait_for "$T/bob-up.out" 'Kith is up' 10 || fail "bob's node didn't start"
start_alice auto
if wait_for "$T/bob-up.log" 'player closed' 30; then
    pass "auto-open: bob's node opened the player when alice went live"
else
    fail "auto-open (see $T/bob-up.log)"
fi
status=$(kith bob status 2>&1)
if [[ $status == *alice*LIVE* ]]; then
    pass "status over the control socket shows alice LIVE"
else
    fail "status: $status"
fi
stop_alice

# 4. Notification path: auto-open off, and the notification's Watch opens the
#    player. First over D-Bus, on a private session bus whose fake
#    notification server clicks Watch; then with no bus at all, where Kith
#    falls back to notify-send and a fake one clicks it.
kill -TERM "$BOB_UP"; wait "$BOB_UP" 2>/dev/null
kith bob friend set alice --no-auto-open >/dev/null
if command -v dbus-daemon >/dev/null && python3 -c 'import gi' 2>/dev/null; then
    cat >"$T/notifications.py" <<'PY'
import sys, warnings
from gi.repository import Gio, GLib
warnings.simplefilter("ignore", DeprecationWarning)
XML = """<node><interface name="org.freedesktop.Notifications">
<method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/>
<arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/>
<arg type="as" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/>
<arg type="u" direction="out"/></method>
<signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
<signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
</interface></node>"""
log = open(sys.argv[1], "a")
interface = Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0]
def call(bus, sender, path, iface, method, params, invocation):
    _, _, _, summary, _, actions, hints, _ = params.unpack()
    log.write(f"{summary}|{' '.join(actions)}|{' '.join(sorted(hints))}\n")
    log.flush()
    invocation.return_value(GLib.Variant("(u)", (7,)))
    def click():
        bus.emit_signal(None, path, iface, "ActionInvoked", GLib.Variant("(us)", (7, "watch")))
    if "watch" in actions:
        GLib.timeout_add(300, click)
def own(bus, name):
    bus.register_object("/org/freedesktop/Notifications", interface, call)
Gio.bus_own_name(Gio.BusType.SESSION, "org.freedesktop.Notifications", 0, own, None, None)
GLib.MainLoop().run()
PY
    { read -r BUS; read -r BUS_PID; } < <(dbus-daemon --session --fork --print-address=1 --print-pid=1)
    PIDS+=("$BUS_PID")
    DBUS_SESSION_BUS_ADDRESS=$BUS python3 "$T/notifications.py" "$T/dbus-notified" &
    PIDS+=($!)
    DBUS_SESSION_BUS_ADDRESS=$BUS "$KITH" --home "$T/bob" up >"$T/bob-dbus.out" 2>"$T/bob-dbus.log" &
    PIDS+=($!)
    BOB_UP=$!
    wait_for "$T/bob-dbus.out" 'Kith is up' 10 || fail "bob's node didn't restart (D-Bus)"
    start_alice dbus
    if wait_for "$T/bob-dbus.log" 'player closed' 30 &&
        grep -q '^alice is live|default Watch watch Watch|desktop-entry image-data$' "$T/dbus-notified"; then
        pass "notification over D-Bus: clicking Watch opened the player"
    else
        fail "D-Bus notification (see $T/bob-dbus.log and $T/dbus-notified)"
    fi
    stop_alice
    kill -TERM "$BOB_UP"; wait "$BOB_UP" 2>/dev/null
else
    echo "SKIP  notification over D-Bus (needs dbus-daemon and python3-gi)"
fi
mkdir -p "$T/bin"
printf '#!/bin/sh\necho "$@" >> %q\necho watch\n' "$T/notified" >"$T/bin/notify-send"
chmod +x "$T/bin/notify-send"
DBUS_SESSION_BUS_ADDRESS=disabled: PATH="$T/bin:$PATH" "$KITH" --home "$T/bob" up \
    >"$T/bob-up2.out" 2>"$T/bob-up2.log" &
PIDS+=($!)
BOB_UP=$!
wait_for "$T/bob-up2.out" 'Kith is up' 10 || fail "bob's node didn't restart"
start_alice notify
if wait_for "$T/bob-up2.log" 'player closed' 30 && grep -q 'alice is live' "$T/notified"; then
    pass "notification without D-Bus: notify-send's Watch opened the player"
else
    fail "notify-send notification (see $T/bob-up2.log)"
fi

# 5. carol isn't alice's friend, so alice refuses her.
C=$(kith carol id)
kith carol friend add alice "$A" >/dev/null
if timeout 30 "$KITH" --home "$T/carol" watch alice --output "$T/carol.ts" 2>"$T/carol.log"; then
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
"$KITH" --home "$T/alice" up >"$T/alice-up.out" 2>"$T/alice-up.log" &
PIDS+=($!)
wait_for "$T/alice-up.out" 'Kith is up' 10 || fail "alice's node didn't start"
handoff=$(kith alice live --source test 2>&1)
if [[ $handoff == live* ]] && wait_for "$T/alice-up.log" 'live as' 10; then
    pass "live handed off to alice's running node"
else
    fail "handoff: $handoff (see $T/alice-up.log)"
fi
# Waits up to $1 seconds for bob's status to match the glob $2.
bob_status_matches() {
    local deadline=$((SECONDS + $1))
    until [[ $(kith bob status 2>&1) == $2 ]]; do
        ((SECONDS < deadline)) || return 1
        sleep 0.5
    done
}
if bob_status_matches 15 '*alice*LIVE*direct,*ms*'; then
    pass "bob's status shows alice LIVE over a direct path"
else
    fail "status path: $(kith bob status 2>&1)"
fi
stopped=$(kith alice live --stop 2>&1)
if [[ $stopped == stopped ]] && bob_status_matches 10 '*alice*online*'; then
    pass "live --stop through the control socket ended the stream for bob"
else
    fail "stop: $stopped / $(kith bob status 2>&1)"
fi

# 7. What opening Kith again sends a running one: a kith://watch link opens
#    the stream, a bare `open` needs a window `up` doesn't have, and `quit`
#    stops it.
players=$(grep -c 'player closed' "$T/bob-up2.log")
kith alice live --source test >/dev/null
opened=$(kith bob open "kith://watch/$A" 2>&1)
deadline=$((SECONDS + 30))
until (($(grep -c 'player closed' "$T/bob-up2.log") > players)) || ((SECONDS > deadline)); do sleep 0.3; done
if (($(grep -c 'player closed' "$T/bob-up2.log") > players)); then
    pass "a kith://watch link handed to bob's node opened the player"
else
    fail "watch link: $opened (see $T/bob-up2.log)"
fi
kith alice live --stop >/dev/null
window=$(kith bob open 2>&1)
[[ $window == *"without its window"* ]] && pass "open says \`up\` has no window" || fail "open: $window"
kith bob quit >/dev/null
deadline=$((SECONDS + 10))
while kill -0 "$BOB_UP" 2>/dev/null && ((SECONDS < deadline)); do sleep 0.2; done
kill -0 "$BOB_UP" 2>/dev/null && fail "quit didn't stop bob's node" || pass "quit stopped bob's node"
wait "$BOB_UP" 2>/dev/null

# 8. --serve hands the stream to one HTTP player. It needs bob's own node,
#    which is why his `up` stopped first.
kith alice live --source test >/dev/null
PORT=$((20000 + RANDOM % 20000))
"$KITH" --home "$T/bob" watch alice --serve "127.0.0.1:$PORT" 2>"$T/bob-serve.log" &
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

# 9. A player command with {url} (how VLC is run) gets a one-time local URL
#    instead of stdin. ffmpeg stands in, recording 3 s of what it's served.
if KITH_PLAYER="ffmpeg -v error -i {url} -t 3 -c copy -f mpegts $T/url.ts" \
    timeout 30 "$KITH" --home "$T/bob" watch alice 2>"$T/bob-url.log" \
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
# 10. The browser fallback: a fake browser fetches what a real one would, the
#     page, mpegts.js and a stray favicon, then records 3 s of the stream.
cat >"$T/bin/fake-browser" <<EOF
#!/bin/sh
{
    curl -sf "\$1" >"$T/page.html"
    curl -sf "\$1mpegts.js" >"$T/mpegts.js"
    curl -s -o /dev/null -w '%{http_code}' "\$1../favicon.ico" >"$T/favicon.status"
    curl -s --max-time 3 "\$1stream.ts" >"$T/browser.ts"
} >/dev/null 2>&1 &
EOF
chmod +x "$T/bin/fake-browser"
if KITH_PLAYER=browser BROWSER="$T/bin/fake-browser" \
    timeout 30 "$KITH" --home "$T/bob" watch alice 2>"$T/bob-browser.log" \
    && grep -q 'player closed' "$T/bob-browser.log"; then
    n=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 "$T/browser.ts" 2>/dev/null | head -1)
    js=$(wc -c <"$T/mpegts.js")
    if [[ ${n:-0} -gt 30 && $js -gt 100000 && $(cat "$T/favicon.status") == 404 ]] \
        && grep -q 'src="mpegts.js"' "$T/page.html"; then
        pass "browser fallback served the page, mpegts.js ($js bytes) and $n frames"
    else
        fail "browser fallback: frames=${n:-none} js=$js favicon=$(cat "$T/favicon.status") (see $T/bob-browser.log)"
    fi
else
    fail "browser fallback (see $T/bob-browser.log)"
fi
kith alice live --stop >/dev/null

# 11. Invites: erin pastes alice's invite, and then each has the other,
#     without erin's code going anywhere. The invite works only once.
kith alice name Alice >/dev/null
kith erin name Erin >/dev/null
link=$(kith alice invite 2>/dev/null)
joined=$(kith erin join "$link" 2>&1)
if [[ $joined == *"You and Alice are friends"* ]] && kith alice friend ls | grep -q '^Erin ' \
    && kith erin friend ls | grep -q '^Alice '; then
    pass "an invite made alice and erin friends with each other"
else
    fail "invite: $joined / alice: $(kith alice friend ls | tr '\n' ' ')"
fi
again=$(kith dave name Dave && kith dave join "$link" 2>&1)
[[ $again == *"doesn't work anymore"* ]] && pass "a used invite is turned down" \
    || fail "a second use of the invite: $again"

# 12. Installing for this user: the program in ~/.local/bin, an app menu
#     entry that opens kith:// links, and the icon; uninstalling removes them.
#     A throwaway $HOME stands in for the real one.
F=$T/installhome
mkdir -p "$F"
as_user() { env -u XDG_DATA_HOME -u XDG_CONFIG_HOME HOME="$F" "$@"; }
as_user "$KITH" install >/dev/null
entry=$F/.local/share/applications/kith.desktop
if [[ -x $F/.local/bin/kith ]] && grep -qx 'MimeType=x-scheme-handler/kith;' "$entry" \
    && grep -qxF "Exec=$F/.local/bin/kith open %u" "$entry" \
    && [[ -f $F/.local/share/icons/hicolor/48x48/apps/kith.png ]] \
    && [[ $(as_user "$F/.local/bin/kith" --version) == "kith "* ]]; then
    pass "install: program, app menu entry for kith:// links, and icon"
else
    fail "install (see $entry)"
fi
as_user "$KITH" uninstall >/dev/null
if [[ ! -e $F/.local/bin/kith && ! -e $entry && ! -e $F/.local/share/icons/hicolor/48x48/apps/kith.png ]]; then
    pass "uninstall removed them"
else
    fail "uninstall left files in $F"
fi

if ((FAILED)); then
    echo "logs kept in $T"
    exit 1
fi
rm -rf "$T"
echo "all passed"
