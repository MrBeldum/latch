# PR #53 review fixes

Compared with reviewed head `0b4f83b`. MacBook Pro M3 Pro, RX 6600 on
Windows 11; test hosts only, TCP/UDP 47893. Installed hosts were untouched.

| Check | Reviewed head | Fixed branch |
| --- | --- | --- |
| Old picture held in sender queue across complete capture reopen | accepted as a different picture in the replacement encoder; regression fails (`refused == 0`) | refused (`refused == 1`); IDR decodes identically in the existing and a fresh decoder |
| Recovery datagram kinds | 13/14; 13 collides with microphone | 14/15; 13 reserved for microphone; registry uniqueness and serializer assertions pass |
| AMF recovery across LTR slot reuse | hardware metadata smoke test only | 128 uninterrupted / 112 delivered pictures; every delivered YUV byte matches, four recoveries, zero FFmpeg errors |

The reopen regression reuses the review's deterministic interleaving, now
with two real `SoftwarePipeline`s, `encode_video`, its bounded sender queue,
and `paced_video`. It also fails when installed on `0b4f83b` (only the
recovery-map tuple and `take` signature need adapting). Local logs and raw
bitstreams are retained in `/tmp/latch-rfi-fix/`.

The session stamps `EncodedVideo` with a generation before enqueueing and
increments it after a complete capture/pipeline reconstruction. Both the
sender's reference map and pending recovery mark carry that generation;
recovery refuses an old generation regardless of a sender that inserts an
old frame after the map is cleared. Software encoder replacements inside
one pipeline retain their existing monotonic picture offsets. AMF already
allocates process-unique picture numbers, and gets the generation guard as
well. Other encoders currently refuse reference recovery.

Registry checked at `main` `5e2583e`, `gfn/audio` `85381d1` (Mic=13), and
`gfn/hdr` `9023917` (kinds 1–12 only): neither 14 nor 15 is allocated there.
`#54` (`087c3f3`) was merged directly while main still pointed at `5e2583e`;
its `KeyframeAnswers` tracks actual IDR transmission and permits repeats
after a round trip. The viewer retains reference recovery before fallback,
and calls #54's `KeyframeRequests::lost` for further frame loss.

The post-merge recovery simulator also uses #54's actual `KeyframeAnswers`
and `KeyframeRequests::lost` on both paths. Each run sends 1,800 pictures:

| Impairment / metric | IDR fallback | Reference recovery |
| --- | ---: | ---: |
| 12% random loss: IDRs/min | 190 | 12 |
| 12% random loss: resume p95, ms | 281 | 114 |
| 12% random loss: pictures delivered and checked | 1,417 | 1,543 |
| Burst loss: IDRs/min | 66 | 4 |
| Burst loss: resume p95, ms | 149 | 149 |

Raw output: [simulator.log](simulator.log). The earlier measurements in the
parent directory describe the reviewed implementation before #54.

## Repeat the checks

```sh
cargo test -p latch-engine --lib reopen_refuses_an_old_picture_held_in_the_send_queue
cargo test -p latch-proto the_full_kind_registry
cargo test -p latch-proto --test keyframe_recovery
```

The review's `impairment_edges.rs` and `h264_wrap.rs` were rerun unchanged
apart from output paths as temporary integration tests: 64 expanded seeds,
**15,186 pictures checked exactly**, wire-frame wrap, lost IDRs, alternating
bitrates, missing acknowledgments, plus 65,560 H.264 pictures crossing
frame_num wrap. FFmpeg decoded seeds 1–8 and the wrap stream exactly against
the encoder reconstructions, without decoder errors. Scratch tests were
removed after execution; their sources remain in `/tmp/latch-review/scratch`.

On Windows, run the ignored test in the signed-in desktop, through a hidden
interactive scheduled task (session 0 cannot enumerate the GPU's output):

```powershell
$env:LATCH_AMF_RECOVERY_DUMP = 'C:\Users\Daniel\dev\latch-gfn-rfi\recovery-dump'
cargo test --locked -j 2 -p latch-engine --lib hardware_amf_recovery_across_slot_reuse -- --ignored --nocapture
```

Copy that dump directory to the Mac, then:

```sh
python3 scripts/check-amf-recovery.py /tmp/latch-rfi-fix/recovery-dump
```

The test encodes a moving 640×360 texture for 128 pictures, drops pictures
19–22, 43–46, 67–70 and 91–94 from the loss stream, and forces recovery at
23, 47, 71 and 95. Four LTR slots cycle before and between losses; requesting
the original overwritten picture is explicitly refused. Both decoders see
the same encoded recovery frames, one after uninterrupted delivery and one
after loss. The comparator invokes FFmpeg with `-err_detect explode` and
`-fps_mode passthrough`, then compares all 112 delivered YUV420 pictures
against the corresponding uninterrupted pictures (including the tail after
each recovery), byte for byte. This checks decode correctness for this
moving synthetic stream, not every possible HEVC reference pattern.

## Old/new interoperability on Mac loopback

Built independent `serve` and `view` examples from `main` `5e2583e` and
`v5.1.0`, then ran each against the branch in both directions. Commands:

```sh
LATCH_TEST_PATTERN=sweep serve --port 47893
view 127.0.0.1 --port 47893 --width 640 --height 360 --fps 60 --kbps 8000 --seconds 8
```

For the new viewer against each old host, also set
`LATCH_TEST_VIDEO_LOSS=3:3` to exercise legacy IDR fallback. All four sessions
connected, decoded hundreds of pictures, and shut down cleanly; every
sample's `video_problem` was null. New viewers reported zero recovery frames
against old hosts. JSON next to this file records the sessions. This is
compatibility evidence, not a performance comparison: loss is enabled only
for the new viewer.

## Limits

AMF was tested on one AMD RX 6600, HEVC 8-bit SDR. Non-AMD hardware was not
available. VideoToolbox/MF/NVENC/VA-API reference control remains unimplemented
and continues to fall back to IDRs; no claim of hardware recovery support
is added for those paths. No installed service, display mode, or driver was
changed. The hardware scheduled task was deleted afterward.

## Local build validation

After integrating #54: Mac workspace `--all-targets` tests (682 passed),
workspace doc tests, native Windows engine/proto/codec/stream unit tests
(188 passed), CI clippy with `-D warnings` on Mac and
`x86_64-pc-windows-msvc` / `aarch64-unknown-linux-gnu`, Windows-target release
workspace check, formatting, and native dependency audit all passed. The
initial parallel Mac run hit the previously reported loopback hang; a stack
sample is retained locally. The full serial rerun passed, as did the full
serial run after #54. The Linux target used `/tmp/latch-bench/armenv.sh`.
