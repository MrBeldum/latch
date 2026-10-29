//! Annex B byte streams: NAL units and access units.

/// The NAL units of an Annex B byte stream, start codes removed (3- and
/// 4-byte start codes both), emulation prevention bytes still in.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let _ = stream;
    todo!()
}

/// Splits a whole Annex B stream into access units by the rules of
/// H.264 7.4.1.2.3/7.4.1.2.4 (a new picture starts at an AUD, SPS, PPS or
/// SEI after a slice, or at a slice whose first_mb_in_slice is 0, frame_num,
/// pic_parameter_set_id, nal_ref_idc, idr or idr_pic_id differ). For test
/// harnesses that feed conformance streams one picture at a time.
pub fn access_units(stream: &[u8]) -> Vec<&[u8]> {
    let _ = stream;
    todo!()
}
