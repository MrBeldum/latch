use latch_proto::{
    audio::{AudioOut, AudioPacketizer, AudioReceiver, AUDIO_HOLD},
    input::{Event, InputReceiver, InputSender, MAX_UNACKED, RESEND},
    sim::{Impairment, Link},
    video::{KeyframeRequests, OutFrame, Packetizer, Reassembled, Reassembler, FRAME_TIMEOUT},
    wire::{self, AudioPacket, Datagram, WireError},
    MAX_DATAGRAM,
};
use std::time::{Duration, Instant};

fn position(x: u16) -> Event {
    Event::MousePosition {
        x,
        y: 0,
        width: 1920,
        height: 1080,
    }
}

fn drain_input(sender: &mut InputSender, now: Instant) -> Vec<Event> {
    let mut link = Link::new(Impairment::NONE, 1);
    let mut receiver = InputReceiver::new();
    let mut applied = Vec::new();
    let mut out = Vec::new();
    for step in 0..1000 {
        let now = now + RESEND * step;
        if sender.poll(now, &mut out) {
            link.send(now, &out);
        }
        for dg in link.receive(now) {
            let Datagram::Input { first, events, .. } = wire::parse(&dg).unwrap() else {
                panic!()
            };
            let next = receiver.receive(first, events.iter(), |event| applied.push(event));
            sender.ack(next);
        }
        if sender.unacked() == 0 {
            return applied;
        }
    }
    panic!("input did not recover: {} unacknowledged", sender.unacked());
}

#[test]
fn input_overflow_preserves_oldest_and_critical_events() {
    let mut sender = InputSender::new(1);
    let start = Instant::now();
    let mut blackout = Link::new(
        Impairment {
            loss: 1.0,
            ..Impairment::NONE
        },
        1,
    );
    let mut out = Vec::new();
    for i in 0..1034 {
        let now = start + Duration::from_millis(i * 8);
        sender.push(position(i as u16));
        assert!(sender.poll(now, &mut out));
        blackout.send(now, &out);
    }
    assert!(blackout.receive(start + Duration::from_secs(9)).is_empty());
    let critical = [
        Event::Key {
            vk: 65,
            down: true,
            modifiers: 0,
        },
        Event::MouseButton {
            button: 1,
            down: false,
        },
        Event::Text("hello".into()),
        Event::ReleaseAll,
    ];
    for event in &critical {
        sender.push(event.clone());
    }
    assert_eq!(sender.unacked(), 1034 + critical.len());
    let applied = drain_input(&mut sender, start + Duration::from_secs(9));
    assert_eq!(
        &applied[..1034],
        &(0..1034).map(position).collect::<Vec<_>>()
    );
    assert_eq!(&applied[1034..], &critical);
}

#[test]
fn input_full_queue_preserves_relative_motion_without_saturation() {
    let mut sender = InputSender::new(1);
    for _ in 0..MAX_UNACKED - 1 {
        sender.push(Event::ReleaseAll);
    }
    sender.push(Event::MouseMove {
        dx: 30_000,
        dy: -30_000,
    });
    sender.push(Event::MouseMove {
        dx: 10_000,
        dy: -10_000,
    });
    let applied = drain_input(&mut sender, Instant::now());
    assert_eq!(applied.len(), MAX_UNACKED + 1);
    assert_eq!(
        applied.last(),
        Some(&Event::MouseMove {
            dx: 10_000,
            dy: -10_000
        })
    );
}

#[test]
fn input_backlog_never_discards_critical_events_or_creates_sequence_gaps() {
    let mut sender = InputSender::new(1);
    for _ in 0..65_536 + 1 {
        sender.push(Event::ReleaseAll);
    }
    assert_eq!(sender.unacked(), 65_537);
    assert_eq!(drain_input(&mut sender, Instant::now()).len(), 65_537);
    sender.push(position(1));
    let mut out = Vec::new();
    assert!(sender.poll(Instant::now(), &mut out));
    let Datagram::Input { first, .. } = wire::parse(&out).unwrap() else {
        panic!()
    };
    assert_eq!(first, 65_537);
}

#[test]
fn input_receiver_buffers_the_hard_cap_window() {
    let mut receiver = InputReceiver::new();
    let mut applied = Vec::new();
    assert_eq!(
        receiver.receive(65_535, [position(42)], |e| applied.push(e)),
        0
    );
    assert_eq!(
        receiver.receive(0, (0..65_535).map(|_| Event::ReleaseAll), |e| applied
            .push(e)),
        65_536
    );
    assert_eq!(applied.last(), Some(&position(42)));
}

