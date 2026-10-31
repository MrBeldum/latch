//! The components every Latch screen is assembled from.
//!
//! Everything here takes its colours, sizes and distances from
//! [`theme`](crate::theme); nothing carries its own, so the machine list,
//! the sharing page and the stream overlay stay one product. Custom-painted
//! widgets report themselves to accesskit and draw a focus ring, so the
//! whole window works from the keyboard and with a screen reader.

use crate::theme::{self, radius, size, space, stroke, text, PALETTE as P};
use egui::{
    collapsing_header::CollapsingState, Align, Align2, Color32, CornerRadius, Frame, Id,
    InnerResponse, Label, Layout, Margin, Painter, Pos2, Rect, Response, RichText, Sense, Shape,
    Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo, WidgetText, WidgetType,
};

/// What a mark, a notice or a status line means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Plain information; no judgement.
    Neutral,
    /// Something happened that is worth a line; drawn like `Neutral`.
    Info,
    /// In progress: connecting, waking, pairing.
    Accent,
    /// Online, ready, done.
    Success,
    /// Works, but something could be better or needs a decision.
    Warning,
    /// Failed or unreachable.
    Danger,
}

impl Tone {
    /// The colour of this tone's mark and, for text in that tone, its ink.
    pub fn color(self) -> Color32 {
        match self {
            Tone::Neutral | Tone::Info => P.text_tertiary,
            Tone::Accent => P.accent,
            Tone::Success => P.success,
            Tone::Warning => P.warning,
            Tone::Danger => P.danger,
        }
    }

    /// The outline of a card or notice in this tone.
    fn line(self) -> Color32 {
        match self {
            Tone::Neutral | Tone::Info => P.border,
            t => t.color().gamma_multiply(0.5),
        }
    }

    /// The faint wash behind a notice in this tone.
    fn wash(self) -> Color32 {
        match self {
            Tone::Neutral | Tone::Info => P.surface,
            t => t.color().gamma_multiply(0.07),
        }
    }
}

fn margin(x: f32, y: f32) -> Margin {
    Margin::symmetric(x as i8, y as i8)
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Small line icons, painted rather than taken from a font so they match
/// on every machine and at every scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    ChevronDown,
    ChevronRight,
    /// Three dots: more actions.
    More,
    Check,
    Close,
    /// Opens elsewhere (a browser).
    External,
}

/// Paint `icon` in a 12-point box centred on `c`.
fn paint_icon(painter: &Painter, c: Pos2, icon: Icon, color: Color32) {
    let s = stroke(1.5, color);
    let p = |x: f32, y: f32| c + Vec2::new(x, y);
    match icon {
        Icon::ChevronDown => {
            painter.add(Shape::line(
                vec![p(-4.0, -2.0), p(0.0, 2.0), p(4.0, -2.0)],
                s,
            ));
        }
        Icon::ChevronRight => {
            painter.add(Shape::line(
                vec![p(-2.0, -4.0), p(2.0, 0.0), p(-2.0, 4.0)],
                s,
            ));
        }
        Icon::More => {
            for x in [-5.0, 0.0, 5.0] {
                painter.circle_filled(p(x, 0.0), 1.5, color);
            }
        }
        Icon::Check => {
            painter.add(Shape::line(
                vec![p(-4.0, 0.0), p(-1.5, 3.0), p(4.0, -3.5)],
                s,
            ));
        }
        Icon::Close => {
            painter.line_segment([p(-4.0, -4.0), p(4.0, 4.0)], s);
            painter.line_segment([p(-4.0, 4.0), p(4.0, -4.0)], s);
        }
        Icon::External => {
            painter.line_segment([p(-3.5, 3.5), p(3.5, -3.5)], s);
            painter.add(Shape::line(
                vec![p(-1.5, -3.5), p(3.5, -3.5), p(3.5, 1.5)],
                s,
            ));
        }
    }
}

/// Keep keyboard navigation visible for controls with custom painting: an
/// accent outline two points outside the control, so it shows against the
/// background whatever the control's own fill.
fn focus_ring(ui: &Ui, response: &Response, corner: u8) {
    if response.has_focus() && ui.is_enabled() {
        ui.painter().rect_stroke(
            response.rect.expand(2.0),
            CornerRadius::same(corner.saturating_add(2)),
            stroke(1.5, P.accent),
            StrokeKind::Outside,
        );
    }
}

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

/// The frame of a card: a surface one step up from the window, with a
/// hairline around it.
fn card_frame() -> Frame {
    Frame::new()
        .fill(P.surface)
        .stroke(stroke(1.0, P.hairline))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(Margin::same(space::LG as i8))
}

/// A full-width card for free-form content.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    card_frame().show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        add(ui)
    })
}

/// A card whose outline carries a tone, for the one thing on screen that
/// needs attention now (a connection in progress, a failure, setup).
pub fn toned_card<R>(ui: &mut Ui, tone: Tone, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    card_frame()
        .stroke(stroke(1.0, tone.line()))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui)
        })
}

/// A card's title with an optional one-line explanation underneath.
pub fn heading(ui: &mut Ui, title: &str, subtitle: Option<&str>) {
    ui.add(
        Label::new(
            RichText::new(title)
                .font(theme::semibold(text::TITLE))
                .color(P.text),
        )
        .wrap(),
    );
    if let Some(s) = subtitle {
        ui.add_space(-space::XS);
        caption(ui, s);
    }
    ui.add_space(space::XS);
}

/// A grouped list: rows separated by hairlines on one surface, as in the
/// machine list and every settings section. Rows bring their own padding.
pub fn group<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    Frame::new()
        .fill(P.surface)
        .stroke(stroke(1.0, P.hairline))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(margin(space::LG, 0.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
}

/// A mono uppercase label over a section, as on the product site.
pub fn section_label(ui: &mut Ui, label: &str) -> Response {
    ui.add(
        Label::new(
            RichText::new(label.to_uppercase())
                .font(theme::mono_medium(text::LABEL))
                .extra_letter_spacing(text::LABEL_TRACKING)
                .color(P.text_tertiary),
        )
        .truncate(),
    )
}

/// A [`section_label`] over a [`group`].
pub fn section<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = space::SM;
        section_label(ui, label);
        group(ui, add).inner
    })
    .inner
}

/// A page's title, an optional line under it, and controls on the right.
pub fn page_header(ui: &mut Ui, title: &str, subtitle: Option<&str>, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::MD;
        ui.add(
            Label::new(
                RichText::new(title)
                    .font(theme::semibold(text::PAGE_TITLE))
                    .extra_letter_spacing(text::TITLE_TRACKING)
                    .color(P.text),
            )
            .truncate(),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    if let Some(s) = subtitle {
        ui.add_space(-space::XS);
        muted(ui, s);
    }
}

/// A sunken area for a log or a block of code.
pub fn well<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    Frame::new()
        .fill(P.well)
        .stroke(stroke(1.0, P.hairline))
        .corner_radius(CornerRadius::same(radius::SM))
        .inner_margin(margin(space::MD, space::SM))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui)
        })
}

/// A scrolling log in a [`well`], newest line at the bottom.
pub fn log_view(ui: &mut Ui, id: impl std::hash::Hash, lines: &[String], max_height: f32) {
    well(ui, |ui| {
        ui.spacing_mut().item_spacing.y = space::XXS;
        egui::ScrollArea::vertical()
            .id_salt(id)
            .max_height(max_height)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                if lines.is_empty() {
                    ui.label(
                        RichText::new("Nothing logged yet.")
                            .font(theme::mono(text::MONO))
                            .color(P.text_tertiary),
                    );
                }
                for line in lines {
                    ui.add(
                        Label::new(
                            RichText::new(line)
                                .font(theme::mono(text::MONO))
                                .color(P.text_secondary),
                        )
                        .wrap(),
                    );
                }
            });
    });
}

/// A row that opens and closes the content under it, with a painted
/// chevron. `id` is global to the context; use distinct ids.
pub fn disclosure<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash,
    title: &str,
    default_open: bool,
    body: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let id = Id::new("latch.disclosure").with(id);
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        theme::medium(text::BODY),
        Color32::PLACEHOLDER,
    );
    let h = size::CONTROL;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(galley.size().x + 24.0), h),
        Sense::click(),
    );
    keep_in_view(&response);
    if response.clicked() {
        state.toggle(ui);
    }
    let open = state.is_open();
    response.widget_info(|| {
        WidgetInfo::selected(WidgetType::CollapsingHeader, ui.is_enabled(), open, title)
    });
    if ui.is_rect_visible(rect) {
        let hot = response.hovered() || response.has_focus();
        let color = if hot { P.text } else { P.text_secondary };
        paint_icon(
            ui.painter(),
            Pos2::new(rect.left() + 6.0, rect.center().y),
            if open {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            color,
        );
        ui.painter().galley(
            Pos2::new(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        focus_ring(ui, &response, radius::SM);
    }
    let out = state.show_body_unindented(ui, |ui| {
        ui.add_space(space::XS);
        body(ui)
    });
    out.map(|r| r.inner)
}

/// A [`card`] whose content folds away under a [`disclosure`] row.
pub fn collapsible<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash,
    title: &str,
    default_open: bool,
    body: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    Frame::new()
        .fill(P.surface)
        .stroke(stroke(1.0, P.hairline))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(margin(space::MD, space::XS))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            let r = disclosure(ui, id, title, default_open, body);
            if r.is_some() {
                ui.add_space(space::SM);
            }
            r
        })
        .inner
}

