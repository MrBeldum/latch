#!/usr/bin/env bash
# Regression tests for the relay kit. Runs the real install-relay.sh against stubbed
# docker/iptables/sudo — no daemon, no network, no root, nothing is mutated outside
# a temp dir. The point is the failure paths: a run that reports success while the
# node is not actually relaying is the bug this file exists to catch.
#
#   ./selftest.sh
set -euo pipefail
cd "$(dirname "$0")"
KIT=$PWD

pass=0
fail=0
ok() {
	pass=$((pass + 1))
	echo "  ok   $*"
}
no() {
	fail=$((fail + 1))
	echo "  FAIL $*"
}
check() { # $1 = description, $2 = 0/1 result
	if [ "$2" = 0 ]; then ok "$1"; else no "$1"; fi
}
has() { grep -qF -- "$2" "$1"; }

make_stubs() { # $1 = sandbox dir
	local d=$1
	mkdir -p "$d/bin" "$d/kit" "$d/dev"
	: >"$d/dev/tun"
	: >"$d/calls.log"
	cp "$KIT/install-relay.sh" "$KIT/relay-entrypoint.sh" "$KIT/docker-compose.yml" \
		"$KIT/.env.example" "$d/kit/"

	echo "2026-09-14T12:00:00.000000000Z" >"$d/started_at"
	echo 0 >"$d/restarts"
	cat >"$d/bin/docker" <<-'STUB'
		#!/usr/bin/env bash
		echo "docker $*" >> "$STUB_LOG"
		cmd=$1
		shift || true
		marker_now() {
		  if [ -f "$STUB_DIR/marker" ]; then cat "$STUB_DIR/marker"
		  else echo "${STUB_CONFIG_MARKER:-OK}"
		  fi
		}
		emit_old() {
		  echo "latch-relay: RELAY CONFIG OK port=40000"
		  echo "magicsock: old-lifecycle chatter"
		}
		emit_current() {
		  case "$(marker_now)" in
		    FAILED) echo "latch-relay: RELAY CONFIG FAILED port=40000 - prefs not applied, this node is NOT a peer relay" ;;
		    OK) echo "latch-relay: RELAY CONFIG OK port=40000" ;;
		  esac
		  i=0
		  while [ "$i" -lt "${STUB_VERBOSE:-0}" ]; do
		    echo "magicsock: endpoint update $i"
		    i=$((i + 1))
		  done
		}
		case "$cmd" in
		  info) exit 0 ;;
		  compose)
		    case "$1" in
		      version) exit 0 ;;
		      config)
		        port=40000; wait=180
		        if [ -f .env ]; then
		          while IFS= read -r line || [ -n "$line" ]; do
		            case "$line" in
		              RELAY_PORT=*) port=${line#RELAY_PORT=} ;;
		              RELAY_WAIT_SECS=*) wait=${line#RELAY_WAIT_SECS=} ;;
		            esac
		          done < .env
		        fi
		        port=${port%% #*}; wait=${wait%% #*}
		        port=${port%\"}; port=${port#\"}
		        wait=${wait%\"}; wait=${wait#\"}
		        printf 'name: latch-relay\nservices:\n  relay:\n    environment:\n      RELAY_PORT: "%s"\n      RELAY_WAIT_SECS: "%s"\n' "$port" "$wait"
		        exit 0 ;;
		      pull) [ "${STUB_PULL_FAIL:-}" = 1 ] && exit 1; exit 0 ;;
		      up) echo "Container latch-relay  Started"; exit 0 ;;
		    esac ;;
		  volume)
		    [ -n "${STUB_STATE_DIR:-}" ] || exit 1
		    echo "$STUB_STATE_DIR"; exit 0 ;;
		  run)
		    # has_authenticated_state probes the volume through Docker, not sudo.
		    if [ -n "${STUB_STATE_DIR:-}" ] && [ -s "$STUB_STATE_DIR/tailscaled.state" ]; then
		      exit 0
		    fi
		    exit 1 ;;
		  exec)
		    case "$*" in
		      "latch-relay tailscale status --json")
		        printf '{\n\t"BackendState": "%s"\n}\n' "${STUB_BACKEND:-Running}"; exit 0 ;;
		      "latch-relay tailscale debug prefs")
		        if [ -n "${STUB_PREFS_PORT:-}" ]; then
		          printf '{\n\t"WantRunning": true,\n\t"RelayServerPort": %s\n}\n' "$STUB_PREFS_PORT"
		        else
		          printf '{\n\t"WantRunning": true\n}\n'
		        fi; exit 0 ;;
		      "latch-relay tailscale status")
		        echo "100.64.0.1   latch-relay   me@   linux   -"; exit 0 ;;
		      "latch-relay tailscale version")
		        printf '1.104.1\n  tailscale commit: stub\n'; exit 0 ;;
		    esac ;;
		  inspect)
		    cat "$STUB_DIR/started_at"; exit 0 ;;
		  restart)
		    echo $(($(cat "$STUB_DIR/restarts") + 1)) >"$STUB_DIR/restarts"
		    echo "2026-09-14T13:00:00.000000000Z" >"$STUB_DIR/started_at"
		    if [ -n "${STUB_AFTER_RESTART_MARKER+x}" ]; then
		      echo "$STUB_AFTER_RESTART_MARKER" >"$STUB_DIR/marker"
		    fi
		    exit 0 ;;
		  logs)
		    since=""; tailn=""
		    while [ $# -gt 0 ]; do
		      case "$1" in
		        --since) since=$2; shift 2 ;;
		        --tail) tailn=$2; shift 2 ;;
		        *) shift ;;
		      esac
		    done
		    started=$(cat "$STUB_DIR/started_at")
		    body=$(mktemp)
		    if [ -n "$since" ] && [ "$since" = "$started" ]; then
		      emit_current >"$body"
		    elif [ -n "$tailn" ]; then
		      { [ "${STUB_OLD_OK:-}" = 1 ] && emit_old; emit_current; } | tail -n "$tailn" >"$body"
		    else
		      { [ "${STUB_OLD_OK:-}" = 1 ] && emit_old; emit_current; } >"$body"
		    fi
		    cat "$body"
		    rm -f "$body"
		    exit 0 ;;
		esac
		echo "STUB: unexpected docker $cmd $*" >&2
		exit 1
	STUB

	# Minimal iptables: the chain lives in a file and -I really renumbers it, so the
	# script's "insert above REJECT, then re-read and verify" path is exercised for real.
	cat >"$d/bin/iptables" <<-'STUB'
		#!/usr/bin/env bash
		set -eu
		echo "${0##*/} $*" >> "$STUB_LOG"
		chain="$STUB_DIR/${0##*/}.chain"
		[ -f "$chain" ] || : > "$chain"
		op=$1; pos=""; port=""
		[ "$op" = "-I" ] && pos=$3
		while [ $# -gt 0 ]; do [ "$1" = "--dport" ] && port=$2; shift; done
		rule="ACCEPT     udp  --  0.0.0.0/0            0.0.0.0/0            udp dpt:$port"
		case "$op" in
		-L) echo "Chain INPUT (policy ACCEPT)"
		    echo "num  target     prot opt source               destination"
		    n=0
		    while IFS= read -r l; do n=$((n+1)); printf '%d    %s\n' "$n" "$l"; done < "$chain" ;;
		-I) total=$(wc -l < "$chain")
		    if [ "$pos" -gt "$total" ]; then echo "$rule" >> "$chain"
		    else awk -v p="$pos" -v r="$rule" 'NR==p{print r} {print}' "$chain" > "$chain.t" && mv "$chain.t" "$chain"
		    fi ;;
		-A) echo "$rule" >> "$chain" ;;
		esac
	STUB
	cp "$d/bin/iptables" "$d/bin/ip6tables"
	printf '#!/usr/bin/env bash\nexec "$@"\n' >"$d/bin/sudo"
	printf '#!/usr/bin/env bash\nexit 1\n' >"$d/bin/systemctl"
	chmod +x "$d/bin/docker" "$d/bin/iptables" "$d/bin/ip6tables" "$d/bin/sudo" "$d/bin/systemctl"
}

