# Windows virtual-display setup failure

PC: Windows 11, RX 6600, MikeTheTech VDD 11.30.4.434. Base: `925123e`.
Mac: M3 Pro, `view` example over Tailscale. Windows logs use UTC+09:00;
the Mac uses Pacific time (UTC-08:00), with tracing timestamps in UTC.
The transcripts retain their original timestamps.

The wanted list appended `LARGE_MODES` after `stream_modes()`, which already
contains 7680×4320 and 7680×2160. The existing PC XML has 42 resolutions,
84 refresh entries. Two gaming sizes gain five rates each; the two new
8K sizes should gain four entries, for 98 total. Appending the 8K sizes
twice instead created four new resolution nodes and **102** refresh entries.
The existing-resolution lookup was not updated while adding new nodes.

The first change uses only the deduplicated shared list. The second makes
the step transactional and non-fatal on rejection, with a warning and a
"virtual display modes unchanged" completion summary. It checks native
restart exit codes, waits at least 15 seconds for monitor arrival, restores
XML bytes and VDDPATH on failure, restarts and checks the old monitor, and
removes unused relocation files/record. A restore failure marks setup failed
but still allows later setup steps to run.

## Driver diagnosis

Every trial used `C:\Program Files\Latch\Display`, the preserved XML,
`pnputil /restart-device ROOT\DISPLAY\0000`, and `Get-PnpDevice -Class Monitor`
polling. Every reproduction restored the original XML/registry and restarted
the driver in `finally`; no reboot, driver upgrade, or installed-host stop.

| Input | Outcome |
| --- | --- |
| Original generated shape, 102 entries (duplicate 8K nodes) | Monitor `Unknown` for 25.5 s |
| Corrected list, 98 entries | Monitor `OK` by 2.2 s |
| 99 entries including a duplicate 1080p60 refresh | Monitor `OK` by 2.7 s |
| 100 distinct entries | Monitor `OK` by 2.7 s |
| 101 distinct entries | Monitor `Unknown` throughout 8.6 s |
| 102 distinct entries | Monitor `Unknown` throughout 8.6 s |

The 99/100/101/102 probes distinguish duplicates from a mode-count limit.
This installed driver accepts at most 100 entries in these tests. The
102-entry failure is not fixed by waiting beyond five seconds. Relocation
works with an ordinary device restart: the driver's log names the relocated
VDDPATH, counts the requested target modes, and reports success at 100 or
monitor-arrival error `-1073741637` (`0xC00000BB`, STATUS_NOT_SUPPORTED) at
101/102. No individual gaming refresh rate needed removal.

Driver logging was temporarily enabled with `LOGS`/`DEBUGLOGS` and XML
`logging`/`debuglogging`. Its LocalService process cannot write logs directly
under Program Files, so the research-only `Display\Logs` subdirectory was
given LocalService Modify permission. It was removed before real setup.
No matching driver failure was found in the queried System/Application
logs. Excerpts here are cumulative: later probes include earlier trials.

See `reproduce.log`, `reproduce-boundary.log`, `reproduce-limit.log`, and
`driver-101-excerpt.log`. `modes-before.log` is an interactive-session
`EnumDisplaySettingsW` listing: 1080p/1440p offer only 60 and 120 Hz.

## Reproduction commands

Generate the exact step through the Rust test hook:

```sh
LATCH_DUMP_VDD_SCRIPT=vdd.ps1 cargo test -p latch-host --lib \
  the_step_lists_every_mode_keeps_the_file_and_restarts_only_on_change
```

`reproduce.ps1` is the controlled PC reproduction (run over SSH). The
`reproduce-boundary.ps1` and `reproduce-limit.ps1` variants isolate the limit.
They preserve the original XML and registry values, save candidate XML,
restart, observe, and restore. `modes.ps1` calls user32 display enumeration;
run it in Daniel's interactive session through a temporary scheduled task,
with a hidden wscript launcher. Session 0 cannot enumerate desktop modes.

The mock regression script runs on Windows without touching installed
hardware; CI also runs it:

```powershell
scripts/test-vdd-setup.ps1 -Script vdd.ps1
```

## Real setup and streaming

Built on the PC in `C:\Users\Daniel\dev\latch-fix-vdd`, `-j 2`:

```powershell
cargo build --locked --release -p latch-host -j 2
cargo build --locked --release -p latch-engine --example serve -j 2
```

`run-setup.ps1` stages the built exe under Program Files, verifies its hash,
and runs the real `--setup-elevated --program <installed-exe>` through a
`.cmd` file and `cmd /c`, redirecting both streams before PowerShell sees
nested CLIXML. The program argument keeps the firewall rules pointing at
the deployed host. The installed executable/process was never replaced or
stopped. Setup refreshes the input helper, so its main-build copy was backed
up and restored afterwards.

