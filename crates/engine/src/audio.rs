//! Desktop sound: whatever this machine plays, as 48 kHz stereo 16-bit
//! frames of 10 ms.

use std::time::Duration;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one frame.
pub const FRAME: usize = 480;

pub trait AudioCapture: Send {
    /// Fills `out` with the next frame, interleaved, waiting up to
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
}

/// Opens loopback capture of the default output device.
pub fn open() -> anyhow::Result<Box<dyn AudioCapture>> {
    #[cfg(target_os = "macos")]
    return crate::mac::open_audio();
    #[cfg(windows)]
    return crate::windows::open_audio();
    #[cfg(target_os = "linux")]
    return crate::linux::open_audio();
}

/// Converts interleaved f32 samples in −1..1 to i16, saturating.
pub fn f32_to_i16(src: &[f32], dst: &mut [i16]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    }
}

/// A linear resampler from any rate and channel count to 48 kHz stereo,
/// for devices that do not run at 48 kHz.
pub struct Resampler {
    from_rate: u32,
    from_channels: usize,
    mix: Vec<[f32; 2]>,
    pos: i64,
    last: [f32; 2],
}

impl Resampler {
    pub fn new(from_rate: u32, from_channels: usize) -> Self {
        Self::with_channel_mask(from_rate, from_channels, 0)
    }

    /// WAVEFORMATEXTENSIBLE speaker bits, in ascending channel order. A zero
    /// mask uses the conventional mono/stereo/quad/5.1/7.1 layout.
    pub fn with_channel_mask(from_rate: u32, from_channels: usize, mask: u32) -> Self {
        let ch = from_channels.max(1);
        let mask = if mask.count_ones() as usize == ch {
            mask
        } else {
            match ch {
                1 => 0x4,
                2 => 0x3,
                3 => 0x7,
                4 => 0x33,
                5 => 0x37,
                6 => 0x3f,
                7 => 0x70f,
                8 => 0x63f,
                _ => 0,
            }
        };
        let side = std::f32::consts::FRAC_1_SQRT_2;
        let mut mix: Vec<_> = (0..32)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| match bit {
                0 => [1.0, 0.0], // front left/right
                1 => [0.0, 1.0],
                3 => [0.5, 0.5], // LFE
                4 | 6 | 9 | 12 | 15 => [side, 0.0],
                5 | 7 | 10 | 14 | 17 => [0.0, side],
                _ => [side, side], // centre, back centre and height centres
            })
            .collect();
        if ch == 1 {
            mix = vec![[1.0, 1.0]];
        }
        if mix.len() != ch {
            mix = vec![[1.0 / ch as f32; 2]; ch];
        }
        Self {
            from_rate: from_rate.max(1),
            from_channels: ch,
            mix,
            pos: 0,
            last: [0.0; 2],
        }
    }

    /// Appends `src` (interleaved, `from_channels`) converted to 48 kHz
    /// stereo f32 to `dst`.
    pub fn push(&mut self, src: &[f32], dst: &mut Vec<f32>) {
        let ch = self.from_channels;
        let frames = src.len() / ch;
        let pick = |i: usize| -> [f32; 2] {
            let f = &src[i * ch..i * ch + ch];
            let mut stereo = [0.0; 2];
            for (sample, weights) in f.iter().zip(&self.mix) {
                stereo[0] += sample * weights[0];
                stereo[1] += sample * weights[1];
            }
            stereo.map(|v| v.clamp(-1.0, 1.0))
        };
        if self.from_rate == RATE {
            for i in 0..frames {
                dst.extend_from_slice(&pick(i));
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
            dst.push(a[0] + (b[0] - a[0]) * t);
            dst.push(a[1] + (b[1] - a[1]) * t);
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
