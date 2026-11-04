//! Loss recovery end to end on a simulated network: the software encoder,
//! the packetizer with FEC, an impaired link both ways, the viewer's
//! reassembler and its recovery requests, and the decoder. Recovery frames
//! against keyframe requests, on the same pictures and the same losses.
//! Every frame the viewer decodes must be exactly the picture the encoder
//! reconstructed for it.

use latch_codec::h264::{Decoder, Encoder, EncoderConfig};
use latch_codec::Yuv420;
use latch_proto::sim::{Impairment, Link};
use latch_proto::video::{
    KeyframeAnswers, KeyframeRequests, OutFrame, Packetizer, Reassembled, Reassembler, Recovery,
};
use latch_proto::wire::{self, Datagram, RecoveryAck, RecoveryRequest, RecoveryStatus};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 192;
const FPS: u32 = 60;
const FRAMES: u32 = 1800;
const SESSION: u32 = 5;

#[derive(Debug, Default)]
struct Outcome {
    /// Keyframes after the first.
    keyframes: u32,
    keyframe_bytes: Vec<usize>,
    recovery_bytes: Vec<usize>,
    p_bytes: Vec<usize>,
    /// Loss declared to next frame delivered, and how long the picture
    /// stood still, per loss, in ms.
    resume_ms: Vec<f64>,
    freeze_ms: Vec<f64>,
    frames_lost: u64,
    delivered: u32,
    checked: u32,
}

impl Outcome {
    fn summary(&self, name: &str) -> String {
        let mean = |v: &[usize]| v.iter().sum::<usize>() as f64 / v.len().max(1) as f64;
        let pct = |v: &[f64], p: f64| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v.get(((v.len() as f64 - 1.0) * p).round() as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let minutes = f64::from(FRAMES) / f64::from(FPS) / 60.0;
        format!(
            "{name}: {} losses, IDRs/min {:.0}, resume p50 {:.0} ms p95 {:.0} ms, \
             freeze p50 {:.0} ms p95 {:.0} ms, IDR {:.0} B, recovery {:.0} B, P {:.0} B, \
             {} of {} frames delivered and checked",
            self.resume_ms.len(),
            f64::from(self.keyframes) / minutes,
            pct(&self.resume_ms, 0.5),
            pct(&self.resume_ms, 0.95),
            pct(&self.freeze_ms, 0.5),
            pct(&self.freeze_ms, 0.95),
            mean(&self.keyframe_bytes),
            mean(&self.recovery_bytes),
            mean(&self.p_bytes),
            self.checked,
            FRAMES,
        )
    }
}

/// A desktop: a detailed still background (text-like texture), a bar
/// sliding across it and a small animated block.
fn draw(picture: &mut Yuv420, frame: u32) {
    let x = (frame * 3 % (WIDTH - 24)) as usize;
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = row * picture.y_stride + col;
            let glyph = ((((col / 3) * 2654435761) ^ ((row / 4) * 40503)) >> 7) & 1 == 1;
            picture.y[at] = if (x..x + 24).contains(&col) {
                220
            } else if (60..92).contains(&row) && (200..232).contains(&col) {
                ((col * 7 + row * 3) as u32 + frame * 5) as u8
            } else if glyph && row % 12 < 9 {
                40
            } else {
                200
            };
        }
    }
    for row in 0..HEIGHT as usize / 2 {
        for col in 0..WIDTH as usize / 2 {
            let at = row * picture.c_stride + col;
            picture.u[at] = 128 + (col / 20) as u8;
            picture.v[at] = 120 + (row / 24) as u8;
        }
    }
    picture.pad_edges();
}

trait TestCodec {
    fn recover_from(&mut self, picture: u64) -> bool;
    fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
    ) -> (Vec<u8>, bool, u64, Option<u64>, Vec<u8>);
    fn decode(&mut self, data: &[u8], keyframe: bool) -> Vec<u8>;
}
fn nv12(picture: &Yuv420) -> Vec<u8> {
    let (mut y, mut uv) = (Vec::new(), Vec::new());
    latch_codec::color::yuv420_to_nv12(picture, &mut y, &mut uv);
    y.extend(uv);
    y
}
struct SoftwareCodec(Encoder, Decoder);
impl TestCodec for SoftwareCodec {
    fn recover_from(&mut self, picture: u64) -> bool {
        self.0.recover_from(picture)
    }
    fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
    ) -> (Vec<u8>, bool, u64, Option<u64>, Vec<u8>) {
        let mut data = Vec::new();
        let info = self.0.encode(picture, keyframe, &mut data).unwrap();
        (
            data,
            info.keyframe,
            info.picture,
            info.recovery,
            nv12(self.0.reconstruction()),
        )
    }
    fn decode(&mut self, data: &[u8], _: bool) -> Vec<u8> {
        nv12(self.1.decode(data).unwrap().unwrap())
    }
}