#[test]
fn input_continuous_motion_retransmits_oldest_after_loss() {
    let mut sender = InputSender::new(1);
    let mut receiver = InputReceiver::new();
    let mut link = Link::new(Impairment::NONE, 1);
    let start = Instant::now();
    let mut applied = Vec::new();
    let mut out = Vec::new();
    for i in 0..300 {
        let now = start + Duration::from_millis(i * 10);
        sender.push(position(i as u16));
        assert!(sender.poll(now, &mut out));
        if i != 1 {
            link.send(now, &out);
        }
        for dg in link.receive(now) {
            let Datagram::Input { first, events, .. } = wire::parse(&dg).unwrap() else {
                panic!()
            };
            sender.ack(receiver.receive(first, events.iter(), |e| applied.push(e)));
        }
    }
    assert_eq!(applied, (0..300).map(position).collect::<Vec<_>>());
    assert_eq!(sender.unacked(), 0);
}

#[test]
fn input_backlog_waits_for_ack_or_resend_without_spinning() {
    let mut sender = InputSender::new(1);
    for i in 0..500 {
        sender.push(position(i));
    }
    let now = Instant::now();
    let mut out = Vec::new();
    assert!(sender.poll(now, &mut out));
    let Datagram::Input { first, events, .. } = wire::parse(&out).unwrap() else {
        panic!()
    };
    assert_eq!(first, 0);
    let count = events.iter().count() as u32;
    assert!(out.len() + position(0).wire_len() > MAX_DATAGRAM);
    assert!(!sender.poll(now, &mut out));
    assert_eq!(sender.next_deadline(), Some(now + RESEND));
    assert!(sender.poll(now + RESEND, &mut out));
    let Datagram::Input { first, .. } = wire::parse(&out).unwrap() else {
        panic!()
    };
    assert_eq!(first, 0);
    sender.ack(count);
    assert!(sender.poll(now + RESEND, &mut out));
    let Datagram::Input { first, .. } = wire::parse(&out).unwrap() else {
        panic!()
    };
    assert_eq!(first, count);
    assert!(!sender.poll(now + RESEND, &mut out));
}

#[test]
fn audio_packetizer_limits_data_and_parity_without_consuming_rejected_sequence() {
    let now = Instant::now();
    let mut link = Link::new(Impairment::NONE, 1);
    let mut packetizer = AudioPacketizer::new(1);
    let payload = vec![0xaa; MAX_DATAGRAM - 18];
    for seq in 0..4 {
        assert!(!packetizer.packetize(0, &vec![0; MAX_DATAGRAM - 17], |_| {
            panic!("oversized audio emitted")
        }));
        assert!(packetizer.packetize(seq * 5000, &payload, |dg| link.send(now, dg)));
    }
    let datagrams = link.receive(now);
    assert_eq!(datagrams.len(), 5);
    for (i, dg) in datagrams.iter().enumerate() {
        assert!(dg.len() <= MAX_DATAGRAM);
        let Datagram::Audio { packet, .. } = wire::parse(dg).unwrap() else {
            panic!()
        };
        assert_eq!(packet.seq, if i == 4 { 0 } else { i as u32 });
        assert_eq!(packet.parity, i == 4);
    }
    assert_eq!(datagrams[4].len(), MAX_DATAGRAM);
}

#[test]
fn wire_rejects_oversized_datagrams_of_every_kind() {
    for kind in 1..=8 {
        let mut dg = vec![0; MAX_DATAGRAM + 1];
        dg[0] = kind;
        assert!(
            matches!(wire::parse(&dg), Err(WireError::Length)),
            "kind {kind}"
        );
    }
}

#[test]
fn audio_recovers_after_long_blackout_with_data_or_parity_first() {
    for parity_first in [false, true] {
        let mut link = Link::new(Impairment::NONE, 1);
        let mut packetizer = AudioPacketizer::new(1);
        let mut receiver = AudioReceiver::new(1);
        let start = Instant::now();
        let mut delivered = Vec::new();
        for seq in 0..400 {
            let now = start + Duration::from_millis(seq as u64 * 5);
            packetizer.packetize(seq * 5000, &[seq as u8], |dg| {
                let Datagram::Audio { packet, .. } = wire::parse(dg).unwrap() else {
                    panic!()
                };
                // A 650 ms outage. In one case the first surviving group has parity only.
                if !(40..170).contains(&seq)
                    && !(parity_first && (170..172).contains(&seq) && !packet.parity)
                {
                    link.send(now, dg);
                }
            });
            for dg in link.receive(now) {
                let Datagram::Audio { packet, .. } = wire::parse(&dg).unwrap() else {
                    panic!()
                };
                receiver.push(&packet, now);
                while let Some(out) = receiver.pop(now) {
                    if let AudioOut::Frame { seq, payload, .. } = out {
                        assert_eq!(payload, [seq as u8]);
                        delivered.push(seq);
                    }
                }
            }
        }
        assert_eq!(delivered.last(), Some(&399), "parity first: {parity_first}");
        assert!(delivered.contains(&172));
    }
}

