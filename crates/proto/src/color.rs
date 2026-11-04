//! Negotiated CICP colour descriptions. Missing fields mean legacy SDR.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorCapabilities {
    pub hdr: bool,
    /// Display peak luminance, cd/m². Zero means unknown (HDR stays off).
    pub peak_nits: u32,
    pub ten_bit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorDescription {
    /// ITU-T H.273 CICP identifiers (709 = 1; 2020 = 9).
    pub primaries: u8,
    /// 709 = 1; SMPTE ST 2084 = 16.
    pub transfer: u8,
    /// 709 = 1; BT.2020 non-constant luminance = 9.
    pub matrix: u8,
    pub full_range: bool,
    pub bit_depth: u8,
    pub hdr: Option<HdrMetadata>,
    /// Where the 4:2:0 chroma samples sit. Hosts before this field all
    /// produced left-sited chroma, which is also H.273's unsignalled default.
    #[serde(default)]
    pub chroma: ChromaSiting,
}

/// H.273 ChromaSampleLocType for 4:2:0: `Left` (type 0) is co-sited with the
/// even luma columns and midway between rows; `Center` (type 1) is midway in
/// both directions. Windows' video processor (`*_LEFT_*` colour spaces) and
/// VideoToolbox's Main10 VUI both use `Left`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChromaSiting {
    #[default]
    Left,
    Center,
}
impl ChromaSiting {
    /// Horizontal offset, in chroma texels, from the centred sampling
    /// position a bilinear sampler uses at `luma_uv` to this siting.
    pub fn texel_offset(self) -> f32 {
        match self {
            Self::Left => 0.25,
            Self::Center => 0.0,
        }
    }
}
impl Default for ColorDescription {
    fn default() -> Self {
        Self {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
            bit_depth: 8,
            hdr: None,
            chroma: ChromaSiting::Left,
        }
    }
}
impl ColorDescription {
    pub fn negotiate(caps: ColorCapabilities, hevc: bool, hdr_source: bool) -> Self {
        if !hevc {
            return Self::default();
        }
        if caps.ten_bit && caps.hdr && caps.peak_nits > 0 && hdr_source {
            Self {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                bit_depth: 10,
                hdr: Some(HdrMetadata::default()),
                ..Self::default()
            }
        } else {
            Self {
                bit_depth: if caps.ten_bit { 10 } else { 8 },
                ..Self::default()
            }
        }
    }
    pub fn is_hdr(self) -> bool {
        self.transfer == 16
    }
    pub fn supported(self) -> bool {
        // Either siting is presentable; the rest must be a known format.
        let s = Self {
            chroma: ChromaSiting::Left,
            ..self
        };
        s == Self::default()
            || s == Self {
                bit_depth: 10,
                ..Self::default()
            }
            || (s.primaries == 9
                && s.transfer == 16
                && s.matrix == 9
                && !s.full_range
                && s.bit_depth == 10
                && s.hdr.is_some())
    }
}

/// ST 2086 units: chromaticities /50000, luminance /10000 cd/m².
/// Live desktop capture has no mastering monitor; use a canonical 1000-nit
/// BT.2020 container, clipping the source to that peak before encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HdrMetadata {
    pub primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    pub max_luminance: u32,
    pub min_luminance: u32,
    pub max_cll: u16,
    pub max_fall: u16,
}
impl Default for HdrMetadata {
    fn default() -> Self {
        Self {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
            white_point: [15635, 16450],
            max_luminance: 10_000_000,
            min_luminance: 50,
            max_cll: 1000,
            max_fall: 1000,
        }
    }
}

/// ST 2084 absolute luminance in cd/m².
pub fn pq_eotf(code: f64) -> f64 {
    let p = code.clamp(0.0, 1.0).powf(1.0 / (2523.0 / 32.0));
    10000.0
        * ((p - 3424.0 / 4096.0).max(0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * p))
            .powf(1.0 / (2610.0 / 16384.0))
}
pub fn pq_oetf(nits: f64) -> f64 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(2610.0 / 16384.0);
    ((3424.0 / 4096.0 + 2413.0 / 128.0 * p) / (1.0 + 2392.0 / 128.0 * p)).powf(2523.0 / 32.0)
}
pub fn bt709_to_bt2020(v: [f64; 3]) -> [f64; 3] {
    mul(
        [
            [0.627404, 0.329283, 0.043313],
            [0.069097, 0.919540, 0.011362],
            [0.016391, 0.088013, 0.895595],
        ],
        v,
    )
}
pub fn bt2020_to_bt709(v: [f64; 3]) -> [f64; 3] {
    mul(
        [
            [1.660491, -0.587641, -0.072850],
            [-0.124550, 1.132900, -0.008349],
            [-0.018151, -0.100579, 1.118730],
        ],
        v,
    )
}
fn mul(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    m.map(|r| r[0] * v[0] + r[1] * v[1] + r[2] * v[2])
}

