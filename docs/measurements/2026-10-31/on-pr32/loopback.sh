#!/bin/zsh
set -e
for r in 1 2 3; do
 KEYS=40 /tmp/latch-bench/measure.sh /tmp/latch-bench/serve-pr32 /tmp/latch-bench/view-pr32-timing integrated-loop-pr32-$r 1920 1080 60 20000 8 sweep
 KEYS=40 /tmp/latch-bench/measure.sh /tmp/latch-bench/serve-merged /tmp/latch-bench/view-merged integrated-loop-merged-$r 1920 1080 60 20000 8 sweep
done
KEYS=0 /tmp/latch-bench/measure.sh /tmp/latch-bench/serve-pr32 /tmp/latch-bench/view-pr32-timing integrated-loop-pr32-noise 1920 1080 60 30000 8 noise
KEYS=0 /tmp/latch-bench/measure.sh /tmp/latch-bench/serve-merged /tmp/latch-bench/view-merged integrated-loop-merged-noise 1920 1080 60 30000 8 noise