/// Centre the content in a column no wider than `max_width`, with at
/// least the page gutter on each side.
pub fn content_column<R>(ui: &mut Ui, max_width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let avail = ui.available_width();
    let w = (avail - 2.0 * space::XL).max(0.0).min(max_width);
    let pad = ((avail - w) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(w);
            add(ui)
        })
        .inner
    })
    .inner
}

/// The bar across the top of the window: brand, tabs, status.
pub fn top_bar<R>(ctx: &egui::Context, id: impl Into<Id>, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::TopBottomPanel::top(id)
        .frame(Frame::new().fill(P.bg).inner_margin(margin(space::LG, 0.0)))
        .exact_height(size::TOP_BAR)
        .show_separator_line(true)
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = space::SM;
                add(ui)
            })
            .inner
        })
        .inner
}

/// The bar across the bottom of the window: version, account, small print.
pub fn bottom_bar<R>(ctx: &egui::Context, id: impl Into<Id>, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::TopBottomPanel::bottom(id)
        .frame(Frame::new().fill(P.bg).inner_margin(margin(space::LG, 6.0)))
        .show_separator_line(true)
        .show(ctx, |ui| {
            ui.style_mut().override_font_id = Some(theme::mono(text::LABEL + 0.5));
            ui.visuals_mut().override_text_color = Some(P.text_tertiary);
            add(ui)
        })
        .inner
}

/// The frame of anything that floats over the stream picture: toasts,
/// questions, the toolbar's menus.
pub fn overlay_frame() -> Frame {
    Frame::new()
        .fill(P.overlay)
        .stroke(stroke(1.0, Color32::from_white_alpha(22)))
        .corner_radius(CornerRadius::same(radius::LG))
        .inner_margin(margin(space::MD, space::MD))
        .shadow(theme::elevation::OVERLAY)
}

/// The frame of a panel over the stream (stream settings, performance).
pub fn panel_frame() -> Frame {
    Frame::new()
        .fill(P.surface)
        .stroke(stroke(1.0, P.border))
        .corner_radius(CornerRadius::same(radius::LG))
        .inner_margin(Margin::same(space::LG as i8))
        .shadow(theme::elevation::OVERLAY)
}

/// A floating panel's title row with a close button. Returns true when
/// the close button was pressed.
pub fn panel_header(ui: &mut Ui, title: &str) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .font(theme::semibold(text::TITLE))
                .color(P.text),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            close = icon_button(ui, Icon::Close, &format!("Close {title}")).clicked();
        });
    });
    ui.add_space(space::XS);
    close
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// Secondary text under a label: hints, help, metadata. Wraps.
pub fn caption(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(
        Label::new(
            RichText::new(text)
                .text_style(theme::caption())
                .color(P.text_secondary),
        )
        .wrap(),
    )
}

/// Small print: the quietest text that still reads at AA.
pub fn small_print(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(
        Label::new(
            RichText::new(text)
                .text_style(theme::caption())
                .color(P.text_tertiary),
        )
        .wrap(),
    )
}

/// Body text in the secondary colour. Wraps.
pub fn muted(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(Label::new(RichText::new(text).color(P.text_secondary)).wrap())
}

/// Medium-weight body text, for a name in a row.
pub fn strong(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme::medium(text::BODY))
            .color(P.text),
    )
}

/// Mono data: an address, a version, a rate.
pub fn mono(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme::mono(text::MONO))
            .color(P.text_secondary),
    )
}

/// A code such as the pairing PIN, one digit to a box.
pub fn display_digits(ui: &mut Ui, digits: &str) -> Response {
    let font = theme::mono_medium(text::DISPLAY);
    let cell = Vec2::new(44.0, 56.0);
    let n = digits.chars().count().max(1) as f32;
    let total = Vec2::new(n * cell.x + (n - 1.0) * space::SM, cell.y);
    let (rect, response) = ui.allocate_exact_size(total, Sense::hover());
    response.widget_info(|| {
        let spoken: Vec<String> = digits.chars().map(String::from).collect();
        WidgetInfo::labeled(WidgetType::Label, true, format!("PIN {}", spoken.join(" ")))
    });
    if ui.is_rect_visible(rect) {
        for (i, ch) in digits.chars().enumerate() {
            let min = rect.min + Vec2::new(i as f32 * (cell.x + space::SM), 0.0);
            let r = Rect::from_min_size(min, cell);
            ui.painter().rect(
                r,
                CornerRadius::same(radius::SM),
                P.raised,
                stroke(1.0, P.border_strong),
                StrokeKind::Inside,
            );
            ui.painter()
                .text(r.center(), Align2::CENTER_CENTER, ch, font.clone(), P.text);
        }
    }
    response
}

/// Text for a list with nothing in it yet, with a spinner while something
/// is being looked for. Wraps: a line that ran past the card would widen
/// everything laid out after it.
pub fn empty_state(ui: &mut Ui, text: &str, busy: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::SM;
        if busy {
            ui.add(spinner(14.0, P.text_tertiary));
        }
        ui.label(RichText::new(text).color(P.text_secondary));
    });
}

/// A dot in a fixed gutter and wrapped text beside it: every line of the
/// text starts at the same x, however many there are.
fn dotted<R>(ui: &mut Ui, tone: Tone, add: impl FnOnce(&mut Ui) -> R) -> R {
    let gutter = 16.0;
    let avail = ui.available_rect_before_wrap();
    let text_rect = Rect::from_min_max(
        Pos2::new(avail.left() + gutter, avail.top()),
        Pos2::new(avail.right(), avail.bottom()),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    let r = add(&mut child);
    let used = child.min_rect();
    // The dot sits on the first line's centre.
    let first_line = ui.text_style_height(&egui::TextStyle::Body);
    let dot_c = Pos2::new(
        avail.left() + size::DOT / 2.0,
        avail.top() + first_line / 2.0,
    );
    ui.painter()
        .circle_filled(dot_c, size::DOT / 2.0 - 0.5, tone.color());
    ui.allocate_rect(
        Rect::from_min_max(avail.min, Pos2::new(avail.right(), used.bottom())),
        Sense::hover(),
    );
    r
}

fn notice_frame(tone: Tone) -> Frame {
    Frame::new()
        .fill(tone.wash())
        .stroke(stroke(1.0, tone.line()))
        .corner_radius(CornerRadius::same(radius::MD))
        .inner_margin(margin(space::MD, space::MD - 2.0))
}

/// An inline message: errors, warnings, confirmations. The wash and outline
/// carry the tone; the text stays in the primary colour.
pub fn notice(ui: &mut Ui, tone: Tone, text: &str) -> Response {
    notice_frame(tone)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            dotted(ui, tone, |ui| {
                ui.add(Label::new(RichText::new(text).color(P.text)).wrap());
            });
        })
        .response
}

/// A [`notice`] with a title, a body and a row of actions under them.
pub fn banner(
    ui: &mut Ui,
    tone: Tone,
    title: &str,
    body: Option<&str>,
    actions: impl FnOnce(&mut Ui),
) -> Response {
    notice_frame(tone)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            dotted(ui, tone, |ui| {
                ui.spacing_mut().item_spacing.y = space::XS;
                ui.add(
                    Label::new(
                        RichText::new(title)
                            .font(theme::medium(text::BODY))
                            .color(P.text),
                    )
                    .wrap(),
                );
                if let Some(b) = body {
                    muted(ui, b);
                }
                action_row(ui, space::XS, actions);
            });
        })
        .response
}

/// A row of actions under a block of text, lined up with the text: a
/// filled or outlined button's edge starts the line, and a quiet button
/// (text until hovered) is pulled back by its padding so its label does.
/// Takes no room at all when `add` adds nothing; otherwise `gap` goes
/// above it.
pub fn action_row(ui: &mut Ui, gap: f32, add: impl FnOnce(&mut Ui)) {
    let top = ui.available_rect_before_wrap();
    // One control high, as `horizontal_wrapped` starts: its lines centre
    // in that height and further lines go below.
    let line = Rect::from_min_size(
        top.min + Vec2::new(0.0, gap),
        Vec2::new(top.width(), ui.spacing().interact_size.y),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(line)
            .layout(Layout::left_to_right(Align::Center).with_main_wrap(true)),
    );
    child.spacing_mut().item_spacing.x = space::SM;
    let key = flush_key(child.id());
    child.data_mut(|d| d.insert_temp(key, true));
    add(&mut child);
    child.data_mut(|d| d.remove::<bool>(key));
    let used = child.min_rect();
    if used.width() > 0.0 && used.height() > 0.0 {
        ui.allocate_rect(
            Rect::from_min_max(top.min, Pos2::new(top.right(), used.bottom())),
            Sense::hover(),
        );
    }
}

fn flush_key(row: Id) -> Id {
    Id::new("latch.action_row").with(row)
}

/// Whether the next widget starts a line of an [`action_row`].
fn starts_action_line(ui: &Ui) -> bool {
    ui.data(|d| d.get_temp::<bool>(flush_key(ui.id())))
        .unwrap_or(false)
        && ui.cursor().min.x <= ui.max_rect().min.x + 0.5
}

// ---------------------------------------------------------------------------
// Key / value
// ---------------------------------------------------------------------------

/// One line of a [`kv_grid`].
pub struct Kv<'a> {
    pub key: &'a str,
    pub value: String,
    /// A status dot before the value.
    pub tone: Option<Tone>,
    /// Set the value in mono, for figures that should line up.
    pub mono: bool,
}

