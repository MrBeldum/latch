use latch_codec::h264::{
    nal::{access_units, nal_units},
    CodecError, Decoder,
};

#[derive(Default)]
struct Bits {
    data: Vec<u8>,
    pos: usize,
}
impl Bits {
    fn bit(&mut self, value: bool) {
        if self.pos.is_multiple_of(8) {
            self.data.push(0);
        }
        if value {
            let last = self.data.len() - 1;
            self.data[last] |= 0x80 >> (self.pos % 8);
        }
        self.pos += 1;
    }
    fn bits(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit(value & (1 << i) != 0);
        }
    }
    fn ue(&mut self, value: u32) {
        let v = u64::from(value) + 1;
        let n = 63 - v.leading_zeros();
        for _ in 0..n {
            self.bit(false);
        }
        for i in (0..=n).rev() {
            self.bit(v & (1 << i) != 0);
        }
    }
    fn se(&mut self, value: i32) {
        self.ue(if value > 0 {
            value as u32 * 2 - 1
        } else {
            value.unsigned_abs() * 2
        });
    }
    fn align(&mut self) {
        while !self.pos.is_multiple_of(8) {
            self.bit(false);
        }
    }
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.bit(true);
        self.align();
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for b in self.data {
            if zeros == 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }
}

fn sps(profile: u32, wm: u32, hm: u32, crop: [u32; 4], refs: u32, gaps: bool) -> Vec<u8> {
    let mut b = Bits::default();
    b.bits(profile, 8);
    b.bits(0xc0, 8);
    b.bits(40, 8);
    b.ue(0);
    if profile == 100 {
        b.ue(1);
        b.ue(0);
        b.ue(0);
        b.bit(false);
        b.bit(false);
    }
    b.ue(0);
    b.ue(2);
    b.ue(refs);
    b.bit(gaps);
    b.ue(wm - 1);
    b.ue(hm - 1);
    b.bit(true);
    b.bit(true);
    b.bit(crop != [0; 4]);
    if crop != [0; 4] {
        for c in crop {
            b.ue(c);
        }
    }
    b.bit(false);
    b.nal(0x67)
}
fn pps(redundant: bool, groups: u32, cabac: bool, weighted: bool) -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0);
    b.ue(0);
    b.bit(cabac);
    b.bit(false);
    b.ue(groups);
    b.ue(0);
    b.ue(0);
    b.bit(weighted);
    b.bits(0, 2);
    b.se(0);
    b.se(0);
    b.se(0);
    b.bit(true);
    b.bit(false);
    b.bit(redundant);
    b.nal(0x68)
}
fn header(first: u32, frame: u32, idr: bool, redundant: Option<u32>, long: bool) -> Bits {
    let mut b = Bits::default();
    b.ue(first);
    b.ue(if idr { 2 } else { 0 });
    b.ue(0);
    b.bits(frame, 4);
    if idr {
        b.ue(0);
    }
    if let Some(r) = redundant {
        b.ue(r);
    }
    if !idr {
        b.bit(false);
        b.bit(long);
        if long {
            b.ue(2);
            b.ue(0);
            b.ue(3);
        }
    }
    if idr {
        b.bit(false);
        b.bit(long);
    } else {
        b.bit(false);
    }
    b.se(0);
    b.ue(1);
    b
}
fn pcm(b: &mut Bits, index: u32, wm: u32, flat: bool) {
    b.ue(25);
    b.align();
    for y in 0..16 {
        for x in 0..16 {
            b.bits(
                if flat {
                    128
                } else {
                    index % wm * 16 + x + index / wm * 16 + y
                },
                8,
            );
        }
    }
    for plane in 0..2 {
        for y in 0..8 {
            for x in 0..8 {
                b.bits(
                    if flat {
                        128
                    } else if plane == 0 {
                        64 + index % wm * 8 + x
                    } else {
                        192 + index / wm * 8 + y
                    },
                    8,
                );
            }
        }
    }
}
fn picture(
    profile: u32,
    wm: u32,
    hm: u32,
    crop: [u32; 4],
    order: &[u32],
    redundant: bool,
) -> Vec<u8> {
    let mut out = sps(profile, wm, hm, crop, 16, false);
    out.extend(pps(redundant, 0, false, false));
    for &i in order {
        let mut b = header(i, 0, true, redundant.then_some(0), false);
        pcm(&mut b, i, wm, false);
        out.extend(b.nal(0x65));
    }
    out
}

