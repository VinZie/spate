//! Programmatic artwork: the window icon, toolbar icons and category glyphs.
//! Everything is drawn from simple vector shapes so no image assets ship.

use eframe::egui::{self, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, pos2, vec2};
use spate_core::Category;

use crate::theme;

// ---------------------------------------------------------------------------
// Window / taskbar icon
// ---------------------------------------------------------------------------

/// Window and tray icon; the artwork lives in `icon_art` so the build script
/// can embed the same picture in the executable.
pub fn app_icon(size: u32) -> egui::IconData {
    egui::IconData { rgba: crate::icon_art::app_icon_rgba(size), width: size, height: size }
}

// ---------------------------------------------------------------------------
// Toolbar icons
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Add,
    Batch,
    Resume,
    Pause,
    StopAll,
    Delete,
    DeleteDone,
    QueueStart,
    QueueStop,
    Options,
}

/// Paint `icon` into the square `rect`. `fg` is the text colour; accents
/// come from the theme palette.
pub fn paint_icon(p: &Painter, rect: Rect, icon: Icon, fg: Color32, enabled: bool) {
    let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.35) };
    let fg = dim(fg);
    let r = rect.shrink(rect.width() * 0.08);
    let w = r.width();
    let at = |x: f32, y: f32| pos2(r.left() + x * w, r.top() + y * w);
    let thick = (w * 0.09).max(1.5);
    match icon {
        Icon::Add => {
            p.circle_filled(r.center(), w * 0.46, dim(theme::ACCENT));
            let s = Stroke::new(thick * 1.2, Color32::WHITE.gamma_multiply(if enabled { 1.0 } else { 0.6 }));
            p.line_segment([at(0.5, 0.26), at(0.5, 0.74)], s);
            p.line_segment([at(0.26, 0.5), at(0.74, 0.5)], s);
        }
        Icon::Batch => {
            let sheet = |dx: f32, fill: Color32| {
                let rr = Rect::from_min_max(at(0.12 + dx, 0.10 + dx), at(0.68 + dx, 0.80 + dx));
                p.rect(rr, CornerRadius::same(2), fill, Stroke::new(thick * 0.7, fg), StrokeKind::Inside);
            };
            sheet(0.0, Color32::TRANSPARENT);
            sheet(0.12, dim(theme::panel_fill_hint()));
            p.circle_filled(at(0.78, 0.78), w * 0.2, dim(theme::ACCENT));
            let s = Stroke::new(thick * 0.8, Color32::WHITE);
            p.line_segment([at(0.78, 0.68), at(0.78, 0.88)], s);
            p.line_segment([at(0.68, 0.78), at(0.88, 0.78)], s);
        }
        Icon::Resume => {
            p.add(Shape::convex_polygon(vec![at(0.24, 0.12), at(0.84, 0.5), at(0.24, 0.88)], dim(theme::GOOD), Stroke::NONE));
        }
        Icon::Pause => {
            let bar = |x0: f32| Rect::from_min_max(at(x0, 0.14), at(x0 + 0.22, 0.86));
            p.rect_filled(bar(0.2), CornerRadius::same(2), dim(theme::WARN));
            p.rect_filled(bar(0.58), CornerRadius::same(2), dim(theme::WARN));
        }
        Icon::StopAll => {
            let back = Rect::from_min_max(at(0.30, 0.08), at(0.92, 0.70));
            p.rect(back, CornerRadius::same(3), Color32::TRANSPARENT, Stroke::new(thick * 0.8, dim(theme::BAD)), StrokeKind::Inside);
            let front = Rect::from_min_max(at(0.08, 0.30), at(0.70, 0.92));
            p.rect_filled(front, CornerRadius::same(3), dim(theme::BAD));
        }
        Icon::Delete | Icon::DeleteDone => {
            let s = Stroke::new(thick * 0.8, fg);
            p.line_segment([at(0.14, 0.22), at(0.86, 0.22)], s);
            p.line_segment([at(0.40, 0.10), at(0.60, 0.10)], s);
            let body = vec![at(0.22, 0.28), at(0.78, 0.28), at(0.72, 0.92), at(0.28, 0.92)];
            p.add(Shape::closed_line(body, s));
            for x in [0.40, 0.60] {
                p.line_segment([at(x, 0.40), at(x, 0.80)], Stroke::new(thick * 0.6, fg));
            }
            if icon == Icon::DeleteDone {
                p.circle_filled(at(0.78, 0.76), w * 0.2, dim(theme::GOOD));
                let c = Stroke::new(thick * 0.8, Color32::WHITE);
                p.line_segment([at(0.69, 0.77), at(0.76, 0.84)], c);
                p.line_segment([at(0.76, 0.84), at(0.88, 0.68)], c);
            }
        }
        Icon::QueueStart | Icon::QueueStop => {
            let s = Stroke::new(thick * 0.9, fg);
            for (i, y) in [0.2, 0.42, 0.64].into_iter().enumerate() {
                let x1 = if i == 2 { 0.48 } else { 0.86 };
                p.line_segment([at(0.08, y), at(x1, y)], s);
            }
            if icon == Icon::QueueStart {
                p.add(Shape::convex_polygon(vec![at(0.58, 0.56), at(0.94, 0.76), at(0.58, 0.96)], dim(theme::GOOD), Stroke::NONE));
            } else {
                p.rect_filled(Rect::from_min_max(at(0.60, 0.60), at(0.92, 0.92)), CornerRadius::same(2), dim(theme::BAD));
            }
        }
        Icon::Options => {
            let c = r.center();
            let teeth = 8;
            let mut pts = Vec::with_capacity(teeth * 4);
            for i in 0..teeth {
                let a0 = (i as f32) / teeth as f32 * std::f32::consts::TAU;
                let step = std::f32::consts::TAU / teeth as f32;
                for (frac, rad) in [(0.0, 0.34), (0.12, 0.47), (0.38, 0.47), (0.5, 0.34)] {
                    let a = a0 + frac * step;
                    pts.push(c + vec2(a.cos(), a.sin()) * w * rad);
                }
            }
            p.add(Shape::closed_line(pts, Stroke::new(thick * 0.8, fg)));
            p.circle_stroke(c, w * 0.14, Stroke::new(thick * 0.8, fg));
        }
    }
}

