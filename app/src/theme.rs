//! Look and feel: the auggiesoft **Instrument** look (auggiesoft UI guide, `looks/instrument.md`).
//!
//! Flat neutral chrome, black plot wells, and colour only on traces, markers and alarms. The colour
//! roles have one meaning each (`rules/color-semantics.md`): `ACCENT` is on/selected, `CAUTION` is
//! armed or out of tolerance, `ALARM` is failure, `OK` is healthy (status tile only), `INFO` is
//! advisory. Traces use the identity colours, never a state colour. See `../DESIGN.md`.

use std::sync::Arc;
use std::time::Instant;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin, Pos2, Rect, Response, RichText, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

// ---- tokens (tokens/tokens.css, data-look="instrument") ----
pub const BG: Color32 = Color32::from_rgb(0x11, 0x12, 0x14);
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x1d, 0x21);
pub const HEADER: Color32 = Color32::from_rgb(0x16, 0x17, 0x1a);
pub const WELL: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const LINE: Color32 = Color32::from_rgb(0x2c, 0x2e, 0x33);
pub const EDGE: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0c);
pub const CHIP: Color32 = Color32::from_rgb(0x26, 0x27, 0x2c);
pub const BTN: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x32);
pub const BTN_HI: Color32 = Color32::from_rgb(0x32, 0x34, 0x3a);
pub const INK: Color32 = Color32::from_rgb(0xe6, 0xe7, 0xea);
pub const INK2: Color32 = Color32::from_rgb(0xa9, 0xab, 0xb1);
/// Disabled and decorative only, never meaning.
pub const INK3: Color32 = Color32::from_rgb(0x6c, 0x6e, 0x74);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x0e);
/// On, selected, you are here. Never a data series.
pub const ACCENT: Color32 = Color32::from_rgb(0x5a, 0xa9, 0xf0);
pub const ACCENT_HI: Color32 = Color32::from_rgb(0x86, 0xc1, 0xf5);
/// The gold thread: the wordmark only.
pub const HOUSE: Color32 = Color32::from_rgb(0xc9, 0xa8, 0x4c);
/// A pointer is on this control: neutral, never a state colour.
pub const TOUCH: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);
pub const CAUTION: Color32 = Color32::from_rgb(0xf0, 0xa8, 0x30);
pub const ALARM: Color32 = Color32::from_rgb(0xff, 0x4d, 0x4d);
pub const OK: Color32 = Color32::from_rgb(0x3c, 0xcf, 0x6e);
pub const INFO: Color32 = Color32::from_rgb(0x9f, 0xd3, 0xff);
/// The live trace.
pub const TRACE: Color32 = Color32::from_rgb(0x5c, 0xc8, 0xe8);
pub const SCALE_INK: Color32 = Color32::from_rgb(0xa0, 0xa3, 0xa9);
pub const GRATICULE: Color32 = Color32::from_rgb(0x33, 0x36, 0x3c);
/// Trace identity: cyan, violet, sand, teal, white, lime, blue, grey. No red, amber, green or magenta.
pub const ID: [Color32; 8] = [
    Color32::from_rgb(0x5c, 0xc8, 0xe8),
    Color32::from_rgb(0xb0, 0x8c, 0xff),
    Color32::from_rgb(0xd8, 0xc9, 0xa3),
    Color32::from_rgb(0x3f, 0xb9, 0xa3),
    Color32::from_rgb(0xe6, 0xe7, 0xea),
    Color32::from_rgb(0xb5, 0xe0, 0x5a),
    Color32::from_rgb(0x7a, 0xa2, 0xff),
    Color32::from_rgb(0x8a, 0x8d, 0x93),
];
/// Peak hold and average traces (identity colours, so they never read as a state).
pub const PEAK: Color32 = ID[2];
pub const AVERAGE: Color32 = ID[1];

// ---- type scale (px) ----
pub const XS: f32 = 11.0;
pub const SM: f32 = 13.0;
pub const MD: f32 = 15.0;
pub const LG: f32 = 20.0;
pub const RADIUS: u8 = 6;
/// Toasts only acknowledge the user's own action (rule A1).
pub const TOAST_S: f32 = 2.6;

// ---- fonts (rules/signature.md §1) ----
const LABEL: &str = "label";
const HEADLINE: &str = "headline";
const NUM_MEDIUM: &str = "num-medium";

