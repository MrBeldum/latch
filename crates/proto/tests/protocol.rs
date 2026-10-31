use latch_proto::{
    audio::{AudioOut, AudioPacketizer, AudioReceiver},
    control::{
        end, read_message, token_from_hex, token_hex, write_message, Accept, AudioFormat, Codec,
        End, Message, Notice, Report, MAX_MESSAGE,
    },
    input::{Event, InputReceiver, InputSender, BUTTON_LEFT, MOD_SHIFT},
    sim::{Impairment, Link},
    video::{InFrame, OutFrame, Packetizer, Reassembled, Reassembler},
    wire::{self, Datagram, VideoShard},
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Test 1: Frames of random sizes (1 B .. 400 KB; some keyframes) through
/// Packetizer -> sim Link(loss 2%, reorder 1%, dup 1%, jitter 2 ms) ->
/// Reassembler at fec 20%: every delivered frame byte-identical to what was sent,
/// in increasing order; losses reported; nothing delivered after a Lost until a
/// keyframe; report the delivered share.
#[test]
fn test_1_random_video_frames_lossy_link() {
    let mut seed = 0xdeadbeefcafe0001u64;
    let mut prng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let impairment = Impairment {
        loss: 0.02,
        burst_enter: 0.0,
        burst_exit: 1.0,
        duplicate: 0.01,
        reorder: 0.01,
        reorder_delay: Duration::from_millis(5),
        delay: Duration::from_millis(5),
        jitter: Duration::from_millis(2),
    };
    let mut link = Link::new(impairment, prng());
    let mut packetizer = Packetizer::new(1);
    packetizer.set_fec_percent(20);
    let mut reassembler = Reassembler::new(1);

    let num_frames = 60;
    let mut sent_frames: HashMap<u32, (Vec<u8>, bool)> = HashMap::new();

    let base_time = Instant::now();
    let mut now = base_time;

    let mut last_delivered_frame: Option<u32> = None;
    let mut waiting_for_keyframe = true;
    let mut delivered_count = 0;
    let mut lost_count = 0;

    for frame_idx in 0..num_frames {
        // Frame sizes range from 1 B to 400 KB
        let size = match frame_idx {
            0 => 20_000,
            1 => 1,
            2 => 100,
            3 => 400_000,
            _ => 1 + (prng() as usize % 400_000),
        };
        let is_keyframe = frame_idx == 0 || frame_idx % 15 == 0;
        let data: Vec<u8> = (0..size).map(|i| (i ^ frame_idx as usize) as u8).collect();

        sent_frames.insert(frame_idx as u32, (data.clone(), is_keyframe));

        packetizer.packetize(
            &OutFrame {
                data: &data,
                keyframe: is_keyframe,
                host_delay: 10,
            },
            |buf| {
                link.send(now, buf);
            },
        );

        // Advance simulated time in 1 ms steps up to the next frame (16 ms, ~60 fps)
        for _ in 0..16 {
            now += Duration::from_millis(1);
            for dg in link.receive(now) {
                if let Ok(Datagram::Video { shard, .. }) = wire::parse(&dg) {
                    reassembler.push(&shard, now);
                }
            }
            while let Some(res) = reassembler.poll(now) {
                match res {
                    Reassembled::Frame(InFrame {
                        frame,
                        data,
                        keyframe,
                        ..
                    }) => {
                        assert!(
                            !waiting_for_keyframe || keyframe,
                            "Delivered non-keyframe {frame} while waiting for keyframe!"
                        );
                        if let Some(prev) = last_delivered_frame {
                            assert!(
                                latch_proto::video::before(prev, frame),
                                "Delivered frame {frame} out of order after {prev}"
                            );
                        }
                        last_delivered_frame = Some(frame);
                        waiting_for_keyframe = false;
                        delivered_count += 1;

                        let (expected_data, expected_kf) = &sent_frames[&frame];
                        assert_eq!(
                            &data, expected_data,
                            "Delivered frame {frame} bit-for-bit mismatch!"
                        );
                        assert_eq!(keyframe, *expected_kf);
                    }
                    Reassembled::Lost { frame: _ } => {
                        waiting_for_keyframe = true;
                        lost_count += 1;
                    }
                }
            }
        }
    }

    // Drain link flight and wait for reassembler deadlines
    for _ in 0..150 {
        now += Duration::from_millis(2);
        for dg in link.receive(now) {
            if let Ok(Datagram::Video { shard, .. }) = wire::parse(&dg) {
                reassembler.push(&shard, now);
            }
        }
        while let Some(res) = reassembler.poll(now) {
            match res {
                Reassembled::Frame(InFrame {
                    frame,
                    data,
                    keyframe,
                    ..
                }) => {
                    assert!(
                        !waiting_for_keyframe || keyframe,
                        "Delivered non-keyframe {frame} while waiting for keyframe!"
                    );
                    if let Some(prev) = last_delivered_frame {
                        assert!(
                            latch_proto::video::before(prev, frame),
                            "Delivered frame {frame} out of order after {prev}"
                        );
                    }
                    last_delivered_frame = Some(frame);
                    waiting_for_keyframe = false;
                    delivered_count += 1;

                    let (expected_data, expected_kf) = &sent_frames[&frame];
                    assert_eq!(&data, expected_data);
                    assert_eq!(keyframe, *expected_kf);
                }
                Reassembled::Lost { frame: _ } => {
                    waiting_for_keyframe = true;
                    lost_count += 1;
                }
            }
        }
    }

    let stats = reassembler.stats();
    println!(
        "Test 1 Results: Delivered {}/{} ({:.1}%), Lost: {}, Skipped: {}, Missing shards: {}, Recovered shards: {}",
        delivered_count,
        num_frames,
        100.0 * delivered_count as f64 / num_frames as f64,
        lost_count,
        stats.frames_skipped,
        stats.shards_missing,
        stats.shards_recovered
    );

    assert!(
        delivered_count >= (num_frames * 85) / 100,
        "Delivered share too low: {delivered_count}/{num_frames}"
    );
    assert_eq!(stats.frames_delivered, delivered_count as u64);
    assert_eq!(stats.frames_lost, lost_count as u64);
}

/// Test 2: Burst loss behaves the same way; no impairment -> every frame delivered.
#[test]
fn test_2_burst_loss_and_no_impairment() {
    let mut seed = 0x9876543210fedcbau64;
    let mut prng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    // Part A: Burst loss
    let burst_impairment = Impairment {
        loss: 0.0,
        burst_enter: 0.005,
        burst_exit: 0.20,
        duplicate: 0.01,
        reorder: 0.01,
        reorder_delay: Duration::from_millis(5),
        delay: Duration::from_millis(5),
        jitter: Duration::from_millis(2),
    };
    let mut link = Link::new(burst_impairment, prng());
    let mut packetizer = Packetizer::new(1);
    packetizer.set_fec_percent(25);
    let mut reassembler = Reassembler::new(1);

    let num_frames = 40;
    let mut sent_frames: HashMap<u32, (Vec<u8>, bool)> = HashMap::new();
    let mut now = Instant::now();
    let mut last_delivered_frame = None;
    let mut waiting_for_keyframe = true;
    let mut delivered_count = 0;

    for frame_idx in 0..num_frames {
        let size = 1 + (prng() as usize % 50_000);
        let is_keyframe = frame_idx == 0 || frame_idx % 8 == 0;
        let data: Vec<u8> = (0..size).map(|i| (i ^ frame_idx as usize) as u8).collect();
        sent_frames.insert(frame_idx as u32, (data.clone(), is_keyframe));

        packetizer.packetize(
            &OutFrame {
                data: &data,
                keyframe: is_keyframe,
                host_delay: 0,
            },
            |buf| {
                link.send(now, buf);
            },
        );

        for _ in 0..16 {
            now += Duration::from_millis(1);
            for dg in link.receive(now) {
                if let Ok(Datagram::Video { shard, .. }) = wire::parse(&dg) {
                    reassembler.push(&shard, now);
                }
            }
            while let Some(res) = reassembler.poll(now) {
                match res {
                    Reassembled::Frame(InFrame {
                        frame,
                        data,
                        keyframe,
                        ..
                    }) => {
                        assert!(!waiting_for_keyframe || keyframe);
                        if let Some(prev) = last_delivered_frame {
                            assert!(latch_proto::video::before(prev, frame));
                        }
                        last_delivered_frame = Some(frame);
                        waiting_for_keyframe = false;
                        delivered_count += 1;
                        assert_eq!(&data, &sent_frames[&frame].0);
                    }
                    Reassembled::Lost { frame: _ } => {
                        waiting_for_keyframe = true;
                    }
                }
            }
        }
    }

    // Drain
    for _ in 0..100 {
        now += Duration::from_millis(2);
        for dg in link.receive(now) {
            if let Ok(Datagram::Video { shard, .. }) = wire::parse(&dg) {
                reassembler.push(&shard, now);
            }
        }
        while let Some(res) = reassembler.poll(now) {
            match res {
                Reassembled::Frame(InFrame {
                    frame,
                    data,
                    keyframe,
                    ..
                }) => {
                    assert!(!waiting_for_keyframe || keyframe);
                    if let Some(prev) = last_delivered_frame {
                        assert!(latch_proto::video::before(prev, frame));
                    }
                    last_delivered_frame = Some(frame);
                    waiting_for_keyframe = false;
                    delivered_count += 1;
                    assert_eq!(&data, &sent_frames[&frame].0);
                }
                Reassembled::Lost { frame: _ } => {
                    waiting_for_keyframe = true;
                }
            }
        }
    }

    println!(
        "Test 2 (Burst) Results: Delivered {}/{} ({:.1}%)",
        delivered_count,
        num_frames,
        100.0 * delivered_count as f64 / num_frames as f64
    );

    // Part B: No impairment -> every frame delivered
    let mut perfect_link = Link::new(Impairment::NONE, 1);
    let mut perfect_packetizer = Packetizer::new(2);
    let mut perfect_reassembler = Reassembler::new(2);
    let mut perfect_sent = HashMap::new();
    let now = Instant::now();

    for i in 0..25 {
        let size = match i {
            0 => 10,
            1 => 0, // Test empty frame
            2 => 1500,
            3 => 80_000,
            _ => (i * 3700) % 200_000 + 1,
        };
        let is_keyframe = i == 0 || i == 10;
        let data = vec![i as u8; size];
        perfect_sent.insert(i as u32, data.clone());

        perfect_packetizer.packetize(
            &OutFrame {
                data: &data,
                keyframe: is_keyframe,
                host_delay: 5,
            },
            |buf| {
                perfect_link.send(now, buf);
            },
        );
    }

    for dg in perfect_link.receive(now) {
        if let Ok(Datagram::Video { shard, .. }) = wire::parse(&dg) {
            perfect_reassembler.push(&shard, now);
        }
    }

    let mut perfect_delivered = 0;
    while let Some(res) = perfect_reassembler.poll(now) {
        match res {
            Reassembled::Frame(InFrame { frame, data, .. }) => {
                assert_eq!(data, perfect_sent[&frame]);
                perfect_delivered += 1;
            }
            Reassembled::Lost { frame } => panic!("Unexpected lost frame {frame} on perfect link!"),
        }
    }
    assert_eq!(
        perfect_delivered, 25,
        "Every frame must be delivered on perfect link!"
    );
    assert_eq!(perfect_reassembler.stats().frames_lost, 0);
    assert_eq!(perfect_reassembler.stats().frames_skipped, 0);
}

/// Test 3: Input: 10 000 random events through a Link with 20% loss, 5% reorder,
/// 5% dup, acks returned through another lossy Link, sender polled on a simulated
/// clock: the host applies exactly the same events, once, in order.
#[test]
fn test_3_input_10k_events_lossy_link() {
    let mut seed = 0x5a5a5a5a5a5a5a5au64;
    let mut prng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let fwd_impairment = Impairment {
        loss: 0.20,
        burst_enter: 0.0,
        burst_exit: 1.0,
        duplicate: 0.05,
        reorder: 0.05,
        reorder_delay: Duration::from_millis(5),
        delay: Duration::from_millis(5),
        jitter: Duration::from_millis(2),
    };
    let bwd_impairment = Impairment {
        loss: 0.20,
        burst_enter: 0.0,
        burst_exit: 1.0,
        duplicate: 0.05,
        reorder: 0.05,
        reorder_delay: Duration::from_millis(5),
        delay: Duration::from_millis(5),
        jitter: Duration::from_millis(2),
    };

    let mut forward_link = Link::new(fwd_impairment, prng());
    let mut backward_link = Link::new(bwd_impairment, prng());

    let mut sender = InputSender::new(1);
    let mut receiver = InputReceiver::new();

    let total_events = 10_000;
    let mut events_to_send = Vec::with_capacity(total_events);

    for i in 0..total_events {
        let ev = match i % 7 {
            0 => Event::MouseMove {
                dx: ((prng() % 200) as i16) - 100,
                dy: ((prng() % 200) as i16) - 100,
            },
            1 => Event::MousePosition {
                x: (prng() % 1920) as u16,
                y: (prng() % 1080) as u16,
                width: 1920,
                height: 1080,
            },
            2 => Event::MouseButton {
                button: (prng() % 5 + 1) as u8,
                down: prng() % 2 == 1,
            },
            3 => Event::Key {
                vk: (prng() % 256) as u16,
                down: prng() % 2 == 1,
                modifiers: (prng() % 16) as u8,
            },
            4 => Event::Scroll {
                vertical: (((prng() % 5) as i16) - 2) * 120,
                horizontal: (((prng() % 5) as i16) - 2) * 120,
            },
            5 => Event::Text(format!("hello_{}", i % 100)),
            _ => Event::ReleaseAll,
        };
        events_to_send.push(ev);
    }

    let mut applied_events = Vec::with_capacity(total_events);
    let mut now = Instant::now();
    let mut push_idx = 0;
    let mut out_buf = Vec::new();
    let mut ack_buf = Vec::new();

    // Run simulation until all events are pushed, delivered, and acked
    let mut step = 0;
    while applied_events.len() < total_events || sender.unacked() > 0 {
        step += 1;
        if step > 200_000 {
            panic!(
                "Simulation took too long! applied: {}, sent unacked: {}",
                applied_events.len(),
                sender.unacked()
            );
        }

        // Push up to 10 events per simulated ms
        for _ in 0..10 {
            if push_idx < total_events && sender.unacked() < 500 {
                sender.push(events_to_send[push_idx].clone());
                push_idx += 1;
            }
        }

        // Poll sender (send all ready batches)
        while sender.poll(now, &mut out_buf) {
            forward_link.send(now, &out_buf);
        }

        // Forward link delivery -> Host
        for dg in forward_link.receive(now) {
            if let Ok(Datagram::Input { first, events, .. }) = wire::parse(&dg) {
                let next = receiver.receive(first, events.iter(), |ev| {
                    applied_events.push(ev);
                });
                wire::write_input_ack(&mut ack_buf, 1, next);
                backward_link.send(now, &ack_buf);
            }
        }

        // Backward link delivery -> Client
        for dg in backward_link.receive(now) {
            if let Ok(Datagram::InputAck { next, .. }) = wire::parse(&dg) {
                sender.ack(next);
            }
        }

        // Advance simulated time in 1 ms steps
        now += Duration::from_millis(1);
    }

    assert_eq!(
        applied_events.len(),
        total_events,
        "All events must be applied!"
    );
    assert_eq!(
        applied_events, events_to_send,
        "Applied events must exactly match sent events in order!"
    );
}

/// Test 4: Audio: 5% loss -> most lost packets rebuilt, output in order,
/// Missing only for unrecoverable ones, no duplicates.
#[test]
fn test_4_audio_5_pct_loss() {
    let impairment = Impairment {
        loss: 0.05,
        burst_enter: 0.0,
        burst_exit: 1.0,
        duplicate: 0.0,
        reorder: 0.0,
        reorder_delay: Duration::ZERO,
        delay: Duration::from_millis(5),
        jitter: Duration::from_millis(1),
    };

    let mut link = Link::new(impairment, 0x12345678);
    let mut packetizer = AudioPacketizer::new(1);
    let mut receiver = AudioReceiver::new(1);

    let num_packets = 1000;
    let mut sent_payloads = HashMap::new();
    let mut now = Instant::now();
    let mut outputs = Vec::new();

    for i in 0..num_packets {
        let pts = i * 10_000;
        let payload = vec![(i % 256) as u8; 60 + (i % 30) as usize];
        sent_payloads.insert(i, payload.clone());

        packetizer.packetize(pts, &payload, |buf| {
            link.send(now, buf);
        });
        now += Duration::from_millis(10);
        // Consume audio as it arrives, as the player does. Holding ten
        // seconds in the receiver would deliberately exceed its window.
        for dg in link.receive(now) {
            if let Ok(Datagram::Audio { packet, .. }) = wire::parse(&dg) {
                receiver.push(&packet, now);
                while let Some(out) = receiver.pop() {
                    outputs.push(out);
                }
            }
        }
    }

    // Drain link
    for _ in 0..100 {
        now += Duration::from_millis(2);
        for dg in link.receive(now) {
            if let Ok(Datagram::Audio { packet, .. }) = wire::parse(&dg) {
                receiver.push(&packet, now);
                while let Some(out) = receiver.pop() {
                    outputs.push(out);
                }
            }
        }
    }

    // Pop all outputs from receiver
    while let Some(out) = receiver.pop() {
        outputs.push(out);
    }

    assert!(!outputs.is_empty(), "Receiver must produce audio outputs!");

    let mut last_seq = None;
    let mut delivered_frames = 0;
    let mut missing_packets = 0;

    for out in &outputs {
        match out {
            AudioOut::Frame { seq, pts, payload } => {
                if let Some(prev) = last_seq {
                    assert_eq!(*seq, prev + 1, "Audio frames must be strictly sequential!");
                }
                last_seq = Some(*seq);
                delivered_frames += 1;
                assert_eq!(
                    payload, &sent_payloads[seq],
                    "Audio payload must match sent payload!"
                );
                assert_eq!(*pts, seq * 10_000);
            }
            AudioOut::Missing { seq } => {
                if let Some(prev) = last_seq {
                    assert_eq!(
                        *seq,
                        prev + 1,
                        "Missing audio packets must be strictly sequential!"
                    );
                }
                last_seq = Some(*seq);
                missing_packets += 1;
            }
        }
    }

    let (recv_count, rebuilt_count, given_up_count) = receiver.stats();
    println!(
        "Test 4 (Audio) Results: {} outputs ({} delivered, {} missing). Stats: received {}, rebuilt {}, given up {}",
        outputs.len(),
        delivered_frames,
        missing_packets,
        recv_count,
        rebuilt_count,
        given_up_count
    );

    assert!(
        rebuilt_count > 0,
        "At 5% loss, parity should have rebuilt lost packets!"
    );
    assert!(
        rebuilt_count > given_up_count,
        "Most lost packets should be rebuilt (rebuilt: {rebuilt_count} vs given up: {given_up_count})!"
    );
}

/// Test 5: Fuzz: 100 000 random byte strings and random mutations of valid datagrams
/// through wire::parse and Event::read never panic.
#[test]
fn test_5_wire_fuzz_random_and_mutations() {
    let mut seed = 0xabcdef0123456789u64;
    let mut prng = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    // 1. 50 000 pure random byte slices
    for _ in 0..50_000 {
        let len = (prng() as usize) % 1300;
        let mut bytes = vec![0u8; len];
        for b in &mut bytes {
            *b = prng() as u8;
        }
        let _ = wire::parse(&bytes);
        let _ = Event::read(&bytes);
    }

    // 2. 50 000 mutations of valid datagrams
    let mut valid_packets = Vec::new();

    // Hello
    let mut buf = Vec::new();
    wire::write_hello(&mut buf, 42, Some(&[7u8; 16]));
    valid_packets.push(buf.clone());
    wire::write_hello(&mut buf, 42, None);
    valid_packets.push(buf.clone());

    // Video
    let shard_bytes = vec![0xee; 500];
    let shard = VideoShard {
        keyframe: true,
        host_delay: 15,
        frame: 100,
        frame_len: 2000,
        shard: 1,
        block: 0,
        blocks: 1,
        data: 4,
        parity: 2,
        shard_len: 500,
        bytes: &shard_bytes,
    };
    wire::write_video(&mut buf, 42, &shard);
    valid_packets.push(buf.clone());

    // Audio
    let audio_payload = vec![0xaa; 120];
    let audio_pkt = wire::AudioPacket {
        parity: false,
        seq: 10,
        pts: 100_000,
        payload: &audio_payload,
    };
    wire::write_audio(&mut buf, 42, &audio_pkt);
    valid_packets.push(buf.clone());

    // Input
    let events = vec![
        Event::MouseMove { dx: 10, dy: -5 },
        Event::MouseButton {
            button: BUTTON_LEFT,
            down: true,
        },
        Event::Key {
            vk: 65,
            down: true,
            modifiers: MOD_SHIFT,
        },
        Event::Text("test string".into()),
        Event::ReleaseAll,
    ];
    wire::write_input(&mut buf, 42, 1, &events);
    valid_packets.push(buf.clone());

    // InputAck, KeyframeRequest, Ping, Pong
    wire::write_input_ack(&mut buf, 42, 5);
    valid_packets.push(buf.clone());
    wire::write_keyframe_request(&mut buf, 42, 10);
    valid_packets.push(buf.clone());
    wire::write_ping(&mut buf, 42, 123456789);
    valid_packets.push(buf.clone());
    wire::write_pong(&mut buf, 42, 123456789);
    valid_packets.push(buf.clone());

    for i in 0..50_000 {
        let base = &valid_packets[i % valid_packets.len()];
        let mut mutated = base.clone();

        let mutation_kind = prng() % 4;
        match mutation_kind {
            0 => {
                // Truncate
                if !mutated.is_empty() {
                    let new_len = (prng() as usize) % mutated.len();
                    mutated.truncate(new_len);
                }
            }
            1 => {
                // Flip random bits / bytes
                let count = (prng() % 5 + 1) as usize;
                for _ in 0..count {
                    if !mutated.is_empty() {
                        let idx = (prng() as usize) % mutated.len();
                        mutated[idx] ^= prng() as u8;
                    }
                }
            }
            2 => {
                // Extend with random bytes
                let extra = (prng() % 100 + 1) as usize;
                mutated.extend((0..extra).map(|_| prng() as u8));
            }
            _ => {
                // Replace random slices
                if mutated.len() > 4 {
                    let start = (prng() as usize) % (mutated.len() - 2);
                    mutated[start] = prng() as u8;
                    mutated[start + 1] = prng() as u8;
                }
            }
        }

        let _ = wire::parse(&mutated);
        let _ = Event::read(&mutated);
    }
}

/// Test 6: Control: round-trip every message; truncated input -> error;
/// oversize length -> InvalidData.
#[test]
fn test_6_control_messages_roundtrip_and_errors() {
    let messages = vec![
        Message::Accept(Accept {
            session: 0x12345678,
            token: "0123456789abcdef0123456789abcdef".into(),
            codec: Codec::Hevc,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 15_000,
            encoder: "nvenc".into(),
            audio: Some(AudioFormat {
                rate: 48000,
                channels: 2,
                frame_samples: 480,
                codec: "opus".into(),
            }),
            audio_problem: "".into(),
            cursor: true,
        }),
        Message::End(End {
            code: end::CAPTURE_FAILED,
            message: "GPU device reset".into(),
        }),
        Message::Keepalive,
        Message::Notice(Notice {
            kind: "video".into(),
            message: "Resolution changed to 1280x720".into(),
        }),
        Message::Report(Report {
            fps: 59.9,
            loss_pct: 1.2,
            decode_ms: 3.5,
            rtt_ms: 12.0,
        }),
        Message::Cursor(latch_proto::control::Cursor {
            visible: true,
            shape: Some(latch_proto::control::CursorShape::new(
                1,
                16,
                16,
                0,
                0,
                &[255; 16 * 16 * 4],
            )),
        }),
        Message::DrawCursor(latch_proto::control::DrawCursor { draw: false }),
        Message::Unknown(42),
    ];

    for msg in &messages {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).expect("write_message failed");

        let mut slice = &buf[..];
        let decoded = read_message(&mut slice).expect("read_message failed");
        assert_eq!(&decoded, msg);

        // Truncated input tests
        for trunc_len in 0..buf.len() {
            let mut trunc_slice = &buf[..trunc_len];
            assert!(
                read_message(&mut trunc_slice).is_err(),
                "Truncated input of len {trunc_len} should return error!"
            );
        }
    }

    // Oversize length in read_message
    let oversize_len = (MAX_MESSAGE as u32) + 1;
    let mut oversize_header = oversize_len.to_le_bytes().to_vec();
    oversize_header.extend(vec![0u8; 100]);
    let mut slice = &oversize_header[..];
    let err = read_message(&mut slice).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    // Zero length in read_message
    let zero_len = 0u32.to_le_bytes().to_vec();
    let mut slice = &zero_len[..];
    let err = read_message(&mut slice).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    // Token hex helpers
    let token = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10,
    ];
    let hex = token_hex(&token);
    assert_eq!(hex, "0123456789abcdeffedcba9876543210");
    assert_eq!(token_from_hex(&hex), Some(token));
    assert_eq!(
        token_from_hex("0123456789ABCDEFFEDCBA9876543210"),
        Some(token)
    );
    assert_eq!(token_from_hex("short"), None);
    assert_eq!(token_from_hex("0123456789abcdef0123456789abcdeg"), None);
}

