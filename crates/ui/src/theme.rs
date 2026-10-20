//! Design tokens: colour, type, spacing, radii and elevation, and the egui
//! style built from them.
//!
//! One dark theme, deliberately. The client spends its life next to a black
//! video surface and the product site (bardbro.com) is black, white and
//! hairlines, so the app is too: neutral surfaces a step apart, one text
//! colour in three strengths, white for the primary action and for "on",
//! and colour only where it carries meaning — green, amber and red for
//! status, and the logo's blue for keyboard focus, selection and
//! progress. Screens take every colour, size and distance from here; a
//! retune is a change to this file.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Margin,
    Stroke, TextStyle, Vec2, Visuals,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// Every colour the two windows and the stream overlay use.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Window background (elevation 0).
    pub bg: Color32,
    /// Grouped lists, cards and panels on the background (elevation 1).
    pub surface: Color32,
    /// Controls on a surface: buttons, fields, the off track of a switch.
    pub raised: Color32,
    /// A raised control under the pointer, and a hovered list row.
    pub raised_hover: Color32,
    /// A raised control being pressed.
    pub pressed: Color32,
    /// Sunken areas: logs and blocks of code.
    pub well: Color32,
    /// Floating panels over the stream picture.
    pub overlay: Color32,
    /// Separators between rows and around groups.
    pub hairline: Color32,
    /// Outlines of controls.
    pub border: Color32,
    /// Outlines of hovered controls.
    pub border_strong: Color32,
    /// Primary text. 16:1 on `bg`.
    pub text: Color32,
    /// Secondary text: descriptions, metadata. 7:1 on `surface`.
    pub text_secondary: Color32,
    /// Tertiary text: labels, captions, small print. At least 4.5:1 on
    /// every surface up to `raised_hover`.
    pub text_tertiary: Color32,
    /// Disabled labels. Exempt from contrast minimums; never the only cue.
    pub text_disabled: Color32,
    /// The primary button's fill and a switch that is on.
    pub primary: Color32,
    pub primary_hover: Color32,
    pub primary_pressed: Color32,
    /// Text and marks drawn on `primary`.
    pub on_primary: Color32,
    /// The logo's blue, lightened to read on the dark surfaces. Keyboard
    /// focus, text selection, progress, and the logo's own square. Never a
    /// fill behind text.
    pub accent: Color32,
    /// Healthy, online, ready.
    pub success: Color32,
    /// Needs attention; still works.
    pub warning: Color32,
    /// Broken, failed, destructive. Text on dark surfaces.
    pub danger: Color32,
    /// Fill behind white text on a destructive button.
    pub danger_fill: Color32,
}

/// The palette. Contrast ratios are checked by the tests at the bottom.
pub const PALETTE: Palette = Palette {
    bg: Color32::from_rgb(0x0a, 0x0a, 0x0a),
    surface: Color32::from_rgb(0x11, 0x11, 0x11),
    raised: Color32::from_rgb(0x1a, 0x1a, 0x1a),
    raised_hover: Color32::from_rgb(0x24, 0x24, 0x24),
    pressed: Color32::from_rgb(0x2e, 0x2e, 0x2e),
    well: Color32::from_rgb(0x06, 0x06, 0x06),
    overlay: Color32::from_rgba_premultiplied(0x08, 0x08, 0x08, 0xeb),
    hairline: Color32::from_rgb(0x24, 0x24, 0x24),
    border: Color32::from_rgb(0x36, 0x36, 0x36),
    border_strong: Color32::from_rgb(0x4a, 0x4a, 0x4a),
    text: Color32::from_rgb(0xed, 0xed, 0xed),
    text_secondary: Color32::from_rgb(0xa1, 0xa1, 0xa1),
    text_tertiary: Color32::from_rgb(0x8c, 0x8c, 0x8c),
    text_disabled: Color32::from_rgb(0x5e, 0x5e, 0x5e),
    primary: Color32::from_rgb(0xed, 0xed, 0xed),
    primary_hover: Color32::from_rgb(0xd4, 0xd4, 0xd4),
    primary_pressed: Color32::from_rgb(0xbd, 0xbd, 0xbd),
    on_primary: Color32::from_rgb(0x0a, 0x0a, 0x0a),
    accent: Color32::from_rgb(0x4f, 0x8f, 0xe8),
    success: Color32::from_rgb(0x4c, 0xc3, 0x8a),
    warning: Color32::from_rgb(0xf0, 0xb2, 0x49),
    danger: Color32::from_rgb(0xff, 0x63, 0x69),
    danger_fill: Color32::from_rgb(0xcd, 0x2b, 0x31),
};