/// UI text: Barlow Medium.
pub fn ui_font(size: f32) -> FontId {
    FontId::proportional(size)
}
/// Values, frequencies, ticks and identifiers: Auggie Mono.
pub fn num(size: f32) -> FontId {
    FontId::monospace(size)
}
pub fn num_medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(NUM_MEDIUM.into()))
}
/// Headline numerals: Barlow SemiBold with tabular figures.
pub fn headline(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(HEADLINE.into()))
}
pub fn label_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(LABEL.into()))
}

/// An uppercase label: Barlow Semi Condensed, tracking 0.08 em.
pub fn label(text: &str, size: f32) -> RichText {
    RichText::new(text.to_uppercase()).font(label_font(size)).extra_letter_spacing(size * 0.08).color(INK2)
}

fn load_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add("barlow-500", include_bytes!("../assets/fonts/Barlow-500.ttf"));
    add("barlow-tab-600", include_bytes!("../assets/fonts/BarlowTabular-600.ttf"));
    add("barlow-sc-600", include_bytes!("../assets/fonts/BarlowSemiCondensed-600.ttf"));
    add("auggie-400", include_bytes!("../assets/fonts/AuggieMono-400.ttf"));
    add("auggie-500", include_bytes!("../assets/fonts/AuggieMono-500.ttf"));
    // egui's own fonts stay behind ours for glyphs the latin subsets lack (thin space, ↶, ⚙)
    let fallback_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fallback_mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let fam = |first: &str, rest: &[String]| std::iter::once(first.to_string()).chain(rest.iter().cloned()).collect::<Vec<_>>();
    fonts.families.insert(FontFamily::Proportional, fam("barlow-500", &fallback_prop));
    fonts.families.insert(FontFamily::Monospace, fam("auggie-400", &fallback_mono));
    fonts.families.insert(FontFamily::Name(NUM_MEDIUM.into()), fam("auggie-500", &fallback_mono));
    fonts.families.insert(FontFamily::Name(HEADLINE.into()), fam("barlow-tab-600", &fallback_prop));
    fonts.families.insert(FontFamily::Name(LABEL.into()), fam("barlow-sc-600", &fallback_prop));
    ctx.set_fonts(fonts);
}

/// Rule R2: legible by default, compact for experts. Hit areas stay ≥ 44 px in both (rule I4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Density {
    /// Visual control height, and the extra pointer reach around it that makes up the 44 px.
    fn control(self) -> (f32, f32) {
        match self {
            Density::Comfortable => (28.0, 8.0),
            Density::Compact => (24.0, 10.0),
        }
    }
    pub fn gap(self) -> f32 {
        match self {
            Density::Comfortable => 8.0,
            Density::Compact => 5.0,
        }
    }
    pub fn text(self) -> f32 {
        match self {
            Density::Comfortable => 14.0,
            Density::Compact => 13.0,
        }
    }
}

pub fn install_fonts(ctx: &egui::Context) {
    load_fonts(ctx);
}

pub fn apply(ctx: &egui::Context, density: Density) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, LINE);
    v.window_corner_radius = CornerRadius::same(RADIUS + 2);
    v.window_shadow = egui::Shadow { offset: [0, 2], blur: 10, spread: 0, color: Color32::from_black_alpha(110) };
    v.popup_shadow = v.window_shadow;
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.extreme_bg_color = WELL;
    v.faint_bg_color = CHIP;
    v.code_bg_color = WELL;
    v.override_text_color = Some(INK);
    v.weak_text_color = Some(INK2);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.text_cursor.stroke = Stroke::new(2.0, TOUCH);
    v.slider_trailing_fill = true;
    for w in [&mut v.widgets.inactive, &mut v.widgets.noninteractive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(RADIUS);
        w.expansion = 0.0;
    }
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.inactive.bg_fill = BTN;
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, EDGE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);
    // hover and press: neutral touch ring, never a state colour (rule C5)
    v.widgets.hovered.bg_fill = BTN_HI;
    v.widgets.hovered.weak_bg_fill = BTN_HI;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, TOUCH.gamma_multiply(0.55));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.active.bg_fill = BTN_HI;
    v.widgets.active.weak_bg_fill = BTN_HI;
    v.widgets.active.bg_stroke = Stroke::new(1.5, TOUCH);
    v.widgets.active.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.open = v.widgets.hovered;
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    let (h, reach) = density.control();
    let text = density.text();
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(6.0, density.gap() * 0.6);
        s.spacing.button_padding = vec2(10.0, 4.0);
        s.spacing.interact_size = vec2(40.0, h);
        s.spacing.menu_margin = Margin::same(8);
        s.spacing.window_margin = Margin::same(14);
        s.interaction.interact_radius = reach;
        s.interaction.selectable_labels = false;
        s.drag_value_text_style = egui::TextStyle::Monospace;
        s.text_styles.insert(egui::TextStyle::Body, ui_font(text));
        s.text_styles.insert(egui::TextStyle::Button, ui_font(text));
        s.text_styles.insert(egui::TextStyle::Small, ui_font(XS + 1.0));
        s.text_styles.insert(egui::TextStyle::Monospace, num(text - 0.5));
        s.text_styles.insert(egui::TextStyle::Heading, ui_font(LG));
    });
}