# An INPUT chain shaped like the stock Oracle Linux one.
oracle_chain() {
	cat <<-'EOF'
		ACCEPT     all  --  0.0.0.0/0            0.0.0.0/0
		ACCEPT     tcp  --  0.0.0.0/0            0.0.0.0/0            tcp dpt:22
		REJECT     all  --  0.0.0.0/0            0.0.0.0/0            reject-with icmp-host-prohibited
	EOF
}

run_case() { # $1 = sandbox dir ; remaining env comes from the caller
	( # subshell: the stub environment must not leak into the next case
		cd "$1/kit"
		PATH="$1/bin:$PATH" \
			STUB_LOG="$1/calls.log" STUB_DIR="$1" \
			TUN_DEVICE="$1/dev/tun" AUTH_WAIT_SECS=4 PREFS_WAIT_SECS=4 \
			./install-relay.sh
	) >"$1/out" 2>&1
}

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/latch-relay-selftest.XXXXXX")
# Sibling named like the old glob. If cleanup still does rm -rf .../relaykit.*,
# this vanishes and the ownership check fails.
DECOY=$(mktemp -d "${TMPDIR:-/tmp}/relaykit.XXXXXX")
echo owned >"$DECOY/keep-me"

cleanup() {
	rm -rf "$WORKDIR"
	if [ ! -f "$DECOY/keep-me" ]; then
		echo "FAIL: cleanup deleted $DECOY, which this script does not own" >&2
		exit 1
	fi
	rm -rf "$DECOY"
}
trap cleanup EXIT

