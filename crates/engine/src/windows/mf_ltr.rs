//! MFT setters are not evidence of reference control. An encoder earns LTR
//! recovery only by decoding the same stream with and without missing frames.
use super::{
    capture::Gpu,
    encode::{self, Encoder},
};
use crate::video::VideoConfig;
use anyhow::{ensure, Context, Result};
use latch_stream::video::mediafoundation::{DecodedNv12, MfDecoder};
use windows::Win32::Graphics::{
    Direct3D11::D3D11_BIND_RENDER_TARGET, Dxgi::Common::DXGI_FORMAT_NV12,
};

/// Four application-controlled slots on two cadences: slots 0 and 1 take a
/// picture every ~33 ms, so a short round trip recovers from a recent one;
/// slots 2 and 3 every ~667 ms, so a base up to ⅔ s old (three round trips
/// of the ~155 ms peer-relayed path to the Mac) still has a slot. No round
/// trip reaches the pipeline, so the window is fixed.
pub(super) struct Ltr {
    slots: [Option<u64>; 4],
    /// The picture that filled each slot since the IDR, or since a refresh
    /// gave the slot up: until the viewer has it, its DPB holds one picture
    /// fewer than the encoder's.
    filled: [Option<u64>; 4],
    fast: u64,
    slow: u64,
    force: Option<(usize, u64)>,
}
impl Ltr {
    pub fn new(fps: u32) -> Self {
        Self {
            slots: [None; 4],
            filled: [None; 4],
            fast: u64::from(fps / 30).max(1),
            slow: u64::from(fps * 2 / 3).max(1),
            force: None,
        }
    }
    pub fn recover_from(&mut self, base: u64) -> bool {
        // A slot first filled by a picture the viewer lost: the encoder
        // replaces it when it marks the slot again, the viewer adds a
        // picture, and after the frame_num gap that exceeds
        // max_num_ref_frames (Apple's decoder blocks on it). An IDR instead.
        if self.filled.iter().flatten().any(|&p| p > base) {
            self.force = None;
            return false;
        }
        self.force = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.filter(|&p| p <= base).map(|p| (i, p)))
            .max_by_key(|&(_, p)| p)
            .map(|(slot, _)| (slot, base));
        self.force.is_some()
    }
    pub fn plan(&mut self, picture: u64, idr: bool) -> (Option<u32>, Option<u32>, Option<u64>) {
        if idr {
            *self = Self {
                fast: self.fast,
                slow: self.slow,
                ..Self::new(30)
            };
        }
        if let Some((slot, base)) = self.force.take() {
            for (i, p) in self.slots.iter_mut().enumerate() {
                if i != slot {
                    // The encoder may unmark these (the bitmap lists one
                    // slot): count their next picture as a new one.
                    *p = None;
                    self.filled[i] = None;
                }
            }
            return (None, Some(0x10000 | (1 << slot)), Some(base));
        }
        // LTRBufferControl auto-marks the first four pictures after each IDR.
        let older = |a: usize, b: usize, slots: &[Option<u64>; 4]| {
            if slots[a] <= slots[b] {
                a
            } else {
                b
            }
        };
        let slot = if let Some(empty) = self.slots.iter().position(Option::is_none) {
            Some(empty)
        } else if picture.is_multiple_of(self.slow) {
            Some(older(2, 3, &self.slots))
        } else if picture.is_multiple_of(self.fast) {
            Some(older(0, 1, &self.slots))
        } else {
            None
        };
        let mark = slot.map(|slot| {
            if self.slots[slot].is_none() {
                self.filled[slot] = Some(picture);
            }
            self.slots[slot] = Some(picture);
            slot as u32
        });
        (mark, None, None)
    }
}

fn pixels(frame: &DecodedNv12, config: &VideoConfig) -> Result<Vec<u8>> {
    ensure!(
        frame.bit_depth == 8 && frame.width == config.width && frame.height == config.height,
        "MFT probe decoded a different format"
    );
    ensure!(
        frame.shared.is_none(),
        "MFT probe needs CPU pixels, not shared renderer surfaces"
    );
    ensure!(
        frame.y_stride >= config.width as usize
            && frame.uv_stride >= config.width as usize
            && frame.y.len() >= frame.y_stride * config.height as usize
            && frame.uv.len() >= frame.uv_stride * config.height as usize / 2,
        "MFT probe decoded no complete CPU picture"
    );
    let mut bytes = Vec::new();
    for (plane, stride, rows) in [
        (&frame.y, frame.y_stride, frame.height),
        (&frame.uv, frame.uv_stride, frame.height / 2),
    ] {
        for row in plane.chunks(stride).take(rows as usize) {
            bytes.extend_from_slice(&row[..frame.width as usize]);
        }
    }
    Ok(bytes)
}