fn simulate(impairment: Impairment, recover: bool, seed: u64) -> Outcome {
    let mut encoder = Encoder::new(
        EncoderConfig {
            width: WIDTH,
            height: HEIGHT,
            fps: FPS,
            bitrate_kbps: 2500,
            threads: 2,
        }
        .with_frame_budget(0),
    )
    .unwrap();
    encoder.set_recovery_depth(latch_engine::software::recovery_depth(WIDTH, HEIGHT, FPS));
    simulate_with(
        impairment,
        recover,
        seed,
        SoftwareCodec(encoder, Decoder::new()),
    )
}

fn simulate_with(
    impairment: Impairment,
    recover: bool,
    seed: u64,
    mut encoder: impl TestCodec,
) -> Outcome {
    let t0 = Instant::now();
    let tick = Duration::from_millis(1);
    let interval = Duration::from_secs(1) / FPS;
    let mut packetizer = Packetizer::new(SESSION);
    let mut forward = Link::new(impairment, seed);
    let mut backward = Link::new(
        Impairment {
            burst_enter: 0.0,
            ..impairment
        },
        seed ^ 0x5555,
    );
    let mut picture = Yuv420::new(WIDTH, HEIGHT);
    // Host: frame number to picture number, what the decoder must show
    // for each frame, the newest request and its answer.
    let mut sent: VecDeque<(u32, u64)> = VecDeque::new();
    let mut expected: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut request: Option<RecoveryRequest> = None;
    let mut answer: Option<RecoveryAck> = None;
    let mut asked: Option<(u64, u32)> = None;
    let mut keyframe_answers = KeyframeAnswers::new();
    let mut keyframe_owed = true;
    let mut last_keyframe: Option<Instant> = None;
    // Viewer.
    let mut reassembler = Reassembler::new(SESSION);
    let mut recovery = Recovery::new();
    let mut keyframes = KeyframeRequests::new();
    let round_trip = impairment.delay * 2;
    let rtt = Some(round_trip);
    let mut out = Vec::new();
    let mut outcome = Outcome::default();
    let mut lost_at: Option<(Instant, Option<Instant>)> = None;
    let mut last_delivery: Option<Instant> = None;
    let mut next_frame = t0;
    let mut frame = 0;
    let mut now = t0;
    while frame < FRAMES || now < next_frame + Duration::from_millis(300) {
        // Host: requests, then a frame when one is due.
        for datagram in backward.receive(now) {
            match wire::parse(&datagram) {
                Ok(Datagram::RecoveryRequest { request: r, .. }) => {
                    let ack = match answer {
                        Some(a) if a.id == r.id => a,
                        _ => {
                            request = Some(r);
                            RecoveryAck {
                                id: r.id,
                                status: RecoveryStatus::Pending,
                                frame: 0,
                            }
                        }
                    };
                    answer = Some(ack);
                    wire::write_recovery_ack(&mut out, SESSION, &ack);
                    forward.send(now, &out);
                }
                Ok(Datagram::KeyframeRequest { last, .. })
                    if keyframe_answers.request(last, now, rtt) =>
                {
                    keyframe_owed = true;
                }
                _ => {}
            }
        }
        if frame < FRAMES && now >= next_frame {
            next_frame += interval;
            if let Some(r) = request.take() {
                let picture = sent
                    .iter()
                    .find(|(n, _)| *n == r.last_delivered)
                    .map(|p| p.1);
                match picture {
                    Some(p) if !keyframe_owed && encoder.recover_from(p) => asked = Some((p, r.id)),
                    _ => {
                        keyframe_owed = true;
                        let ack = RecoveryAck {
                            id: r.id,
                            status: RecoveryStatus::Refused,
                            frame: 0,
                        };
                        answer = Some(ack);
                        wire::write_recovery_ack(&mut out, SESSION, &ack);
                        forward.send(now, &out);
                    }
                }
            }
            // The host's gate: one requested keyframe per max(50 ms, RTT).
            let keyframe = frame == 0
                || (keyframe_owed
                    && last_keyframe
                        .is_none_or(|t| now - t >= round_trip.max(Duration::from_millis(50))));
            draw(&mut picture, frame);
            let (data, idr, picture_number, recovery_base, reconstruction) =
                encoder.encode(&picture, keyframe);
            if idr {
                keyframe_owed = false;
                last_keyframe = Some(now);
                if frame > 0 {
                    outcome.keyframes += 1;
                    outcome.keyframe_bytes.push(data.len());
                }
            } else if recovery_base.is_some() {
                outcome.recovery_bytes.push(data.len());
            } else {
                outcome.p_bytes.push(data.len());
            }
            let number = packetizer.next_frame();
            if let Some(base) = recovery_base {
                let (p, id) = asked.take().unwrap();
                assert_eq!(p, base);
                let base_frame = sent.iter().find(|(_, q)| *q == base).unwrap().0;
                packetizer.mark_recovery(base_frame);
                let ack = RecoveryAck {
                    id,
                    status: RecoveryStatus::Sent,
                    frame: number,
                };
                answer = Some(ack);
                wire::write_recovery_ack(&mut out, SESSION, &ack);
                forward.send(now, &out);
            }
            sent.push_back((number, picture_number));
            if sent.len() > 256 {
                sent.pop_front();
            }
            expected.insert(number, reconstruction);
            expected.remove(&number.wrapping_sub(256));
            packetizer.packetize(
                &OutFrame {
                    data: &data,
                    keyframe: idr,
                    host_delay: 0,
                },
                |datagram| forward.send(now, datagram),
            );
            if idr {
                keyframe_answers.sent(now);
            }
            frame += 1;
        }
        // Viewer.
        for datagram in forward.receive(now) {
            match wire::parse(&datagram) {
                Ok(Datagram::Video { shard, .. }) => reassembler.push(&shard, now),
                Ok(Datagram::RecoveryAck { ack, .. }) => recovery.ack(ack, now),
                _ => {}
            }
        }
        while let Some(r) = reassembler.poll(now) {
            match r {
                Reassembled::Frame(f) => {
                    let decoded = encoder.decode(&f.data, f.keyframe);
                    assert_eq!(
                        Some(&decoded),
                        expected.get(&f.frame),
                        "frame {} (recovery {}) is not the encoder's picture",
                        f.frame,
                        f.recovery
                    );
                    outcome.checked += 1;
                    outcome.delivered += 1;
                    if let Some((at, before)) = lost_at.take() {
                        outcome.resume_ms.push((now - at).as_secs_f64() * 1000.0);
                        outcome
                            .freeze_ms
                            .push(before.map_or(0.0, |b| (now - b).as_secs_f64() * 1000.0));
                    }
                    last_delivery = Some(now);
                }
                Reassembled::Lost { .. } => {
                    keyframes.lost();
                    lost_at.get_or_insert((now, last_delivery));
                }
            }
        }
        let lost = reassembler.waiting_for_keyframe();
        if recover {
            if let Some(r) = recovery.due(
                lost,
                reassembler.last_delivered(),
                reassembler.next_expected(),
                reassembler.newest_seen(),
                now,
                rtt,
            ) {
                wire::write_recovery_request(&mut out, SESSION, &r);
                backward.send(now, &out);
            }
        }
        let waiting = reassembler.needs_keyframe(t0, now)
            && (!recover || recovery.wants_keyframe() || reassembler.last_delivered().is_none());
        if keyframes.due(waiting, now, rtt) {
            wire::write_keyframe_request(
                &mut out,
                SESSION,
                reassembler.last_delivered().unwrap_or(0),
            );
            backward.send(now, &out);
        }
        now += tick;
    }
    outcome.frames_lost = reassembler.stats().frames_lost;
    outcome
}

