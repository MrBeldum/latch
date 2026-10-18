# Latch relay

A Latch stream is fastest when the two devices connect directly. When a strict NAT
or firewall blocks that, Tailscale falls back to a shared DERP server, which adds
latency and caps throughput. This kit turns a machine you own — a cheap cloud VPS is
ideal — into a **Tailscale peer relay**, so blocked streams go through your box
instead of DERP.

The order is Tailscale's own and needs no Latch code: **direct → peer relay → DERP.**
DERP stays as the last-resort fallback. If the relay is down, streaming still works,
just at DERP quality.

**A relay only serves its own tailnet.** There is no shared Latch relay and there
cannot be one: a peer relay can only relay for devices in the same tailnet as itself.
If you and a friend are on separate tailnets, each of you runs a relay for your own.

---

## Prerequisites

| | |
|---|---|
| Relay host | Any Linux box with a public IP. Not iOS/tvOS/Android. 1 vCPU is enough; bandwidth is what matters. |
| Docker | Engine with the `docker compose` plugin (v2). `docker-compose` v1 is not supported. |
| Kernel networking | `/dev/net/tun` present. The kit refuses userspace networking — it is too slow to relay. |
| No native tailscaled | The container uses host networking; a tailscaled installed on the host would fight it for the same interface. `install-relay.sh` refuses to run if it finds one. |
| Tailscale version | **1.86 or later on the relay and on every device that uses it.** Peer relay does not exist before 1.86; older clients silently stay on DERP. The entrypoint checks the relay's own version and refuses rather than pretending. |
| UDP port | One UDP port (default **40000**) reachable from the internet, open in **both** the host firewall and your cloud provider's security list, **IPv4 and IPv6**. |
| Tailnet role | Owner, Admin, or Network admin — you need to edit the policy file. |

## Install

```sh
cd deploy/relay
cp .env.example .env && chmod 600 .env
# put a reusable, NON-ephemeral auth key tagged tag:relay into .env
./install-relay.sh
```

`install-relay.sh` is idempotent — run it as many times as you like. It:

1. verifies docker, the compose plugin, `/dev/net/tun`, and the absence of a native tailscaled;
2. rejects a `RELAY_PORT` or `RELAY_WAIT_SECS` that is not a number in range **before**
   touching the firewall or starting anything, reading the values from
   `docker compose config` (what `up` will actually run with);
3. **stops with resume instructions if there is no `TS_AUTHKEY` and no already-authenticated
   node** — it does not start a half-configured relay;
4. inserts an `ACCEPT udp/<port>` rule *above* the distro's catch-all `REJECT` in `iptables`
   and `ip6tables`, re-reads the chain to confirm the rule is really reachable, and saves
   current rules with `netfilter-persistent` whenever that tool is installed (including a
   re-run after you install it);
