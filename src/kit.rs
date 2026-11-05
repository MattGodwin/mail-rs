//! Small, consistent UI building blocks on top of egui, drawn from the
//! theme's tokens: icon buttons, primary/quiet buttons, switches, avatars,
//! navigation rows, inline notices, empty states and settings rows.

use crate::theme::{self, ty, Palette};
use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
pub use egui_material_icons::icons as ic;
use egui_material_icons::MaterialIcon;

pub const ICON_SIZE: f32 = 20.0;

pub fn pal(ui: &Ui) -> Palette {
    theme::palette(ui.ctx())
}

/// Outline icon glyph (filled = active state).
pub fn glyph(icon: MaterialIcon, size: f32, color: Color32) -> RichText {
    icon.outlined().rich_text().size(size).color(color)
}

fn paint_icon(ui: &Ui, rect: Rect, icon: MaterialIcon, filled: bool, size: f32, color: Color32) {
    let font = FontId::new(size, if filled { icon.filled().font_family() } else { icon.outlined().font_family() });
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, <&str>::from(icon), font, color);
}

/// A 32×32 frameless icon button with a tooltip (the accessible name).
pub fn icon_button(ui: &mut Ui, icon: MaterialIcon, tooltip: &str) -> Response {
    icon_button_ex(ui, icon, tooltip, false, None)
}

pub fn icon_button_ex(ui: &mut Ui, icon: MaterialIcon, tooltip: &str, active: bool, tint: Option<Color32>) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), tooltip));
    if ui.is_rect_visible(rect) {
        if resp.hovered() || resp.has_focus() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(2.0, p.accent), StrokeKind::Inside);
        }
        let color = if !ui.is_enabled() { p.text_disabled } else { tint.unwrap_or(p.text_secondary) };
        paint_icon(ui, rect, icon, active, ICON_SIZE, color);
    }
    resp.on_hover_text(tooltip)
}

/// A quiet button: icon + label, no fill until hovered.
pub fn quiet_button(ui: &mut Ui, icon: Option<MaterialIcon>, label: &str) -> Response {
    let p = pal(ui);
    let font = theme::regular(ty::UI);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, p.text);
    let icon_w = if icon.is_some() { ICON_SIZE + 6.0 } else { 0.0 };
    let size = egui::vec2(galley.size().x + icon_w + 24.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    if ui.is_rect_visible(rect) {
        if resp.hovered() || resp.has_focus() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(2.0, p.accent), StrokeKind::Inside);
        }
        let mut x = rect.left() + 12.0;
        let color = if ui.is_enabled() { p.text } else { p.text_disabled };
        if let Some(icon) = icon {
            let r = Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(ICON_SIZE, rect.height()));
            paint_icon(ui, r, icon, false, ICON_SIZE, if ui.is_enabled() { p.text_secondary } else { p.text_disabled });
            x += icon_w;
        }
        ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, color);
    }
    resp
}

/// The one high-emphasis action on a surface.
pub fn primary_button(ui: &mut Ui, icon: Option<MaterialIcon>, label: &str) -> Response {
    filled_button(ui, icon, label, true)
}

/// A destructive action, named for its consequence ("Delete permanently").
pub fn danger_button(ui: &mut Ui, label: &str) -> Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(label.to_string(), theme::semibold(ty::UI), p.on_accent);
    let size = egui::vec2((galley.size().x + 32.0).max(80.0), 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() { lerp(p.error, if p.dark { Color32::WHITE } else { Color32::BLACK }, 0.1) } else { p.error };
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        let fg = if p.dark { Color32::from_rgb(0x2A, 0x0A, 0x0E) } else { Color32::WHITE };
        let galley = ui.painter().layout_no_wrap(label.to_string(), theme::semibold(ty::UI), fg);
        ui.painter().galley(rect.center() - galley.size() / 2.0, galley, fg);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(8), Stroke::new(2.0, p.accent), StrokeKind::Outside);
        }
    }
    resp
}

/// The standard modal frame.
pub fn dialog_frame(ctx: &egui::Context) -> egui::Frame {
    let p = theme::palette(ctx);
    egui::Frame::new()
        .fill(p.raised)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(24))
        .shadow(ctx.global_style().visuals.window_shadow)
}

/// A bordered secondary button (Cancel, Back…).
pub fn secondary_button(ui: &mut Ui, label: &str) -> Response {
    filled_button(ui, None, label, false)
}