sandbox() { # prints a fresh sandbox dir with the Oracle chain loaded
	local d
	d=$(mktemp -d "$WORKDIR/sb.XXXXXX")
	make_stubs "$d"
	oracle_chain >"$d/iptables.chain"
	oracle_chain >"$d/ip6tables.chain"
	echo "$d"
}

echo "== unit selftests =="
"$KIT/install-relay.sh" selftest | sed 's/^/  /'
sh "$KIT/relay-entrypoint.sh" selftest | sed 's/^/  /'
pass=$((pass + 2))

echo "== case 1: relay prefs never take effect =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT="" STUB_CONFIG_MARKER=FAILED run_case "$d" || rc=$?
check "exits nonzero instead of reporting success" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "says the prefs did not take effect" "$(has "$d/out" 'relay prefs did NOT take effect' && echo 0 || echo 1)"
check "does not print Done" "$(! has "$d/out" 'Done. Next' && echo 0 || echo 1)"
check "surfaces the entrypoint's failure marker" "$(has "$d/out" 'RELAY CONFIG FAILED' && echo 0 || echo 1)"

echo "== case 2: malformed RELAY_PORT =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=99999999\n' >"$d/kit/.env"
rc=0
run_case "$d" || rc=$?
check "exits nonzero" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "names the bad value in full, untruncated" "$(has "$d/out" "RELAY_PORT='99999999'" && echo 0 || echo 1)"
check "no firewall rule was touched" "$(! grep -q '^ip6\?tables -[IA]' "$d/calls.log" && echo 0 || echo 1)"
check "no container was started" "$(! has "$d/calls.log" 'compose up' && echo 0 || echo 1)"
check "no image was pulled" "$(! has "$d/calls.log" 'compose pull' && echo 0 || echo 1)"

echo "== case 2b: malformed RELAY_WAIT_SECS =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_WAIT_SECS=0\n' >"$d/kit/.env"
rc=0
run_case "$d" || rc=$?
check "exits nonzero" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "no container was started" "$(! has "$d/calls.log" 'compose up' && echo 0 || echo 1)"
check "no image was pulled" "$(! has "$d/calls.log" 'compose pull' && echo 0 || echo 1)"

echo "== case 3: authenticated node, no auth key in .env =="
d=$(sandbox)
mkdir -p "$d/state"
echo '{"_machinekey":"redacted"}' >"$d/state/tailscaled.state"
printf 'RELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_STATE_DIR="$d/state" STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "exits zero without demanding a fresh key" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "says it is reusing the persisted node" "$(has "$d/out" 'already holds an authenticated node' && echo 0 || echo 1)"
check "verifies RelayServerPort from the node itself" "$(has "$d/out" 'RelayServerPort = 40000' && echo 0 || echo 1)"
check "no auth key was printed" "$(! grep -qi 'tskey' "$d/out" && echo 0 || echo 1)"