/// Test 7: Performance (#[ignore]; run it in release and report):
/// Packetizer + FEC for a 1 MB keyframe at 20% < 5 ms on this Mac;
/// Reassembler rebuilding it with 5% loss also < 5 ms.
#[test]
#[ignore]
fn test_7_performance_video_and_fec() {
    let data = vec![0x55u8; 1_000_000];
    let mut packetizer = Packetizer::new(1);
    packetizer.set_fec_percent(20);

    let start = Instant::now();
    let mut shards = Vec::with_capacity(1100);
    packetizer.packetize(
        &OutFrame {
            data: &data,
            keyframe: true,
            host_delay: 0,
        },
        |buf| shards.push(buf.to_vec()),
    );
    let encode_time = start.elapsed();

    // Reassembler rebuilding with 5% loss (drop 1 out of 20 shards)
    let mut reassembler = Reassembler::new(1);
    let now = Instant::now();

    let start = Instant::now();
    for (i, shard_buf) in shards.iter().enumerate() {
        if i % 20 == 0 {
            continue; // 5% loss
        }
        if let Ok(Datagram::Video { shard, .. }) = wire::parse(shard_buf) {
            reassembler.push(&shard, now);
        }
    }
    let res = reassembler.poll(now);
    let decode_time = start.elapsed();

    match res {
        Some(Reassembled::Frame(InFrame {
            data: assembled, ..
        })) => {
            assert_eq!(
                assembled, data,
                "Reassembled data must match original 1 MB!"
            );
        }
        other => panic!("Expected reassembled frame, got {other:?}"),
    }

    println!(
        "Test 7 Performance: 1 MB keyframe at 20% FEC: packetize+encode: {:?}, reassemble 5% loss: {:?}",
        encode_time, decode_time
    );

    assert!(
        encode_time < Duration::from_millis(5),
        "Encode took {:?}, expected < 5 ms",
        encode_time
    );
    assert!(
        decode_time < Duration::from_millis(5),
        "Decode took {:?}, expected < 5 ms",
        decode_time
    );
}
