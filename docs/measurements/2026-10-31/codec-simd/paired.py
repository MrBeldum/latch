#!/usr/bin/env python3
"""Paired 30-frame bursts; assert quality fields and bitstreams are identical."""
import argparse
import csv
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile

p = argparse.ArgumentParser()
p.add_argument('before')
p.add_argument('after')
p.add_argument('inputs', type=Path)
p.add_argument('--runs', type=int, default=3)
p.add_argument('--widths', type=int, nargs='+', default=[1280, 1920])
p.add_argument('--threads', type=int, nargs='+', default=[2, 4])
p.add_argument('--kinds', nargs='+', default=['typing', 'scroll', 'video', 'motion', 'noise', 'testsrc2', 'mandelbrot'])
a = p.parse_args()
writer = None
with tempfile.TemporaryDirectory(prefix='latch-paired-') as temp:
    output = Path(temp) / 'stream.h264'
    for w in a.widths:
        h = {1280: 720, 1920: 1080}[w]
        for threads in a.threads:
            for kind in a.kinds:
                for run in range(a.runs):
                    results = []
                    builds = [('before', a.before), ('after', a.after)]
                    if run % 2:
                        builds.reverse()
                    for label, binary in builds:
                        cmd = [binary, '--width', str(w), '--height', str(h), '--threads', str(threads),
                               '--kind', kind, '--frames', '30', '--fps', '60', '--kbps', '8000',
                               '--metrics', 'yes', '--output', str(output)]
                        if kind in ['testsrc2', 'mandelbrot']:
                            cmd += ['--input', str(a.inputs / f'{kind}-{w}.yuv')]
                        row = next(csv.DictReader(subprocess.check_output(cmd, text=True).splitlines()))
                        row['sha256'] = hashlib.sha256(output.read_bytes()).hexdigest()
                        if writer is None:
                            writer = csv.DictWriter(sys.stdout, fieldnames=['build', 'run'] + list(row))
                            writer.writeheader()
                        writer.writerow(dict(build=label, run=run, **row))
                        sys.stdout.flush()
                        results.append(row)
                    assert all(results[0][k] == results[1][k] for k in results[0] if k != 'ms_frame'), results
