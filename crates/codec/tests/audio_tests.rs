use latch_codec::audio::{decode, encode, FRAME_SAMPLES, MAX_SAMPLES};

#[test]
fn test_silence() {
    let pcm = vec![0i16; FRAME_SAMPLES * 2];
    let mut encoded = Vec::new();
    encode(&pcm, &mut encoded);

    let mut decoded = Vec::new();
    decode(&encoded, &mut decoded).expect("decode silence");
    assert_eq!(pcm, decoded);
    // Silence should compress to very few bytes (under 10 bytes)
    assert!(
        encoded.len() <= 10,
        "Silence encoded size was {}",
        encoded.len()
    );
}

#[test]
fn test_constant() {
    for val in [-32768, -1234, -1, 0, 1, 42, 32767] {
        let pcm = vec![val; 200 * 2];
        let mut encoded = Vec::new();
        encode(&pcm, &mut encoded);

        let mut decoded = Vec::new();
        decode(&encoded, &mut decoded).expect("decode constant");
        assert_eq!(pcm, decoded, "constant mismatch for val {val}");
    }
}

#[test]
fn test_one_channel_silent() {
    let mut pcm = vec![0i16; FRAME_SAMPLES * 2];
    for i in 0..FRAME_SAMPLES {
        let val = ((i as f64 * 0.1).sin() * 10000.0) as i16;
        pcm[2 * i] = 0;
        pcm[2 * i + 1] = val;
    }
    let mut encoded = Vec::new();
    encode(&pcm, &mut encoded);

    let mut decoded = Vec::new();
    decode(&encoded, &mut decoded).expect("decode one channel silent");
    assert_eq!(pcm, decoded);
}

#[test]
fn test_sine_and_square_waves() {
    let mut pcm = vec![0i16; FRAME_SAMPLES * 2];
    // Left: sine wave; Right: square wave
    for i in 0..FRAME_SAMPLES {
        let sine = ((i as f64 * 2.0 * std::f64::consts::PI / 48.0).sin() * 20000.0) as i16;
        let square = if (i % 48) < 24 { 15000 } else { -15000 };
        pcm[2 * i] = sine;
        pcm[2 * i + 1] = square;
    }

    let mut encoded = Vec::new();
    encode(&pcm, &mut encoded);

    let mut decoded = Vec::new();
    decode(&encoded, &mut decoded).expect("decode sine/square");
    assert_eq!(pcm, decoded);
}

#[test]
fn test_full_scale_extrema() {
    let mut pcm = Vec::new();
    for _ in 0..100 {
        pcm.push(i16::MIN);
        pcm.push(i16::MAX);
        pcm.push(i16::MAX);
        pcm.push(i16::MIN);
    }
    let mut encoded = Vec::new();
    encode(&pcm, &mut encoded);

    let mut decoded = Vec::new();
    decode(&encoded, &mut decoded).expect("decode extrema");
    assert_eq!(pcm, decoded);
}

#[test]
fn test_white_noise() {
    let mut state: u64 = 0xcafebabe12345678;
    let mut pcm = vec![0i16; FRAME_SAMPLES * 2];
    for s in &mut pcm {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *s = (state as i32 % 65536 - 32768) as i16;
    }

    let mut encoded = Vec::new();
    encode(&pcm, &mut encoded);

    let mut decoded = Vec::new();
    decode(&encoded, &mut decoded).expect("decode white noise");
    assert_eq!(pcm, decoded);
}

#[test]
fn test_various_sample_counts() {
    let mut state: u64 = 0x1337beefcafed00d;
    // Test sizes from 1 up to MAX_SAMPLES
    let sizes = [
        1, 2, 3, 5, 8, 16, 31, 64, 127, 240, 480, 512, 1000, 2048, 4096,
    ];

    for &n in &sizes {
        let mut pcm = vec![0i16; n * 2];
        for s in &mut pcm {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *s = (state as i32 % 20000 - 10000) as i16;
        }

        let mut encoded = Vec::new();
        encode(&pcm, &mut encoded);

        let mut decoded = Vec::new();
        decode(&encoded, &mut decoded).unwrap_or_else(|e| panic!("Failed size {n}: {e}"));
        assert_eq!(pcm, decoded, "Size {n} mismatch");
    }
}

#[test]
fn test_fuzz_100k_inputs_never_panics() {
    let mut state: u64 = 0x9876543210fedcba;
    let mut next_u64 = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    // Valid encoded packet as base for mutations
    let base_pcm = vec![42i16; 64 * 2];
    let mut valid_pkt = Vec::new();
    encode(&base_pcm, &mut valid_pkt);

    let mut decoded_buf = Vec::new();

    for i in 0..100_000 {
        let mode = i % 4;
        let input_data = match mode {
            0 => {
                // Completely random garbage of varying lengths
                let len = (next_u64() % 256) as usize;
                let mut data = vec![0u8; len];
                for b in &mut data {
                    *b = next_u64() as u8;
                }
                data
            }
            1 => {
                // Truncated valid packet
                let truncate_at = (next_u64() as usize) % (valid_pkt.len() + 1);
                valid_pkt[..truncate_at].to_vec()
            }
            2 => {
                // Bit-flipped valid packet
                let mut mutated = valid_pkt.clone();
                let flips = (next_u64() % 4 + 1) as usize;
                for _ in 0..flips {
                    let idx = (next_u64() as usize) % mutated.len();
                    mutated[idx] ^= (1 << (next_u64() % 8)) as u8;
                }
                mutated
            }
            _ => {
                // Large frame count spoofing
                let mut spoofed = valid_pkt.clone();
                // Set sample count to something random, possibly > MAX_SAMPLES
                let count = (next_u64() & 0xffff) as u16;
                spoofed[1..3].copy_from_slice(&count.to_le_bytes());
                spoofed
            }
        };

        // decode must never panic and never allocate > MAX_SAMPLES per channel
        let _ = decode(&input_data, &mut decoded_buf);
        assert!(
            decoded_buf.capacity() <= MAX_SAMPLES * 2 * 2 + 1024,
            "Excessive allocation: {} on iteration {i}",
            decoded_buf.capacity()
        );
    }
}
