# Roadmap

## v1.0 – v1.2 (retired)

A custom H.264 protocol, encoder, decoder, tickets, UPnP, STUN, relay,
rendezvous and PIN dialogs. Replaced in 2.0; the git history has it.

## v2.0 (retired)

Glue around Sunshine, Moonlight and Tailscale: the Mac app installed
Moonlight and launched it; Latch itself did wake, pairing and power.

## v3.0

- [x] The GameStream client compiled into the Mac app (moonlight-common-c,
      VideoToolbox, Opus); no Moonlight to install
- [x] Pairing, app list, launch and resume against Sunshine's HTTPS API with
      a pinned certificate
- [x] A stream window with a toolbar: capture, ⌘ mapping, Keys menu, stats,
      full screen, power, disconnect
- [x] Sunshine's installer shipped in the Windows zip; setup installs it
      offline
- [x] Wake: Fast Startup off, driver keywords for sleep/standby/shutdown,
      a wake-packet listener on the host and **Test wake** on the Mac
- [x] Both Sunshine PIN APIs (with and without pairing ids)
- [x] GPL-3.0-or-later

## v3.1 – v3.3

- [x] Clipboard sync both ways while streaming (text, up to 32 KB)
- [x] Automatic updates: the Mac fetches releases from GitHub and pushes
      the new host to every PC over the control API
- [x] Direct or relayed path shown per PC, with the reason for a relay
- [x] A peer relay you run yourself (`deploy/relay/`)
- [x] Match screen: the stream is the viewing machine's own resolution

## v4.0

- [x] One app on every OS: each machine lists the others and can share
      its own desktop (Windows, macOS, Linux, a VPS)
- [x] `deploy/node/` Docker kit and `deploy/native/` units for a VPS desktop
- [x] The chosen bitrate is the one the PC's encoder targets

## v5.0

- [x] Pure-Rust in-process streaming engine (`latch-engine`, `latch-proto`,
      `latch-codec`, `latch-stream`); retired Sunshine, Moonlight, OpenH264,
      and Opus
- [x] Latch stream protocol version 1 over UDP with Reed-Solomon FEC and
      lossless `lpcm-rice` audio
- [x] WireGuard encryption and authentication via Tailscale identity; no
      separate PIN pairing layer
- [x] Desktop follows viewer sizing per session via prep commands and
      in-engine display sizing
- [x] Idempotent retirement of Latch 4 engine services, tasks, firewall
      rules, and directories
- [x] Zero C/C++/assembler build dependencies in the build graph

## Later

- [ ] AV1 decode on M3 and newer (moonlight-common-c negotiates it; the
      VideoToolbox path is HEVC and H.264 today)
- [ ] Gamepads on the Mac (GameController framework to Sunshine's virtual pad)
- [ ] Signed Windows and macOS binaries (Authenticode, Developer ID and
      notarization)
- [ ] Menu-bar presence on the Mac, tray icon on Windows
- [ ] Per-PC stream settings
