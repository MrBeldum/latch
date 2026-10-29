# Latch: evidence-driven Rust rewrite

## The four questions before rewriting

1. **What would an expert do that the original request omits?** Establish
   the shipped behavior, the ownership boundary, and a measured latency,
   quality and resource baseline before replacing the engine. Locate time
   spent waiting, copying, allocating and recovering from loss. Preserve
   working Rust and replace foreign implementation where evidence supports
   the change. Language choice alone does not establish efficiency.
2. **What is the raw material?** The exact checked-out source and lockfile;
   the vendored protocol implementation and its patches; native API headers
   and vendor documentation; real encoded access units and packet captures;
   timestamped frame/audio events; profiler traces; hardware, driver and
   network configurations. These take precedence over architecture
   summaries, marketing comparisons and an agent's estimates.
3. **Which methods and tools?** Trace capture → encode → transport →
   recover → decode → present, and input/audio/lifecycle separately. Use
   `cargo metadata/tree`, `cargo fmt/clippy/test`, release builds, known-pixel
   decode checks, differential protocol tests, malformed-input tests,
   before/after profiling, and repeated matched trials. Use VideoToolbox,
   ScreenCaptureKit, DXGI/Direct3D and vendor GPU APIs where supported.
   Profile with Instruments/Metal System Trace, WPR/ETW/PresentMon or
   `perf`, as appropriate. Exercise loss, burst loss, delay, jitter and
   reordering with a documented network impairment configuration.
4. **What establishes that the result is right?** Reproducible artifacts:
   decoded pixels compared with known fixtures; independently observed
   input-to-display timing; host and viewer resource traces; loss recovery
   and long-session results; preserved feature checks; clean installations;
   and an audit of the actual build graph and packaged binaries. Passing
   unit tests or an author's confidence cannot replace these observations.

## Rewritten prompt

Act as the senior engineer responsible for replacing Latch's Sunshine host
and Moonlight C client engine with the smallest maintainable Rust
implementation that preserves Latch's current product. Make the task deeper
through stronger evidence and verification, without adding features.

Use Rust for all Latch-owned runtime implementation. Rust crates and
documented OS/GPU/driver APIs called from Rust are allowed. Reuse existing
Rust code that works. Removing Sunshine and `moonlight-common-c` is a real
completion requirement, not a rename or a wrapper. Inventory other compiled
foreign dependencies, including Opus, OpenH264 and TLS providers; either
replace them within this boundary or record the remaining requirement as
unmet. OS APIs are an explicit exception, not a claim that the operating
system, firmware or driver is written in Rust. Packaging configuration and
data assets need not become Rust programs just to change their extension.

Before changing the streaming engine:

1. Inspect the exact source, lockfile, deployment artifacts, native API
   contracts and retired implementation in git. Trace the real video,
   audio, input, authentication and lifecycle paths. Write down the behavior
   contract, the Rust/native boundary and the failure modes the replacement
   must preserve. Distinguish inspected facts from hypotheses.
2. Establish a repeatable release-build baseline on the actual supported
   host/viewer combinations. Record source revision, dirty diff, toolchain,
   OS, GPU, drivers, resolution, frame rate, codec, bitrate, content,
   network conditions and power mode. Preserve raw outputs. A tiny synthetic
   decode fixture is a component check, not evidence of 1080p/4K streaming
   performance or encoding efficiency.
3. Measure capture/encode, assembly/queue, decode and presentation timing;
   p50/p95/p99 frame times; input-to-display latency; frame loss/recovery;
   audio delay/drift/underruns; CPU/GPU utilization, memory and energy;
   startup time and installed size. Keep host and viewer resource usage
   separate. Do not subtract unsynchronized clocks on different machines.
   Compare at equal content, visual quality and operating conditions, not
   merely equal encoder settings. Set numerical release budgets from the
   recorded baseline before choosing a replacement.

Implement the smallest architecture supported by those findings. Keep
capture and codec surfaces on the GPU where the platform supports sharing;
bound queues and memory; favor the newest usable video frame; handle
backpressure, cancellation, display changes and decoder resets explicitly.
Report the backend actually in use. Preserve a clearly identified software
path for the existing CPU-only VPS use case rather than silently pretending
it has a GPU. Avoid inventing encryption, NAT traversal or a new relay when
Latch's existing authenticated Tailscale path already supplies them.

Preserve the current Mac, Windows and Linux/VPS use cases; desktop audio;
relative and absolute mouse/keyboard behavior and host-key release;
clipboard; display selection and sizing; peer authorization; reconnect and
disconnect; wake and power behavior; and update rollback. Do not add AV1,
HDR support, gamepads, mobile clients or a new account system to expand this
rewrite. Document the upgrade and old-peer compatibility policy before
changing the wire protocol; do not silently strand existing installations.

Use independent Gemini 3.8 Flash tasks through Antigravity wherever they
reduce Codex usage without overlapping edits. Give each a bounded task and
verify its result. More agents are useful only while there is independent
work. Retain primary evidence, review their code, and reject unsupported
performance estimates. No claim of GeForce Now parity is permitted without
a reproducible comparison; use publicly documented low-latency techniques
as engineering inputs, not as knowledge of NVIDIA's proprietary service.

Verification must include known-bitstream/pixel checks, protocol and parser
boundary checks, unauthorized-peer rejection, loss/reordering recovery,
audio integrity, display/GPU resets, repeated connect/disconnect, resource
stability, and clean-install/upgrade checks on the supported systems. Keep
the existing working engine until a replacement passes the corresponding
gate. Delete its source, build hooks, downloads, service and release-package
dependencies after that evidence exists.

Finish only when the replacement is integrated into the shipped application,
all preserved workflows and measured budgets pass, and the source/build/
package audit confirms the removed engines are absent. Deliver code,
repeatable commands, raw before/after results, and a concise explanation of
the remaining native boundary. If a required machine, GPU, permission or
reference measurement is unavailable, complete the independent preparation,
name the precise unmet gate and the evidence needed to resume, and report
the rewrite as incomplete. Do not substitute a design document, scaffold,
synthetic benchmark or passing compilation for a completed migration.