pub fn top_bar_frame() -> Frame {
    Frame::new().fill(HEADER).inner_margin(Margin::symmetric(16, 6)).stroke(Stroke::new(1.0, LINE))
}

pub fn status_frame() -> Frame {
    Frame::new().fill(HEADER).inner_margin(Margin::symmetric(16, 4))
}

pub fn side_frame() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin { left: 10, right: 12, top: 10, bottom: 10 })
}

pub fn central_frame() -> Frame {
    Frame::new().fill(BG).inner_margin(Margin::same(10))
}

/// A flat panel with a hairline rim.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

pub fn section(ui: &mut Ui, title: &str) {
    ui.label(label(title, SM));
}

/// A section that starts closed: rarely used controls sit one labelled click away (rule L4).
pub fn more<R>(ui: &mut Ui, id: &str, title: &str, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    egui::CollapsingHeader::new(RichText::new(title).size(SM).color(INK2)).id_salt(id).default_open(false).show(ui, add).body_returned
}

/// A whole card that starts closed, titled like a section (rule L4).
pub fn card_more<R>(ui: &mut Ui, id: &str, title: &str, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    card(ui, |ui| egui::CollapsingHeader::new(label(title, SM)).id_salt(id).default_open(false).show(ui, add).body_returned)
}

pub fn caption(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(XS + 1.0).color(INK2));
}

/// A chip: lit (accent fill, dark ink) when selected, flat otherwise (rule C4).
pub fn chip(ui: &mut Ui, selected: bool, text: &str) -> Response {
    let (fill, stroke, fg) = if selected { (ACCENT, Stroke::new(1.0, ACCENT_HI), ON_ACCENT) } else { (BTN, Stroke::new(1.0, EDGE), INK2) };
    ui.add(egui::Button::new(RichText::new(text).color(fg)).fill(fill).stroke(stroke).corner_radius(RADIUS))
}

/// Segmented control over `options`; returns true when changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(3.0, 4.0);
        for (v, text) in options {
            if chip(ui, *value == *v, text).clicked() && *value != *v {
                *value = *v;
                changed = true;
            }
        }
    });
    changed
}

pub const ROW_LABEL_W: f32 = 82.0;

/// A labelled row: label left, widget right.
pub fn row<R>(ui: &mut Ui, text: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        ui.add_sized([ROW_LABEL_W, h], egui::Label::new(RichText::new(text).size(SM).color(INK2)));
        add(ui)
    })
    .inner
}

/// A labelled row whose label explains the control: hover shows the text, a click pins it open
/// under the row (rules/signature.md §4, "hover first, click to pin"). The dotted underline says
/// there's more to read.
pub fn row_hint<R>(ui: &mut Ui, text: &str, hint: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let id = ui.id().with(("hint", text));
    let mut pinned: bool = ui.data(|d| d.get_temp(id)).unwrap_or(false);
    let out = ui
        .horizontal(|ui| {
            let h = ui.spacing().interact_size.y;
            let lab = ui.add_sized([ROW_LABEL_W, h], egui::Label::new(RichText::new(text).size(SM).color(INK2)).sense(Sense::click()));
            let g = ui.painter().layout_no_wrap(text.to_string(), ui_font(SM), INK2);
            let y = lab.rect.center().y + g.size().y / 2.0 + 1.0;
            let x0 = lab.rect.center().x - g.size().x / 2.0;
            ui.painter().extend(Shape::dotted_line(&[pos2(x0, y), pos2(x0 + g.size().x, y)], INK3, 3.0, 0.7));
            let lab = if pinned { lab } else { lab.on_hover_text(hint) };
            if lab.clicked() {
                pinned = !pinned;
            }
            add(ui)
        })
        .inner;
    if pinned {
        caption(ui, hint);
    }
    ui.data_mut(|d| d.insert_temp(id, pinned));
    out
}

