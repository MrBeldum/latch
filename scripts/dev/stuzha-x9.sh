#!/bin/sh
# stuzha-x9.sh start|stop: a private dummy-driver X server on :9 for engine
# tests, so the owner's :1 desktop is never injected into or resized.
# Runs as the calling user, like latch-display.service does for :1.
set -eu
run=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
auth=$run/latch-test9.Xauthority
conf=$run/latch-test9-xorg.conf
case ${1:-} in
start)
    umask 077
    : > "$auth"
    xauth -f "$auth" add :9 . "$(mcookie)"
    sed 's/Identifier "dummy"/Identifier "dummy9"/' /etc/X11/xorg-stuzha-dummy.conf > "$conf"
    Xorg :9 -config "$conf" -noreset -novtswitch -keeptty -nolisten tcp -auth "$auth" \
        -logfile "$run/latch-test9-Xorg.log" >/dev/null 2>&1 &
    for i in $(seq 1 50); do DISPLAY=:9 XAUTHORITY=$auth xdpyinfo >/dev/null 2>&1 && break; sleep 0.2; done
    DISPLAY=:9 XAUTHORITY=$auth xdpyinfo | grep dimensions
    echo "DISPLAY=:9 XAUTHORITY=$auth"
    ;;
stop)
    pkill -f "Xorg :9 -config $conf" || true
    pkill -f "ffplay.*latch-test9" || true
    rm -f "$auth" "$conf"
    ;;
*) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
