//! The segment map: a bar showing which byte ranges each connection owns,
//! how much of each has arrived, and where the active connections are.

use eframe::egui::{self, Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use spate_core::segments::SegmentView;
use spate_core::{ItemView, Status};

use crate::{format, theme};

const UNKNOWN_END: u64 = u64::MAX;

/// Known total size of a download for drawing purposes.
fn extent(item: &ItemView, segs: &[SegmentView]) -> Option<u64> {
    item.total.filter(|&t| t > 0).or_else(|| {
        let max = segs.iter().map(|s| s.end).filter(|&e| e != UNKNOWN_END).max()?;
        (max > 0).then_some(max)
    })
}

/// Horizontal position (0..=1) of byte offset `b` within `total`.
pub fn frac(b: u64, total: u64) -> f32 {
    if total == 0 { 0.0 } else { (b.min(total) as f64 / total as f64) as f32 }
}

pub fn segment_map(ui: &mut egui::Ui, item: &ItemView, segs: &[SegmentView], height: f32) -> egui::Response {
    let width = ui.available_width().max(60.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter_at(rect.expand(1.0));
    let track = theme::track_color(ui);
    let radius = CornerRadius::same(4);
    p.rect_filled(rect, radius, track);

    let dark = ui.visuals().dark_mode;
    let done_color = if item.status == Status::Completed { theme::GOOD } else { theme::ACCENT.gamma_multiply(if dark { 0.75 } else { 0.85 }) };
    let live_color = if dark { Color32::from_rgb(92, 166, 255) } else { Color32::from_rgb(30, 110, 230) };
    let claimed_tint = theme::ACCENT.gamma_multiply(0.18);
    let head_color = if dark { Color32::WHITE } else { Color32::from_rgb(8, 40, 100) };

    let Some(total) = extent(item, segs) else {
        // Unknown size: a moving stripe while active, otherwise empty.
        if item.status.is_active() {
            let t = ui.input(|i| i.time) as f32;
            let w = rect.width() * 0.2;
            let x = rect.left() + ((t * 0.5).fract() * (rect.width() + w)) - w;
            let r = Rect::from_min_max(pos2(x.max(rect.left()), rect.top()), pos2((x + w).min(rect.right()), rect.bottom()));
            p.rect_filled(r, radius, live_color);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
        label_center(&p, rect, &format::size(item.downloaded), ui);
        return resp;
    };

    let x_of = |b: u64| rect.left() + frac(b, total) * rect.width();

    if segs.is_empty() {
        // No live map (paused, queued or finished): show overall progress.
        let f = if item.status == Status::Completed { 1.0 } else { frac(item.downloaded, total) };
        if f > 0.0 {
            let r = Rect::from_min_max(rect.min, pos2(rect.left() + f * rect.width(), rect.bottom()));
            p.rect_filled(r, radius, done_color);
        }
    } else {
        for s in segs {
            let end = if s.end == UNKNOWN_END { total } else { s.end.min(total) };
            let x0 = x_of(s.start);
            let x1 = x_of(end);
            let xr = x_of(s.start.saturating_add(s.received).min(end));
            if s.active && x1 > xr {
                p.rect_filled(Rect::from_min_max(pos2(xr, rect.top()), pos2(x1, rect.bottom())), 0.0, claimed_tint);
            }
            if xr > x0 {
                let c = if s.active { live_color } else { done_color };
                p.rect_filled(Rect::from_min_max(pos2(x0, rect.top()), pos2(xr, rect.bottom())), 0.0, c);
            }
            if s.active && s.received < end - s.start {
                // Connection head: where this connection is writing right now.
                p.line_segment([pos2(xr, rect.top() + 1.0), pos2(xr, rect.bottom() - 1.0)], Stroke::new(2.0, head_color));
            }
            if s.start > 0 {
                p.line_segment(
                    [pos2(x0, rect.top()), pos2(x0, rect.bottom())],
                    Stroke::new(1.0, ui.visuals().panel_fill.gamma_multiply(0.9)),
                );
            }
        }
    }
    p.rect_stroke(rect, radius, Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color), StrokeKind::Inside);

    if let Some(pos) = resp.hover_pos() {
        let byte = (((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * total as f64) as u64;
        let hit = segs.iter().enumerate().find(|(_, s)| byte >= s.start && byte < s.end.min(total).max(s.start + 1));
        let text = match hit {
            Some((i, s)) => {
                let len = s.end.min(total) - s.start;
                let mut t = format!(
                    "Part {} · {} – {}\n{} received ({})",
                    i + 1,
                    format::size(s.start),
                    format::size(s.end.min(total)),
                    format::progress_of(s.received.min(len), Some(len)),
                    format::percent(frac(s.received, len.max(1))),
                );
                if s.active {
                    t.push_str(&format!("\nActive connection · {}", format::speed(s.speed)));
                }
                t
            }
            None => format!("Offset {}", format::size(byte)),
        };
        resp.clone().on_hover_text_at_pointer(text);
    }
    resp
}

fn label_center(p: &egui::Painter, rect: Rect, text: &str, ui: &egui::Ui) {
    p.text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::TextStyle::Small.resolve(ui.style()), ui.visuals().strong_text_color());
}

/// Thin progress bar with the percentage drawn on top (table cell).
pub fn progress_cell(ui: &mut egui::Ui, item: &ItemView) {
    let h = 16.0;
    let w = ui.available_width().max(40.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    let radius = CornerRadius::same(4);
    p.rect_filled(rect, radius, theme::track_color(ui));
    let (f, label) = match (item.status, item.fraction()) {
        (Status::Completed, _) => (1.0, "100%".to_string()),
        (_, Some(f)) => (f, format::percent(f)),
        (_, None) => (0.0, if item.downloaded > 0 { format::size(item.downloaded) } else { String::new() }),
    };
    if f > 0.0 {
        let color = match item.status {
            Status::Completed => theme::GOOD,
            Status::Failed => theme::BAD,
            Status::Paused | Status::Queued => theme::MUTED,
            _ => theme::ACCENT,
        };
        let fill = Rect::from_min_max(rect.min, pos2(rect.left() + rect.width() * f, rect.bottom()));
        p.rect_filled(fill, radius, color);
    }
    if !label.is_empty() {
        let font = egui::TextStyle::Small.resolve(ui.style());
        let text_color = if f > 0.5 { Color32::WHITE } else { ui.visuals().text_color() };
        p.text(rect.center(), egui::Align2::CENTER_CENTER, label, font, text_color);
    }
}

/// Per-connection breakdown under the map.
pub fn connections_grid(ui: &mut egui::Ui, segs: &[SegmentView], total: Option<u64>) {
    egui::Grid::new("connections_grid").striped(true).num_columns(5).spacing(vec2(16.0, 4.0)).show(ui, |ui| {
        for h in ["Part", "Range", "Received", "Speed", "State"] {
            ui.label(egui::RichText::new(h).small().strong());
        }
        ui.end_row();
        for (i, s) in segs.iter().enumerate() {
            let end = if s.end == UNKNOWN_END { None } else { Some(s.end) };
            let len = end.map(|e| e.saturating_sub(s.start));
            ui.label(format!("{}", i + 1));
            ui.label(match end {
                Some(e) => format!("{} – {}", format::size(s.start), format::size(e.min(total.unwrap_or(e)))),
                None => format!("{} – end", format::size(s.start)),
            });
            ui.label(format::progress_of(len.map_or(s.received, |l| s.received.min(l)), len));
            ui.label(if s.active { format::speed(s.speed) } else { String::new() });
            let complete = len.is_some_and(|l| s.received >= l);
            let (state, color) = if complete {
                ("done", theme::GOOD)
            } else if s.active {
                ("receiving", theme::ACCENT)
            } else {
                ("waiting", theme::MUTED)
            };
            ui.colored_label(color, state);
            ui.end_row();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractions_are_clamped() {
        assert_eq!(frac(0, 100), 0.0);
        assert_eq!(frac(50, 100), 0.5);
        assert_eq!(frac(500, 100), 1.0);
        assert_eq!(frac(5, 0), 0.0);
    }
}