impl<'a> Kv<'a> {
    pub fn new(key: &'a str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: value.into(),
            tone: None,
            mono: false,
        }
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = Some(tone);
        self
    }
}

/// Aligned key/value rows. A value wraps inside the card: an `egui::Grid`
/// extends instead, and one row wider than the card widens every widget
/// laid out after it. Below 300 points the key goes above its value.
pub fn kv_grid(ui: &mut Ui, rows: &[Kv<'_>]) {
    let font = theme::regular(text::CAPTION);
    let key_w = rows
        .iter()
        .map(|kv| {
            ui.painter()
                .layout_no_wrap(kv.key.to_owned(), font.clone(), Color32::PLACEHOLDER)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max)
        .min(ui.available_width() * 0.4);
    let stacked = ui.available_width() < 300.0;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(space::LG, space::SM);
        for kv in rows {
            let rich = || {
                let t = RichText::new(&kv.value).color(P.text);
                if kv.mono {
                    t.font(theme::mono(text::MONO))
                } else {
                    t
                }
            };
            let value = |ui: &mut Ui| {
                if let Some(t) = kv.tone {
                    dotted(ui, t, |ui| {
                        ui.add(Label::new(rich()).wrap());
                    });
                } else {
                    ui.add(Label::new(rich()).wrap());
                }
            };
            if stacked {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = space::XXS;
                    ui.label(
                        RichText::new(kv.key)
                            .font(font.clone())
                            .color(P.text_tertiary),
                    );
                    value(ui);
                });
            } else {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(key_w, 18.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(key_w);
                            ui.add_space(1.5);
                            ui.add(
                                Label::new(
                                    RichText::new(kv.key)
                                        .font(font.clone())
                                        .color(P.text_tertiary),
                                )
                                .truncate(),
                            );
                        },
                    );
                    ui.vertical(value);
                });
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// The five kinds of button. One `Primary` per screen at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The screen's main action: white, like the site's primary button.
    Primary,
    /// A real action that is not the main one: outlined.
    Secondary,
    /// Everything else: text until hovered.
    Quiet,
    /// A quiet button that ends or removes something: red text.
    Danger,
    /// The confirming step of a destructive action: red fill.
    Destructive,
}

struct Look {
    fill: Color32,
    line: Color32,
    ink: Color32,
}

fn look(kind: Kind, enabled: bool, hovered: bool, pressed: bool) -> Look {
    let none = Color32::TRANSPARENT;
    if !enabled {
        return match kind {
            Kind::Primary | Kind::Destructive => Look {
                fill: P.raised,
                line: P.border,
                ink: P.text_disabled,
            },
            Kind::Secondary => Look {
                fill: none,
                line: P.hairline,
                ink: P.text_disabled,
            },
            Kind::Quiet | Kind::Danger => Look {
                fill: none,
                line: none,
                ink: P.text_disabled,
            },
        };
    }
    match kind {
        Kind::Primary => Look {
            fill: if pressed {
                P.primary_pressed
            } else if hovered {
                P.primary_hover
            } else {
                P.primary
            },
            line: none,
            ink: P.on_primary,
        },
        Kind::Destructive => Look {
            fill: if pressed || hovered {
                lerp_color(P.danger_fill, Color32::BLACK, 0.12)
            } else {
                P.danger_fill
            },
            line: none,
            ink: Color32::WHITE,
        },
        Kind::Secondary => Look {
            fill: if pressed {
                P.pressed
            } else if hovered {
                P.raised_hover
            } else {
                P.raised
            },
            line: if hovered { P.border_strong } else { P.border },
            ink: P.text,
        },
        Kind::Quiet => Look {
            fill: if pressed {
                P.pressed
            } else if hovered {
                P.raised_hover
            } else {
                none
            },
            line: none,
            ink: if hovered || pressed {
                P.text
            } else {
                P.text_secondary
            },
        },
        Kind::Danger => Look {
            fill: if pressed || hovered {
                P.danger.gamma_multiply(0.14)
            } else {
                none
            },
            line: none,
            ink: P.danger,
        },
    }
}

/// A text button of any [`Kind`], at the height of the surrounding
/// `interact_size` (32 points, 28 in the stream toolbar).
pub fn button(ui: &mut Ui, kind: Kind, label: &str) -> Response {
    labelled_button(ui, kind, label, None, label)
}

fn labelled_button(
    ui: &mut Ui,
    kind: Kind,
    label: &str,
    trailing: Option<Icon>,
    a11y: &str,
) -> Response {
    let font = theme::medium(if ui.spacing().interact_size.y < size::CONTROL {
        text::CAPTION + 0.5
    } else {
        text::BODY
    });
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let h = ui.spacing().interact_size.y;
    let pad = space::MD;
    let icon_w = if trailing.is_some() { 16.0 } else { 0.0 };
    let w = (galley.size().x + 2.0 * pad + icon_w).max(h);
    if matches!(kind, Kind::Quiet | Kind::Danger) && starts_action_line(ui) {
        // No fill or outline marks a quiet button's box, so its label is
        // what the eye lines up: pull it back by the padding.
        ui.add_space(-pad);
    }
    let (rect, response) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    keep_in_view(&response);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), a11y));
    if ui.is_rect_visible(rect) {
        let l = look(
            kind,
            ui.is_enabled(),
            response.hovered() || response.has_focus(),
            response.is_pointer_button_down_on(),
        );
        ui.painter().rect(
            rect,
            CornerRadius::same(radius::SM),
            l.fill,
            stroke(1.0, l.line),
            StrokeKind::Inside,
        );
        let text_x = rect.left() + pad + (w - 2.0 * pad - icon_w - galley.size().x) / 2.0;
        let pos = Pos2::new(text_x, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(pos, galley.clone(), l.ink);
        if let Some(icon) = trailing {
            paint_icon(
                ui.painter(),
                Pos2::new(pos.x + galley.size().x + 10.0, rect.center().y),
                icon,
                l.ink,
            );
        }
        focus_ring(ui, &response, radius::SM);
    }
    response
}

/// The one white button on a screen.
pub fn primary_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, Kind::Primary, label)
}

/// An outlined button for a real but secondary action.
pub fn secondary_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, Kind::Secondary, label)
}

/// A quiet button: text only until hovered.
pub fn ghost_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, Kind::Quiet, label)
}

/// A quiet button that ends or removes something.
pub fn danger_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, Kind::Danger, label)
}

/// The red, filled confirming step of a destructive action.
pub fn destructive_button(ui: &mut Ui, label: &str) -> Response {
    button(ui, Kind::Destructive, label)
}

/// A square quiet button showing only `icon`; `label` is what a screen
/// reader says and the tooltip shows.
pub fn icon_button(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    let s = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(s), Sense::click());
    keep_in_view(&response);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), label));
    if ui.is_rect_visible(rect) {
        let l = look(
            Kind::Quiet,
            ui.is_enabled(),
            response.hovered() || response.has_focus(),
            response.is_pointer_button_down_on(),
        );
        ui.painter().rect(
            rect,
            CornerRadius::same(radius::SM),
            l.fill,
            Stroke::NONE,
            StrokeKind::Inside,
        );
        paint_icon(ui.painter(), rect.center(), icon, l.ink);
        focus_ring(ui, &response, radius::SM);
    }
    response.on_hover_text(label)
}

/// A link: primary text with a hairline underline that brightens on hover.
pub fn link(ui: &mut Ui, label: &str) -> Response {
    let font = theme::regular(text::CAPTION);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let size = Vec2::new(galley.size().x, galley.size().y.max(size::HIT_MIN));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    keep_in_view(&response);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Link, ui.is_enabled(), label));
    if ui.is_rect_visible(rect) {
        let hot = response.hovered() || response.has_focus();
        let top = rect.center().y - galley.size().y / 2.0;
        let base = top + galley.size().y + 1.0;
        let w = galley.size().x;
        ui.painter()
            .galley(Pos2::new(rect.left(), top), galley, P.text);
        ui.painter().hline(
            rect.left()..=rect.left() + w,
            base,
            stroke(1.0, if hot { P.text } else { P.border_strong }),
        );
        focus_ring(ui, &response, radius::SM);
    }
    response
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

/// Where a popup hung from a control goes, and how tall it may be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drop {
    /// The corner of the popup that touches the control's edge.
    pub pos: Pos2,
    pub pivot: Align2,
    /// The popup's outer height may not exceed this; anything longer
    /// scrolls inside it.
    pub max_height: f32,
    /// True when it opens upwards, over the control.
    pub above: bool,
}

/// Place a popup of outer height `wanted` against `anchor` inside
/// `screen`. It opens below when it fits there, and otherwise on whichever
/// side has more room (a machine's menu near the bottom of a short window
/// opens upwards). Its height is capped at that side's room, less a margin
/// from the window's edge, so no part of it is ever off screen: what does
/// not fit scrolls. `right` hangs it from the control's right edge, for
/// controls on the right of the window.
pub fn place_popup(screen: Rect, anchor: Rect, wanted: f32, right: bool) -> Drop {
    let gap = space::XS;
    let edge = space::SM;
    let below = (screen.bottom() - edge - (anchor.bottom() + gap)).max(0.0);
    let above = (anchor.top() - gap - (screen.top() + edge)).max(0.0);
    let x = if right { anchor.right() } else { anchor.left() };
    if wanted <= below || below >= above {
        Drop {
            pos: Pos2::new(x, anchor.bottom() + gap),
            pivot: if right {
                Align2::RIGHT_TOP
            } else {
                Align2::LEFT_TOP
            },
            max_height: below,
            above: false,
        }
    } else {
        Drop {
            pos: Pos2::new(x, anchor.top() - gap),
            pivot: if right {
                Align2::RIGHT_BOTTOM
            } else {
                Align2::LEFT_BOTTOM
            },
            max_height: above,
            above: true,
        }
    }
}

