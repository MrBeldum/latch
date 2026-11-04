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

pub(super) struct Ltr {
    slots: [Option<u64>; 4],
    next_slot: usize,
    every: u64,
    force: Option<(usize, u64)>,
}
impl Ltr {
    pub fn new(fps: u32) -> Self {
        Self {
            slots: [None; 4],
            next_slot: 0,
            every: u64::from(fps / 30).max(1),
            force: None,
        }
    }
    pub fn recover_from(&mut self, base: u64) -> bool {
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
                every: self.every,
                ..Self::new(30)
            };
        }
        if let Some((slot, base)) = self.force.take() {
            for (i, p) in self.slots.iter_mut().enumerate() {
                if i != slot {
                    *p = None;
                }
            }
            return (None, Some(0x10000 | (1 << slot)), Some(base));
        }
        // LTRBufferControl auto-marks the first four pictures after each IDR.
        let mark = if idr
            || self.slots.iter().any(Option::is_none)
            || picture.is_multiple_of(self.every)
        {
            let slot = self.next_slot;
            self.next_slot = (slot + 1) % self.slots.len();
            self.slots[slot] = Some(picture);
            Some(slot as u32)
        } else {
            None
        };
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
    for n in 0..32u32 {
        let dropping = (6..9).contains(&n) || (18..21).contains(&n);
        if n == 9 || n == 21 {
            ensure!(encoder.recover_from(delivered), "MFT lost its marked LTR");
        }
        let mut input = vec![128; (config.width * config.height * 3 / 2) as usize];
        for y in 0..config.height {
            for x in 0..config.width {
                input[(y * config.width + x) as usize] =
                    16 + (((x + n * 13) ^ (y + n * 7)).wrapping_mul(37) % 220) as u8;
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
            if !dropping {
                loss_bytes.extend_from_slice(&bytes);
                kept.push(n);
            }
            if n == 0 || n == 9 || n == 21 {
                eprintln!("MFT probe frame {n}: {} B IDR={idr}", bytes.len());
            }
        }
        let expected = full
            .decode(&bytes)?
            .context("uninterrupted MFT decode produced no picture")?;
        if !dropping {
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
    ensure!(checked == 26, "incomplete MFT reference test");
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
    encoder.reset_ltr(config.fps);
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
        assert!(!ltr.recover_from(1));
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
