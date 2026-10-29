//! A synthetic desktop for tests and measurements: known pixels a decoded
//! frame can be checked against, motion when asked, and a square that
//! changes colour when a key is pressed (the viewer times that change to
//! measure input to picture).

use crate::input::{DisplayRect, Event};
use crate::software::{Bgra, CpuCapture};
use std::time::{Duration, Instant};

/// What the pattern draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Never changes, except for the input square.
    Still,
    /// A bar sweeps across every frame.
    Sweep,
    /// Everything changes every frame (noise): the worst case for an
    /// encoder.
    Noise,
}

pub struct Pattern {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub motion: Motion,
    frame_count: u64,
    square_white: bool,
    dirty: bool,
    last_grab_time: Option<Instant>,
}

impl Pattern {
    pub fn new(width: u32, height: u32, fps: u32, motion: Motion) -> Self {
        Self {
            width,
            height,
            fps,
            motion,
            frame_count: 0,
            square_white: false,
            dirty: true,
            last_grab_time: None,
        }
    }

    /// Feeds an input event in (the square at the top left flips between
    /// black and white on every key press).
    pub fn input(&mut self, event: &Event) {
        if let Event::Key { down: true, .. } = event {
            self.square_white = !self.square_white;
            self.dirty = true;
        }
    }

    /// The colour the 64×64 square at (0, 0) has right now (BGRA).
    pub fn square(&self) -> [u8; 4] {
        if self.square_white {
            [255, 255, 255, 255]
        } else {
            [0, 0, 0, 255]
        }
    }

