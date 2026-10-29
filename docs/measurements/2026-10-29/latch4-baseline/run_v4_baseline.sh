#!/bin/zsh
# Latch 4 baseline: stream stuzha at 720p for 30 s with the old client, sampling CPU on both ends.
cd /tmp/latch-v4
BIN=$(/bin/ls -t target/release/deps/latch_client-* | grep -v '\.d$' | head -1)
echo "BIN=$BIN"
until ! ssh stuzha pgrep -f proc_cpu.py >/dev/null; do sleep 1; done
ssh stuzha 'python3 /tmp/proc_cpu.py sunshine 45' > /tmp/v4-stuzha-cpu.txt 2>&1 &
sleep 1
export LATCH_DATA_DIR=/tmp/latch-v4-data LATCH_TEST_PC=stuzha LATCH_TEST_RESOLUTIONS=720p LATCH_TEST_SECONDS=30 LATCH_TEST_NO_INPUT=1 TMPDIR=/tmp
"$BIN" --ignored --exact session::tests::connect_real --nocapture > /tmp/v4-client.log 2>&1 &
CPID=$!
sleep 3
: > /tmp/v4-mac-cpu.txt
for i in $(seq 1 36); do ps -o %cpu=,rss= -p $CPID 2>/dev/null | awk -v i=$i '{print i, "cpu%", $1, "rss_mb", int($2/1024)}' >> /tmp/v4-mac-cpu.txt; sleep 1; done
wait $CPID
echo "client exit $?"
wait