// ---------------------------------------------------------------------------
// Spacing, sizes, radii
// ---------------------------------------------------------------------------

/// The spacing scale, on a 4-point grid. Layout uses these and nothing in
/// between.
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
    pub const XXXL: f32 = 48.0;
}

/// Fixed sizes of recurring elements.
pub mod size {
    use egui::Vec2;
    /// Height of a button, field or select.
    pub const CONTROL: f32 = 32.0;
    /// Height of a compact control (the stream toolbar, menus).
    pub const CONTROL_SM: f32 = 28.0;
    /// The smallest target a pointer is asked to hit (WCAG 2.5.8).
    pub const HIT_MIN: f32 = 24.0;
    /// Height of the window's top bar.
    pub const TOP_BAR: f32 = 48.0;
    /// Height of the stream toolbar.
    pub const TOOLBAR: f32 = 44.0;
    /// Minimum height of a row in a list or a settings group.
    pub const ROW: f32 = 44.0;
    /// Diameter of a status dot.
    pub const DOT: f32 = 8.0;
    /// Width of the switch control.
    pub const SWITCH: Vec2 = Vec2::new(36.0, 20.0);
}

/// Corner radii. Small and consistent: controls are nearly square, as on
/// the product site; only floating panels round off more.
pub mod radius {
    /// Buttons, fields, keycaps, tags.
    pub const SM: u8 = 4;
    /// Cards, grouped lists, notices, menus.
    pub const MD: u8 = 6;
    /// Floating panels over the stream.
    pub const LG: u8 = 10;
}

/// Maximum widths of the content column, per kind of page.
pub mod column {
    /// Lists: the machine list.
    pub const WIDE: f32 = 880.0;
    /// Forms: settings, sharing.
    pub const NARROW: f32 = 720.0;
}

/// Shadows, by how far a layer floats above the window.
pub mod elevation {
    use egui::{epaint::Shadow, Color32};
    /// Menus, dropdowns, tooltips.
    pub const POPUP: Shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    /// Panels floating over the stream picture.
    pub const OVERLAY: Shadow = Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(160),
    };
}

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

/// Font families registered by [`apply`]. [`FontFamily::Proportional`] is
/// Geist Regular and [`FontFamily::Monospace`] is Geist Mono Regular; these
/// name the heavier cuts.
pub const MEDIUM: &str = "Geist-Medium";
pub const SEMIBOLD: &str = "Geist-SemiBold";
pub const MONO_MEDIUM: &str = "GeistMono-Medium";

/// The type scale, in points. Every piece of text uses one of these.
pub mod text {
    /// A page's title: "Machines", "Settings", a machine's name.
    pub const PAGE_TITLE: f32 = 22.0;
    /// A card or dialog title.
    pub const TITLE: f32 = 15.0;
    /// Running text and controls.
    pub const BODY: f32 = 14.0;
    /// Descriptions under a label, metadata, small print.
    pub const CAPTION: f32 = 12.5;
    /// Mono uppercase section labels, as on the product site.
    pub const LABEL: f32 = 11.0;
    /// Mono data: addresses, versions, rates, logs.
    pub const MONO: f32 = 12.5;
    /// The pairing PIN.
    pub const DISPLAY: f32 = 32.0;
    /// Tracking of the mono uppercase labels (0.06 em at 11 pt).
    pub const LABEL_TRACKING: f32 = 0.66;
    /// Tracking of page titles (-0.02 em).
    pub const TITLE_TRACKING: f32 = -0.44;
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn mono_medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO_MEDIUM.into()))
}

/// Named text styles beyond egui's built-ins.
pub const TITLE: &str = "title";
pub const CAPTION: &str = "caption";

pub fn title() -> TextStyle {
    TextStyle::Name(TITLE.into())
}

pub fn caption() -> TextStyle {
    TextStyle::Name(CAPTION.into())
}

/// A stroke with an explicitly `f32` width, so float literals do not fall
/// back through `f64` (a warning under `-D warnings`).
pub fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke::new(width, color)
}

