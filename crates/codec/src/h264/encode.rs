//! A desktop encoder with one independently predicted slice per worker band.

mod analyse;
mod bits;
mod cavlc;
mod deblock;
mod predict;
mod rate;
mod transform;
mod worker;

use super::CodecError;
use crate::Yuv420;
use bits::Bits;
use std::collections::VecDeque;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

#[derive(Debug, Clone, PartialEq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Worker threads; 0 picks the number of CPUs (at most 8).
    pub threads: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedInfo {
    pub keyframe: bool,
    /// Average QP of the coded macroblocks.
    pub qp: u8,
    pub skipped_mbs: u32,
    pub total_mbs: u32,
    pub bytes: usize,
}

struct Worker {
    tx: Option<mpsc::Sender<worker::Task>>,
    rx: mpsc::Receiver<worker::Reply>,
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
    workers: Vec<Worker>,
    reconstruction: Arc<Yuv420>,
    source: Option<Arc<Yuv420>>,
    /// Row hashes of `source`, for the next frame's scroll search.
    hashes: Arc<Vec<u64>>,
    /// Motion vectors of the last coded frame, per macroblock.
    mvs: Arc<Vec<(i32, i32)>>,
    /// Its most common nonzero motion vector.
    dominant: (i32, i32),
    quality: Arc<Vec<u8>>,
    frame_num: u32,
    idr_id: u32,
    /// QP of the last coded frame, for skip-only frames.
    qp: u8,
    rate: rate::RateControl,
    subpixel: bool,
    refresh_start: usize,
    window: VecDeque<usize>,
    /// Each worker's last band, handed back for its buffers.
    bands: Vec<Option<worker::Band>>,
}

/// What every band of one frame shares.
struct Frame {
    source: Arc<Yuv420>,
    previous: Option<Arc<Yuv420>>,
    half_pel: Option<Arc<predict::HalfPelPlanes>>,
    /// Each band's scroll.
    globals: Vec<(i32, i32)>,
    /// The analysis's displacement per macroblock.
    hints: Arc<Vec<(i32, i32)>>,
    is_motion: bool,
    idr: bool,
    frame_num: u32,
    refresh: usize,
}

/// One pass over the picture: its quantizer, bits for what changed and
/// for refining what did not.
struct Pass {
    q: i32,
    content: f64,
    refine: f64,
}