echo "== case 4: no key and no state =="
d=$(sandbox)
printf 'RELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
run_case "$d" || rc=$?
check "exits nonzero" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "prints resume instructions" "$(has "$d/out" 'Re-run ./install-relay.sh' && echo 0 || echo 1)"
check "no container was started" "$(! has "$d/calls.log" 'compose up' && echo 0 || echo 1)"
check "no image was pulled" "$(! has "$d/calls.log" 'compose pull' && echo 0 || echo 1)"

echo "== case 5: an ACCEPT rule exists but sits below the catch-all REJECT =="
d=$(sandbox)
{
	oracle_chain
	echo 'ACCEPT     udp  --  0.0.0.0/0            0.0.0.0/0            udp dpt:40000'
} >"$d/iptables.chain"
cp "$d/iptables.chain" "$d/ip6tables.chain"
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "calls the dead rule out instead of trusting it" "$(has "$d/out" 'never matches' && echo 0 || echo 1)"
check "inserts at the REJECT's line, not append" "$(has "$d/calls.log" 'iptables -I INPUT 3 -p udp --dport 40000 -j ACCEPT' && echo 0 || echo 1)"
check "re-reads and confirms the new rule is above REJECT" "$(has "$d/out" 'udp/40000 now allowed at line 3' && echo 0 || echo 1)"

echo "== case 5b: iptables 1.8 prints the protocol as 17, and the rule is live =="
d=$(sandbox)
{
	oracle_chain | sed '$d'
	echo 'ACCEPT     17   --  0.0.0.0/0            0.0.0.0/0            udp dpt:40000'
	oracle_chain | tail -n 1
} >"$d/iptables.chain"
cp "$d/iptables.chain" "$d/ip6tables.chain"
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "recognises the numeric-protocol rule" "$(has "$d/out" 'udp/40000 already allowed at line 3' && echo 0 || echo 1)"
check "does not insert a duplicate" "$(! has "$d/calls.log" 'iptables -I INPUT' && echo 0 || echo 1)"
check "does not say it is closed" "$(! has "$d/out" 'is NOT open for' && echo 0 || echo 1)"

echo "== case 6: ip6tables missing =="
d=$(sandbox)
rm "$d/bin/ip6tables"
# CI images ship /usr/sbin/ip6tables. run_case keeps the host PATH, so after the
# stub is removed command -v finds the real binary and open_udp takes the
# permissions-failure path instead of "not installed". Drop sbin so only the
# sandbox stubs (plus /usr/bin:/bin) remain visible for this case.
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
(
	cd "$d/kit"
	PATH="$d/bin:/usr/bin:/bin" \
		STUB_LOG="$d/calls.log" STUB_DIR="$d" \
		TUN_DEVICE="$d/dev/tun" AUTH_WAIT_SECS=4 PREFS_WAIT_SECS=4 \
		STUB_PREFS_PORT=40000 \
		./install-relay.sh
) >"$d/out" 2>&1 || rc=$?
check "still completes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "says IPv6 was not opened" "$(has "$d/out" 'ip6tables is not installed' && echo 0 || echo 1)"
check "repeats it in the final summary" "$(has "$d/out" 'is NOT open for: IPv6' && echo 0 || echo 1)"
check "does not claim both families" "$(! has "$d/out" 'ip6tables: udp/40000 now allowed' && echo 0 || echo 1)"

echo "== case 7: persistence package installed after first run =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "run 1 completes" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "run 1 warns persistence is missing" "$(has "$d/out" 'netfilter-persistent is not installed' && echo 0 || echo 1)"
check "run 1 did not call save" "$(! grep -q 'netfilter-persistent save' "$d/calls.log" && echo 0 || echo 1)"
printf '#!/usr/bin/env bash\necho "netfilter-persistent $*" >> "$STUB_LOG"\n' >"$d/bin/netfilter-persistent"
chmod +x "$d/bin/netfilter-persistent"
: >"$d/calls.log"
rc=0
STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "run 2 completes with rules already effective" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "run 2 saved existing rules" "$(has "$d/calls.log" 'netfilter-persistent save' && echo 0 || echo 1)"
check "run 2 did not insert again" "$(! grep -qE 'iptables -[IA]' "$d/calls.log" && echo 0 || echo 1)"

