#!/bin/zsh
# The real latch-host 5.0 service on stuzha, briefly in place of 4.1.1: run by hand against a
# copy of its data folder (the owner's units, files and :1 desktop untouched; no input sent),
# streamed from this Mac through the real control port, then 4.1.1 restored.
OUT=${1:-/tmp/v5-service}
mkdir -p $OUT
ssh stuzha 'set -e
  rm -rf /tmp/latch-v5-data; cp -a ~/.local/share/latch /tmp/latch-v5-data
  systemctl --user is-active latch.service latch-engine.service || true
  systemctl --user stop latch.service latch-engine.service
  pkill -x latch-host-v5 || true
  cd /tmp && DISPLAY=:1 XAUTHORITY=/run/user/$(id -u)/latch.Xauthority PATH=/usr/local/bin:/usr/bin:/bin \
    LATCH_DISPLAY_FOLLOWS_VIEWER=1 LATCH_DATA_DIR=/tmp/latch-v5-data \
    setsid nohup /tmp/latch-host-v5 --background > /tmp/latch-v5.log 2>&1 &
  sleep 4; grep -i prep /tmp/latch-v5-data/host.toml || echo "no prep in host.toml"' > $OUT/setup.txt 2>&1
for i in $(seq 1 15); do curl -fsS --max-time 2 http://100.70.121.94:47850/v1/status > $OUT/status.json 2>/dev/null && break; sleep 1; done
python3 -c "import json; s=json.load(open('$OUT/status.json')); print('status:', s.get('app'), s.get('version'), s['streamer'])" > $OUT/status.txt 2>&1
ssh stuzha 'python3 /tmp/proc_cpu.py latch-host-v5 22' > $OUT/stuzha-cpu.txt 2>&1 &
SP=$!
sleep 1
/Users/daniel/Desktop/dev/latch-int/target/release/examples/view 100.70.121.94 --port 47850 --width 1280 --height 720 --fps 60 --kbps 10000 --seconds 15 > $OUT/view.json 2> $OUT/view.log
echo "view exit $?" >> $OUT/status.txt
wait $SP
ssh stuzha 'sleep 2; cat /tmp/latch-v5.log | tail -30; echo ---journal; journalctl --user -t stuzha-display --since "-5 min" --no-pager 2>/dev/null | tail -6; journalctl -t stuzha-display --since "-5 min" --no-pager 2>/dev/null | tail -6' > $OUT/host-log.txt 2>&1
ssh stuzha 'pkill -x latch-host-v5; sleep 1; systemctl --user start latch-engine.service latch.service; sleep 6; systemctl --user is-active latch.service latch-engine.service' > $OUT/restore.txt 2>&1
curl -fsS --max-time 3 http://100.70.121.94:47850/v1/status | python3 -c "import json,sys; s=json.load(sys.stdin); print('restored:', s.get('version'), s['streamer']['kind'], s['streamer']['running'])" >> $OUT/restore.txt 2>&1
cat $OUT/status.txt $OUT/restore.txt
