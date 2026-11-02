//! The crop box shared by the image editor and the video editor: its aspect choices, what a
//! drag on it does (draw a new box, move it, pull an edge or corner), and how it's painted.
//! Rects are in picture pixels, except where a function says "screen".

use eframe::egui;

/// How close (in points) the pointer must be to grab a crop edge or corner.
pub(crate) const GRAB: f32 = 8.0;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Aspect {
    Free,
    Original,
    Ratio(u8, u8),
}

pub(crate) const ASPECTS: [Aspect; 9] = [
    Aspect::Free,
    Aspect::Original,
    Aspect::Ratio(1, 1),
    Aspect::Ratio(4, 3),
    Aspect::Ratio(3, 4),
    Aspect::Ratio(3, 2),
    Aspect::Ratio(2, 3),
    Aspect::Ratio(16, 9),
    Aspect::Ratio(9, 16),
];

impl Aspect {
    pub(crate) fn label(self) -> String {
        match self {
            Aspect::Free => "Free".into(),
            Aspect::Original => "Original".into(),
            Aspect::Ratio(w, h) => format!("{w}:{h}"),
        }
    }

    /// Width over height, if fixed.
    pub(crate) fn ratio(self, size: egui::Vec2) -> Option<f32> {
        match self {
            Aspect::Free => None,
            Aspect::Original => Some(size.x / size.y),
            Aspect::Ratio(w, h) => Some(w as f32 / h as f32),
        }
    }
}

/// What a drag on the crop box is doing. Rects are in image pixels.
#[derive(Clone, Copy)]
pub(crate) enum Grab {
    /// Drawing a new box from this corner.
    New(egui::Pos2),
    Move { start: egui::Rect, from: egui::Pos2 },
    /// Moving edges: -1 = left/top, 1 = right/bottom, 0 = that axis stays.
    Edges { start: egui::Rect, dx: i8, dy: i8 },
}

pub(crate) fn clamp(p: egui::Pos2, bounds: egui::Rect) -> egui::Pos2 {
    p.clamp(bounds.min, bounds.max)
}

/// Round a box to whole pixels inside `bounds`.
pub(crate) fn snap(r: egui::Rect, bounds: egui::Rect) -> egui::Rect {
    let r = egui::Rect::from_min_max(r.min.round(), r.max.round());
    r.intersect(bounds)
}

/// The box with one corner at `anchor` and the other towards `p`, kept to `ratio` (width
/// over height) and inside `bounds` when given.
pub(crate) fn box_from(anchor: egui::Pos2, p: egui::Pos2, ratio: Option<f32>, bounds: egui::Rect) -> egui::Rect {
    let d = p - anchor;
    let Some(ratio) = ratio else { return egui::Rect::from_two_pos(anchor, p) };
    let (w, h) = (d.x.abs(), d.y.abs());
    // Follow whichever direction the pointer went further in.
    let (mut w, mut h) = if w / ratio >= h { (w, w / ratio) } else { (h * ratio, h) };
    let room_x = if d.x >= 0.0 { bounds.max.x - anchor.x } else { anchor.x - bounds.min.x };
    let room_y = if d.y >= 0.0 { bounds.max.y - anchor.y } else { anchor.y - bounds.min.y };
    if w > 0.0 && h > 0.0 {
        let k = (room_x / w).min(room_y / h).min(1.0);
        (w, h) = (w * k, h * k);
    }
    egui::Rect::from_two_pos(anchor, anchor + egui::vec2(w * d.x.signum(), h * d.y.signum()))
}

/// Which part of the crop box (on screen) `pos` grabs: `(dx, dy, inside)`, where dx/dy say
/// which edges move (-1 left/top, 1 right/bottom). Locked shapes only offer corners.
pub(crate) fn hit(r: egui::Rect, pos: egui::Pos2, locked: bool) -> (i8, i8, bool) {
    let near = |a: f32, b: f32| (a - b).abs() <= GRAB;
    let within_x = pos.x >= r.min.x - GRAB && pos.x <= r.max.x + GRAB;
    let within_y = pos.y >= r.min.y - GRAB && pos.y <= r.max.y + GRAB;
    let dx = if !within_y {
        0
    } else if near(pos.x, r.min.x) {
        -1
    } else if near(pos.x, r.max.x) {
        1
    } else {
        0
    };
    let dy = if !within_x {
        0
    } else if near(pos.y, r.min.y) {
        -1
    } else if near(pos.y, r.max.y) {
        1
    } else {
        0
    };
    let inside = r.contains(pos);
    if locked && (dx == 0 || dy == 0) {
        return (0, 0, inside);
    }
    (dx, dy, inside)
}

