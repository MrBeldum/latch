//! The platform-free half of the Windows encoder: what comes out of a Media
//! Foundation encoder as an access unit, and how it is made into what the
//! viewer needs, and how the rate controller's buffer is sized. Kept apart
//! from `windows/` so that it is tested on every CI platform, not only where
//! the encoder runs.
//!
//! A viewer joining at a keyframe needs the parameter sets in band before
//! the slices: SPS and PPS for H.264, VPS, SPS and PPS for HEVC. Media
//! Foundation encoders differ: some put them in the first access unit, some
//! only in the output type's sequence header (Annex B, `avcC` or `hvcC`),
//! some in both.
#![cfg_attr(not(windows), allow(dead_code))]

use anyhow::{bail, ensure, Context, Result};
use std::borrow::Cow;

const START: [u8; 4] = [0, 0, 0, 1];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    H264,
    Hevc,
}

impl Format {
    /// The type in a NAL unit's header.
    pub fn nal_type(self, nal: &[u8]) -> u8 {
        match self {
            Self::H264 => nal[0] & 31,
            Self::Hevc => (nal[0] >> 1) & 63,
        }
    }

    /// The parameter sets a keyframe carries, in stream order: SPS, PPS;
    /// VPS, SPS, PPS.
    fn parameter_sets(self) -> &'static [u8] {
        match self {
            Self::H264 => &[7, 8],
            Self::Hevc => &[32, 33, 34],
        }
    }

    fn is_delimiter(self, kind: u8) -> bool {
        kind == if self == Self::H264 { 9 } else { 35 }
    }

    fn is_idr(self, kind: u8) -> bool {
        match self {
            Self::H264 => kind == 5,
            Self::Hevc => kind == 19 || kind == 20,
        }
    }

    fn is_cra(self, kind: u8) -> bool {
        self == Self::Hevc && kind == 21
    }
}

/// The NAL units of an Annex B stream, without their start codes. Bytes
/// before the first start code are ignored, and so are the zero bytes
/// before a start code (a NAL unit never ends in one).
pub fn nals(data: &[u8]) -> Nals<'_> {
    Nals {
        data,
        next: next_start(data, 0).map(|(_, payload)| payload),
    }
}

pub struct Nals<'a> {
    data: &'a [u8],
    next: Option<usize>,
}

/// `(where the start code begins, where the NAL unit begins)`.
fn next_start(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            // No start code can begin at i, i + 1 or i + 2.
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, i + 3));
        } else {
            i += 1;
        }
    }
    None
}

impl<'a> Iterator for Nals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let start = self.next?;
            let (end, after) = match next_start(self.data, start) {
                Some((code, payload)) => (code, Some(payload)),
                None => (self.data.len(), None),
            };
            self.next = after;
            let mut nal = &self.data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

/// `data` as Annex B: borrowed when it already is, otherwise converted from
/// an `avcC` or `hvcC` record or from 4-byte NAL lengths.
pub fn annex_b(format: Format, data: &[u8]) -> Result<Cow<'_, [u8]>> {
    if data.is_empty() || data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Ok(Cow::Borrowed(data));
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    match format {
        Format::H264 if data[0] == 1 && data.len() >= 7 => {
            // AVCDecoderConfigurationRecord, used by some hardware sequence headers.
            let mut at = 6;
            let sps = data[5] & 31;
            for pass in 0..2 {
                let count = if pass == 0 {
                    sps
                } else {
                    let n = *data.get(at).context("truncated avcC PPS count")?;
                    at += 1;
                    n
                };
                for _ in 0..count {
                    push_prefixed(data, &mut at, &mut out).context("truncated avcC NAL")?;
                }
            }
        }
        Format::Hevc if data[0] == 1 && data.len() >= 23 => {
            // HEVCDecoderConfigurationRecord: 22 bytes, the number of arrays,
            // then per array a type byte, a count and length-prefixed NALs.
            let arrays = data[22];
            let mut at = 23;
            for _ in 0..arrays {
                let head = data.get(at + 1..at + 3).context("truncated hvcC array")?;
                let count = u16::from_be_bytes([head[0], head[1]]);
                at += 3;
                for _ in 0..count {
                    push_prefixed(data, &mut at, &mut out).context("truncated hvcC NAL")?;
                }
            }
        }
        _ => {
            let mut at = 0;
            while at < data.len() {
                let bytes = data.get(at..at + 4).context("truncated NAL length")?;
                let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
                at += 4;
                if length == 0 {
                    bail!("empty length-prefixed NAL");
                }
                let nal = data.get(at..at + length).context("truncated NAL")?;
                at += length;
                out.extend_from_slice(&START);
                out.extend_from_slice(nal);
            }
        }
    }
    Ok(Cow::Owned(out))
}