/// A button that restores a previous value: a back arrow, dashed outline, quiet ink.
pub fn restore_button(ui: &mut Ui, text: &str) -> Response {
    let r = ui.add(egui::Button::new(RichText::new(format!("       {text}")).size(SM).color(INK2)).fill(Color32::TRANSPARENT).stroke(Stroke::NONE).corner_radius(RADIUS));
    let c = if r.hovered() { INK } else { INK2 };
    dashed_rect(ui.painter(), r.rect.shrink(1.0), Stroke::new(1.0, c.gamma_multiply(0.8)));
    back_icon(ui.painter(), pos2(r.rect.left() + 16.0, r.rect.center().y), c);
    r
}

/// "↶", drawn: no bundled face has U+21B6.
pub fn back_icon(p: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let pts: Vec<Pos2> = (0..=10)
        .map(|i| {
            let a = std::f32::consts::PI * (1.0 + i as f32 / 10.0 * 1.15);
            pos2(c.x + 5.0 * a.cos() + 1.0, c.y + 5.0 * a.sin() + 3.0)
        })
        .collect();
    let start = pts[0];
    p.add(Shape::line(pts, stroke));
    p.line_segment([start, start + vec2(-2.5, -3.5)], stroke);
    p.line_segment([start, start + vec2(3.5, -1.5)], stroke);
}

/// Two-step confirm (rule S5): the first click arms a hatched caution button that names the
/// outcome, the second commits, and it disarms by itself after 4 s. Returns true on commit.
pub fn confirm_button(ui: &mut Ui, idle: &str, armed: &str) -> bool {
    let id = ui.next_auto_id().with("confirm");
    let armed_at: Option<Instant> = ui.data(|d| d.get_temp(id)).flatten();
    let is_armed = armed_at.is_some_and(|t| t.elapsed().as_secs_f32() < 4.0);
    if is_armed {
        let galley = ui.painter().layout_no_wrap(armed.to_string(), ui_font(SM), ON_ACCENT);
        let size = galley.size() + vec2(20.0, 0.0);
        let (r, resp) = ui.allocate_exact_size(vec2(size.x, ui.spacing().interact_size.y), Sense::click());
        let p = ui.painter();
        p.rect_filled(r, RADIUS, CAUTION);
        hatch(p, r, CAUTION.gamma_multiply(0.0), Color32::from_rgb(0xc8, 0x86, 0x1c));
        p.galley(pos2(r.left() + 10.0, r.center().y - galley.size().y / 2.0), galley, ON_ACCENT);
        if resp.hovered() {
            p.rect_stroke(r, RADIUS, Stroke::new(1.5, TOUCH), StrokeKind::Outside);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        if resp.clicked() {
            ui.data_mut(|d| d.insert_temp::<Option<Instant>>(id, None));
            return true;
        }
    } else {
        if ui.button(RichText::new(idle).size(SM)).clicked() {
            ui.data_mut(|d| d.insert_temp(id, Some(Instant::now())));
        }
        if armed_at.is_some() {
            ui.data_mut(|d| d.insert_temp::<Option<Instant>>(id, None));
        }
    }
    false
}

/// 45° stripes over a rect (caution is always hatched as well as coloured).
pub fn hatch(p: &egui::Painter, r: Rect, _bg: Color32, stripe: Color32) {
    let p = p.with_clip_rect(r);
    let mut x = r.left() - r.height();
    while x < r.right() {
        p.add(Shape::convex_polygon(
            vec![pos2(x, r.bottom()), pos2(x + 7.0, r.bottom()), pos2(x + 7.0 + r.height(), r.top()), pos2(x + r.height(), r.top())],
            stripe,
            Stroke::NONE,
        ));
        x += 14.0;
    }
}

pub fn dashed_rect(p: &egui::Painter, r: Rect, stroke: Stroke) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    p.extend(Shape::dashed_line(&pts, stroke, 4.0, 3.0));
}

