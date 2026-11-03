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
| `codecs` | [string] | what the viewer decodes, best first: `"av1"`, `"hevc"`, `"h264-high"`, `"h264-baseline"`; a name the host does not know is skipped |
| `udp_port` | u16 | the viewer's media socket port |
| `audio` | bool | send desktop audio |
| `name` | string | the viewer's machine name, for the host's log |
| `cursor` | bool | optional, default false; viewer can draw pointer locally and handle shape updates |

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
| 2 | `End` | either | `{"code": u32, "message": string}`; the sender closes after it |
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
reference can start from it. `aux`: the host's capture-to-send time for this
frame in units of 0.1 ms (saturating).

| Offset | Size | Field |
| --- | --- | --- |
| 8 | 4 | `frame` — frame number, +1 per encoded frame, wraps |
| 12 | 4 | `frame_len` — bytes in the whole encoded frame (Annex B) |
| 16 | 2 | `shard` — index of this shard in its block: data shards first, then parity |
| 18 | 1 | `block` — index of the FEC block |
| 19 | 1 | `blocks` — FEC blocks in the frame |
| 20 | 2 | `data` — data shards in this block |
| 22 | 2 | `parity` — parity shards in this block |
| 24 | 2 | `shard_len` — bytes per shard in this frame |
| 26 | 2 | reserved, 0 |
| 28 | `shard_len` | shard bytes |

The host cuts the frame into `D = ceil(frame_len / shard_len)` data shards,
the last one padded with zeros, and splits them into
`blocks = ceil(D / 64)` consecutive blocks of nearly equal size (the first
`D mod blocks` blocks get one shard more). Each block gets
`parity = ceil(data * fec_percent / 100)` parity shards, at least one, at
most `255 - data`. The block's data and parity shards together form a
systematic Reed–Solomon code over GF(2⁸) (polynomial 0x11D) with a
Cauchy generator matrix, as specified in `crates/proto/src/fec.rs`: any
`data` of the block's shards rebuild it. `shard_len` is at most 1 172.

The viewer delivers frames in frame-number order. A frame is complete when
every block has `data` shards. It is lost when a later frame completes
while it is still incomplete, or 100 ms after its first shard arrived,
whichever comes first. A later incomplete frame's 100 ms timeout also
declares any entirely missing earlier frames lost. After a lost frame the
viewer decodes nothing until a keyframe arrives, and asks for one.

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
length. The host picks `frame_samples` and announces it in `Accept`;
the default is 240 samples (5 ms at 48 kHz).

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
it when a frame is lost, and again every 100 ms (or the measured round
trip, if longer) until a keyframe newer than `last` arrives. The host
answers with an IDR frame; it encodes at most one requested keyframe per max(50 ms, smoothed RTT).
Duplicate requests with the same `last` are additionally throttled for 250 ms.

At session start the viewer waits for the host's first keyframe without
requesting another one, unless a frame is declared lost.

### Ping (7) and Pong (8)

`Ping` carries `u64 time` at offset 8, the sender's clock. The receiver
answers at once with a `Pong` carrying the same `time`. The viewer pings
twice a second, the host fifty times a second (the lowest round trip between
two viewer reports tells a filling queue from the path's base), and each
keeps a smoothed round trip and its variance. No clocks are compared
across machines.

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
VideoToolbox is tried at a ladder of sizes until one encodes in hardware).
A picture is counted as coded, in whole 16-pixel blocks
(`latch_proto::control::fit`, which host and viewer both use): an M3 Pro's
H.264 takes 4096 × 2160 and fails 3776 × 2352, fewer pixels but more
blocks.
`streamer.max_fps`, when not 0, is the software encoder's frame rate on
that CPU at `capture_size` (or 1920 × 1080), measured by the host rather
than guessed; a viewer asking for fewer pixels scales it up in proportion.
Until the measurement (taken when the host starts, about 1.5 s) ends, a
host encoding in software reports 30 fps at 1080p, and it holds every
request to what it reports, pixels a second, whatever the viewer asked:
the frame rate comes down first, to 30, then the picture in proportion.

## Additive transport feedback (5.0.x compatible)

Report retains its four original required fields. New JSON fields all have
serde defaults, so older endpoints ignore them and new hosts accept the
original four-field report. No datagram layout or protocol number changes.

Viewer reports add `received_kbps` (received media rate), `frames_lost`
(unrecovered frames in the last measurement window), `unrecovered_loss_pct`
(optional; absence means legacy raw loss), `queue_ms` (decode queue age),
and `window_ms` (measurement duration). New measurements and reports use 50 ms
windows (older peers retain their 250 ms or one-second windows). Raw `loss_pct` includes FEC-recovered shards; recovery alone is
not congestion. RTT is milliseconds on the reporter's clock.

Hosts may reply using the existing Report type, adding
`requested_bitrate_kbps`, `applied_bitrate_kbps`, `pacing_queue_ms`,
`send_errors`, `send_retries`, `socket_send_bytes`, `socket_receive_bytes`.
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
above max(6 ms, base RTT / 10, 3× ping jitter) is pressure, as are uncorrected
loss and lost frames at any raw loss. Two windows trigger a budget cut; one
does when `received_kbps` is under half the host's send rate while loss is
above 10% or delay above 20 ms (reports without `received_kbps` never do).
Clean recovery doubles the budget per 400 ms of actual time between reports
(up to 2 s apart) back to just under the last sustainable rate, then probes
above it in bounded, RTT-aware steps; it is capped below a policer's rate for
three seconds after loss without queueing. The wire floor is 1 Mbps. Legacy
reports without `unrecovered_loss_pct` count raw loss above 10% (or any lost
frame) as damage.
The session starts at its ceiling, and clean paths bypass pacing. Recovered
random loss alone enables neither cuts nor pacing. Parity uses fast-attack,
two-second-decay raw loss with a base-RTT floor (15% at 40 ms, 20% at 100 ms),
capped at 30%. Data shards precede parity computation and transmission; a
completed frame is handed to decode immediately, including mid-receive-batch.
Protocol 1 datagrams remain at most 1200 bytes. No MTU or socket-offload
capability is advertised by this change.

## Proposal: reference-frame invalidation (not implemented)

Negotiate `reference_invalidation: true` in both StreamRequest and Accept,
with serde default false. Only send the proposed message when both endpoints
advertise it; protocol 1 peers without the capability keep KeyframeRequest.
Reserve a future UDP kind after checking the then-current registry. Its payload
should contain `u32 last_delivered`, `u32 first_invalid`, `u32 last_invalid`
(inclusive wrapping frame numbers), and `u32 request_id`. Numbers are those
already used by Video shards; `last_delivered` has exactly KeyframeRequest's
semantics, never "the missing frame" or "last received shard".

On loss, the viewer stops reference-dependent delivery and repeats the same
request_id at max(100 ms, RTT). The host invalidates the indicated references
only when its encoder can retain a known-good reference at/before last_delivered;
it acknowledges request_id and the first safe recovery frame. The viewer resumes
only at that frame, after complete FEC reassembly, and discards obsolete replies
using wrapping sequence comparisons. Duplicate requests are idempotent. If no
safe reference remains, either endpoint falls back to the existing IDR request,
with the same max(50 ms, RTT) host gate. Never send an unnegotiated new kind.

This needs encoder reference bookkeeping, recovery-frame marking, and a loss
matrix test with an actual supporting encoder. No message plumbing is shipped
without those semantics: an acknowledgment must not promise a recoverable frame
that the encoder cannot produce.