/// The height a scrolling region inside a panel may take so the panel
/// ends a margin above the window's bottom edge: from the cursor down,
/// less `below` for what the panel still has to lay out under it (its
/// footer and bottom margin). Never less than one control, so a tiny
/// window still shows something to scroll.
pub fn room_below(ui: &Ui, below: f32) -> f32 {
    let bottom = ui.ctx().screen_rect().bottom() - space::SM;
    (bottom - ui.cursor().top() - below).max(size::CONTROL)
}

/// A vertical scroll area for content that may be cut off by the window:
/// its scroll bar shows at rest, so a clipped list reads as one. Keyboard
/// focus moving to a control inside scrolls it into view (see
/// [`keep_in_view`]).
pub fn clipped_scroll<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash,
    max_height: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    let saved = ui.spacing().scroll;
    ui.spacing_mut().scroll = theme::clipped_scroll();
    let out = egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(max_height)
        // A popup or window lays out in the size it had last frame; without
        // this the list could never grow past its first, smaller, height.
        .min_scrolled_height(max_height)
        .show(ui, |ui| {
            ui.spacing_mut().scroll = saved;
            add(ui)
        });
    ui.spacing_mut().scroll = saved;
    out
}

/// When keyboard focus arrives at a control, scroll it into view: egui
/// moves focus to a control clipped away in a scroll area without showing
/// it, and a menu or panel cut short by a small window would otherwise take
/// focus somewhere nobody can see.
pub fn keep_in_view(response: &Response) {
    if response.gained_focus() {
        response.scroll_to_me_animation(None, egui::style::ScrollAnimation::none());
    }
}

/// Where the menu open now sits on screen, or the visible part of an open
/// [`select`]'s list, for layout checks.
pub fn open_menu_rect(ctx: &egui::Context) -> Option<Rect> {
    let (id, rect, _): (Id, Rect, u64) = ctx.data(|d| d.get_temp(open_menu_key()))?;
    ctx.memory(|m| m.is_popup_open(id)).then_some(rect)
}

fn open_menu_key() -> Id {
    Id::new("latch.open_menu")
}

/// Note that the menu or list `id` was drawn at `rect` in this pass.
fn drawn_menu(ctx: &egui::Context, id: Id, rect: Rect) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(open_menu_key(), (id, rect, pass)));
}

/// Run at the end of every pass (see [`crate::apply`]): close a menu or
/// list that egui still holds open but nothing drew this pass. Its button
/// went away (the toolbar folded into More, the stream ended, the page
/// changed), so nothing on screen could close it, and an open popup keeps
/// the stream's toolbar up and its keys and mouse from the PC. Popups that
/// are not Latch's own are left alone.
pub(crate) fn close_orphan_menu(ctx: &egui::Context) {
    let Some((id, _, pass)) = ctx.data(|d| d.get_temp::<(Id, Rect, u64)>(open_menu_key())) else {
        return;
    };
    if pass != ctx.cumulative_pass_nr() && ctx.memory(|m| m.is_popup_open(id)) {
        ctx.memory_mut(|m| m.close_popup());
    }
}

/// Open a popup menu under `trigger` while it is toggled on. A real egui
/// popup: `any_popup_open` is true while it shows, Escape and a click
/// outside close it. It hangs from the trigger's nearer edge, so a menu at
/// the right of the window opens leftwards instead of running off it, and
/// it never runs past the window's top or bottom: it opens upwards when
/// there is more room above, and scrolls when neither side has room for
/// all of it (see [`place_popup`]).
///
/// The menu is known by its `label`, not by the button's place: egui names
/// a widget by its position among its siblings, so a toolbar that gained a
/// tag, or a machine list that gained a row above, handed an open menu to
/// another button (another machine's Sleep).
fn popup_menu<R>(
    ui: &mut Ui,
    trigger: &Response,
    label: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    popup_menu_id(ui, trigger, label, add)
}

fn popup_menu_id<R>(
    ui: &mut Ui,
    trigger: &Response,
    id_source: impl std::hash::Hash,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let id = Id::new("latch.menu").with(id_source);
    if trigger.clicked() {
        ui.memory_mut(|m| m.toggle_popup(id));
    }
    if !ui.memory(|m| m.is_popup_open(id)) {
        return None;
    }
    let ctx = ui.ctx().clone();
    let screen = ctx.screen_rect();
    let right = trigger.rect.center().x > screen.center().x;
    // The menu's full height, measured when it was last laid out.
    let key = id.with("height");
    let known: Option<f32> = ctx.data(|d| d.get_temp(key));
    let drop = place_popup(screen, trigger.rect, known.unwrap_or(0.0), right);
    let style = ui.style().clone();
    let chrome = style.spacing.menu_margin.sum().y + 2.0 * style.visuals.window_stroke.width;
    let shown = egui::Area::new(id)
        .kind(egui::UiKind::Popup)
        .order(egui::Order::Foreground)
        .fixed_pos(drop.pos)
        .pivot(drop.pivot)
        .constrain(true)
        .show(&ctx, |ui| {
            Frame::popup(&style)
                .show(ui, |ui| {
                    ui.set_min_width(200.0);
                    ui.set_max_width(320.0);
                    let max = (drop.max_height - chrome).max(size::CONTROL_SM);
                    clipped_scroll(ui, "latch.menu", max, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.spacing_mut().interact_size.y = size::CONTROL_SM;
                        ui.with_layout(Layout::top_down_justified(Align::Min), add)
                            .inner
                    })
                })
                .inner
        });
    let full = shown.inner.content_size.y + chrome;
    if known.is_none_or(|k| (k - full).abs() > 0.5) {
        ctx.data_mut(|d| d.insert_temp(key, full));
        if place_popup(screen, trigger.rect, full, right) != drop {
            ctx.request_discard("menu moved to fit the window");
        }
    }
    drawn_menu(&ctx, id, shown.response.rect);
    let outside = trigger.clicked_elsewhere() && shown.response.clicked_elsewhere();
    // Escape that closes the menu is used up here: otherwise the stream
    // forwards it to the PC once the menu has gone, and a panel behind the
    // menu closes on it too.
    let escape = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if outside || escape {
        ui.memory_mut(|m| m.close_popup());
    }
    Some(shown.inner.inner)
}

/// A quiet button with a chevron that opens a menu.
pub fn menu_button<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let trigger = labelled_button(ui, Kind::Quiet, label, Some(Icon::ChevronDown), label);
    popup_menu(ui, &trigger, label, add)
}

/// An icon button that opens a menu; `label` is its accessible name.
pub fn icon_menu<R>(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    icon_menu_id(ui, icon, label, label, add)
}

/// An icon button that opens a menu, with a distinct ID source.
pub fn icon_menu_id<R>(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    id_source: impl std::hash::Hash,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let trigger = icon_button(ui, icon, label);
    popup_menu_id(ui, &trigger, id_source, add)
}

fn menu_row(ui: &mut Ui, label: &str, checked: Option<bool>, enabled: bool) -> Response {
    let font = theme::regular(text::BODY - 0.5);
    // The check column is always there, so every item's text lines up.
    let gutter = 26.0;
    let wrap = ui.available_width() - gutter - space::SM;
    let galley = ui
        .painter()
        .layout(label.to_owned(), font, Color32::PLACEHOLDER, wrap);
    let h = (galley.size().y + 10.0).max(size::CONTROL_SM);
    let w = ui.available_width();
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(w, h), sense);
    keep_in_view(&response);
    response.widget_info(|| match checked {
        Some(on) => WidgetInfo::selected(WidgetType::RadioButton, enabled, on, label),
        None => WidgetInfo::labeled(WidgetType::Button, enabled, label),
    });
    if ui.is_rect_visible(rect) {
        let hot = enabled && (response.hovered() || response.has_focus());
        if hot {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(radius::SM), P.raised_hover);
        }
        let ink = if !enabled {
            P.text_disabled
        } else if hot {
            P.text
        } else {
            P.text_secondary
        };
        if checked == Some(true) {
            paint_icon(
                ui.painter(),
                Pos2::new(rect.left() + 12.0, rect.top() + h.min(28.0) / 2.0),
                Icon::Check,
                P.text,
            );
        }
        ui.painter().galley(
            Pos2::new(
                rect.left() + gutter,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            ink,
        );
    }
    if response.clicked() {
        ui.memory_mut(|m| m.close_popup());
    }
    response
}

/// An item in a menu. Closes the menu when clicked.
pub fn menu_item(ui: &mut Ui, label: &str) -> Response {
    menu_row(ui, label, None, true)
}

/// A menu item that can be greyed out.
pub fn menu_item_enabled(ui: &mut Ui, enabled: bool, label: &str) -> Response {
    menu_row(ui, label, None, enabled)
}

/// A menu item with a check mark when chosen.
pub fn menu_choice(ui: &mut Ui, label: &str, chosen: bool) -> Response {
    menu_row(ui, label, Some(chosen), true)
}

