#!/usr/bin/env python3
"""Short paced runs. The baseline needs the updated bench harness, not governor code."""
import csv
import io
import pathlib
import subprocess
import sys

machine, before, after, destination = sys.argv[1:]
root = pathlib.Path(destination)
root.mkdir(parents=True, exist_ok=True)
rows = []
for repeat in range(2):
    for width, height in [(1280, 720), (1920, 1080)]:
        for kind in ['typing', 'scroll', 'motion']:
            builds = [('before', before, 0), ('off', after, 0), ('on', after, 65)]
            if repeat:
                builds.reverse()
            for label, binary, budget in builds:
                name = f'{machine}-{width}-{kind}-{label}-{repeat}'
                args = [binary, '--width', str(width), '--height', str(height),
                        '--kind', kind, '--fps', '60', '--frames', '90',
                        '--threads', '4', '--kbps', '8000', '--timings', 'yes',
                        '--paced', 'yes', '--budget', str(budget)]
                run = subprocess.run(args, text=True, capture_output=True, check=True)
                (root / f'{name}.csv').write_text(run.stdout)
                (root / f'{name}.timings').write_text(run.stderr)
                row = next(csv.DictReader(io.StringIO(run.stdout)))
                row.update(machine=machine, build=label, repeat=repeat)
                rows.append(row)
                print(name, row['ms_frame'], row['p95_ms'], flush=True)
with (root / f'{machine}-matrix.csv').open('w') as f:
    writer = csv.DictWriter(f, fieldnames=rows[0].keys())
    writer.writeheader()
    writer.writerows(rows)