/// Appends the NAL unit with a 2-byte length at `*at`.
fn push_prefixed(data: &[u8], at: &mut usize, out: &mut Vec<u8>) -> Option<()> {
    let bytes = data.get(*at..*at + 2)?;
    let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    *at += 2;
    let nal = data.get(*at..*at + length)?;
    *at += length;
    out.extend_from_slice(&START);
    out.extend_from_slice(nal);
    Some(())
}

/// Parameter sets learned from an encoder, and the rule that every keyframe
/// leaves with all of them in front of its slices.
pub struct Sequence {
    format: Format,
    /// Parallel to [`Format::parameter_sets`]; empty until seen.
    sets: Vec<Vec<u8>>,
    idr_seen: bool,
}

impl Sequence {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            sets: vec![Vec::new(); format.parameter_sets().len()],
            idr_seen: false,
        }
    }

    /// Forgets the parameter sets, as after the encoder's output type
    /// changed. Whether it has emitted an IDR stays known.
    pub fn clear(&mut self) {
        self.sets.iter_mut().for_each(Vec::clear);
    }

    /// Remembers the parameter sets in `data` (a sequence header in any of
    /// the forms [`annex_b`] reads, or an access unit).
    pub fn learn(&mut self, data: &[u8]) -> Result<()> {
        let bytes = annex_b(self.format, data)?;
        for nal in nals(&bytes) {
            self.store(nal);
        }
        Ok(())
    }

    fn store(&mut self, nal: &[u8]) {
        let kind = self.format.nal_type(nal);
        if let Some(i) = self.format.parameter_sets().iter().position(|&k| k == kind) {
            self.sets[i].clear();
            self.sets[i].extend_from_slice(nal);
        }
    }

    /// The Annex B access unit for one encoder output, and whether it is a
    /// keyframe. `header` is asked for the encoder's sequence header, only
    /// for a keyframe.
    ///
    /// A keyframe is an IDR picture (H.264 type 5; HEVC 19 or 20). An HEVC
    /// CRA (21) counts only while the encoder has never emitted an IDR: with
    /// no B-frames and no leading pictures a CRA is a clean random access
    /// point, so an encoder whose only intra picture is a CRA still serves a
    /// viewer joining at it. Once the encoder has made an IDR, a CRA is an
    /// ordinary intra picture of an open GOP and the force-IDR check in the
    /// pipeline must see it as not a keyframe.
    ///
    /// A keyframe leaves as the access-unit delimiter if any, then every
    /// parameter set (the encoder's own in-band ones win over remembered
    /// ones), then the rest; any other picture leaves as it came.
    pub fn assemble(
        &mut self,
        data: &[u8],
        header: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Result<(Vec<u8>, bool)> {
        let format = self.format;
        let bytes = annex_b(format, data)?;
        let (mut idr, mut cra) = (false, false);
        for nal in nals(&bytes) {
            let kind = format.nal_type(nal);
            idr |= format.is_idr(kind);
            cra |= format.is_cra(kind);
        }
        let key = idr || (cra && !self.idr_seen);
        self.idr_seen |= idr;
        if !key {
            return Ok((bytes.into_owned(), false));
        }
        if let Some(header) = header() {
            if let Err(e) = self.learn(&header) {
                tracing::debug!("encoder sequence header unreadable: {e:#}");
            }
        }
        for nal in nals(&bytes) {
            self.store(nal);
        }
        let wanted = format.parameter_sets();
        for (set, kind) in self.sets.iter().zip(wanted) {
            ensure!(
                !set.is_empty(),
                "encoder keyframe has no parameter set {kind} in band or in its sequence header"
            );
        }
        let sets: usize = self.sets.iter().map(|s| s.len() + START.len()).sum();
        let mut out = Vec::with_capacity(bytes.len() + sets);
        for nal in nals(&bytes).filter(|n| format.is_delimiter(format.nal_type(n))) {
            out.extend_from_slice(&START);
            out.extend_from_slice(nal);
        }
        for set in &self.sets {
            out.extend_from_slice(&START);
            out.extend_from_slice(set);
        }
        for nal in nals(&bytes) {
            let kind = format.nal_type(nal);
            if !format.is_delimiter(kind) && !wanted.contains(&kind) {
                out.extend_from_slice(&START);
                out.extend_from_slice(nal);
            }
        }
        Ok((out, true))
    }
}

/// How many frames' worth of bits the rate controller's buffer holds, in
/// percent. The buffer bounds how far one frame may run over the average:
/// at the target rate a frame of a full buffer takes `percent / 100` frame
/// intervals to send, so that is also the worst serialization delay a frame
/// adds on a link that carries the bitrate. One frame is the strictest and
/// leaves a keyframe at the size of a P-frame, smearing it; 1.5 lets a
/// keyframe or a scene change take half a frame interval more than average
/// (8 ms at 60 fps) and no more.
pub const VBV_PERCENT: u64 = 150;

