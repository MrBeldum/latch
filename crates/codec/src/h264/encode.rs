//! A desktop encoder with one independently predicted slice per worker band.

mod bits;
mod cavlc;
mod deblock;
mod effort;
mod predict;
mod transform;
mod worker;

use super::CodecError;
use crate::color::Rect;
use crate::Yuv420;
use bits::Bits;
use std::collections::VecDeque;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Worker threads; 0 picks the number of CPUs (at most 8).
    pub threads: usize,
}

/// Encoding configuration with an explicit real-time budget.
/// Construct with [`EncoderConfig::with_frame_budget`].
#[derive(Debug, Clone, PartialEq)]
pub struct EncoderOptions {
    config: EncoderConfig,
    frame_budget_percent: u8,
}
impl EncoderConfig {
    /// Encode-time budget as a percentage of `1 / fps` (default 65).
    /// Zero disables adaptation for deterministic tests and offline encoding.
    /// Values above 100 are rejected by `Encoder::new`.
    pub fn with_frame_budget(self, percent: u8) -> EncoderOptions {
        EncoderOptions {
            config: self,
            frame_budget_percent: percent,
        }
    }
}
impl From<EncoderConfig> for EncoderOptions {
    fn from(config: EncoderConfig) -> Self {
        config.with_frame_budget(65)
    }
}

/// Wall times for the last successful encode, including rate-control retries.
#[derive(Debug, Clone, Default)]
pub struct EncodeTiming {
    pub elapsed_ms: f64,
    pub preparation_ms: f64,
    pub band_ms: Vec<f64>,
    /// Sum of filtering wall times across workers (parallel, not frame latency).
    pub deblock_ms: f64,
    pub passes: u32,
    /// 0 is full quality; 4 is minimum compression effort.
    pub effort: u8,
    /// Recent nearest-rank p95 at this effort; zero with adaptation off.
    pub p95_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedInfo {
    pub keyframe: bool,
    /// Average QP of the coded macroblocks.
    pub qp: u8,
    pub skipped_mbs: u32,
    pub total_mbs: u32,
    pub bytes: usize,
    /// This frame's number, +1 per access unit from 1; what
    /// [`Encoder::recover_from`] names it by.
    pub picture: u64,
    /// The picture this frame predicts from when it is the first after
    /// [`Encoder::recover_from`]: decodable by a decoder that holds that
    /// picture and nothing after it.
    pub recovery: Option<u64>,
}

/// A decoder's state after one picture: what the encoder rolls back to
/// when the frames after it are lost.
struct Snapshot {
    picture: u64,
    reconstruction: Arc<Yuv420>,
    source: Arc<Yuv420>,
    quality: Arc<Vec<u8>>,
    /// `frame_num` and `idr_pic_id` for the frame that follows.
    frame_num: u32,
    idr_id: u32,
}

enum Task {
    Prepare {
        source: Arc<Yuv420>,
        previous: Option<Arc<Yuv420>>,
        scroll: bool,
        first: usize,
        end: usize,
    },
    Encode(worker::Job),
}

struct Worker {
    tx: Option<mpsc::Sender<Task>>,
    rx: mpsc::Receiver<worker::Band>,
    prepared_rx: mpsc::Receiver<worker::Prepared>,
    handle: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct Encoder {
    config: EncoderConfig,
    governor: effort::Governor,
    timing: EncodeTiming,
    workers: Vec<Worker>,
    reconstruction: Arc<Yuv420>,
    source: Option<Arc<Yuv420>>,
    /// Pictures no frame refers to any more, reused rather than allocated.
    spare_reconstruction: Option<Yuv420>,
    spare_source: Option<Yuv420>,
    /// `spare_source` is the picture before the last one given, so only
    /// what the last two changed needs copying into it.
    spare_source_recent: bool,
    /// `spare_reconstruction` is the frame before `reconstruction`, and
    /// `kept` says where the two are the same.
    spare_is_previous: bool,
    kept: Option<Arc<Vec<bool>>>,
    /// Macroblocks the last picture may have changed, when the caller said.
    last_dirty: Option<Arc<Vec<bool>>>,
    /// False after a picture that was given but refused: the next one's
    /// changes are from it, not from the last picture encoded.
    hints_hold: bool,
    quality: Arc<Vec<u8>>,
    frame_num: u32,
    idr_id: u32,
    qp: u8,
    model: f64,
    subpixel: bool,
    refresh_start: usize,
    window: VecDeque<usize>,
    /// The last `depth` pictures, oldest first, while recovery is on.
    history: VecDeque<Snapshot>,
    depth: usize,
    picture: u64,
    /// The next frame is the first after a roll back to this picture.
    recovering: Option<u64>,
}

impl Encoder {
    pub fn new(options: impl Into<EncoderOptions>) -> Result<Self, CodecError> {
        let EncoderOptions {
            config,
            frame_budget_percent,
        } = options.into();
        if frame_budget_percent > 100 {
            return Err(CodecError::Config("frame budget must be 0..=100 percent"));
        }
        if config.width == 0
            || config.height == 0
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
            || config.width > 8192
            || config.height > 8192
        {
            return Err(CodecError::Config(
                "dimensions must be even, nonzero, and at most 8192",
            ));
        }
        if config.fps == 0 || config.fps > 1000 || config.bitrate_kbps == 0 {
            return Err(CodecError::Config(
                "fps must be 1..=1000 and bitrate must be nonzero",
            ));
        }
        level(&config)?;
        let reconstruction = Arc::new(Yuv420::new(config.width, config.height));
        let rows = reconstruction.padded_height() / 16;
        let threads = if config.threads == 0 {
            thread::available_parallelism()
                .map_or(1, usize::from)
                .min(8)
        } else {
            config.threads.min(64)
        }
        .min(rows);
        let mut workers = Vec::new();
        for i in 0..threads {
            let (tx, rx) = mpsc::channel();
            let (out_tx, out_rx) = mpsc::channel();
            let (prepared_tx, prepared_rx) = mpsc::channel();
            let handle = thread::Builder::new()
                .name(format!("h264-{i}"))
                .spawn(move || {
                    #[cfg(not(test))]
                    crate::priority::media_thread();
                    while let Ok(task) = rx.recv() {
                        let sent = match task {
                            Task::Encode(job) => out_tx.send(worker::encode(job)).is_ok(),
                            Task::Prepare {
                                source,
                                previous,
                                scroll,
                                first,
                                end,
                            } => prepared_tx
                                .send(worker::prepare(
                                    &source,
                                    previous.as_deref(),
                                    scroll,
                                    first,
                                    end,
                                ))
                                .is_ok(),
                        };
                        if !sent {
                            break;
                        }
                    }
                })
                .map_err(|_| CodecError::Config("cannot start encoder worker"))?;
            workers.push(Worker {
                tx: Some(tx),
                rx: out_rx,
                prepared_rx,
                handle: Some(handle),
            });
        }
        let total = reconstruction.padded_width() / 16 * rows;
        Ok(Self {
            governor: effort::Governor::new(config.fps, frame_budget_percent),
            timing: EncodeTiming::default(),
            config,
            workers,
            reconstruction,
            source: None,
            spare_reconstruction: None,
            spare_source: None,
            spare_source_recent: false,
            spare_is_previous: false,
            kept: None,
            last_dirty: None,
            hints_hold: true,
            quality: Arc::new(vec![45; total]),
            frame_num: 0,
            idr_id: 0,
            qp: 30,
            model: 0.,
            subpixel: true,
            refresh_start: 0,
            window: VecDeque::new(),
            history: VecDeque::new(),
            depth: 0,
            picture: 0,
            recovering: None,
        })
    }

