#!/bin/sh
# FFmpeg is an independent verification tool; the encoder never launches it.
set -eu
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH" TMPDIR=/tmp
cargo build --release -p latch-codec --example encode_bench
exec python3 - "$@" <<'PY'
import argparse, json, math, os, pathlib, re, resource, shutil, subprocess, tempfile, time
p=argparse.ArgumentParser()
p.add_argument('--frames',type=int,default=30)
p.add_argument('--threads',type=int,default=4)
p.add_argument('--width',type=int,default=1920)
p.add_argument('--height',type=int,default=1080)
p.add_argument('--kinds',default='still,typing,scroll,motion,noise')
p.add_argument('--bitrates',default='8000,20000')
p.add_argument('--directory')
a=p.parse_args()
ffmpeg=shutil.which('ffmpeg')
if not ffmpeg:
 print('SKIP: ffmpeg is unavailable');raise SystemExit(0)
folder=pathlib.Path(a.directory or tempfile.mkdtemp(prefix='latch-encoder-compare-'))
folder.mkdir(parents=True,exist_ok=True)
def run(args):
 before=resource.getrusage(resource.RUSAGE_CHILDREN);start=time.perf_counter()
 r=subprocess.run(args,text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True)
 after=resource.getrusage(resource.RUSAGE_CHILDREN)
 return r,(time.perf_counter()-start)*1000/a.frames,((after.ru_utime-before.ru_utime)+(after.ru_stime-before.ru_stime))*1000/a.frames
def psnr(stream,raw):
 r,_,_=run([ffmpeg,'-hide_banner','-i',str(stream),'-f','rawvideo','-pixel_format','yuv420p','-video_size',f'{a.width}x{a.height}','-framerate','30','-i',str(raw),'-filter_complex','[0:v][1:v]psnr','-frames:v',str(a.frames),'-f','null','-'])
 m=re.search(r'PSNR.*average:([\d.inf]+)',r.stderr)
 if not m:raise RuntimeError(r.stderr)
 return float(m[1])
rows=[]
print('| Content | Target Mbps | Encoder | PSNR dB | Actual Mbps | Encode wall ms/frame | Process CPU ms/frame | Max frame / budget |',flush=True)
print('|---|---:|---|---:|---:|---:|---:|---:|',flush=True)
for kbps in map(int,a.bitrates.split(',')):
 for kind in a.kinds.split(','):
  prefix=folder/f'{kind}-{kbps}'
  ours=prefix.with_suffix('.h264');raw=prefix.with_suffix('.yuv');x264=folder/f'{kind}-{kbps}-x264.h264'
  r,wall,cpu=run(['target/release/examples/encode_bench','--width',str(a.width),'--height',str(a.height),'--frames',str(a.frames),'--threads',str(a.threads),'--kbps',str(kbps),'--kind',kind,'--output',str(ours),'--source',str(raw)])
  r,wall,cpu=run(['target/release/examples/encode_bench','--width',str(a.width),'--height',str(a.height),'--frames',str(a.frames),'--threads',str(a.threads),'--kbps',str(kbps),'--input',str(raw),'--kind',kind,'--output',str(ours),'--metrics','no'])
  report=dict(zip(r.stdout.splitlines()[0].split(','),r.stdout.splitlines()[1].split(',')))
  rows.append(dict(kind=kind,kbps=kbps,encoder='latch',psnr=psnr(ours,raw),mbps=ours.stat().st_size*8*30/a.frames/1e6,wall=wall,cpu=cpu,max_budget=float(report['max_frame_budget'])))
  _,wall,cpu=run([ffmpeg,'-v','error','-y','-f','rawvideo','-pixel_format','yuv420p','-video_size',f'{a.width}x{a.height}','-framerate','30','-i',str(raw),'-frames:v',str(a.frames),'-c:v','libx264','-preset','superfast','-tune','zerolatency','-profile:v','baseline','-threads',str(a.threads),'-b:v',f'{kbps}k','-g','2147483647','-sc_threshold','0','-f','h264',str(x264)])
  rows.append(dict(kind=kind,kbps=kbps,encoder='x264 superfast',psnr=psnr(x264,raw),mbps=x264.stat().st_size*8*30/a.frames/1e6,wall=wall,cpu=cpu,max_budget=None))
  for row in rows[-2:]:
   cap='—' if row['max_budget'] is None else f"{row['max_budget']:.3f}"
   print(f"| {kind} | {kbps/1000:g} | {row['encoder']} | {row['psnr']:.3f} | {row['mbps']:.3f} | {row['wall']:.3f} | {row['cpu']:.3f} | {cap} |",flush=True)
(folder/'results.json').write_text(json.dumps(rows,indent=2))
print(f'Artifacts: {folder}',flush=True)
PY
