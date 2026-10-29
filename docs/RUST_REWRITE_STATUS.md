# Rust rewrite execution record — 2026-10-24

**The engine rewrite is incomplete.** This change establishes the brief,
component measurement tool and available baseline. Sunshine and
`moonlight-common-c` still implement the shipping stream. No native Rust
host or replacement transport has been integrated, and no dependency-removal
or performance-improvement claim is made.

The [rewritten prompt](RUST_REWRITE_PROMPT.md) answers the four expert-method
questions and defines the full task. The user delegated the native boundary
decision: Latch-owned runtime code should be Rust, with OS/GPU APIs allowed.
The user subsequently confirmed that the offline Windows host cannot be
brought online and no alternative Windows test host is available. That blocks
the mandatory Windows GPU baseline before changing its engine.

## Inspected facts

Base revision: `8aa2165`; local branch: `rewrite/rust-streaming`.

| Stage | Actual current implementation | Source of evidence |
| --- | --- | --- |
| Host capture, video encode, audio capture, remote input injection | External Sunshine | `crates/host/src/streamer.rs`, `setup.rs`, `unix_setup.rs`, deployment units |
| Session transport, packet reassembly, loss recovery | Vendored Moonlight C, ENet and nanors | `crates/stream/build.rs`, `csrc/shim.c`, `third_party/moonlight-common-c/VERSION` |
| Mac video decode | Rust bindings to VideoToolbox, with an output copy into CPU frame buffers | `crates/stream/src/video/videotoolbox.rs`, `video.rs` |
| Windows/Linux video decode | OpenH264 software decoder | `crates/stream/src/video/openh264.rs`, `Cargo.toml` |
| Audio decode and playback | Rust orchestration, compiled C Opus, OS output through cpal | `crates/stream/src/audio.rs`, `Cargo.toml` |
| Presentation | Rust/wgpu NV12 upload and shader | `crates/client/src/video.rs` |

The current Mac path copies decoded planes into `Vec<u8>` and subsequently
uploads them to wgpu textures. It is not a zero-copy path. Removing Moonlight
alone would not remove OpenH264, Opus or native code in the TLS provider.

The retired v1 source (`6aae415`) used an external FFmpeg encoder and
software video decode. Restoring it does not satisfy either dependency
removal or the hardware-efficiency requirement. Its existence is evidence
to inspect, not evidence that a new implementation is verified.

