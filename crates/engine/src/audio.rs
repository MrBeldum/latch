//! Desktop sound: whatever this machine plays, as 48 kHz 16-bit frames of
//! 5 ms, stereo or (when the viewer asks and the device has it) 5.1/7.1.

use std::time::Duration;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Most channels a capture delivers (7.1).
pub const MAX_CHANNELS: usize = latch_codec::audio::MAX_CHANNELS;
/// Samples per channel in one frame: 5 ms, so a frame leaves as soon as
/// a 5 ms device period has it, not after a second one.
pub const FRAME: usize = 240;

pub trait AudioCapture: Send {
    /// Fills `out` with the next stereo frame, interleaved, waiting up to
    /// `timeout`. Returns `Ok(false)` when no frame came in time (nothing
    /// is playing on a device that sends nothing while idle). Converts the
    /// device's format and rate.
    fn read(
        &mut self,
        out: &mut [i16; FRAME * CHANNELS],
        timeout: Duration,
    ) -> anyhow::Result<bool>;

    /// The device being captured, for people.
    fn describe(&self) -> String;

    /// Channels [`AudioCapture::read_frame`] delivers, in WAVE order.
    fn channels(&self) -> usize {
        CHANNELS
    }

    /// As `read`, in [`AudioCapture::channels`] channels: `out` is replaced
    /// by `FRAME` interleaved samples of each.
    fn read_frame(&mut self, out: &mut Vec<i16>, timeout: Duration) -> anyhow::Result<bool> {
        let mut frame = [0; FRAME * CHANNELS];
        let got = self.read(&mut frame, timeout)?;
        if got {
            out.clear();
            out.extend_from_slice(&frame);
        }
        Ok(got)
    }
}

/// Opens loopback capture of the default output device, in stereo.
pub fn open() -> anyhow::Result<Box<dyn AudioCapture>> {
    open_with(Layout::UpTo(CHANNELS))
}

/// How many channels a capture is to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The device's own layout (stereo, 5.1 or 7.1), down-mixed to fit.
    UpTo(usize),
    /// Exactly this many, whatever the device has: what a session agreed
    /// on stays when its device changes underneath it.
    Exactly(usize),
}

impl Layout {
    /// Latch's layout for a device with `device` channels.
    pub fn pick(self, device: usize) -> usize {
        match self {
            Layout::Exactly(n) => n.clamp(1, MAX_CHANNELS),
            Layout::UpTo(max) if max >= 8 && device >= 8 => 8,
            Layout::UpTo(max) if max >= 6 && device >= 6 => 6,
            Layout::UpTo(_) => CHANNELS,
        }
    }
}

/// Opens loopback capture of the default output device in `layout`.
pub fn open_with(layout: Layout) -> anyhow::Result<Box<dyn AudioCapture>> {
    let capture = open_native(layout)?;
    let want = layout.pick(capture.channels());
    Ok(if capture.channels() == want {
        capture
    } else {
        Box::new(Remix::new(capture, want))
    })
}

fn open_native(layout: Layout) -> anyhow::Result<Box<dyn AudioCapture>> {
    if let Some(clicks) = crate::testmode::open_audio() {
        return clicks;
    }
    #[cfg(target_os = "macos")]
    return {
        let _ = layout;
        crate::mac::open_audio()
    };
    #[cfg(windows)]
    return crate::windows::open_audio(layout);
    #[cfg(target_os = "linux")]
    return crate::linux::open_audio(layout);
}

/// A capture in another channel count than the session agreed on (a
/// stereo-only device, or a reopened one that changed), mixed to it.
struct Remix {
    inner: Box<dyn AudioCapture>,
    channels: usize,
    mix: Vec<Vec<f32>>,
    frame: Vec<i16>,
}

impl Remix {
    fn new(inner: Box<dyn AudioCapture>, channels: usize) -> Self {
        let from = inner.channels();
        let mask = latch_codec::audio::speaker_mask(from);
        Self {
            mix: latch_codec::audio::mix_matrix(from, mask, channels),
            inner,
            channels,
            frame: Vec::new(),
        }
    }
}

impl AudioCapture for Remix {
    fn read(
        &mut self,
        out: &mut [i16; FRAME * CHANNELS],
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        self.inner.read(out, timeout)
    }

    fn describe(&self) -> String {
        format!("{} → {} channels", self.inner.describe(), self.channels)
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn read_frame(&mut self, out: &mut Vec<i16>, timeout: Duration) -> anyhow::Result<bool> {
        if !self.inner.read_frame(&mut self.frame, timeout)? {
            return Ok(false);
        }
        let from = self.inner.channels();
        out.clear();
        for f in self.frame.chunks_exact(from) {
            for row in &self.mix {
                let v: f32 = f.iter().zip(row).map(|(&s, w)| f32::from(s) * w).sum();
                out.push(v.round().clamp(-32768.0, 32767.0) as i16);
            }
        }
        Ok(true)
    }
}

/// Converts interleaved f32 samples in −1..1 to i16, saturating.
pub fn f32_to_i16(src: &[f32], dst: &mut [i16]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    }
}

/// A linear resampler from any rate and channel layout to 48 kHz in one of
/// Latch's layouts (stereo unless asked otherwise), for devices that do not
/// run at 48 kHz or have other speakers.
pub struct Resampler {
    from_rate: u32,
    from_channels: usize,
    out_channels: usize,
    /// `mix[out][in]`, from [`latch_codec::audio::mix_matrix`].
    mix: Vec<Vec<f32>>,
    pos: i64,
    last: [f32; MAX_CHANNELS],
}

