#!/bin/zsh
# run.sh BUILD LABEL W H FPS KBPS SECONDS PATTERN(sweep|noise|desktop)
# Host: ~/gfn-h/BUILD/serve on linux, port 47887. Viewer: /tmp/gfn-h-bench/view on this Mac.
build=$1 label=$2 w=$3 h=$4 fps=$5 kbps=$6 secs=$7 pat=$8
out=/tmp/gfn-h-bench/out; mkdir -p $out
SSH=(ssh -o ConnectTimeout=20 -o ControlMaster=auto -o ControlPath=/tmp/gfn-h-ssh-%C -o ControlPersist=60m -o ServerAliveInterval=10 linux)
$SSH "pkill -x serve; sleep 0.5" 2>/dev/null
if [[ $pat == desktop ]]; then envs="DISPLAY=:1 XAUTHORITY=/run/user/1004/latch.Xauthority"; keys=0
elif [[ $pat == x9 ]]; then envs="DISPLAY=:9 XAUTHORITY=/home/daniel/gfn-h/xauth9"; keys=0
else envs="LATCH_TEST_PATTERN=$pat"; keys=${KEYS:-30}; fi
[[ $pat == x9 ]] && $SSH "bash ~/gfn-h/x9.sh play 640 360 400 300" >/dev/null
$SSH "cd ~/gfn-h/$build && (env $envs RUST_LOG=info nohup ./serve --bind 100.70.121.94 --port 47887 > ~/gfn-h/out-$label.log 2>&1 &)"
sleep 1.5
# CPU of the serve process (all threads) over the run: utime+stime ticks at start and end.
$SSH 'p=$(pgrep -x serve); t=$(awk "{print \$14+\$15}" /proc/$p/stat); echo $p $t $(date +%s.%N) > ~/gfn-h/cpu0' 
/tmp/gfn-h-bench/view 100.70.121.94 --port 47887 --width $w --height $h --fps $fps --kbps $kbps --seconds $secs --keys $keys > $out/$label.json 2> $out/$label.err
cpu=$($SSH 'read p t0 s0 < ~/gfn-h/cpu0; t1=$(awk "{print \$14+\$15}" /proc/$p/stat 2>/dev/null); s1=$(date +%s.%N); hz=$(getconf CLK_TCK); awk -v a=$t0 -v b=$t1 -v x=$s0 -v y=$s1 -v hz=$hz "BEGIN{printf \"%.0f\", (b-a)/hz/(y-x)*100}"; echo " load $(cut -d" " -f1 /proc/loadavg)"')
$SSH "pkill -x serve"
[[ $pat == x9 ]] && $SSH "bash ~/gfn-h/x9.sh noplay" >/dev/null
python3 - $out/$label.json $label "$cpu" <<'PY'
import json,sys,statistics as st
d=json.load(open(sys.argv[1]))
S=d['seconds'][2:]
fps=[s['fps'] for s in S]; host=[s['host_ms'] for s in S]; mb=[s['mbps'] for s in S]; loss=[s['loss_pct'] for s in S]
k=d['key_to_picture_ms']; fi=d.get('frame_interval_ms',{})
f=lambda v: f"{v:5.1f}" if v is not None else "   - "
print(f"{sys.argv[2]:28s} fps med {st.median(fps):5.1f} min {min(fps):5.1f} | int p99 {fi.get('p99') or 0:5.1f} | key p50 {f(k['p50'])} p95 {f(k['p95'])} n {k['n']} | host {st.median(host):5.2f} ms | {st.median(mb):5.2f} Mbps | loss max {max(loss):4.1f} | host cpu {sys.argv[3]} | {d['encoder']}")
PY
