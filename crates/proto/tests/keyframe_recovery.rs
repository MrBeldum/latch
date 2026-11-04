//! Loss recovery through a bad network, deterministically: a 60 fps stream
//! whose frames and keyframe requests cross seeded impaired links (the
//! engine's `impaired` test's loss, bursts, duplicates, reordering and
//! jitter), with the viewer's [`KeyframeRequests`] and the host's
//! [`KeyframeAnswers`] deciding when to ask and when to answer. All time
//! is simulated. The picture may stall while a keyframe is on its way, but
//! not for longer than a few round trips: a recovery keyframe lost in a
//! burst must be asked for and sent again at once, not after the host's
//! duplicate hold.

use latch_proto::{
    sim::{Impairment, Link},
    video::{
        before, KeyframeAnswers, KeyframeRequests, OutFrame, Packetizer, Reassembled, Reassembler,
        DUPLICATE_REQUEST_HOLD,
    },
    wire::{self, Datagram},
};
use std::time::{Duration, Instant};

const BAD: Impairment = Impairment {
    loss: 0.03,
    burst_enter: 0.002,
    burst_exit: 0.3,
    duplicate: 0.01,
    reorder: 0.01,
    reorder_delay: Duration::from_millis(4),
    delay: Duration::from_millis(10),
    jitter: Duration::from_millis(4),
};
/// About the round trip [`BAD`] makes: twice its delay and mean jitter;
/// and less its variation, as the host judges repeats.
const RTT: Duration = Duration::from_millis(24);
const LOW_RTT: Duration = Duration::from_millis(22);
const FRAME: Duration = Duration::from_micros(16_667);
/// What the engine's test pattern encodes to at the 1 Mbps wire floor the
/// controller cuts to under this loss, with the parity share it reaches.
const KEYFRAME_LEN: usize = 5_600;
const FEC_PERCENT: u8 = 30;
/// The host's gate on requested keyframes: max(50 ms, RTT).
const KEYFRAME_GAP: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Rules {
    /// 5.1's: the viewer retries on a 100 ms timer only, and the host
    /// holds every repeat of a request for 250 ms.
    Held,
    Current,
}

/// The longest time between two frames delivered to decode, over `secs` of
/// a stream through [`BAD`] seeded with `seed`, and how many keyframes the
/// host sent.
fn longest_freeze(rules: Rules, seed: u64, secs: u64) -> (Duration, u32) {
    let start = Instant::now();
    let mut down = Link::new(BAD, seed);
    let mut up = Link::new(BAD, seed.wrapping_mul(31).wrapping_add(7));
    let mut packetizer = Packetizer::new(1);
    packetizer.set_fec_percent(FEC_PERCENT);
    let mut reassembler = Reassembler::new(1);
    let mut requests = KeyframeRequests::new();
    let mut answers = KeyframeAnswers::new();
    let mut held: Option<(u32, Instant)> = None;
    // The host's encoder: whether a keyframe is asked for, and whether the
    // frame being encoded (out at the next tick) is one.
    let mut asked = true;
    let mut encoding_key = false;
    let mut last_key: Option<Instant> = None;
    let mut next_frame = start;
    let mut next_pong = start;
    let mut random = seed | 1;
    let mut out = Vec::new();
    let mut delivered: Option<Instant> = None;
    let mut longest = Duration::ZERO;
    let mut keyframes = 0;
    let mut step = Duration::ZERO;
    while step < Duration::from_secs(secs) {
        let now = start + step;
        step += Duration::from_millis(1);
        if now >= next_frame {
            next_frame += FRAME;
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let len = if encoding_key {
                KEYFRAME_LEN
            } else {
                900 + (random % 900) as usize
            };
            let data = vec![(random >> 32) as u8; len];
            packetizer.packetize(
                &OutFrame {
                    data: &data,
                    keyframe: encoding_key,
                    host_delay: 0,
                },
                |datagram| down.send(now, datagram),
            );
            if encoding_key {
                keyframes += 1;
                answers.sent(now);
                last_key = Some(now);
            }
            // The next frame's encode starts now and reads the request.
            let due = last_key.is_none_or(|at| now - at >= KEYFRAME_GAP.max(RTT));
            encoding_key = asked && due;
            asked &= !encoding_key;
        }
        // The viewer answers the host's 50 Hz pings: the uplink is never
        // idle, so its bursts last about as long as the downlink's.
        if now >= next_pong {
            next_pong += Duration::from_millis(20);
            wire::write_pong(&mut out, 1, 0);
            up.send(now, &out);
        }
        for datagram in up.receive(now) {
            if let Ok(Datagram::KeyframeRequest { last, .. }) = wire::parse(&datagram) {
                asked |= match rules {
                    Rules::Current => answers.request(last, now, Some(LOW_RTT)),
                    Rules::Held => {
                        let wanted = held.is_none_or(|(n, at)| {
                            before(n, last) || (n == last && now - at >= DUPLICATE_REQUEST_HOLD)
                        });
                        if wanted {
                            held = Some((last, now));
                        }
                        wanted
                    }
                };
            }
        }
        for datagram in down.receive(now) {
            if let Ok(Datagram::Video { shard, .. }) = wire::parse(&datagram) {
                reassembler.push(&shard, now);
            }
        }
        while let Some(event) = reassembler.poll(now) {
            match event {
                Reassembled::Frame(_) => {
                    if let Some(previous) = delivered.replace(now) {
                        longest = longest.max(now - previous);
                    }
                }
                Reassembled::Lost { .. } => {
                    if rules == Rules::Current {
                        requests.lost();
                    }
                }
            }
        }
        // 5.1's viewer retried every max(100 ms, RTT).
        let retry_rtt = match rules {
            Rules::Current => RTT,
            Rules::Held => Duration::from_millis(100),
        };
        if requests.due(reassembler.needs_keyframe(start, now), now, Some(retry_rtt)) {
            let last = reassembler.last_delivered().unwrap_or(0);
            wire::write_keyframe_request(&mut out, 1, last);
            up.send(now, &out);
        }
    }
    (longest, keyframes)
}

