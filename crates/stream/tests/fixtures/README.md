`gray-bars.h264` is the existing 64×64 white-top/black-bottom reference.
The viewer hardware-probe fixtures were transcoded from it with FFmpeg 9.0.2
(Homebrew), which is a test tool and is never linked or shipped by Latch:

```
ffmpeg -i gray-bars.h264 -frames:v 1 -c:v libx265 -x265-params pools=1:log-level=error -f hevc gray-bars.hevc
ffmpeg -i gray-bars.h264 -frames:v 1 -pix_fmt yuv420p10le -c:v libx265 -x265-params pools=1:bframes=0:log-level=error -f hevc gray-bars-main10.hevc
ffmpeg -i gray-bars.h264 -frames:v 1 -c:v libsvtav1 -preset 12 -svtav1-params lp=1 -f obu gray-bars.av1
```

AV1 is a low-overhead OBU temporal unit, including its sequence header.
The HEVC and H.264 files use Annex B. The tests check decoded visible
pixels as well as native storage and hardware selection.

`decode-probe.h264`, `.hevc` and `.av1` use the same pattern scaled to
256×256 (`-vf scale=256:256`), avoiding driver minimum surface sizes for
tiny pictures. H.264 uses `-tune zerolatency -bf 0`, HEVC `bframes=0`, and
AV1 `-svtav1-params lp=1:pred-struct=1`. These are capability probes, not
throughput benchmarks or evidence of maximum resolution/rate support.