/// The scroll bar of a menu or panel whose content is cut off by the
/// window: the window's usual floating bar, but with its handle showing at
/// rest, so a clipped list says there is more below without a hover.
pub fn clipped_scroll() -> egui::style::ScrollStyle {
    egui::style::ScrollStyle {
        floating: true,
        bar_width: 6.0,
        floating_width: 3.0,
        floating_allocated_width: 0.0,
        bar_inner_margin: space::XXS,
        foreground_color: true,
        dormant_background_opacity: 0.0,
        dormant_handle_opacity: 0.45,
        active_background_opacity: 0.0,
        active_handle_opacity: 0.6,
        interact_background_opacity: 0.2,
        interact_handle_opacity: 0.8,
        ..egui::style::ScrollStyle::floating()
    }
}

/// Install the fonts and style on a context. Call once at startup.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    let mut style = (*ctx.style()).clone();
    style.visuals = visuals();
    style.text_styles = [
        (TextStyle::Small, regular(text::CAPTION)),
        (TextStyle::Body, regular(text::BODY)),
        (TextStyle::Button, medium(text::BODY)),
        (TextStyle::Heading, semibold(text::PAGE_TITLE)),
        (TextStyle::Monospace, mono(text::MONO)),
        (title(), semibold(text::TITLE)),
        (caption(), regular(text::CAPTION)),
    ]
    .into();
    let sp = &mut style.spacing;
    sp.item_spacing = Vec2::new(space::SM, space::SM);
    sp.button_padding = Vec2::new(space::MD, 6.0);
    sp.interact_size = Vec2::new(40.0, size::CONTROL);
    sp.indent = space::LG;
    sp.slider_width = 200.0;
    sp.slider_rail_height = 4.0;
    sp.text_edit_width = 240.0;
    sp.combo_width = 160.0;
    sp.combo_height = 320.0;
    sp.icon_width = 16.0;
    sp.icon_width_inner = 10.0;
    sp.icon_spacing = 6.0;
    sp.menu_width = 220.0;
    sp.menu_spacing = space::XS;
    sp.window_margin = Margin::same(space::LG as i8);
    sp.menu_margin = Margin::same(space::XS as i8);
    sp.tooltip_width = 320.0;
    sp.scroll.bar_width = 6.0;
    sp.scroll.floating = true;
    sp.scroll.bar_inner_margin = space::XS;
    style.url_in_tooltip = true;
    // Popups, menus and tooltips appear and go at once: a fade would ask
    // for repaints the stream view does not want.
    style.animation_time = 0.1;
    // Latch has one look, dark, whatever the system's appearance. egui
    // keeps a style per theme and picks one from the system's every frame,
    // so on a Mac or PC in Light mode it drew with its stock light style,
    // which has no caption or title text styles: the first caption panicked.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_style_of(egui::Theme::Light, style.clone());
    ctx.set_style_of(egui::Theme::Dark, style);
    // A menu whose button is no longer drawn (the toolbar folded into More,
    // the stream ended, the page changed) would stay open in egui's memory
    // with nothing on screen, blocking the stream's input and Escape.
    ctx.on_end_pass(
        "latch.orphan_menu",
        Arc::new(crate::widgets::close_orphan_menu),
    );
}

fn font(bytes: &'static [u8]) -> Arc<FontData> {
    // Geist sits a hair high on egui's line box; nudge it so text centres
    // in buttons and rows.
    let mut data = FontData::from_static(bytes);
    data.tweak = FontTweak {
        y_offset_factor: 0.04,
        ..Default::default()
    };
    Arc::new(data)
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    // Geist and Geist Mono are bundled so every machine draws the same
    // letters as the product site. egui's own fonts stay at the end of each
    // family as fallbacks for glyphs Geist does not carry.
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), font(bytes));
    };
    add(
        &mut fonts,
        "Geist",
        include_bytes!("../assets/Geist-Regular.ttf"),
    );
    add(
        &mut fonts,
        MEDIUM,
        include_bytes!("../assets/Geist-Medium.ttf"),
    );
    add(
        &mut fonts,
        SEMIBOLD,
        include_bytes!("../assets/Geist-SemiBold.ttf"),
    );
    add(
        &mut fonts,
        "GeistMono",
        include_bytes!("../assets/GeistMono-Regular.ttf"),
    );
    add(
        &mut fonts,
        MONO_MEDIUM,
        include_bytes!("../assets/GeistMono-Medium.ttf"),
    );
    let fallbacks = |family: FontFamily| -> Vec<String> {
        fonts.families.get(&family).cloned().unwrap_or_default()
    };
    let sans = fallbacks(FontFamily::Proportional);
    let monos = fallbacks(FontFamily::Monospace);
    let with = |name: &str, rest: &[String]| {
        let mut v = vec![name.to_string()];
        v.extend(rest.iter().cloned());
        v
    };
    let families = [
        (FontFamily::Proportional, with("Geist", &sans)),
        (FontFamily::Name(MEDIUM.into()), with(MEDIUM, &sans)),
        (FontFamily::Name(SEMIBOLD.into()), with(SEMIBOLD, &sans)),
        (FontFamily::Monospace, with("GeistMono", &monos)),
        (
            FontFamily::Name(MONO_MEDIUM.into()),
            with(MONO_MEDIUM, &monos),
        ),
    ];
    for (family, list) in families {
        fonts.families.insert(family, list);
    }
    // Last of all, the system's Korean and CJK font: none of the above has
    // Hangul, so a Korean PC's error messages or an app named in Korean
    // came out as empty boxes.
    if let Some(bytes) = cjk_fallback() {
        fonts
            .font_data
            .insert(CJK.into(), Arc::new(FontData::from_static(bytes)));
        for list in fonts.families.values_mut() {
            list.push(CJK.into());
        }
    }
    fonts
}

