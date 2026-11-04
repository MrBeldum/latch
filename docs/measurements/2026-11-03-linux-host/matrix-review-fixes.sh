#!/bin/zsh
# matrix-review-fixes.sh ROUNDS SECONDS: interleaved runs of the PR head before the review fixes (after) and with them (h3).
rounds=$1; secs=$2
configs=("1920 1080 sweep 20000" "1920 1080 noise 30000" "2560 1440 sweep 20000" "1920 1080 x9 20000")
for r in $(seq 1 $rounds); do
  if (( r % 2 )); then order=(after h3); else order=(h3 after); fi
  for c in $configs; do
    set -- ${=c}
    for b in $order; do $(dirname $0)/run.sh $b $b-$1x$2-$3-r$r $1 $2 60 $4 $secs $3; sleep 1; done
  done
done
