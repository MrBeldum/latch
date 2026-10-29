# Latch node: a virtual desktop and the Latch service, optionally joined
# to Tailscale as its own machine.
#
# Build from the repository root:
#   docker build -f deploy/node/Dockerfile -t latch-node .
#

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config \
        libasound2-dev \
        libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev \
        libxrandr-dev libxi-dev libx11-xcb-dev libxcb-render0-dev \
        libxcb-shape0-dev libxcb-xfixes0-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# --locked: build exactly the dependencies Cargo.lock pins and CI tested.
RUN cargo build --locked --release -p latch-host

FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl python3 \
        xvfb xfce4 xfce4-terminal dbus-x11 \
        pulseaudio libasound2t64 \
        libxkbcommon0 libxkbcommon-x11-0 libvulkan1 libgl1 libegl1 \
        libxrandr2 libxi6 libxcursor1 libx11-xcb1 \
        libxcb-render0 libxcb-shape0 libxcb-xfixes0 \
        iptables iproute2 \
    && rm -rf /var/lib/apt/lists/*

# Optional Tailscale client so this container is its own node. It runs with
# kernel networking (see docker-compose.yml), so Latch Host sees each peer's
# own tailnet address.
COPY --from=docker.io/tailscale/tailscale:stable /usr/local/bin/tailscale /usr/local/bin/tailscaled /usr/local/bin/

COPY --from=build /src/target/release/latch-host /usr/local/bin/latch-host
COPY deploy/node/entrypoint.sh /entrypoint.sh
COPY deploy/node/bootstrap.py /opt/latch/bootstrap.py
RUN chmod +x /entrypoint.sh \
    && mkdir -p /root/.config /var/lib/tailscale

ENV DISPLAY=:1 \
    HOME=/root

EXPOSE 47850/tcp 47850/udp

VOLUME ["/root/.config", "/root/.local", "/var/lib/tailscale"]
ENTRYPOINT ["/entrypoint.sh"]
