#!/usr/bin/env python3
"""Short serial 60-fps/8-Mbps matrix. Generate natural inputs with FFmpeg first."""
import argparse
import subprocess
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('binary')
p.add_argument('inputs', type=Path)
a = p.parse_args()
for w, h in [(1280, 720), (1920, 1080)]:
    for threads in [2, 4]:
        for kind in ['typing', 'scroll', 'video', 'motion', 'noise', 'testsrc2', 'mandelbrot']:
            cmd = [a.binary, '--width', str(w), '--height', str(h), '--threads', str(threads),
                   '--kind', kind, '--frames', '30', '--fps', '60', '--kbps', '8000', '--metrics', 'yes']
            if kind in ['testsrc2', 'mandelbrot']:
                cmd += ['--input', str(a.inputs / f'{kind}-{w}.yuv')]
            print(subprocess.check_output(cmd, text=True), end='', flush=True)