#[test]
fn audio_resynchronizes_across_sequence_wrap() {
    let now = Instant::now();
    let mut receiver = AudioReceiver::new(1);
    let mut link = Link::new(Impairment::NONE, 1);
    let mut dg = Vec::new();
    for seq in [u32::MAX - 79, 1, 4, 5] {
        wire::write_audio(
            &mut dg,
            1,
            &AudioPacket {
                parity: false,
                seq,
                pts: 0,
                payload: &[42],
            },
        );
        link.send(now, &dg);
        for dg in link.receive(now) {
            let Datagram::Audio { packet, .. } = wire::parse(&dg).unwrap() else {
                panic!()
            };
            receiver.push(&packet, now);
        }
        let mut output = None;
        while let Some(out) = receiver.pop(now) {
            output = Some(out);
        }
        if seq == 5 {
            assert!(matches!(output, Some(AudioOut::Frame { seq: 5, .. })));
        }
    }
}

fn receive_video(link: &mut Link, receiver: &mut Reassembler, now: Instant) {
    for dg in link.receive(now) {
        let Datagram::Video { shard, .. } = wire::parse(&dg).unwrap() else {
            panic!()
        };
        receiver.push(&shard, now);
    }
}

#[test]
fn video_timeout_advances_past_head_with_no_shards() {
    let now = Instant::now();
    let mut link = Link::new(Impairment::NONE, 1);
    let mut sender = Packetizer::new(1);
    let mut receiver = Reassembler::new(1);
    sender.packetize(
        &OutFrame {
            data: &[1],
            keyframe: true,
            host_delay: 0,
        },
        |dg| link.send(now, dg),
    );
    receive_video(&mut link, &mut receiver, now);
    assert!(matches!(receiver.poll(now), Some(Reassembled::Frame(_))));
    sender.packetize(
        &OutFrame {
            data: &[2],
            keyframe: false,
            host_delay: 0,
        },
        |_| {},
    );
    let arrival = now + Duration::from_millis(10);
    let mut sent = false;
    sender.packetize(
        &OutFrame {
            data: &[3; 3000],
            keyframe: false,
            host_delay: 0,
        },
        |dg| {
            if !sent {
                link.send(arrival, dg);
                sent = true;
            }
        },
    );
    receive_video(&mut link, &mut receiver, arrival);
    let deadline = arrival + FRAME_TIMEOUT;
    assert_eq!(receiver.next_deadline(), Some(deadline));
    assert!(receiver.poll(deadline - Duration::from_millis(1)).is_none());
    assert_eq!(
        receiver.poll(deadline),
        Some(Reassembled::Lost { frame: 1 })
    );
    assert_eq!(
        receiver.poll(deadline),
        Some(Reassembled::Lost { frame: 2 })
    );
    assert!(receiver.poll(deadline).is_none());
    assert_eq!(receiver.next_deadline(), None);
}

#[test]
fn video_initial_keyframe_requests_start_only_after_loss() {
    let now = Instant::now();
    for lose_initial in [false, true] {
        let mut link = Link::new(Impairment::NONE, 1);
        let mut sender = Packetizer::new(1);
        let mut receiver = Reassembler::new(1);
        let mut requests = KeyframeRequests::new();
        assert!(!requests.due(receiver.waiting_for_keyframe(), now, None));
        let mut shards = Vec::new();
        sender.packetize(
            &OutFrame {
                data: &[1; 3000],
                keyframe: true,
                host_delay: 0,
            },
            |dg| shards.push(dg.to_vec()),
        );
        link.send(now, &shards[0]);
        receive_video(&mut link, &mut receiver, now);
        assert!(receiver.poll(now).is_none());
        assert!(!requests.due(receiver.waiting_for_keyframe(), now, None));
        if lose_initial {
            let deadline = now + FRAME_TIMEOUT;
            assert_eq!(
                receiver.poll(deadline),
                Some(Reassembled::Lost { frame: 0 })
            );
            assert!(requests.due(receiver.waiting_for_keyframe(), deadline, None));
            assert!(!requests.due(receiver.waiting_for_keyframe(), deadline, None));
            assert!(requests.due(
                receiver.waiting_for_keyframe(),
                deadline + Duration::from_millis(100),
                None
            ));
        } else {
            for dg in &shards[1..] {
                link.send(now, dg);
            }
            receive_video(&mut link, &mut receiver, now);
            assert!(matches!(receiver.poll(now), Some(Reassembled::Frame(_))));
            assert!(!requests.due(receiver.waiting_for_keyframe(), now, None));
        }
    }
}

