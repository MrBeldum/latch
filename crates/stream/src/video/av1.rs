//! AV1 low-overhead OBU sequence headers (AV1 spec §5.5). Only Main 4:2:0
//! is negotiated; platform decoders still validate the complete bitstream.
use anyhow::{ensure, Context, Result};

struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Bits<'_> {
    fn read(&mut self, n: usize) -> Result<u32> {
        ensure!(
            n <= 32 && self.at + n <= self.bytes.len() * 8,
            "truncated AV1 sequence header"
        );
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | u32::from((self.bytes[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.read(1)? != 0)
    }
}
pub struct Sequence {
    pub width: i32,
    pub height: i32,
    pub config: Vec<u8>,
}
/// Find the sequence OBU without treating AV1 payload as Annex B NAL units.
pub fn sequence(data: &[u8]) -> Result<Option<Sequence>> {
    let mut at = 0;
    while at < data.len() {
        let start = at;
        let header = data[at];
        at += 1;
        ensure!(
            header & 0x81 == 0 && header & 2 != 0,
            "invalid or unsized AV1 OBU"
        );
        if header & 4 != 0 {
            at += 1;
        }
        let mut size = 0u64;
        let mut complete = false;
        for i in 0..8 {
            let byte = *data.get(at).context("truncated AV1 OBU size")?;
            at += 1;
            size |= u64::from(byte & 127) << (i * 7);
            if byte & 128 == 0 {
                complete = true;
                break;
            }
        }
        ensure!(complete, "invalid AV1 OBU size");
        let end = at
            .checked_add(usize::try_from(size)?)
            .context("AV1 OBU size overflow")?;
        let payload = data.get(at..end).context("truncated AV1 OBU")?;
        if (header >> 3) & 15 == 1 {
            return parse(payload, &data[start..end]).map(Some);
        }
        at = end;
    }
    Ok(None)
}
fn parse(payload: &[u8], obu: &[u8]) -> Result<Sequence> {
    let mut b = Bits {
        bytes: payload,
        at: 0,
    };
    let profile = b.read(3)?;
    ensure!(profile == 0, "only AV1 Main profile is supported");
    b.read(1)?; // still_picture
    let reduced = b.flag()?;
    let (mut level, mut tier, mut delay) = (0, 0, 0);
    if reduced {
        level = b.read(5)?;
    } else {
        let mut delay_bits = 0;
        if b.flag()? {
            // timing_info_present
            b.read(32)?;
            b.read(32)?;
            if b.flag()? {
                let mut leading = 0;
                while !b.flag()? {
                    leading += 1;
                    ensure!(leading < 32, "AV1 timing overflow");
                }
                b.read(leading)?;
            }
            if b.flag()? {
                // decoder_model_info_present
                delay_bits = b.read(5)? as usize + 1;
                b.read(32)?;
                b.read(5)?;
                b.read(5)?;
            }
        }
        let display_delay = b.flag()?;
        let count = b.read(5)? + 1;
        for i in 0..count {
            b.read(12)?;
            let op_level = b.read(5)?;
            let op_tier = if op_level > 7 { b.read(1)? } else { 0 };
            if i == 0 {
                level = op_level;
                tier = op_tier;
            }
            if delay_bits != 0 && b.flag()? {
                b.read(delay_bits)?;
                b.read(delay_bits)?;
                b.read(1)?;
            }
            if display_delay && b.flag()? {
                let op_delay = b.read(4)?;
                if i == 0 {
                    delay = 0x10 | op_delay;
                }
            }
        }
    }
    let width_bits = b.read(4)? as usize + 1;
    let height_bits = b.read(4)? as usize + 1;
    let width = b.read(width_bits)? + 1;
    let height = b.read(height_bits)? + 1;
    ensure!(
        width <= 16384 && height <= 16384,
        "AV1 dimensions exceed limit"
    );
    if !reduced && b.flag()? {
        b.read(4)?;
        b.read(3)?;
    }
    b.read(3)?; // superblock, filter intra, intra edge filter
    if !reduced {
        b.read(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = b.flag()?;
        if order_hint {
            b.read(2)?;
        }
        let screen_tools = if b.flag()? { 2 } else { b.read(1)? };
        if screen_tools > 0 && !b.flag()? {
            b.read(1)?;
        }
        if order_hint {
            b.read(3)?;
        }
    }
    b.read(3)?; // superres, CDEF, restoration
    let high_depth = b.read(1)?;
    ensure!(!b.flag()?, "monochrome AV1 is unsupported");
    if b.flag()? {
        let primaries = b.read(8)?;
        let transfer = b.read(8)?;
        let matrix = b.read(8)?;
        ensure!(
            (primaries, transfer, matrix) != (1, 13, 0),
            "AV1 RGB is unsupported"
        );
    }
    b.read(1)?; // color_range
    let chroma_position = b.read(2)?;
    b.read(1)?; // separate_uv_delta_q
    b.read(1)?; // film_grain_params_present
    let mut config = vec![
        0x81,
        level as u8,
        ((tier << 7) | (high_depth << 6) | 0x0c | chroma_position) as u8,
        delay as u8,
    ];
    config.extend_from_slice(obu);
    Ok(Sequence {
        width: width as i32,
        height: height as i32,
        config,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_sequence_and_rejects_truncated_obus() {
        let data = include_bytes!("../../tests/fixtures/gray-bars.av1");
        let seq = sequence(data).unwrap().unwrap();
        assert_eq!((seq.width, seq.height), (64, 64));
        assert_eq!(&seq.config[..4], &[0x81, 0, 0x0d, 0x14]);
        for len in 1..data.len() {
            let _ = sequence(&data[..len]); // malformed input must never panic
        }
        assert!(sequence(&[0x0a, 0xff]).is_err());
    }
}