    fn render(&self, frame: &mut Bgra) {
        let w = self.width as usize;
        let h = self.height as usize;
        let stride = w * 4;
        frame.width = self.width;
        frame.height = self.height;
        frame.stride = stride;
        frame.data.resize(stride * h, 0);

        let top_h = h / 2;

        // 1. 75% SMPTE Colour Bars (BGRA)
        // White, Yellow, Cyan, Green, Magenta, Red, Blue, Black
        const BARS_75: [[u8; 4]; 8] = [
            [191, 191, 191, 255], // White
            [0, 191, 191, 255],   // Yellow
            [191, 191, 0, 255],   // Cyan
            [0, 191, 0, 255],     // Green
            [191, 0, 191, 255],   // Magenta
            [0, 0, 191, 255],     // Red
            [191, 0, 0, 255],     // Blue
            [0, 0, 0, 255],       // Black
        ];

        for y in 0..top_h {
            let row = &mut frame.data[y * stride..(y + 1) * stride];
            for x in 0..w {
                let bar_idx = (x * 8) / w.max(1);
                let color = BARS_75[bar_idx.min(7)];
                row[x * 4..x * 4 + 4].copy_from_slice(&color);
            }
        }

        // 2. 1-pixel alternating black-and-white checkerboard patch in top-right
        let cb_size = (w / 8).clamp(32, 128);
        let cb_x_start = w.saturating_sub(cb_size + 16);
        let cb_y_start = (top_h / 2).saturating_sub(cb_size / 2);
        for y in cb_y_start..(cb_y_start + cb_size).min(top_h) {
            let row = &mut frame.data[y * stride..(y + 1) * stride];
            for x in cb_x_start..(cb_x_start + cb_size).min(w) {
                let c = if (x + y) % 2 == 0 { 255 } else { 0 };
                row[x * 4..x * 4 + 4].copy_from_slice(&[c, c, c, 255]);
            }
        }

        // 3. Bottom half background
        for y in top_h..h {
            let row = &mut frame.data[y * stride..(y + 1) * stride];
            for x in 0..w {
                row[x * 4..x * 4 + 4].copy_from_slice(&[32, 32, 32, 255]);
            }
        }

        // 4. Render readable text with 5x7 font
        let motion_str = match self.motion {
            Motion::Still => "STILL",
            Motion::Sweep => "SWEEP",
            Motion::Noise => "NOISE",
        };
        let line1 = format!("RESOLUTION: {}x{}", self.width, self.height);
        let line2 = format!("FPS: {}", self.fps);
        let line3 = format!("FRAME: {}", self.frame_count);
        let line4 = format!("MOTION: {motion_str}");

        let text_x = 32;
        let mut text_y = (top_h + 32).min(h.saturating_sub(120));
        draw_text(frame, &line1, text_x, text_y, 2, [255, 255, 255, 255]);
        text_y += 24;
        draw_text(frame, &line2, text_x, text_y, 2, [255, 255, 255, 255]);
        text_y += 24;
        draw_text(frame, &line3, text_x, text_y, 2, [255, 255, 255, 255]);
        text_y += 24;
        draw_text(frame, &line4, text_x, text_y, 2, [255, 255, 255, 255]);

        // 5. Big digital frame counter
        let counter_str = format!("{:06}", self.frame_count % 1_000_000);
        let counter_scale = 6;
        let counter_x = w.saturating_sub(6 * (5 * counter_scale + 2 * counter_scale) + 40);
        let counter_y = (top_h + 32).min(h.saturating_sub(70));
        // Backing box for big counter
        let box_w = 6 * (5 * counter_scale + 2 * counter_scale) + 16;
        let box_h = 7 * counter_scale + 16;
        for y in counter_y.saturating_sub(8)..(counter_y + box_h).min(h) {
            let row = &mut frame.data[y * stride..(y + 1) * stride];
            for x in counter_x.saturating_sub(8)..(counter_x + box_w).min(w) {
                row[x * 4..x * 4 + 4].copy_from_slice(&[16, 16, 16, 255]);
            }
        }
        draw_text(
            frame,
            &counter_str,
            counter_x,
            counter_y,
            counter_scale,
            [0, 255, 0, 255],
        );

        // 6. Motion-specific features
        match self.motion {
            Motion::Still => {}
            Motion::Sweep => {
                // Moving vertical bar: 8 px/frame
                let bar_w = 16usize;
                let sweep_pos = ((self.frame_count * 8) % (w as u64)) as usize;
                for y in 0..h {
                    let row = &mut frame.data[y * stride..(y + 1) * stride];
                    for bx in 0..bar_w {
                        let x = (sweep_pos + bx) % w;
                        row[x * 4..x * 4 + 4].copy_from_slice(&[0, 165, 255, 255]);
                    }
                }
            }
            Motion::Noise => {
                // Noise patch seeded per frame via xorshift
                let patch_w = (w / 3).clamp(64, 256);
                let patch_h = (h / 4).clamp(64, 256);
                let patch_x = (w / 2).saturating_sub(patch_w / 2);
                let patch_y = (top_h + 20).min(h.saturating_sub(patch_h));

                let mut rng = (self.frame_count ^ 0x9e3779b97f4a7c15).max(1);
                for y in patch_y..(patch_y + patch_h).min(h) {
                    let row = &mut frame.data[y * stride..(y + 1) * stride];
                    for x in patch_x..(patch_x + patch_w).min(w) {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        let val = (rng & 0xff) as u8;
                        row[x * 4..x * 4 + 4].copy_from_slice(&[val, val, val, 255]);
                    }
                }
            }
        }

        // 7. 64x64 Key toggled square at top-left (0, 0)
        let sq_color = self.square();
        let sq_w = 64.min(w);
        let sq_h = 64.min(h);
        for y in 0..sq_h {
            let row = &mut frame.data[y * stride..(y + 1) * stride];
            for x in 0..sq_w {
                row[x * 4..x * 4 + 4].copy_from_slice(&sq_color);
            }
        }
    }
}

impl Default for Pattern {
    fn default() -> Self {
        Self::new(1920, 1080, 60, Motion::Still)
    }
}