/// Where an HDR picture's highlights go in SDR: everything up to this
/// fraction of SDR white is untouched, and the rest up to the HDR ceiling is
/// rolled off into the top of the range.
pub const SDR_KNEE: f64 = 0.75;

/// HDR → SDR for linear BT.709 relative to SDR white (1.0), with `peak` the
/// brightest value to keep apart from the ones below it (the 1000-nit HDR
/// ceiling over the display's SDR white). Out-of-gamut colours (negative
/// channels from a wider gamut) are desaturated toward their own luminance,
/// then highlights compress on max(R,G,B): both scale the colour about
/// neutral, so hue is kept and brighter stays brighter instead of every
/// channel clipping at SDR white on its own. Mirrors the Windows capture
/// shader; returns linear light in 0..=1.
pub fn hdr_to_sdr(rgb: [f64; 3], peak: f64) -> [f64; 3] {
    let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    if y <= 0.0 {
        return [0.0; 3];
    }
    let lo = rgb[0].min(rgb[1]).min(rgb[2]);
    let c = if lo < 0.0 {
        let t = y / (y - lo);
        rgb.map(|v| y + (v - y) * t)
    } else {
        rgb
    };
    let m = c[0].max(c[1]).max(c[2]);
    if m <= SDR_KNEE {
        return c;
    }
    let range = 1.0 - SDR_KNEE;
    let x = (m - SDR_KNEE) / range;
    let top = ((peak - SDR_KNEE) / range).max(1.0);
    let g = (x * (1.0 + x / (top * top)) / (1.0 + x)).min(1.0);
    c.map(|v| (v * (SDR_KNEE + range * g) / m).clamp(0.0, 1.0))
}
pub fn srgb_oetf(v: f64) -> f64 {
    if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// A prefix SEI access unit, with emulation prevention, before each HDR IDR.
/// Independent of vendor APIs: identical metadata on AMF, MF and VT.
pub fn hevc_hdr_sei(meta: HdrMetadata) -> Vec<u8> {
    let mut rbsp = vec![137, 24];
    // HEVC mastering_display_colour_volume orders the primaries G, B, R.
    for i in [1, 2, 0] {
        for v in meta.primaries[i] {
            rbsp.extend(v.to_be_bytes());
        }
    }
    for v in meta.white_point {
        rbsp.extend(v.to_be_bytes());
    }
    rbsp.extend(meta.max_luminance.to_be_bytes());
    rbsp.extend(meta.min_luminance.to_be_bytes());
    rbsp.extend([144, 4]);
    rbsp.extend(meta.max_cll.to_be_bytes());
    rbsp.extend(meta.max_fall.to_be_bytes());
    rbsp.push(0x80);
    let mut nal = vec![0, 0, 0, 1, 78, 1];
    let mut zeros = 0;
    for b in rbsp {
        if zeros == 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        nal.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    nal
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_is_default_off() {
        assert_eq!(
            ColorDescription::negotiate(ColorCapabilities::default(), true, true),
            ColorDescription::default()
        );
        let caps = ColorCapabilities {
            hdr: true,
            ten_bit: true,
            peak_nits: 1000,
        };
        assert!(ColorDescription::negotiate(caps, true, true).is_hdr());
        assert_eq!(ColorDescription::negotiate(caps, true, false).bit_depth, 10);
        assert_eq!(ColorDescription::negotiate(caps, false, true).bit_depth, 8);
        assert!(!ColorDescription::negotiate(
            ColorCapabilities {
                peak_nits: 0,
                ..caps
            },
            true,
            true
        )
        .is_hdr());
    }
    #[test]
    fn pq_and_gamut_roundtrip() {
        assert!((pq_oetf(100.0) - 0.5080784215).abs() < 1e-8);
        for n in [0.0, 0.001, 0.1, 1.0, 80.0, 100.0, 1000.0, 10000.0] {
            assert!((pq_eotf(pq_oetf(n)) - n).abs() < 1e-6);
        }
        for rgb in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0; 3]] {
            for (a, b) in bt2020_to_bt709(bt709_to_bt2020(rgb)).into_iter().zip(rgb) {
                assert!((a - b).abs() < 3e-6);
            }
        }
    }
    #[test]
    fn ten_bit_gradient_has_four_times_the_levels() {
        let levels = |span: f64| {
            (0..4096)
                .map(|i| (64.0 + f64::from(i) / 4095.0 * span).round() as u16)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        assert_eq!(levels(219.0), 220);
        assert_eq!(levels(876.0), 877);
        // P010 limited range endpoints in R16Unorm, not 8-bit normalisation.
        assert_eq!(64u16 << 6, 4096);
        assert_eq!(940u16 << 6, 60160);
    }
    #[test]
    fn chroma_siting_is_optional_on_the_wire() {
        let hdr = ColorDescription::negotiate(
            ColorCapabilities {
                hdr: true,
                ten_bit: true,
                peak_nits: 1000,
            },
            true,
            true,
        );
        assert_eq!(hdr.chroma, ChromaSiting::Left);
        let mut json = serde_json::to_value(hdr).unwrap();
        assert_eq!(json["chroma"], "left");
        json.as_object_mut().unwrap().remove("chroma");
        let old: ColorDescription = serde_json::from_value(json).unwrap();
        assert_eq!(old, hdr);
        for chroma in [ChromaSiting::Left, ChromaSiting::Center] {
            assert!(ColorDescription { chroma, ..hdr }.supported());
            assert!(ColorDescription {
                chroma,
                ..Default::default()
            }
            .supported());
        }
    }
    #[test]
    fn hdr_highlights_stay_apart_in_sdr_and_keep_their_hue() {
        let white = 203.0;
        let code =
            |rgb: [f64; 3]| hdr_to_sdr(rgb, 1000.0 / white).map(|v| (255.0 * srgb_oetf(v)).round());
        let neutral: Vec<f64> = [203.0, 400.0, 600.0, 1000.0]
            .iter()
            .map(|n| code([n / white; 3])[0])
            .collect();
        assert!(neutral.windows(2).all(|w| w[0] < w[1]), "{neutral:?}");
        assert!(neutral[0] >= 235.0 && neutral[3] == 255.0, "{neutral:?}");
        // SDR content well below white is untouched.
        assert_eq!(hdr_to_sdr([0.5, 0.25, 0.1], 4.9), [0.5, 0.25, 0.1]);
        let hue = |c: [f64; 3]| (3f64.sqrt() * (c[1] - c[2])).atan2(2.0 * c[0] - c[1] - c[2]);
        let bt2020_green = bt2020_to_bt709([0.0, 1000.0 / white, 0.0]);
        for rgb in [
            [1000.0 / white, 0.0, 0.0],
            [1000.0 / white, 500.0 / white, 0.0],
            [0.2, 0.4, 600.0 / white],
            bt2020_green,
        ] {
            let out = hdr_to_sdr(rgb, 1000.0 / white);
            assert!(out.iter().all(|v| (0.0..=1.0).contains(v)), "{out:?}");
            assert!(out.iter().any(|&v| v > 0.5), "{rgb:?} went dark: {out:?}");
            assert!((hue(out) - hue(rgb)).abs() < 1e-9, "{rgb:?} → {out:?}");
        }
        // Brighter saturated colours stay brighter, not equal.
        let red = |n: f64| hdr_to_sdr([n / white, 0.0, 0.0], 1000.0 / white)[0];
        assert!(red(203.0) < red(400.0) && red(400.0) < red(1000.0));
        assert_eq!(hdr_to_sdr([-0.1, -0.2, -0.3], 4.9), [0.0; 3]);
    }
    #[test]
    fn sei_is_escaped_and_has_both_payloads() {
        let nal = hevc_hdr_sei(HdrMetadata::default());
        assert_eq!(&nal[..8], &[0, 0, 0, 1, 78, 1, 137, 24]);
        assert!(nal.windows(2).any(|w| w == [144, 4]));
        assert!(!nal[6..]
            .windows(3)
            .any(|w| w[0] == 0 && w[1] == 0 && w[2] < 3));
    }
}
