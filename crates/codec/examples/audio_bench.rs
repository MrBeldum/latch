//! Benchmark for lpcm-rice audio codec.

use latch_codec::audio::{decode, encode, FRAME_SAMPLES};
use std::time::Instant;

fn generate_signals() -> Vec<(&'static str, Vec<i16>)> {
    let mut signals = Vec::new();

    // 1. Silence
    signals.push(("Silence", vec![0i16; FRAME_SAMPLES * 2]));

    // 2. Quiet sine wave (e.g. 440 Hz at -20 dB)
    let mut quiet_sine = vec![0i16; FRAME_SAMPLES * 2];
    for i in 0..FRAME_SAMPLES {
        let t = i as f64 / 48000.0;
        let s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 1500.0;
        quiet_sine[2 * i] = s as i16;
        quiet_sine[2 * i + 1] = s as i16;
    }
    signals.push(("Quiet sine", quiet_sine));

    // 3. Loud music-like signal (sum of harmonics + slight noise)
    let mut music = vec![0i16; FRAME_SAMPLES * 2];
    let mut state: u64 = 0x55aa55aa33cc33cc;
    for i in 0..FRAME_SAMPLES {
        let t = i as f64 / 48000.0;
        let s1 = (2.0 * std::f64::consts::PI * 220.0 * t).sin() * 8000.0;
        let s2 = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 6000.0;
        let s3 = (2.0 * std::f64::consts::PI * 880.0 * t).sin() * 4000.0;
        let s4 = (2.0 * std::f64::consts::PI * 1760.0 * t).sin() * 2000.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let noise = ((state as i32 % 400) - 200) as f64;
        let total_l = s1 + s2 + s3 + s4 + noise;
        let total_r = s1 - s2 + s3 - s4 + noise;
        music[2 * i] = total_l.clamp(-32768.0, 32767.0) as i16;
        music[2 * i + 1] = total_r.clamp(-32768.0, 32767.0) as i16;
    }
    signals.push(("Music-like", music));

    // 4. White noise
    let mut white_noise = vec![0i16; FRAME_SAMPLES * 2];
    for s in &mut white_noise {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *s = (state as i32 % 65536 - 32768) as i16;
    }
    signals.push(("White noise", white_noise));

    signals
}

fn bench_signal(pcm: &[i16], iterations: usize) -> (usize, f64, f64) {
    let mut enc_buf = Vec::new();
    let mut dec_buf = Vec::new();

    // Warm-up
    for _ in 0..10 {
        enc_buf.clear();
        encode(pcm, &mut enc_buf);
        dec_buf.clear();
        decode(&enc_buf, &mut dec_buf).expect("decode");
    }

    let encoded_bytes = enc_buf.len();

    // Measure encode
    let start_enc = Instant::now();
    for _ in 0..iterations {
        enc_buf.clear();
        encode(pcm, &mut enc_buf);
    }
    let elapsed_enc = start_enc.elapsed();
    let us_enc = elapsed_enc.as_secs_f64() * 1_000_000.0 / (iterations as f64);

    // Measure decode
    let start_dec = Instant::now();
    for _ in 0..iterations {
        dec_buf.clear();
        decode(&enc_buf, &mut dec_buf).expect("decode");
    }
    let elapsed_dec = start_dec.elapsed();
    let us_dec = elapsed_dec.as_secs_f64() * 1_000_000.0 / (iterations as f64);

    (encoded_bytes, us_enc, us_dec)
}

fn main() {
    println!("=== Latch Codec Audio (lpcm-rice) Benchmark ===");
    println!("Frame duration: 10 ms (480 stereo samples = 1920 uncompressed bytes)\n");

    println!(
        "{:<15} | {:<12} | {:<12} | {:<12} | {:<14}",
        "Signal", "Raw (bytes)", "Coded (bytes)", "Ratio", "Enc+Dec (µs)"
    );
    println!(
        "{:-<15}-+-{:-<12}-+-{:-<12}-+-{:-<12}-+-{:-<14}",
        "", "", "", "", ""
    );

    let raw_bytes = FRAME_SAMPLES * 2 * 2;
    let signals = generate_signals();

    for (name, pcm) in signals {
        let (coded_len, us_enc, us_dec) = bench_signal(&pcm, 10_000);
        let ratio = (coded_len as f64) / (raw_bytes as f64) * 100.0;
        let total_us = us_enc + us_dec;
        println!(
            "{:<15} | {:<12} | {:<12} | {:>5.1}%      | {:>6.2} µs (enc: {:.2}, dec: {:.2})",
            name, raw_bytes, coded_len, ratio, total_us, us_enc, us_dec
        );
    }
}
