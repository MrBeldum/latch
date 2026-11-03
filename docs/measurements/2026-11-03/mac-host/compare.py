#!/usr/bin/env python3
"""Repeat the independent review's paced NV12/FFmpeg comparison.

Run from the repository root. Builds git archives in --work-dir, leaving the
checkout alone. Requires cargo and FFmpeg; uses no screen-recording permission.
"""
import argparse
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile


def build(repo, work, label, revision, synchronous):
    source = work / label
    source.mkdir(exist_ok=True)
    archive = subprocess.check_output(["git", "archive", revision], cwd=repo)
    with tarfile.open(fileobj=io.BytesIO(archive)) as files:
        files.extractall(source)  # Trusted, locally generated git archive.
    tests = source / "crates/engine/src/mac/encoder/tests.rs"
    quality = repo / "crates/engine/src/mac/encoder/quality.rs"
    harness = quality.read_text()
    if synchronous:
        start = harness.index("        while enc.in_flight() >= 2")
        end = harness.index("\n    let wall =", start)
        harness = harness[:start] + """        let now = Instant::now();
        pacer.taken(now);
        let frame = enc.encode(pool[f % 12].0, now, f % fps as usize == 0).unwrap();
        outputs.push((f, frame, now.elapsed().as_secs_f64() * 1000.));
    }
""" + harness[end:]
    # The original revisions do not yet include the quality module.
    if "mod quality;" not in tests.read_text():
        with tests.open("a") as file:
            file.write('\n#[path = "quality.rs"]\nmod quality;\n')
    (tests.parent / "quality.rs").write_text(harness)
    env = dict(os.environ, CARGO_TARGET_DIR=str(work / f"target-{label}"), TMPDIR="/tmp")
    result = subprocess.run(
        ["cargo", "test", "--locked", "--release", "-p", "latch-engine", "--lib",
         "quality_benchmark", "--no-run", "--message-format=json"],
        cwd=source, env=env, capture_output=True, text=True, check=True,
    )
    (work / f"build-{label}.log").write_text(result.stdout + result.stderr)
    executable = next(
        json.loads(line)["executable"] for line in result.stdout.splitlines()
        if line.startswith("{") and json.loads(line).get("executable")
    )
    binary = work / f"{label}-test"
    shutil.copy2(executable, binary)
    return binary


def measure(binary, work, label, case, repeat):
    output = work / "output"
    output.mkdir(exist_ok=True)
    env = dict(os.environ, LATCH_VT_QUALITY=case, LATCH_VT_OUTPUT=str(output))
    result = subprocess.run(
        [str(binary), "quality_benchmark", "--ignored", "--nocapture"],
        env=env, capture_output=True, text=True, check=True,
    )
    (work / "last-test.log").write_text(result.stdout + result.stderr)
    line = next(line for line in result.stdout.splitlines() if line.startswith("QUALITY "))
    data = {key: float(value) for key, value in re.findall(r"(\w+)=([\d.]+)", line)}
    _, width, height, _, _, _ = case.split(":")
    result = subprocess.run([
        "ffmpeg", "-v", "info", "-threads", "1", "-i", str(output / "stream.bin"),
        "-f", "rawvideo", "-pixel_format", "nv12", "-video_size", f"{width}x{height}",
        "-i", str(output / "source.nv12"), "-filter_complex_threads", "1",
        "-filter_complex", "[0:v]format=nv12,settb=AVTB,setpts=N/(25*TB)[a];"
        "[1:v]settb=AVTB,setpts=N/(25*TB)[b];[a][b]psnr",
        "-threads", "1", "-f", "null", "-",
    ], capture_output=True, text=True, check=True)
    (work / "last-decode.log").write_text(result.stderr)
    data.update(build=label, case=case, repeat=repeat,
                psnr=float(re.findall(r"PSNR .*average:([\d.]+)", result.stderr)[-1]))
    for file in output.iterdir():
        file.unlink()
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, default=Path("/tmp/latch-mac-compare"))
    parser.add_argument("--after", default="HEAD")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--case", action="append", help="e.g. hevc:1920:1080:60:20000:sweep")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[4]
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    revisions = {"main": "9fe0ae9", "ac7": "ac7b2f1", "after": args.after}
    binaries = {label: build(repo, work, label, revision, label == "main")
                for label, revision in revisions.items()}
    (work / "revisions.json").write_text(json.dumps({
        label: subprocess.check_output(["git", "rev-parse", revision], cwd=repo, text=True).strip()
        for label, revision in revisions.items()
    }, indent=2))
    cases = args.case or [
        f"{codec}:{width}:{height}:{fps}:{kbps}:{motion}"
        for codec in ["hevc", "high"]
        for width, height, fps, kbps in [
            (1920, 1080, 60, 20000), (1920, 1080, 60, 50000),
            (1920, 1080, 120, 50000), (1920, 1080, 240, 80000),
            (3024, 1964, 120, 100000),
        ] for motion in ["sweep", "noise", "still"]
    ]
    with (work / "runs.jsonl").open("w") as file:
        for case in cases:
            for repeat in range(args.repeats):
                labels = list(binaries)
                offset = repeat % len(labels)
                for label in labels[offset:] + labels[:offset]:
                    result = measure(binaries[label], work, label, case, repeat)
                    line = json.dumps(result)
                    print(line, flush=True)
                    file.write(line + "\n")
                    file.flush()


if __name__ == "__main__":
    main()
