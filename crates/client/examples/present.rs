//! Native viewer measurement, using the application's renderer and present settings.
//! present [host] --port 47882 --width 1920 --height 1080 --fps 120 --seconds 15
//! present --replay stream.hevc --codec hevc --width 3840 --height 2160 --fps 60 [--cpu]
use latch_stream::{Event, FrameSlot, Session, Settings};
use std::{
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

struct Viewer {
    session: Option<Session>,
    replay: Option<(
        Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<Vec<f64>>,
    )>,
    frames: Arc<FrameSlot>,
    events: mpsc::Receiver<Event>,
    start: Instant,
    seconds: u64,
    last_stats: Instant,
}
impl eframe::App for Viewer {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        for event in self.events.try_iter() {
            eprintln!("{event:?}");
        }
        if self.last_stats.elapsed() >= Duration::from_secs(1) {
            if let Some(session) = &self.session {
                eprintln!("stats {:?}", session.stats());
            }
            self.last_stats = Instant::now();
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                    ui.max_rect(),
                    latch_client::video::Paint {
                        frames: self.frames.clone(),
                    },
                ));
            });
        if self.start.elapsed() >= Duration::from_secs(self.seconds) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(
            Duration::from_secs(self.seconds).saturating_sub(self.start.elapsed()),
        );
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            session.stop();
        }
        if let Some((stop, worker)) = self.replay.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut times = worker.join().unwrap();
            times.sort_by(f64::total_cmp);
            if !times.is_empty() {
                eprintln!(
                    "decode_summary frames={} p50_ms={:.3} p95_ms={:.3}",
                    times.len(),
                    times[times.len() / 2],
                    times[(times.len() - 1) * 95 / 100]
                );
            }
        }
        let mut times: Vec<f64> = self
            .frames
            .render_times()
            .iter()
            .map(|t| t.as_secs_f64() * 1000.0)
            .collect();
        times.sort_by(f64::total_cmp);
        let percentile = |p: f64| {
            times
                .get(((times.len().saturating_sub(1)) as f64 * p) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        eprintln!("render_summary decoded={} rendered={} decode_to_render_ms_p50={:.3} p95={:.3} p99={:.3}", self.frames.seq(), times.len(), percentile(0.5), percentile(0.95), percentile(0.99));
    }
}
/// Split a replay file into access units: at H.264/HEVC access unit
/// delimiters, or at AV1 temporal delimiters (which are dropped, as in
/// ISOBMFF samples). A file without delimiters is one access unit, so a
/// single IDR replays as before. The first unit must be a keyframe.
fn access_units(codec: latch_stream::Codec, bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    if codec == latch_stream::Codec::Av1 {
        let mut units = Vec::new();
        let (mut at, mut unit) = (0, 0);
        while at < bytes.len() {
            let header = bytes[at];
            let mut end = at + 1 + usize::from(header & 4 != 0);
            let mut size = 0usize;
            for i in 0..8 {
                let Some(&byte) = bytes.get(end) else {
                    return vec![bytes];
                };
                end += 1;
                size |= usize::from(byte & 127) << (i * 7);
                if byte & 128 == 0 {
                    break;
                }
            }
            end += size;
            if (header >> 3) & 15 == 2 {
                if at > unit {
                    units.push(&bytes[unit..at]);
                }
                unit = end;
            }
            at = end;
        }
        if bytes.len() > unit {
            units.push(&bytes[unit.min(bytes.len())..]);
        }
        return units;
    }
    for i in 0..bytes.len().saturating_sub(4) {
        if bytes[i..i + 3] == [0, 0, 1] {
            let delimiter = match codec {
                latch_stream::Codec::Hevc => (bytes[i + 3] >> 1) & 63 == 35,
                _ => bytes[i + 3] & 31 == 9,
            };
            if delimiter {
                starts.push(if i > 0 && bytes[i - 1] == 0 { i - 1 } else { i });
            }
        }
    }
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts.push(bytes.len());
    starts.windows(2).map(|w| &bytes[w[0]..w[1]]).collect()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|w| w[0] == name)
            .map_or(default.to_string(), |w| w[1].clone())
    };
    let width = arg("--width", "1920").parse()?;
    let height = arg("--height", "1080").parse()?;
    let fps = arg("--fps", "120").parse()?;
    let seconds = arg("--seconds", "15").parse()?;
    let server = latch_stream::Server {
        address: args
            .get(1)
            .filter(|s| !s.starts_with('-'))
            .map_or("127.0.0.1", String::as_str)
            .parse()?,
        port: arg("--port", "47882").parse()?,
    };
    let replay = if args.iter().any(|a| a == "--replay") {
        Some(std::fs::read(arg("--replay", ""))?)
    } else {
        None
    };
    let codec = match arg("--codec", "h264-high").as_str() {
        "av1" => latch_stream::Codec::Av1,
        "hevc" => latch_stream::Codec::Hevc,
        "h264-baseline" => latch_stream::Codec::H264Baseline,
        "h264-high" => latch_stream::Codec::H264High,
        _ => anyhow::bail!("unknown codec"),
    };
    eprintln!("decodable {:?}", latch_stream::video::decodable());
    let mut options = latch_client::native_options();
    options.viewport = options
        .viewport
        .with_inner_size([width as f32 / 2.0, height as f32 / 2.0])
        .with_active(false);
    eframe::run_native(
        "Latch present measurement",
        options,
        Box::new(move |cc| {
            let render_state = cc.wgpu_render_state.as_ref().unwrap();
            let adapter = render_state.adapter.get_info();
            eprintln!("renderer {:?} {}", adapter.backend, adapter.name);
            latch_client::video::install(render_state);
            // `--cpu`: copy every picture through the CPU, as before zero-copy.
            #[cfg(windows)]
            if args.iter().any(|a| a == "--cpu") {
                latch_stream::video::render_on_d3d12(None);
            }
            let frames = Arc::new(FrameSlot::default());
            frames.measure_render();
            let (tx, events) = mpsc::channel();
            let ctx = cc.egui_ctx.clone();
            let (session, replay) = if let Some(bytes) = replay {
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let done = stop.clone();
                let slot = frames.clone();
                let format = if args.iter().any(|a| a == "--p010") {
                    latch_stream::video::PixelFormat::P010
                } else {
                    latch_stream::video::PixelFormat::Nv12
                };
                let worker = std::thread::spawn(move || {
                    let decoder = if format == latch_stream::video::PixelFormat::Nv12 {
                        latch_stream::video::new_decoder_for_stream(codec, width, height)
                    } else {
                        latch_stream::video::new_decoder_with_format(codec, format)
                    };
                    let mut decoder = match decoder {
                        Ok(decoder) => decoder,
                        Err(error) => {
                            eprintln!("decoder setup failed: {error:#}");
                            return Vec::new();
                        }
                    };
                    let units = access_units(codec, &bytes);
                    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
                    let mut next = Instant::now();
                    let mut times = Vec::new();
                    for (index, unit) in units.iter().enumerate().cycle() {
                        if done.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        let start = Instant::now();
                        match decoder.decode(unit, index == 0, slot.spare()) {
                            Ok(Some(mut frame)) => {
                                times.push(start.elapsed().as_secs_f64() * 1000.0);
                                frame.decoded_at = Some(Instant::now());
                                slot.publish(frame);
                                ctx.request_repaint();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("decode failed: {e:#}");
                                break;
                            }
                        }
                        next += interval;
                        std::thread::sleep(next.saturating_duration_since(Instant::now()));
                    }
                    eprintln!("decoder {}", decoder.name());
                    times
                });
                (None, Some((stop, worker)))
            } else {
                (
                    Some(Session::start(
                        server,
                        Settings {
                            width,
                            height,
                            fps,
                            bitrate_kbps: 50_000,
                            hevc: !args.iter().any(|a| a == "--h264"),
                            av1: !args.iter().any(|a| a == "--h264"),
                            audio: false,
                            name: "present measurement".into(),
                            cursor: false,
                        },
                        frames.clone(),
                        tx,
                        move || ctx.request_repaint(),
                    )),
                    None,
                )
            };
            Ok(Box::new(Viewer {
                session,
                replay,
                frames,
                events,
                start: Instant::now(),
                last_stats: Instant::now(),
                seconds,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
