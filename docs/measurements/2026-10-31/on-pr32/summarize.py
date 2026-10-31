#!/usr/bin/env python3
"""Summarize saved viewer JSON; medians across steady per-second samples."""
import csv
import json
from pathlib import Path
from statistics import median

root = Path(__file__).parent
rows = []
for path in sorted((root / 'raw').glob('integrated-*.json')):
    d = json.loads(path.read_text())
    samples = d['seconds'][1:]
    intervals, keys = d.get('frame_interval_ms', {}), d['key_to_picture_ms']
    rows.append(dict(
        run=path.stem, fps=median(s['fps'] for s in samples),
        sd=intervals.get('stdev'), p99=intervals.get('p99'),
        key_n=keys['n'], key_p50=keys['p50'], key_p95=keys['p95'],
        host_ms=median(s['host_ms'] for s in samples),
        mbps=median(s['mbps'] for s in samples),
        loss=max(s['loss_pct'] for s in samples),
    ))
with (root / 'runs.csv').open('w', newline='') as out:
    writer = csv.DictWriter(out, fieldnames=rows[0].keys(), lineterminator='\n')
    writer.writeheader()
    writer.writerows(rows)
print('| Path / pattern | Pair | fps | Interval SD / p99 ms | Key p50 / p95 ms | Host ms | Mbps | Loss % |')
print('|---|---|---:|---:|---:|---:|---:|---:|')
summary = []
for location in ['loop', 'path']:
    for pattern in ['sweep', 'noise']:
        for pair in ['pr32', 'merged']:
            group = [r for r in rows if r['run'].startswith(f'integrated-{location}-{pair}-') and
                     (('noise' in r['run']) == (pattern == 'noise'))]
            if not group:
                continue
            m = {k: median(r[k] for r in group) if group[0][k] is not None else None
                 for k in ['fps', 'sd', 'p99', 'key_p50', 'key_p95', 'host_ms', 'mbps', 'loss']}
            m.update(location=location, pattern=pattern, pair=pair, runs=len(group))
            summary.append(m)
            key = f"{m['key_p50']:.1f} / {m['key_p95']:.1f}" if m['key_p50'] is not None else '—'
            print(f"| {location} / {pattern} | {pair} | {m['fps']:.1f} | {m['sd']:.2f} / {m['p99']:.2f} | {key} | {m['host_ms']:.2f} | {m['mbps']:.2f} | {m['loss']:.1f} |")
(root / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