5. starts the container and waits for the node to authenticate, printing the login URL if it needs one;
6. **waits for `RELAY CONFIG OK` in this container start's logs** (`docker logs --since
   StartedAt`, not a 50-line tail). A leftover port or an OK from a previous start is
   not success. If this start logged FAILED or nothing, it restarts **once** so a late
   login can apply prefs, then re-checks. A second failure exits non-zero.

**After the first successful login you can delete `TS_AUTHKEY` from `.env`.** The node's
identity lives in the `relay-state` volume, and `TS_AUTH_ONCE=true` means the key is only
consulted when there is no identity yet — so a re-run, a reboot or a `docker compose down
&& up` needs no key at all. Keeping a long-lived reusable key on disk forever is the
avoidable risk here.

Run `./selftest.sh` to exercise the installer's failure paths (bad port, prefs that never
land, a dead firewall rule below `REJECT`, a missing `ip6tables`, an authenticated node
with no key) against stubbed docker and iptables. It touches nothing outside a temp dir.

It never opens your cloud provider's security list — do that yourself, for UDP,
IPv4 and IPv6. And a firewall rule that has not survived a reboot is not done:
reboot the host and re-check `iptables -L INPUT --line-numbers`.

Rule *order* is what matters, not presence: `iptables -C` reports success for an
`ACCEPT` that sits below the catch-all `REJECT` and is therefore never reached. The
script compares line numbers, says so when it finds a dead rule, and inserts a
reachable one above the `REJECT` (leaving the dead duplicate alone — deleting rules
on someone else's firewall is not this script's business). If `iptables` or
`ip6tables` is missing it says which family was left closed instead of reporting
that both were opened.

## Policy file

Two pieces. Without the grant, the relay is a node like any other and nothing uses it.

```json
{
  "tagOwners": {
    "tag:relay": ["autogroup:admin"]
  },
  "grants": [
    {
      // Devices that may be reached through the relay.
      "src": ["autogroup:member"],
      // The relay itself.
      "dst": ["tag:relay"],
      "app": {
        // No parameters; the capability is the whole permission.
        "tailscale.com/cap/relay": []
      }
    }
  ]
}
```

`src` is *not* "who may use the relay" — it is the set of devices that other devices
are allowed to reach *through* the relay. `autogroup:member` means "the devices in my
tailnet", which for a personal tailnet is what you want.

**Do not use `"src": ["*"]`.** `*` includes devices that have no business being
relayed and sends traffic on detours that make latency worse. If you want to be
stricter than `autogroup:member`, tag the PCs that host Latch streams and narrow it:

```json
"src": ["tag:latch-host"]
```

Tailscale's own guidance: `src` should be devices in a stable location behind a strict
NAT — a desktop at home, a cloud VM — rather than laptops and phones that roam.

## Verify

From another device on the tailnet, generate some traffic, then:

```sh
tailscale status | grep peer-relay
```

A relayed peer prints its connection type as `peer-relay` plus the endpoint:

```
100.x.y.z  hostname  user@  windows  active; peer-relay 203.0.113.9:40000:vni:123, tx 12345 rx 67890
```

`relay "tok"` (or any other DERP region code) means it is still on DERP. `direct`
means it never needed a relay at all — that is the best case, not a failure.
`tailscale ping <host>` shows the same path.

After a restart, allow time for peers to rediscover the relay. The container can
already be `Running` while traffic temporarily uses DERP. Generate traffic and
repeat the path check for up to two minutes; verify an actual `peer-relay` path,
not only a healthy container or saved relay preferences.

## Operations

The default tailnet name is `relay` (`TS_HOSTNAME` in `.env`). To rename an
existing node, update `.env`, run
`docker exec latch-relay tailscale set --hostname=relay`, then
`docker compose up -d` to persist the container environment. This retains the
node identity, IP address, tags, and pairing.

**Identity survives restarts.** State lives in the `relay-state` named volume mounted
at `TS_STATE_DIR=/var/lib/tailscale`, and `TS_AUTH_ONCE=true` stops it re-logging in
on every boot. `docker compose down && docker compose up -d` keeps the same node — if
you ever see a second `relay` in the admin console, the volume was lost.
Deleting the volume is the only way to start over:
`docker compose down -v` (this forces a fresh login and a new node).

**Stale nodes after a re-login.** A `docker compose down -v` (or a `tailscale logout` inside
the container) leaves the previous node listed *offline* in the admin console, and the new
node may come up as `relay-1` because the old one still holds the plain name.
Nothing uses the offline node (no grant matches it, and Latch only looks at `tag:relay`
nodes that are online), so it is cosmetic. Delete it on the Machines page; there is no CLI
for that. The live relay is whichever `relay*` row shows `tag:relay` and is online.

**Relay prefs survive restarts too** — `tailscale set` writes them into that same
state. The entrypoint re-applies them on every start anyway, which is what picks up a
new public IP after a stop/start on an instance without a reserved address.

**Changing the port:** edit `RELAY_PORT` in `.env`, re-run `./install-relay.sh` (it
opens the new port), then remove the old rule by hand. The relay picks the new port up
on restart.

**Updating Tailscale.** The relay is an internet-facing tailscaled, and nothing inside
the container updates it: Compose fetches `tailscale/tailscale:latest` only when it has
no copy at all. Re-running `./install-relay.sh` pulls the current image first and
recreates the container only when the image changed (it prints the version it ends up
on); `docker compose pull && docker compose up -d` does the same by hand. The state
volume keeps the node, its address and its prefs. Streams through the relay fall back to
DERP for the few seconds the container restarts, so do it at a quiet time, monthly or
whenever Tailscale ships a security fix. A pinned `TS_IMAGE` is updated only by bumping
the pin.

**Turning it off:** `docker compose down`. Streams fall back to DERP by themselves.
To keep the node but stop relaying: `docker exec latch-relay tailscale set --relay-server-port=""`.

### Troubleshooting

| Symptom | Cause |
|---|---|
| `install-relay: no TS_AUTHKEY, and no authenticated state` | Expected on a first run. Nothing was started, nothing changed. Follow the printed steps. |
| `relay prefs did NOT take effect. RelayServerPort is 'unset'` | The container is up but not relaying. The script prints the configurator's own log lines underneath; look for `RELAY CONFIG FAILED`. Usual causes are the next two rows. |
| `RELAY_PORT='…' in .env is not a UDP port` | Nothing was changed. Fix the value; a trailing `# comment` is fine, anything non-numeric is not. |
| Container up, node `NeedsLogin` | The key was empty, expired, or single-use and already spent. `docker logs latch-relay \| grep login.tailscale.com` for a manual login URL. |
| `Relay prefs NOT applied` in the logs | The node never authenticated inside `RELAY_WAIT_SECS`. Finish the login, then `docker restart latch-relay`. |
| Node authenticates but peers still say `relay "xxx"` | The grant is missing, a peer is older than 1.86, or the UDP port is not actually reachable. Check the cloud security list, and `iptables -L INPUT --line-numbers` for a rule *above* `REJECT`. |
| Relay node disappears after a restart | The auth key was ephemeral. Create a non-ephemeral one. |
| `peer relay unavailable` at startup | The image is older than 1.86. Bump a pinned `TS_IMAGE`, or re-run `./install-relay.sh`, which pulls `latest` (an unpinned image is only fetched on a pull). |

## Files

| File | |
|---|---|
| `docker-compose.yml` | The relay service: host networking, kernel networking, `/dev/net/tun`, `NET_ADMIN`/`NET_RAW`, named state volume. |
| `relay-entrypoint.sh` | Wraps the stock entrypoint. Waits for the backend to reach `Running`, then applies the relay port and static endpoint, and logs `RELAY CONFIG OK` or `RELAY CONFIG FAILED` so a background failure cannot pass silently. The official image has no relay setting and runs no user command, so this wrapper is the supported way to do it. |
| `selftest.sh` | Stub-driven regression tests for the installer's failure paths. No docker, no network, no root. |
| `.env.example` | Every knob, no secrets. Copy to `.env`. |
| `.env` | **Your auth key. Gitignored. Never commit it, never paste it into a shell with tracing on.** |
| `install-relay.sh` | Idempotent installer described above. Pulls the image on every run. |
