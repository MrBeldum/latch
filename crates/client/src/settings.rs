//! The stream controls, shared by Settings and the in-stream panel so the
//! two cannot drift apart. Rows only: the caller supplies the group.

use crate::config::{Codec, Quality, Resolution, StreamSettings, MIN_FPS};
use latch_ui::{self as ui, space};

/// Frame rates the list offers, besides the display's own: the common
/// monitor refresh rates up to the fastest made.
const FRAME_RATES: [u32; 15] = [
    30, 60, 90, 100, 120, 144, 165, 175, 200, 240, 280, 300, 360, 480, 500,
];

/// The bitrate slider's stops, in Mbps: fine where a step is a large share
/// of the stream, coarse where it is not.
const BITRATES: [u32; 35] = [
    2, 3, 4, 5, 6, 8, 10, 12, 15, 20, 25, 30, 35, 40, 45, 50, 60, 70, 80, 90, 100, 120, 150, 175,
    200, 250, 300, 350, 400, 500, 600, 700, 800, 900, 1000,
];

/// "50 Mbps", "1000 Mbps".
pub fn mbps_label(kbps: u32) -> String {
    format!("{} Mbps", kbps / 1000)
}

fn hint(q: Quality) -> &'static str {
    match q {
        Quality::Competitive => "1080p at this display's refresh rate, up to 360 fps: the most frames and the least delay.",
        Quality::Auto => "This display's own size at its own refresh rate. Only the PC's encoder and the network set limits; a relay or a long round trip never lowers the frame rate or the bitrate.",
        Quality::Cinematic => "This display's size up to 4K, up to 120 fps, at half again Balanced's bitrate: the sharpest picture.",
        Quality::Custom => "Your own resolution, frame rate and bitrate, up to what the PC's encoder takes.",
    }
}