/// The least buffer a driver is asked for, bits.
const VBV_MIN: u64 = 16_000;

/// The rate controller's buffer for a stream, bits.
pub fn vbv_bits(kbps: u32, fps: u32) -> u32 {
    let bits = u64::from(kbps) * 1000 * VBV_PERCENT / 100 / u64::from(fps.max(1));
    bits.clamp(VBV_MIN, u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const VPS: [u8; 4] = [0x40, 1, 0xaa, 0xbb];
    const SPS: [u8; 4] = [0x42, 1, 0xcc, 0xdd];
    const PPS: [u8; 3] = [0x44, 1, 0xee];
    const IDR: [u8; 5] = [0x26, 1, 0x11, 0x22, 0x33];
    const IDR_N_LP: [u8; 4] = [0x28, 1, 0x44, 0x55];
    const CRA: [u8; 4] = [0x2a, 1, 0x66, 0x77];
    const TRAIL: [u8; 4] = [0x02, 1, 0x88, 0x99];

    fn unit(parts: &[&[u8]]) -> Vec<u8> {
        parts
            .iter()
            .flat_map(|p| START.iter().chain(p.iter()))
            .copied()
            .collect()
    }

    fn kinds(format: Format, data: &[u8]) -> Vec<u8> {
        nals(data).map(|n| format.nal_type(n)).collect()
    }

    #[test]
    fn nal_units_split_on_three_and_four_byte_start_codes() {
        let data = [
            9, 9, // ignored
            0, 0, 1, 0x67, 1, 0, 0, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3, 0, 0, 3, 0, 0, 1,
            0x41,
        ];
        let units: Vec<_> = nals(&data).collect();
        // The zero bytes before a start code are not part of the unit; an
        // emulation-prevention byte inside one is.
        assert_eq!(
            units,
            [&[0x67, 1][..], &[0x68, 2], &[0x65, 3, 0, 0, 3], &[0x41]]
        );
        assert_eq!(nals(&[]).count(), 0);
        assert_eq!(nals(&[0, 0, 1]).count(), 0);
        assert_eq!(nals(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn h264_headers_in_every_form() -> Result<()> {
        let avcc = [1, 66, 0, 31, 255, 225, 0, 2, 0x67, 42, 1, 0, 2, 0x68, 7];
        let sequence = annex_b(Format::H264, &avcc)?;
        assert_eq!(kinds(Format::H264, &sequence), [7, 8]);
        assert_eq!(
            annex_b(Format::H264, &[0, 0, 0, 2, 0x65, 3])?.as_ref(),
            [0, 0, 0, 1, 0x65, 3]
        );
        assert!(annex_b(Format::H264, &[0, 0, 0, 9, 0x65]).is_err());
        assert!(annex_b(Format::H264, &[0, 0, 0, 0]).is_err());
        // Annex B is not copied.
        assert!(matches!(
            annex_b(Format::H264, &[0, 0, 0, 1, 0x65]),
            Ok(Cow::Borrowed(_))
        ));
        Ok(())
    }

    #[test]
    fn h264_keyframe_gets_its_sets_from_the_sequence_header() -> Result<()> {
        let mut sequence = Sequence::new(Format::H264);
        let header = unit(&[&[0x67, 1], &[0x68, 2]]);
        let idr = unit(&[&[0x09, 0xf0], &[0x65, 9, 9]]);
        let (out, key) = sequence.assemble(&idr, || Some(header))?;
        assert!(key);
        // The delimiter stays first.
        assert_eq!(
            out,
            unit(&[&[0x09, 0xf0], &[0x67, 1], &[0x68, 2], &[0x65, 9, 9]])
        );
        // A P-frame is untouched.
        let p = unit(&[&[0x41, 5]]);
        assert_eq!(sequence.assemble(&p, || panic!("not asked"))?, (p, false));
        Ok(())
    }

    #[test]
    fn hevc_keyframe_without_sets_gets_them_from_the_header_in_order() -> Result<()> {
        let mut sequence = Sequence::new(Format::Hevc);
        let header = unit(&[&VPS, &SPS, &PPS]);
        let (out, key) = sequence.assemble(&unit(&[&IDR]), || Some(header))?;
        assert!(key);
        assert_eq!(out, unit(&[&VPS, &SPS, &PPS, &IDR]));
        assert_eq!(kinds(Format::Hevc, &out), [32, 33, 34, 19]);
        // Now remembered: the next IDR needs no header.
        let (out, key) = sequence.assemble(&unit(&[&IDR_N_LP]), || None)?;
        assert!(key);
        assert_eq!(kinds(Format::Hevc, &out), [32, 33, 34, 20]);
        Ok(())
    }

    #[test]
    fn hevc_in_band_sets_win_and_are_not_duplicated() -> Result<()> {
        let mut sequence = Sequence::new(Format::Hevc);
        sequence.learn(&unit(&[&VPS, &SPS, &PPS]))?;
        // The encoder changed its SPS (a new size) and sends it out of order
        // with an SEI after the slice's parameter sets.
        let sps2 = [0x42, 1, 0x01, 0x02, 0x03];
        let sei = [0x4e, 1, 0x04];
        let (out, key) = sequence.assemble(&unit(&[&sps2, &sei, &IDR]), || None)?;
        assert!(key);
        assert_eq!(out, unit(&[&VPS, &sps2, &PPS, &sei, &IDR]));
        Ok(())
    }

    #[test]
    fn hevc_keyframe_with_a_missing_set_is_an_error() {
        let mut sequence = Sequence::new(Format::Hevc);
        let error = sequence
            .assemble(&unit(&[&SPS, &PPS, &IDR]), || None)
            .unwrap_err();
        assert!(format!("{error:#}").contains("32"), "{error:#}");
        // A picture that is not a keyframe never needs them.
        assert!(sequence.assemble(&unit(&[&TRAIL]), || None).is_ok());
    }

    #[test]
    fn hevc_cra_is_a_keyframe_only_until_an_idr_has_been_seen() -> Result<()> {
        let mut sequence = Sequence::new(Format::Hevc);
        let header = unit(&[&VPS, &SPS, &PPS]);
        let (out, key) = sequence.assemble(&unit(&[&CRA]), || Some(header.clone()))?;
        assert!(key, "an encoder that only makes CRAs");
        assert_eq!(kinds(Format::Hevc, &out), [32, 33, 34, 21]);
        let (_, key) = sequence.assemble(&unit(&[&IDR]), || Some(header))?;
        assert!(key);
        let cra = unit(&[&CRA]);
        assert_eq!(sequence.assemble(&cra, || None)?, (cra, false));
        Ok(())
    }

    #[test]
    fn hevc_length_prefixed_output_and_hvcc_headers() -> Result<()> {
        let mut prefixed = Vec::new();
        for nal in [&IDR[..], &TRAIL[..]] {
            prefixed.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            prefixed.extend_from_slice(nal);
        }
        assert_eq!(
            annex_b(Format::Hevc, &prefixed)?.as_ref(),
            unit(&[&IDR, &TRAIL])
        );
        // hvcC: 22 header bytes, 3 arrays of one NAL each.
        let mut hvcc = vec![1];
        hvcc.extend_from_slice(&[0; 21]);
        hvcc.push(3);
        for (kind, nal) in [(32, &VPS[..]), (33, &SPS[..]), (34, &PPS[..])] {
            hvcc.push(0x80 | kind);
            hvcc.extend_from_slice(&1u16.to_be_bytes());
            hvcc.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            hvcc.extend_from_slice(nal);
        }
        assert_eq!(
            annex_b(Format::Hevc, &hvcc)?.as_ref(),
            unit(&[&VPS, &SPS, &PPS])
        );
        let mut sequence = Sequence::new(Format::Hevc);
        let (out, key) = sequence.assemble(&unit(&[&IDR]), || Some(hvcc))?;
        assert!(key);
        assert_eq!(out, unit(&[&VPS, &SPS, &PPS, &IDR]));
        // An array that claims more than the record holds.
        let mut cut = vec![1];
        cut.extend_from_slice(&[0; 21]);
        cut.push(1);
        cut.extend_from_slice(&[0xa0, 0, 1, 0, 9, 0x40]);
        assert!(annex_b(Format::Hevc, &cut).is_err());
        Ok(())
    }

    #[test]
    fn a_stream_change_forgets_the_sets_but_not_the_idr() -> Result<()> {
        let mut sequence = Sequence::new(Format::Hevc);
        sequence.assemble(&unit(&[&VPS, &SPS, &PPS, &IDR]), || None)?;
        sequence.clear();
        assert!(sequence.assemble(&unit(&[&IDR]), || None).is_err());
        let cra = unit(&[&CRA]);
        assert_eq!(sequence.assemble(&cra, || None)?, (cra, false));
        Ok(())
    }

    #[test]
    fn the_rate_buffer_is_a_frame_and_a_half_of_bits() {
        // 20 Mb/s at 60 fps: 333 333 bits a frame.
        assert_eq!(vbv_bits(20_000, 60), 500_000);
        assert_eq!(vbv_bits(8_000, 30), 400_000);
        // Never below what a driver will take, never past u32.
        assert_eq!(vbv_bits(500, 240), VBV_MIN as u32);
        assert_eq!(vbv_bits(u32::MAX, 1), u32::MAX);
        assert_eq!(vbv_bits(1000, 0), 1_500_000);
    }
}
