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
| `bitrate_kbps` | u32 | video bitrate wanted |
| `codecs` | [string] | what the viewer decodes, best first: `"hevc"`, `"h264-high"`, `"h264-baseline"` |
| `udp_port` | u16 | the viewer's media socket port |
| `audio` | bool | send desktop audio |
| `name` | string | the viewer's machine name, for the host's log |
| `cursor` | bool | optional, default false; viewer can draw pointer locally and handle shape updates |
| `audio_channels` | u32 | optional; most channels the viewer plays (6 for 5.1, 8 for 7.1). Absent, or below 3, means stereo |
| `mic` | bool | optional, default false; the viewer offers its microphone (datagram kind 9) |

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
| 5 | `Report` | either | `{"fps", "loss_pct", "decode_ms", "rtt_ms"}`, every 250 ms (legacy viewers: once a second), for bitrate and FEC feedback |
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
| 9 | `Mic` | viewer → host |

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
length. The host picks `frame_samples` and announces it in `Accept`: 240
samples (5 ms at 48 kHz) for stereo, 80 for 5.1 and 60 for 7.1, the most
whose worst case fits. Stereo frames are `lpcm-rice` version 1, exactly as
5.1.0 sent them; more channels are version 2 (see the codec's module
documentation), which only a viewer that asked with `audio_channels`
receives. Channels are in WAVE order (5.1: FL FR FC LFE BL BR; 7.1 adds
SL SR).

### Mic (9)

The viewer's microphone, laid out exactly as `Audio` (3) with its own
`seq` and parity groups, sent only after an `Accept` with `mic`, in that
format (5 ms of 48 kHz mono). The host takes it only from the address the
session's `Hello` came from. A host that does not know kind 9 drops it.

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

`engine::congestion` is the only bitrate controller. Report windows are 250 ms
(new viewers) or one second (legacy). Host ping samples at 10 Hz supply each
window's lowest RTT; a standing rise above max(10 ms, base RTT / 4) is pressure.
Budget cuts account for actual wire throughput. Parity uses raw loss with a
base-RTT floor (15% at 40 ms, 20% at 100 ms), capped at 30%. Data shards for the
whole frame precede parity computation and transmission. Clean paths bypass
send pacing; loss/queue pressure enables the bounded wire-byte token bucket.
See [ENGINE.md](ENGINE.md#decisions) for the retained consumer controller,
transport telemetry, audio latency bounds and platform pipeline choices.