/// A heading inside a menu.
pub fn menu_label(ui: &mut Ui, label: &str) {
    ui.add_space(space::XS + 2.0);
    ui.horizontal(|ui| {
        ui.add_space(26.0);
        section_label(ui, label);
    });
    ui.add_space(space::XS);
}

/// A line of help at the foot of a menu.
pub fn menu_note(ui: &mut Ui, note: &str) {
    ui.add_space(space::XS);
    Frame::new()
        .inner_margin(Margin {
            left: 26,
            right: space::SM as i8,
            top: space::XS as i8,
            bottom: space::XS as i8,
        })
        .show(ui, |ui| {
            small_print(ui, note);
        });
}

/// A hairline between groups of menu items.
pub fn menu_separator(ui: &mut Ui) {
    ui.add_space(space::XS);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, stroke(1.0, P.hairline));
    ui.add_space(space::XS);
}

// ---------------------------------------------------------------------------
// Choices
// ---------------------------------------------------------------------------

/// Tabs in the top bar: text, with a rule under the chosen one sitting on
/// the bar's bottom hairline. Each option carries a tooltip (its keyboard
/// shortcut, say); empty for none. Returns true when the choice changed.
pub fn tabs<T: PartialEq + Copy>(
    ui: &mut Ui,
    options: &[(T, &str, String)],
    selected: &mut T,
) -> bool {
    let mut changed = false;
    let h = ui.available_height().max(size::CONTROL);
    ui.spacing_mut().item_spacing.x = 0.0;
    for (value, label, tip) in options {
        let on = *selected == *value;
        let galley = ui.painter().layout_no_wrap(
            (*label).to_owned(),
            theme::medium(text::BODY - 0.5),
            Color32::PLACEHOLDER,
        );
        let w = galley.size().x + 2.0 * space::MD;
        let (rect, mut response) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
        response.widget_info(|| {
            WidgetInfo::selected(WidgetType::SelectableLabel, ui.is_enabled(), on, *label)
        });
        if !tip.is_empty() {
            response = response.on_hover_text(tip);
        }
        if response.clicked() && !on {
            *selected = *value;
            changed = true;
        }
        if ui.is_rect_visible(rect) {
            let hot = response.hovered() || response.has_focus();
            let ink = if on || hot { P.text } else { P.text_secondary };
            ui.painter()
                .galley(rect.center() - galley.size() / 2.0, galley, ink);
            if on {
                let y = rect.bottom() - 1.0;
                ui.painter().line_segment(
                    [
                        Pos2::new(rect.left() + space::MD, y),
                        Pos2::new(rect.right() - space::MD, y),
                    ],
                    stroke(2.0, P.text),
                );
            }
            if response.has_focus() {
                ui.painter().rect_stroke(
                    rect.shrink2(Vec2::new(2.0, 10.0)),
                    CornerRadius::same(radius::SM),
                    stroke(1.5, P.accent),
                    StrokeKind::Outside,
                );
            }
        }
    }
    changed
}

/// A row of mutually exclusive choices. The chosen one is filled white,
/// like "on" everywhere else. Returns true when the choice changed.
///
/// Sized and allocated as one block, so it sits correctly in any parent
/// layout (including the right-to-left side of a [`setting_row`]).
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    options: &[(T, &str)],
    selected: &mut T,
) -> bool {
    let font = theme::medium(text::CAPTION + 0.5);
    let pad_x = space::MD;
    let h = ui.spacing().interact_size.y;
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), Color32::PLACEHOLDER)
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 2.0 * pad_x).collect();
    let total = Vec2::new(widths.iter().sum(), h);
    let (rect, block) = ui.allocate_exact_size(total, Sense::hover());
    let visible = ui.is_rect_visible(rect);
    if visible {
        ui.painter().rect(
            rect,
            CornerRadius::same(radius::SM),
            P.raised,
            stroke(1.0, P.border),
            StrokeKind::Inside,
        );
    }
    let n = options.len();
    let mut changed = false;
    let mut x = rect.left();
    for (i, ((value, label), galley)) in options.iter().zip(galleys).enumerate() {
        let seg = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(widths[i], h));
        x += widths[i];
        let resp = ui.interact(seg, block.id.with(i), Sense::click());
        keep_in_view(&resp);
        resp.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::RadioButton,
                ui.is_enabled(),
                *selected == *value,
                *label,
            )
        });
        if resp.clicked() && *selected != *value {
            *selected = *value;
            changed = true;
        }
        if !visible {
            continue;
        }
        let on = *selected == *value;
        let r = radius::SM;
        let corner = CornerRadius {
            nw: if i == 0 { r } else { 0 },
            sw: if i == 0 { r } else { 0 },
            ne: if i + 1 == n { r } else { 0 },
            se: if i + 1 == n { r } else { 0 },
        };
        let hot = resp.hovered() || resp.has_focus();
        let (fill, fg) = if on {
            (P.primary, P.on_primary)
        } else if hot {
            (P.raised_hover, P.text)
        } else {
            (Color32::TRANSPARENT, P.text_secondary)
        };
        ui.painter()
            .rect(seg, corner, fill, Stroke::NONE, StrokeKind::Inside);
        if i > 0 && !on && *selected != options[i - 1].0 {
            ui.painter()
                .vline(seg.left(), seg.y_range().shrink(7.0), stroke(1.0, P.border));
        }
        ui.painter()
            .galley(seg.center() - galley.size() / 2.0, galley, fg);
        focus_ring(ui, &resp, radius::SM);
    }
    changed
}

fn labelled_toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let track = size::SWITCH;
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(track.x, track.y.max(size::HIT_MIN)),
        Sense::click(),
    );
    keep_in_view(&response);
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, ui.is_enabled(), *on, label));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let track_rect = Rect::from_center_size(rect.center(), track);
        let hot = response.hovered() || response.has_focus();
        let off = if hot { P.raised_hover } else { P.raised };
        let mut fill = lerp_color(off, P.primary, t);
        let mut knob = lerp_color(P.text_tertiary, P.on_primary, t);
        let mut line = lerp_color(P.border_strong, P.primary, t);
        if !ui.is_enabled() {
            fill = fill.gamma_multiply(0.5);
            knob = knob.gamma_multiply(0.5);
            line = line.gamma_multiply(0.5);
        }
        let r = track.y / 2.0;
        ui.painter().rect(
            track_rect,
            CornerRadius::same(r as u8),
            fill,
            stroke(1.0, line),
            StrokeKind::Inside,
        );
        let x = egui::lerp((track_rect.left() + r)..=(track_rect.right() - r), t);
        ui.painter()
            .circle_filled(Pos2::new(x, track_rect.center().y), r - 4.0, knob);
        if response.has_focus() && ui.is_enabled() {
            ui.painter().rect_stroke(
                track_rect.expand(2.0),
                CornerRadius::same(r as u8 + 2),
                stroke(1.5, P.accent),
                StrokeKind::Outside,
            );
        }
    }
    response
}

/// A dropdown in the house style, with a painted chevron. `add` fills the
/// list with [`select_option`] rows. The list is no taller than the room on
/// the side of the control it opens towards, so it stays inside the window
/// and scrolls, with its scroll bar showing, when it has to.
pub fn select<R>(
    ui: &mut Ui,
    id: impl std::hash::Hash,
    selected: impl Into<WidgetText>,
    width: f32,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    // egui opens the list below the control when it fits there, and above
    // it otherwise; cap it at the larger of the two rooms so either way it
    // ends a margin inside the window.
    let screen = ui.ctx().screen_rect();
    let y = ui.next_widget_position().y;
    let h = ui.spacing().interact_size.y;
    let below = screen.bottom() - space::SM - (y + h);
    let above = y - h / 2.0 - space::XS - (screen.top() + space::SM);
    let chrome = ui.spacing().menu_margin.sum().y + 2.0 * ui.visuals().window_stroke.width;
    let height = (below.max(above) - chrome).clamp(size::CONTROL, 320.0);
    let selected: WidgetText = selected.into();
    let shown = selected.text().to_owned();
    let mut list = None;
    let out = egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(width)
        // egui's own scroll area never clips; the one inside does.
        .height(f32::INFINITY)
        .icon(|ui, rect, visuals, _open, _| {
            paint_icon(
                ui.painter(),
                rect.center(),
                Icon::ChevronDown,
                visuals.fg_stroke.color,
            );
        })
        .show_ui(ui, |ui| {
            // The chosen row is marked by its wash and full-strength text:
            // accent text on the accent wash read at 3.7:1, under AA.
            ui.visuals_mut().selection.stroke = stroke(1.0, P.text);
            let out = clipped_scroll(ui, "latch.select", height, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.spacing_mut().button_padding = Vec2::new(space::SM, 6.0);
                add(ui)
            });
            list = Some(out.inner_rect);
            out.inner
        });
    keep_in_view(&out.response);
    // egui names the control with an empty string; a screen reader said
    // only "pop up button". Name it after its setting row, and give the
    // choice as its value.
    let name: Option<String> = ui.ctx().data(|d| d.get_temp(row_label_key()));
    let enabled = ui.is_enabled();
    out.response.widget_info(|| WidgetInfo {
        label: Some(name.clone().unwrap_or_else(|| shown.clone())),
        current_text_value: Some(shown.clone()),
        enabled,
        ..WidgetInfo::new(WidgetType::ComboBox)
    });
    if let Some(rect) = list {
        // How egui's ComboBox names its popup.
        let popup = out.response.id.with("popup");
        drawn_menu(ui.ctx(), popup, rect);
        // egui has just closed the list on Escape without using the key
        // up: the Stream settings panel behind it would close on it too.
        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    }
    out.inner
}

