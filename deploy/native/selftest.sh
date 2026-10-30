#!/usr/bin/env bash
# Run the actual unit's cookie command repeatedly without starting Xvfb.
set -euo pipefail
cd "$(dirname "$0")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
command="$(sed -n "s/^ExecStartPre=\/bin\/sh -c '\(.*\)'$/\1/p" latch-display.service)"
[[ -n "$command" ]]
command="${command//%t/$work}"
for attempt in 1 2 3 4; do
    /bin/sh -c "$command"
    entries="$(/usr/bin/xauth -f "$work/latch.Xauthority" list | wc -l)"
    [[ "$entries" -eq 1 ]] || { echo "expected one cookie, got $entries" >&2; exit 1; }
done
echo 'display cookie replacement: passed'
