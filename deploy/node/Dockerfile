# Latch node: a virtual desktop plus Sunshine plus the Latch control
# service, optionally joined to Tailscale as its own machine.
#
# Build from the repository root:
#   docker build -f deploy/node/Dockerfile -t latch-node .
#
# Ubuntu 24.04 matches the pinned Sunshine .deb.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake clang nasm pkg-config \
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
ARG SUNSHINE_TAG=v2026.906.222525
ARG TARGETARCH
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl python3 \
        xvfb xfce4 xfce4-terminal dbus-x11 \
        pulseaudio libasound2t64 \
        libcap2-bin \
        iptables iproute2 \
    && rm -rf /var/lib/apt/lists/*

# Sunshine Ubuntu 24.04 package for this architecture.
RUN set -eux; \
    arch="${TARGETARCH:-amd64}"; \
    if [ "$arch" = "arm64" ]; then deb_arch=arm64; sha=17a20e57e9fd547a01adb818c172ec8481f2ac95cd1421ac976a96e056c5a073; \
    else deb_arch=amd64; sha=044f68a144f2beb8c38094828c789b5c1ae376ff07a6b095c901d15abf742a32; \
    fi; \
    file="sunshine_${SUNSHINE_TAG#v}-1+ubuntu24.04_${deb_arch}.deb"; \
    url="https://github.com/LizardByte/Sunshine/releases/download/${SUNSHINE_TAG}/${file}"; \
    curl -fsSL -o /tmp/sunshine.deb "$url"; \
    echo "${sha}  /tmp/sunshine.deb" | sha256sum -c -; \
    apt-get update && apt-get install -y --no-install-recommends /tmp/sunshine.deb \
    && rm -rf /tmp/sunshine.deb /var/lib/apt/lists/*

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
    HOME=/root \
    LATCH_USER=latch \
    LATCH_PASS=

VOLUME ["/root/.config", "/root/.local", "/var/lib/tailscale"]
ENTRYPOINT ["/entrypoint.sh"]
