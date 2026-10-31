//! Pure-Rust standard Base64 encoding/decoding (RFC 4648) and RGBA downscaling.

const ENCODE_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes binary data to a standard Base64 string with padding.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let chunks = data.chunks_exact(3);
    let remainder = chunks.remainder();

    for chunk in chunks {
        let b0 = chunk[0];
        let b1 = chunk[1];
        let b2 = chunk[2];
        out.push(ENCODE_TABLE[(b0 >> 2) as usize] as char);
        out.push(ENCODE_TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(ENCODE_TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        out.push(ENCODE_TABLE[(b2 & 0x3f) as usize] as char);
    }

    match remainder.len() {
        1 => {
            let b0 = remainder[0];
            out.push(ENCODE_TABLE[(b0 >> 2) as usize] as char);
            out.push(ENCODE_TABLE[((b0 & 0x03) << 4) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let b0 = remainder[0];
            let b1 = remainder[1];
            out.push(ENCODE_TABLE[(b0 >> 2) as usize] as char);
            out.push(ENCODE_TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            out.push(ENCODE_TABLE[((b1 & 0x0f) << 2) as usize] as char);
            out.push('=');
        }
        _ => {}
    }

    out
}

fn decode_char(c: u8) -> Result<u8, &'static str> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("invalid base64 character"),
    }
}

/// Decodes standard Base64 string into bytes.
pub fn decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = s.trim().as_bytes();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() % 4 != 0 {
        return Err("invalid base64 length");
    }

    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks_exact(4) {
        let (c0, c1, c2, c3) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        let d0 = decode_char(c0)?;
        let d1 = decode_char(c1)?;
        out.push((d0 << 2) | (d1 >> 4));

        if c2 != b'=' {
            let d2 = decode_char(c2)?;
            out.push(((d1 & 0x0f) << 4) | (d2 >> 2));
            if c3 != b'=' {
                let d3 = decode_char(c3)?;
                out.push(((d2 & 0x03) << 6) | d3);
            }
        } else if c3 != b'=' {
            return Err("invalid base64 padding");
        }
    }
    Ok(out)
}

/// Downscales an RGBA buffer so neither width nor height exceeds 96 pixels,
/// preserving aspect ratio and scaling the hotspot coordinates accordingly.
pub fn downscale_rgba(
    width: u32,
    height: u32,
    hot_x: u32,
    hot_y: u32,
    rgba: &[u8],
) -> (u32, u32, u32, u32, Vec<u8>) {
    if width <= 96 && height <= 96 {
        return (width, height, hot_x, hot_y, rgba.to_vec());
    }
    let max_dim = width.max(height);
    let scale = 96.0 / max_dim as f32;
    let new_w = ((width as f32 * scale).round() as u32).max(1);
    let new_h = ((height as f32 * scale).round() as u32).max(1);
    let new_hot_x = ((hot_x as f32 * scale).round() as u32).min(new_w.saturating_sub(1));
    let new_hot_y = ((hot_y as f32 * scale).round() as u32).min(new_h.saturating_sub(1));

    let mut out = Vec::with_capacity((new_w * new_h * 4) as usize);
    for y in 0..new_h {
        let src_y = ((y as f32 + 0.5) * (height as f32 / new_h as f32) - 0.5)
            .clamp(0.0, (height - 1) as f32);
        let y0 = src_y.floor() as u32;
        let y1 = (y0 + 1).min(height - 1);
        let wy1 = src_y - y0 as f32;
        let wy0 = 1.0 - wy1;

        for x in 0..new_w {
            let src_x = ((x as f32 + 0.5) * (width as f32 / new_w as f32) - 0.5)
                .clamp(0.0, (width - 1) as f32);
            let x0 = src_x.floor() as u32;
            let x1 = (x0 + 1).min(width - 1);
            let wx1 = src_x - x0 as f32;
            let wx0 = 1.0 - wx1;

            let idx00 = (y0 * width + x0) as usize * 4;
            let idx01 = (y0 * width + x1) as usize * 4;
            let idx10 = (y1 * width + x0) as usize * 4;
            let idx11 = (y1 * width + x1) as usize * 4;

            for c in 0..4 {
                let p00 = rgba[idx00 + c] as f32;
                let p01 = rgba[idx01 + c] as f32;
                let p10 = rgba[idx10 + c] as f32;
                let p11 = rgba[idx11 + c] as f32;

                let top = p00 * wx0 + p01 * wx1;
                let bot = p10 * wx0 + p11 * wx1;
                let val = (top * wy0 + bot * wy1).round().clamp(0.0, 255.0) as u8;
                out.push(val);
            }
        }
    }
    (new_w, new_h, new_hot_x, new_hot_y, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");

        assert_eq!(decode("").unwrap(), b"");
        assert_eq!(decode("Zg==").unwrap(), b"f");
        assert_eq!(decode("Zm8=").unwrap(), b"fo");
        assert_eq!(decode("Zm9v").unwrap(), b"foo");
        assert_eq!(decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_roundtrip_binary() {
        let binary: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let encoded = encode(&binary);
        let decoded = decode(&encoded).unwrap();
        assert_eq!(binary, decoded);
    }

    #[test]
    fn downscales_when_exceeding_96() {
        let w = 192;
        let h = 96;
        let data = vec![128u8; (w * h * 4) as usize];
        let (nw, nh, hx, hy, ndata) = downscale_rgba(w, h, 20, 10, &data);
        assert_eq!(nw, 96);
        assert_eq!(nh, 48);
        assert_eq!(hx, 10);
        assert_eq!(hy, 5);
        assert_eq!(ndata.len(), (96 * 48 * 4) as usize);
    }

    #[test]
    fn preserves_dimensions_within_96() {
        let w = 32;
        let h = 32;
        let data = vec![255u8; (w * h * 4) as usize];
        let (nw, nh, hx, hy, ndata) = downscale_rgba(w, h, 5, 6, &data);
        assert_eq!(nw, 32);
        assert_eq!(nh, 32);
        assert_eq!(hx, 5);
        assert_eq!(hy, 6);
        assert_eq!(ndata, data);
    }
}
