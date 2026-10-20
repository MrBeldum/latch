#!/usr/bin/env bash
# Bring up (or re-converge) the Latch peer relay on this host. Safe to run twice:
# every step checks before it changes anything, and the node keeps its identity
# because state lives in a named volume.
#
# Usage: ./install-relay.sh            (also: ./install-relay.sh selftest, ./selftest.sh)
#
# Never `set -x` in here: .env holds an auth key.
set -euo pipefail

cd "$(dirname "$0")"

CONTAINER=latch-relay
STATE_VOLUME=latch-relay_relay-state
# Overridable so ./selftest.sh can drive the real script instead of a patched copy.
TUN_DEVICE=${TUN_DEVICE:-/dev/net/tun}
AUTH_WAIT_SECS=${AUTH_WAIT_SECS:-120}
PREFS_WAIT_SECS=${PREFS_WAIT_SECS:-60}

die() {
	echo "install-relay: $*" >&2
	exit 1
}
say() { echo "==> $*"; }
warn() { echo "==> WARNING: $*" >&2; }

# --- pure helpers; all of these are covered by ./selftest.sh ------------------

# First REJECT/DROP line number in `iptables -L INPUT --line-numbers`, or nothing.
# The Oracle Linux image ends INPUT with a catch-all REJECT, so an appended ACCEPT
# rule would never be reached — the new rule has to be inserted above it.
first_reject_line() {
	awk '$2 == "REJECT" || $2 == "DROP" { print $1; exit }'
}

# Line number of the first `ACCEPT udp dpt:<port>` rule, or nothing. Compares the
# dpt: field exactly, so a rule for port 4000 is not mistaken for one for 40000.
# `iptables -n` on 1.8+ prints the protocol as its number (17), older builds as udp.
accept_line() {
	awk -v want="dpt:$1" '$2 == "ACCEPT" && ($3 == "udp" || $3 == "17") {
		for (i = 5; i <= NF; i++) if ($i == want) { print $1; exit }
	}'
}

# 0 when an ACCEPT at line $1 is actually reached: it exists and sits before the
# catch-all REJECT at line $2 (or there is no catch-all at all). An ACCEPT *after*
# the REJECT is dead, so `iptables -C` succeeding means nothing on its own.
rule_is_effective() {
	[ -n "$1" ] || return 1
	[ -n "$2" ] || return 0
	[ "$1" -lt "$2" ]
}

# RelayServerPort out of `tailscale debug prefs`. Default output is indented JSON
# ("RelayServerPort": 40000); --pretty prints relayServerPort=40000. Both accepted.
# The field is omitempty, so nothing is printed when the relay is not configured.
relay_port_from_prefs() {
	sed -n \
		-e 's/.*"RelayServerPort"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' \
		-e 's/.*relayServerPort=\([0-9][0-9]*\).*/\1/p' | head -n 1
}

# 0 when $1 is a plain decimal integer within [$2,$3]. Rejects empty, negative,
# non-numeric and absurdly long input rather than letting it be truncated.
is_uint() {
	case "${1:-}" in '' | *[!0-9]*) return 1 ;; esac
	[ "${#1}" -le 10 ] || return 1
	[ "$1" -ge "$2" ] && [ "$1" -le "$3" ]
}

# Key $1 from `docker compose config` YAML on stdin. Last match wins (Compose's
# env map); surrounding quotes stripped. Prints only that value, so a pipeline
# that extracts RELAY_PORT cannot leak TS_AUTHKEY.
yaml_env_value() {
	awk -v k="$1:" '
		$1 == k {
			v = $2
			for (i = 3; i <= NF; i++) v = v " " $i
			if (v ~ /^".*"$/) { sub(/^"/, "", v); sub(/"$/, "", v) }
			last = v
		}
		END { if (last != "") print last }
	'
}

