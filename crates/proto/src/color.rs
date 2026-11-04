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
        self == Self::default()
            || self
                == Self {
                    bit_depth: 10,
                    ..Self::default()
                }
            || (self.primaries == 9
                && self.transfer == 16
                && self.matrix == 9
                && !self.full_range
                && self.bit_depth == 10
                && self.hdr.is_some())
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
    fn sei_is_escaped_and_has_both_payloads() {
        let nal = hevc_hdr_sei(HdrMetadata::default());
        assert_eq!(&nal[..8], &[0, 0, 0, 1, 78, 1, 137, 24]);
        assert!(nal.windows(2).any(|w| w == [144, 4]));
        assert!(!nal[6..]
            .windows(3)
            .any(|w| w[0] == 0 && w[1] == 0 && w[2] < 3));
    }
}