/// How a slider maps and shows its value.
pub struct Slide {
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub integer: bool,
    pub log: bool,
    pub decimals: usize,
    pub unit: &'static str,
}

impl Slide {
    pub const fn int(min: f64, max: f64, default: f64, unit: &'static str) -> Slide {
        Slide { min, max, default, integer: true, log: false, decimals: 0, unit }
    }
    fn norm(&self, v: f64) -> f64 {
        if self.log { (v / self.min).ln() / (self.max / self.min).ln() } else { (v - self.min) / (self.max - self.min) }
    }
    fn denorm(&self, n: f64) -> f64 {
        let n = n.clamp(0.0, 1.0);
        let v = if self.log { self.min * (self.max / self.min).powf(n) } else { self.min + n * (self.max - self.min) };
        self.snap(v)
    }
    fn snap(&self, v: f64) -> f64 {
        let v = v.clamp(self.min, self.max);
        if self.integer { v.round() } else { v }
    }
    fn fmt(&self, v: f64) -> String {
        format!("{v:.*}", self.decimals)
    }
}

#[derive(Clone, Default)]
struct SlideState {
    /// Float position while dragging, so integer sliders don't stall.
    norm: f64,
    reach: f32,
    moved: bool,
    before: f64,
    ghost: Option<f64>,
    edit: Option<String>,
    focus_edit: bool,
    wheel_at: Option<(Instant, f64)>,
}