# Negative asserts are written `! cmd || die`, never `cmd && die`: under `set -e` a
# failing left side of && aborts the shell, so a passing assert would exit 1 silently.
selftest() {
	local fixture out tmp
	fixture='Chain INPUT (policy ACCEPT)
num  target     prot opt source               destination
1    ACCEPT     all  --  0.0.0.0/0            0.0.0.0/0
2    ACCEPT     tcp  --  0.0.0.0/0            0.0.0.0/0            tcp dpt:22
3    ACCEPT     udp  --  0.0.0.0/0            0.0.0.0/0            udp dpt:4000
4    REJECT     all  --  0.0.0.0/0            0.0.0.0/0            reject-with icmp-host-prohibited
5    ACCEPT     udp  --  0.0.0.0/0            0.0.0.0/0            udp dpt:40000'

	out=$(printf '%s\n' "$fixture" | first_reject_line)
	[ "$out" = "4" ] || die "selftest: reject line, got '$out'"
	out=$(printf 'Chain INPUT (policy ACCEPT)\nnum  target\n1    ACCEPT     all\n' | first_reject_line)
	[ -z "$out" ] || die "selftest: no-reject chain, got '$out'"

	out=$(printf '%s\n' "$fixture" | accept_line 40000)
	[ "$out" = "5" ] || die "selftest: accept line, got '$out'"
	out=$(printf '%s\n' "$fixture" | accept_line 4000)
	[ "$out" = "3" ] || die "selftest: dpt:4000 must not match dpt:40000, got '$out'"
	out=$(printf '%s\n' "$fixture" | accept_line 9999)
	[ -z "$out" ] || die "selftest: absent port matched '$out'"

	# The whole point: rule 5 exists but is below the REJECT at 4, so it is dead.
	! rule_is_effective 5 4 || die "selftest: ACCEPT below REJECT treated as effective"
	rule_is_effective 3 4 || die "selftest: ACCEPT above REJECT treated as dead"
	rule_is_effective 5 '' || die "selftest: ACCEPT with no REJECT treated as dead"
	! rule_is_effective '' 4 || die "selftest: missing ACCEPT treated as effective"

	out=$(printf '{\n\t"WantRunning": true,\n\t"RelayServerPort": 40000\n}\n' | relay_port_from_prefs)
	[ "$out" = "40000" ] || die "selftest: prefs json, got '$out'"
	out=$(printf 'relayServerPort=41999 routes=[]\n' | relay_port_from_prefs)
	[ "$out" = "41999" ] || die "selftest: prefs pretty, got '$out'"
	out=$(printf '{\n\t"WantRunning": true\n}\n' | relay_port_from_prefs)
	[ -z "$out" ] || die "selftest: unset prefs produced '$out'"

	is_uint 40000 1 65535 || die "selftest: 40000 rejected"
	is_uint 1 1 65535 || die "selftest: 1 rejected"
	is_uint 65535 1 65535 || die "selftest: 65535 rejected"
	! is_uint 65536 1 65535 || die "selftest: 65536 accepted"
	! is_uint 0 1 65535 || die "selftest: 0 accepted"
	! is_uint '' 1 65535 || die "selftest: empty accepted"
	! is_uint 40000abc 1 65535 || die "selftest: trailing junk accepted"
	! is_uint ' 40000' 1 65535 || die "selftest: leading space accepted"
	! is_uint -1 1 65535 || die "selftest: negative accepted"
	! is_uint 99999999999999999999 1 65535 || die "selftest: overlong accepted"

	# Last match wins, matching Compose's env map. Extracting RELAY_PORT must
	# not print the neighbouring TS_AUTHKEY.
	out=$(printf '      RELAY_PORT: "11111"\n      TS_AUTHKEY: tskey-should-never-appear\n      RELAY_PORT: "41999"\n      RELAY_WAIT_SECS: "60"\n' | yaml_env_value RELAY_PORT)
	[ "$out" = "41999" ] || die "selftest: yaml last-wins, got '$out'"
	case "$out" in *tskey*) die "selftest: auth key leaked through yaml_env_value" ;; esac
	out=$(printf '      RELAY_PORT: "11111"\n      RELAY_WAIT_SECS: "60"\n' | yaml_env_value RELAY_WAIT_SECS)
	[ "$out" = "60" ] || die "selftest: yaml wait, got '$out'"
	out=$(printf '      RELAY_PORT: 40000\n' | yaml_env_value RELAY_PORT)
	[ "$out" = "40000" ] || die "selftest: unquoted yaml, got '$out'"
	out=$(printf '      TS_HOSTNAME: latch-relay\n' | yaml_env_value RELAY_PORT)
	[ -z "$out" ] || die "selftest: absent yaml key produced '$out'"
	out=$(printf '      RELAY_PORT: "99999999"\n' | yaml_env_value RELAY_PORT)
	[ "$out" = "99999999" ] || die "selftest: malformed port was silently repaired to '$out'"
	! is_uint "$out" 1 65535 || die "selftest: 99999999 accepted as a port"
	out=$(printf '      RELAY_PORT: "abc"\n' | yaml_env_value RELAY_PORT)
	[ "$out" = "abc" ] || die "selftest: non-numeric port became '$out'"

	echo "install-relay selftest OK"
}