impl CpuCapture for Pattern {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn grab(&mut self, frame: &mut Bgra, timeout: Duration) -> anyhow::Result<bool> {
        let frame_interval = if self.fps > 0 {
            Duration::from_secs_f64(1.0 / self.fps as f64)
        } else {
            Duration::from_millis(16)
        };

        let now = Instant::now();
        if let Some(last) = self.last_grab_time {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < frame_interval {
                let remaining = frame_interval - elapsed;
                let sleep_dur = remaining.min(timeout);
                if sleep_dur > Duration::ZERO {
                    std::thread::sleep(sleep_dur);
                }
            }
        }
        self.last_grab_time = Some(Instant::now());

        let changed = match self.motion {
            Motion::Still => {
                if self.dirty {
                    self.dirty = false;
                    true
                } else {
                    false
                }
            }
            Motion::Sweep | Motion::Noise => true,
        };

        if changed {
            self.render(frame);
            self.frame_count += 1;
        }

        Ok(changed)
    }

    fn display(&self) -> DisplayRect {
        DisplayRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    fn name(&self) -> String {
        "test pattern".into()
    }
}

// Minimal 5x7 bitmap font for ASCII 32 (' ') ..= 126 ('~')
// 5 columns per glyph, bit 0 is top row, bit 6 is bottom row.
fn glyph_5x7(c: char) -> [u8; 5] {
    match c {
        '0' => [0x3E, 0x51, 0x49, 0x45, 0x3E],
        '1' => [0x00, 0x42, 0x7F, 0x40, 0x00],
        '2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => [0x21, 0x41, 0x45, 0x4B, 0x31],
        '4' => [0x18, 0x14, 0x12, 0x7F, 0x10],
        '5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => [0x3C, 0x4A, 0x49, 0x49, 0x30],
        '7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => [0x06, 0x49, 0x49, 0x29, 0x1E],
        'A' => [0x7E, 0x11, 0x11, 0x11, 0x7E],
        'B' => [0x7F, 0x49, 0x49, 0x49, 0x36],
        'C' => [0x3E, 0x41, 0x41, 0x41, 0x22],
        'D' => [0x7F, 0x41, 0x41, 0x22, 0x1C],
        'E' => [0x7F, 0x49, 0x49, 0x49, 0x41],
        'F' => [0x7F, 0x09, 0x09, 0x09, 0x01],
        'G' => [0x3E, 0x41, 0x49, 0x49, 0x7A],
        'H' => [0x7F, 0x08, 0x08, 0x08, 0x7F],
        'I' => [0x00, 0x41, 0x7F, 0x41, 0x00],
        'J' => [0x20, 0x40, 0x41, 0x3F, 0x01],
        'K' => [0x7F, 0x08, 0x14, 0x22, 0x41],
        'L' => [0x7F, 0x40, 0x40, 0x40, 0x40],
        'M' => [0x7F, 0x02, 0x0C, 0x02, 0x7F],
        'N' => [0x7F, 0x04, 0x08, 0x10, 0x7F],
        'O' => [0x3E, 0x41, 0x41, 0x41, 0x3E],
        'P' => [0x7F, 0x09, 0x09, 0x09, 0x06],
        'Q' => [0x3E, 0x41, 0x51, 0x21, 0x5E],
        'R' => [0x7F, 0x09, 0x19, 0x29, 0x46],
        'S' => [0x46, 0x49, 0x49, 0x49, 0x31],
        'T' => [0x01, 0x01, 0x7F, 0x01, 0x01],
        'U' => [0x3F, 0x40, 0x40, 0x40, 0x3F],
        'V' => [0x1F, 0x20, 0x40, 0x20, 0x1F],
        'W' => [0x7F, 0x20, 0x18, 0x20, 0x7F],
        'X' | 'x' => [0x63, 0x14, 0x08, 0x14, 0x63],
        'Y' => [0x07, 0x08, 0x70, 0x08, 0x07],
        'Z' => [0x61, 0x51, 0x49, 0x45, 0x43],
        ':' => [0x00, 0x36, 0x36, 0x00, 0x00],
        '-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        '.' => [0x00, 0x60, 0x60, 0x00, 0x00],
        '/' => [0x20, 0x10, 0x08, 0x04, 0x02],
        _ => [0x00, 0x00, 0x00, 0x00, 0x00],
    }
}

fn draw_text(
    frame: &mut Bgra,
    text: &str,
    start_x: usize,
    start_y: usize,
    scale: usize,
    color: [u8; 4],
) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let stride = frame.stride;

