//! A desktop encoder with one independently predicted slice per worker band.

mod bits;
mod cavlc;
mod deblock;
mod effort;
mod predict;
mod transform;
mod worker;

use super::CodecError;
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
    quality: Arc<Vec<u8>>,
    frame_num: u32,
    idr_id: u32,
    qp: u8,
    model: f64,
    subpixel: bool,
    refresh_start: usize,
    window: VecDeque<usize>,
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
            quality: Arc::new(vec![45; total]),
            frame_num: 0,
            idr_id: 0,
            qp: 30,
            model: 0.,
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
        let started = Instant::now();
        self.timing = EncodeTiming {
            effort: self.governor.level,
            band_ms: vec![0.; self.workers.len()],
            ..EncodeTiming::default()
        };
        let idr = keyframe || self.source.is_none();
        if !idr && self.quality.iter().all(|&q| q <= 18) && self.source.as_deref() == Some(picture)
        {
            let result = self.encode_settled(out)?;
            self.finish_timing(started);
            return Ok(result);
        }
        let frame_num = if idr { 0 } else { self.frame_num };
        let source = Arc::new(match self.spare_source.take() {
            Some(mut spare) => {
                spare.copy_from(picture);
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
        let mut next = self
            .spare_reconstruction
            .take()
            .unwrap_or_else(|| Yuv420::new(self.config.width, self.config.height));
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
                return Err(CodecError::Config(
                    "frame exceeds eight bitrate intervals at the allowed QP",
                ));
            }
            break (data, mbs, skipped, qp_sum);
        };
        // Filtering was completed independently by each slice worker.
        let old = std::mem::replace(&mut self.reconstruction, Arc::new(next));
        self.spare_reconstruction = Arc::try_unwrap(old).ok();
        if let Some(old) = self.source.replace(source) {
            self.spare_source = Arc::try_unwrap(old).ok();
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
        self.finish_timing(started);
        Ok(EncodedInfo {
            keyframe: idr,
            qp: (qp_sum / (mbs.len() as u32)) as u8,
            skipped_mbs: skipped,
            total_mbs: mbs.len() as u32,
            bytes,
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
