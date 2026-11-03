#!/bin/zsh
# matrix.sh ROUNDS SECONDS: interleaved base/after runs, linux host -> this Mac.
rounds=$1; secs=$2
configs=("1920 1080 sweep 20000" "1920 1080 noise 30000" "2560 1440 sweep 20000" "2560 1440 noise 30000" "1280 720 desktop 20000" "1920 1080 x9 20000")
for r in $(seq 1 $rounds); do
  if (( r % 2 )); then order=(base after); else order=(after base); fi
  for c in $configs; do
    set -- ${=c}
    for b in $order; do /tmp/gfn-h-bench/run.sh $b $b-$1x$2-$3-r$r $1 $2 60 $4 $secs $3; sleep 1; done
  done
done
