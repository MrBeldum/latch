#!/bin/bash
# x9.sh start|stop|play W H|noplay : isolated Xvfb on :9 with its own cookie
cd ~/gfn-h
case $1 in
start)
  rm -f xauth9; touch xauth9
  xauth -f xauth9 add :9 . $(od -An -N16 -tx1 /dev/urandom | tr -d ' \n') >/dev/null
  nohup Xvfb :9 -screen 0 ${2:-1920}x${3:-1080}x24 -nolisten tcp -auth ~/gfn-h/xauth9 > xvfb9.log 2>&1 &
  echo $! > xvfb9.pid; sleep 1; echo started $(cat xvfb9.pid);;
play)
  DISPLAY=:9 XAUTHORITY=~/gfn-h/xauth9 nohup ffplay -loglevel error -f lavfi -i "testsrc2=size=${2}x${3}:rate=60" -x $2 -y $3 -left ${4:-0} -top ${5:-0} -noborder > ffplay9.log 2>&1 &
  echo $! > ffplay9.pid; sleep 2; echo playing $(cat ffplay9.pid);;
noplay)
  [ -f ffplay9.pid ] && kill $(cat ffplay9.pid) 2>/dev/null; rm -f ffplay9.pid; echo stopped ffplay;;
stop)
  [ -f ffplay9.pid ] && kill $(cat ffplay9.pid) 2>/dev/null
  [ -f xvfb9.pid ] && kill $(cat xvfb9.pid) 2>/dev/null
  rm -f ffplay9.pid xvfb9.pid xauth9; echo stopped;;
esac
