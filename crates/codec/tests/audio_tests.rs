use latch_codec::audio::{
    decode, decode_channels, encode, encode_channels, frame_samples_within, max_coded_len,
    FRAME_SAMPLES, MAX_CHANNELS, MAX_SAMPLES,
};

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
            decoded_buf.capacity() <= MAX_SAMPLES * MAX_CHANNELS,
            "Excessive allocation: {} on iteration {i}",
            decoded_buf.capacity()
        );
    }
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// One of several kinds of `channels`-channel sound, `n` samples per channel.
fn surround_signal(kind: usize, channels: usize, n: usize, state: &mut u64) -> Vec<i16> {
    (0..n * channels)
        .map(|i| {
            let (t, c) = ((i / channels) as f64, i % channels);
            match kind {
                0 => 0,
                1 => ((t * 0.05 * (c + 1) as f64).sin() * 12_000.0) as i16,
                // Every channel the same: pairs decorrelate to silence.
                2 => ((t * 0.07).sin() * 20_000.0) as i16,
                3 => {
                    if (i / channels + c) % 2 == 0 {
                        i16::MIN
                    } else {
                        i16::MAX
                    }
                }
                4 if c == 3 => 0, // a silent LFE among noise
                _ => xorshift(state) as i16,
            }
        })
        .collect()
}

#[test]
fn every_channel_count_round_trips_bit_exact_within_its_bound() {
    let mut state = 0x5eed_0f_5ca1ab1eu64;
    for channels in 1..=MAX_CHANNELS {
        for n in [0, 1, 2, 3, 17, 60, 96, 120, 240, 480, MAX_SAMPLES] {
            for kind in 0..6 {
                let pcm = surround_signal(kind, channels, n, &mut state);
                let mut coded = vec![0xaa]; // appends, never overwrites
                encode_channels(&pcm, channels, &mut coded);
                assert_eq!(coded[0], 0xaa);
                let coded = &coded[1..];
                assert!(
                    coded.len() <= max_coded_len(channels, n),
                    "{channels} ch, {n} samples, kind {kind}: {} bytes",
                    coded.len()
                );
                let mut decoded = Vec::new();
                let got = decode_channels(coded, &mut decoded).unwrap();
                assert_eq!(got, channels);
                assert_eq!(decoded, pcm, "{channels} ch, {n} samples, kind {kind}");
            }
        }
    }
}

#[test]
fn stereo_stays_version_1_byte_for_byte() {
    // What hosts sent before version 2 existed: old viewers keep decoding it.
    let mut state = 0x0123_4567_89ab_cdefu64;
    for kind in 0..6 {
        let pcm = surround_signal(kind, 2, 240, &mut state);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        encode(&pcm, &mut a);
        encode_channels(&pcm, 2, &mut b);
        assert_eq!(a, b);
        assert_eq!(a[0] >> 4, 1);
    }
    // A pinned frame (5.1.0's encoder made the same bytes): a 4-sample ramp
    // on the left, silence on the right.
    let mut coded = Vec::new();
    encode(&[0, 0, 1, 0, 2, 0, 3, 0], &mut coded);
    assert_eq!(coded, [0x10, 4, 0, 0xa0, 0x00, 0x02, 0x06, 0x00]);
}

#[test]
fn frames_sized_by_the_bound_fit_a_datagram_payload() {
    // The audio packet's room for one coded frame.
    let room = 1_200 - 16 - 2;
    for channels in 1..=MAX_CHANNELS {
        let n = frame_samples_within(channels, room);
        assert!(max_coded_len(channels, n) <= room);
        assert!(max_coded_len(channels, n + 1) > room);
    }
    assert_eq!(frame_samples_within(2, room), 294);
    assert_eq!(frame_samples_within(6, room), 97);
    assert_eq!(frame_samples_within(8, room), 73);
}

#[test]
fn version_2_rejects_what_it_cannot_be() {
    let mut pcm = Vec::new();
    let mut coded = Vec::new();
    encode_channels(&[1, 2, 3, 4, 5, 6], 6, &mut coded);
    assert_eq!(coded[0], 0x25);
    decode_channels(&coded, &mut pcm).unwrap();
    // A mode for a fourth pair of a six-channel frame.
    let mut bad = coded.clone();
    bad[3] |= 0x40;
    assert!(decode_channels(&bad, &mut pcm).is_err());
    // Nine-plus channels and unknown versions.
    for first in [0x28, 0x2f, 0x30, 0x00] {
        let mut bad = coded.clone();
        bad[0] = first;
        assert!(decode_channels(&bad, &mut pcm).is_err(), "{first:#x}");
    }
    for len in 0..coded.len() {
        assert!(decode_channels(&coded[..len], &mut pcm).is_err(), "{len}");
    }
}