The script first adds four legitimate custom timings to the original XML:
88 input entries become 102 when setup adds the requested fourteen.
Rejection restores the valid 88-entry input, removes `Display` and the
stale uninstall record, reaches power/gamepad/wake steps, exits **0**, and
ends with "Latch setup finished (virtual display modes unchanged)".
Then it restores the original 84-entry input and runs setup successfully,
producing **98** entries. A third setup reports everything listed already.
Full transcripts: `rejected-setup.log`, `accepted-setup.log`,
`idempotent-setup.log`; all three exited zero.

`modes-after.log` lists **60/90/120/144/165/240/360 Hz** for both 1920×1080
and 2560×1440 through `EnumDisplaySettingsW`. Other stream sizes retain
60/120, preserving the 100-mode budget. `modes-during-stream.log` records
**CURRENT 1920x1080@240**, rather than just the XML advertising it.

```sh
target/release/examples/view 100.106.23.40 --port 47850 \
  --width 1920 --height 1080 --fps 240 --kbps 80000 --hevc --seconds 8
target/release/examples/view 100.106.23.40 --port 47897 \
  --width 1920 --height 1080 --fps 240 --kbps 80000 --hevc --seconds 12
```

`live-stream.ps1` runs the assigned test server with `--prepare-display`
and a temporary non-activating, topmost moving GDI window in the interactive
session. This is real Desktop Duplication capture, not `LATCH_TEST_PATTERN`.
The server logs AMF HEVC and the negotiated 1920×1080@240 request.

| Check | Before | Fixed setup applied |
| --- | --- | --- |
| Generated XML refresh entries from original PC input | 102, rejected | 98, accepted |
| Advertised 1080p/1440p rates | 60/120 | 60/90/120/144/165/240/360 |
| Rejected-list setup result | Fatal; all later steps skipped | Warning; later steps complete; exit 0 |
| Real stream display timing | Previously limited to 120 Hz | 1920×1080@240 observed |

| Moving real desktop → Mac, 1080p240 HEVC | Result |
| --- | --- |
| Installed host, port 47850, 8 s | 211.4 decoded fps overall; steady samples ~240 fps |
| Test server, port 47897, 12 s | 216.6 decoded fps overall; steady samples ~240 fps |
| Installed host frame interval p50 / p95 / p99 | 4.14 / 5.66 / 9.72 ms |
| Test server frame interval p50 / p95 / p99 | 4.15 / 5.39 / 9.76 ms |

Both runs had viewer decode-backlog/keyframe restarts and ~150–167 ms RTT;
this is not a claim of uninterrupted 240 fps. The earlier non-topmost/still
window trials (`stream-before.log`, `stream-after.log`) yielded 12.8/14.7 fps;
they are not comparable moving-desktop throughput measurements.

`restore.ps1` removes test tasks/firewall rules, stops only the test server
and animation, restores the original XML bytes, removes VDDPATH and the
unused protected copy/record, restarts the display, restores the main input
helper, and checks the unchanged installed PID. It never reboots. The new
modes were intentionally rolled back after verification; the fixed build
was not deployed over the installed host.

## Scope and limitations

- The 100-entry boundary was tested on this installed driver only. Custom
  lists exceeding it are preserved/restored, not silently truncated.
- High rates are seeded at 1080p/1440p; a full cross-product at every stream
  size would exceed the tested budget and is not introduced here.
- 1080p240 is tested by a real stream; 360 Hz and the other listed rates are
  enumerated, not individually streamed. No reboot or driver upgrade tested.
- Viewer decode stalls remain outside this setup fix. No codec, capture,
  protocol or input-injection behavior is changed.

## Validation

- Mac: `cargo fmt --all -- --check`,
  `cargo clippy --locked --workspace --all-targets -- -D warnings`,
  `cargo test --locked --workspace --all-targets`, and
  `bash scripts/audit-native.sh` passed.
- Windows release build passed with `-j 2`; no rustc crash retry was needed.
- Windows: `cargo clippy --locked --workspace --all-targets -j 2 -- -D warnings`
  passed. Generated setup, uninstall and VDD scripts parsed with Windows
  PowerShell 5.1. `scripts/test-vdd-setup.ps1` passed on the real PC using
  mocked PnP and isolated files/registry keys.
- `restore.log` confirms the original XML bytes and helper binary, absent
  VDDPATH/Display folder, and unchanged installed PID 37272. A subsequent
  installed-host HEVC stream decoded successfully (`stream-restored.log`).
- Committed text transcripts normalize line endings and trailing whitespace;
  measurements, messages and full setup steps are retained.
- Full Windows `cargo test --locked --workspace --all-targets -j 2` passed;
  the combined check command reported `clippy=0 test=0`. All build/test
  commands completed before handoff. Tested Rust code is `4b61ad6`; subsequent
  commits contain documentation/transcripts only.