if [ "${1:-}" = selftest ]; then
	selftest
	exit 0
fi

# --- prerequisites -----------------------------------------------------------
command -v docker >/dev/null 2>&1 || die "docker is not installed. See RELAY.md."
docker compose version >/dev/null 2>&1 || die "the docker compose plugin is missing (docker-compose v1 is not supported)."
docker info >/dev/null 2>&1 || die "cannot talk to the docker daemon. Run as root or add yourself to the docker group."
[ -e "$TUN_DEVICE" ] || die "$TUN_DEVICE is missing. The relay needs kernel networking; load the tun module."

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
	command -v sudo >/dev/null 2>&1 && SUDO=sudo
fi

# A native tailscaled would fight this container for the host's tailnet identity.
if command -v tailscaled >/dev/null 2>&1 || systemctl status tailscaled >/dev/null 2>&1; then
	die "a native tailscaled exists on this host. Remove it or run the relay without host networking; do not run both."
fi

[ -f .env ] || cp .env.example .env
chmod 600 .env

# --- validate the values Compose will actually run with ----------------------
# One parser: `docker compose config` is what `up` uses. A second .env reader
# would disagree on duplicate keys, quotes, and shell-env overrides.
if ! port=$(docker compose config | yaml_env_value RELAY_PORT); then
	die "docker compose config failed. Nothing was changed."
fi
port=${port:-40000}
is_uint "$port" 1 65535 ||
	die "RELAY_PORT='$port' (from docker compose config) is not a UDP port (1-65535). Nothing was changed."

if ! wait_secs=$(docker compose config | yaml_env_value RELAY_WAIT_SECS); then
	die "docker compose config failed. Nothing was changed."
fi
wait_secs=${wait_secs:-180}
is_uint "$wait_secs" 5 3600 ||
	die "RELAY_WAIT_SECS='$wait_secs' (from docker compose config) is not a number of seconds (5-3600). Nothing was changed."

# --- auth gate ---------------------------------------------------------------
# An authenticated node does not need the key again: containerboot with
# TS_AUTH_ONCE=true only logs in when the state directory has no identity yet.
# This probes the size of the state file, never its contents.
has_authenticated_state() {
	docker volume inspect "$STATE_VOLUME" >/dev/null 2>&1 || return 1
	# The volume's host mountpoint is root-owned. Probe through Docker so a
	# docker-group user without sudo still sees an authenticated node.
	docker run --rm -v "$STATE_VOLUME":/var/lib/tailscale:ro alpine:3.20 \
		test -s /var/lib/tailscale/tailscaled.state >/dev/null 2>&1
}

if grep -Eq '^TS_AUTHKEY=.+' .env; then
	: # a key is present; containerboot decides whether it is needed
elif has_authenticated_state; then
	say "no TS_AUTHKEY in .env, but $STATE_VOLUME already holds an authenticated node — reusing it"
else
	cat >&2 <<-EOF

		install-relay: no TS_AUTHKEY, and no authenticated state in $STATE_VOLUME,
		so nothing was started. Nothing else changed.

		To resume:
		  1. Create a reusable, NON-ephemeral auth key tagged tag:relay at
		     https://login.tailscale.com/admin/settings/keys
		  2. Put it in $(pwd)/.env as TS_AUTHKEY=tskey-auth-...
		     (.env is gitignored and now chmod 600; do not paste the key into a traced shell)
		  3. Add the peer-relay grant from RELAY.md to your tailnet policy file.
		  4. Re-run ./install-relay.sh

		Once the node has authenticated once you can delete the key from .env;
		re-runs use the persisted state instead.

	EOF
	exit 1
