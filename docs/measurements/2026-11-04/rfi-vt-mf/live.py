import os, subprocess, time, pathlib, json
out=pathlib.Path('/tmp/latch-rfi-vt-mf')
for codec in ['h264','hevc']:
 for mode in ['idr','ltr']:
  env=os.environ|{'LATCH_TEST_PATTERN':'sweep','LATCH_TEST_VIDEOTOOLBOX':'1','LATCH_MAC_LTR':'1' if mode=='ltr' else '0'}
  with (out/f'mac-{codec}-{mode}-serve.log').open('w') as log:
   server=subprocess.Popen(['target/release/examples/serve','--port','47900','--telemetry'],env=env,stdout=log,stderr=log)
   try:
    time.sleep(1)
    for loss in [1,3]:
     label=f'mac-{codec}-{mode}-{loss}'
     args=['target/release/examples/view','127.0.0.1','--port','47900','--width','1280','--height','720','--fps','60','--kbps','20000','--seconds','20']
     if codec=='hevc': args+=['--hevc']
     r=subprocess.run(args,env=os.environ|{'LATCH_TEST_VIDEO_LOSS':f'{loss}:8'},capture_output=True,text=True,timeout=50)
     (out/f'{label}.log').write_text(r.stdout+'\nSTDERR\n'+r.stderr)
     if r.returncode: raise RuntimeError(label+r.stderr)
     start=r.stdout.find('\n{')
     summary=json.loads(r.stdout[start+1:])
     (out/f'{label}.json').write_text(json.dumps(summary,indent=2))
     print(label,json.dumps(summary['recovery']),flush=True)
   finally:
    server.terminate()
    server.wait(timeout=10)
