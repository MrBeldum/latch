//! Annex-B NAL unit splitting and emulation-prevention removal.
//!
//! A H.264 byte stream is a run of start-code-prefixed NAL units
//! (`00 00 01` or `00 00 00 01`). Inside each NAL's payload the encoder
//! inserts `03` after `00 00` whenever the next byte would be `00`,
//! `01`, `02` or `03`, so the RBSP is recovered by stripping every
//! `00 00 03` triple (spec 7.4.1.1).

use std::vec::Vec;

use super::{Error, Result};

/// NAL unit type codes (spec Table 7-1). Only the codes the decoder
/// acts on get named constants.
pub(crate) const NAL_SLICE: u8 = 1;
/// DPA — partitioning is a baseline-legal feature this crate declines.
pub(crate) const NAL_DPA: u8 = 2;
/// DPB.
pub(crate) const NAL_DPB: u8 = 3;
/// DPC.
pub(crate) const NAL_DPC: u8 = 4;
/// IDR slice.
pub(crate) const NAL_IDR: u8 = 5;
/// SEI — skipped.
pub(crate) const NAL_SEI: u8 = 6;
/// Sequence parameter set.
pub(crate) const NAL_SPS: u8 = 7;
/// Picture parameter set.
pub(crate) const NAL_PPS: u8 = 8;
/// Access unit delimiter — skipped.
pub(crate) const NAL_AUD: u8 = 9;
/// End of sequence — flushes the last picture.
pub(crate) const NAL_EOSEQ: u8 = 10;
/// End of stream — flushes the last picture.
pub(crate) const NAL_EOSTREAM: u8 = 11;
/// Filler data — skipped.
pub(crate) const NAL_FILLER: u8 = 12;
/// SPS extension.
pub(crate) const NAL_SPS_EXT: u8 = 13;
/// Auxiliary / extension slices and reserved codes this crate declines.
pub(crate) const NAL_CODED_SLICE_AUX: u8 = 19;
/// Slice without partitioning is legal but beyond baseline scope here
/// (extension/slice-aux and scalable types 14/15/20/21 are all routed
/// to [`Error::Unsupported`]).
///
/// One Annex-B NAL unit: header fields plus the payload span in the
/// source buffer (emulation prevention NOT yet removed).
#[derive(Copy, Clone, Debug)]
pub(crate) struct Nal<'a> {
    /// `nal_ref_idc` (0..=3).
    pub ref_idc: u8,
    /// `nal_unit_type` (0..=31).
    pub unit_type: u8,
    /// Raw NAL payload including emulation-prevention bytes.
    pub payload: &'a [u8],
}

/// Splits an Annex-B stream into NAL units.
///
/// `Err` when the stream contains no start code at all — a raw RBSP
/// without Annex-B framing is not a byte stream. Trailing zero bytes
/// after the last start code (or before the next one) are dropped from
/// each payload, matching how decoders delimit the RBSP.
pub(crate) fn split_annex_b(data: &[u8]) -> Result<Vec<Nal<'_>>> {
    let mut out = Vec::new();
    for payload in super::super::nal::iter_nal_units(data) {
        if out.len() >= 262144 {
            return Err(Error::BadValue("too many NAL units in access unit"));
        }
        if payload[0] & 0x80 != 0 {
            return Err(Error::BadValue("forbidden_zero_bit"));
        }
        out.push(Nal {
            ref_idc: (payload[0] >> 5) & 3,
            unit_type: payload[0] & 31,
            payload: &payload[1..],
        });
    }
    if out.is_empty() {
        return Err(Error::BadValue("Annex B access unit has no NAL units"));
    }
    Ok(out)
}

/// Removes emulation-prevention bytes from a NAL payload, producing the
/// RBSP. The scan runs from `start` so slice headers can hand their
/// `SODB` view in directly.
pub(crate) fn rbsp(payload: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    let mut i = 0;
    while i < payload.len() {
        let b = payload[i];
        if zeros == 2 {
            if b == 3 {
                if payload.get(i + 1).is_none_or(|&next| next > 3) {
                    return Err(Error::BadValue("emulation prevention byte"));
                }
                zeros = 0;
                i += 1;
                continue;
            }
            if b <= 2 {
                return Err(Error::BadValue("unescaped start code in NAL"));
            }
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
        i += 1;
    }
    Ok(out)
}