fn filled_button(ui: &mut Ui, icon: Option<MaterialIcon>, label: &str, primary: bool) -> Response {
    let p = pal(ui);
    let enabled = ui.is_enabled();
    let (fill, fg) = if primary { (p.accent, p.on_accent) } else { (p.surface, p.text) };
    let font = theme::semibold(ty::UI);
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, fg);
    let icon_w = if icon.is_some() { ICON_SIZE + 6.0 } else { 0.0 };
    let size = egui::vec2((galley.size().x + icon_w + 32.0).max(80.0), 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    if ui.is_rect_visible(rect) {
        let mut f = fill;
        if !enabled {
            f = if primary { p.hover } else { p.surface };
        } else if resp.is_pointer_button_down_on() {
            f = if primary { f.gamma_multiply(0.85) } else { p.selected };
        } else if resp.hovered() {
            f = if primary { lerp(fill, if p.dark { Color32::WHITE } else { Color32::BLACK }, 0.10) } else { p.hover };
        }
        ui.painter().rect_filled(rect, CornerRadius::same(6), f);
        if !primary {
            ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, p.outline), StrokeKind::Inside);
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(8), Stroke::new(2.0, p.accent), StrokeKind::Outside);
        }
        let fg = if enabled { fg } else { p.text_disabled };
        let total = galley.size().x + icon_w;
        let mut x = rect.center().x - total / 2.0;
        if let Some(icon) = icon {
            let r = Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(ICON_SIZE, rect.height()));
            paint_icon(ui, r, icon, false, ICON_SIZE, fg);
            x += icon_w;
        }
        let galley = ui.painter().layout_no_wrap(label.to_string(), theme::semibold(ty::UI), fg);
        ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    }
    resp
}

/// Single-line text, truncated with an ellipsis.
pub fn one_line(ui: &Ui, text: String, font: FontId, color: Color32, max_width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text, egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping { max_width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// An on/off switch. Returns true when toggled.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let p = pal(ui);
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(36.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), *on, label));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let track = if ui.is_enabled() { lerp(p.outline, p.accent, t) } else { p.divider };
        ui.painter().rect_filled(rect, CornerRadius::same(10), track);
        let knob_x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        ui.painter().circle_filled(egui::pos2(knob_x, rect.center().y), 7.0, if p.dark && t > 0.5 { p.on_accent } else { Color32::WHITE });
        if resp.has_focus() {
            ui.painter().rect_stroke(rect.expand(3.0), CornerRadius::same(12), Stroke::new(2.0, p.accent), StrokeKind::Outside);
        }
    }
    resp
}

/// Initials for a display name or address.
pub fn initials(name: &str) -> String {
    let base = name.split('<').next().unwrap_or(name).trim().trim_matches('"');
    let base = if base.is_empty() { name } else { base };
    let base = base.split('@').next().unwrap_or(base);
    let words: Vec<&str> = base.split(|c: char| c.is_whitespace() || c == '.' || c == '_' || c == '-').filter(|w| !w.is_empty()).collect();
    let mut s: String = words.iter().take(2).filter_map(|w| w.chars().next()).flat_map(|c| c.to_uppercase()).collect();
    if s.is_empty() {
        s = "?".into();
    }
    s
}

/// Restrained, deterministic avatar tones (no saturated rainbow).
const AVATAR_TONES: [(u32, u32); 8] = [
    (0xDCE6F2, 0x2B4A6F),
    (0xDDEBE4, 0x24543D),
    (0xEFE6D8, 0x5F4723),
    (0xE8E1EC, 0x4F3E5C),
    (0xE0E8EC, 0x34505C),
    (0xECE3DE, 0x5C4136),
    (0xE3E6EF, 0x3B4466),
    (0xE5E9DD, 0x48552F),
];

pub fn avatar(ui: &mut Ui, name: &str, diameter: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(diameter), Sense::hover());
    if ui.is_rect_visible(rect) {
        let p = pal(ui);
        let h = name.to_lowercase().bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
        let (bg, fg) = AVATAR_TONES[(h as usize) % AVATAR_TONES.len()];
        let rgb = |x: u32| Color32::from_rgb((x >> 16) as u8, (x >> 8) as u8, x as u8);
        let (bg, fg) = if p.dark { (rgb(fg), rgb(bg)) } else { (rgb(bg), rgb(fg)) };
        ui.painter().circle_filled(rect.center(), diameter / 2.0, bg);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, initials(name), theme::semibold(diameter * 0.38), fg);
    }
    resp
}