const CJK: &str = "SystemCJK";

/// The system font with Hangul and the CJK ideographs, read once per
/// process (it is 15-20 MB) and kept; None when the machine has none of
/// the usual ones, or the file does not parse (egui would panic on it).
fn cjk_fallback() -> Option<&'static [u8]> {
    static FONT: std::sync::OnceLock<Option<&'static [u8]>> = std::sync::OnceLock::new();
    *FONT.get_or_init(|| {
        let bytes = cjk_candidates().iter().find_map(|p| usable_font(p))?;
        Some(&*Box::leak(bytes.into_boxed_slice()))
    })
}

/// The font file at `path`, if egui will be able to read it.
fn usable_font(path: &std::path::Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    ab_glyph::FontRef::try_from_slice_and_index(&bytes, 0).ok()?;
    Some(bytes)
}

/// Where each OS keeps a font that covers Korean, first choice first. On a
/// Mac the smaller AppleGothic comes before Apple SD Gothic Neo, whose
/// collection of nine weights is 55 MB.
fn cjk_candidates() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    if cfg!(target_os = "macos") {
        vec![
            "/System/Library/Fonts/Supplemental/AppleGothic.ttf".into(),
            "/System/Library/Fonts/AppleSDGothicNeo.ttc".into(),
        ]
    } else if cfg!(windows) {
        let windir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        vec![windir.join("Fonts").join("malgun.ttf")]
    } else {
        [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        ]
        .map(PathBuf::from)
        .to_vec()
    }
}