fn compare(name: &str, impairment: Impairment, seed: u64) -> (Outcome, Outcome) {
    let before = simulate(impairment, false, seed);
    let after = simulate(impairment, true, seed);
    eprintln!("{}", before.summary(&format!("{name}, keyframes")));
    eprintln!("{}", after.summary(&format!("{name}, recovery frames")));
    assert!(before.keyframes >= 3, "{name}: the link loses frames");
    // Recovery frames replace nearly every keyframe, each far smaller,
    // and the picture stands still no longer for it.
    assert!(
        after.keyframes * 4 <= before.keyframes,
        "{name}: {} keyframes against {}",
        after.keyframes,
        before.keyframes
    );
    assert!(!after.recovery_bytes.is_empty());
    let mean = |v: &[usize]| v.iter().sum::<usize>() / v.len().max(1);
    assert!(
        mean(&after.recovery_bytes) * 3 < mean(&before.keyframe_bytes),
        "{name}: recovery frames are not small"
    );
    (before, after)
}

#[test]
fn recovery_frames_replace_keyframes_on_random_loss() {
    compare(
        "random 12%",
        Impairment {
            loss: 0.12,
            delay: Duration::from_millis(15),
            jitter: Duration::from_millis(3),
            ..Impairment::NONE
        },
        11,
    );
}

