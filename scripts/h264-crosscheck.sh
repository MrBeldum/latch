#!/bin/sh
# FFmpeg is an independent encoder/decoder oracle, never a runtime dependency.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ffmpeg=${FFMPEG:-/opt/homebrew/bin/ffmpeg}
dest=${H264_CROSSCHECK_DIR:-$root/crates/codec/tests/conformance/crosscheck}
frames=${H264_CROSSCHECK_FRAMES:-24}
mkdir -p "$dest"
case ${1:-} in
--bench)
    for rate in 8 10; do
        "$ffmpeg" -hide_banner -loglevel error -f lavfi -i testsrc2=size=1920x1080:rate=30 -frames:v 120 -an -c:v libx264 -preset medium -profile:v baseline -b:v "${rate}M" -maxrate "${rate}M" -bufsize "${rate}M" -x264-params ref=4:bframes=0:scenecut=0 -f h264 -y "$dest/bench-1080-${rate}m.h264"
    done
    "$ffmpeg" -hide_banner -loglevel error -f lavfi -i 'color=c=0x293344:size=1920x1080:rate=30,drawgrid=width=80:height=80:thickness=1:color=0x394455' -f lavfi -i testsrc2=size=320x240:rate=30 -filter_complex '[0:v][1:v]overlay=x=mod(n*7\,1500):y=mod(n*3\,800):shortest=1' -frames:v 120 -an -c:v libx264 -profile:v baseline -b:v 8M -maxrate 8M -bufsize 8M -x264-params ref=4:bframes=0:scenecut=0 -f h264 -y "$dest/bench-desktop.h264"
    "$ffmpeg" -hide_banner -loglevel error -f lavfi -i testsrc2=size=3840x2160:rate=30 -frames:v 60 -an -c:v libx264 -profile:v baseline -b:v 20M -x264-params ref=4:bframes=0:scenecut=0 -f h264 -y "$dest/bench-2160.h264"
    ;;
*)
    i=0
    for content in testsrc2 mandelbrot noise; do
        for qp in 10 26 45; do
            for refs in 1 4; do
                case $((i % 3)) in 0) size=642x362;; 1) size=1366x768;; 2) size=1920x1080;; esac
                case $content in
                    testsrc2) source="testsrc2=size=$size:rate=30";;
                    mandelbrot) source="mandelbrot=size=$size:rate=30,format=yuv420p";;
                    noise) source="nullsrc=size=$size:rate=30,noise=alls=65:allf=t:all_seed=12345,format=yuv420p";;
                esac
                case $((i % 3)) in 0) filter=deblock=0,0;; 1) filter=no-deblock=1;; 2) filter=deblock=-3,2;; esac
                name="$content-qp$qp-ref$refs-$size"
                "$ffmpeg" -hide_banner -loglevel error -f lavfi -i "$source" -frames:v "$frames" -an -c:v libx264 -preset medium -profile:v baseline -qp "$qp" -x264-params "ref=$refs:slices=4:bframes=0:scenecut=0:$filter" -f h264 -y "$dest/$name.h264"
                i=$((i + 1))
            done
        done
    done
    "$ffmpeg" -hide_banner -loglevel error -f lavfi -i testsrc2=size=642x362:rate=30 -frames:v "$frames" -an -c:v libx264 -profile:v baseline -qp 26 -x264-params keyint=1:slices=4 -f h264 -y "$dest/intra-only.h264"
    for profile in baseline constrained_baseline; do
        if "$ffmpeg" -hide_banner -loglevel error -f lavfi -i testsrc2=size=642x362:rate=30 -frames:v "$frames" -an -c:v h264_videotoolbox -allow_sw 0 -realtime 1 -profile:v "$profile" -bf 0 -b:v 4M -f h264 -y "$dest/videotoolbox-$profile.h264" 2> "$dest/videotoolbox-$profile.log"; then
            printf 'VideoToolbox %s: generated\n' "$profile"
        else
            rm -f "$dest/videotoolbox-$profile.h264"
            printf 'VideoToolbox %s unavailable: ' "$profile"
            cat "$dest/videotoolbox-$profile.log"
        fi
    done
    ;;
esac
for stream in "$dest"/*.h264; do
    "$ffmpeg" -hide_banner -loglevel error -threads 1 -i "$stream" -an -fps_mode passthrough -pix_fmt yuv420p -f rawvideo -y "${stream%.h264}.yuv"
done
printf 'Generated %s; H264_CROSSCHECK=1 cargo test -p latch-codec --test decode_crosscheck -- --nocapture\n' "$dest"
