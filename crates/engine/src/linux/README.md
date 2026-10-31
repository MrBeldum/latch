# Linux streaming

`open_video` captures the X11 root window at its current size. The capture
requires a 32-bit little-endian BGR visual. x11rb handles `DISPLAY` and
`XAUTHORITY`; no Xlib is linked. MIT-SHM 1.2 server-allocated segments are
mapped read-only, with GetImage as the fallback. Damage bounds the pixel
transfer, and XFixes supplies the premultiplied cursor. RandR and root
ConfigureNotify events rebuild the image and SHM allocation after a resize;
so does a GetImage that fails because the root shrank after the event poll.
Rows whose pixels did not change are not copied, and a grab where none did
(and the cursor stayed) reports no change, so the encoder repeats instead.

X11 initialization has one elapsed deadline (2 s for capture/XTest, 10 s
for RandR), covering display parsing, DNS, Unix/abstract/TCP socket connect,
authority-file open/read and the setup exchange. A short-lived copy of the
executable connects and selects credentials using x11rb, then passes the live
socket and credentials over a private inherited socket. The parent kills and
reaps it on completion or failure, allowing up to 100 ms of cleanup beyond
the deadline. If SIGKILL cannot finish an uninterruptible kernel operation,
the single helper slot is retained and further attempts fail until it exits;
the next attempt reaps it before proceeding. No unbounded waitpid or growing
set of workers is used. Concurrent bootstrap attempts wait within their own deadline for the slot; an unreaped timed-out helper returns a retry error. Setup
runs in the parent with the remaining deadline, including slow partial replies.
`DISPLAY` and `XAUTHORITY`/`HOME/.Xauthority` selection are unchanged; TCP
and hostname displays remain accepted (MIT-SHM still requires a local server).
Cookies are never printed or passed as command-line arguments. A network or
stalled authority path returns an initialization error; use an accessible X
server and a readable authority file. Embedders must call
`latch_engine::linux::run_helper()` before normal startup and return when it
returns true. The shipping host and the capture/input/serve examples do this.
The helper process needs the running executable to remain executable.

Every X round trip fails after 2 s without a byte from the server (RandR:
10 s), so a stalled X server ends a grab with an error instead of holding the
session's stop and display restore. Rootless Xwayland (the XWAYLAND
extension, or RandR outputs named `XWAYLAND*`) is refused with a message to
log in to an Xorg session; so is a failed connection while
`WAYLAND_DISPLAY`/`XDG_SESSION_TYPE` say Wayland. A separate Xorg server on
`DISPLAY` inside a Wayland session still works.

RandR sizes follow the CRTC's rotation: a request is a size on screen, and a
quarter-turned CRTC gets the mode with swapped sides. Modes that `prepare`
added to the output are removed again by `restore`, after it has switched
back; modes the server had before are left alone.

Audio uses Colin Marc's `pulseaudio` 0.3.2 native-protocol client, rather than
libpulse or a child process. Its normal and build dependency graph contains
no `cc`, `cmake`, or `nasm-rs`. The default sink's monitor is requested as
48 kHz stereo S16LE, with 10 ms fragments; the server performs conversion.
A bounded 64-frame queue absorbs protocol batches without blocking the
reactor. If the consumer stalls beyond that queue, incoming frames are
dropped. Reads return false on timeout and leave the destination unchanged.
The client ignores the server's notice that it killed a recording, so while
reads time out a latency query is sent at most once a second; the server
answers it with an error for a killed stream, and the read returns that error.

Text temporarily binds an unused keycode to a Unicode keysym. X11 has no
acknowledgement that a client has consumed a key event, so each character
allows 12 ms before the mapping is reused or restored. This was verified
with XFCE Terminal, including Latin and Greek text. A stalled application
or another client remapping the same spare keycode remains a race.

## Reproduce the checks

On the Mac build machine:

```sh
export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp
cargo fmt --all
cargo clippy -p latch-engine --all-targets -- -D warnings
cargo clippy -p latch-engine --target aarch64-unknown-linux-gnu --all-targets -- -D warnings
cargo test -p latch-engine --all-targets
cargo zigbuild --release --target aarch64-unknown-linux-gnu -p latch-engine --examples --tests
```

Run Linux tests and examples with `nice` on an isolated X server. Never use
Stuzha's automation display `:1` for injection or resizing. `linux_input`
refuses displays other than `:9`; it creates its own receiver window, checks
pointer coordinates, button and key events, scroll remainders, keyboard
mapping restoration, known captured pixels, damage, cursor movement, and
capture resizing. It exercises an existing 1280x720 mode and a created
3024x1964_60 mode, then restores the original size. `--keys` also exercises
the shared VK keymap once that parallel implementation is available.

`linux_capture 5 out.h264` records a stream and reports capture, conversion,
and encoding times. `linux_capture 5 --capture-only` forces fresh full-root
grabs at 60 Hz without calling the codec. `linux_audio 4` checks the monitor
for non-silent samples; play a quiet tone with `paplay` during the capture.

## Verified on Stuzha, 2026-10-28

A second dummy-driver Xorg on `:9` ran the input, resize, and known-pixel
checks. A subsequent authenticated server with MIT-SHM disabled passed the
same checks using GetImage. Missing X credentials and a closed display
returned errors. The test servers, terminal, and temporary files were
removed afterwards; `:1` was never injected into or resized.

For 299 full 1920x1080 SHM grabs over 5.019 seconds, paced at 60 Hz, capture
was 2.741 ms p50 and 5.813 ms p95. Process CPU was 10.26%, where one full
core is 100%. The display contained an XFCE terminal. These times include
cursor acquisition/compositing and copying into the caller's BGRA buffer.

The default `auto_null.monitor` supplied 201 ten-millisecond frames during
a four-second capture with quiet playback, including 142,562 nonzero
samples with peak magnitude 113. Nineteen idle reads timed out normally.

The Mac tests pass (two tests); the Linux binary's tests pass (six tests).
One codec integration test is ignored because this checkout's shared H.264
encoder and color conversion are still skeletons. Encode throughput and
bitrate changes inside a live encoder are not measured. VK-key injection
also awaits the shared keymap. Rotated and multiple-output RandR setups
and PipeWire's PulseAudio server have not been tested.

## Qualification on Stuzha, 2026-10-30

The bootstrap implementation and concurrent-initialization fix (497f431) passed
eight regressions on ARM Linux, including abstract sockets, a full connect
queue, silent/partial setup, FIFO authority input, cookie selection and helper
cleanup. A separate authenticated dummy-driver Xorg on `:9` passed XTest,
Unicode keymap restoration, resize to an existing mode, creation of a larger
mode, restoration, known-pixel capture and damage/cursor checks. Captured H.264
also decoded successfully with FFmpeg.

The candidate engine streamed moving testsrc2 content to the Mac viewer at
actual 1280x720 and 1920x1080 dimensions for ten seconds each. The 720p run
reported no loss; the 1080p run had one zero-frame sample with reported loss
and recovered in the next sample. Its cause is not isolated, so sustained
network quality remains unqualified. The source ran at 30 fps; these runs do
not establish 60 fps throughput. The measurement server used a separate port
and display; the installed host and automation display were unchanged.
Temporary processes and files were removed. Audio, physical input devices,
HDR, long-run network recovery and software performance targets remain
separate qualification work.
