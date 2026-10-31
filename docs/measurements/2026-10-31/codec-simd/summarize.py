#!/usr/bin/env python3
"""Print medians of paired runs, including quality and reciprocal throughput."""
import csv
from pathlib import Path
import statistics

root = Path(__file__).parent
print('| Machine | Size | Threads | Kind | Before ms / fps | After ms / fps | Speedup | PSNR dB | Actual kbps |')
print('|---|---|---:|---|---:|---:|---:|---:|---:|')
for machine in ['mac', 'stuzha']:
    rows = list(csv.DictReader((root / f'{machine}-paired.csv').open()))
    for w in ['1280', '1920']:
        for threads in ['2', '4']:
            for kind in ['typing', 'scroll', 'video', 'motion', 'noise', 'testsrc2', 'mandelbrot']:
                group = [r for r in rows if (r['width'], r['threads'], r['kind']) == (w, threads, kind)]
                if len(group) != 6:
                    continue
                times = [statistics.median(float(r['ms_frame']) for r in group if r['build'] == build) for build in ['before', 'after']]
                b, a = times
                r = group[0]
                assert len({r['sha256'] for r in group}) == 1
                print(f"| {machine} | {w}×{r['height']} | {threads} | {kind} | {b:.3f} / {1000/b:.1f} | {a:.3f} / {1000/a:.1f} | {b/a:.2f}× | {r['psnr']} | {r['achieved_kbps']} |")
