# Latch node (Docker)

A container that **shares a desktop** on your Tailscale network. Any Latch
app — Mac, Windows, or another node — lists it and can Connect.

Use this for a VPS, or run several copies on one host so each container is
its own machine (its own Tailscale identity, its own desktop).

It is not the packet relay. The relay kit stays in `deploy/relay/`.

## What runs inside

- Xvfb + XFCE (a virtual 1920×1080 desktop)
- Latch 5 streaming engine (pure-Rust, in-process inside `latch-host`)
- Latch's control service on TCP 47850 and streaming on UDP 47850
- Optional Tailscale (`TS_AUTHKEY`), with kernel networking: the compose
  file gives the container `/dev/net/tun` and `NET_ADMIN`. Latch Host
  authorizes each peer from its own tailnet address; userspace Tailscale
  would hand it every peer from `127.0.0.1`, which it trusts as the
  machine itself, so the node refuses to start Tailscale without the device.

## One node

From the Latch repository:

```bash
cp deploy/node/env.example deploy/node/.env
# edit .env: paste a Tailscale auth key, pick TS_HOSTNAME
docker compose -f deploy/node/docker-compose.yml up -d --build
```

Sign the auth key in at [the admin console](https://login.tailscale.com/admin/machines)
if it asks. Open Latch on your Mac or Windows PC: the hostname appears
under **Your machines**. Click **Connect**.

## Several containers on one VPS

Each compose project needs its own name and hostname:

```bash
LATCH_CONTAINER=latch-node-a TS_HOSTNAME=vps-a \
  docker compose -p node-a -f deploy/node/docker-compose.yml up -d --build

LATCH_CONTAINER=latch-node-b TS_HOSTNAME=vps-b \
  docker compose -p node-b -f deploy/node/docker-compose.yml up -d --build
```

Give each a distinct `TS_AUTHKEY` (or the same reusable key; Tailscale
issues a new node per `up`). Do not put `tag:relay` on these nodes.

## Same host as the peer relay

The relay container uses host networking and `tag:relay`. A node container
should **not** use host networking: its own network namespace and its own
tailscaled give it a second identity, so it shows up as a machine instead
of being hidden as a relay.

## Build notes

The image compiles `latch-host` (with `--locked`, from the dependencies
`Cargo.lock` pins) and installs the Ubuntu 24.04 Sunshine
package pinned to the same tag Windows setup uses (`v2026.906.222525`).
The VPS in this project is aarch64; the Dockerfile picks the arm64 or
amd64 `.deb` from `TARGETARCH`.

`.dockerignore` keeps every `.env` out of the build context: the build
stage copies the whole repository, and a layer keeps what it copied, so an
auth key there would travel with any image built `--target build` and
pushed or saved.