    /// Keeps the last `depth` pictures (0, the default, keeps none) so
    /// that [`Encoder::recover_from`] can make the next frame predict
    /// from any of them. Each costs a reconstruction and a source
    /// picture of memory while it is held.
    pub fn set_recovery_depth(&mut self, depth: usize) {
        self.depth = depth;
        while self.history.len() > depth {
            self.forget_oldest();
        }
    }

    /// After a loss: the next frame predicts from `picture` (an
    /// [`EncodedInfo::picture`]) as if no frame after it had been sent,
    /// so a decoder that stopped at `picture` decodes it exactly. False,
    /// and nothing changes, when `picture` is no longer held.
    ///
    /// Every frame is a reference and only the last is ever used, so the
    /// recovery frame takes the `frame_num` after `picture`'s: to the
    /// decoder the frames it never decoded never existed, and the stream
    /// it sees is an ordinary one.
    pub fn recover_from(&mut self, picture: u64) -> bool {
        let Some(at) = self.history.iter().position(|s| s.picture == picture) else {
            return false;
        };
        // What came after it is a branch no decoder will hold.
        self.history.truncate(at + 1);
        let s = &self.history[at];
        self.reconstruction = s.reconstruction.clone();
        self.source = Some(s.source.clone());
        self.quality = s.quality.clone();
        self.frame_num = s.frame_num;
        self.idr_id = s.idr_id;
        // The hints and the kept macroblocks describe the abandoned frames.
        self.kept = None;
        self.spare_is_previous = false;
        self.spare_source_recent = false;
        self.last_dirty = None;
        self.hints_hold = false;
        self.recovering = Some(picture);
        true
    }

    /// Records the decoder's state after the frame just made.
    fn remember(&mut self) -> (u64, Option<u64>) {
        self.picture += 1;
        let recovery = self.recovering.take();
        if self.depth > 0 {
            if let Some(source) = self.source.clone() {
                if self.history.len() >= self.depth {
                    self.forget_oldest();
                }
                self.history.push_back(Snapshot {
                    picture: self.picture,
                    reconstruction: self.reconstruction.clone(),
                    source,
                    quality: self.quality.clone(),
                    frame_num: self.frame_num,
                    idr_id: self.idr_id,
                });
            }
        }
        (self.picture, recovery)
    }