#[test]
fn a_lost_recovery_keyframe_is_sent_again_within_a_few_round_trips() {
    let mut worst = Duration::ZERO;
    let mut worst_held = Duration::ZERO;
    let (mut keyframes, mut keyframes_held) = (0, 0);
    for seed in 1..=24 {
        let (freeze, sent) = longest_freeze(Rules::Current, seed, 60);
        let (held, sent_held) = longest_freeze(Rules::Held, seed, 60);
        eprintln!(
            "seed {seed:2}: longest freeze {:4} ms, {sent:3} keyframes \
             (5.1's rules: {:4} ms, {sent_held:3})",
            freeze.as_millis(),
            held.as_millis()
        );
        worst = worst.max(freeze);
        worst_held = worst_held.max(held);
        keyframes += sent;
        keyframes_held += sent_held;
    }
    // The network alone cannot be the reason: 5.1's rules froze the same
    // streams for longer.
    assert!(
        worst_held > Duration::from_millis(250),
        "the simulated network never lost a recovery keyframe: {worst_held:?}"
    );
    assert!(
        worst <= Duration::from_millis(250),
        "the picture froze for {worst:?} through a lossy network"
    );
    // Faster repeats must not turn into more keyframes than losses need:
    // only lost answers are answered again.
    assert!(
        keyframes * 10 <= keyframes_held * 11,
        "{keyframes} keyframes where 5.1's rules sent {keyframes_held}"
    );
}

#[test]
fn the_host_answers_a_repeat_only_once_its_keyframe_had_time_to_arrive() {
    let t = Instant::now();
    let ms = Duration::from_millis;
    let mut answers = KeyframeAnswers::new();
    assert!(answers.request(10, t, Some(ms(30))), "a first loss");
    assert!(!answers.request(10, t + ms(5), Some(ms(30))), "a duplicate");
    answers.sent(t + ms(20));
    // Sent before the keyframe could have arrived: still a duplicate.
    assert!(!answers.request(10, t + ms(40), Some(ms(30))));
    // A round trip after it went out, the viewer still has nothing newer:
    // the keyframe was lost.
    assert!(answers.request(10, t + ms(50), Some(ms(30))));
    assert!(!answers.request(10, t + ms(60), Some(ms(30))), "answered");
    // With no keyframe since, only the hold lets a repeat through.
    assert!(!answers.request(10, t + ms(250), Some(ms(30))));
    assert!(answers.request(10, t + ms(300), Some(ms(30))));
    // A newer loss is always news; an older request is stale.
    assert!(answers.request(11, t + ms(301), Some(ms(30))));
    assert!(!answers.request(10, t + ms(900), Some(ms(30))));
}

#[test]
fn the_viewer_asks_again_at_once_when_a_frame_is_lost() {
    let t = Instant::now();
    let ms = Duration::from_millis;
    let rtt = Some(ms(30));
    let mut requests = KeyframeRequests::new();
    assert!(requests.due(true, t, rtt));
    assert!(!requests.due(true, t + ms(20), rtt));
    // Perhaps the keyframe that answered: the host decides.
    requests.lost();
    assert!(requests.due(true, t + ms(25), rtt));
    // Once: a loss is not saved up for later.
    assert!(!requests.due(true, t + ms(40), rtt));
    assert!(!requests.due(true, t + ms(70), rtt));
    assert!(requests.due(true, t + ms(75), rtt), "the timer still runs");
    // Never more often than the round trip, however long.
    let far = Some(ms(200));
    assert!(!requests.due(true, t + ms(250), far));
    assert!(requests.due(true, t + ms(275), far));
    requests.lost();
    assert!(!requests.due(false, t + ms(276), rtt), "nothing to ask for");
    assert!(
        requests.due(true, t + ms(277), rtt),
        "a new loss asks at once"
    );
}
