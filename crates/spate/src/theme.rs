//! Colours and style tweaks for both light and dark mode (follows the OS).

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Theme, vec2};
use serde::{Deserialize, Serialize};

pub const ACCENT: Color32 = Color32::from_rgb(43, 125, 233);
pub const GOOD: Color32 = Color32::from_rgb(46, 160, 67);
pub const WARN: Color32 = Color32::from_rgb(214, 152, 32);
pub const BAD: Color32 = Color32::from_rgb(222, 72, 64);
pub const MUTED: Color32 = Color32::from_rgb(128, 134, 142);

/// Neutral fill used inside icons (readable on either background).
pub fn panel_fill_hint() -> Color32 {
    Color32::from_rgba_unmultiplied(128, 128, 128, 60)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Follow system",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        ctx.set_theme(match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        });
    }
}

pub fn install(ctx: &egui::Context) {
    for theme in [Theme::Dark, Theme::Light] {
        ctx.style_mut_of(theme, |s| {
            s.spacing.item_spacing = vec2(8.0, 6.0);
            s.spacing.button_padding = vec2(10.0, 4.0);
            s.spacing.interact_size.y = 24.0;
            s.spacing.menu_margin = egui::Margin::same(6);
            s.spacing.window_margin = egui::Margin::same(14);
            s.text_styles.insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Button, FontId::new(14.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Small, FontId::new(11.5, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional));
            s.text_styles.insert(TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace));

            let v = &mut s.visuals;
            let dark = theme == Theme::Dark;
            v.selection.bg_fill = if dark { ACCENT.gamma_multiply(0.55) } else { ACCENT.gamma_multiply(0.30) };
            v.selection.stroke = Stroke::new(1.0, if dark { Color32::WHITE } else { Color32::from_rgb(10, 50, 110) });
            v.hyperlink_color = if dark { Color32::from_rgb(110, 170, 255) } else { Color32::from_rgb(20, 90, 200) };
            v.window_corner_radius = CornerRadius::same(10);
            v.menu_corner_radius = CornerRadius::same(8);
            for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
                w.corner_radius = CornerRadius::same(6);
            }
            v.widgets.noninteractive.corner_radius = CornerRadius::same(6);
            if dark {
                v.panel_fill = Color32::from_rgb(30, 32, 36);
                v.window_fill = Color32::from_rgb(36, 38, 43);
                v.faint_bg_color = Color32::from_rgb(36, 39, 44);
                v.extreme_bg_color = Color32::from_rgb(22, 23, 26);
            } else {
                v.panel_fill = Color32::from_rgb(246, 247, 249);
                v.window_fill = Color32::from_rgb(252, 252, 253);
                v.faint_bg_color = Color32::from_rgb(238, 241, 245);
                v.extreme_bg_color = Color32::from_rgb(255, 255, 255);
            }
        });
    }
}

/// Slightly different background for side panels / toolbars.
pub fn sidebar_fill(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(26, 27, 31) } else { Color32::from_rgb(236, 239, 243) }
}

pub fn toolbar_fill(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(34, 36, 41) } else { Color32::from_rgb(251, 251, 252) }
}

pub fn track_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(52, 55, 62) } else { Color32::from_rgb(221, 225, 231) }
}

pub fn status_color(status: spate_core::Status, ui: &egui::Ui) -> Color32 {
    use spate_core::Status::*;
    match status {
        Connecting | Downloading | Finishing => {
            if ui.visuals().dark_mode { Color32::from_rgb(110, 170, 255) } else { ACCENT }
        }
        Completed => GOOD,
        Failed => BAD,
        Queued => WARN,
        Paused => ui.visuals().weak_text_color(),
    }
}