    /// Drops the oldest picture held, keeping its planes as spares.
    fn forget_oldest(&mut self) {
        let Some(old) = self.history.pop_front() else {
            return;
        };
        if self.spare_reconstruction.is_none() {
            self.spare_reconstruction = Arc::try_unwrap(old.reconstruction).ok();
            self.spare_is_previous = false;
        }
        if self.spare_source.is_none() {
            self.spare_source = Arc::try_unwrap(old.source).ok();
            self.spare_source_recent = false;
        }
    }

    /// Encodes `picture` (the configured visible size, padded) as one
    /// access unit appended to `out`. The first frame, and any frame with
    /// `keyframe`, is an IDR preceded by SPS and PPS.
    pub fn encode(
        &mut self,
        picture: &Yuv420,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedInfo, CodecError> {
        self.encode_changed(picture, None, keyframe, out)
    }

    /// [`Encoder::encode`], told where `picture` may differ from the last
    /// picture given: outside `changed` (pixel rectangles) the caller
    /// promises it is the same. That spares comparing and copying what
    /// did not change; the access unit is the one `encode` would make.
    /// `None` promises nothing.
    pub fn encode_changed(
        &mut self,
        picture: &Yuv420,
        changed: Option<&[Rect]>,
        keyframe: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodedInfo, CodecError> {
        self.validate(picture)?;
        let dirty = changed
            .filter(|_| self.hints_hold)
            .map(|rects| Arc::new(dirty_map(picture, rects)));
        self.hints_hold = true;
        let started = Instant::now();
        self.timing = EncodeTiming {
            effort: self.governor.level,
            band_ms: vec![0.; self.workers.len()],
            ..EncodeTiming::default()
        };
        let idr = keyframe || self.source.is_none();
        // A hint can name a change that left the picture as it was (a
        // cursor redrawn in place): what it names is compared, so a hint
        // changes the work, never the access unit.
        let unchanged = || match (&dirty, self.source.as_deref()) {
            (Some(d), Some(source)) => {
                !d.contains(&true)
                    || (same_layout(source, picture)
                        && macroblocks_equal(source, picture, |i| d[i]))
            }
            (Some(d), None) => !d.contains(&true),
            (None, source) => source == Some(picture),
        };
        if !idr && self.quality.iter().all(|&q| q <= 18) && unchanged() {
            let result = self.encode_settled(out)?;
            self.finish_timing(started);
            return Ok(result);
        }
        let frame_num = if idr { 0 } else { self.frame_num };
        // The spare is the picture before the last: only what either of
        // the last two changed needs copying into it.
        let source = Arc::new(match self.spare_source.take() {
            Some(mut spare) => {
                match (&dirty, &self.last_dirty) {
                    (Some(now), Some(before))
                        if self.spare_source_recent && same_layout(&spare, picture) =>
                    {
                        copy_macroblocks(&mut spare, picture, |i| now[i] || before[i])
                    }
                    _ => spare.copy_from(picture),
                }
                spare
            }
            None => picture.clone(),
        });
        let budget = self.config.bitrate_kbps as f64 * 1000. / self.config.fps as f64;
        // Scroll detection, the change count and the complexity estimate
        // read every band of the picture: the slice workers do them.
        let rows = picture.padded_height() / 16;
        let bands = self.workers.len();
        let prepared = self.round_trip(
            |i| Task::Prepare {
                source: source.clone(),
                previous: self.source.clone(),
                scroll: !idr,
                first: i * rows / bands,
                end: (i + 1) * rows / bands,
            },
            |worker| worker.prepared_rx.recv().ok(),
        )?;
        let complexity = prepared.iter().map(|p| p.complexity).sum::<u64>() as f64;
        let changed_mbs = prepared.iter().map(|p| p.changed_mbs).sum::<usize>();
        let globals: Vec<_> = prepared.iter().map(|p| p.global).collect();
        let debt =
            self.window.iter().sum::<usize>() as f64 * 8. - budget * self.window.len() as f64;
        let target = (budget - debt * 0.5).clamp(budget * 0.5, budget * 1.5);
        let proposed = if self.model > 0. {
            6. * (self.model * complexity / target).max(1e-6).log2() + 24.
        } else {
            // Start from bits per pixel rather than over-quantizing smaller/high-bitrate pictures.
            8. + 6. * (picture.width as f64 * picture.height as f64 / budget).log2()
        };
        let lo = if self.source.is_none() {
            18
        } else {
            self.qp.saturating_sub(4).max(18)
        };
        let hi = if self.source.is_none() {
            51
        } else {
            (self.qp + 4).min(51)
        };
        let mut qp = (proposed.round() as u8).clamp(lo, hi);
        // Exhaust normal quantization before dropping individual coefficients.
        let mut suppression = if qp == 51 {
            ((proposed - 51.).max(0.) / 6.).floor().min(10.) as u8
        } else {
            0
        };
        let total_mbs = rows * picture.padded_width() / 16;
        let is_motion = changed_mbs * 2 > total_mbs;
        // At high QP fractional search costs more than its quality gain; also skip tiny updates and scrolls.
        let refine_motion = self.governor.level == 0
            && self.subpixel
            && qp < 38
            && changed_mbs * 32 > total_mbs
            && globals.contains(&(0, 0));
        let half_pel = if !idr && refine_motion {
            Some(Arc::new(predict::compute_half_pel(
                &self.reconstruction.y,
                self.reconstruction.y_stride,
                self.reconstruction.padded_width(),
                self.reconstruction.padded_height(),
            )))
        } else {
            None
        };
        self.timing.preparation_ms = started.elapsed().as_secs_f64() * 1000.;
        let kept = self.kept.take().filter(|_| self.spare_is_previous && !idr);
        let mut next = self
            .spare_reconstruction
            .take()
            .unwrap_or_else(|| Yuv420::new(self.config.width, self.config.height));
        self.spare_is_previous = false;
        let target_rows = worker::Target::new(&mut next);
        let (data, mbs, skipped, qp_sum) = loop {
            self.timing.passes += 1;
            let width = picture.padded_width() / 16;
            let refresh = (budget / 128.).max(1.) as usize;
            let bands = self.round_trip(
                |i| {
                    Task::Encode(worker::Job {
                        source: source.clone(),
                        previous: self.source.clone(),
                        reference: self.reconstruction.clone(),
                        target: target_rows,
                        dirty: dirty.clone(),
                        // A retry rewrites what the first pass left.
                        kept: kept.clone().filter(|_| self.timing.passes == 1),
                        half_pel: half_pel.clone(),
                        quality: self.quality.clone(),
                        first: i * rows / self.workers.len(),
                        end: (i + 1) * rows / self.workers.len(),
                        qp,
                        suppression,
                        effort: self.governor.level,
                        subpixel: half_pel.is_some() && qp < 38,
                        is_motion,
                        global: globals[i],
                        idr,
                        frame_num,
                        idr_id: self.idr_id,
                        refresh,
                        refresh_start: self.refresh_start,
                    })
                },
                |worker| worker.rx.recv().ok(),
            )?;
            let mut data = Vec::new();
            if idr {
                parameter_sets(&self.config, &mut data)?;
            }
            let mut mbs = Vec::with_capacity(width * rows);
            let mut skipped = 0;
            let mut qp_sum = 0;
            for (i, band) in bands.into_iter().enumerate() {
                self.timing.band_ms[i] += band.elapsed_ms;
                self.timing.deblock_ms += band.deblock_ms;
                data.extend_from_slice(&band.nal);
                skipped += band.skipped;
                qp_sum += band.qp_sum;
                mbs.extend(band.mbs);
            }
            // Under pressure, spend the next frame's feedback correcting a soft
            // bitrate overshoot instead of encoding this frame repeatedly. The
            // hard access-unit bound below still applies at every effort.
            let bits = data.len() as f64 * 8.;
            let soft_retry = self.governor.level < 2 || idr;
            if bits > budget * 8.
                || (soft_retry && bits > if idr { budget * 4. } else { target * 1.3 })
            {
                if qp < 51 {
                    let step = (6.
                        * (data.len() as f64 * 8. / if idr { budget * 3.5 } else { target }).log2())
                    .ceil() as u8;
                    qp = (qp + step.clamp(1, 12)).min(51);
                    continue;
                }
                if suppression < 10 {
                    suppression += 1;
                    continue;
                }
            }
            // Refusing an impossible access unit preserves the reference and the caller's output.
            if data.len() as f64 * 8. > budget * 8. {
                self.spare_reconstruction = Some(next);
                self.hints_hold = false;
                self.last_dirty = None;
                return Err(CodecError::Config(
                    "frame exceeds eight bitrate intervals at the allowed QP",
                ));
            }
            break (data, mbs, skipped, qp_sum);
        };
        // Filtering was completed independently by each slice worker.
        let old = std::mem::replace(&mut self.reconstruction, Arc::new(next));
        self.spare_reconstruction = Arc::try_unwrap(old).ok();
        self.spare_is_previous = self.spare_reconstruction.is_some();
        self.kept = Some(Arc::new(mbs.iter().map(|m| m.kept).collect()));
        self.last_dirty = dirty;
        if let Some(old) = self.source.replace(source) {
            self.spare_source = Arc::try_unwrap(old).ok();
            self.spare_source_recent = self.spare_source.is_some();
        }
        self.quality = Arc::new(mbs.iter().map(|m| m.quality).collect());
        self.frame_num = (frame_num + 1) & 65535;
        if idr {
            self.idr_id ^= 1;
        }
        self.qp = qp;
        let measurement =
            data.len() as f64 * 8. * 2f64.powf((qp as f64 + suppression as f64 * 6. - 24.) / 6.)
                / complexity;
        self.model = if self.model == 0. {
            measurement
        } else {
            self.model * 0.2 + measurement * 0.8
        };
        self.refresh_start =
            (self.refresh_start + (budget / 128.).max(1.) as usize) % self.quality.len();
        self.window.push_back(data.len());
        if self.window.len() > self.config.fps as usize {
            self.window.pop_front();
        }
        let bytes = data.len();
        out.extend_from_slice(&data);
        let (picture, recovery) = self.remember();
        self.finish_timing(started);
        Ok(EncodedInfo {
            keyframe: idr,
            qp: (qp_sum / (mbs.len() as u32)) as u8,
            skipped_mbs: skipped,
            total_mbs: mbs.len() as u32,
            bytes,
            picture,
            recovery: recovery.filter(|_| !idr),
        })
    }

    /// Hands every worker its task and collects every answer, in band order.
    /// Answers are awaited even after a failure: a band still running may be
    /// writing into the picture being reconstructed.
    fn round_trip<T>(
        &self,
        task: impl Fn(usize) -> Task,
        answer: impl Fn(&Worker) -> Option<T>,
    ) -> Result<Vec<T>, CodecError> {
        let sent: Vec<bool> = self
            .workers
            .iter()
            .enumerate()
            .map(|(i, worker)| {
                worker
                    .tx
                    .as_ref()
                    .is_some_and(|tx| tx.send(task(i)).is_ok())
            })
            .collect();
        let answers: Vec<_> = self
            .workers
            .iter()
            .zip(&sent)
            .map(|(worker, &sent)| if sent { answer(worker) } else { None })
            .collect();
        answers
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(CodecError::Config("encoder worker stopped"))
    }

    /// Timings and effort actually used, before the next feedback adjustment.
    pub fn timing(&self) -> &EncodeTiming {
        &self.timing
    }

    fn finish_timing(&mut self, started: Instant) {
        self.timing.elapsed_ms = started.elapsed().as_secs_f64() * 1000.;
        self.governor.observe(self.timing.elapsed_ms);
        self.timing.p95_ms = self.governor.p95;
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps.max(1);
    }

    /// Enables adaptive fractional search (on by default); tiny desktop updates and scrolls skip it.
    pub fn set_subpixel_refinement(&mut self, enabled: bool) {
        self.subpixel = enabled;
    }

    /// The picture a decoder holds after the last frame.
    pub fn reconstruction(&self) -> &Yuv420 {
        &self.reconstruction
    }

    /// The last second of encoded frames, in kilobits per second.
    pub fn achieved_bitrate_kbps(&self) -> f64 {
        if self.window.is_empty() {
            0.
        } else {
            self.window.iter().sum::<usize>() as f64 * 8. * self.config.fps as f64
                / self.window.len() as f64
                / 1000.
        }
    }

    fn encode_settled(&mut self, out: &mut Vec<u8>) -> Result<EncodedInfo, CodecError> {
        let rows = self.reconstruction.padded_height() / 16;
        let width = self.reconstruction.padded_width() / 16;
        let mut data = Vec::new();
        for i in 0..self.workers.len() {
            let first = i * rows / self.workers.len() * width;
            let end = (i + 1) * rows / self.workers.len() * width;
            let mut b = worker::header(first, false, self.frame_num, 0, self.qp, true);
            b.ue((end - first) as u32);
            b.nal(0x41, &mut data);
        }
        let budget = self.config.bitrate_kbps as f64 * 1000. / self.config.fps as f64;
        if data.len() as f64 * 8. > budget * 8. {
            return Err(CodecError::Config(
                "skip headers exceed eight bitrate intervals",
            ));
        }
        self.frame_num = (self.frame_num + 1) & 65535;
        self.window.push_back(data.len());
        if self.window.len() > self.config.fps as usize {
            self.window.pop_front();
        }
        let bytes = data.len();
        out.extend_from_slice(&data);
        let total_mbs = (width * rows) as u32;
        let (picture, recovery) = self.remember();
        Ok(EncodedInfo {
            keyframe: false,
            qp: self.qp,
            skipped_mbs: total_mbs,
            total_mbs,
            bytes,
            picture,
            recovery,
        })
    }

    fn validate(&self, p: &Yuv420) -> Result<(), CodecError> {
        if p.width != self.config.width || p.height != self.config.height {
            return Err(CodecError::Config("picture size differs from encoder size"));
        }
        let w = p.padded_width();
        let h = p.padded_height();
        if p.y_stride < w
            || p.c_stride < w / 2
            || p.y_stride.checked_mul(h).is_none_or(|n| p.y.len() < n)
            || p.c_stride
                .checked_mul(h / 2)
                .is_none_or(|n| p.u.len() < n || p.v.len() < n)
        {
            return Err(CodecError::Config(
                "picture planes are not padded to macroblocks",
            ));
        }
        Ok(())
    }
}

/// Which macroblocks of `p` the pixel rectangles touch.
fn dirty_map(p: &Yuv420, rects: &[Rect]) -> Vec<bool> {
    let (w, h) = (p.padded_width() / 16, p.padded_height() / 16);
    let mut map = vec![false; w * h];
    for r in rects.iter().filter(|r| r.width > 0 && r.height > 0) {
        let (x0, y0) = ((r.x as usize / 16).min(w), (r.y as usize / 16).min(h));
        let x1 = ((r.x as usize + r.width as usize).div_ceil(16)).min(w);
        let y1 = ((r.y as usize + r.height as usize).div_ceil(16)).min(h);
        for row in map.chunks_mut(w).take(y1).skip(y0) {
            row[x0..x1].fill(true);
        }
    }
    map
}

fn same_layout(a: &Yuv420, b: &Yuv420) -> bool {
    (
        a.width,
        a.height,
        a.y_stride,
        a.c_stride,
        a.y.len(),
        a.u.len(),
        a.v.len(),
    ) == (
        b.width,
        b.height,
        b.y_stride,
        b.c_stride,
        b.y.len(),
        b.u.len(),
        b.v.len(),
    )
}

/// Copies the macroblocks `copy` picks (by index) from `src` into `dst`,
/// padding included: runs of neighbours in a row as one copy per line.
/// Whether `a` and `b` hold the same samples in every macroblock `which`
/// names (raster order).
fn macroblocks_equal(a: &Yuv420, b: &Yuv420, which: impl Fn(usize) -> bool) -> bool {
    let (w, h) = (b.padded_width() / 16, b.padded_height() / 16);
    (0..h).all(|my| {
        (0..w).filter(|&mx| which(my * w + mx)).all(|mx| {
            [
                (&a.y, &b.y, b.y_stride, 16),
                (&a.u, &b.u, b.c_stride, 8),
                (&a.v, &b.v, b.c_stride, 8),
            ]
            .into_iter()
            .all(|(ap, bp, stride, n)| {
                (my * n..(my + 1) * n).all(|row| {
                    let at = row * stride + mx * n;
                    ap[at..at + n] == bp[at..at + n]
                })
            })
        })
    })
}

fn copy_macroblocks(dst: &mut Yuv420, src: &Yuv420, copy: impl Fn(usize) -> bool) {
    let (w, h) = (src.padded_width() / 16, src.padded_height() / 16);
    for my in 0..h {
        let mut mx = 0;
        while mx < w {
            if !copy(my * w + mx) {
                mx += 1;
                continue;
            }
            let first = mx;
            while mx < w && copy(my * w + mx) {
                mx += 1;
            }
            for (dp, sp, stride, n) in [
                (&mut dst.y, &src.y, src.y_stride, 16),
                (&mut dst.u, &src.u, src.c_stride, 8),
                (&mut dst.v, &src.v, src.c_stride, 8),
            ] {
                for row in my * n..(my + 1) * n {
                    let at = row * stride;
                    dp[at + first * n..at + mx * n]
                        .copy_from_slice(&sp[at + first * n..at + mx * n]);
                }
            }
        }
    }
}

fn level(c: &EncoderConfig) -> Result<u8, CodecError> {
    let fs = (c.width as u64).div_ceil(16) * (c.height as u64).div_ceil(16);
    for (id, mbps, maxfs, kbps) in [
        (30, 40500, 1620, 10000),
        (31, 108000, 3600, 14000),
        (32, 216000, 5120, 20000),
        (40, 245760, 8192, 20000),
        (41, 245760, 8192, 50000),
        (42, 522240, 8704, 50000),
        (50, 589824, 22080, 135000),
        (51, 983040, 36864, 240000),
        (52, 2073600, 36864, 240000),
        (60, 4177920, 139264, 240000),
        (61, 8355840, 139264, 480000),
        (62, 16711680, 139264, 800000),
    ] {
        if fs <= maxfs
            && fs * c.fps as u64 <= mbps
            && c.bitrate_kbps <= kbps
            && (c.width as u64).div_ceil(16).pow(2) <= maxfs * 8
            && (c.height as u64).div_ceil(16).pow(2) <= maxfs * 8
        {
            return Ok(id);
        }
    }
    Err(CodecError::Config(
        "size, fps, or bitrate exceeds supported H.264 levels",
    ))
}
fn parameter_sets(c: &EncoderConfig, out: &mut Vec<u8>) -> Result<(), CodecError> {
    let w = c.width.div_ceil(16);
    let h = c.height.div_ceil(16);
    let mut b = Bits::default();
    b.put(66, 8);
    b.put(0xc0, 8);
    b.put(level(c)? as u32, 8);
    b.ue(0);
    b.ue(12);
    b.ue(2);
    b.ue(1);
    b.bit(false);
    b.ue(w - 1);
    b.ue(h - 1);
    b.bit(true);
    b.bit(true);
    let crop = !c.width.is_multiple_of(16) || !c.height.is_multiple_of(16);
    b.bit(crop);
    if crop {
        b.ue(0);
        b.ue((w * 16 - c.width) / 2);
        b.ue(0);
        b.ue((h * 16 - c.height) / 2);
    }
    b.bit(true);
    b.bit(false);
    b.bit(false);
    b.bit(true);
    b.put(5, 3);
    b.bit(false);
    b.bit(true);
    b.put(1, 8);
    b.put(1, 8);
    b.put(1, 8);
    b.bit(false);
    b.bit(true);
    b.put(1, 32);
    b.put(c.fps * 2, 32);
    b.bit(true);
    b.bit(false);
    b.bit(false);
    b.bit(false);
    b.bit(true);
    b.bit(true);
    b.ue(0);
    b.ue(0);
    b.ue(16);
    b.ue(16);
    b.ue(0);
    b.ue(1);
    b.nal(0x67, out);
    let mut b = Bits::default();
    b.ue(0);
    b.ue(0);
    b.bit(false);
    b.bit(false);
    b.ue(0);
    b.ue(0);
    b.ue(0);
    b.bit(false);
    b.put(0, 2);
    b.se(0);
    b.se(0);
    b.se(0);
    b.bit(true);
    b.bit(false);
    b.bit(false);
    b.nal(0x68, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_hints_change_nothing_but_the_work() {
        // Noise, and smooth pictures at a low rate, where the deblocking
        // filter reaches into macroblocks that did not change.
        for (width, height, kbps, smooth) in [
            (200, 120, 3000, false),
            (64, 48, 60_000, false),
            (200, 120, 150, true),
            (320, 192, 400, true),
        ] {
            let config = EncoderConfig {
                width,
                height,
                fps: 60,
                bitrate_kbps: kbps,
                threads: 3,
            };
            let mut hinted = Encoder::new(config.clone().with_frame_budget(0)).unwrap();
            let mut plain = Encoder::new(config.with_frame_budget(0)).unwrap();
            let mut decoder = crate::h264::Decoder::new();
            let mut seed = 5u32;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            let mut picture = Yuv420::new(width, height);
            let stride = picture.y_stride;
            for (i, v) in picture.y.iter_mut().enumerate() {
                *v = if smooth {
                    (i % stride / 3 + i / stride / 2) as u8
                } else {
                    next() as u8
                };
            }
            for v in picture.u.iter_mut().chain(&mut picture.v) {
                *v = if smooth { 128 } else { next() as u8 };
            }
            picture.pad_edges();
            for frame in 0..40 {
                // Some frames change nothing; others a few rectangles, some
                // at the picture's edges.
                let mut rects = Vec::new();
                for _ in 0..(if frame % 5 == 3 { 0 } else { 1 + next() % 3 }) {
                    let r = Rect {
                        x: next() % width,
                        y: next() % height,
                        width: 1 + next() % 40,
                        height: 1 + next() % 30,
                    };
                    let fill = next() as u8;
                    for y in r.y..(r.y + r.height).min(height) {
                        for x in r.x..(r.x + r.width).min(width) {
                            let at = y as usize * picture.y_stride + x as usize;
                            picture.y[at] = if smooth {
                                picture.y[at].wrapping_add(fill % 24)
                            } else {
                                next() as u8
                            };
                            let c = (y / 2) as usize * picture.c_stride + (x / 2) as usize;
                            picture.u[c] = if smooth { 128 + fill % 8 } else { next() as u8 };
                        }
                    }
                    rects.push(r);
                }
                picture.pad_edges();
                let keyframe = frame == 20;
                let (mut a, mut b) = (Vec::new(), Vec::new());
                let ra = hinted.encode_changed(&picture, Some(&rects), keyframe, &mut a);
                let rb = plain.encode(&picture, keyframe, &mut b);
                assert_eq!(ra, rb, "frame {frame}");
                assert_eq!(
                    a, b,
                    "{width}x{height} frame {frame}: the bitstream changed"
                );
                assert_eq!(
                    hinted.reconstruction(),
                    plain.reconstruction(),
                    "frame {frame}"
                );
                let decoded = decoder.decode(&a).unwrap().unwrap();
                let r = hinted.reconstruction();
                let (w, h) = (width as usize, height as usize);
                for row in 0..h {
                    let (d, e) = (row * decoded.y_stride, row * r.y_stride);
                    assert_eq!(
                        decoded.y[d..d + w],
                        r.y[e..e + w],
                        "frame {frame} row {row}"
                    );
                }
                for row in 0..h / 2 {
                    let (d, e) = (row * decoded.c_stride, row * r.c_stride);
                    assert_eq!(decoded.u[d..d + w / 2], r.u[e..e + w / 2], "frame {frame}");
                    assert_eq!(decoded.v[d..d + w / 2], r.v[e..e + w / 2], "frame {frame}");
                }
            }
        }
    }

    /// A capture can report damage where nothing changed (an X cursor
    /// whose serial moved on, redrawn as it was): a settled picture still
    /// takes the all-skip shortcut, as it would without the report.
    #[test]
    fn damage_over_an_unchanged_picture_changes_nothing() {
        let config = EncoderConfig {
            width: 32,
            height: 32,
            fps: 60,
            bitrate_kbps: 60,
            threads: 1,
        };
        let mut hinted = Encoder::new(config.clone().with_frame_budget(0)).unwrap();
        let mut plain = Encoder::new(config.with_frame_budget(0)).unwrap();
        let mut picture = Yuv420::new(32, 32);
        for (i, v) in picture.y.iter_mut().enumerate() {
            *v = (i * 7 % 251) as u8;
        }
        picture.pad_edges();
        let cursor = [Rect {
            x: 3,
            y: 3,
            width: 12,
            height: 12,
        }];
        let mut settled = 0;
        for frame in 0..120 {
            // Now and then a real change, then the cursor's report again.
            let changed = [Rect {
                x: 16,
                y: 16,
                width: 16,
                height: 16,
            }];
            let hint: &[Rect] = if frame % 30 == 29 {
                for row in 16..32 {
                    let at = row * picture.y_stride + 16;
                    picture.y[at..at + 16].fill((frame * 37 % 251) as u8);
                }
                picture.pad_edges();
                &changed
            } else {
                &cursor
            };
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let ra = hinted.encode_changed(&picture, Some(hint), false, &mut a);
            let rb = plain.encode(&picture, false, &mut b);
            assert_eq!(ra, rb, "frame {frame}");
            assert_eq!(a, b, "frame {frame}: the bitstream changed");
            settled += usize::from(a.len() < 12);
        }
        assert!(
            settled > 10,
            "the picture settles: {settled} all-skip frames"
        );
    }

    /// Frames lost on the way: the encoder rolls back to the last picture
    /// the decoder has, and from the recovery frame on the decoder holds
    /// exactly the encoder's reconstruction again, without an IDR.
    #[test]
    fn recovery_frames_decode_exactly_after_a_loss() {
        for (threads, hinted) in [(1, false), (3, true), (2, false)] {
            let config = EncoderConfig {
                width: 96,
                height: 64,
                fps: 60,
                bitrate_kbps: 900,
                threads,
            };
            let mut encoder = Encoder::new(config.with_frame_budget(0)).unwrap();
            encoder.set_recovery_depth(8);
            let mut decoder = crate::h264::Decoder::new();
            let mut picture = Yuv420::new(96, 64);
            let mut seed = 99u32;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            // Frames 10..13 and 30 are lost, 20 is an IDR that is lost
            // too, and 40..41 are lost while the screen stands still.
            let lost =
                |f: u32| (10..13).contains(&f) || f == 20 || f == 30 || (40..42).contains(&f);
            let mut delivered = None;
            let mut recover = false;
            let mut recoveries = 0;
            for frame in 0..60u32 {
                let mut rects = Vec::new();
                if !(38..46).contains(&frame) {
                    // A moving bar and some noise: motion, skips and intra.
                    let x = (frame * 5 % 80) as usize;
                    for row in 0..64 {
                        for col in 0..96 {
                            let at = row * picture.y_stride + col;
                            picture.y[at] = if (x..x + 12).contains(&col) {
                                200
                            } else {
                                (col + row * 2) as u8
                            };
                        }
                    }
                    let r = Rect {
                        x: next() % 80,
                        y: next() % 48,
                        width: 16,
                        height: 16,
                    };
                    for row in r.y..r.y + 16 {
                        let at = row as usize * picture.y_stride + r.x as usize;
                        picture.y[at..at + 16].fill(next() as u8);
                    }
                    picture.pad_edges();
                    rects.push(Rect {
                        x: 0,
                        y: 0,
                        width: 96,
                        height: 64,
                    });
                }
                if recover {
                    assert!(encoder.recover_from(delivered.unwrap()), "frame {frame}");
                    recover = false;
                }
                let mut bits = Vec::new();
                let info = encoder
                    .encode_changed(
                        &picture,
                        hinted.then_some(&rects[..]),
                        frame == 0 || frame == 20,
                        &mut bits,
                    )
                    .unwrap();
                if lost(frame) {
                    // The request arrives before the next frame is made.
                    recover = !lost(frame + 1);
                    continue;
                }
                if let Some(base) = info.recovery {
                    assert_eq!(Some(base), delivered, "frame {frame}");
                    assert!(!info.keyframe);
                    recoveries += 1;
                }
                let decoded = decoder.decode(&bits).unwrap().unwrap();
                assert_eq!(
                    decoded,
                    encoder.reconstruction(),
                    "{threads}: frame {frame}"
                );
                delivered = Some(info.picture);
            }
            assert!(recoveries >= 3, "{recoveries} recoveries");
            assert!(!encoder.recover_from(1), "the first picture was forgotten");
        }
    }

    #[test]
    fn effort_transitions_preserve_decoder_reference() {
        let config = EncoderConfig {
            width: 64,
            height: 48,
            fps: 60,
            bitrate_kbps: 800,
            threads: 3,
        };
        assert!(Encoder::new(config.clone().with_frame_budget(101)).is_err());
        let mut encoder = Encoder::new(config.with_frame_budget(0)).unwrap();
        let mut decoder = crate::h264::Decoder::new();
        let mut picture = Yuv420::new(64, 48);
        let mut seed = 17u32;
        for effort in [0, 1, 2, 3, 4, 3, 2, 1, 0] {
            encoder.governor.level = effort;
            for v in picture
                .y
                .iter_mut()
                .chain(&mut picture.u)
                .chain(&mut picture.v)
            {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *v = (seed >> 16) as u8;
            }
            let mut bits = Vec::new();
            encoder.encode(&picture, false, &mut bits).unwrap();
            let decoded = decoder.decode(&bits).unwrap().unwrap();
            assert_eq!(decoded, encoder.reconstruction(), "effort {effort}");
            assert_eq!(encoder.timing().band_ms.len(), 3);
            assert_eq!(encoder.timing().effort, effort);
        }
    }
}