fi

# --- firewall ----------------------------------------------------------------
FIREWALL_MISSING=""

family_of() { case "$1" in ip6tables) echo IPv6 ;; *) echo IPv4 ;; esac; }

open_udp() { # $1 = iptables|ip6tables, $2 = port ; 0 only when the port is reachable
	local cmd=$1 port=$2 listing accept reject
	if ! command -v "$cmd" >/dev/null 2>&1; then
		warn "$cmd is not installed: $(family_of "$cmd") udp/$port was NOT opened."
		return 1
	fi
	if ! listing=$($SUDO "$cmd" -L INPUT -n --line-numbers 2>/dev/null); then
		warn "$cmd -L INPUT failed (permissions?): $(family_of "$cmd") udp/$port was NOT verified."
		return 1
	fi

	accept=$(printf '%s\n' "$listing" | accept_line "$port")
	reject=$(printf '%s\n' "$listing" | first_reject_line)
	if rule_is_effective "$accept" "$reject"; then
		say "$cmd: udp/$port already allowed at line $accept${reject:+ (above REJECT at $reject)}"
		return 0
	fi
	if [ -n "$accept" ]; then
		warn "$cmd: an ACCEPT for udp/$port exists at line $accept but the catch-all REJECT is at line $reject, so it never matches. Inserting an effective rule; the dead one is left alone."
	fi

	if [ -n "$reject" ]; then
		$SUDO "$cmd" -I INPUT "$reject" -p udp --dport "$port" -j ACCEPT
	else
		$SUDO "$cmd" -A INPUT -p udp --dport "$port" -j ACCEPT
	fi

	# Re-read: report what the chain now says, not what the insert was meant to do.
	listing=$($SUDO "$cmd" -L INPUT -n --line-numbers 2>/dev/null) || listing=""
	accept=$(printf '%s\n' "$listing" | accept_line "$port")
	reject=$(printf '%s\n' "$listing" | first_reject_line)
	if rule_is_effective "$accept" "$reject"; then
		say "$cmd: udp/$port now allowed at line $accept${reject:+ (above REJECT at $reject)}"
		return 0
	fi
	warn "$cmd: could not make udp/$port effective (ACCEPT ${accept:-none}, REJECT ${reject:-none})."
	return 1
}

open_udp iptables "$port" || FIREWALL_MISSING="$FIREWALL_MISSING IPv4"
open_udp ip6tables "$port" || FIREWALL_MISSING="$FIREWALL_MISSING IPv6"

# Save whenever the tool exists, even if no rule was inserted this run: the
# documented recovery is "install iptables-persistent, then re-run".
if command -v netfilter-persistent >/dev/null 2>&1; then
	if $SUDO netfilter-persistent save >/dev/null; then
		say "firewall rules saved (netfilter-persistent)"
	else
		warn "netfilter-persistent save failed; the rules will not survive a reboot."
	fi
else
	warn "netfilter-persistent is not installed; these rules are lost on reboot."
	say "         apt-get install -y iptables-persistent, then re-run, then reboot-test."
fi
say "Cloud firewall is separate: the provider's security list must also allow udp/$port (v4 and v6)."

# --- run ---------------------------------------------------------------------
# Logs are bounded to this container start. `docker logs --tail 50` loses the
# startup marker under minutes of tailscaled chatter, and logs persist across
# restart so a previous OK would mask this start's FAILED.
container_started_at() {
	docker inspect -f '{{.State.StartedAt}}' "$CONTAINER" 2>/dev/null || true
}

# Only `latch-relay:` lines from the current start — never dump tailscaled.
configurator_lines() {
	local since
	since=$(container_started_at)
	[ -n "$since" ] || return 0
	docker logs --since "$since" "$CONTAINER" 2>&1 | grep 'latch-relay:' || true
}

current_config_marker() {
	configurator_lines | grep 'RELAY CONFIG ' | tail -n 1 || true
}