/// Where [`setting_row`] leaves its label for the control it holds.
fn row_label_key() -> Id {
    Id::new("latch.row_label")
}

/// A row in a [`select`]'s list: chosen when clicked, marked while chosen,
/// and scrolled into view when Tab reaches it.
pub fn select_option<T: PartialEq>(
    ui: &mut Ui,
    current: &mut T,
    value: T,
    label: impl Into<WidgetText>,
) -> Response {
    let response = ui.selectable_value(current, value, label);
    keep_in_view(&response);
    response
}

/// A horizontal slider for a whole number, with its value in mono beside
/// it. Drag or click the track; with keyboard focus the arrow keys step by
/// one and Shift steps by ten.
pub fn slider(
    ui: &mut Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    width: f32,
    label: &str,
) -> Response {
    let (lo, hi) = (*range.start(), *range.end());
    let knob = 7.0;
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(width, size::HIT_MIN.max(ui.spacing().interact_size.y)),
        Sense::click_and_drag(),
    );
    keep_in_view(&response);
    let rail = Rect::from_min_max(
        Pos2::new(rect.left() + knob, rect.center().y - 2.0),
        Pos2::new(rect.right() - knob, rect.center().y + 2.0),
    );
    let before = *value;
    if let Some(p) = response.interact_pointer_pos() {
        if response.dragged() || response.clicked() {
            let t = ((p.x - rail.left()) / rail.width()).clamp(0.0, 1.0);
            *value = (lo as f32 + t * (hi - lo) as f32).round() as u32;
        }
    }
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
        let (up, down, big) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown),
                i.modifiers.shift,
            )
        });
        let step = if big { 10 } else { 1 };
        if up {
            *value = value.saturating_add(step);
        }
        if down {
            *value = value.saturating_sub(step);
        }
    }
    // A screen reader's increment and decrement (VoiceOver's VO-Up and
    // VO-Down) and its "set to", as egui's own slider takes them.
    {
        use egui::accesskit::{Action, ActionData};
        let (inc, dec, set) = ui.input(|i| {
            (
                i.num_accesskit_action_requests(response.id, Action::Increment),
                i.num_accesskit_action_requests(response.id, Action::Decrement),
                i.accesskit_action_requests(response.id, Action::SetValue)
                    .filter_map(|r| match &r.data {
                        Some(ActionData::NumericValue(v)) => Some(*v),
                        _ => None,
                    })
                    .last(),
            )
        });
        *value = value.saturating_add(inc as u32).saturating_sub(dec as u32);
        if let Some(v) = set {
            *value = v.round().clamp(f64::from(lo), f64::from(hi)) as u32;
        }
    }
    *value = (*value).clamp(lo, hi);
    if *value != before {
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::slider(ui.is_enabled(), f64::from(*value), label));
    let now = *value;
    ui.ctx().accesskit_node_builder(response.id, |b| {
        use egui::accesskit::Action;
        b.set_min_numeric_value(f64::from(lo));
        b.set_max_numeric_value(f64::from(hi));
        b.set_numeric_value_step(1.0);
        b.add_action(Action::SetValue);
        if now < hi {
            b.add_action(Action::Increment);
        }
        if now > lo {
            b.add_action(Action::Decrement);
        }
    });
    if ui.is_rect_visible(rect) {
        let t = (*value - lo) as f32 / (hi - lo).max(1) as f32;
        let x = egui::lerp(rail.left()..=rail.right(), t);
        let enabled = ui.is_enabled();
        let r = CornerRadius::same(2);
        ui.painter().rect_filled(rail, r, P.border_strong);
        ui.painter().rect_filled(
            Rect::from_min_max(rail.min, Pos2::new(x, rail.max.y)),
            r,
            if enabled { P.text_secondary } else { P.border },
        );
        let hot = response.hovered() || response.dragged() || response.has_focus();
        let c = Pos2::new(x, rail.center().y);
        ui.painter().circle(
            c,
            if hot { knob + 1.0 } else { knob },
            if enabled { P.primary } else { P.text_disabled },
            stroke(2.0, P.surface),
        );
        if response.has_focus() {
            ui.painter()
                .circle_stroke(c, knob + 4.0, stroke(1.5, P.accent));
        }
    }
    response
}

/// The value beside a [`slider`], in a fixed width so the row does not
/// shift as it changes.
pub fn slider_value(ui: &mut Ui, text: &str) {
    ui.allocate_ui_with_layout(
        Vec2::new(64.0, size::HIT_MIN),
        Layout::right_to_left(Align::Center),
        |ui| {
            ui.label(
                RichText::new(text)
                    .font(theme::mono(text::MONO))
                    .color(P.text),
            );
        },
    );
}

pub(crate) fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Lay out `add` left to right, pushed against the right edge of the space
/// left in `ui`. Controls sit where a right-to-left layout would put them,
/// but keyboard focus and screen readers meet them in reading order: egui
/// focuses widgets in the order they are made, and a right-to-left layout
/// makes the rightmost first. The width is measured on the previous pass;
/// when it changes, the pass is discarded and laid out again before
/// anything is shown.
pub fn trailing<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let avail = ui.available_rect_before_wrap();
    let key = ui.auto_id_with("latch.trailing");
    let known: f32 = ui.data(|d| d.get_temp(key)).unwrap_or(0.0);
    let w = known.min(avail.width()).max(0.0);
    let rect = Rect::from_min_max(Pos2::new(avail.max.x - w, avail.min.y), avail.max);
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let inner = add(&mut child);
    let used = child.min_rect();
    let got = if used.is_positive() {
        used.width()
    } else {
        0.0
    };
    if (got - known).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(key, got));
        ui.ctx().request_discard("trailing controls changed width");
    }
    let response = ui.allocate_rect(
        if used.is_positive() {
            used
        } else {
            Rect::from_min_size(avail.right_top(), Vec2::ZERO)
        },
        Sense::hover(),
    );
    InnerResponse::new(inner, response)
}

/// A row with text on the left and controls on the right. The controls are
/// laid out first, so the text gets exactly the width they leave and wraps
/// there instead of running underneath them. When they leave too little,
/// the text takes the full width below the controls.
///
/// `min_h` is the row's height with padding; controls centre in it and
/// text starts `text_top` below its top. Controls are added left to right
/// (see [`trailing`]).
fn split_row<R>(
    ui: &mut Ui,
    min_h: f32,
    text_top: f32,
    right: impl FnOnce(&mut Ui) -> R,
    left: impl FnOnce(&mut Ui),
) -> R {
    let avail = ui.available_rect_before_wrap();
    let row = Rect::from_min_size(avail.min, Vec2::new(avail.width(), min_h));
    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(row)
            .layout(Layout::top_down(Align::Min)),
    );
    let placed = trailing(&mut right_ui, |ui| {
        ui.spacing_mut().item_spacing.x = space::SM;
        ui.set_min_height(min_h);
        right(ui)
    });
    let r = placed.inner;
    let used = if placed.response.rect.width() > 0.0 {
        placed.response.rect
    } else {
        Rect::NOTHING
    };
    let taken = if used.is_positive() {
        row.max.x - used.min.x + space::LG
    } else {
        0.0
    };
    let beside = row.width() - taken;
    let text_rect = if beside >= MIN_TEXT_BESIDE_CONTROLS || taken == 0.0 {
        Rect::from_min_size(row.min + Vec2::new(0.0, text_top), Vec2::new(beside, min_h))
    } else {
        Rect::from_min_size(
            Pos2::new(row.min.x, used.max.y + space::XS),
            Vec2::new(row.width(), min_h),
        )
    };
    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    left_ui.spacing_mut().item_spacing.y = space::XXS;
    left(&mut left_ui);
    let bottom = (left_ui.min_rect().max.y + text_top)
        .max(if used.is_positive() {
            used.max.y
        } else {
            row.min.y
        })
        .max(row.min.y + min_h);
    ui.allocate_rect(
        Rect::from_min_max(row.min, Pos2::new(row.max.x, bottom)),
        Sense::hover(),
    );
    r
}

/// Below this, the text goes under the controls instead of beside them.
const MIN_TEXT_BESIDE_CONTROLS: f32 = 180.0;

/// A settings row: a label (and optional hint) on the left, a control on
/// the right.
pub fn setting_row<R>(
    ui: &mut Ui,
    label: &str,
    hint: Option<&str>,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    let (h, top) = if hint.is_some() {
        (56.0, 10.0)
    } else {
        (size::ROW, 13.0)
    };
    // The label is drawn after the control (see `split_row`); a select in
    // the row takes its accessible name from here.
    let key = row_label_key();
    ui.ctx().data_mut(|d| d.insert_temp(key, label.to_owned()));
    let r = split_row(ui, h, top, control, |ui| {
        ui.label(RichText::new(label).color(P.text));
        if let Some(h) = hint {
            caption(ui, h);
        }
    });
    ui.ctx().data_mut(|d| d.remove::<String>(key));
    r
}

/// A settings row whose control is too wide to sit beside its label: the
/// label and hint, then the control under them.
pub fn setting_block<R>(
    ui: &mut Ui,
    label: &str,
    hint: Option<&str>,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    ui.add_space(space::MD - 2.0);
    ui.label(RichText::new(label).color(P.text));
    if let Some(h) = hint {
        ui.add_space(space::XXS);
        caption(ui, h);
    }
    ui.add_space(space::SM);
    let r = ui
        .horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(space::SM, space::SM);
            control(ui)
        })
        .inner;
    ui.add_space(space::MD);
    r
}

