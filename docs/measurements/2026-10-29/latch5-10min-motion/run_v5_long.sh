#!/bin/zsh
# 10 minutes of the sweep test pattern (motion every frame) from stuzha to this Mac over the
# tailnet at 1280x720 60 fps 8 Mbps: memory and CPU on both ends, fps and loss every second.
OUT=${1:-/tmp/v5-long}
mkdir -p $OUT
ssh stuzha 'pkill -x latch-serve; cd /tmp && LATCH_TEST_PATTERN=sweep setsid nohup /tmp/latch-serve --bind 100.70.121.94 --port 47860 > /tmp/latch-serve.log 2>&1 &'
sleep 1
ssh stuzha 'python3 /tmp/proc_cpu.py latch-serve 610' > $OUT/stuzha-cpu.txt 2>&1 &
SP=$!
/Users/daniel/Desktop/dev/latch-int/target/release/examples/view 100.70.121.94 --port 47860 --width 1280 --height 720 --fps 60 --kbps 8000 --seconds 600 > $OUT/view.json 2> $OUT/view.log &
VP=$!
sleep 3
: > $OUT/mac-mem.txt
while kill -0 $VP 2>/dev/null; do ps -o rss=,%cpu= -p $VP >> $OUT/mac-mem.txt; sleep 10; done
wait $SP
ssh stuzha 'cat /tmp/latch-serve.log; pkill -x latch-serve' > $OUT/serve.log 2>&1
echo done