#[test]
fn recovery_frames_replace_keyframes_on_burst_loss() {
    compare(
        "bursts",
        Impairment {
            loss: 0.002,
            burst_enter: 0.01,
            burst_exit: 0.3,
            delay: Duration::from_millis(25),
            jitter: Duration::from_millis(4),
            ..Impairment::NONE
        },
        23,
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "hardware loss simulation; LATCH_MAC_LTR=1"]
fn videotoolbox_impairment_simulator() {
    use latch_engine::{
        software::PictureEncoder,
        video::{Codec, VideoConfig},
    };
    struct Native {
        encoder: Box<dyn PictureEncoder>,
        full: Box<dyn latch_stream::video::Decoder>,
        loss: Box<dyn latch_stream::video::Decoder>,
    }
    impl TestCodec for Native {
        fn recover_from(&mut self, picture: u64) -> bool {
            self.encoder.recover_from(picture)
        }
        fn encode(
            &mut self,
            picture: &Yuv420,
            keyframe: bool,
        ) -> (Vec<u8>, bool, u64, Option<u64>, Vec<u8>) {
            let mut data = Vec::new();
            let idr = self.encoder.encode(picture, keyframe, &mut data).unwrap();
            let (number, recovery) = self.encoder.last_picture();
            let expected = native_pixels(self.full.decode(&data, idr, None).unwrap().unwrap());
            (data, idr, number, recovery, expected)
        }
        fn decode(&mut self, data: &[u8], idr: bool) -> Vec<u8> {
            native_pixels(self.loss.decode(data, idr, None).unwrap().unwrap())
        }
    }
    fn native_pixels(frame: latch_stream::video::Frame) -> Vec<u8> {
        frame.with_planes(|y, ys, uv, cs| {
            let mut out = Vec::new();
            for (p, stride, rows) in [(y, ys, HEIGHT), (uv, cs, HEIGHT / 2)] {
                for row in p.chunks(stride).take(rows as usize) {
                    out.extend_from_slice(&row[..WIDTH as usize]);
                }
            }
            out
        })
    }
    for codec in [Codec::H264High, Codec::Hevc] {
        for loss in [0.01, 0.03] {
            for recover in [false, true] {
                let config = VideoConfig {
                    width: WIDTH,
                    height: HEIGHT,
                    fps: FPS,
                    bitrate_kbps: 2500,
                    codec,
                    color: Default::default(),
                };
                let encoder = latch_engine::testmode::videotoolbox_encoder(&config).unwrap();
                assert!(
                    encoder.recovers(),
                    "run with LATCH_MAC_LTR=1 on supporting hardware"
                );
                let outcome = simulate_with(
                    Impairment {
                        loss: 0.0,
                        burst_enter: loss / 4.0,
                        burst_exit: 0.25,
                        delay: Duration::from_millis(15),
                        jitter: Duration::from_millis(4),
                        duplicate: 0.1,
                        reorder: 0.1,
                        reorder_delay: Duration::from_millis(30),
                    },
                    recover,
                    23,
                    Native {
                        encoder,
                        full: latch_stream::video::new_decoder(codec).unwrap(),
                        loss: latch_stream::video::new_decoder(codec).unwrap(),
                    },
                );
                assert!(outcome.checked > FRAMES / 2);
                if recover {
                    assert!(!outcome.recovery_bytes.is_empty());
                }
                eprintln!(
                    "{}",
                    outcome.summary(&format!(
                        "{codec:?} burst {}% recover={recover}",
                        loss * 100.0
                    ))
                );
            }
        }
    }
}
