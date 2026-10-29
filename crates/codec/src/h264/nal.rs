//! Annex B byte streams: NAL units and access units.

use super::decode::{golomb::Br, nal::rbsp, pps, sps};

fn start_code(stream: &[u8], from: usize) -> Option<(usize, usize)> {
    let tail = stream.get(from..)?;
    let offset = tail.windows(3).position(|w| w == [0, 0, 1])? + from;
    Some((
        if offset > from && stream[offset - 1] == 0 {
            offset - 1
        } else {
            offset
        },
        offset + 3,
    ))
}

/// Streams byte ranges so a hostile access unit cannot allocate a second
/// start-code table before the decoder applies its NAL-count bound.
fn spans(stream: &[u8]) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let mut next = start_code(stream, 0);
    std::iter::from_fn(move || {
        let (start, begin) = next?;
        next = start_code(stream, begin);
        let mut end = next.map_or(stream.len(), |(start, _)| start);
        while end > begin && stream[end - 1] == 0 {
            end -= 1;
        }
        Some((start, begin, end))
    })
}

pub(crate) fn iter_nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    spans(stream)
        .filter(|(_, begin, end)| begin < end)
        .map(|(_, begin, end)| &stream[begin..end])
}

/// The NAL units of an Annex B byte stream, start codes removed.
/// Both start-code lengths are accepted; emulation prevention stays in.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    iter_nal_units(stream).collect()
}

#[derive(PartialEq, Eq)]
struct PictureKey {
    pps: u32,
    frame_num: u32,
    reference: bool,
    idr: bool,
    idr_id: u32,
    poc: [i64; 3],
}

fn picture_key(
    n: &[u8],
    sets: &[Option<sps::Sps>; 32],
    pictures: &[Option<pps::Pps>; 256],
) -> Option<PictureKey> {
    let header = *n.first()?;
    let data = rbsp(n.get(1..)?).ok()?;
    let mut b = Br::new(&data);
    b.ue().ok()?;
    b.ue().ok()?;
    let pid = b.ue().ok()?;
    let p = pictures.get(pid as usize)?.as_ref()?;
    let s = sets.get(p.sps_id as usize)?.as_ref()?;
    let frame_num = b.bits(s.log2_max_frame_num as usize).ok()?;
    let idr = header & 31 == 5;
    let idr_id = if idr { b.ue().ok()? } else { 0 };
    let mut poc = [0; 3];
    match s.pic_order_cnt_type {
        0 => {
            poc[0] = i64::from(b.bits(s.log2_max_poc_lsb as usize).ok()?);
            if p.bottom_field_pic_order {
                poc[1] = i64::from(b.se().ok()?);
            }
        }
        1 if !s.delta_pic_order_always_zero => {
            poc[1] = i64::from(b.se().ok()?);
            if p.bottom_field_pic_order {
                poc[2] = i64::from(b.se().ok()?);
            }
        }
        _ => {}
    }
    Some(PictureKey {
        pps: pid,
        frame_num,
        reference: header & 0x60 != 0,
        idr,
        idr_id,
        poc,
    })
}

/// Splits Annex B by the first-VCL-of-picture rules of 7.4.1.2.4.
/// Parameter sets and SEI after a VCL start the following access unit;
/// picture identity includes POC, so nonzero first macroblock addresses
/// and arbitrary slice order do not create false boundaries.
pub fn access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut sets = std::array::from_fn(|_| None);
    let mut pictures = std::array::from_fn(|_| None);
    let mut result = Vec::new();
    let mut start = 0;
    let mut seen_vcl = false;
    let mut key = None;
    for (sc, begin, end) in spans(stream) {
        let Some(n) = stream.get(begin..end).filter(|n| !n.is_empty()) else {
            continue;
        };
        let kind = n[0] & 31;
        let vcl = matches!(kind, 1..=5);
        let new_key = if matches!(kind, 1 | 5) {
            picture_key(n, &sets, &pictures)
        } else {
            None
        };
        let boundary = (kind == 9 && sc > start)
            || (seen_vcl && matches!(kind, 6..=9 | 14..=18))
            || (seen_vcl
                && vcl
                && new_key
                    .as_ref()
                    .zip(key.as_ref())
                    .is_some_and(|(a, b)| a != b));
        if boundary {
            result.push(&stream[start..sc]);
            start = sc;
            seen_vcl = false;
            key = None;
        }
        if kind == 7 {
            if let Ok(data) = rbsp(&n[1..]) {
                if let Ok(s) = sps::parse(&data) {
                    let id = s.id as usize;
                    sets[id] = Some(s);
                }
            }
        } else if kind == 8 {
            if let Ok(data) = rbsp(&n[1..]) {
                if let Ok(p) = pps::parse(&data) {
                    let id = p.id as usize;
                    pictures[id] = Some(p);
                }
            }
        }
        if vcl {
            seen_vcl = true;
            if new_key.is_some() {
                key = new_key;
            }
        }
    }
    if start < stream.len() && iter_nal_units(&stream[start..]).next().is_some() {
        result.push(&stream[start..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_start_codes_and_trailing_zeros() {
        let s = [0, 0, 0, 1, 0x67, 0x80, 0, 0, 1, 0x68, 0x80, 0, 0];
        assert_eq!(nal_units(&s), vec![&s[4..6], &s[9..11]]);
        assert!(nal_units(&[0, 0, 1]).is_empty());
        assert!(nal_units(&[]).is_empty());
    }
}