#[test]
fn input_lost_ack_does_not_mutate_already_applied_motion() {
    let mut sender = InputSender::new(1);
    let mut receiver = InputReceiver::new();
    let mut out = Vec::new();
    let now = Instant::now();
    let mut total = 0i64;
    for i in 0..10_000 {
        sender.push(Event::MouseMove { dx: 1, dy: 0 });
        // Polling marks the oldest batch in flight, even when the ACK is lost.
        assert!(sender.poll(now + Duration::from_millis(i), &mut out));
        let Datagram::Input { first, events, .. } = wire::parse(&out).unwrap() else {
            panic!()
        };
        receiver.receive(first, events.iter(), |e| {
            if let Event::MouseMove { dx, .. } = e {
                total += i64::from(dx);
            }
        });
    }
    // Once acknowledgements resume all 1000 Hz motion must arrive exactly once.
    sender.ack(receiver.next());
    for step in 0..1000 {
        if sender.unacked() == 0 {
            break;
        }
        assert!(sender.poll(now + Duration::from_secs(10) + RESEND * step, &mut out));
        let Datagram::Input { first, events, .. } = wire::parse(&out).unwrap() else {
            panic!()
        };
        sender.ack(receiver.receive(first, events.iter(), |e| {
            if let Event::MouseMove { dx, .. } = e {
                total += i64::from(dx);
            }
        }));
    }
    assert_eq!(sender.unacked(), 0);
    assert_eq!(total, 10_000);
}

#[test]
fn input_full_queue_never_changes_sent_payload_or_moves_across_keys() {
    let mut sender = InputSender::new(1);
    let now = Instant::now();
    let mut out = Vec::new();
    sender.push(Event::MouseMove { dx: 1, dy: 0 });
    sender.poll(now, &mut out);
    let original = out.clone();
    for _ in 1..MAX_UNACKED {
        sender.push(Event::ReleaseAll);
    }
    sender.push(Event::MouseMove { dx: 2, dy: 0 });
    sender.poll(now, &mut out);
    let Datagram::Input { events, .. } = wire::parse(&original).unwrap() else {
        panic!()
    };
    let first = events.iter().next().unwrap();
    let Datagram::Input { events, .. } = wire::parse(&out).unwrap() else {
        panic!()
    };
    assert_eq!(events.iter().next().unwrap(), first);
    let applied = drain_input(&mut sender, now);
    assert_eq!(applied.last(), Some(&Event::MouseMove { dx: 2, dy: 0 }));
}

#[test]
fn another_frames_timeout_leaves_an_assembling_frame_alone() {
    let start = Instant::now();
    let mut link = Link::new(Impairment::NONE, 1);
    let mut sender = Packetizer::new(1);
    let mut receiver = Reassembler::new(1);
    let mut frame = |data: &[u8], keyframe| {
        let mut shards = Vec::new();
        sender.packetize(
            &OutFrame {
                data,
                keyframe,
                host_delay: 0,
            },
            |dg| shards.push(dg.to_vec()),
        );
        shards
    };
    for dg in frame(&[1; 2400], true) {
        link.send(start, &dg);
    }
    receive_video(&mut link, &mut receiver, start);
    assert!(matches!(receiver.poll(start), Some(Reassembled::Frame(_))));
    let one = frame(&[2; 3000], false);
    let _two = frame(&[3; 3000], false);
    let three = frame(&[4; 3000], false);
    // Frame 3 starts at 0 and loses the rest; frame 1 starts 90 ms later.
    link.send(start, &three[0]);
    receive_video(&mut link, &mut receiver, start);
    let at = start + Duration::from_millis(90);
    link.send(at, &one[0]);
    receive_video(&mut link, &mut receiver, at);
    // Frame 3's timeout is not frame 1's: frame 1 has had 10 ms.
    let other = start + FRAME_TIMEOUT;
    assert_eq!(receiver.poll(other), None);
    assert_eq!(receiver.next_deadline(), Some(at + FRAME_TIMEOUT));
    // The rest of frame 1 arrives and it is delivered, not dropped as late.
    let late = other + Duration::from_millis(5);
    for dg in &one[1..] {
        link.send(late, dg);
    }
    receive_video(&mut link, &mut receiver, late);
    assert!(matches!(
        receiver.poll(late),
        Some(Reassembled::Frame(f)) if f.frame == 1 && f.data == [2; 3000]
    ));
    // Frame 2 never came; frame 3's timeout now decides both.
    assert_eq!(receiver.poll(late), Some(Reassembled::Lost { frame: 2 }));
    assert_eq!(receiver.poll(late), Some(Reassembled::Lost { frame: 3 }));
}