echo "== case 8: duplicate RELAY_PORT, last assignment wins =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=11111\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "opened the last port, not the first" "$(has "$d/calls.log" '--dport 40000' && echo 0 || echo 1)"
check "did not open the first port" "$(! grep -q 'dport 11111' "$d/calls.log" && echo 0 || echo 1)"

echo "== case 9: tailscale set failed, stale RelayServerPort still matches =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 STUB_CONFIG_MARKER=FAILED run_case "$d" || rc=$?
check "exits nonzero" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "does not print Done" "$(! has "$d/out" 'Done. Next' && echo 0 || echo 1)"
check "surfaces CONFIG FAILED, not leftover port" "$(has "$d/out" 'RELAY CONFIG FAILED' && echo 0 || echo 1)"

echo "== case 10: healthy rerun, marker buried under >50 chatter lines =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 STUB_VERBOSE=500 run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "did not restart a healthy relay" "$(! grep -q '^docker restart' "$d/calls.log" && echo 0 || echo 1)"
check "read logs since StartedAt, not tail 50" "$(has "$d/calls.log" 'logs --since' && echo 0 || echo 1)"
check "did not dump magicsock chatter" "$(! grep -q magicsock "$d/out" && echo 0 || echo 1)"

echo "== case 11: old-start OK must not mask this-start with no marker =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 STUB_OLD_OK=1 STUB_CONFIG_MARKER=ABSENT STUB_AFTER_RESTART_MARKER=FAILED run_case "$d" || rc=$?
check "exits nonzero" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "does not print Done" "$(! has "$d/out" 'Done. Next' && echo 0 || echo 1)"
check "restarted once" "$([ "$(cat "$d/restarts")" = 1 ] && echo 0 || echo 1)"

echo "== case 12: late-login rerun recovers with one restart =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 STUB_CONFIG_MARKER=FAILED STUB_AFTER_RESTART_MARKER=OK run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "restarted once" "$([ "$(cat "$d/restarts")" = 1 ] && echo 0 || echo 1)"
check "prints Done" "$(has "$d/out" 'Done. Next' && echo 0 || echo 1)"

echo "== case 13: restart still FAILED does not loop =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 STUB_CONFIG_MARKER=FAILED STUB_AFTER_RESTART_MARKER=FAILED run_case "$d" || rc=$?
check "exits nonzero" "$([ "$rc" -ne 0 ] && echo 0 || echo 1)"
check "restarted once, not twice" "$([ "$(cat "$d/restarts")" = 1 ] && echo 0 || echo 1)"
check "does not print Done" "$(! has "$d/out" 'Done. Next' && echo 0 || echo 1)"

echo "== case 14: the image is pulled before the relay starts =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
pull_at=$(grep -n 'compose pull' "$d/calls.log" | head -n 1 | cut -d: -f1)
up_at=$(grep -n 'compose up' "$d/calls.log" | head -n 1 | cut -d: -f1)
check "pulls, then starts" "$([ -n "$pull_at" ] && [ -n "$up_at" ] && [ "$pull_at" -lt "$up_at" ] && echo 0 || echo 1)"
check "says which tailscale runs" "$(has "$d/out" 'tailscale 1.104.1' && echo 0 || echo 1)"

echo "== case 15: a failed pull still starts the relay =="
d=$(sandbox)
printf 'TS_AUTHKEY=tskey-not-a-real-key\nRELAY_PORT=40000\n' >"$d/kit/.env"
rc=0
STUB_PREFS_PORT=40000 STUB_PULL_FAIL=1 run_case "$d" || rc=$?
check "exits zero" "$([ "$rc" -eq 0 ] && echo 0 || echo 1)"
check "warns about the pull" "$(has "$d/out" 'could not pull the relay image' && echo 0 || echo 1)"
check "starts the relay anyway" "$(has "$d/calls.log" 'compose up' && echo 0 || echo 1)"

echo "== cleanup owns only its workdir =="
check "decoy sibling still present" "$([ -f "$DECOY/keep-me" ] && echo 0 || echo 1)"

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