/// A navigation row: icon, label, right-aligned count, selection pill.
pub fn nav_row(ui: &mut Ui, icon: MaterialIcon, label: &str, count: usize, selected: bool, indent: f32, enabled: bool) -> Response {
    let p = pal(ui);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 32.0), if enabled { Sense::click() } else { Sense::hover() });
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::ListBoxOption, enabled, selected, label));
    if ui.is_rect_visible(rect) {
        let r = rect.shrink2(egui::vec2(0.0, 1.0));
        if selected {
            ui.painter().rect_filled(r, CornerRadius::same(6), p.selected);
            ui.painter().rect_filled(Rect::from_min_size(r.min + egui::vec2(0.0, 7.0), egui::vec2(3.0, r.height() - 14.0)), 2.0, p.accent);
        } else if enabled && (resp.hovered() || resp.has_focus()) {
            ui.painter().rect_filled(r, CornerRadius::same(6), p.hover);
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(r, CornerRadius::same(6), Stroke::new(2.0, p.accent), StrokeKind::Inside);
        }
        let fg = if !enabled {
            p.text_disabled
        } else if selected {
            p.accent
        } else {
            p.text
        };
        let icon_rect = Rect::from_min_size(egui::pos2(r.left() + 10.0 + indent, r.top()), egui::vec2(ICON_SIZE, r.height()));
        paint_icon(ui, icon_rect, icon, selected, ICON_SIZE, if selected { p.accent } else { p.text_secondary });
        let font = if selected || count > 0 { theme::semibold(ty::UI) } else { theme::regular(ty::UI) };
        let count_w = if count > 0 { 40.0 } else { 8.0 };
        let max_w = (r.right() - count_w - icon_rect.right() - 10.0).max(10.0);
        let galley = one_line(ui, label.to_string(), font, fg, max_w);
        ui.painter().galley(egui::pos2(icon_rect.right() + 10.0, r.center().y - galley.size().y / 2.0), galley, fg);
        if count > 0 {
            let txt = if count > 9999 { "9999+".to_string() } else { count.to_string() };
            ui.painter().text(
                egui::pos2(r.right() - 10.0, r.center().y),
                Align2::RIGHT_CENTER,
                txt,
                theme::semibold(ty::META),
                if selected { p.accent } else { p.text },
            );
        }
    }
    resp
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Info,
    Warning,
    Error,
}

/// An inline notice bar. `action` adds a quiet button; returns true if clicked.
pub fn notice(ui: &mut Ui, tone: Tone, text: &str, action: Option<&str>) -> bool {
    let p = pal(ui);
    let (icon, fg) = match tone {
        Tone::Info => (ic::ICON_INFO, p.accent),
        Tone::Warning => (ic::ICON_WARNING, p.warning),
        Tone::Error => (ic::ICON_ERROR, p.error),
    };
    let bg = lerp(p.surface, fg, if p.dark { 0.14 } else { 0.08 });
    let mut clicked = false;
    egui::Frame::new().fill(bg).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(glyph(icon, 18.0, fg));
            let label_w = ui.available_width() - if action.is_some() { 160.0 } else { 0.0 };
            ui.allocate_ui(egui::vec2(label_w.max(120.0), 0.0), |ui| {
                ui.add(egui::Label::new(RichText::new(text).color(p.text).size(ty::UI - 1.0)).wrap());
            });
            if let Some(a) = action {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new(a).color(p.accent).strong()).frame(false)).clicked() {
                        clicked = true;
                    }
                });
            }
        });
    });
    clicked
}

/// Centered icon + message (+ optional action) for empty, loading and error states.
pub fn empty_state(ui: &mut Ui, icon: MaterialIcon, title: &str, detail: &str, action: Option<&str>) -> bool {
    let p = pal(ui);
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.28).max(24.0));
        ui.label(glyph(icon, 40.0, p.text_disabled));
        ui.add_space(8.0);
        ui.label(RichText::new(title).font(theme::semibold(ty::UI + 1.0)).color(p.text));
        if !detail.is_empty() {
            ui.add_space(2.0);
            ui.label(RichText::new(detail).color(p.text_secondary));
        }
        if let Some(a) = action {
            ui.add_space(12.0);
            clicked = secondary_button(ui, a).clicked();
        }
    });
    clicked
}

/// A settings row: title and help on the left, control on the right.
pub fn setting_row(ui: &mut Ui, title: &str, help: &str, control: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    ui.horizontal(|ui| {
        let w = ui.available_width();
        ui.allocate_ui_with_layout(egui::vec2((w - 220.0).max(200.0), 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.label(RichText::new(title).color(p.text));
            if !help.is_empty() {
                ui.add(egui::Label::new(RichText::new(help).size(ty::META).color(p.text_secondary)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    ui.add_space(6.0);
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.divider));
    ui.add_space(10.0);
}

pub fn section_title(ui: &mut Ui, text: &str) {
    let p = pal(ui);
    ui.label(RichText::new(text).font(theme::semibold(ty::HEADING)).color(p.text));
    ui.add_space(12.0);
}

/// Text input styled for the theme (flat, 36 high).
pub fn text_field<'a>(text: &'a mut dyn egui::TextBuffer, hint: &str) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(text).hint_text(hint).margin(egui::Margin::symmetric(10, 8)).min_size(egui::vec2(0.0, 36.0))
}
