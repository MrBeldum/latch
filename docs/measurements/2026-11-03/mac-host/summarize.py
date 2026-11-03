#!/usr/bin/env python3
"""Write per-metric medians and a Markdown matrix from compare.py's JSONL."""
import csv
import json
from pathlib import Path
import statistics
import sys

source = Path(sys.argv[1])
output = Path(sys.argv[2]) if len(sys.argv) > 2 else source.parent
output.mkdir(parents=True, exist_ok=True)
runs = [json.loads(line) for line in source.read_text().splitlines()]
groups = {}
for run in runs:
    groups.setdefault((run["case"], run["build"]), []).append(run)
rows = []
for (case, build), samples in groups.items():
    codec, width, height, fps, target, pattern = case.split(":")
    row = dict(build=build, codec=codec, width=int(width), height=int(height),
               requested_fps=int(fps), target_kbps=int(target), pattern=pattern,
               repeats=len(samples))
    for key in ["frames", "fps", "p50ms", "p95ms", "wall_mbps", "mbps", "psnr",
                "max_idr", "IDR", "max", "mean", "sd"]:
        row[key] = statistics.median(sample[key] for sample in samples)
    rows.append(row)
with (output / "medians.csv").open("w") as file:
    writer = csv.DictWriter(file, fieldnames=list(rows[0]))
    writer.writeheader()
    writer.writerows(rows)
lines = [
    "| Codec / size / fps / Mbps / pattern | Build | Frames | Achieved fps | Encode p50 / p95 ms | Achieved Mbps | PSNR dB | IDR max / mean kB | Max frame kB |",
    "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
]
for row in rows:
    label = f"{row['codec']} / {row['width']}×{row['height']} / {row['requested_fps']} / {row['target_kbps']//1000} / {row['pattern']}"
    lines.append(
        f"| {label} | {row['build']} | {row['frames']:.0f}/{2*row['requested_fps']} | "
        f"{row['fps']:.1f} | {row['p50ms']:.2f} / {row['p95ms']:.2f} | "
        f"{row['wall_mbps']:.2f} | {row['psnr']:.2f} | "
        f"{row['max_idr']/1000:.0f} / {row['IDR']/1000:.0f} | {row['max']/1000:.0f} |"
    )
(output / "matrix.md").write_text("\n".join(lines) + "\n")