impl Resampler {
    pub fn new(from_rate: u32, from_channels: usize) -> Self {
        Self::with_channel_mask(from_rate, from_channels, 0)
    }

    /// WAVEFORMATEXTENSIBLE speaker bits, in ascending channel order. A zero
    /// mask uses the conventional mono/stereo/quad/5.1/7.1 layout.
    pub fn with_channel_mask(from_rate: u32, from_channels: usize, mask: u32) -> Self {
        Self::with_layout(from_rate, from_channels, mask, CHANNELS)
    }

    /// As [`Resampler::with_channel_mask`], into `out_channels` (1..=8).
    pub fn with_layout(
        from_rate: u32,
        from_channels: usize,
        mask: u32,
        out_channels: usize,
    ) -> Self {
        let ch = from_channels.max(1);
        let out_channels = out_channels.clamp(1, MAX_CHANNELS);
        Self {
            from_rate: from_rate.max(1),
            from_channels: ch,
            out_channels,
            mix: latch_codec::audio::mix_matrix(ch, mask, out_channels),
            pos: 0,
            last: [0.0; MAX_CHANNELS],
        }
    }

    pub fn out_channels(&self) -> usize {
        self.out_channels
    }

    /// Appends `src` (interleaved, `from_channels`) converted to 48 kHz
    /// f32 in the output layout to `dst`.
    pub fn push(&mut self, src: &[f32], dst: &mut Vec<f32>) {
        let ch = self.from_channels;
        let out = self.out_channels;
        let frames = src.len() / ch;
        let pick = |i: usize| -> [f32; MAX_CHANNELS] {
            let f = &src[i * ch..i * ch + ch];
            let mut mixed = [0.0; MAX_CHANNELS];
            for (m, weights) in mixed.iter_mut().zip(&self.mix) {
                *m = f
                    .iter()
                    .zip(weights)
                    .map(|(sample, w)| sample * w)
                    .sum::<f32>()
                    .clamp(-1.0, 1.0);
            }
            mixed
        };
        if self.from_rate == RATE {
            for i in 0..frames {
                dst.extend_from_slice(&pick(i)[..out]);
            }
            return;
        }
        if frames == 0 {
            return;
        }
        // Keep phase in units of 1/RATE source frames so rounding cannot
        // change output counts when the same input is split into small chunks.
        // -RATE means `last`; div_euclid also handles fractional negative positions.
        let rate = i64::from(RATE);
        while self.pos < (frames - 1) as i64 * rate {
            let i = self.pos.div_euclid(rate) as isize;
            let t = self.pos.rem_euclid(rate) as f32 / RATE as f32;
            let a = if i < 0 { self.last } else { pick(i as usize) };
            let b = pick((i + 1) as usize);
            for c in 0..out {
                dst.push(a[c] + (b[c] - a[c]) * t);
            }
            self.pos += i64::from(self.from_rate);
        }
        // Defer the final interval until the next chunk supplies its right
        // endpoint. A negative position now bridges `last` to the new first frame.
        self.last = pick(frames - 1);
        self.pos -= frames as i64 * rate;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_every_51_and_71_speaker_using_the_mask() {
        for mask in [0x3f_u32, 0x60f, 0x63f] {
            let channels = mask.count_ones() as usize;
            let mut r = Resampler::with_channel_mask(RATE, channels, mask);
            for i in 0..channels {
                let mut input = vec![0.0; channels];
                input[i] = 0.5;
                let mut out = Vec::new();
                r.push(&input, &mut out);
                let bit = (0..32).filter(|bit| mask & (1 << bit) != 0).nth(i).unwrap();
                let expected = match bit {
                    0 => [0.5, 0.0],
                    1 => [0.0, 0.5],
                    3 => [0.25, 0.25],
                    4 | 9 => [0.5 * std::f32::consts::FRAC_1_SQRT_2, 0.0],
                    5 | 10 => [0.0, 0.5 * std::f32::consts::FRAC_1_SQRT_2],
                    _ => [0.5 * std::f32::consts::FRAC_1_SQRT_2; 2],
                };
                assert_eq!(out, expected, "mask={mask:x}, speaker={bit}");
            }
            let mut out = Vec::new();
            r.push(&vec![1.0; channels], &mut out);
            assert_eq!(out, [1.0, 1.0]);
        }
        let mut stereo = Vec::new();
        Resampler::new(RATE, 2).push(&[0.25, -0.5], &mut stereo);
        assert_eq!(stereo, [0.25, -0.5]);
    }

    #[test]
    fn interpolation_is_continuous_across_chunks_and_empty_pushes() {
        for rate in [44_100, 96_000, 8_000] {
            let input: Vec<_> = (0..1000).map(|i| i as f32 / 1000.0).collect();
            let mut whole = Vec::new();
            Resampler::new(rate, 1).push(&input, &mut whole);
            for chunk in [1, 7, 441] {
                let mut split = Vec::new();
                let mut r = Resampler::new(rate, 1);
                for part in input.chunks(chunk) {
                    r.push(part, &mut split);
                    r.push(&[], &mut split);
                }
                assert_eq!(split.len(), whole.len());
                for (a, b) in split.iter().zip(&whole) {
                    assert!((a - b).abs() < 1e-6);
                }
            }
            if rate == 44_100 {
                assert!((whole[2] - 0.00091875).abs() < 1e-7);
            }
        }
    }
}
