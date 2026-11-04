#!/bin/zsh
# summary.sh <panel.log>: presenter counters from the app's own log.
F=$1
print "presenter choices:   $(grep -c 'video presenter for this stream' $F) ($(grep -o 'video presenter for this stream: .*' $F | sort -u | tr '\n' ' '))"
print "presenter switches:  $(grep -c 'video presenter ->' $F)"
print "display changes:     $(grep -c 'video display changed' $F)"
print "failed warnings:     $(grep -c 'presentation failed' $F)"
print "panics:              $(grep -c 'panicked' $F)"
print "seconds counted:     $(grep -c 'last second' $F)"
print "seconds with both:   $(grep 'last second' $F | awk '{ if ($7>0 && $10>0) n++ } END {print n+0}')"
print "frames EDR / wgpu:   $(grep 'last second' $F | awk '{e+=$7; w+=$10} END {print e+0 " / " w+0}')"
print "exercise steps:      $(grep 'test exercise:' $F | grep -vc focus)"
print "focus changes:       $(grep -c 'test exercise: window focus' $F)"
print "full-screen steps:   $(grep -c 'test exercise: full screen' $F)"
