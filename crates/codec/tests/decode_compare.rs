use latch_codec::h264::{nal::access_units, Decoder};
use std::{
    fs,
    io::{BufReader, Read},
    path::Path,
};

pub fn compare(bitstream: &Path, reference: &Path) -> Result<usize, String> {
    let data = fs::read(bitstream).map_err(|e| e.to_string())?;
    let mut expected = BufReader::new(fs::File::open(reference).map_err(|e| e.to_string())?);
    let mut decoder = Decoder::new();
    let mut frames = 0;
    let mut pictures = Vec::new();
    for (au, unit) in access_units(&data).into_iter().enumerate() {
        let picture = decoder
            .decode(unit)
            .map_err(|e| format!("AU {au}, frame {frames}: {e}"))?;
        if let Some(p) = picture {
            let p = p.clone();
            let (epoch, poc) = decoder.picture_order().unwrap_or((0, 0));
            pictures.push((epoch, poc, p));
            frames += 1;
        }
    }
    pictures.sort_by_key(|(epoch, poc, _)| (*epoch, *poc));
    for (frame, (_, _, p)) in pictures.iter().enumerate() {
        for (name, plane, stride, w, h) in [
            ("Y", &p.y, p.y_stride, p.width as usize, p.height as usize),
            (
                "U",
                &p.u,
                p.c_stride,
                p.width as usize / 2,
                p.height as usize / 2,
            ),
            (
                "V",
                &p.v,
                p.c_stride,
                p.width as usize / 2,
                p.height as usize / 2,
            ),
        ] {
            let mut row = vec![0; w];
            for (y, actual) in plane.chunks(stride).take(h).enumerate() {
                expected
                    .read_exact(&mut row)
                    .map_err(|e| format!("frame {frame} {name} row {y}: {e}"))?;
                if let Some(x) = row.iter().zip(actual).position(|(a, b)| a != b) {
                    return Err(format!(
                        "frame {frame} {name} ({x},{y}): ours {}, reference {}",
                        actual[x], row[x]
                    ));
                }
            }
        }
    }
    let mut tail = [0];
    if expected.read(&mut tail).map_err(|e| e.to_string())? != 0 {
        return Err(format!(
            "reference has pictures after {frames} decoded frames"
        ));
    }
    if frames == 0 {
        return Err("no pictures".into());
    }
    Ok(frames)
}