#[test]
fn pcm_planes_cropping_aso_and_profile_subset() {
    for profile in [66, 77, 100] {
        let data = picture(profile, 2, 2, [1, 0, 1, 0], &[3, 1, 2, 0], false);
        assert_eq!(access_units(&data).len(), 1);
        let mut decoder = Decoder::new();
        let p = decoder.decode(&data).unwrap().unwrap();
        assert_eq!((p.width, p.height), (30, 30));
        for y in 0..30 {
            for x in 0..30 {
                assert_eq!(p.y[y * p.y_stride + x], (x + y + 4) as u8);
            }
        }
        for y in 0..15 {
            for x in 0..15 {
                assert_eq!(p.u[y * p.c_stride + x], (65 + x) as u8);
                assert_eq!(p.v[y * p.c_stride + x], (193 + y) as u8);
            }
        }
    }
}

#[test]
fn pcm_keeps_previous_qp_for_the_next_macroblock() {
    let mut data = sps(66, 2, 1, [0; 4], 1, false);
    data.extend(pps(false, 0, false, false));
    let mut b = header(0, 0, true, None, false);
    pcm(&mut b, 0, 2, true);
    b.ue(3);
    b.ue(0);
    b.se(0);
    b.bits(1, 6);
    b.bit(false);
    b.bit(true);
    data.extend(b.nal(0x65));
    if let Some(dir) = std::env::var_os("H264_SYNTAX_DUMP_DIR") {
        std::fs::write(std::path::Path::new(&dir).join("pcm-qp.h264"), &data).unwrap();
    }
    let mut decoder = Decoder::new();
    let p = decoder.decode(&data).unwrap().unwrap();
    assert!(p.y[..16].iter().all(|&v| v == 128));
    assert!(p.y[16..32].iter().all(|&v| v == 129));
}

#[test]
fn redundant_pictures_are_ignored_and_overlaps_rejected() {
    let mut data = picture(66, 1, 1, [0; 4], &[0], true);
    let mut b = header(0, 0, true, Some(1), false);
    pcm(&mut b, 0, 1, true);
    data.extend(b.nal(0x65));
    let mut decoder = Decoder::new();
    assert!(decoder.decode(&data).unwrap().is_some());
    let good = picture(66, 1, 1, [0; 4], &[0], false);
    let mut bad = picture(66, 2, 1, [0; 4], &[0], false);
    let nals = nal_units(&bad);
    let repeat = nals.last().unwrap().to_vec();
    bad.extend_from_slice(&[0, 0, 1]);
    bad.extend(repeat);
    assert!(decoder.decode(&bad).is_err());
    assert!(decoder.decode(&good).unwrap().is_some());
}

#[test]
fn sixteen_references_sliding_window_and_frame_num_wrap() {
    let mut decoder = Decoder::new();
    let p = decoder
        .decode(&picture(66, 1, 1, [0; 4], &[0], false))
        .unwrap()
        .unwrap()
        .clone();
    for frame in 1..=40 {
        let mut b = header(0, frame % 16, false, None, false);
        b.ue(1);
        let au = b.nal(0x41);
        assert_eq!(decoder.decode(&au).unwrap().unwrap(), &p);
    }
}

#[test]
fn idr_long_term_reference_and_large_unrestricted_motion() {
    let mut data = sps(66, 1, 1, [0; 4], 2, false);
    data.extend(pps(false, 0, false, false));
    let mut b = header(0, 0, true, None, true);
    pcm(&mut b, 0, 1, false);
    data.extend(b.nal(0x65));
    let mut decoder = Decoder::new();
    let p = decoder.decode(&data).unwrap().unwrap().clone();
    let mut b = header(0, 1, false, None, true);
    b.ue(0);
    b.ue(0);
    b.se(9000);
    b.se(0);
    b.ue(0);
    let p2 = decoder.decode(&b.nal(0x41)).unwrap().unwrap();
    for y in 0..16 {
        assert!(p2.y[y * p2.y_stride..y * p2.y_stride + 16]
            .iter()
            .all(|&v| v == p.y[y * p.y_stride + 15]));
    }
}

#[test]
fn unsupported_tools_and_allocation_bounds_are_explicit() {
    for params in [
        pps(false, 1, false, false),
        pps(false, 0, true, false),
        pps(false, 0, false, true),
    ] {
        assert!(matches!(
            Decoder::new().decode(&params),
            Err(CodecError::Unsupported(_))
        ));
    }
    for (wm, hm) in [(513, 1), (1, 513), (512, 512)] {
        assert!(Decoder::new()
            .decode(&sps(66, wm, hm, [0; 4], 1, false))
            .is_err());
    }
    for st in [1, 3, 4, 6, 8, 9] {
        let mut data = sps(66, 1, 1, [0; 4], 1, false);
        data.extend(pps(false, 0, false, false));
        let mut b = Bits::default();
        b.ue(0);
        b.ue(st);
        data.extend(b.nal(0x41));
        assert!(matches!(
            Decoder::new().decode(&data),
            Err(CodecError::Unsupported(_))
        ));
    }
}