/// Edit `s` in place on a display of `native` pixels refreshing at `hz`
/// (0 when unknown); true when anything changed.
pub fn stream_controls(
    ui: &mut egui::Ui,
    s: &mut StreamSettings,
    native: (u32, u32),
    hz: u32,
) -> bool {
    let before = s.clone();
    let mut quality = s.quality;
    let options = Quality::ALL.map(|q| (q, q.label()));
    ui::setting_block(ui, "Quality", Some(hint(s.quality)), |ui| {
        if ui::segmented(ui, &options, &mut quality) {
            s.choose(quality);
        }
    });
    // A profile's rows show what it asks for here; changing one starts
    // Custom from there.
    let shown = crate::path::effective_for(s, native, hz);
    let customise = |s: &mut StreamSettings| {
        if s.quality != Quality::Custom {
            s.quality = Quality::Custom;
            s.resolution = shown.resolution;
            s.fps = shown.fps;
            s.bitrate_kbps = shown.bitrate_kbps;
        }
    };
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Resolution",
        Some("Match screen fills this display exactly. The 16:9 sizes leave bars on a display of another shape. A PC whose encoder takes less gets the largest picture it does, in proportion."),
        |ui| {
            let mut resolution = shown.resolution;
            ui::select(ui, "stream-resolution", resolution.describe(native), 200.0, |ui| {
                for r in Resolution::ALL {
                    ui::select_option(ui, &mut resolution, r, r.describe(native));
                }
            });
            if resolution != shown.resolution {
                customise(s);
                s.resolution = resolution;
                s.custom_resolution = Some(resolution);
            }
        },
    );
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Frame rate",
        Some("Above this display's refresh rate, frames are made that it never shows."),
        |ui| {
            let mut fps = shown.fps;
            let mut rates = FRAME_RATES.to_vec();
            if hz >= MIN_FPS && !rates.contains(&hz) {
                rates.push(hz);
                rates.sort_unstable();
            }
            let label = |r: u32| {
                if r == hz {
                    format!("{r} fps · this display")
                } else {
                    format!("{r} fps")
                }
            };
            ui::select(ui, "stream-fps", format!("{fps} fps"), 150.0, |ui| {
                for choice in rates {
                    ui::select_option(ui, &mut fps, choice, label(choice));
                }
            });
            if fps != shown.fps {
                customise(s);
                s.fps = fps;
                s.custom_fps = Some(fps);
            }
        },
    );
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Bitrate",
        Some("The most the encoder spends while the picture moves; a still screen uses less, and the PC comes down from it whenever the network carries less."),
        |ui| {
            let mbps = shown.bitrate_kbps / 1000;
            let nearest = BITRATES
                .iter()
                .enumerate()
                .min_by_key(|(_, &b)| b.abs_diff(mbps))
                .map_or(0, |(i, _)| i as u32);
            let mut stop = nearest;
            let last = BITRATES.len() as u32 - 1;
            if ui::slider(ui, &mut stop, 0..=last, 180.0, "Bitrate").changed() && stop != nearest {
                customise(s);
                s.bitrate_kbps = BITRATES[stop.min(last) as usize] * 1000;
                s.custom_bitrate_kbps = Some(s.bitrate_kbps);
            }
            let now = if s.quality == Quality::Custom {
                s.bitrate_kbps
            } else {
                shown.bitrate_kbps
            };
            ui::slider_value(ui, &mbps_label(now));
        },
    );
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Codec",
        Some("AV1 and HEVC are sharper than H.264 for the same bitrate. Auto takes the best both ends have; a choice either end lacks falls back to H.264."),
        |ui| {
            ui::segmented(
                ui,
                &[
                    (Codec::Auto, "Auto"),
                    (Codec::Av1, "AV1"),
                    (Codec::Hevc, "HEVC"),
                    (Codec::H264, "H.264"),
                ],
                &mut s.codec,
            );
        },
    );
    ui::row_separator(ui);
    ui::toggle_row(
        ui,
        &mut s.mic,
        "Send my microphone",
        Some("The PC hears it as a microphone: on Linux by itself, on Windows and macOS through a virtual cable (VB-CABLE, BlackHole) installed there."),
    );
    ui::row_separator(ui);
    let p = crate::session::plan(s, native, hz, &crate::session::HostLimits::default());
    ui.add_space(space::MD);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::SM;
        ui::section_label(ui, "Next stream");
        ui.add_space(space::XS);
        ui::tag(ui, None, &format!("{} × {}", p.width, p.height));
        ui::tag(ui, None, &format!("{} fps", p.fps));
        ui::tag(ui, None, &mbps_label(p.bitrate_kbps));
    });
    ui.add_space(space::MD);
    *s != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    fn harness(
        s: StreamSettings,
        hz: u32,
    ) -> egui_kittest::Harness<'static, (bool, StreamSettings)> {
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(640.0, 520.0))
            .build_state(
                move |ctx, state: &mut (bool, StreamSettings)| {
                    if !state.0 {
                        latch_ui::apply(ctx);
                        state.0 = true;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        stream_controls(ui, &mut state.1, (1920, 1080), hz);
                    });
                },
                (false, s),
            )
    }

    #[test]
    fn frame_rate_selector_switches_quality_to_custom() {
        let mut harness = harness(StreamSettings::default(), 144);
        harness.run();
        assert_eq!(harness.state().1.quality, Quality::Auto);
        harness
            .get_all_by_label("Frame rate")
            .into_iter()
            .find(|n| n.role() == egui::accesskit::Role::ComboBox)
            .expect("frame rate combo box")
            .click();
        harness.run();
        // The display's own rate is marked, and the list runs to 500.
        harness.get_by_label("144 fps · this display");
        harness.get_by_label("500 fps").click();
        harness.run();
        let s = &harness.state().1;
        assert_eq!((s.fps, s.quality), (500, Quality::Custom));
        assert_eq!(
            s.resolution,
            Resolution::Native,
            "the rest is what Balanced asked for"
        );
        assert_eq!(s.bitrate_kbps, 61_967, "1080p144");
    }

    #[test]
    fn custom_settings_preserved_across_profile_switches() {
        let mut harness = harness(
            StreamSettings {
                quality: Quality::Custom,
                resolution: Resolution::P720,
                fps: 30,
                bitrate_kbps: 15_000,
                ..Default::default()
            },
            60,
        );
        harness.run();
        harness.get_by_label("Balanced").click();
        harness.run();
        assert_eq!(harness.state().1.quality, Quality::Auto);

        harness.get_by_label("Custom").click();
        harness.run();
        let s = &harness.state().1;
        assert_eq!(s.quality, Quality::Custom);
        assert_eq!(
            (s.resolution, s.bitrate_kbps, s.fps),
            (Resolution::P720, 15_000, 30)
        );
        for label in ["Competitive", "Cinematic", "Balanced", "Custom"] {
            harness.get_by_label(label).click();
            harness.run();
        }
        let s = &harness.state().1;
        assert_eq!(
            (s.resolution, s.bitrate_kbps, s.fps),
            (Resolution::P720, 15_000, 30)
        );
    }

    #[test]
    fn the_bitrate_stops_run_from_2_to_1000_mbps_in_order() {
        assert_eq!((BITRATES[0], BITRATES[BITRATES.len() - 1]), (2, 1000));
        assert!(BITRATES.windows(2).all(|w| w[0] < w[1]));
        assert!(FRAME_RATES.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            FRAME_RATES[FRAME_RATES.len() - 1],
            latch_proto::control::MAX_FPS
        );
    }
}