/// A [`setting_row`] whose control is an on/off switch. Returns true on change.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, label: &str, hint: Option<&str>) -> bool {
    setting_row(ui, label, hint, |ui| {
        labelled_toggle(ui, on, label).changed()
    })
}

/// Third-party notices, as bundled at build time.
pub const NOTICES: &str = include_str!("../../../NOTICE");

/// A row in a list of machines or devices: an optional status dot, the
/// name and one line of detail on the left, actions on the right, added in
/// reading order (left to right). The detail stays on one line
/// and is cut with an ellipsis rather than wrapping; the full text shows
/// on hover.
pub fn list_row(
    ui: &mut Ui,
    status: Option<Tone>,
    title: &str,
    detail: &str,
    actions: impl FnOnce(&mut Ui),
) {
    let start = ui.available_rect_before_wrap().min;
    let indent = if status.is_some() { 20.0 } else { 0.0 };
    split_row(ui, 60.0, 11.0, actions, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(indent);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = space::XXS;
                ui.add(
                    Label::new(
                        RichText::new(title)
                            .font(theme::medium(text::BODY))
                            .color(P.text),
                    )
                    .truncate(),
                )
                .on_hover_text(title);
                ui.add(
                    Label::new(
                        RichText::new(detail)
                            .text_style(theme::caption())
                            .color(P.text_secondary),
                    )
                    .truncate(),
                )
                .on_hover_text(detail);
            });
        });
    });
    if let Some(t) = status {
        let c = Pos2::new(start.x + size::DOT / 2.0 + 1.0, start.y + 11.0 + 9.0);
        ui.painter().circle_filled(c, size::DOT / 2.0, t.color());
    }
}

/// Hairline between rows of a [`group`] or a card.
pub fn row_separator(ui: &mut Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, stroke(1.0, P.hairline));
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// A status dot on its own.
pub fn status_dot(ui: &mut Ui, tone: Tone) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size::DOT), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size::DOT / 2.0, tone.color());
    response
}

/// A dot and a short secondary label: "● Online", "● Tailscale off".
/// Painted as one piece, so the dot leads in any layout direction.
pub fn status_text(ui: &mut Ui, tone: Tone, label: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        theme::regular(text::CAPTION),
        Color32::PLACEHOLDER,
    );
    let gap = space::SM - 2.0;
    let size = Vec2::new(
        size::DOT + gap + galley.size().x,
        galley.size().y.max(size::DOT),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, label));
    if ui.is_rect_visible(rect) {
        ui.painter().circle_filled(
            Pos2::new(rect.left() + size::DOT / 2.0, rect.center().y),
            size::DOT / 2.0,
            tone.color(),
        );
        ui.painter().galley(
            Pos2::new(
                rect.left() + size::DOT + gap,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            P.text_secondary,
        );
    }
    response
}

/// A small outlined mono tag for a fact: "1920 × 1080", "HEVC". With a
/// tone, a status dot leads it.
pub fn tag(ui: &mut Ui, tone: Option<Tone>, label: &str) -> Response {
    let font = theme::mono(text::LABEL + 0.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let dot = if tone.is_some() { size::DOT + 6.0 } else { 0.0 };
    let pad = space::SM;
    let size = Vec2::new(galley.size().x + dot + 2.0 * pad, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, label));
    if ui.is_rect_visible(rect) {
        ui.painter().rect(
            rect,
            CornerRadius::same(radius::SM),
            Color32::TRANSPARENT,
            stroke(1.0, P.border),
            StrokeKind::Inside,
        );
        if let Some(t) = tone {
            ui.painter().circle_filled(
                Pos2::new(rect.left() + pad + size::DOT / 2.0, rect.center().y),
                size::DOT / 2.0 - 0.5,
                t.color(),
            );
        }
        ui.painter().galley(
            Pos2::new(
                rect.left() + pad + dot,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            P.text_secondary,
        );
    }
    response
}

/// A status dot in the gutter and any content beside it, every line of it
/// starting at the text column: a checklist item with a link under its
/// text, say.
pub fn dot_item<R>(ui: &mut Ui, tone: Tone, add: impl FnOnce(&mut Ui) -> R) -> R {
    dotted(ui, tone, add)
}

/// A small coloured dot followed by wrapped text, for checklists.
pub fn dot_label(ui: &mut Ui, tone: Tone, text: &str) {
    dotted(ui, tone, |ui| {
        ui.add(Label::new(RichText::new(text).color(P.text)).wrap());
    });
}

/// A key as printed on the keyboard, in a box.
pub fn keycap(ui: &mut Ui, key: &str) -> Response {
    let font = theme::mono_medium(text::LABEL);
    let galley = ui
        .painter()
        .layout_no_wrap(key.to_owned(), font, Color32::PLACEHOLDER);
    let size = Vec2::new((galley.size().x + 10.0).max(20.0), 20.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect(
            rect,
            CornerRadius::same(radius::SM),
            P.raised,
            stroke(1.0, P.border_strong),
            StrokeKind::Inside,
        );
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, P.text);
    }
    response
}

/// A chord as keycaps, then what it does: `[Ctrl] [Alt] releases the mouse`.
pub fn shortcut(ui: &mut Ui, keys: &[&str], does: &str) -> Response {
    let label = format!("{} {does}", keys.join("+"));
    let r = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::XS;
            for k in keys {
                keycap(ui, k);
            }
            ui.add_space(space::XS);
            ui.label(
                RichText::new(does)
                    .text_style(theme::caption())
                    .color(P.text_secondary),
            );
        })
        .response;
    r.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    r
}

// ---------------------------------------------------------------------------
// Brand
// ---------------------------------------------------------------------------

/// The logo, for the window's top bar.
pub struct Brand;

impl Brand {
    pub fn new(_ctx: &egui::Context) -> Self {
        Self
    }

    /// The mark and the product name, as on the product site.
    pub fn lockup(&self, ui: &mut Ui, name: &str) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::SM;
            mark(ui, 20.0);
            ui.label(
                RichText::new(name)
                    .font(theme::semibold(text::TITLE + 1.0))
                    .extra_letter_spacing(-0.3)
                    .color(P.text),
            );
        });
    }
}

/// The logo, `size` points square: a square with its lower right corner
/// cut away, and a smaller blue square in the cut. Drawn from rectangles
/// on the logo's own grid (its 72 units run 14 to 86 in `docs/brand`), so
/// it is sharp at any size and on any screen.
pub fn mark(ui: &mut Ui, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_mark(ui, rect, P.text, P.accent);
    }
    response
}

fn paint_mark(ui: &Ui, rect: egui::Rect, ink: Color32, accent: Color32) {
    use egui::emath::GuiRounding;
    let ppp = ui.ctx().pixels_per_point();
    let unit = rect.width() / 72.0;
    let at = |x: f32, y: f32| rect.min + Vec2::new((x - 14.0) * unit, (y - 14.0) * unit);
    let part = |x0: f32, y0: f32, x1: f32, y1: f32, colour: Color32| {
        // Edges on whole pixels, so the corners stay crisp at 1x.
        let r = egui::Rect::from_min_max(at(x0, y0), at(x1, y1)).round_to_pixels(ppp);
        ui.painter().rect_filled(r, CornerRadius::ZERO, colour);
    };
    part(14.0, 14.0, 86.0, 54.0, ink);
    part(14.0, 54.0, 54.0, 86.0, ink);
    part(60.0, 60.0, 86.0, 86.0, accent);
}

/// A paced activity indicator: unlike egui's immediate-repaint spinner this
/// asks for at most 30 frames a second, and only while it is inside the
/// visible part of the window, so it stays bounded when the low-latency
/// stream renderer has vsync off. While the window is not the one in front
/// (behind another, minimised) it turns four times a second: a page left
/// on a spinner for hours redrew the whole window 30 times a second.
pub fn spinner(size: f32, color: egui::Color32) -> impl egui::Widget {
    move |ui: &mut egui::Ui| {
        let (rect, response) =
            ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            let time = ui.input(|i| i.time) as f32;
            let points = (0..=20)
                .map(|i| {
                    let angle = time * 4.0 + i as f32 * std::f32::consts::PI / 15.0;
                    rect.center() + egui::vec2(angle.cos(), angle.sin()) * (size * 0.4)
                })
                .collect();
            ui.painter()
                .add(egui::Shape::line(points, egui::Stroke::new(1.5_f32, color)));
            ui.ctx().request_repaint_after(spinner_pace(
                ui.input(|i| i.viewport().focused.unwrap_or(true)),
            ));
        }
        response
    }
}

