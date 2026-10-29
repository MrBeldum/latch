# proc_cpu.py NAME SECONDS: per-second CPU% (of one core) and RSS of the process named NAME.
import os, sys, time
name, secs = sys.argv[1], int(sys.argv[2])
hz = os.sysconf("SC_CLK_TCK")
def pid():
    for p in os.listdir("/proc"):
        if p.isdigit():
            try:
                if open(f"/proc/{p}/comm").read().strip() == name:
                    return p
            except OSError:
                pass
p = pid()
if not p:
    sys.exit(f"no process {name}")
def ticks():
    f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
    return int(f[11]) + int(f[12])
def rss():
    for l in open(f"/proc/{p}/status"):
        if l.startswith("VmRSS"):
            return int(l.split()[1]) // 1024
last, t0 = ticks(), time.time()
for i in range(secs):
    time.sleep(1)
    now, t1 = ticks(), time.time()
    print(f"{i+1} cpu% {100*(now-last)/hz/(t1-t0):.1f} rss_mb {rss()}", flush=True)
    last, t0 = now, t1
