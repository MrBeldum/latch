#!/bin/zsh
# drive.sh <app bundle> <data dir> <events log>: run the real app against the
# test serve for ~75 s while focus and Spaces change from outside; the events
# log records each action and whether the app's window was then on screen.
APP=$1; DATA=$2; EV=$3
S=${0:h}
rm -rf $DATA; mkdir -p $DATA; : > $EV
LATCH_DATA_DIR=$DATA LATCH_TEST_SERVE=127.0.0.1:47963 LATCH_TEST_EXERCISE=3 \
  RUST_LOG=info,latch_client::video=debug,wgpu_hal=error \
  $APP/Contents/MacOS/Latch > $DATA/stderr.log 2>&1 &
P=$!
note() { print "$(date -u +%H:%M:%S.%N | cut -c1-12) $1 $(osascript -l JavaScript $S/onscreen.js $P)" >> $EV; }
sleep 6; note start
for i in 1 2 3 4 5 6; do
  open -a Finder; sleep 2.5; note "finder $i"
  open -a $APP; sleep 2; note "latch $i"
  osascript -e 'tell application "System Events" to key code 124 using control down' 2>>$EV; sleep 1.5; note "space-right $i"
  osascript -e 'tell application "System Events" to key code 123 using control down' 2>>$EV; sleep 1.5; note "space-left $i"
  open -a $APP; sleep 0.5
done
sleep 5; note end
kill -0 $P 2>/dev/null && print "app alive at the end" >> $EV
kill $P; wait $P; print "exit $?" >> $EV
