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
- [x] `latch-host --uninstall` on every OS; Windows setup elevates a
      checked copy in Program Files and records the power settings it
      changes so uninstall can put them back

## Later

- [ ] AV1 on M3 and newer (the engine negotiates HEVC and H.264 today)
- [ ] Gamepads (Latch 5 forwards none yet)
- [ ] Signed Windows and macOS binaries; see below
- [ ] Menu-bar presence on the Mac, tray icon on Windows
- [ ] Per-PC stream settings

## Signing and notarization: what the owner has to provide

Latch's releases are ad-hoc signed on the Mac and unsigned on Windows.
Nothing in the repository can change that: it needs certificates that
belong to a person or company, and their private keys. The code is ready
for them; this is the list.

### macOS: Developer ID and notarization

Needed:

- An Apple Developer Program membership (individual or organisation),
  and in it a **Developer ID Application** certificate. Its private key
  lives in the signing Mac's keychain; for CI, export certificate and key
  as a password-protected `.p12`.
- Credentials for the notary service: an App Store Connect API key (the
  `.p8` file, its key ID and the issuer ID), or an Apple ID with an
  app-specific password and the team ID.
- For CI, as repository secrets: the `.p12` (base64) and its password, the
  identity name (`Developer ID Application: Name (TEAMID)`), and the notary
  credentials.

Then:

- `scripts/bundle-macos.sh` already signs with `CODESIGN_IDENTITY`, with
  the hardened runtime, a secure timestamp and the Apple Events
  entitlement a shared Mac needs to restart and shut down. `docs/MACOS.md`
  (Signing) has the local commands.
- `.github/workflows/release.yml`'s macOS job would import the `.p12` into
  a temporary keychain, set `CODESIGN_IDENTITY`, run `xcrun notarytool
  submit --wait` on a zip of `Latch.app`, `xcrun stapler staple` the app,
  and only then make `latch-macos-arm64.tar.gz` (and the 4.0 updaters'
  copy).
- The `xattr -dr com.apple.quarantine` step then leaves `README.md`,
  `docs/MACOS.md` and `scripts/install-macos-release.sh`.
- macOS keys Screen Recording and Accessibility grants to the app's code
  signature. Ad-hoc, that is the hash of each build, so every update can
  make macOS forget them and the owner has to allow Latch again; under a
  Developer ID they survive updates. The updater
  (`crates/client/src/update.rs`) could then also require the team ID
  in the new app's signature, not only GitHub's digest.

### Windows: Authenticode

Needed:

- A code-signing certificate from a certificate authority (OV or EV), or
  an Azure Trusted Signing account. Since 2023 the private key has to sit
  in hardware, so signing in CI goes through the CA's cloud signing
  service (Azure Trusted Signing, DigiCert KeyLocker, SSL.com eSigner and
  the like) and its credentials as repository secrets.
- An RFC 3161 timestamp server URL (every CA has one), so signatures stay
  valid after the certificate expires.

Then:

- The release workflow's Windows job signs `latch-host.exe` (`signtool sign
  /fd sha256 /tr <timestamp url> /td sha256`, or the service's action)
  before zipping it.
- SmartScreen stops warning once the certificate has reputation, and the
  UAC prompt names a verified publisher.
- Setup's elevated step (`stage_script` in `crates/host/src/setup.rs`)
  today runs only a copy whose SHA-256 matches the file the window was
  started from. With a signature it should also require
  `Get-AuthenticodeSignature` to be `Valid` for the expected publisher
  before running the copy, and could elevate `latch-host.exe` itself
  (whose prompt would show Latch) instead of Windows PowerShell. The
  update a Mac pushes to `/v1/update` can be held to the same signer.
