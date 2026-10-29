use latch_proto::{
    audio::{AudioOut, AudioPacketizer, AudioReceiver},
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
    assert_eq!(sender.unacked(), MAX_UNACKED + critical.len());
    let applied = drain_input(&mut sender, start + Duration::from_secs(9));
    assert_eq!(
        &applied[..MAX_UNACKED - 1],
        &(0..1023).map(position).collect::<Vec<_>>()
    );
    assert_eq!(applied[MAX_UNACKED - 1], position(1033));
    assert_eq!(&applied[MAX_UNACKED..], &critical);
}

#[test]
fn input_full_queue_merges_relative_motion_saturating() {
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
    assert_eq!(applied.len(), MAX_UNACKED);
    assert_eq!(
        applied.last(),
        Some(&Event::MouseMove {
            dx: i16::MAX,
            dy: i16::MIN
        })
    );
}

#[test]
fn input_hard_cap_refuses_only_new_events_without_sequence_gaps() {
    let mut sender = InputSender::new(1);
    for _ in 0..65_536 + 1 {
        sender.push(Event::ReleaseAll);
    }
    assert_eq!(sender.unacked(), 65_536);
    assert_eq!(drain_input(&mut sender, Instant::now()).len(), 65_536);
    sender.push(position(1));
    let mut out = Vec::new();
    assert!(sender.poll(Instant::now(), &mut out));
    let Datagram::Input { first, .. } = wire::parse(&out).unwrap() else {
        panic!()
    };
    assert_eq!(first, 65_536);
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
                while let Some(out) = receiver.pop() {
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
        while let Some(out) = receiver.pop() {
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