Public native API documentation supports the chosen implementation boundary:
[Apple VideoToolbox](https://developer.apple.com/documentation/videotoolbox),
[Microsoft Desktop Duplication](https://learn.microsoft.com/en-us/windows/desktop/direct3ddxgi/desktop-dup-api),
and [NVIDIA NVENC](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvenc-application-note/index.html).
Those documents do not establish Latch's measured performance or reveal
GeForce Now's proprietary implementation.

## Work completed

- Six bounded Gemini 3.8 Flash High tasks through Antigravity covered source,
  native dependencies, measurements, implementation and independent review.
  Unsupported latency/RAM estimates, claims of existing zero-copy behavior,
  and proposals that removed the CPU-only VPS use case were rejected.
- Added `crates/stream/examples/decode_bench.rs`, using the existing decoder
  and the already-locked `serde_json` dependency for development only. The
  example validates the built-in pixel samples on every measured frame,
  retains ordered raw timings, reports nearest-rank percentiles, records the
  actual initialized backend, and fails on a backend change. Fixture input
  and iteration counts are bounded. Unsupported multi-frame/multi-slice
  fixtures fail explicitly.
- Updated the live test to recycle consumed frame buffers as the real
  renderer does. This changes the test consumer, not the production engine.
- Extended CI's test commands with `--all-targets` so the example's six
  deterministic checks run on the existing OS matrix. The remote CI matrix
  has not been run as part of this local task.
- Documented repeatable commands in [CONTRIBUTING.md](../CONTRIBUTING.md).
  The isolated copy of the existing pairing used for the live test was
  removed afterward; no pairing secrets are included in the evidence.

## Recorded observations

[The evidence directory](measurements/2026-10-24/) contains a manifest with
source hashes, three raw decoder runs, process resource outputs, CLI failure
checks and a sanitized live-run summary. These are baseline observations,
not a comparison against replacement code.

The local machine is an Apple M3 Pro running macOS 26.6.2 (25G83), Rust
1.99.0. The release microbenchmark repeatedly decodes the **66-byte, 64×64
single-IDR bars fixture**, with one initialization decode, 30 warmups and
300 measured decodes per run. Each run reported hardware H.264 decoding and
passed the pixel check.

| Trial | p50 decode + CPU output copy | p95 | p99 |
| --- | ---: | ---: | ---: |
| 1 | 0.257 ms | 0.294 ms | 0.310 ms |
| 2 | 0.254 ms | 0.292 ms | 0.400 ms |
| 3 | 0.258 ms | 0.291 ms | 0.317 ms |

The percentiles and means were independently recomputed from every ordered
raw sample using Python's standard library. JSON output, zero-warmup custom
fixture handling, invalid counts/options, missing/empty files, container
input and multi-frame rejection were exercised through the compiled CLI.
Backend initialization and backend-change checks were exercised with a
deterministic decoder in the example's tests.

This tiny fixture cannot establish 1080p/4K decode performance. Power mode
and background load were not controlled. Hardware use is reported by
VideoToolbox and was not independently corroborated by a GPU trace. Process
RSS excludes driver/service memory and is not the footprint of the full app.

The available Linux VPS reported software encoding, a 30 fps cap and no
viewer before testing. The current stream passed a 12-second debug test
(354 published frames) and a release test (353 published frames) at requested
1280×720, H.264, 30 fps and 10,000 kbps. The release test finished its stop
wait in approximately 92 ms. The host reported no viewer afterward.

The release test exercises the existing transport and real Mac decoder; it
has no renderer, known-pixel content oracle, audio integrity check or input
injection. Per-second `Stats` are window averages or last-frame observations,
not per-frame distributions or glass-to-glass latency. An initialized audio
output is not proof of correct sound or synchronization. Static-desktop
bitrate is not a gaming-quality benchmark. The local raw log is retained at
`target/rust-rewrite/measurements/live-release.log`; its numeric observations
are preserved in the committed sanitized summary.

The Mac has command-line developer tools, but `xcrun xctrace list templates`
failed with `unable to find utility "xctrace", not a developer tool or in PATH`.
Instruments/Metal trace validation therefore also remains unavailable in
this environment. No profiling installation or production deployment was
performed.

## Verification and unmet release gates

Local format checking, clippy with warnings denied, the release example
build, and `cargo test --locked --workspace --all-targets` passed. The final
suite reports **401 passed, 0 failed, 15 ignored**, including the example's
six deterministic checks. The explicitly run live tests are recorded
separately. Commands and limitations are preserved in
[`verification.json`](measurements/2026-10-24/verification.json).

The first unchanged workspace test run failed
`update::tests::failed_launch_restores_the_previous_executable`; the isolated
test and subsequent full run passed. This intermittent baseline failure is
recorded rather than treated as a rewrite regression or silently erased.

| Required gate | State |
| --- | --- |
| Rust/native boundary and behavior contract | Recorded in the rewritten brief |
| Reproducible component baseline | Available, with narrow limitations above |
| Windows GPU host/viewer baseline | Unavailable; user confirmed no test machine |
| Cross-platform native capture/encode/decode replacement | Not implemented |
| Rust replacement transport integrated into host and client | Not implemented |
| Sunshine/Moonlight removal from build, installers and packages | Not done |
| Other compiled foreign dependency removal | Not done |
| Matching-quality host/viewer CPU, GPU, memory and energy comparison | Not measured |
| Input-to-display and presentation timing | Not measured |
| Impaired-network recovery, audio drift and long-session stability | Not validated for a replacement |
| Clean install, upgrade compatibility and rollback of a replacement | Not validated |

Completion requires all of these gates, not just an available Windows
machine. Resume by collecting a matched Windows GPU baseline, filling the
host/viewer timing and quality evidence, and fixing the release budgets.
Then integrate and verify one replacement path at a time before removing
the corresponding working engine. The current change is reviewable
preparation for that work, not the completed rewrite.