/// One group of four audio packets with payloads `[0]`..`[3]`, then its
/// parity: the datagrams in the order the host sends them.
fn audio_group() -> Vec<Vec<u8>> {
    let mut packetizer = AudioPacketizer::new(1);
    let mut out = Vec::new();
    for seq in 0..4u32 {
        packetizer.packetize(seq * 5000, &[seq as u8], |dg| out.push(dg.to_vec()));
    }
    out
}

/// Pushes each datagram and pops until nothing is ready, as the viewer
/// does; returns the payloads played and the packets given up on.
fn deliver_audio(
    receiver: &mut AudioReceiver,
    order: &[&[u8]],
    now: Instant,
) -> (Vec<Vec<u8>>, Vec<u32>) {
    let (mut played, mut missing) = (Vec::new(), Vec::new());
    for dg in order {
        let Datagram::Audio { packet, .. } = wire::parse(dg).unwrap() else {
            panic!()
        };
        receiver.push(&packet, now);
        while let Some(out) = receiver.pop(now) {
            match out {
                AudioOut::Frame { payload, .. } => played.push(payload),
                AudioOut::Missing { seq } => missing.push(seq),
            }
        }
    }
    (played, missing)
}

#[test]
fn reordered_audio_parity_does_not_discard_data_that_arrives_after_it() {
    let g = audio_group();
    let (data, parity) = (&g[..4], &g[4]);
    let now = Instant::now();
    let all: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i]).collect();
    for order in [
        vec![&data[0][..], &data[1], parity, &data[2], &data[3]],
        vec![&parity[..], &data[0], &data[1], &data[2], &data[3]],
    ] {
        let mut receiver = AudioReceiver::new(1);
        assert_eq!(
            deliver_audio(&mut receiver, &order, now),
            (all.clone(), vec![])
        );
    }
    // One packet really lost: parity still rebuilds it.
    let mut receiver = AudioReceiver::new(1);
    let order = [&data[0][..], &data[1], &data[2], parity];
    assert_eq!(deliver_audio(&mut receiver, &order, now), (all, vec![]));
}

#[test]
fn audio_after_a_lost_packet_is_released_when_the_host_goes_quiet() {
    let g = audio_group();
    let start = Instant::now();
    let mut receiver = AudioReceiver::new(1);
    // Packet 2 and the parity are lost, then the desktop falls silent.
    let order = [&g[0][..], &g[1], &g[3]];
    let (played, missing) = deliver_audio(&mut receiver, &order, start);
    assert_eq!((played, missing), (vec![vec![0], vec![1]], vec![]));
    assert_eq!(receiver.next_deadline(), Some(start + AUDIO_HOLD));
    let later = start + AUDIO_HOLD;
    assert_eq!(receiver.pop(later), Some(AudioOut::Missing { seq: 2 }));
    assert!(matches!(
        receiver.pop(later),
        Some(AudioOut::Frame { seq: 3, .. })
    ));
    assert_eq!(receiver.pop(later), None);
    // Nothing is held now, so silence produces no concealment.
    assert_eq!(receiver.next_deadline(), None);
    assert_eq!(receiver.pop(later + Duration::from_secs(1)), None);
}

#[test]
fn audio_given_up_at_the_end_of_a_sound_leaves_no_deadline_behind() {
    let g = audio_group();
    let start = Instant::now();
    let mut receiver = AudioReceiver::new(1);
    // Two packets lost, so parity cannot rebuild them; then silence.
    let order = [&g[0][..], &g[1], &g[4]];
    deliver_audio(&mut receiver, &order, start);
    let later = start + AUDIO_HOLD;
    assert_eq!(receiver.pop(later), Some(AudioOut::Missing { seq: 2 }));
    assert_eq!(receiver.pop(later), Some(AudioOut::Missing { seq: 3 }));
    assert_eq!(receiver.pop(later), None);
    // A deadline in the past would spin the receive loop until sound resumes.
    assert_eq!(receiver.next_deadline(), None);
}