/// Pictures the self-test encodes at every encoder open.
const PROBE_FRAMES: u32 = 40;

pub(super) fn prove(encoder: &mut Encoder, config: &VideoConfig, gpu: Option<&Gpu>) -> Result<()> {
    let mut full = MfDecoder::with_size(config.codec, config.width, config.height)?;
    let mut loss = MfDecoder::with_size(config.codec, config.width, config.height)?;
    let texture = gpu
        .map(|gpu| {
            gpu.texture(
                config.width,
                config.height,
                DXGI_FORMAT_NV12,
                D3D11_BIND_RENDER_TARGET.0 as u32,
                false,
            )
        })
        .transpose()?;
    let mut delivered = 0;
    let mut checked = 0;
    #[cfg(test)]
    let (mut full_bytes, mut loss_bytes, mut kept) = (Vec::new(), Vec::new(), Vec::new());
    // Three losses; the last spans both fast slots, so its refresh comes
    // from an older slow one.
    let dropping = |n: u32| (6..9).contains(&n) || (18..21).contains(&n) || (28..34).contains(&n);
    for n in 0..PROBE_FRAMES {
        if n == 9 || n == 21 || n == 34 {
            ensure!(encoder.recover_from(delivered), "MFT lost its marked LTR");
        }
        // Two pictures in turn, each drifting: a picture is best predicted
        // by the one two before it, so after a loss by a lost one, which a
        // recovery frame and everything after it must not use.
        let mut input = vec![128; (config.width * config.height * 3 / 2) as usize];
        let (phase, step) = (n % 2, n / 2);
        for y in 0..config.height {
            for x in 0..config.width {
                let (sx, sy) = (x + step * 13, y + step * 7);
                let value = if phase == 0 {
                    (sx ^ sy).wrapping_mul(37) % 220
                } else {
                    (sx.wrapping_mul(5) + (sy / 3).wrapping_mul(11) + (sx * sy) % 97) % 220
                };
                input[(y * config.width + x) as usize] = 16 + value as u8;
            }
        }
        let sample = if let (Some(gpu), Some(texture)) = (gpu, &texture) {
            // SAFETY: contiguous NV12 input contains both planes, with width-byte pitch.
            unsafe {
                gpu.context.UpdateSubresource(
                    texture,
                    0,
                    None,
                    input.as_ptr().cast(),
                    config.width,
                    0,
                );
            }
            Encoder::texture_sample(texture)?
        } else {
            encode::memory_sample(&input)?
        };
        let (bytes, idr) = encoder
            .encode(&sample, n == 0)?
            .context("MFT buffers frames; LTR association is unproven")?;
        ensure!(idr == (n == 0), "MFT used an IDR instead of an LTR refresh");
        #[cfg(test)]
        {
            full_bytes.extend_from_slice(&bytes);
            if !dropping(n) {
                loss_bytes.extend_from_slice(&bytes);
                kept.push(n);
            }
            if n == 0 || n == 9 || n == 21 || n == 34 {
                eprintln!("MFT probe frame {n}: {} B IDR={idr}", bytes.len());
            }
        }
        let expected = full
            .decode(&bytes)?
            .context("uninterrupted MFT decode produced no picture")?;
        if !dropping(n) {
            let actual = loss
                .decode(&bytes)?
                .context("LTR MFT decode produced no picture")?;
            ensure!(
                pixels(&actual, config)? == pixels(&expected, config)?,
                "MFT ignored LTR control at probe frame {n}"
            );
            checked += 1;
            delivered = encoder.last_picture().0;
        }
    }
    ensure!(
        checked == (0..PROBE_FRAMES).filter(|&n| !dropping(n)).count(),
        "incomplete MFT reference test"
    );
    #[cfg(test)]
    if let Some(dir) = std::env::var_os("LATCH_MF_RECOVERY_DUMP") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("full.h264"), full_bytes)?;
        std::fs::write(dir.join("loss.h264"), loss_bytes)?;
        std::fs::write(
            dir.join("kept.txt"),
            kept.iter().map(|n| format!("{n}\n")).collect::<String>(),
        )?;
    }
    // Start the real stream at a fresh IDR, with no test-picture identities.
    encoder.after_probe(config.fps);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_or_shared_output_cannot_pass_a_pixel_comparison() {
        let config = VideoConfig {
            width: 64,
            height: 64,
            fps: 60,
            bitrate_kbps: 1000,
            codec: crate::video::Codec::H264High,
            color: Default::default(),
        };
        let mut frame = DecodedNv12 {
            bit_depth: 8,
            width: 64,
            height: 64,
            y: Vec::new(),
            y_stride: 64,
            uv: Vec::new(),
            uv_stride: 64,
            shared: None,
        };
        assert!(pixels(&frame, &config).is_err());
        frame.y = vec![16; 4096];
        frame.uv = vec![128; 2048];
        assert_eq!(pixels(&frame, &config).unwrap().len(), 6144);
    }
    #[test]
    fn stale_slots_idr_and_repeated_loss() {
        let mut ltr = Ltr::new(60);
        for p in 1..=12 {
            ltr.plan(p, p == 1);
        }
        // Slots 2 and 3 still hold pictures 3 and 4; 1 and 2 are gone.
        assert!(!ltr.recover_from(2));
        assert!(ltr.recover_from(11));
        let (mark, used, base) = ltr.plan(13, false);
        assert!(mark.is_none());
        assert!(used.unwrap() & 0x10000 != 0);
        assert_eq!(base, Some(11));
        assert!(ltr.recover_from(11));
        assert_eq!(ltr.plan(14, false).2, Some(11));
        ltr.plan(15, true);
        assert!(!ltr.recover_from(14));
    }
    /// A base ⅔ s old still has a slot at 60 and 240 fps, and the newest
    /// slot at or before the base is chosen.
    #[test]
    fn slow_slots_cover_two_thirds_of_a_second() {
        for fps in [30, 60, 120, 240] {
            let mut ltr = Ltr::new(fps);
            let window = u64::from(fps) * 2 / 3;
            for p in 1..=u64::from(fps) * 5 {
                ltr.plan(p, p == 1);
                if p > window + 4 {
                    assert!(ltr.recover_from(p - window), "{fps} fps, picture {p}");
                    let chosen = ltr.force.unwrap().0;
                    assert!(ltr.slots[chosen].unwrap() <= p - window);
                    ltr.force = None;
                }
            }
        }
    }
    /// The first pictures after an IDR fill empty slots; losing one of them
    /// takes an IDR, since the viewer's DPB would later hold one too many.
    #[test]
    fn a_lost_first_fill_refuses_recovery() {
        let mut ltr = Ltr::new(60);
        ltr.plan(1, true);
        ltr.plan(2, false);
        // Pictures 3 and 4 fill slots 2 and 3; the viewer has only 1 and 2.
        ltr.plan(3, false);
        ltr.plan(4, false);
        assert!(!ltr.recover_from(2));
        assert!(ltr.recover_from(4));
        // The refresh gives up slots 0, 1 and 2; the next fill is new again.
        assert_eq!(ltr.plan(5, false).1, Some(0x10000 | 1 << 3));
        assert_eq!(ltr.plan(6, false).0, Some(0));
        assert!(!ltr.recover_from(5));
        assert!(ltr.recover_from(6));
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;
    #[test]
    #[ignore]
    fn hardware_mf_ltr_open() -> Result<()> {
        let _runtime = super::super::Runtime::new(false)?;
        let gpu = Gpu::new(&super::super::display::select()?.adapter)?;
        let config = VideoConfig {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_kbps: 8000,
            codec: crate::video::Codec::H264High,
            color: Default::default(),
        };
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let encoder = Encoder::new(&config, &gpu)?;
        eprintln!(
            "{}: hardware={} proven LTR={}",
            encoder.name,
            encoder.hardware,
            encoder.recovers()
        );
        if encoder.recovers() {
            let mut encoder = encoder;
            encoder.ignore_ltr_use = true;
            let error = prove(&mut encoder, &config, Some(&gpu))
                .expect_err("ignored UseLTRFrame must fail the probe");
            ensure!(
                error.to_string().contains("ignored LTR control"),
                "negative control failed for another reason: {error:#}"
            );
            eprintln!("negative control rejected: {error:#}");
        }
        Ok(())
    }
}
