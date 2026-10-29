# Contributing

## Building

Rust stable, a C compiler (MSVC Build Tools on Windows, Xcode command line
tools on macOS) and CMake. The C compiler builds the vendored
moonlight-common-c; CMake builds libopus for the audio decoder.

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`LATCH_SKIP_C=1` skips the C build for a quick cross-target check, for
example `LATCH_SKIP_C=1 cargo check -p latch-client --target
aarch64-apple-darwin` from Windows after `rustup target add`.

`cargo test` and `cargo run` keep Latch's settings, pairing identity and
logs in `target/latch-data` (`.cargo/config.toml` sets
`LATCH_DATA_DIR`), so neither touches an installed Latch's files. To
point a development build at an installed one's data on purpose, set
`LATCH_DATA_DIR` yourself, for example to `%LOCALAPPDATA%\Latch` on
the development PC before `cargo run -p latch-host -- --background`.

## Where things live

- `crates/stream`: everything that talks to Sunshine. `nvhttp` is the
  pairing and launch API, `session` drives moonlight-common-c through the C
  shim in `csrc/`, `video` and `audio` decode. It knows nothing about
  windows or Tailscale. The vendored moonlight-common-c, what was trimmed
  from it and its two local patches are described in
  `third_party/moonlight-common-c/VERSION`.
- `crates/client`: the viewer UI (a library plus `latch-client`).
  `session.rs` is the path from a listed machine to a live stream;
  `stream.rs` is the toolbar and input while streaming; `video.rs` draws
  frames with wgpu.
- `crates/host`: the node (`latch-host`): control service, engine setup,
  and the unified window that both views other machines and shares this
  one. Windows administrator work stays in the generated setup script in
  `setup.rs`. macOS/Linux setup is `unix_setup.rs`.
- `crates/core`: the control API, a small HTTP client and server, the
  Tailscale CLI wrapper and wake packets.
- `crates/ui`: one palette, one typeface, one set of cards, rows, pills and
  buttons. New visual elements go there rather than inline.

The control API (`crates/core/src/api.rs`) is read by older clients and
hosts at times: new JSON fields get `#[serde(default)]` and existing ones
keep their meaning.

## Seeing the UI without a Mac or a PC

```bash
cargo test -p latch-client -p latch-host snapshots -- --ignored
```

renders every screen to `target/ui-snapshots/*.png`: each state of the
machine list, Settings, the Sharing page and the stream overlay at the
minimum window (640×420), a typical one (1280×800) and a large one
(1920×1200), at 1× and 2× (`client-lobby-1280x800@2x.png`); `*-full.png`
show whole scrolling pages. The window is built headless with default
settings, so nothing reads or writes your real configuration. The same
states run unignored, checking that nothing runs past the window and no
two controls overlap, at the minimum and the large size.

Every colour, size and distance comes from `crates/ui/src/theme.rs`, and
every control from `crates/ui/src/widgets.rs`; a screen that needs
something new adds it there, with accesskit info and a focus ring.

## Tests against a real Sunshine

With Sunshine running on the development PC and Latch Host's service
started (`cargo run -p latch-host -- --background`, pointed at the
installed data directory as above so it has the engine login):

```bash
cargo test -p latch-stream pair_real -- --ignored --nocapture     # five-phase pairing
cargo test -p latch-stream stream_real -- --ignored --nocapture   # ten seconds of video
LATCH_DEV_LOCAL=1 cargo test -p latch-client stream_snapshot -- --ignored --nocapture
cargo test -p latch-client wake_test_real -- --ignored            # wake packet round trip
```

`LATCH_DEV_LOCAL=1` also makes the client app list this PC at 127.0.0.1,
so `cargo run -p latch-client` on Windows streams the PC to itself for
trying the toolbar and input.

The Windows client decodes with OpenH264 in software; that path exists for
development only. The Mac path (VideoToolbox) is the one that ships.

On a Mac, the snapshot test can use an already saved PC and its existing
pairing. It verifies decoded video, renders the stream, and disconnects:

```bash
LATCH_TEST_PC='Gaming-PC' cargo test -p latch-client stream_snapshot -- --ignored --nocapture
cargo test -p latch-client decoded_frame_reaches -- --ignored
```

`stream_real` streams from another machine too: `LATCH_TEST_IP` is its
Tailscale address, `LATCH_TEST_DIR` a directory holding copies of the files in the
Mac's `identity/` directory and the PC's certificate as `server.der`, and
`LATCH_TEST_WIDTH`, `_HEIGHT`, `_FPS`, `_BITRATE_KBPS`, `_SECONDS` and
`LATCH_TEST_HEVC=1` shape the stream.

`LATCH_TEST_CODEC=h264` or `hevc` selects the codec. A black capture fails
the live test by default; `LATCH_TEST_EXPECT_BLACK=1` instead verifies the
persistent black-picture diagnosis when testing a PC without an active
display. The GPU test uses a synthetic NV12 image to check that the picture
is drawn with the correct orientation and range and retained between frames.

## Rust engine rewrite evidence

The [rewrite brief](docs/RUST_REWRITE_PROMPT.md) defines the Rust/native
boundary and release gates. The [execution record](docs/RUST_REWRITE_STATUS.md)
distinguishes completed preparation from the unfinished engine replacement.

The decoder component baseline needs no host, pairing or screen capture:

```bash
cargo test --locked -p latch-stream --example decode_bench
cargo build --locked --release -p latch-stream --example decode_bench
target/release/examples/decode_bench --warmup 30 --iterations 300 > decode.json
```

On Windows the executable ends in `.exe`. Add `--require-hardware` to fail
unless every decode uses the initialized hardware backend. Windows/Linux
currently use OpenH264, so that gate is expected to fail there. A backend
change during warmup or measurement also fails rather than mixing results.

The default is the 64×64 bars fixture: its visible pixel samples are checked
on every measured frame. `--fixture path/to/frame.h264` accepts a single
Annex B H.264 IDR slice with SPS and PPS (at most 32 MiB), not a container,
multi-frame video, multi-slice picture or packet capture. A custom fixture
gets plane validation and a null pixel-oracle result. Collect a representative
access unit separately for resolution-specific measurements.

JSON includes the fixture hash, actual decoder, configuration, ordered raw
decode timings and nearest-rank p50/p95/p99. Decode time includes the
decoder's CPU output copy; total loop time also includes validation. Output
plane byte counts are not GPU transfer measurements. Neither result measures
capture, encoding, network, audio, presentation or input-to-display latency.
Record revision/diff, hardware, OS/driver and power state beside each run.

CI runs `cargo test --locked --workspace --all-targets`, including the
example's deterministic checks. Live and hardware performance gates still
require the appropriate machines and explicit runs.
