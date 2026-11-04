//! VideoToolbox LTR recovery through the real pipeline: frames submitted
//! with [`super::super::super::IN_FLIGHT`] in flight and collected as
//! `mac::Video::next` does, so frames in flight at a request are discarded
//! at its boundary. The viewer loses bursts, asks late, asks again when a
//! refresh is lost, and meets IDR requests and bitrate changes. Every
//! picture it accepts must equal the picture an uninterrupted decoder makes
//! of everything VideoToolbox produced, discarded frames included, in
//! Apple's decoder and in FFmpeg, and FFmpeg must not have to conceal or
//! repair anything.
use super::*;
use std::collections::{HashMap, HashSet};

/// 320×192 unless `LATCH_VT_SIM_SIZE=WxH`, at `LATCH_VT_SIM_KBPS` (3000).
fn size() -> (usize, usize) {
    std::env::var("LATCH_VT_SIM_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((320, 192))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

/// Twelve pictures in turn: periodic content, so a later picture is best
/// predicted by an older one as well as by its predecessor.
fn pool() -> Vec<Owned> {
    let (sim_width, sim_height) = size();
    (0..12)
        .map(|i| {
            let image = create_surface_sized(sim_width, sim_height);
            // SAFETY: exclusive buffer, locked while its planes are written with their strides.
            unsafe {
                CVPixelBufferLockBaseAddress(image.0, 0);
                for (plane, rows, cols) in
                    [(0, sim_height, sim_width), (1, sim_height / 2, sim_width)]
                {
                    let stride = CVPixelBufferGetBytesPerRowOfPlane(image.0, plane);
                    let base = CVPixelBufferGetBaseAddressOfPlane(image.0, plane);
                    for y in 0..rows {
                        for x in 0..cols {
                            let (sx, sy) = (x + i * 21, y + i * 9);
                            let mut h = (sx as u32).wrapping_mul(0x45d9f3b)
                                ^ (sy as u32).wrapping_mul(0x119de1f3);
                            h ^= h >> 15;
                            let value = if plane == 0 {
                                48 + ((sx / 6 + sy / 6) % 128) as u32 + h % 41
                            } else {
                                108 + (sx / 16 + plane) as u32 % 40
                            };
                            *base.add(y * stride + x) = value.min(235) as u8;
                        }
                    }
                }
                CVPixelBufferUnlockBaseAddress(image.0, 0);
            }
            image
        })
        .collect()
}

fn pixels(frame: &latch_stream::video::Frame) -> Vec<u8> {
    let (width, height) = size();
    frame.with_planes(|y, ys, uv, cs| {
        let mut out = Vec::new();
        for (plane, stride, rows) in [(y, ys, height), (uv, cs, height / 2)] {
            for row in plane.chunks(stride).take(rows) {
                out.extend_from_slice(&row[..width]);
            }
        }
        out
    })
}

#[derive(Default, Debug)]
pub(super) struct Stats {
    pub emitted: usize,
    pub accepted: usize,
    /// Requests the encoder answered with a refresh, and refused.
    pub refreshes: usize,
    pub refused: usize,
    pub recovery_frames: usize,
    pub idrs: usize,
    /// Acknowledged tokens of pictures the viewer does not have.
    pub lost_token_acks: usize,
    pub mismatches: usize,
    pub decode_errors: usize,
    pub first_problem: Option<String>,
    pub ffmpeg: Option<String>,
}

struct Viewer {
    rng: Rng,
    loss: f64,
    burst: u64,
    broken: bool,
    asked_at: usize,
    last_delivered: u64,
    has: HashSet<u64>,
    requests: VecDeque<(usize, u64)>,
    decoder: Box<dyn latch_stream::video::Decoder>,
    full: Box<dyn latch_stream::video::Decoder>,
    truth: HashMap<i64, Vec<u8>>,
    full_stream: Vec<(i64, Vec<u8>)>,
    loss_stream: Vec<(i64, Vec<u8>)>,
    stats: Stats,
}

impl Viewer {
    fn problem(&mut self, what: String) {
        eprintln!("{what}");
        self.stats.first_problem.get_or_insert(what);
    }
    /// Decodes everything VideoToolbox produced since the last call.
    fn ground_truth(&mut self, encoder: &Encoder) {
        let tap = std::mem::take(encoder.output.output.lock().tap.as_mut().unwrap());
        for (index, data, idr) in tap {
            let picture = self
                .full
                .decode(&data, idr, None)
                .unwrap()
                .expect("uninterrupted decode");
            self.truth.insert(index, pixels(&picture));
            self.full_stream.push((index, data));
        }
    }
    fn drain(&mut self, encoder: &mut Encoder, n: usize, wait: Duration) {
        loop {
            let done = encoder.collect(wait);
            self.ground_truth(encoder);
            let Some((index, _, frame)) = done else {
                return;
            };
            match frame {
                // A dropped frame: the pipeline asks for an IDR.
                None => encoder.request_keyframe(),
                Some(frame) => self.receive(n, index, frame.unwrap()),
            }
            if !wait.is_zero() {
                return;
            }
        }
    }
    fn receive(&mut self, n: usize, index: i64, frame: EncodedVideo) {
        self.stats.emitted += 1;
        self.stats.idrs += frame.keyframe as usize;
        self.stats.recovery_frames += frame.recovery.is_some() as usize;
        let lost = if self.burst > 0 {
            self.burst -= 1;
            true
        } else if self.rng.chance(self.loss) {
            self.burst = self.rng.next() % 4;
            true
        } else {
            false
        };
        let delay = 1 + (self.rng.next() % 5) as usize;
        if lost {
            if !self.broken {
                self.broken = true;
                self.asked_at = n;
                self.requests.push_back((n + delay, self.last_delivered));
            }
            return;
        }
        if self.broken {
            if !frame.keyframe && frame.recovery != Some(self.last_delivered) {
                if n - self.asked_at > 10 {
                    // The request or its refresh was lost: ask again.
                    self.asked_at = n;
                    self.requests.push_back((n + delay, self.last_delivered));
                }
                return;
            }
            self.broken = false;
        }
        if frame.keyframe {
            self.has.clear();
        }
        self.stats.accepted += 1;
        self.last_delivered = frame.picture;
        self.has.insert(frame.picture);
        self.loss_stream.push((index, frame.data.clone()));
        let expected = self.truth.get(&index).cloned();
        match self.decoder.decode(&frame.data, frame.keyframe, None) {
            Ok(Some(picture)) if Some(pixels(&picture)) == expected => {}
            Ok(picture) => {
                self.stats.mismatches += 1;
                self.problem(format!(
                    "picture {} (IDR {}, recovery {:?}) differs; decoded {}",
                    frame.picture,
                    frame.keyframe,
                    frame.recovery,
                    picture.is_some()
                ));
            }
            Err(error) => {
                self.stats.decode_errors += 1;
                self.problem(format!(
                    "picture {} (IDR {}, recovery {:?}): {error:#}",
                    frame.picture, frame.keyframe, frame.recovery
                ));
            }
        }
        self.truth.retain(|&i, _| i > index);
    }
    /// FFmpeg decodes both streams; it must stay silent, and agree.
    fn ffmpeg(&mut self, codec: Codec, name: &str) {
        let Some(ffmpeg) = ffmpeg() else {
            return;
        };
        let dir =
            std::env::temp_dir().join(format!("latch-vt-pipeline-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ext = if codec == Codec::Hevc { "hevc" } else { "h264" };
        let mut decoded = Vec::new();
        for (part, stream) in [("full", &self.full_stream), ("loss", &self.loss_stream)] {
            let input = dir.join(format!("{part}.{ext}"));
            fs::write(
                &input,
                stream
                    .iter()
                    .flat_map(|(_, d)| d.iter().copied())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let output = dir.join(format!("{part}.nv12"));
            let result = Command::new(&ffmpeg)
                .args(["-v", "warning", "-y", "-threads", "1", "-i"])
                .arg(&input)
                .args([
                    "-fps_mode",
                    "passthrough",
                    "-pix_fmt",
                    "nv12",
                    "-f",
                    "rawvideo",
                ])
                .arg(&output)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
            if !result.status.success() || !stderr.is_empty() {
                self.stats.ffmpeg = Some(format!("{part}: {stderr}"));
            }
            decoded.push(fs::read(&output).unwrap_or_default());
        }
        let size = size().0 * size().1 * 3 / 2;
        let at: HashMap<i64, usize> = self
            .full_stream
            .iter()
            .enumerate()
            .map(|(i, (index, _))| (*index, i))
            .collect();
        for (i, (index, _)) in self.loss_stream.iter().enumerate() {
            let full = decoded[0].get(at[index] * size..(at[index] + 1) * size);
            let loss = decoded[1].get(i * size..(i + 1) * size);
            if full.is_none() || full != loss {
                self.stats
                    .ffmpeg
                    .get_or_insert(format!("FFmpeg differs at index {index}"));
                break;
            }
        }
        if self.stats.ffmpeg.is_none() {
            fs::remove_dir_all(dir).unwrap();
        } else {
            eprintln!("FFmpeg streams kept in {}", dir.display());
        }
    }
}

pub(super) fn simulate(
    codec: Codec,
    seed: u64,
    frames: usize,
    loss: f64,
    events: bool,
    unsafe_h264_refresh: bool,
) -> Option<Stats> {
    let config = VideoConfig {
        width: size().0 as u32,
        height: size().1 as u32,
        fps: 60,
        bitrate_kbps: env("LATCH_VT_SIM_KBPS", 3000),
        codec,
        color: Default::default(),
    };
    let mut encoder = Encoder::open(&config, true, Depth::Eight, true).unwrap();
    if !encoder.recovers() {
        return None;
    }
    encoder.unsafe_h264_refresh = unsafe_h264_refresh;
    encoder.output.output.lock().tap = Some(Vec::new());
    let pool = pool();
    let mut viewer = Viewer {
        rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
        loss,
        burst: 0,
        broken: false,
        asked_at: 0,
        last_delivered: 0,
        has: HashSet::new(),
        requests: VecDeque::new(),
        decoder: latch_stream::video::new_decoder(codec).unwrap(),
        full: latch_stream::video::new_decoder(codec).unwrap(),
        truth: HashMap::new(),
        full_stream: Vec::new(),
        loss_stream: Vec::new(),
        stats: Stats::default(),
    };
    for n in 0..frames {
        while viewer.requests.front().is_some_and(|&(at, _)| at <= n) {
            let (_, base) = viewer.requests.pop_front().unwrap();
            if encoder.recover_from(base) {
                viewer.stats.refreshes += 1;
                let tokens = encoder.output.output.lock().tokens.clone();
                let lost = encoder.acknowledged.iter().any(|token| {
                    tokens
                        .iter()
                        .any(|(p, t)| t == token && !viewer.has.contains(p))
                });
                viewer.stats.lost_token_acks += lost as usize;
            } else {
                viewer.stats.refused += 1;
                encoder.request_keyframe();
            }
        }
        if events {
            if viewer.rng.chance(0.01) {
                encoder.request_keyframe();
            }
            if viewer.rng.chance(0.03) {
                let kbps = config.bitrate_kbps / 3 + (viewer.rng.next() % 6000) as u32;
                encoder.bitrate(kbps).unwrap();
            }
        }
        while encoder.in_flight() >= super::super::super::IN_FLIGHT {
            viewer.drain(&mut encoder, n, Duration::from_secs(1));
        }
        encoder
            .submit(pool[n % pool.len()].0, Instant::now(), n == 0)
            .unwrap();
        viewer.drain(&mut encoder, n, Duration::ZERO);
    }
    while encoder.in_flight() > 0 {
        viewer.drain(&mut encoder, frames, Duration::from_secs(1));
    }
    viewer.ffmpeg(codec, &format!("{codec:?}-{seed}-{events}"));
    Some(viewer.stats)
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `LATCH_VT_SIM_SEEDS`, `LATCH_VT_SIM_FRAMES` and `LATCH_VT_SIM_LOSS`
/// scale it up (the 2026-11-04 measurements: 8 seeds of 600 frames).
#[test]
fn ltr_pipeline_with_losses_matches_uninterrupted_decoding() {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let seeds: u64 = env("LATCH_VT_SIM_SEEDS", 3);
    let frames: usize = env("LATCH_VT_SIM_FRAMES", 300);
    let loss: f64 = env("LATCH_VT_SIM_LOSS", 0.03);
    let mut bad = Vec::new();
    let mut total = Stats::default();
    for codec in [Codec::H264High, Codec::Hevc] {
        for seed in 1..=seeds {
            for events in [false, true] {
                let Some(stats) = simulate(codec, seed, frames, loss, events, false) else {
                    eprintln!("{codec:?}: no LTR here");
                    return;
                };
                eprintln!("SIM {codec:?} seed {seed} events {events}: {stats:?}");
                if stats.mismatches + stats.decode_errors + stats.lost_token_acks > 0
                    || stats.ffmpeg.is_some()
                {
                    bad.push(format!("{codec:?} seed {seed} events {events}: {stats:?}"));
                }
                total.emitted += stats.emitted;
                total.accepted += stats.accepted;
                total.refreshes += stats.refreshes;
                total.refused += stats.refused;
                total.recovery_frames += stats.recovery_frames;
                total.idrs += stats.idrs;
            }
        }
    }
    eprintln!("SIM total: {total:?}");
    assert!(
        total.recovery_frames > 0,
        "no loss was recovered by a refresh"
    );
    assert!(bad.is_empty(), "{bad:#?}");
}

/// The refresh `refresh_safe` withholds, sent anyway: after a loss of
/// pictures with long-term marks the viewer's references overflow, which
/// FFmpeg reports and Apple's decoder may answer by blocking (the decoder's
/// watchdog turns that into an error).
#[test]
#[ignore = "demonstrates the H.264 stream the host no longer sends"]
fn unsafe_h264_refresh_overflows_the_viewer_references() {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let seeds: u64 = env("LATCH_VT_SIM_SEEDS", 8);
    let frames: usize = env("LATCH_VT_SIM_FRAMES", 400);
    let mut broken = 0;
    for seed in 1..=seeds {
        let Some(stats) = simulate(Codec::H264High, seed, frames, 0.03, true, true) else {
            return;
        };
        eprintln!("UNSAFE seed {seed}: {stats:?}");
        broken += (stats.ffmpeg.is_some() || stats.decode_errors > 0) as usize;
    }
    assert!(broken > 0);
}

/// A low-latency session that falls behind is replaced by an ordinary one
/// that keeps the frame rate; it no longer offers LTR recovery, and its
/// first frame is an IDR.
#[test]
fn a_low_latency_session_that_misses_its_pace_reopens_ordinary() {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let config = VideoConfig {
        width: 320,
        height: 192,
        fps: 60,
        bitrate_kbps: 3000,
        codec: Codec::Hevc,
        color: Default::default(),
    };
    let mut encoder = Encoder::open(&config, true, Depth::Eight, true).unwrap();
    if !encoder.low_latency() {
        return;
    }
    assert!(!encoder.misses_pace(), "a fresh window is not judged");
    {
        let mut out = encoder.output.output.lock();
        out.pace.since = Instant::now() - PACE_WINDOW;
        out.pace.frames = 120;
        out.pace.late = 5;
    }
    assert!(
        !encoder.misses_pace(),
        "one late frame in 24 is within pace"
    );
    {
        let mut out = encoder.output.output.lock();
        out.pace.since = Instant::now() - PACE_WINDOW;
        out.pace.frames = 120;
        out.pace.late = 7;
    }
    assert!(encoder.misses_pace());
    let pool = pool();
    let first = encoder
        .encode(pool[0].0, Instant::now(), true)
        .unwrap()
        .unwrap();
    encoder.reopen_ordinary().unwrap();
    assert!(!encoder.low_latency() && !encoder.recovers() && !encoder.misses_pace());
    let next = encoder
        .encode(pool[1].0, Instant::now(), false)
        .unwrap()
        .unwrap();
    assert!(next.keyframe && next.picture > first.picture);
}

/// EnableLowLatencyRateControl stops giving frames back at 3024×1964@120
/// H.264 on an M3 Pro (frames stay in flight, no callbacks, idle CPU). The
/// watchdog sees it within [`STALL`] and an ordinary session takes over.
#[test]
#[ignore = "hardware stall reproduction; seconds of full-load encoding"]
fn a_stalled_low_latency_session_is_replaced() {
    let _serial = ENCODER_TEST.lock().unwrap_or_else(|e| e.into_inner());
    let (width, height) = (3024, 1964);
    let config = VideoConfig {
        width,
        height,
        fps: 120,
        bitrate_kbps: 100_000,
        codec: Codec::H264High,
        color: Default::default(),
    };
    let mut encoder = Encoder::open(&config, true, Depth::Eight, true).unwrap();
    let (w, h) = (width as usize, height as usize);
    // realtime_benchmark's picture, which stalled there.
    let mut base = vec![128u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut hash = (x as u32).wrapping_mul(0x45d9f3b) ^ (y as u32).wrapping_mul(0x119de1f3);
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x45d9f3b);
            hash ^= hash >> 16;
            let noise = (hash % 17) as i32 - 8;
            base[y * w + x] = (48 + ((x / 8 + y / 8) % 128) as i32 + noise).clamp(16, 235) as u8;
        }
    }
    let images: Vec<Owned> = (0..12)
        .map(|f| {
            let image = create_surface_sized(w, h);
            fill_surface_pattern(&image, &base, f, w, h);
            image
        })
        .collect();
    let mut pacer = crate::video::Pacer::new(config.fps);
    let start = Instant::now();
    let (mut stalls, mut after, mut n) = (0, 0, 0);
    let mut first_after = None;
    while start.elapsed() < Duration::from_secs(8) {
        if encoder.stalled() {
            stalls += 1;
            eprintln!(
                "stall {stalls} at {:?} (low latency {})",
                start.elapsed(),
                encoder.low_latency()
            );
            encoder.reopen_ordinary().unwrap();
        }
        while let Some((_, _, frame)) = encoder.collect(Duration::ZERO) {
            if stalls > 0 {
                let frame = frame.unwrap().unwrap();
                first_after.get_or_insert(frame.keyframe);
                after += 1;
            }
        }
        let due = pacer.due().is_none_or(|at| Instant::now() >= at);
        if due && encoder.in_flight() < super::super::super::IN_FLIGHT {
            pacer.taken(Instant::now());
            encoder
                .submit(images[n % images.len()].0, Instant::now(), n == 0)
                .unwrap();
            n += 1;
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    eprintln!("{n} submitted, {stalls} stalls, {after} frames after the first");
    if stalls > 0 {
        assert_eq!(first_after, Some(true), "the replacement starts at an IDR");
        assert!(after > 100, "the ordinary session keeps going");
    }
}
