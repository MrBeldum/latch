# Latch stream protocol, version 1

Latch 5 streams a desktop with its own protocol. It replaces the
GameStream/Moonlight protocol that Sunshine and moonlight-common-c spoke
until Latch 4. Every byte of it is implemented in Rust in `crates/proto`.

The protocol only runs inside a Tailscale tailnet. WireGuard already
encrypts and authenticates every packet between two machines, and the
host's control service already decides which tailnet peers may use it
(`tailscale whois`, see `crates/host/src/service.rs`). So the protocol has
no encryption, no key exchange and no PIN pairing of its own. Whoever may
call the control API may stream; nobody else gets a session token.

All integers are little-endian. Times are microseconds.

## Ports

| What | Where |
| --- | --- |
| Session control (TCP) | the control port, 47850, upgraded from HTTP |
| Media (UDP) | host UDP 47850; the viewer uses any local port |

## Session start

The viewer opens a TCP connection to the host's control port and sends

```
POST /v1/stream HTTP/1.1
Host: <host ip>:47850
Connection: Upgrade
Upgrade: latch-stream/1
Content-Type: application/json
Content-Length: <n>

<StreamRequest JSON>
```

`StreamRequest`:

| Field | Type | Meaning |
| --- | --- | --- |
| `protocol` | u32 | 1 |
| `width`, `height` | u32 | picture size the viewer wants (even numbers) |
| `fps` | u32 | frames per second wanted |
| `bitrate_kbps` | u32 | video bitrate wanted (a ceiling: the host's rate control comes down from it) |
| `codecs` | [string] | what the viewer decodes, best first: `"av1"`, `"hevc"`, `"h264-high"`, `"h264-baseline"`; only hardware-supported codecs are advertised above Baseline; a name the host does not know is skipped |
| `udp_port` | u16 | the viewer's media socket port |
| `audio` | bool | send desktop audio |
| `name` | string | the viewer's machine name, for the host's log |
| `cursor` | bool | optional, default false; viewer can draw pointer locally and handle shape updates |
| `gamepads` | bool | optional, default false; the viewer sends gamepads (see [Gamepads](#gamepads-912)) |
| `reference_invalidation` | bool | optional, default false; after a loss the viewer asks for a recovery frame (see [RecoveryRequest](#recoveryrequest-14-and-recoveryack-15)) |
| `audio_channels` | u32 | optional; most channels the viewer plays (6 for 5.1, 8 for 7.1). Absent, or below 3, means stereo |
| `mic` | bool | optional, default false; the viewer offers its microphone (datagram kind 13) |
| `probe_trains` | bool | optional, default false; the viewer times probe trains (datagram kind 16) and reports their rate |

A request is taken within 64 to 16384 pixels a side, 1 to 500 fps and
500 kbps to 1 Gbps (`latch_proto::control::MAX_*`). The host then meets
it with what the encoder for the codec it picked takes (see "Encoder
limits"): a larger picture is scaled down in proportion before the desktop
is sized for it, the frame rate is lowered to the codec level's pixel rate,
and the bitrate to the level's ceiling. `Accept` says what was granted.

The control service authorizes the caller exactly as for every other
route. A refusal is an ordinary HTTP error with an `Ack` JSON body. On
success the host answers

```
HTTP/1.1 101 Switching Protocols
Connection: Upgrade
Upgrade: latch-stream/1
```

and from then on the TCP connection carries control messages.

## Control messages (TCP)

Each message is `u32 length | u8 type | payload`, where `length` counts the
type byte and the payload and is at most 65 536. Payloads are JSON objects.

| Type | Name | Direction | Payload |
| --- | --- | --- | --- |
| 1 | `Accept` | host → viewer | see below; always the first message |
| 2 | `End` | either | `{"code": u32, "message": string}`; the sender closes after it (a host half-closes and reads for up to 100 ms first, so a report in flight cannot reset the connection before the viewer reads the End) |
| 3 | `Keepalive` | either | `{}` |
| 4 | `Notice` | host → viewer | `{"kind": "audio" \| "video" \| "input", "message": string}`; `message` empty clears it |
| 5 | `Report` | either | `{"fps", "loss_pct", "decode_ms", "rtt_ms"}`, every 50 ms (older viewers: 250 ms or once a second), for bitrate and FEC feedback |
| 6 | `Cursor` | host → viewer | `{"visible": bool, "shape": null \| {"serial": u32, "width": u32, "height": u32, "hot_x": u32, "hot_y": u32, "rgba": "<base64>"}}`; only after an `Accept` with `cursor` true; sent when visibility or shape changes (`shape` null: unchanged); straight-alpha RGBA, at most 96×96 |
| 7 | `DrawCursor` | viewer → host | `{"draw": bool}`; tells host whether to composite pointer into video |

`Accept`:

| Field | Type | Meaning |
| --- | --- | --- |
| `session` | u32 | session id, in every datagram |
| `token` | string | 32 hex digits (16 random bytes); proves a datagram's sender is this viewer |
| `codec` | string | one of the viewer's `codecs` |
| `width`, `height`, `fps` | u32 | what will actually be sent |
| `bitrate_kbps` | u32 | what the encoder was set to |
| `encoder` | string | e.g. `"VideoToolbox HEVC"`, `"Media Foundation H.264 (NVIDIA)"`, `"Latch software H.264"` |
| `audio` | object or null | `{"rate": 48000, "channels": 2, "frame_samples": 240, "codec": "lpcm-rice"}`; null when there is none |
| `audio_problem` | string | why there is no audio, when `audio` is null |
| `cursor` | bool | optional, default false; host supports omitting pointer from video and sending shape updates |
| `gamepads` | u8 | optional, default 0; gamepad slots the host emulates (0: none, so the viewer sends none) |
| `gamepad_kinds` | [string] | optional; the pad kinds the host emulates as themselves, of `"xbox"`, `"playstation"`, `"generic"`; unknown names are skipped |
| `gamepad_problem` | string | optional; why the host takes no gamepads although the viewer offered them |
| `reference_invalidation` | bool | optional, default false; true only when the viewer asked and the session's encoder makes recovery frames |
| `mic` | object | optional; the format the viewer's microphone is to be sent in (`{"rate": 48000, "channels": 1, "frame_samples": 240, "codec": "lpcm-rice"}`), present only when the viewer offered it and the host has somewhere to play it |
| `mic_problem` | string | optional; why an offered microphone is not wanted, when `mic` is absent |

Either side sends `Keepalive` when it has sent nothing for one second. A
connection that has been silent for ten seconds is dead. The session ends
when the TCP connection closes, whoever closes it. `End` codes:

| Code | Meaning |
| --- | --- |
| 0 | ended on purpose (the viewer disconnected, or the host's owner stopped it) |
| 1 | another viewer took the host over |
| 2 | capture failed and could not be restarted |
| 3 | the encoder failed |
| 4 | the viewer stopped answering |
| 5 | the host is shutting down or updating |

## Datagrams (UDP)

At most 1 200 bytes, so a datagram fits Tailscale's 1 280-byte MTU over
IPv6 with room to spare. Every datagram starts with

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | `kind` |
| 1 | 1 | `flags` (per kind) |
| 2 | 2 | `aux` (per kind, else 0) |
| 4 | 4 | `session` |

A datagram with an unknown kind, a wrong session id, or a length that does
not match its kind is dropped without an answer.

| Kind | Name | Direction |
| --- | --- | --- |
| 1 | `Hello` | both |
| 2 | `Video` | host → viewer |
| 3 | `Audio` | host → viewer |
| 4 | `Input` | viewer → host |
| 5 | `InputAck` | host → viewer |
| 6 | `KeyframeRequest` | viewer → host |
| 7 | `Ping` | both |
| 8 | `Pong` | both |
| 9 | `Pad` | viewer → host |
| 10 | `PadAck` | host → viewer |
| 11 | `PadFeedback` | host → viewer |
| 12 | `PadFeedbackAck` | viewer → host |
| 13 | `Mic` | viewer → host |
| 14 | `RecoveryRequest` | viewer → host |
| 15 | `RecoveryAck` | host → viewer |
| 16 | `Probe` | host → viewer |

### Hello (1)

Viewer → host: 16 bytes of `token` follow the header. The viewer sends one
every 50 ms until any datagram from the host arrives, then once a second.
The host accepts the first `Hello` with the right session and token, and
from then on sends media to the address it came from (under userspace
Tailscale that is a loopback address tailscaled maps back to the viewer).
A later valid `Hello` from another address moves the session there.

Host → viewer: header only. The host sends one every 50 ms to the
viewer's tailnet address and `udp_port` until the viewer's `Hello`
arrives. A stateful firewall in front of either program (Windows Defender
Firewall, the macOS application firewall) then sees each side's packets
as answers to its own.

### Video (2)

`flags` bit 0: the frame is a keyframe (IDR); a viewer that lost a
reference can start from it. Bit 1, only to a viewer whose `Accept` has
`reference_invalidation`: a recovery frame, which predicts only from the
frame `recovery` frames before it (offset 26). Other bits are 0, and bits 0
and 1 are never both set. `aux`: the host's capture-to-send time for this
frame in units of 0.1 ms (saturating).

| Offset | Size | Field |
| --- | --- | --- |
| 8 | 4 | `frame` — frame number, +1 per encoded frame, wraps |
| 12 | 4 | `frame_len` — bytes in the whole encoded frame (Annex B for H.264/HEVC, low-overhead OBUs for AV1) |
| 16 | 2 | `shard` — index of this shard in its block: data shards first, then parity |
| 18 | 1 | `block` — index of the FEC block |
| 19 | 1 | `blocks` — FEC blocks in the frame |
| 20 | 2 | `data` — data shards in this block |
| 22 | 2 | `parity` — parity shards in this block |
| 24 | 2 | `shard_len` — bytes per shard in this frame |
| 26 | 2 | `recovery` — with flags bit 1: frame number minus the number of the frame it predicts from (≥ 1); otherwise 0 |
| 28 | `shard_len` | shard bytes |

The host cuts the frame into `D = ceil(frame_len / shard_len)` data shards,
the last one padded with zeros, and splits them into
`blocks = ceil(D / 64)` consecutive blocks of nearly equal size (the first
`D mod blocks` blocks get one shard more). Each block gets
`parity = ceil(data * fec_percent / 100)` parity shards, at least one, at
most `255 - data`. Hosts may raise this within the existing field: keyframes
and negotiated recovery frames now get at least 30% and four parity shards;
normal frames cover an observed burst plus two shards, at most their data count.
No datagram size changes. The block's data and parity shards together form a
systematic Reed–Solomon code over GF(2⁸) (polynomial 0x11D) with a
Cauchy generator matrix, as specified in `crates/proto/src/fec.rs`: any
`data` of the block's shards rebuild it. `shard_len` is at most 1 172.

The viewer delivers frames in frame-number order. A frame is complete when
every block has `data` shards. It is lost when a later frame completes
while it is still incomplete, or 100 ms after its first shard arrived,
whichever comes first. A later incomplete frame's 100 ms timeout also
declares any entirely missing earlier frames lost. After a lost frame the
viewer decodes nothing until a keyframe arrives, and asks for one; with
`reference_invalidation` it asks for a recovery frame first, and also
resumes at a recovery frame whose reference is the last frame it delivered.
A recovery frame that predicts from any other frame is treated as lost.

### Audio (3)

| Offset | Size | Field |
| --- | --- | --- |
| 8 | 4 | `seq` — packet number, +1 per packet |
| 12 | 4 | `pts` — capture time, host clock, µs, wraps |
| 16 | … | one encoded audio frame (`lpcm-rice`, see `crates/codec/src/audio.rs`) |

`flags` bit 0 marks a parity packet. After packets `4k … 4k+3` the host
sends a parity packet with `seq = 4k`: the XOR of the four packets'
`u16 length | payload`, each zero-padded to the longest. It rebuilds any
one lost packet of the four.

Each coded audio frame is at most `MAX_DATAGRAM - 18` (1 182) bytes,
leaving room for the 16-byte audio header and the parity packet's two-byte
length. The host picks `frame_samples` and announces it in `Accept`: 240
samples (5 ms at 48 kHz) for stereo, 80 for 5.1 and 60 for 7.1, the most
whose worst case fits. Stereo frames are `lpcm-rice` version 1, exactly as
5.1.0 sent them; more channels are version 2 (see the codec's module
documentation), which only a viewer that asked with `audio_channels`
receives. Channels are in WAVE order (5.1: FL FR FC LFE BL BR; 7.1 adds
SL SR).

### Input (4)

| Offset | Size | Field |
| --- | --- | --- |
| 8 | 4 | `first` — sequence number of the first event |
| 12 | 1 | `count` — events in this datagram |
| 13 | … | events `first, first+1, …` |

Events, each `u8 kind` then a fixed payload:

| Kind | Event | Payload |
| --- | --- | --- |
| 1 | relative mouse move | `i16 dx, i16 dy` |
| 2 | absolute mouse position | `u16 x, u16 y, u16 width, u16 height` — a point in a `width × height` picture |
| 3 | mouse button | `u8 button` (1 left, 2 middle, 3 right, 4 back, 5 forward), `u8 down` |
| 4 | key | `u16 vk` (Windows virtual-key code), `u8 down`, `u8 modifiers` (1 shift, 2 ctrl, 4 alt, 8 meta) |
| 5 | scroll | `i16 vertical, i16 horizontal` (120 per notch; up and right positive) |
| 6 | text | `u8 len` (1–64), UTF-8 bytes |
| 7 | release everything held | none |

Input is reliable without retransmission timers: the viewer numbers every
event and keeps it until the host acknowledges it. Each `Input` datagram
carries every unacknowledged event, oldest first, as many as fit. The
viewer sends one whenever it has a new event, and again every 20 ms while
any event is unacknowledged. The host applies events in sequence order,
ignores ones it has applied, and keeps a later event until the gap before
it is filled.

### InputAck (5)

`u32 next` at offset 8: every event before `next` has been applied. Sent at
once for every `Input` datagram.

### KeyframeRequest (6)

`u32 last` at offset 8: the last frame the reassembler delivered to decode. The viewer sends
it when a frame is lost (with `reference_invalidation`, once a recovery
frame is out of reach; see below), and again every 50 ms (or the measured round
trip, if longer) until a keyframe newer than `last` arrives. Every further
frame lost meanwhile (perhaps the keyframe that answered) sends a repeat at
once. Older viewers repeat every 100 ms only.
The host answers with an IDR frame; it encodes at most one requested keyframe per max(50 ms, smoothed RTT).
A repeat with the same `last` is a duplicate, and ignored, until a keyframe
has gone out since the request it repeats and a round trip (smoothed RTT
less its variation) has passed since then: the viewer still asking means
that keyframe was lost too. After 250 ms a repeat is answered regardless.
Older hosts hold every repeat for 250 ms.

At session start the viewer waits for the host's first keyframe without
requesting another one, unless a frame is declared lost.

### RecoveryRequest (14) and RecoveryAck (15)

Only after a `StreamRequest` and an `Accept` that both have
`reference_invalidation` true; an older host or viewer never sees these
kinds, and loss goes on being answered with an IDR.

`RecoveryRequest` (24 bytes): `u32 last_delivered`, `u32 first_invalid`,
`u32 last_invalid`, `u32 id` at offsets 8, 12, 16, 20. `last_delivered`
has `KeyframeRequest`'s meaning: the last frame the reassembler handed to
the decoder, after which it delivered nothing. The invalid range (inclusive,
wrapping) runs from the frame after it to the newest frame the viewer has
seen. `id` is +1 per loss the viewer asks about (never 0); a repeat of the
same request keeps its `id`.

`RecoveryAck` (16 bytes): `flags` is the status, `u32 id` at offset 8 and
`u32 frame` at 12:

| Status | Meaning |
| --- | --- |
| 0 pending | received; the encoder will try with its next frame |
| 1 sent | `frame` is the recovery frame (sent just before it) |
| 2 refused | the encoder cannot; an IDR follows, under the usual max(50 ms, RTT) gate |

The host answers a new `id` at once with pending, hands it to the encoder
before the next frame, and answers again with sent or refused. A repeated
`id` gets the latest answer again; an older one is ignored. The encoder
refuses when it no longer holds a picture the viewer has: the frame is
older than what it keeps, an IDR was made after it, or the encoder has no
reference control at all. It also refuses a base from an earlier capture/
pipeline generation, even if that old frame was still queued when the
pipeline reopened: generation accompanies the encoded frame through the
sender queue and the wire-frame-to-picture map.

The viewer sends a request as soon as a frame is lost, once more halfway to
its answer deadline, max(100 ms, 1.5 × smoothed RTT), and falls back to
`KeyframeRequest` when no answer comes by then, when the host refuses, when
a pending answer is followed by no recovery frame within 250 ms more, or
when three recovery frames in a row are lost too. Otherwise it waits for
the recovery frame, delivers it after complete FEC reassembly, and decodes
on from it. Frames between the loss and the recovery frame are dropped:
they predict from what was lost.

A recovery frame is decodable by a decoder that has decoded the reference
frame and nothing after it. Encoders make it as follows:

| Encoder | Recovery frame |
| --- | --- |
| Latch software H.264 | keeps the last 300 ms of pictures (at least 4, at most 256 MB) and rolls back to the reference: the next frame predicts from it with the `frame_num` that follows it, so to the decoder the lost frames never existed (bit-exact; tested against the decoder) |
| AMF HEVC (AMD, Windows) | four long-term references, one picture marked every fps/30 frames round robin; the next frame is forced to predict from the newest one at or before the reference (`HevcForceLTRReferenceBitfield`) |
| VideoToolbox (opt-in, `LATCH_MAC_LTR=1`) | on 8-bit H.264 High/HEVC sessions accepting `EnableLTR`, acknowledge the tokens of pictures the viewer decoded at or before the base and force an LTR refresh; an H.264 loss of pictures carrying tokens, or a session that stalls or misses its pace, takes an IDR ([macOS mode](MACOS.md)) |
| Media Foundation H.264 (opt-in, `LATCH_MF_LTR=1`) | four application-controlled LTR slots on ~33 ms and ~667 ms cadences, only after an encoder-open loss/decode self-test with periodic content passes; a loss of a slot's first picture takes an IDR; otherwise IDR |
| Media Foundation HEVC/AV1, VA-API, NVENC | refused: an IDR, as before |



`Ping` carries `u64 time` at offset 8, the sender's clock. The receiver
answers at once with a `Pong` carrying the same `time`. The viewer pings
twice a second, the host fifty times a second (the lowest round trip between
two viewer reports tells a filling queue from the path's base), and each
keeps a smoothed round trip and its variance. No clocks are compared
across machines.

### Gamepads (9–12)

Only after a `StreamRequest` with `gamepads` true and an `Accept` with
`gamepads` above 0; an older host never sees these kinds (and would drop
them as unknown). A pad is a state, not a stream of events: each datagram
carries whole snapshots, and the newest replaces every older one.

`Pad` (9): `u8 count` (1–16) at offset 8, then `count` snapshots, at most
one per slot:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | `slot`, 0–15, below the `Accept`'s `gamepads` |
| 1 | 1 | `kind`: 0 gone (unplugged), 1 Xbox, 2 PlayStation, 3 generic |
| 2 | 1 | `flags`: bit 0 motion follows, bit 1 touch follows; others 0 |
| 3 | 1 | reserved, 0 |
| 4 | 4 | `seq` — the slot's snapshot number, +1 per new snapshot, from 1 |
| 8 | 4 | `buttons`, by position on an Xbox-layout pad: bit 0 south (A, Cross), 1 east, 2 west, 3 north, 4 left shoulder, 5 right shoulder, 6 back (View, Share, Create), 7 start (Menu, Options), 8 guide (Xbox, PS), 9 left stick, 10 right stick, 11–14 d-pad up, down, left, right, 15 misc (Share, Mute, Capture), 16 touchpad click, 17–20 paddles 1–4 |
| 12 | 8 | `i16` left x, left y, right x, right y; right and up positive |
| 20 | 4 | `u16` left trigger, right trigger; 0 released, 65535 fully pulled |
| 24 | 12 | if motion: `i16` gyro x, y, z in 1/16 °/s and accelerometer x, y, z in 1/8192 g (x right, y up, z towards the player) |
| … | 10 | if touch: two fingers, each `u8` (bit 7 down, bits 0–6 finger id), `u16 x`, `u16 y` across the pad from the top left, 0–65535 |

The first snapshot of a slot with a kind plugs a pad in; one with kind 0
unplugs it. A host that cannot emulate `kind` emulates an Xbox pad. When
the session ends the host unplugs every pad.

`PadAck` (10) and `PadFeedbackAck` (12): `u8 count` (1–16), then `count`
× (`u8 slot`, `u32 seq`): the newest snapshot of that slot the sender has
applied. `PadAck` answers every `Pad` at once, for each slot in it.

`PadFeedback` (11): `u8 count` (1–16), then `count` × 20 bytes:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | `slot` |
| 1 | 1 | `flags`: bit 0 the light colour is set |
| 2 | 1 | `player`: the number the host's system gave the pad, 1–8; 0 unknown |
| 3 | 1 | reserved, 0 |
| 4 | 4 | `seq` — per slot, as in `Pad` |
| 8 | 8 | `u16` large (low-frequency) motor, small (high-frequency) motor, left trigger motor, right trigger motor; 0 off to 65535 |
| 16 | 3 | light bar red, green, blue |
| 19 | 1 | reserved, 0 |

Reliability is the same in both directions. The sender sends a new
snapshot at once and, while any slot's newest snapshot is unacknowledged,
sends the newest snapshot of every such slot again every 10 ms. The
receiver applies a snapshot only if its `seq` is newer than the last it
applied for that slot (serial-number order), so a late or repeated
datagram never moves a stick back or restarts a rumble that stopped. The
viewer answers every `PadFeedback` with a `PadFeedbackAck`.

### Mic (13)

The viewer's microphone, laid out exactly as `Audio` (3) with its own
`seq` and parity groups, sent only after an `Accept` with `mic`, in that
format (5 ms of 48 kHz mono). The host takes it only from the address the
session's `Hello` came from. A host that does not know kind 13 drops it.

### Probe (16)

Only to a viewer whose `StreamRequest` has `probe_trains`; an older viewer
never sees it (and would drop it as unknown). `aux` is the train number;
`u8 index` at offset 8 and `u8 count` at offset 9 place the datagram in
its train, and zero padding follows (the host sends 1 200-byte datagrams,
at least 10 bytes parse). The host sends a train back to back while video
is idle: after 500 ms of it, two trains 200 ms apart, then one a second,
each 6 to 16 datagrams, skipped below an 8 Mbps retained budget. The
viewer deduplicates indexes and accepts only ordered, contiguous prefixes
of at least four packets (a lost tail is allowed). It uses the median
eight-position packet pair, rather than total span. Pair dispersion must
be at least twice the measured pair timing variation; a dominant stall
is rejected. Equal timestamps allow up to 1 ms of receive batching noise; an entirely
batched train supplies only a high timing bound.
The fastest usable rate goes in `probe_kbps`; zero means unknown.

These datagrams bypass the video pacer and **are outside the wire budget**
and ordinary throughput counters (`received_kbps` counts no probes).
Including the 80-byte per-datagram tunnel allowance, each train bursts
7,680–20,480 B, up to ~330 kbps extra in the first idle second (two trains),
then ≤164 kbps. A 16-packet burst takes 32.8 ms at 5 Mbps in a 40 ms FIFO;
skipping low retained budgets reduces this risk, but cannot prevent a
burst overflowing a path that fell below the retained rate or a small
policer bucket. After every train an ordinary 16-byte Ping (96 modeled
wire bytes) independently times its queue. Its echo is kept separate from
the ordinary minimum RTT; it must show delay above the measured ping
jitter before a train alone can lower the target. Padding and these extra
pings add no new wire fields; old viewers receive neither train nor echo.

## Versioning

`protocol` in `StreamRequest` and the upgrade token's `/1` name the
version. A host that cannot speak the requested version answers 426 with
an `Ack` naming the versions it speaks. `/v1/status` reports
`streamer.protocol` (1 here; absent from Latch 4 hosts, which speak only
GameStream).

## Encoder limits (5.2, additive)

`/v1/status` → `streamer.codecs` lists what each codec's encoder takes on
that host, best first; it is empty from older hosts and until the host has
looked (shortly after it starts), and a viewer then assumes nothing:

| Field | Type | Meaning |
| --- | --- | --- |
| `codec` | string | wire name, as in `codecs` above |
| `max_side` | u32 | longest side of a picture, in pixels |
| `max_pixels` | u64 | largest picture, in pixels as coded: each side rounded up to 16 |
| `max_pixel_rate` | u64 | pixels a second (picture size × frame rate) |
| `max_kbps` | u32 | highest bitrate |

The values start from the codec's top level (H.264 6.2, HEVC Main 6.2
High tier, AV1 6.3 High tier: 8192 × 4352 pixels a picture, 4.28 billion
a second, 800 Mbps, H.264 High 1 Gbps) and are lowered where the
platform's encoder has a smaller limit it can be seen to have (macOS:
VideoToolbox is tried at a ladder of sizes until one encodes in hardware;
Linux: NVENC and VA-API say theirs).
A picture is counted as coded, in whole 16-pixel blocks
(`latch_proto::control::fit`, which host and viewer both use): an M3 Pro's
H.264 takes 4096 × 2160 and fails 3776 × 2352, fewer pixels but more
blocks.
`streamer.max_fps`, when not 0, is the software encoder's frame rate on
that CPU at `capture_size` (or 1920 × 1080), measured by the host rather
than guessed; a viewer asking for fewer pixels scales it up in proportion.
A Linux host whose hardware encodes some codecs reports 0 here and puts
the software rate in the other codecs' `max_pixel_rate` instead.
Until the measurement (taken when the host starts, about 1.5 s) ends, a
host encoding in software reports 30 fps at 1080p, and it holds every
request to what it reports, pixels a second, whatever the viewer asked:
the frame rate comes down first, to 30, then the picture in proportion.

## Additive transport feedback (5.0.x compatible)

Report retains its four original required fields. New JSON fields all have
serde defaults, so older endpoints ignore them and new hosts accept the
original four-field report. No datagram layout or protocol number changes;
the one new datagram kind, `Probe` (16), goes only to a viewer that asked
for it.

Viewer reports add `received_kbps` (received media rate), `frames_lost`
(unrecovered frames in the last measurement window), `unrecovered_loss_pct`
(optional; absence means legacy raw loss), `queue_ms` (decode queue age),
`loss_burst_packets` (longest data-shard erasure run, bounded to 32),
`probe_kbps` (fastest probe train, see [Probe](#probe-16); absent when none)
and `window_ms` (measurement duration). New measurements and reports use 50 ms
windows (older peers retain their 250 ms or one-second windows). Raw `loss_pct` includes FEC-recovered shards; recovery alone is
not congestion. RTT is milliseconds on the reporter's clock.

Hosts may reply using the existing Report type, adding
`requested_bitrate_kbps`, `applied_bitrate_kbps`, `pacing_queue_ms`,
`send_errors`, `send_retries`, `socket_send_bytes`, `socket_receive_bytes`,
`wire_budget_kbps` and `application_limited`. All have serde defaults.
`wire_budget_kbps` is the controller's wire budget, kept through idle;
the encoder's share of it shows in `applied_bitrate_kbps`.
Applied bitrate is the encoder target after wire overhead reservation;
requested bitrate is the session ceiling. Error/retry counts are cumulative.
Legacy viewers ignore host Reports as an unhandled existing message type.

Audio recovery derives PTS spacing from Accept's frame_samples/rate. The
host's 240-sample packets therefore advance by 5000 us at 48 kHz. Rice
encoding falls back to existing independent verbatim subframes when needed
to enforce the 3 + 2*(1 + 2*n) byte bound; no new audio format is introduced.

## Performance integration on the consumer branch

Protocol 1 and every UDP layout remain unchanged. Cursor support is opt-in:
`StreamRequest.cursor` and `Accept.cursor` default to false, so plain #32 and
5.0.1 peers keep their host-drawn cursor. Unknown control message types remain
ignored. Added report fields default on read; old JSON readers ignore them.

`engine::congestion` is the only bitrate controller. New report windows are
50 ms; old 250 ms / one-second reports remain valid. Host pings at 50 Hz supply
each window's lowest RTT. A smoothed increasing delay gradient or standing rise
above max(6 ms, base RTT / 10, 3× ping jitter) is pressure (idle senders retain
a 3 ms jitter allowance). Severe residual loss on a loaded sender, or damage without usable rate
feedback remains pressure; so does >10% loss with a delivery deficit sustained
for 500 ms on an underfilled one. Moderate queue-free damage first raises FEC, then
lowers the rate actually sent by 20% after 500 ms (only at 4 Mbps or more sent);
mean loss falling by half against its mean before the test (or by over
max(35%, 2 points) above a random-loss floor) with the mean actual load
falling at least 10% while filling the reduced budget confirms a policer
after a horizon plus one second.
Inconclusive tests restore the budget and wait three seconds.
Delay/collapse cuts stay immediate under the existing two/one-window gates;
one damaged legacy window of 500 ms or more cuts at once.
Clean recovery doubles the budget per 400 ms of actual time between reports
(up to 2 s apart) back to just under the last sustainable rate, then probes
above it in bounded, RTT-aware steps; it is capped below a policer's rate for
three seconds after severe/legacy loss without queueing, ten after a confirmed
rate-correlation test. The wire floor is 1 Mbps. Legacy
reports without `unrecovered_loss_pct` count raw loss above 10% (or any lost
frame) as damage.
The session starts at its ceiling, and clean paths bypass pacing. Recovered
random loss alone enables neither cuts nor pacing. Parity uses fast-attack,
two-second-decay raw loss with a base-RTT floor (15% at 40 ms, 20% at 100 ms),
capped at 30% before the per-frame packet floors above. The sender reserves
rounded parity, padding and headers inside its wire budget, including the
protected-frame floor while a keyframe or recovery frame is owed (never more
than half the ordinary target). Four reports
below 80% of the encoder's allowance mark it application-limited; under half
of it is idle. Idle keeps the budget and the encoder's target: motion resumes
at once, unpaced on a clean path. While idle the host sends probe trains to a
viewer that times them. Two recent confident trains below 3/4 of the
budget also need independent queue delay (including their trailing ping)
or lossy delivery deficit before lowering it to 90% of the faster train.
Without corroboration they only constrain pacing; unknown trains keep the
target and initially pace at 1/20 of it. Delivery filling that pace without
loss or delay validates 25% pace increases once per feedback horizon;
a fresh fast train removes the limit. Later fast trains restore a confirmed cut. An underfilled stream cut by a standing delay that
still delivered everything gets back its earlier budget once the delay has
been gone for a second; an idle one (or an underfilled one at the floor)
regrows by time a second after a cut, never past the last trains. While
pacing, the pacer's own local queue is not sender pressure. Data shards precede parity computation and transmission; a
completed frame is handed to decode immediately, including mid-receive-batch.
Protocol 1 datagrams remain at most 1200 bytes. No MTU or socket-offload
capability is advertised by this change.

## AV1 and decoder precision

`"av1"` is an additive codec name on protocol 1. Existing hosts skip unknown
offer names and choose a mutually supported codec; an AV1 Accept is only
sent to a viewer that offered AV1, and a host offers it only once a hardware
encoder has produced a trial keyframe. One AV1 frame carries a low-overhead
OBU temporal unit with sized OBUs (not length-prefixed Annex B). Keyframes
include the sequence header; the existing keyframe flag and recovery rules
apply; no UDP header or control message changes. The viewer supports Main
4:2:0, eight or ten bits, but AV1 streams negotiate 8-bit SDR only: Main10
and HDR10 are HEVC (see [Colour negotiation](#colour-negotiation)).

## Reference invalidation (additive)

Implemented as [RecoveryRequest and RecoveryAck](#recoveryrequest-14-and-recoveryack-15).
Measurements: [recovery frames against keyframes](measurements/2026-11-03/rfi/README.md).
The viewer test knobs `LATCH_TEST_VIDEO_LOSS=<percent>[:<burst>]` (drop
that share of video datagrams on arrival, in bursts) and
`LATCH_TEST_NO_RECOVERY=1` (do not ask for recovery frames) reproduce them.

## Colour negotiation

`StreamRequest.color` defaults to `{hdr:false, peak_nits:0, ten_bit:false}`.
HDR means the viewer can present HDR; peak_nits is display peak cd/m².
`Accept.color` carries H.273 CICP primaries, transfer, matrix, full_range,
bit_depth and optional ST 2086 / content-light metadata (`hdr`). Missing
colour means limited-range BT.709, 8-bit. Only HEVC negotiates 10-bit.
HDR requires ten_bit, hdr, a nonzero display peak and an HDR source.
HDR10 is Main10, limited-range BT.2020 NCL / ST 2084 PQ (9/16/9).
SDR Main10 keeps BT.709 (1/1/1). The P010 representation stores each
10-bit sample in bits 15:6; it does not change the negotiated range.
Mastering chromaticities are in 1/50000, luminance in 1/10000 cd/m²,
and MaxCLL/MaxFALL in cd/m². `chroma` is the 4:2:0 chroma siting, `left`
(H.273 type 0, also what a missing field means) or `center`; every current
host produces left-sited chroma. Live desktop HDR uses a canonical 1000-nit
BT.2020 container. Prefix SEI repeats the metadata at every HDR IDR.
Old peers ignore the new fields; missing capabilities never enable HDR.