/// How soon a [`spinner`] asks to be drawn again.
fn spinner_pace(focused: bool) -> std::time::Duration {
    std::time::Duration::from_millis(if focused { 33 } else { 250 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::contrast;

    #[test]
    fn every_button_kind_keeps_its_label_readable() {
        let surfaces = [P.bg, P.surface];
        for kind in [
            Kind::Primary,
            Kind::Secondary,
            Kind::Quiet,
            Kind::Danger,
            Kind::Destructive,
        ] {
            for (hovered, pressed) in [(false, false), (true, false), (true, true)] {
                let l = look(kind, true, hovered, pressed);
                for s in surfaces {
                    let behind = if l.fill.a() == 255 {
                        l.fill
                    } else if l.fill == Color32::TRANSPARENT {
                        s
                    } else {
                        // A translucent wash over the surface.
                        lerp_color(s, l.fill.to_opaque(), f32::from(l.fill.a()) / 255.0)
                    };
                    let c = contrast(l.ink, behind);
                    assert!(
                        c >= 4.5,
                        "{kind:?} hovered={hovered} pressed={pressed}: {c:.2}:1"
                    );
                }
            }
        }
    }

    #[test]
    fn a_popup_never_leaves_the_window() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 420.0));
        for y in [8.0, 40.0, 150.0, 260.0, 380.0] {
            for wanted in [60.0, 170.0, 400.0, 900.0] {
                let anchor = Rect::from_min_size(Pos2::new(560.0, y), Vec2::splat(32.0));
                let d = place_popup(screen, anchor, wanted, true);
                let (top, bottom) = if d.above {
                    (d.pos.y - d.max_height, d.pos.y)
                } else {
                    (d.pos.y, d.pos.y + d.max_height)
                };
                assert!(top >= screen.top() + space::SM, "{y} {wanted}: {d:?}");
                assert!(bottom <= screen.bottom() - space::SM, "{y} {wanted}: {d:?}");
                assert!(d.max_height > 0.0);
            }
        }
    }

    #[test]
    fn a_popup_opens_upwards_only_when_it_needs_to_and_there_is_more_room() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 420.0));
        let low = Rect::from_min_size(Pos2::new(560.0, 380.0), Vec2::splat(32.0));
        assert!(place_popup(screen, low, 170.0, true).above);
        let middle = Rect::from_min_size(Pos2::new(560.0, 200.0), Vec2::splat(32.0));
        assert!(
            !place_popup(screen, middle, 150.0, true).above,
            "it fits below"
        );
        let high = Rect::from_min_size(Pos2::new(8.0, 8.0), Vec2::splat(28.0));
        assert!(!place_popup(screen, high, 900.0, false).above);
    }

    /// A context with Latch's style, after one empty pass.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        crate::apply(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
        ctx
    }

    fn escape() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            events,
            ..Default::default()
        }
    }

    fn menu_id(label: &str) -> Id {
        Id::new("latch.menu").with(label)
    }

    #[test]
    fn escape_that_closes_a_menu_is_used_up() {
        let ctx = context();
        ctx.memory_mut(|m| m.open_popup(menu_id("Keys")));
        let mut later = None;
        let _ = ctx.run(input(vec![escape()]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                menu_button(ui, "Keys", |ui| menu_item(ui, "Escape"));
                later = Some(ui.input(|i| i.key_pressed(egui::Key::Escape)));
            });
        });
        assert!(
            !ctx.memory(|m| m.any_popup_open()),
            "Escape closes the menu"
        );
        assert_eq!(later, Some(false), "and nothing after it sees the key");
    }

    #[test]
    fn a_menu_stays_with_its_button_when_the_widgets_before_it_change() {
        let ctx = context();
        ctx.memory_mut(|m| m.open_popup(menu_id("More for Gaming-PC")));
        for extra in [0, 1, 3] {
            let _ = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    // A row added above shifts every position-based id.
                    for _ in 0..extra {
                        let _ = ui.button("Connect");
                    }
                    icon_menu(ui, Icon::More, "More for Gaming-PC", |ui| {
                        menu_item(ui, "Sleep")
                    });
                });
            });
            assert!(
                open_menu_rect(&ctx).is_some(),
                "still open with {extra} widgets before it"
            );
        }
    }

    #[test]
    fn a_menu_whose_button_is_gone_is_closed() {
        let ctx = context();
        ctx.memory_mut(|m| m.open_popup(menu_id("Keys")));
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                menu_button(ui, "Keys", |ui| menu_item(ui, "Escape"));
            });
        });
        assert!(ctx.memory(|m| m.any_popup_open()));
        // The toolbar folds into More: Keys is not drawn any more.
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let _ = ui.button("More");
            });
        });
        assert!(!ctx.memory(|m| m.any_popup_open()));
        // A popup that is not one of Latch's is left alone.
        let theirs = Id::new("someone else's popup");
        ctx.memory_mut(|m| m.open_popup(theirs));
        let _ = ctx.run(Default::default(), |_| {});
        assert!(ctx.memory(|m| m.is_popup_open(theirs)));
    }

    /// One pass of a "Resolution" setting row holding a select: what the
    /// screen reader is told about the select, whether anything after it
    /// still sees an Escape, and the id of the select's list.
    fn resolution_row(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> (Option<egui::accesskit::Node>, bool, Id) {
        let mut later = false;
        let mut list = Id::NULL;
        let out = ctx.run(input(events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                setting_row(ui, "Resolution", None, |ui| {
                    // How egui's ComboBox names its list.
                    list = ui.make_persistent_id(Id::new("res")).with("popup");
                    let mut pick = 1080;
                    select(ui, "res", "1080p", 160.0, |ui| {
                        for r in [720, 1080] {
                            select_option(ui, &mut pick, r, format!("{r}p"));
                        }
                    });
                });
                later = ui.input(|i| i.key_pressed(egui::Key::Escape));
            });
        });
        let node = out.platform_output.accesskit_update.and_then(|u| {
            u.nodes
                .into_iter()
                .map(|(_, n)| n)
                .find(|n| n.role() == egui::accesskit::Role::ComboBox)
        });
        (node, later, list)
    }

    #[test]
    fn a_select_is_named_after_its_row_and_reads_its_choice() {
        let ctx = context();
        ctx.enable_accesskit();
        let (node, _, _) = resolution_row(&ctx, vec![]);
        let node = node.expect("the select is in the tree");
        assert_eq!(node.label(), Some("Resolution"));
        assert_eq!(node.value(), Some("1080p"));
    }

    #[test]
    fn escape_that_closes_a_select_list_is_used_up() {
        let ctx = context();
        let (_, _, list) = resolution_row(&ctx, vec![]);
        ctx.memory_mut(|m| m.open_popup(list));
        let (_, later, _) = resolution_row(&ctx, vec![]);
        assert!(!later);
        assert!(ctx.memory(|m| m.is_popup_open(list)), "the list is open");
        let (_, later, _) = resolution_row(&ctx, vec![escape()]);
        assert!(
            !ctx.memory(|m| m.any_popup_open()),
            "Escape closes the list"
        );
        assert!(!later, "and the panel behind it does not see the key");
        // With the list closed, Escape is left for whoever wants it.
        let (_, later, _) = resolution_row(&ctx, vec![escape()]);
        assert!(later);
    }

    #[test]
    fn the_chosen_option_in_a_list_reads_at_aa() {
        let v = crate::theme::visuals();
        let wash = v.selection.bg_fill;
        for surface in [P.surface, P.raised] {
            let a = f32::from(wash.a()) / 255.0;
            let behind = lerp_color(surface, wash.to_opaque(), a);
            assert!(contrast(P.text, behind) >= 4.5);
        }
    }

    #[test]
    fn a_screen_reader_can_step_and_set_the_slider() {
        use egui::accesskit::{Action, ActionData, ActionRequest};
        let ctx = context();
        ctx.enable_accesskit();
        let mut value = 50u32;
        let pass = |ctx: &egui::Context, events, value: &mut u32| {
            let mut id = Id::NULL;
            let out = ctx.run(input(events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    id = slider(ui, value, 2..=150, 180.0, "Bitrate").id;
                });
            });
            (out, id)
        };
        let (out, id) = pass(&ctx, vec![], &mut value);
        let node = out
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .into_iter()
            .find(|(n, _)| *n == id.value().into())
            .map(|(_, n)| n)
            .expect("the slider is in the tree");
        assert_eq!(node.min_numeric_value(), Some(2.0));
        assert_eq!(node.max_numeric_value(), Some(150.0));
        assert!(node.supports_action(Action::Increment));
        let request = |action, data| {
            egui::Event::AccessKitActionRequest(ActionRequest {
                action,
                target: id.value().into(),
                data,
            })
        };
        pass(&ctx, vec![request(Action::Increment, None)], &mut value);
        assert_eq!(value, 51);
        pass(&ctx, vec![request(Action::Decrement, None)], &mut value);
        pass(&ctx, vec![request(Action::Decrement, None)], &mut value);
        assert_eq!(value, 49);
        pass(
            &ctx,
            vec![request(
                Action::SetValue,
                Some(ActionData::NumericValue(400.0)),
            )],
            &mut value,
        );
        assert_eq!(value, 150, "a value past the end is held to the range");
    }

    #[test]
    fn a_spinner_slows_down_when_its_window_is_not_in_front() {
        assert_eq!(spinner_pace(true).as_millis(), 33);
        assert!(spinner_pace(false) >= std::time::Duration::from_millis(250));
    }

    #[test]
    fn tones_have_distinct_marks_except_the_two_plain_ones() {
        assert_eq!(Tone::Neutral.color(), Tone::Info.color());
        let marks = [
            Tone::Neutral,
            Tone::Accent,
            Tone::Success,
            Tone::Warning,
            Tone::Danger,
        ]
        .map(Tone::color);
        for (i, a) in marks.iter().enumerate() {
            for b in &marks[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn list_row_renders_with_title_and_detail_tooltips() {
        let ctx = egui::Context::default();
        crate::apply(&ctx);
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                list_row(
                    ui,
                    Some(Tone::Success),
                    "Office-HighPerformance-Workstation-2026",
                    "Windows 11 · Online",
                    |_| {},
                );
            });
        });
    }
}
