#!/usr/bin/env python3
"""Independently decode hardware_amf_recovery_across_slot_reuse's dumps.

Usage: python3 scripts/check-amf-recovery.py DUMP_DIRECTORY
Requires FFmpeg on PATH; it is a test tool, never a Latch dependency.
"""
import pathlib
import subprocess
import sys
import tempfile


def check(directory):
    directory = pathlib.Path(directory)
    kept = [int(n) for n in (directory / "kept.txt").read_text().splitlines()]
    assert len(kept) == 112 and kept == sorted(set(kept))
    frame_bytes = 640 * 360 * 3 // 2
    with tempfile.TemporaryDirectory() as temporary:
        outputs = []
        for name in ("full", "loss"):
            output = pathlib.Path(temporary) / (name + ".yuv")
            result = subprocess.run(
                ["ffmpeg", "-v", "error", "-err_detect", "explode", "-i",
                 str(directory / (name + ".hevc")), "-fps_mode", "passthrough",
                 "-pix_fmt", "yuv420p", "-f", "rawvideo", str(output)],
                capture_output=True, check=True,
            )
            assert not result.stderr, result.stderr.decode(errors="replace")
            outputs.append(output)
        assert outputs[0].stat().st_size == 128 * frame_bytes
        assert outputs[1].stat().st_size == len(kept) * frame_bytes
        with outputs[0].open("rb") as full, outputs[1].open("rb") as loss:
            for number in kept:
                full.seek(number * frame_bytes)
                assert full.read(frame_bytes) == loss.read(frame_bytes), f"picture {number} differs"
    print("128 uninterrupted / 112 delivered pictures: all YUV bytes equal; "
          "four recoveries across LTR slot reuse, no decoder errors")


if __name__ == "__main__":
    check(sys.argv[1])