pub(crate) fn visuals() -> Visuals {
    let p = PALETTE;
    let mut v = Visuals::dark();
    v.override_text_color = None;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = stroke(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(radius::LG);
    v.window_shadow = elevation::OVERLAY;
    v.popup_shadow = elevation::POPUP;
    v.menu_corner_radius = CornerRadius::same(radius::MD);
    v.extreme_bg_color = p.well;
    v.faint_bg_color = p.raised;
    v.code_bg_color = p.well;
    v.hyperlink_color = p.text;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.button_frame = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.selection.bg_fill = p.accent.gamma_multiply(0.30);
    v.selection.stroke = stroke(1.0, p.accent);
    v.text_cursor.stroke = stroke(2.0, p.accent);
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);
    // A focus ring is drawn two points outside its control; a window or a
    // scroll area clipped tight to its content would cut it off at the edge.
    v.clip_rect_margin = 4.0;

    let w = &mut v.widgets;
    let r = CornerRadius::same(radius::SM);
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = stroke(1.0, p.hairline);
    w.noninteractive.fg_stroke = stroke(1.0, p.text_secondary);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = stroke(1.0, p.border);
    w.inactive.fg_stroke = stroke(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = p.raised_hover;
    w.hovered.weak_bg_fill = p.raised_hover;
    w.hovered.bg_stroke = stroke(1.0, p.border_strong);
    w.hovered.fg_stroke = stroke(1.0, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    // egui draws a focused built-in widget (select, slider, menu item) with
    // the active visuals, so this outline is also their focus ring.
    w.active.bg_fill = p.pressed;
    w.active.weak_bg_fill = p.pressed;
    w.active.bg_stroke = stroke(1.5, p.accent);
    w.active.fg_stroke = stroke(1.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.raised_hover;
    w.open.weak_bg_fill = p.raised_hover;
    w.open.bg_stroke = stroke(1.0, p.border_strong);
    w.open.fg_stroke = stroke(1.0, p.text);
    w.open.corner_radius = r;
    v
}

/// WCAG 2 contrast ratio between two opaque colours.
pub fn contrast(fg: Color32, bg: Color32) -> f64 {
    fn lin(c: u8) -> f64 {
        let s = f64::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    let lum = |c: Color32| 0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b());
    let (a, b) = (lum(fg), lum(bg));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AA: f64 = 4.5;
    /// WCAG 1.4.11: meaningful non-text marks (status dots, focus rings).
    const NON_TEXT: f64 = 3.0;

    #[test]
    fn every_text_colour_meets_aa_on_every_surface_it_sits_on() {
        let p = PALETTE;
        let surfaces = [p.bg, p.surface, p.raised, p.raised_hover, Color32::BLACK];
        for bg in surfaces {
            for (name, fg) in [
                ("text", p.text),
                ("secondary", p.text_secondary),
                ("tertiary", p.text_tertiary),
                ("success", p.success),
                ("warning", p.warning),
                ("danger", p.danger),
                ("accent", p.accent),
            ] {
                let c = contrast(fg, bg);
                assert!(c >= AA, "{name} on {bg:?} is {c:.2}:1");
            }
        }
    }

    #[test]
    fn filled_buttons_keep_their_label_readable_in_every_state() {
        let p = PALETTE;
        for fill in [p.primary, p.primary_hover, p.primary_pressed] {
            assert!(contrast(p.on_primary, fill) >= AA, "{fill:?}");
        }
        assert!(contrast(Color32::WHITE, p.danger_fill) >= AA);
    }

    #[test]
    fn focus_and_status_marks_stand_out_from_the_background() {
        let p = PALETTE;
        for mark in [p.accent, p.success, p.warning, p.danger, p.text_tertiary] {
            assert!(contrast(mark, p.bg) >= NON_TEXT, "{mark:?}");
            assert!(contrast(mark, p.surface) >= NON_TEXT, "{mark:?}");
        }
        // The focus ring is drawn outside a control, two points clear of
        // it, so it is always seen against the background.
    }

    #[test]
    fn a_light_mode_system_still_gets_the_dark_style() {
        let ctx = egui::Context::default();
        apply(&ctx);
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            // Captions use a text style only Latch's style has; egui's stock
            // light style panicked on the first one.
            let _ = ctx.run(
                egui::RawInput {
                    system_theme: Some(theme),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        crate::caption(ui, "caption");
                        crate::small_print(ui, "small print");
                    });
                },
            );
            assert_eq!(ctx.theme(), egui::Theme::Dark);
            assert_eq!(ctx.style().visuals.panel_fill, PALETTE.bg);
        }
    }

    #[test]
    fn korean_text_is_drawn_when_the_system_has_a_font_for_it() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
        let has = ctx.fonts(|f| f.has_glyphs(&FontId::proportional(14.0), "연결이 거부되었습니다"));
        assert_eq!(has, cjk_fallback().is_some());
        if cfg!(target_os = "macos") {
            assert!(has, "every Mac has AppleGothic");
        }
    }

    #[test]
    fn a_font_file_that_does_not_parse_is_skipped() {
        let dir = std::env::temp_dir().join(format!("latch-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.ttf");
        std::fs::write(&bad, b"not a font at all").unwrap();
        assert!(usable_font(&bad).is_none());
        assert!(usable_font(&dir.join("missing.ttf")).is_none());
        let geist = include_bytes!("../assets/Geist-Regular.ttf");
        let good = dir.join("good.ttf");
        std::fs::write(&good, geist).unwrap();
        assert_eq!(usable_font(&good).as_deref(), Some(&geist[..]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_scales_step_on_the_grid() {
        for s in [
            space::XS,
            space::SM,
            space::MD,
            space::LG,
            space::XL,
            space::XXL,
            space::XXXL,
        ] {
            assert_eq!(s % 4.0, 0.0, "{s} is off the 4-point grid");
        }
        const { assert!(radius::SM < radius::MD && radius::MD < radius::LG) };
        const { assert!(size::HIT_MIN <= size::SWITCH.y + 4.0) };
        const { assert!(size::CONTROL_SM >= size::HIT_MIN) };
    }
}
