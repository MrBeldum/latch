//! The stream controls, shared by Settings and the in-stream panel so the
//! two cannot drift apart. Rows only: the caller supplies the group.

use crate::config::{Codec, Preset, Quality, Resolution, StreamSettings};
use latch_ui::{self as ui, space};

/// The one-line choice at the top: a named profile, or the values as set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Recommended,
    Preset(Preset),
    Custom,
}

impl Profile {
    fn of(s: &StreamSettings) -> Self {
        match (s.quality, s.preset()) {
            (Quality::Auto, _) => Profile::Recommended,
            (Quality::Custom, Some(p)) => Profile::Preset(p),
            (Quality::Custom, None) => Profile::Custom,
        }
    }
}

/// Edit `s` in place; true when anything changed.
pub fn stream_controls(ui: &mut egui::Ui, s: &mut StreamSettings, native: (u32, u32)) -> bool {
    let before = s.clone();
    let mut profile = Profile::of(s);
    let options = [
        (Profile::Recommended, "Recommended"),
        (Profile::Preset(Preset::Smooth), Preset::Smooth.label()),
        (Profile::Preset(Preset::Balanced), Preset::Balanced.label()),
        (Profile::Preset(Preset::Sharp), Preset::Sharp.label()),
        (Profile::Custom, "Custom"),
    ];
    let hint = match profile {
        Profile::Recommended => "This display's own size at 50 Mbps. A relay or a long round trip never lowers the frame rate or the bitrate.".to_string(),
        Profile::Preset(p) => format!("{}.", p.describe()),
        Profile::Custom => "Your own resolution, frame rate and bitrate.".to_string(),
    };
    ui::setting_block(ui, "Quality", Some(&hint), |ui| {
        if ui::segmented(ui, &options, &mut profile) {
            match profile {
                Profile::Recommended => {
                    if s.quality == Quality::Custom && s.preset().is_none() {
                        s.custom_resolution = Some(s.resolution);
                        s.custom_fps = Some(s.fps);
                        s.custom_bitrate_kbps = Some(s.bitrate_kbps);
                    }
                    s.quality = Quality::Auto;
                    s.resolution = Resolution::Native;
                }
                Profile::Preset(p) => {
                    if s.quality == Quality::Custom && s.preset().is_none() {
                        s.custom_resolution = Some(s.resolution);
                        s.custom_fps = Some(s.fps);
                        s.custom_bitrate_kbps = Some(s.bitrate_kbps);
                    }
                    s.apply_preset(p);
                }
                Profile::Custom => {
                    // Restore previously dialed custom values if available,
                    // otherwise start from what Recommended was asking for.
                    s.resolution = s.custom_resolution.unwrap_or(Resolution::Native);
                    s.fps = s.custom_fps.unwrap_or(s.fps);
                    s.bitrate_kbps = s.custom_bitrate_kbps.unwrap_or(50_000);
                    s.quality = Quality::Custom;
                }
            }
        }
    });
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Resolution",
        Some("Match screen fills this display exactly. The 16:9 sizes leave bars on a display of another shape. Below 1080p, a PC that encodes in software keeps a higher frame rate."),
        |ui| {
            let mut resolution = s.resolution;
            ui::select(ui, "stream-resolution", resolution.describe(native), 200.0, |ui| {
                for r in Resolution::ALL {
                    ui::select_option(ui, &mut resolution, r, r.describe(native));
                }
            });
            if resolution != s.resolution {
                s.resolution = resolution;
                s.quality = Quality::Custom;
                s.custom_resolution = Some(resolution);
            }
        },
    );
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Frame rate",
        Some("Above 60 needs a display there that runs that fast."),
        |ui| {
            let mut fps = s.fps;
            ui::select(ui, "stream-fps", format!("{fps} fps"), 120.0, |ui| {
                for choice in [30, 60, 90, 120, 144, 165, 240] {
                    ui::select_option(ui, &mut fps, choice, format!("{choice} fps"));
                }
            });
            if fps != s.fps {
                s.fps = fps;
                s.quality = Quality::Custom;
                s.custom_fps = Some(fps);
            }
        },
    );
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Bitrate",
        Some("The encoder aims at this while the picture moves; a still screen uses less."),
        |ui| {
            let mut mbps = if s.quality == Quality::Auto {
                50
            } else {
                s.bitrate_kbps / 1000
            };
            if ui::slider(ui, &mut mbps, 2..=150, 180.0, "Bitrate").changed() {
                s.quality = Quality::Custom;
                s.bitrate_kbps = mbps * 1000;
                s.custom_bitrate_kbps = Some(s.bitrate_kbps);
            }
            ui::slider_value(ui, &format!("{mbps} Mbps"));
        },
    );
    ui::row_separator(ui);
    ui::setting_row(
        ui,
        "Codec",
        Some("HEVC is sharper for the same bitrate. Auto uses it when both ends can."),
        |ui| {
            ui::segmented(
                ui,
                &[
                    (Codec::Auto, "Auto"),
                    (Codec::Hevc, "HEVC"),
                    (Codec::H264, "H.264"),
                ],
                &mut s.codec,
            );
        },
    );
    ui::row_separator(ui);
    let effective = crate::path::effective(s);
    let (w, h) = effective.resolution.pixels(native);
    ui.add_space(space::MD);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::SM;
        ui::section_label(ui, "Next stream");
        ui.add_space(space::XS);
        ui::tag(ui, None, &format!("{w} × {h}"));
        ui::tag(ui, None, &format!("{} fps", effective.fps));
        ui::tag(ui, None, &format!("{} Mbps", effective.bitrate_kbps / 1000));
    });
    ui.add_space(space::MD);
    *s != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn the_profile_follows_the_values() {
        let mut s = StreamSettings::default();
        assert_eq!(Profile::of(&s), Profile::Recommended);
        s.apply_preset(Preset::Smooth);
        assert_eq!(Profile::of(&s), Profile::Preset(Preset::Smooth));
        s.fps = 144;
        assert_eq!(Profile::of(&s), Profile::Custom);
    }

    #[test]
    fn frame_rate_selector_switches_quality_to_custom() {
        struct State {
            applied: bool,
            s: StreamSettings,
        }
        let state = State {
            applied: false,
            s: StreamSettings::default(),
        };
        assert_eq!(state.s.quality, Quality::Auto);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 400.0))
            .build_state(
                |ctx, state| {
                    if !state.applied {
                        latch_ui::apply(ctx);
                        state.applied = true;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        stream_controls(ui, &mut state.s, (1920, 1080));
                    });
                },
                state,
            );
        harness.run();
        harness
            .get_all_by_label("Frame rate")
            .into_iter()
            .find(|n| n.role() == egui::accesskit::Role::ComboBox)
            .expect("frame rate combo box")
            .click();
        harness.run();
        harness.get_by_label("120 fps").click();
        harness.run();
        assert_eq!(harness.state().s.fps, 120);
        assert_eq!(harness.state().s.quality, Quality::Custom);
    }

    #[test]
    fn custom_settings_preserved_across_preset_switches() {
        struct State {
            applied: bool,
            s: StreamSettings,
        }
        let state = State {
            applied: false,
            s: StreamSettings {
                quality: Quality::Custom,
                resolution: Resolution::P720,
                fps: 30,
                bitrate_kbps: 15_000,
                ..Default::default()
            },
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 400.0))
            .build_state(
                |ctx, state| {
                    if !state.applied {
                        latch_ui::apply(ctx);
                        state.applied = true;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        stream_controls(ui, &mut state.s, (1920, 1080));
                    });
                },
                state,
            );
        harness.run();
        harness.get_by_label("Recommended").click();
        harness.run();
        assert_eq!(harness.state().s.quality, Quality::Auto);

        harness.get_by_label("Custom").click();
        harness.run();
        assert_eq!(harness.state().s.quality, Quality::Custom);
        assert_eq!(harness.state().s.resolution, Resolution::P720);
        assert_eq!(harness.state().s.bitrate_kbps, 15_000);
        assert_eq!(harness.state().s.fps, 30);
        for label in ["Smooth", "Recommended", "Custom"] {
            harness.get_by_label(label).click();
            harness.run();
        }
        assert_eq!(harness.state().s.resolution, Resolution::P720);
        assert_eq!(harness.state().s.bitrate_kbps, 15_000);
        assert_eq!(harness.state().s.fps, 30);
    }
}