// ---------------------------------------------------------------------------
// Category glyphs
// ---------------------------------------------------------------------------

pub fn category_color(c: Category) -> Color32 {
    match c {
        Category::Compressed => Color32::from_rgb(142, 99, 206),
        Category::Documents => Color32::from_rgb(52, 132, 228),
        Category::Music => Color32::from_rgb(222, 84, 146),
        Category::Programs => Color32::from_rgb(92, 112, 138),
        Category::Video => Color32::from_rgb(232, 112, 48),
        Category::Other => Color32::from_rgb(130, 136, 144),
    }
}

/// A small coloured tile with a white pictogram for `cat`.
pub fn paint_category(p: &Painter, rect: Rect, cat: Category) {
    let r = rect;
    p.rect_filled(r, CornerRadius::same((r.width() * 0.22) as u8), category_color(cat));
    let w = r.width();
    let at = |x: f32, y: f32| pos2(r.left() + x * w, r.top() + y * w);
    let s = Stroke::new((w * 0.09).max(1.0), Color32::WHITE);
    match cat {
        Category::Compressed => {
            for (i, y) in [0.2, 0.36, 0.52].into_iter().enumerate() {
                let x = if i % 2 == 0 { 0.42 } else { 0.58 };
                p.line_segment([at(x - 0.08, y), at(x + 0.08, y)], s);
            }
            p.rect_filled(Rect::from_min_max(at(0.38, 0.62), at(0.62, 0.82)), CornerRadius::same(1), Color32::WHITE);
        }
        Category::Documents => {
            for (y, x1) in [(0.3, 0.75), (0.5, 0.75), (0.7, 0.58)] {
                p.line_segment([at(0.25, y), at(x1, y)], s);
            }
        }
        Category::Music => {
            p.circle_filled(at(0.4, 0.7), w * 0.13, Color32::WHITE);
            p.line_segment([at(0.51, 0.7), at(0.51, 0.22)], s);
            p.line_segment([at(0.51, 0.22), at(0.72, 0.32)], s);
        }
        Category::Programs => {
            let win = Rect::from_min_max(at(0.2, 0.25), at(0.8, 0.75));
            p.rect_stroke(win, CornerRadius::same(1), s, StrokeKind::Inside);
            p.line_segment([at(0.2, 0.38), at(0.8, 0.38)], s);
        }
        Category::Video => {
            p.add(Shape::convex_polygon(vec![at(0.36, 0.26), at(0.74, 0.5), at(0.36, 0.74)], Color32::WHITE, Stroke::NONE));
        }
        Category::Other => {
            let pts = vec![at(0.28, 0.2), at(0.58, 0.2), at(0.72, 0.34), at(0.72, 0.8), at(0.28, 0.8)];
            p.add(Shape::closed_line(pts, s));
        }
    }
}

/// Allocate and paint a category glyph inline with text.
pub fn category_glyph(ui: &mut egui::Ui, cat: Category, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_category(ui.painter(), rect, cat);
    }
    resp
}

/// Small filled circle used as a status/queue badge.
pub fn dot(p: &Painter, center: Pos2, radius: f32, color: Color32) {
    p.circle_filled(center, radius, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_has_transparent_corners_and_white_glyph() {
        let icon = app_icon(64);
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
        assert_eq!(icon.rgba[3], 0, "top-left corner is transparent");
        // Centre of the arrow head is white and opaque.
        let o = ((40 * 64) + 32) * 4;
        assert!(icon.rgba[o] > 240 && icon.rgba[o + 1] > 240 && icon.rgba[o + 3] == 255);
    }
}