/// A slider that never jumps (rule S2, rules/signature.md §4):
/// - clicking does nothing; dragging moves the value relative to where it was, after a 4 px dead zone;
/// - Shift drags at ¼ speed, the wheel steps it, double-click types an exact value;
/// - double-click the label resets it, right-click offers reset and back;
/// - after a change the old value stays as a dashed ghost with a ↶ button back to it (rule S4).
///
/// Returns true when the value changed.
pub fn slider(ui: &mut Ui, text: &str, v: &mut f64, s: &Slide) -> bool {
    let start = *v;
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        let id = ui.id().with(("slide", text));
        let mut st: SlideState = ui.data(|d| d.get_temp(id)).unwrap_or_default();
        let lab = ui.add_sized([ROW_LABEL_W, h], egui::Label::new(RichText::new(text).size(SM).color(INK2)).sense(Sense::click()));
        if lab.double_clicked() && *v != s.default {
            st.ghost = Some(*v);
            *v = s.default;
        }
        let lab_hint = format!("Double-click to reset to {} {}", s.fmt(s.default), s.unit);
        lab.on_hover_text(lab_hint);
        let value_w = 64.0;
        let back_w = 26.0;
        let w = (ui.available_width() - value_w - back_w - 12.0).max(60.0);
        let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
        if let Some(text) = &mut st.edit {
            let te = ui.put(r, egui::TextEdit::singleline(text).font(num(SM)).desired_width(w));
            if std::mem::take(&mut st.focus_edit) {
                te.request_focus();
            } else if te.lost_focus() || !te.has_focus() {
                let typed = st.edit.take().unwrap_or_default();
                if !ui.input(|i| i.key_pressed(egui::Key::Escape))
                    && let Ok(x) = typed.trim().trim_end_matches(s.unit).trim().parse::<f64>()
                {
                    let x = s.snap(x);
                    if x != *v {
                        st.ghost = Some(*v);
                        *v = x;
                    }
                }
            }
        } else {
            if resp.drag_started() {
                st.norm = s.norm(*v);
                st.reach = 0.0;
                st.moved = false;
                st.before = *v;
            }
            if resp.dragged() {
                let dx = resp.drag_delta().x;
                if st.moved {
                    let k = if ui.input(|i| i.modifiers.shift) { 0.25 } else { 1.0 };
                    st.norm = (st.norm + (dx / w) as f64 * k).clamp(0.0, 1.0);
                    *v = s.denorm(st.norm);
                } else {
                    st.reach += dx;
                    st.moved = st.reach.abs() >= 4.0;
                }
            }
            if resp.drag_stopped() && st.moved && *v != st.before {
                st.ghost = Some(st.before);
            }
            if resp.hovered() {
                let wheel: f32 = ui.input(|i| i.raw.events.iter().map(|e| if let egui::Event::MouseWheel { delta, .. } = e { delta.y } else { 0.0 }).sum());
                if wheel != 0.0 {
                    let fresh = st.wheel_at.is_none_or(|(t, _)| t.elapsed().as_secs_f32() > 1.2);
                    let before = if fresh { *v } else { st.wheel_at.map_or(*v, |(_, b)| b) };
                    let step = if s.integer { wheel.signum() as f64 } else { 0.0 };
                    let x = if s.integer { s.snap(*v + step) } else { s.denorm(s.norm(*v) + 0.02 * wheel.signum() as f64) };
                    *v = x;
                    st.wheel_at = Some((Instant::now(), before));
                    if x != before {
                        st.ghost = Some(before);
                    }
                }
            }
            if resp.double_clicked() {
                st.edit = Some(s.fmt(*v));
                st.focus_edit = true;
            }
            paint_slider(ui, r, &resp, *v, st.ghost, s);
        }
        resp.context_menu(|ui| {
            if ui.button(format!("Reset to {} {}", s.fmt(s.default), s.unit)).clicked() {
                st.ghost = Some(*v);
                *v = s.default;
                ui.close();
            }
            if let Some(g) = st.ghost
                && ui.button(format!("Back to {} {}", s.fmt(g), s.unit)).clicked()
            {
                st.ghost = Some(*v);
                *v = g;
                ui.close();
            }
            if ui.button("Type a value").clicked() {
                st.edit = Some(s.fmt(*v));
                st.focus_edit = true;
                ui.close();
            }
        });
        let (vr, _) = ui.allocate_exact_size(vec2(value_w, h), Sense::hover());
        let p = ui.painter();
        let vg = p.layout_no_wrap(s.fmt(*v), num(SM), INK);
        let ug = p.layout_no_wrap(s.unit.to_string(), ui_font(XS + 1.0), INK2);
        let y = vr.center().y;
        let x0 = vr.right() - ug.size().x - if s.unit.is_empty() { 0.0 } else { 3.0 } - vg.size().x;
        p.galley(pos2(x0, y - vg.size().y / 2.0), vg.clone(), INK);
        p.galley(pos2(vr.right() - ug.size().x, y - ug.size().y / 2.0 + 1.0), ug, INK2);
        match st.ghost {
            Some(g) if g != *v => {
                let b = ui.add_sized([back_w, h], egui::Button::new("").fill(Color32::TRANSPARENT).stroke(Stroke::NONE)).on_hover_text(format!("Back to {} {}", s.fmt(g), s.unit));
                back_icon(ui.painter(), b.rect.center(), if b.hovered() { INK } else { INK2 });
                if b.hovered() {
                    dashed_rect(ui.painter(), b.rect.shrink(2.0), Stroke::new(1.0, INK2));
                }
                if b.clicked() {
                    st.ghost = Some(*v);
                    *v = g;
                }
            }
            _ => {
                ui.allocate_exact_size(vec2(back_w, h), Sense::hover());
            }
        }
        ui.data_mut(|d| d.insert_temp(id, st));
    });
    *v != start
}

fn paint_slider(ui: &Ui, r: Rect, resp: &Response, v: f64, ghost: Option<f64>, s: &Slide) {
    let p = ui.painter();
    let track = Rect::from_center_size(r.center(), vec2(r.width() - 12.0, 6.0));
    p.rect_filled(track, 3, WELL);
    p.rect_stroke(track, 3, Stroke::new(1.0, LINE), StrokeKind::Outside);
    let x_of = |x: f64| track.left() + s.norm(x).clamp(0.0, 1.0) as f32 * track.width();
    let x = x_of(v);
    p.rect_filled(Rect::from_min_max(track.min, pos2(x, track.bottom())), 3, INK2.gamma_multiply(0.55));
    let cap = |x: f32| Rect::from_center_size(pos2(x, r.center().y), vec2(12.0, r.height() - 8.0));
    if let Some(g) = ghost.filter(|&g| g != v) {
        dashed_rect(p, cap(x_of(g)), Stroke::new(1.0, INK2));
    }
    let c = cap(x);
    p.rect_filled(c, 3, BTN_HI);
    p.line_segment([pos2(c.left() + 2.0, c.top() + 1.0), pos2(c.right() - 2.0, c.top() + 1.0)], Stroke::new(1.0, Color32::from_white_alpha(18)));
    p.line_segment([pos2(c.left() + 3.0, c.center().y), pos2(c.right() - 3.0, c.center().y)], Stroke::new(1.0, INK2));
    let ring = if resp.dragged() { Some(1.5) } else if resp.hovered() { Some(1.0) } else { None };
    p.rect_stroke(c, 3, Stroke::new(ring.unwrap_or(1.0), if ring.is_some() { TOUCH } else { EDGE }), StrokeKind::Outside);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
}