/// A coded picture before deblocking.
struct Coded {
    data: Vec<u8>,
    rec: Yuv420,
    mbs: Vec<worker::Mb>,
    skipped: u32,
    qp_sum: u32,
    /// Effective quantizer weighted by cost.
    q: f64,
    refine_bits: usize,
}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, CodecError> {
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
        if config.fps == 0 || config.fps > 240 || config.bitrate_kbps == 0 {
            return Err(CodecError::Config(
                "fps must be 1..=240 and bitrate must be nonzero",
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
            let handle = thread::Builder::new()
                .name(format!("h264-{i}"))
                .spawn(move || {
                    while let Ok(task) = rx.recv() {
                        if out_tx.send(worker::run(task)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|_| CodecError::Config("cannot start encoder worker"))?;
            workers.push(Worker {
                tx: Some(tx),
                rx: out_rx,
                handle: Some(handle),
            });
        }
        let total = reconstruction.padded_width() / 16 * rows;
        Ok(Self {
            rate: rate::RateControl::new(config.bitrate_kbps, config.fps),
            config,
            bands: (0..workers.len()).map(|_| None).collect(),
            workers,
            reconstruction,
            source: None,
            hashes: Arc::new(Vec::new()),
            mvs: Arc::new(Vec::new()),
            dominant: (0, 0),
            quality: Arc::new(vec![45; total]),
            frame_num: 0,
            idr_id: 0,
            qp: 30,
            subpixel: true,
            refresh_start: 0,
            window: VecDeque::new(),
        })
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
        self.validate(picture)?;
        let idr = keyframe || self.source.is_none();
        if !idr && self.quality.iter().all(|&q| q <= 18) && self.source.as_deref() == Some(picture)
        {
            return self.encode_settled(out);
        }
        let frame_num = if idr { 0 } else { self.frame_num };
        let source = Arc::new(picture.clone());
        let rows = picture.padded_height() / 16;
        let width = picture.padded_width() / 16;
        let total_mbs = rows * width;
        let previous = if idr { None } else { self.source.clone() };

        // Each worker looks at its band first: scroll, change and cost.
        let analyses = self.analyse(&source, &previous)?;
        let hashes: Vec<u64> = analyses
            .iter()
            .flat_map(|a| a.hashes.iter().copied())
            .collect();
        let row_costs: Vec<u32> = analyses
            .iter()
            .flat_map(|a| a.cost.iter().copied())
            .collect();
        let changed_mbs: usize = analyses.iter().map(|a| a.changed).sum();
        let cost: f64 = row_costs.iter().map(|&c| c as f64).sum();
        let intra: f64 = analyses
            .iter()
            .flat_map(|a| &a.intra)
            .map(|&c| c as f64)
            .sum();
        let mut frame = Frame {
            source,
            previous,
            half_pel: None,
            globals: analyses.iter().map(|a| a.global).collect(),
            hints: Arc::new(
                analyses
                    .iter()
                    .flat_map(|a| a.mvs.iter().copied())
                    .collect(),
            ),
            is_motion: changed_mbs * 2 > total_mbs,
            idr,
            frame_num,
            refresh: (self.rate.budget() / 32.).max(1.) as usize,
        };
        let mut plan = self.rate.plan(
            idr,
            cost,
            intra,
            changed_mbs as f64 / total_mbs as f64,
            self.quality.iter().any(|&q| q > 18),
        );
        if plan.intra() {
            // Nothing earlier predicts this picture's bits: measure them.
            let qa = rate::RateControl::first_trial(&plan);
            let mut trials = vec![(qa, self.trial(&frame, &row_costs, qa)?)];
            if let Some(qb) = self.rate.second_trial(&plan, qa, trials[0].1) {
                trials.push((qb, self.trial(&frame, &row_costs, qb)?));
            }
            self.rate.fit(&mut plan, cost, &trials);
        }
        // At high QP fractional search costs more than its quality gain; also skip tiny updates and scrolls.
        let refine_motion = self.subpixel
            && rate::split(plan.q).0 < 38
            && changed_mbs * 32 > total_mbs
            && frame.globals.contains(&(0, 0));
        if !idr && refine_motion {
            frame.half_pel = Some(Arc::new(predict::compute_half_pel(
                &self.reconstruction.y,
                self.reconstruction.y_stride,
                self.reconstruction.padded_width(),
                self.reconstruction.padded_height(),
            )));
        }
        let budget = self.rate.budget();
        let slope = self.rate.slope(&plan);
        let mut pass = Pass {
            q: plan.q,
            content: plan.content,
            refine: plan.refine,
        };
        let mut second = false;
        let coded = loop {
            let coded = self.code(&frame, &row_costs, &pass, slope)?;
            let bits = coded.data.len() as f64 * 8.;
            // One more pass at most: the first pass's size says how far off the plan was.
            if !second && bits > plan.limit && pass.q < rate::MAX_Q {
                pass = Pass {
                    q: self.rate.replan(&plan, coded.q, bits),
                    content: plan.content * 0.85,
                    refine: 0.,
                };
                second = true;
                continue;
            }
            // Refusing an impossible access unit preserves the reference and the caller's output.
            if bits > budget * 8. {
                return Err(CodecError::Config(
                    "frame exceeds eight bitrate intervals at the allowed QP",
                ));
            }
            break coded;
        };
        let Coded {
            mut data,
            mut rec,
            mbs,
            skipped,
            qp_sum,
            q,
            refine_bits,
        } = coded;
        if idr {
            let mut sets = Vec::new();
            parameter_sets(&self.config, &mut sets)?;
            data.splice(0..0, sets);
        }
        deblock::filter(&mut rec, &mbs);
        self.reconstruction = Arc::new(rec);
        self.source = Some(frame.source);
        self.hashes = Arc::new(hashes);
        self.mvs = Arc::new(mbs.iter().map(|m| m.mv).collect());
        self.dominant = dominant(&self.mvs);
        self.quality = Arc::new(mbs.iter().map(|m| m.quality).collect());
        self.frame_num = (frame_num + 1) & 65535;
        if idr {
            self.idr_id ^= 1;
        }
        let bits = data.len() as f64 * 8.;
        self.rate
            .update(&plan, cost, bits, bits - refine_bits as f64, q);
        self.qp = (qp_sum / mbs.len() as u32) as u8;
        self.refresh_start = (self.refresh_start + frame.refresh) % self.quality.len();
        self.window.push_back(data.len());
        if self.window.len() > self.config.fps as usize {
            self.window.pop_front();
        }
        let bytes = data.len();
        out.extend_from_slice(&data);
        Ok(EncodedInfo {
            keyframe: idr,
            qp: (qp_sum / (mbs.len() as u32)) as u8,
            skipped_mbs: skipped,
            total_mbs: mbs.len() as u32,
            bytes,
        })
    }

    fn band(&self, i: usize) -> (usize, usize) {
        let rows = self.reconstruction.padded_height() / 16;
        let n = self.workers.len();
        (i * rows / n, (i + 1) * rows / n)
    }

    fn analyse(
        &self,
        source: &Arc<Yuv420>,
        previous: &Option<Arc<Yuv420>>,
    ) -> Result<Vec<analyse::Analysis>, CodecError> {
        for (i, worker) in self.workers.iter().enumerate() {
            let (first, end) = self.band(i);
            send(
                worker,
                worker::Task::Analyse(analyse::Job {
                    source: source.clone(),
                    previous: previous.clone(),
                    previous_hashes: self.hashes.clone(),
                    previous_mvs: self.mvs.clone(),
                    dominant: self.dominant,
                    first,
                    end,
                }),
            )?;
        }
        self.workers
            .iter()
            .map(|worker| match receive(worker)? {
                worker::Reply::Analysis(a) => Ok(a),
                worker::Reply::Band(_) => unreachable!("an analysis was asked for"),
            })
            .collect()
    }

    fn job(&self, frame: &Frame, (first, end): (usize, usize), q: i32) -> worker::Job {
        // The band holding row `first`, for its scroll.
        let band = (0..self.workers.len())
            .rposition(|i| self.band(i).0 <= first)
            .unwrap_or(0);
        worker::Job {
            scratch: None,
            source: frame.source.clone(),
            previous: frame.previous.clone(),
            reference: self.reconstruction.clone(),
            half_pel: frame.half_pel.clone(),
            quality: self.quality.clone(),
            first,
            end,
            q,
            rows: rate::RowControl::default(),
            refine_bits: 0.,
            subpixel: frame.half_pel.is_some() && rate::split(q).0 < 38,
            is_motion: frame.is_motion,
            global: frame.globals[band],
            hints: frame.hints.clone(),
            idr: frame.idr,
            frame_num: frame.frame_num,
            idr_id: self.idr_id,
            refresh: frame.refresh,
            refresh_start: self.refresh_start,
        }
    }

    /// Estimates the frame's bits at quantizer `q` by coding a few rows
    /// spread over the picture, scaled up by their share of the cost.
    fn trial(&self, frame: &Frame, row_costs: &[u32], q: i32) -> Result<f64, CodecError> {
        let rows = row_costs.len();
        let width = self.reconstruction.padded_width() / 16;
        let n = rows.min(8);
        let sample: Vec<usize> = (0..n).map(|j| (2 * j + 1) * rows / (2 * n)).collect();
        let weight = |r: usize| row_costs[r] as f64 + 32. * width as f64;
        let scale =
            (0..rows).map(weight).sum::<f64>() / sample.iter().map(|&r| weight(r)).sum::<f64>();
        let workers = self.workers.len();
        for (k, &row) in sample.iter().enumerate() {
            let job = self.job(frame, (row, row + 1), q);
            send(
                &self.workers[k % workers],
                worker::Task::Encode(Box::new(job)),
            )?;
        }
        let mut bits = 0.;
        for k in 0..sample.len() {
            match receive(&self.workers[k % workers])? {
                worker::Reply::Band(band) => bits += band.nal.len() as f64 * 8.,
                worker::Reply::Analysis(_) => unreachable!("a band was asked for"),
            }
        }
        Ok(bits * scale)
    }

    /// Codes the whole picture once, every band in parallel.
    fn code(
        &mut self,
        frame: &Frame,
        row_costs: &[u32],
        pass: &Pass,
        slope: f64,
    ) -> Result<Coded, CodecError> {
        let rows = row_costs.len();
        let width = self.reconstruction.padded_width() / 16;
        let cost: f64 =
            row_costs.iter().map(|&c| c as f64).sum::<f64>() + 32. * (rows * width) as f64;
        let mut shares = Vec::with_capacity(self.workers.len());
        for i in 0..self.workers.len() {
            let (first, end) = self.band(i);
            let costs = &row_costs[first..end];
            let share = (costs.iter().map(|&c| c as f64).sum::<f64>()
                + 32. * ((end - first) * width) as f64)
                / cost;
            shares.push(share);
            let mut job = self.job(frame, (first, end), pass.q);
            job.scratch = self.bands[i].take();
            job.rows = rate::RowControl::new(pass.content * share, costs, width, pass.q, slope);
            // Refinement is spread by area: unchanged macroblocks cost nothing in the analysis.
            job.refine_bits = pass.refine * (end - first) as f64 / rows as f64;
            job.global = frame.globals[i];
            send(&self.workers[i], worker::Task::Encode(Box::new(job)))?;
        }
        let mut coded = Coded {
            data: Vec::new(),
            rec: Yuv420::new(self.config.width, self.config.height),
            mbs: Vec::with_capacity(width * rows),
            skipped: 0,
            qp_sum: 0,
            q: 0.,
            refine_bits: 0,
        };
        for (i, worker) in self.workers.iter().enumerate() {
            let band = match receive(worker)? {
                worker::Reply::Band(band) => band,
                worker::Reply::Analysis(_) => unreachable!("a band was asked for"),
            };
            coded.data.extend_from_slice(&band.nal);
            coded.skipped += band.skipped;
            coded.qp_sum += band.qp_sum;
            coded.q += band.q * shares[i];
            coded.refine_bits += band.refine_bits;
            let rec = &mut coded.rec;
            let first = band.first * 16;
            for (dp, sp, ds, ss, n) in [
                (
                    &mut rec.y,
                    &band.picture.y,
                    rec.y_stride,
                    band.picture.y_stride,
                    1,
                ),
                (
                    &mut rec.u,
                    &band.picture.u,
                    rec.c_stride,
                    band.picture.c_stride,
                    2,
                ),
                (
                    &mut rec.v,
                    &band.picture.v,
                    rec.c_stride,
                    band.picture.c_stride,
                    2,
                ),
            ] {
                for row in 0..band.picture.padded_height() / n {
                    let d = (first / n + row) * ds;
                    dp[d..d + ds].copy_from_slice(&sp[row * ss..row * ss + ds]);
                }
            }
            coded.mbs.extend_from_slice(&band.mbs);
            self.bands[i] = Some(band);
        }
        Ok(coded)
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.config.bitrate_kbps = kbps.max(1);
        self.rate
            .set_rate(self.config.bitrate_kbps, self.config.fps);
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
            let mut b = worker::header(first, false, self.frame_num, 0, self.qp);
            b.ue((end - first) as u32);
            b.nal(0x41, &mut data);
        }
        if data.len() as f64 * 8. > self.rate.budget() * 8. {
            return Err(CodecError::Config(
                "skip headers exceed eight bitrate intervals",
            ));
        }
        self.rate.record(data.len() as f64 * 8.);
        self.frame_num = (self.frame_num + 1) & 65535;
        self.window.push_back(data.len());
        if self.window.len() > self.config.fps as usize {
            self.window.pop_front();
        }
        let bytes = data.len();
        out.extend_from_slice(&data);
        let total_mbs = (width * rows) as u32;
        Ok(EncodedInfo {
            keyframe: false,
            qp: self.qp,
            skipped_mbs: total_mbs,
            total_mbs,
            bytes,
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

/// The most common nonzero vector, by whole samples.
fn dominant(mvs: &[(i32, i32)]) -> (i32, i32) {
    let mut whole: Vec<(i32, i32)> = mvs
        .iter()
        .filter(|&&mv| mv.0 / 4 != 0 || mv.1 / 4 != 0)
        .map(|&(x, y)| (x / 4 * 4, y / 4 * 4))
        .collect();
    whole.sort_unstable();
    whole
        .chunk_by(|a, b| a == b)
        .max_by_key(|run| run.len())
        .map_or((0, 0), |run| run[0])
}
fn send(worker: &Worker, task: worker::Task) -> Result<(), CodecError> {
    worker
        .tx
        .as_ref()
        .and_then(|tx| tx.send(task).ok())
        .ok_or(CodecError::Config("encoder worker stopped"))
}
fn receive(worker: &Worker) -> Result<worker::Reply, CodecError> {
    worker
        .rx
        .recv()
        .map_err(|_| CodecError::Config("encoder worker stopped"))
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