pub(crate) fn grab_cursor((dx, dy, inside): (i8, i8, bool)) -> egui::CursorIcon {
    use egui::CursorIcon::*;
    match (dx, dy) {
        (0, 0) if inside => Grab,
        (0, 0) => Crosshair,
        (0, _) => ResizeVertical,
        (_, 0) => ResizeHorizontal,
        (a, b) if a == b => ResizeNwSe,
        _ => ResizeNeSw,
    }
}

/// Darken everything outside the crop box and draw its frame; `handles` adds the thirds
/// grid and grab handles while the crop tool is active.
pub(crate) fn paint_crop(painter: &egui::Painter, image: egui::Rect, crop: egui::Rect, handles: bool, locked: bool) {
    let shade = egui::Color32::from_black_alpha(150);
    for r in [
        egui::Rect::from_min_max(image.min, egui::pos2(image.max.x, crop.min.y)),
        egui::Rect::from_min_max(egui::pos2(image.min.x, crop.max.y), image.max),
        egui::Rect::from_min_max(egui::pos2(image.min.x, crop.min.y), egui::pos2(crop.min.x, crop.max.y)),
        egui::Rect::from_min_max(egui::pos2(crop.max.x, crop.min.y), egui::pos2(image.max.x, crop.max.y)),
    ] {
        if r.is_positive() {
            painter.rect_filled(r, 0.0, shade);
        }
    }
    let line = egui::Stroke::new(1.5, egui::Color32::WHITE);
    painter.rect_stroke(crop, 0.0, line, egui::StrokeKind::Middle);
    if !handles {
        return;
    }
    let faint = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(90));
    for t in [1.0 / 3.0, 2.0 / 3.0] {
        let x = crop.min.x + crop.width() * t;
        let y = crop.min.y + crop.height() * t;
        painter.line_segment([egui::pos2(x, crop.min.y), egui::pos2(x, crop.max.y)], faint);
        painter.line_segment([egui::pos2(crop.min.x, y), egui::pos2(crop.max.x, y)], faint);
    }
    let mut points = vec![crop.left_top(), crop.right_top(), crop.left_bottom(), crop.right_bottom()];
    if !locked {
        points.extend([crop.center_top(), crop.center_bottom(), crop.left_center(), crop.right_center()]);
    }
    for p in points {
        let handle = egui::Rect::from_center_size(p, egui::vec2(9.0, 9.0));
        painter.rect_filled(handle, 1.0, egui::Color32::WHITE);
        painter.rect_stroke(handle, 1.0, egui::Stroke::new(1.0, egui::Color32::from_black_alpha(160)), egui::StrokeKind::Outside);
    }
}

/// The crop box while `grab` is dragged to `p` (picture pixels, may be outside `bounds`);
/// `ratio` is the locked width-over-height, if any.
pub(crate) fn dragged(grab: Grab, p: egui::Pos2, bounds: egui::Rect, ratio: Option<f32>) -> egui::Rect {
    match grab {
        Grab::New(anchor) => box_from(anchor, clamp(p, bounds), ratio, bounds),
        Grab::Move { start, from } => {
            let mut r = start.translate(p - from);
            // Slide back inside rather than shrinking.
            r = r.translate(egui::vec2((bounds.min.x - r.min.x).max(0.0), (bounds.min.y - r.min.y).max(0.0)));
            r.translate(egui::vec2((bounds.max.x - r.max.x).min(0.0), (bounds.max.y - r.max.y).min(0.0)))
        }
        Grab::Edges { start, dx, dy } => {
            let p = clamp(p, bounds);
            if let Some(ratio) = ratio {
                // Locked shapes only offer corners: the opposite corner stays put.
                let anchor = egui::pos2(if dx < 0 { start.max.x } else { start.min.x }, if dy < 0 { start.max.y } else { start.min.y });
                return box_from(anchor, p, Some(ratio), bounds);
            }
            let mut r = start;
            match dx {
                -1 => r.min.x = p.x.min(r.max.x - 1.0),
                1 => r.max.x = p.x.max(r.min.x + 1.0),
                _ => {}
            }
            match dy {
                -1 => r.min.y = p.y.min(r.max.y - 1.0),
                1 => r.max.y = p.y.max(r.min.y + 1.0),
                _ => {}
            }
            r
        }
    }
}