pub fn slider_u8(ui: &mut Ui, text: &str, v: &mut u8, s: &Slide) -> bool {
    let mut x = *v as f64;
    let ch = slider(ui, text, &mut x, s);
    *v = x.round() as u8;
    ch
}

pub fn slider_usize(ui: &mut Ui, text: &str, v: &mut usize, s: &Slide) -> bool {
    let mut x = *v as f64;
    let ch = slider(ui, text, &mut x, s);
    *v = x.round() as usize;
    ch
}

pub fn slider_f32(ui: &mut Ui, text: &str, v: &mut f32, s: &Slide) -> bool {
    let mut x = *v as f64;
    let ch = slider(ui, text, &mut x, s);
    *v = x as f32;
    ch
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// The status tile's dot: a circle when well, a triangle otherwise, so the level reads without colour.
pub fn status_dot(p: &egui::Painter, c: Pos2, level: Level) {
    match level {
        Level::Ok => {
            p.circle_filled(c, 5.0, OK);
        }
        Level::Info => {
            p.circle_stroke(c, 4.5, Stroke::new(1.5, INFO));
        }
        Level::Caution | Level::Alarm => {
            let col = if level == Level::Caution { CAUTION } else { ALARM };
            let tri = vec![pos2(c.x, c.y - 6.0), pos2(c.x + 6.0, c.y + 5.0), pos2(c.x - 6.0, c.y + 5.0)];
            if level == Level::Caution {
                p.add(Shape::closed_line(tri, Stroke::new(1.6, col)));
            } else {
                p.add(Shape::convex_polygon(tri, col, Stroke::NONE));
            }
        }
    }
}

/// Status levels, in order of severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Info,
    Caution,
    Alarm,
}

/// Waterfall colour maps. Only perceptually ordered maps (rule C9): no rainbow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Palette {
    /// Older settings may still name the retired Hydra and Turbo maps.
    #[serde(alias = "Hydra", alias = "Turbo")]
    Inferno,
    Viridis,
    Mono,
}

impl Palette {
    pub const ALL: [(Palette, &'static str); 3] = [(Palette::Inferno, "Inferno"), (Palette::Viridis, "Viridis"), (Palette::Mono, "Grey")];

    fn stops(self) -> &'static [(f32, [u8; 3])] {
        match self {
            // the look's colour map (--cmap-0…4), with inferno's in-between stops
            Palette::Inferno => &[
                (0.00, [0x00, 0x00, 0x04]),
                (0.20, [0x28, 0x0b, 0x54]),
                (0.30, [0x42, 0x0a, 0x68]),
                (0.50, [0x93, 0x26, 0x67]),
                (0.75, [0xdd, 0x51, 0x3a]),
                (0.88, [0xf5, 0x7d, 0x15]),
                (1.00, [0xfc, 0xff, 0xa4]),
            ],
            Palette::Viridis => &[
                (0.00, [0x44, 0x01, 0x54]),
                (0.25, [0x3b, 0x52, 0x8b]),
                (0.50, [0x21, 0x91, 0x8c]),
                (0.75, [0x5e, 0xc9, 0x62]),
                (1.00, [0xfd, 0xe7, 0x25]),
            ],
            Palette::Mono => &[(0.0, [0x00, 0x00, 0x00]), (1.0, [0xff, 0xff, 0xff])],
        }
    }

    pub fn lut(self) -> Vec<Color32> {
        let s = self.stops();
        (0..256)
            .map(|i| {
                let t = i as f32 / 255.0;
                let k = s.iter().position(|(p, _)| *p >= t).unwrap_or(s.len() - 1).max(1);
                let (p0, c0) = s[k - 1];
                let (p1, c1) = s[k];
                let u = if p1 > p0 { (t - p0) / (p1 - p0) } else { 0.0 };
                lerp(Color32::from_rgb(c0[0], c0[1], c0[2]), Color32::from_rgb(c1[0], c1[1], c1[2]), u)
            })
            .collect()
    }
}
