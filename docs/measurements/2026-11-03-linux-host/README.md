# Linux host, 2026-11-03

The Linux host's software pipeline with X Damage driving capture,
conversion and encode (branch `gfn/linux-host`), against main (9fe0ae9).

- **Host:** `linux` (Oracle A1, 4× Neoverse N1, no GPU, shared with other
  jobs; load average 1.4–4.6 during these runs), `serve` example cross-built
  with `cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39`.
- **Viewer:** the `view` example on a MacBook Pro (M3 Pro), over Tailscale
  (12–20 ms round trip). The Mac was itself very busy (other builds, load
  average above 200), which shows in the frame-rate minima of both builds.
- **Runs:** [`matrix.sh`](matrix.sh) `3 20`: three rounds of 20 s per
  build and configuration, the order of the two builds alternating each
  round; [`run.sh`](run.sh) runs one. 30 key presses per test-pattern run
  (key to picture is measured on the viewer's clock). Host CPU is the
  `serve` process's user + system time over the run, in % of one core.
  Every run is in [`runs.txt`](runs.txt).

Medians of the three runs (range in brackets):

| 60 fps | | main | this branch |
| --- | --- | --- | --- |
| 1080p sweep, 20 Mb/s | fps | 59.8 | 59.9 |
| | host encode | 7.05 ms | **1.34 ms** |
| | key to picture p50 / p95 | 29.9 / 45.4 ms | 29.2 / **34.2 ms** |
| | host CPU | 78% | **29%** |
| 1080p noise, 30 Mb/s | fps | 59.9 | 59.8 |
| | host encode | 10.7 ms | **6.0 ms** |
| | key to picture p50 / p95 | 44.9 / 78.5 ms | **32.0 / 43.5 ms** |
| | host CPU | 96% | **59%** |
| 1440p sweep, 20 Mb/s | fps | 59.7 | 59.9 |
| | host encode | 12.3 ms | **1.8 ms** |
| | key to picture p50 / p95 | 43.6 / 63.6 ms | **29.8 / 36.7 ms** |
| | host CPU | 137% | **45%** |
| | bitrate | 3.7 Mb/s | **0.5 Mb/s** |
| 1440p noise, 30 Mb/s | fps | 51.5 | **59.8** |
| | host encode | 15.1 ms | **6.5 ms** |
| | key to picture p50 / p95 | 51.3 / 64.8 ms | **41.6 / 49.5 ms** |
| | host CPU | 133% | **73%** |
| 1080p X11, a 640×360 60 fps video window | fps | 59.6 | 59.9 |
| | host encode | 7.9 ms | **2.0 ms** |
| | host CPU | 74% | **23%** |
| 720p real XFCE desktop, idle | fps | 11.1 | 11.2 |
| | host encode | 0.18 ms | 0.00 ms |
| | host CPU | 2% | 2% |

- The 1440p sweep at 3.7 Mb/s on main was the scroll search coding the
  moving band as a false scroll; the stricter search threshold sends it
  as the 0.5 Mb/s it is.
- One 1440p noise run on the branch has a 529 ms key-to-picture p95 (one
  press out of 30, after a stall that both builds show at times on this
  path); its p50 and the other two runs are in line.
- The noise bitrates vary between runs of both builds (8–27 Mb/s for
  30 Mb/s asked) with no loss reported: that is the congestion
  controller on this path, not the encoder, and is not this branch's
  scope.
- "X11" is an Xvfb :9 of the host's own with `ffplay`'s `testsrc2` in a
  window ([`x9.sh`](x9.sh)); the real desktop on `:1` (the deployed
  host's XFCE, captured with its `XAUTHORITY`) was left untouched, so
  it was idle: both builds send about 11 fps of nothing changing at 2%
  of a core.

## Output unchanged

`cargo run --release -p latch-codec --example encode_digest` (the
example copied into a main checkout for the "before" build) prints the
same 24 lines on both builds: bitstream and reconstruction digests for
six kinds of content, two sizes and two bitrates
([`encode-digest.txt`](encode-digest.txt)).
`damage_hints_change_nothing_but_the_work` checks the damage path against
a full comparison.

## Hardware encoders

NVENC and VA-API are not measured: no GPU here. On `linux` libva loads,
but `/dev/dri/renderD128` (virtio) is `root:render` and the host's user
is not in `render`, so every probe stops at "Permission denied" and
`latch-codec` is used, which is the fallback working, not the encoders.
