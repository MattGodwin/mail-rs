//! Visual design tokens and egui styling: slate neutrals with one ink-blue
//! accent, in light and dark variants. Every colour the UI paints comes from
//! a `Palette`, so both themes stay consistent.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, Theme};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub window: Color32,
    pub nav: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub input: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_disabled: Color32,
    pub divider: Color32,
    pub outline: Color32,
    pub hover: Color32,
    pub selected: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
    /// Accent-tinted fill for subtle highlights (info banners, focus halos).
    pub accent_subtle: Color32,
    /// Inverse surface for toasts.
    pub inverse: Color32,
    pub on_inverse: Color32,
    pub dark: bool,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const LIGHT: Palette = Palette {
    window: rgb(0xF3F5F7),
    nav: rgb(0xF3F5F7),
    surface: rgb(0xFFFFFF),
    raised: rgb(0xFFFFFF),
    input: rgb(0xF8FAFC),
    text: rgb(0x182230),
    text_secondary: rgb(0x526174),
    text_disabled: rgb(0x788494),
    divider: rgb(0xDFE5EC),
    outline: rgb(0x7C8999),
    hover: rgb(0xEDF1F5),
    selected: rgb(0xE7EFFB),
    accent: rgb(0x245EA8),
    on_accent: rgb(0xFFFFFF),
    success: rgb(0x18704A),
    warning: rgb(0x8A5100),
    error: rgb(0xB42332),
    accent_subtle: rgb(0xEEF4FC),
    inverse: rgb(0x1F2833),
    on_inverse: rgb(0xEDF2F7),
    dark: false,
};

pub const DARK: Palette = Palette {
    window: rgb(0x111418),
    nav: rgb(0x171C22),
    surface: rgb(0x1B2129),
    raised: rgb(0x242C36),
    input: rgb(0x141A21),
    text: rgb(0xEDF2F7),
    text_secondary: rgb(0xACB8C8),
    text_disabled: rgb(0x758294),
    divider: rgb(0x333D49),
    outline: rgb(0x718096),
    hover: rgb(0x293340),
    selected: rgb(0x243B59),
    accent: rgb(0x8AB8FA),
    on_accent: rgb(0x10243F),
    success: rgb(0x77D4A2),
    warning: rgb(0xF1C16D),
    error: rgb(0xFF9CA5),
    accent_subtle: rgb(0x1C2C42),
    inverse: rgb(0xEDF2F7),
    on_inverse: rgb(0x182230),
    dark: true,
};

pub fn palette(ctx: &egui::Context) -> Palette {
    match ctx.theme() {
        Theme::Dark => DARK,
        Theme::Light => LIGHT,
    }
}

/// Typography (logical points).
pub mod ty {
    pub const SUBJECT: f32 = 22.0;
    pub const HEADING: f32 = 18.0;
    pub const UI: f32 = 14.0;
    pub const BODY: f32 = 15.0;
    pub const META: f32 = 12.0;
    pub const MONO: f32 = 13.0;
}

/// The semibold UI face (falls back to regular when unavailable).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.window;
    v.window_fill = p.raised;
    v.window_stroke = Stroke::new(1.0, p.divider);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    let shadow_alpha = if p.dark { 102 } else { 51 };
    v.window_shadow = Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(shadow_alpha) };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(if p.dark { 82 } else { 36 }) };
    v.window_highlight_topmost = false;
    v.extreme_bg_color = p.input;
    v.text_edit_bg_color = Some(p.input);
    v.faint_bg_color = p.hover;
    v.code_bg_color = p.hover;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.error;
    v.override_text_color = None;
    v.weak_text_color = Some(p.text_secondary);
    v.selection.bg_fill = if p.dark { rgb(0x2D4A70) } else { rgb(0xC9DCF5) };
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;
    v.button_frame = true;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let r = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.divider);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    // Resting controls are quiet: no fill until hovered, a hairline where needed.
    w.inactive.bg_fill = p.input;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.selected;
    w.active.weak_bg_fill = p.selected;
    w.active.bg_stroke = Stroke::NONE;
    w.active.fg_stroke = Stroke::new(1.5, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open.bg_fill = p.hover;
    w.open.weak_bg_fill = p.hover;
    w.open.bg_stroke = Stroke::NONE;
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;
    v
}

fn style(p: &Palette, style: &mut egui::Style) {
    use egui::TextStyle::*;
    style.visuals = visuals(p);
    style.text_styles.insert(Body, regular(ty::UI));
    style.text_styles.insert(Button, regular(ty::UI));
    style.text_styles.insert(Heading, semibold(ty::HEADING));
    style.text_styles.insert(Small, regular(ty::META));
    style.text_styles.insert(Monospace, FontId::monospace(ty::MONO));
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(8.0, 8.0);
    s.button_padding = egui::vec2(10.0, 6.0);
    s.interact_size.y = 32.0;
    s.window_margin = egui::Margin::same(16);
    s.menu_margin = egui::Margin::same(6);
    s.indent = 16.0;
    s.icon_width = 16.0;
    s.icon_spacing = 8.0;
    s.combo_height = 320.0;
    s.scroll.bar_width = 8.0;
    s.scroll.floating = true;
    style.interaction.selectable_labels = true;
}

/// Fonts: Segoe UI (+ Semibold) on Windows, with the Material Symbols icon
/// font registered as a fallback so icons render inline with text.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    #[allow(unused_mut)] // only assigned on Windows
    let mut semibold_face = None;
    #[cfg(windows)]
    {
        use std::sync::Arc;
        let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
        let load = |f: &str| std::fs::read(dir.join(f)).ok();
        if let Some(b) = load("segoeui.ttf") {
            fonts.font_data.insert("ui".into(), Arc::new(egui::FontData::from_owned(b)));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        }
        if let Some(b) = load("seguisb.ttf").or_else(|| load("segoeuib.ttf")) {
            fonts.font_data.insert("ui-semibold".into(), Arc::new(egui::FontData::from_owned(b)));
            semibold_face = Some("ui-semibold".to_string());
        }
        if let Some(b) = load("seguisym.ttf") {
            fonts.font_data.insert("sym".into(), Arc::new(egui::FontData::from_owned(b)));
            fonts.families.entry(FontFamily::Proportional).or_default().push("sym".into());
        }
        if let Some(b) = load("consola.ttf") {
            fonts.font_data.insert("mono".into(), Arc::new(egui::FontData::from_owned(b)));
            fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
        }
    }
    let mut semi = Vec::new();
    if let Some(f) = semibold_face {
        semi.push(f);
    }
    semi.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(FontFamily::Name("semibold".into()), semi);
    ctx.set_fonts(fonts);
    egui_material_icons::initialize(ctx);
}

pub fn install(ctx: &egui::Context, preference: &str) {
    ctx.style_mut_of(Theme::Light, |s| style(&LIGHT, s));
    ctx.style_mut_of(Theme::Dark, |s| style(&DARK, s));
    set_preference(ctx, preference);
}

pub fn set_preference(ctx: &egui::Context, preference: &str) {
    ctx.set_theme(match preference {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    });
}
