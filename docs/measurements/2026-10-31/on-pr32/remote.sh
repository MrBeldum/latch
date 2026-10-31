#!/bin/zsh
set -e
ssh stuzha "mkdir -p /tmp/latch-bench"
scp /tmp/latch-bench/serve-pr32-arm stuzha:/tmp/latch-bench/serve-pr32
scp /tmp/latch-bench/serve-merged-arm stuzha:/tmp/latch-bench/serve-new
for r in 1 2 3; do
 KEYS=40 VIEW=/tmp/latch-bench/view-pr32-timing /tmp/latch-bench/remote.sh ./serve-pr32 integrated-path-pr32-$r 1920 1080 60 20000 8 sweep
 KEYS=40 VIEW=/tmp/latch-bench/view-merged /tmp/latch-bench/remote.sh ./serve-new integrated-path-merged-$r 1920 1080 60 20000 8 sweep
done
KEYS=0 VIEW=/tmp/latch-bench/view-pr32-timing /tmp/latch-bench/remote.sh ./serve-pr32 integrated-path-pr32-noise 1920 1080 60 30000 8 noise
KEYS=0 VIEW=/tmp/latch-bench/view-merged /tmp/latch-bench/remote.sh ./serve-new integrated-path-merged-noise 1920 1080 60 30000 8 noise
