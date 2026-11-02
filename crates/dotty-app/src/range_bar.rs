//! A timeline bar with a kept range, shared by the video editor, the GIF/frame picker and the
//! sound timeline: two diamond handles (easy to grab, like the audio editor's fade grips),
//! the range between them, a playhead, and room behind for a waveform.

use eframe::egui;

/// Height of the whole bar, diamonds included.
pub(crate) const HEIGHT: f32 = 40.0;
/// Half-size of a drawn diamond, and how close (points) a press must be to its centre to
/// grab it: much larger than the diamond, since the line itself is thin.
const DIAMOND: f32 = 8.0;
const GRAB_RADIUS: f32 = 14.0;
/// How close to a handle's line a press grabs it.
const LINE_GRAB: f32 = 7.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Handle {
    Start,
    End,
}

/// Where a diamond sits: centred on the top edge, so the track below stays clear.
fn diamond_centre(bar: egui::Rect, x: f32) -> egui::Pos2 {
    egui::pos2(x, bar.top() + DIAMOND + 1.0)
}

/// The handle a press at `pos` grabs: a diamond (generous radius) or the line below it.
/// When both qualify (the range is tiny), the nearer side wins.
pub(crate) fn handle_at(pos: egui::Pos2, bar: egui::Rect, x_start: f32, x_end: f32) -> Option<Handle> {
    let hits = |x: f32| pos.distance(diamond_centre(bar, x)) <= GRAB_RADIUS || ((pos.x - x).abs() <= LINE_GRAB && pos.y >= bar.top() && pos.y <= bar.bottom());
    match (hits(x_start), hits(x_end)) {
        (true, true) => Some(if pos.x < (x_start + x_end) / 2.0 { Handle::Start } else { Handle::End }),
        (true, false) => Some(Handle::Start),
        (false, true) => Some(Handle::End),
        _ => None,
    }
}

pub(crate) struct Output {
    /// The kept range (possibly just changed).
    pub range: (f64, f64),
    /// A new playhead position, when the track was pressed or dragged away from the handles.
    pub playhead: Option<f64>,
}

/// Draw the bar `width` points wide over `0..duration` seconds and handle presses.
/// With `range` `None` there are no handles, only the playhead (picking a single frame).
/// `grab` remembers which handle a drag holds; `min_len` is the shortest range allowed.
/// `background` paints behind the range and handles (a waveform, say) inside the track.
#[allow(clippy::too_many_arguments)]
pub(crate) fn show(
    ui: &mut egui::Ui,
    width: f32,
    duration: f64,
    range: Option<(f64, f64)>,
    playhead: Option<f64>,
    min_len: f64,
    grab: &mut Option<Handle>,
    background: impl FnOnce(&egui::Painter, egui::Rect),
) -> Output {
    let duration = duration.max(1e-6);
    let (bar, resp) = ui.allocate_exact_size(egui::vec2(width.max(40.0), HEIGHT), egui::Sense::click_and_drag());
    let x = |t: f64| bar.left() + (t / duration) as f32 * bar.width();
    let time = |px: f32| (((px - bar.left()) / bar.width()) as f64 * duration).clamp(0.0, duration);
    let handles = range.is_some();
    let (mut a, mut b) = range.unwrap_or((0.0, duration));
    let mut moved = None;

    if let Some(pos) = resp.interact_pointer_pos() {
        if handles && (resp.drag_started() || resp.clicked()) {
            *grab = handle_at(pos, bar, x(a), x(b));
        }
        let t = time(pos.x);
        match *grab {
            Some(Handle::Start) => a = t.min(b - min_len).max(0.0),
            Some(Handle::End) => b = t.max(a + min_len).min(duration),
            None => moved = Some(t),
        }
    }
    if resp.drag_stopped() || resp.clicked() {
        *grab = None;
    }
    if let Some(pos) = resp.hover_pos()
        && handles
        && (grab.is_some() || handle_at(pos, bar, x(a), x(b)).is_some())
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    let top = if handles { 2.0 * DIAMOND + 4.0 } else { HEIGHT * 0.3 };
    let track = egui::Rect::from_min_max(egui::pos2(bar.left(), bar.top() + top), bar.max);
    let painter = ui.painter_at(bar.expand(2.0));
    painter.rect_filled(track, 3.0, ui.visuals().extreme_bg_color);
    background(&painter, track);
    let accent = ui.visuals().selection.stroke.color;
    if handles {
        // The part kept, and the parts cut dimmed.
        painter.rect_filled(egui::Rect::from_x_y_ranges(x(a)..=x(b), track.y_range()), 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.45));
        let dim = egui::Color32::from_black_alpha(90);
        painter.rect_filled(egui::Rect::from_x_y_ranges(bar.left()..=x(a), track.y_range()), 0.0, dim);
        painter.rect_filled(egui::Rect::from_x_y_ranges(x(b)..=bar.right(), track.y_range()), 0.0, dim);
    }
    if let Some(p) = playhead {
        let px = x(p.clamp(0.0, duration));
        painter.line_segment([egui::pos2(px, track.top()), egui::pos2(px, track.bottom())], egui::Stroke::new(2.0, ui.visuals().strong_text_color()));
    }
    for (h, t) in [(Handle::Start, a), (Handle::End, b)].into_iter().filter(|_| handles) {
        let hx = x(t);
        painter.line_segment([egui::pos2(hx, bar.top()), egui::pos2(hx, bar.bottom())], egui::Stroke::new(2.0, accent));
        let c = diamond_centre(bar, hx);
        let held = *grab == Some(h);
        let r = if held { DIAMOND + 2.0 } else { DIAMOND };
        painter.add(egui::Shape::convex_polygon(
            vec![c + egui::vec2(0.0, -r), c + egui::vec2(r, 0.0), c + egui::vec2(0.0, r), c + egui::vec2(-r, 0.0)],
            if held { ui.visuals().strong_text_color() } else { accent },
            egui::Stroke::new(1.0, ui.visuals().extreme_bg_color),
        ));
    }
    Output { range: (a, b), playhead: moved }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, HEIGHT))
    }

    #[test]
    fn diamonds_are_easy_to_grab_and_the_nearer_side_wins_when_they_overlap() {
        let b = bar();
        let top = b.top() + DIAMOND + 1.0;
        // On a diamond, and well off to its side: still grabbed.
        assert_eq!(handle_at(egui::pos2(200.0, top), b, 200.0, 400.0), Some(Handle::Start));
        assert_eq!(handle_at(egui::pos2(212.0, top + 4.0), b, 200.0, 400.0), Some(Handle::Start));
        assert_eq!(handle_at(egui::pos2(391.0, top - 5.0), b, 200.0, 400.0), Some(Handle::End));
        // The line below the diamond works too, but not far from it.
        assert_eq!(handle_at(egui::pos2(203.0, b.bottom() - 2.0), b, 200.0, 400.0), Some(Handle::Start));
        assert_eq!(handle_at(egui::pos2(260.0, b.bottom() - 2.0), b, 200.0, 400.0), None);
        // A press in the middle of the track moves the playhead instead.
        assert_eq!(handle_at(egui::pos2(300.0, b.center().y), b, 200.0, 400.0), None);
        // Both at the same place: which side of it you press decides.
        assert_eq!(handle_at(egui::pos2(296.0, top), b, 300.0, 300.0), Some(Handle::Start));
        assert_eq!(handle_at(egui::pos2(304.0, top), b, 300.0, 300.0), Some(Handle::End));
        // Far away: nothing.
        assert_eq!(handle_at(egui::pos2(300.0, top), b, 200.0, 400.0), None);
    }
}