    let mut cursor_x = start_x;
    for ch in text.chars() {
        if ch == ' ' {
            cursor_x += 4 * scale;
            continue;
        }
        let upper_ch = ch.to_ascii_uppercase();
        let cols = glyph_5x7(upper_ch);
        for (col_idx, &col_byte) in cols.iter().enumerate() {
            for row_idx in 0..7 {
                if (col_byte & (1 << row_idx)) != 0 {
                    for sy in 0..scale {
                        let py = start_y + row_idx * scale + sy;
                        if py >= h {
                            continue;
                        }
                        let row = &mut frame.data[py * stride..(py + 1) * stride];
                        for sx in 0..scale {
                            let px = cursor_x + col_idx * scale + sx;
                            if px >= w {
                                continue;
                            }
                            row[px * 4..px * 4 + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
        cursor_x += (5 + 1) * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bars_and_checkerboard_correct() {
        let mut p = Pattern::new(640, 480, 30, Motion::Still);
        let mut frame = Bgra::default();
        let changed = p.grab(&mut frame, Duration::from_millis(50)).unwrap();
        assert!(changed);

        let stride = frame.stride;
        let y = 100usize; // within top half (480 / 2 = 240)

        // Check the 8 colour bars across x (avoiding top-left 64x64 square and right checkerboard)
        // Bar 1: Yellow (bar_idx 1: x around 640 * 1.5 / 8 = 120)
        let x_yellow = 120usize;
        let px = &frame.data[y * stride + x_yellow * 4..y * stride + x_yellow * 4 + 4];
        assert_eq!(px, [0, 191, 191, 255], "Yellow bar mismatch");

        // Bar 3: Green (bar_idx 3: x around 640 * 3.5 / 8 = 280)
        let x_green = 280usize;
        let px = &frame.data[y * stride + x_green * 4..y * stride + x_green * 4 + 4];
        assert_eq!(px, [0, 191, 0, 255], "Green bar mismatch");

        // Bar 5: Red (bar_idx 5: x around 640 * 5.5 / 8 = 440)
        let x_red = 440usize;
        let px = &frame.data[y * stride + x_red * 4..y * stride + x_red * 4 + 4];
        assert_eq!(px, [0, 0, 191, 255], "Red bar mismatch");

        // Check 1-pixel alternating checkerboard in top-right
        let cb_x = 640 - 30;
        let cb_y = 120;
        let p0 = &frame.data[cb_y * stride + cb_x * 4..cb_y * stride + cb_x * 4 + 4];
        let p1 = &frame.data[cb_y * stride + (cb_x + 1) * 4..cb_y * stride + (cb_x + 1) * 4 + 4];
        assert!(
            (p0 == [255, 255, 255, 255] && p1 == [0, 0, 0, 255])
                || (p0 == [0, 0, 0, 255] && p1 == [255, 255, 255, 255]),
            "Checkerboard not alternating adjacent pixels"
        );
    }

    #[test]
    fn test_text_and_counter_rendered() {
        let mut p = Pattern::new(640, 480, 30, Motion::Still);
        let mut frame = Bgra::default();
        p.grab(&mut frame, Duration::from_millis(50)).unwrap();

        let stride = frame.stride;
        // In the bottom half (y in 240..480), there should be white text pixels and green counter pixels
        let mut found_white = false;
        let mut found_green = false;

        for y in 240..480 {
            for x in 0..640 {
                let px = &frame.data[y * stride + x * 4..y * stride + x * 4 + 4];
                if px == [255, 255, 255, 255] {
                    found_white = true;
                }
                if px == [0, 255, 0, 255] {
                    found_green = true;
                }
            }
        }

        assert!(found_white, "No white text pixels found in bottom half");
        assert!(found_green, "No green counter pixels found in bottom half");
    }

    #[test]
    fn test_motion_modes_differ() {
        // 1. Still: frame 1 returns true, frame 2 returns false
        let mut p_still = Pattern::new(320, 240, 60, Motion::Still);
        let mut frame = Bgra::default();
        assert!(p_still.grab(&mut frame, Duration::from_millis(20)).unwrap());
        assert!(!p_still.grab(&mut frame, Duration::from_millis(20)).unwrap());

        // 2. Sweep: frame 1 and frame 2 both return true and have different pixels
        let mut p_sweep = Pattern::new(320, 240, 60, Motion::Sweep);
        let mut frame1 = Bgra::default();
        let mut frame2 = Bgra::default();
        assert!(p_sweep
            .grab(&mut frame1, Duration::from_millis(20))
            .unwrap());
        assert!(p_sweep
            .grab(&mut frame2, Duration::from_millis(20))
            .unwrap());
        assert_ne!(frame1.data, frame2.data, "Sweep frames should differ");

        // 3. Noise: frame 1 and frame 2 both return true and have different pixels
        let mut p_noise = Pattern::new(320, 240, 60, Motion::Noise);
        let mut f1 = Bgra::default();
        let mut f2 = Bgra::default();
        assert!(p_noise.grab(&mut f1, Duration::from_millis(20)).unwrap());
        assert!(p_noise.grab(&mut f2, Duration::from_millis(20)).unwrap());
        assert_ne!(f1.data, f2.data, "Noise frames should differ");
    }

    #[test]
    fn test_square_toggles_on_key() {
        let mut p = Pattern::new(320, 240, 60, Motion::Still);
        assert_eq!(p.square(), [0, 0, 0, 255]); // black initially

        let mut frame = Bgra::default();
        p.grab(&mut frame, Duration::from_millis(20)).unwrap();
        // Still mode is now clean (dirty = false)
        assert!(!p.grab(&mut frame, Duration::from_millis(20)).unwrap());

        // Key down toggles square to white
        p.input(&Event::Key {
            vk: 0x41,
            down: true,
            modifiers: 0,
        });
        assert_eq!(p.square(), [255, 255, 255, 255]);

        // In Still mode, key down makes it dirty again, so grab returns true!
        assert!(p.grab(&mut frame, Duration::from_millis(20)).unwrap());
        // Verify (0,0) pixel is now white
        assert_eq!(&frame.data[0..4], &[255, 255, 255, 255]);

        // Key up does not toggle
        p.input(&Event::Key {
            vk: 0x41,
            down: false,
            modifiers: 0,
        });
        assert_eq!(p.square(), [255, 255, 255, 255]);

        // Next key down toggles back to black
        p.input(&Event::Key {
            vk: 0x41,
            down: true,
            modifiers: 0,
        });
        assert_eq!(p.square(), [0, 0, 0, 255]);
    }

    #[test]
    fn test_pacing_sleeps_roughly_right_amount() {
        let mut p = Pattern::new(320, 240, 20, Motion::Sweep); // 20 fps = 50 ms interval
        let mut frame = Bgra::default();

        p.grab(&mut frame, Duration::from_millis(100)).unwrap();
        let start = Instant::now();
        p.grab(&mut frame, Duration::from_millis(100)).unwrap();
        let elapsed = start.elapsed();

        // 50 ms interval; should sleep around 50 ms (allow 35 ms - 150 ms margin for OS scheduler)
        assert!(
            elapsed >= Duration::from_millis(35) && elapsed <= Duration::from_millis(150),
            "Pacing slept for {:?}, expected ~50ms",
            elapsed
        );
    }
}