wait_until_running() {
	local deadline=$((SECONDS + AUTH_WAIT_SECS))
	state=""
	while :; do
		state=$(docker exec "$CONTAINER" tailscale status --json 2>/dev/null |
			sed -n 's/.*"BackendState"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1) || true
		[ "$state" = Running ] && return 0
		[ "$SECONDS" -ge "$deadline" ] && return 1
		sleep 2
	done
}

wait_for_prefs_marker() {
	local deadline=$((SECONDS + PREFS_WAIT_SECS))
	actual=""
	marker=""
	while :; do
		actual=$(docker exec "$CONTAINER" tailscale debug prefs 2>/dev/null | relay_port_from_prefs) || actual=""
		marker=$(current_config_marker)
		case "$marker" in
		*FAILED*) return 0 ;;
		*OK*) [ "$actual" = "$port" ] && return 0 ;;
		esac
		[ "$SECONDS" -ge "$deadline" ] && return 0
		sleep 2
	done
}

prefs_applied_this_start() {
	case "$marker" in
	*OK*) [ "$actual" = "$port" ] && return 0 ;;
	esac
	return 1
}

print_login_hint() {
	local since url
	since=$(container_started_at)
	url=""
	if [ -n "$since" ]; then
		url=$(docker logs --since "$since" "$CONTAINER" 2>&1 | grep -o 'https://login\.tailscale\.com/[A-Za-z0-9/]*' | tail -n 1) || url=""
	fi
	[ -n "$url" ] && echo "install-relay: finish login here, then re-run this script: $url" >&2
	echo "install-relay: a re-run against a Running node restarts the relay once so the configurator can apply prefs." >&2
}

config_failed() {
	echo >&2
	echo "install-relay: relay prefs did NOT take effect. configurator='${marker:-none}' RelayServerPort='${actual:-unset}' expected '$port'." >&2
	echo "install-relay: the container is running but this host is NOT a peer relay." >&2
	echo "install-relay: last lines from the configurator:" >&2
	configurator_lines >&2 || echo "  (no configurator output at all — is relay-entrypoint.sh mounted?)" >&2
	exit 1
}

# Compose only fetches an image it does not have, so without this a relay
# would run the tailscaled it was first installed with for good. A failed
# pull (registry down) keeps the copy already here; up -d recreates the
# container only when the image changed, and the state volume keeps the node.
say "pulling the relay image"
docker compose pull --quiet relay || warn "could not pull the relay image; starting the one already here"
say "starting relay"
docker compose up -d

say "waiting for the node to authenticate (up to ${AUTH_WAIT_SECS}s)"
if ! wait_until_running; then
	echo >&2
	echo "install-relay: node is '${state:-unknown}', not Running. The container is up and will keep trying." >&2
	print_login_hint
	exit 1
fi
say "node is Running"

say "waiting for relay prefs to take effect (up to ${PREFS_WAIT_SECS}s)"
wait_for_prefs_marker
if ! prefs_applied_this_start; then
	# Configurator runs once at start. FAILED/absent after a late login needs
	# one restart, not an endless loop and not a leftover OK from last start.
	say "this start did not apply relay prefs (marker='${marker:-none}'); restarting once"
	docker restart "$CONTAINER"
	if ! wait_until_running; then
		echo >&2
		echo "install-relay: node is '${state:-unknown}' after restart." >&2
		print_login_hint
		exit 1
	fi
	wait_for_prefs_marker
	prefs_applied_this_start || config_failed
fi

# --- summary ------------------------------------------------------------------
echo
docker exec "$CONTAINER" tailscale status || true
echo
say "RelayServerPort = $actual, read back from the node's own prefs"
say "tailscale $(docker exec "$CONTAINER" tailscale version 2>/dev/null | head -n 1 || true)"
docker exec "$CONTAINER" tailscale debug prefs 2>/dev/null | grep -i 'relayserverstatic' || true
if [ -n "$FIREWALL_MISSING" ]; then
	echo
	warn "udp/$port is NOT open for:${FIREWALL_MISSING}. Peers on those families cannot reach this relay until you open it yourself."
fi
echo
say "Done. Next: add the grant from RELAY.md, then from another device run"
say "  tailscale status | grep peer-relay"
