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
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    pub fn new(from_rate: u32, from_channels: usize) -> Self {
        Self {
            from_rate,
            from_channels: from_channels.max(1),
            pos: 0.0,
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
            if ch == 1 {
                [f[0], f[0]]
            } else {
                [f[0], f[1]]
            }
        };
        if self.from_rate == RATE {
            for i in 0..frames {
                dst.extend_from_slice(&pick(i));
            }
            return;
        }
        let step = self.from_rate as f64 / RATE as f64;
        // `pos` is in source frames, relative to the start of `src`, with
        // -1 meaning `last`.
        while self.pos < frames as f64 {
            let i = self.pos.floor();
            let t = (self.pos - i) as f32;
            let i = i as isize;
            let a = if i < 0 { self.last } else { pick(i as usize) };
            let b = if i + 1 < frames as isize {
                pick((i + 1) as usize)
            } else {
                a
            };
            dst.push(a[0] + (b[0] - a[0]) * t);
            dst.push(a[1] + (b[1] - a[1]) * t);
            self.pos += step;
        }
        if frames > 0 {
            self.last = pick(frames - 1);
        }
        self.pos -= frames as f64;
    }
}
