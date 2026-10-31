#!/usr/bin/env python3
"""Run from repository root after collecting both machine directories."""
import collections
import csv
import math
import pathlib
import statistics

root = pathlib.Path(__file__).resolve().parent
groups = collections.defaultdict(list)
for machine in ['mac', 'stuzha']:
    for row in csv.DictReader((root / machine / f'{machine}-matrix.csv').open()):
        groups[machine, int(row['width']), row['kind'], row['build']].append(row)

def percentile(values, percent=95):
    return sorted(values)[math.ceil(len(values) * percent / 100) - 1]

def frame_rows(machine, width, kind, build, repeat):
    name = f'{machine}-{width}-{kind}-{build}-{repeat}.timings'
    lines = (root / machine / name).read_text().splitlines()
    return list(csv.DictReader([s for s in lines if not s.startswith('summary:')]))

output = ['Two 90-frame paced runs per row, 60 fps / 8,000 kbps / four workers. Mean, p95 and max exclude the first IDR in each run (178 P frames); IDR is reported separately. FPS includes all 90 frames and the final display interval. Quality and bitrate include IDRs. PSNR is the combined visible Y/U/V metric. Rates and FPS are means of the two runs; p95 is pooled nearest rank, max is the worst observed.\n',
'| Machine | Size | Kind | Build | Mean ms | P95 ms | Max ms | Max IDR ms | Paced fps | PSNR dB | Actual kbps |',
'|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|']
summary = []
for (machine, width, kind, build), rows in sorted(groups.items(), key=lambda pair: (pair[0][0], pair[0][1], ['typing','scroll','motion'].index(pair[0][2]), ['before','off','on'].index(pair[0][3]))):
    frames = [frame for row in rows for frame in frame_rows(machine,width,kind,build,row['repeat'])[1:]]
    times = [float(f['ms']) for f in frames]
    mean = lambda name: statistics.mean(float(row[name]) for row in rows)
    row = dict(machine=machine, width=width, kind=kind, build=build,
               mean=statistics.mean(times), p95=percentile(times), max=max(times),
               idr=max(float(r['idr_ms']) for r in rows), fps=mean('paced_fps'),
               psnr=mean('psnr'), kbps=mean('achieved_kbps'))
    summary.append(row)
    output.append(f"| {machine} | {width} | {kind} | {build} | {row['mean']:.3f} | {row['p95']:.3f} | {row['max']:.3f} | {row['idr']:.3f} | {row['fps']:.2f} | {row['psnr']:.3f} | {row['kbps']:.1f} |")
(root / 'results.md').write_text('\n'.join(output)+'\n')
import json
(root / 'summary.json').write_text(json.dumps(summary, indent=2)+'\n')
for machine in ['mac','stuzha']:
    for width in [1280,1920]:
        frames = [f for repeat in [0,1] for f in frame_rows(machine,width,'motion','on',repeat)[1:]]
        levels = collections.Counter(f['effort'] for f in frames)
        at_floor = [float(f['ms']) for f in frames if f['effort']=='4']
        slowest = collections.Counter()
        ratios=[]
        for f in frames:
            bands=[float(v) for v in f['bands_ms'].split(';')]
            slowest[max(range(len(bands)),key=bands.__getitem__)]+=1
            ratios.append(max(bands)/min(bands))
        print(machine,width,'levels',dict(levels),'floor p95',percentile(at_floor) if at_floor else None,
              'slowest band frequency',dict(sorted(slowest.items())), 'median max/min',statistics.median(ratios))
