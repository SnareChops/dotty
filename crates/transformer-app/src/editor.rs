//! The image editor: a modal over the file list for quick touch-ups before a conversion
//! (removing a background, cropping). It works on an 8-bit preview (capped in size) and
//! produces [`Edits`] that the conversion scales to the full image, so the source file is
//! never changed.

use codecs_image::edit::{self, Rgba8Image, WandSettings};
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use transformer_core::{EditOp, Edits, Format, JobId, Mask, Options, Region};

/// Longest side the editor works at. Bigger images are edited scaled down and the edits are
/// stretched back up, which keeps every click and slider drag fast.
const MAX_SIDE: u32 = 3072;
/// Undo steps kept.
const HISTORY: usize = 50;
/// Longest side of the row thumbnail texture.
pub const THUMB_SIDE: u32 = 192;
/// Tint for selected pixels.
const SELECTION: [u8; 3] = [40, 140, 255];
/// How close (in points) the pointer must be to grab a crop edge or corner.
const GRAB: f32 = 8.0;

pub enum Outcome {
    Open,
    Cancelled,
    /// `edits` is `None` when the image is unchanged. `thumb` shows the result.
    Done { edits: Option<Edits>, thumb: Option<egui::ColorImage> },
}

pub struct Editor {
    pub entry: JobId,
    title: String,
    state: State,
}

/// The decoded working copy and the image's full size.
type Loaded = Result<(Rgba8Image, (u32, u32)), String>;

enum State {
    /// Decoding in the background; edits made earlier wait for the image.
    Loading(mpsc::Receiver<Loaded>, Option<Arc<Edits>>),
    Failed(String),
    Ready(Box<Canvas>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Wand,
    Crop,
}

#[derive(Clone, Copy, PartialEq)]
enum Pick {
    Point(u32, u32),
    Background,
}

#[derive(Clone, Copy, PartialEq)]
enum Combine {
    Replace,
    Add,
    Subtract,
}

#[derive(Clone, Copy, PartialEq)]
enum Aspect {
    Free,
    Original,
    Ratio(u8, u8),
}

const ASPECTS: [Aspect; 9] = [
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
    fn label(self) -> String {
        match self {
            Aspect::Free => "Free".into(),
            Aspect::Original => "Original".into(),
            Aspect::Ratio(w, h) => format!("{w}:{h}"),
        }
    }

    /// Width over height, if fixed.
    fn ratio(self, size: egui::Vec2) -> Option<f32> {
        match self {
            Aspect::Free => None,
            Aspect::Original => Some(size.x / size.y),
            Aspect::Ratio(w, h) => Some(w as f32 / h as f32),
        }
    }
}

/// What a drag on the crop box is doing. Rects are in image pixels.
#[derive(Clone, Copy)]
enum Grab {
    /// Drawing a new box from this corner.
    New(egui::Pos2),
    Move { start: egui::Rect, from: egui::Pos2 },
    /// Moving edges: -1 = left/top, 1 = right/bottom, 0 = that axis stays.
    Edges { start: egui::Rect, dx: i8, dy: i8 },
}

/// One undo step.
struct Snapshot {
    keep: Rle,
    crop: Option<egui::Rect>,
}

struct Canvas {
    source: Rgba8Image,
    /// The image's real size, for showing crop sizes in its pixels.
    full_size: (u32, u32),
    tool: Tool,
    /// 255 keeps a pixel, 0 has removed it.
    keep: Vec<u8>,
    selection: Vec<u8>,
    /// The selection before the last pick, so changing a tool setting can redo that pick.
    base: Vec<u8>,
    last: Option<(Pick, Combine)>,
    settings: WandSettings,
    /// The part of the image kept, in working-image pixels; `None` keeps all of it.
    crop: Option<egui::Rect>,
    aspect: Aspect,
    grab: Option<Grab>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    image_tex: Option<egui::TextureHandle>,
    overlay_tex: Option<egui::TextureHandle>,
    image_dirty: bool,
    overlay_dirty: bool,
    /// 1.0 fits the whole image in view.
    zoom: f32,
    pan: egui::Vec2,
}

impl Editor {
    /// Start decoding `path` in the background. `existing` holds edits made earlier.
    pub fn open(
        ctx: &egui::Context,
        entry: JobId,
        path: PathBuf,
        from: Format,
        options: Options,
        existing: Option<Arc<Edits>>,
    ) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(edit::decode_for_edit(&path, from, &options, MAX_SIDE).map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
        Self { entry, title, state: State::Loading(rx, existing) }
    }

    pub fn show(&mut self, ctx: &egui::Context, checker: &egui::TextureHandle) -> Outcome {
        if let State::Loading(rx, existing) = &mut self.state {
            match rx.try_recv() {
                Ok(Ok((img, full))) => self.state = State::Ready(Box::new(Canvas::new(img, full, existing.as_deref()))),
                Ok(Err(e)) => self.state = State::Failed(e),
                Err(mpsc::TryRecvError::Disconnected) => self.state = State::Failed("couldn't open the image".into()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let screen = ctx.content_rect();
        let size = (screen.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(320.0, 240.0));
        let mut outcome = Outcome::Open;
        egui::Modal::new(egui::Id::unique("image-editor")).show(ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            match &mut self.state {
                State::Loading(..) => {
                    ui.horizontal(|ui| {
                        ui.strong(&self.title);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                outcome = Outcome::Cancelled;
                            }
                        });
                    });
                    ui.centered_and_justified(|ui| ui.spinner());
                }
                State::Failed(msg) => {
                    ui.strong(&self.title);
                    ui.colored_label(ui.visuals().error_fg_color, format!("✖ Couldn't open this image for editing: {msg}"));
                    if ui.button("Close").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                }
                State::Ready(canvas) => outcome = canvas.ui(ui, &self.title, checker),
            }
        });
        outcome
    }
}

impl Canvas {
    fn new(source: Rgba8Image, full_size: (u32, u32), existing: Option<&Edits>) -> Self {
        let (w, h) = source.dimensions();
        let n = (w * h) as usize;
        let keep = existing.and_then(Edits::mask).map_or_else(|| vec![255; n], |m| edit::resize_mask(m, w, h));
        let crop = existing.and_then(Edits::crop).map(|r| {
            let (x, y, cw, ch) = r.scaled(w, h);
            egui::Rect::from_min_size(egui::pos2(x as f32, y as f32), egui::vec2(cw as f32, ch as f32))
        });
        Self {
            source,
            full_size,
            tool: Tool::Wand,
            keep,
            selection: vec![0; n],
            base: vec![0; n],
            last: None,
            settings: WandSettings::default(),
            crop,
            aspect: Aspect::Free,
            grab: None,
            undo: Vec::new(),
            redo: Vec::new(),
            image_tex: None,
            overlay_tex: None,
            image_dirty: true,
            overlay_dirty: true,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
        }
    }

    fn size(&self) -> egui::Vec2 {
        egui::vec2(self.source.width() as f32, self.source.height() as f32)
    }

    fn bounds(&self) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, self.size())
    }

    fn ui(&mut self, ui: &mut egui::Ui, title: &str, checker: &egui::TextureHandle) -> Outcome {
        let mut outcome = Outcome::Open;
        let before = self.settings;
        ui.horizontal_wrapped(|ui| {
            ui.strong(title);
            ui.separator();
            ui.selectable_value(&mut self.tool, Tool::Wand, "🪄 Magic wand").on_hover_text(
                "Click to select an area of similar colour, then press Delete to erase it. Shift+click adds to the selection, Alt+click or Ctrl+click removes from it.",
            );
            ui.selectable_value(&mut self.tool, Tool::Crop, "⛶ Crop")
                .on_hover_text("Drag a box around the part to keep. Drag its edges or corners to adjust it, or inside it to move it.");
            ui.separator();
            match self.tool {
                Tool::Wand => self.wand_controls(ui),
                Tool::Crop => self.crop_controls(ui),
            }
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("↶")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                self.undo();
            }
            if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("↷")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked() {
                self.redo();
            }
            if ui.button("Fit").on_hover_text("Show the whole image").clicked() {
                (self.zoom, self.pan) = (1.0, egui::Vec2::ZERO);
            }
        });
        ui.add_space(4.0);

        let footer = 28.0;
        let canvas_size = ui.available_size() - egui::vec2(0.0, footer);
        let (rect, resp) = ui.allocate_exact_size(canvas_size.max(egui::vec2(64.0, 64.0)), egui::Sense::click_and_drag());

        // Running the last pick again keeps sliders live: drag tolerance, watch the selection.
        if self.settings != before
            && let Some((pick, combine)) = self.last
        {
            self.selection = combine_with(&self.base, self.run(pick), combine);
            self.overlay_dirty = true;
        }

        self.canvas(ui, rect, &resp, checker);
        self.shortcuts(ui, &mut outcome);

        ui.horizontal(|ui| {
            let hint = match self.tool {
                Tool::Wand => "Click: select · Shift: add · Alt/Ctrl: subtract · Del: erase",
                Tool::Crop => "Drag: draw box · Drag edges/corners: resize · Drag inside: move · Double-click: reset",
            };
            ui.weak(format!("{hint} · Scroll: zoom · Middle-drag or Space+drag: pan"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Keep these changes (Enter)").clicked() {
                    outcome = self.done();
                }
                if ui.button("Cancel").on_hover_text("Discard the changes made since opening the editor").clicked() {
                    outcome = Outcome::Cancelled;
                }
            });
        });
        outcome
    }

    fn wand_controls(&mut self, ui: &mut egui::Ui) {
        ui.label("Tolerance");
        ui.add(egui::Slider::new(&mut self.settings.tolerance, 0.0..=100.0).integer())
            .on_hover_text("How different a colour can be from the clicked one and still be selected.");
        ui.checkbox(&mut self.settings.contiguous, "Contiguous")
            .on_hover_text("Only select connected pixels. Turn off to select every similar colour in the image.");
        ui.checkbox(&mut self.settings.smooth, "Smooth edges")
            .on_hover_text("Partly select pixels just past the tolerance, for soft, anti-aliased edges.");
        ui.label("Feather");
        ui.add(egui::Slider::new(&mut self.settings.feather, 0..=10).suffix(" px"))
            .on_hover_text("Blur the edge of the selection.");
        ui.separator();
        if ui.button("Select background").on_hover_text("Select the colour around the edges of the image.").clicked() {
            self.pick(Pick::Background, Combine::Replace);
        }
        let any = self.selection.iter().any(|&v| v > 0);
        if ui.add_enabled(any, egui::Button::new("Erase")).on_hover_text("Make the selection transparent (Delete)").clicked() {
            self.erase();
        }
        if ui.button("Invert").on_hover_text("Ctrl+I").clicked() {
            self.invert();
        }
        if ui.add_enabled(any, egui::Button::new("Deselect")).on_hover_text("Esc").clicked() {
            self.set_selection(vec![0; self.selection.len()]);
        }
        let removed = self.keep.iter().any(|&v| v < 255);
        if ui.add_enabled(removed, egui::Button::new("Restore all")).on_hover_text("Bring back every erased pixel").clicked() {
            self.push_undo();
            self.keep.fill(255);
            self.image_dirty = true;
        }
    }

    fn crop_controls(&mut self, ui: &mut egui::Ui) {
        ui.label("Aspect");
        let mut picked = None;
        egui::ComboBox::from_id_salt("crop-aspect").selected_text(self.aspect.label()).width(80.0).show_ui(ui, |ui| {
            for a in ASPECTS {
                if ui.selectable_label(a == self.aspect, a.label()).clicked() {
                    picked = Some(a);
                }
            }
        });
        if let Some(a) = picked.filter(|&a| a != self.aspect) {
            self.aspect = a;
            if let Some(ratio) = a.ratio(self.size()) {
                // Fit the largest box of the new shape inside the current one, centred.
                let current = self.crop.unwrap_or(self.bounds());
                let (w, h) = if current.width() / current.height() > ratio {
                    (current.height() * ratio, current.height())
                } else {
                    (current.width(), current.width() / ratio)
                };
                self.push_undo();
                self.crop = Some(snap(egui::Rect::from_center_size(current.center(), egui::vec2(w, h)), self.bounds()));
            }
        }
        let (w, h) = self.crop_size();
        ui.label(format!("{w} × {h} px"));
        if ui.add_enabled(self.crop.is_some(), egui::Button::new("Reset crop")).on_hover_text("Keep the whole image").clicked() {
            self.reset_crop();
        }
    }

    /// The kept size in the image's real pixels.
    fn crop_size(&self) -> (u32, u32) {
        let (fw, fh) = self.full_size;
        match self.region() {
            Some(r) => {
                let (_, _, w, h) = r.scaled(fw, fh);
                (w, h)
            }
            None => (fw, fh),
        }
    }

    fn reset_crop(&mut self) {
        if self.crop.is_some() {
            self.push_undo();
            self.crop = None;
        }
    }

    fn region(&self) -> Option<Region> {
        let r = self.crop?;
        let (w, h) = self.source.dimensions();
        let region = Region {
            x: r.min.x as u32,
            y: r.min.y as u32,
            width: r.width() as u32,
            height: r.height() as u32,
            of_width: w,
            of_height: h,
        };
        (!region.is_full()).then_some(region)
    }

    fn canvas(&mut self, ui: &mut egui::Ui, rect: egui::Rect, resp: &egui::Response, checker: &egui::TextureHandle) {
        let size = self.size();
        let fit = (rect.width() / size.x).min(rect.height() / size.y);

        // Zoom about the cursor.
        if let Some(cursor) = resp.hover_pos() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = pinch * (scroll / 300.0).exp();
            if factor != 1.0 {
                let old = fit * self.zoom;
                self.zoom = (self.zoom * factor).clamp(0.1, 64.0 / fit.min(1.0));
                let point = (cursor - rect.center() - self.pan) / old;
                self.pan = cursor - rect.center() - point * fit * self.zoom;
            }
        }
        let space = ui.input(|i| i.key_down(egui::Key::Space));
        if resp.dragged_by(egui::PointerButton::Middle)
            || resp.dragged_by(egui::PointerButton::Secondary)
            || (space && resp.dragged_by(egui::PointerButton::Primary))
        {
            self.pan += resp.drag_delta();
        }

        let scale = fit * self.zoom;
        let image_rect = egui::Rect::from_center_size(rect.center() + self.pan, size * scale);
        let to_image = |pos: egui::Pos2| ((pos - image_rect.min) / scale).to_pos2();
        let to_screen = |r: egui::Rect| {
            egui::Rect::from_min_max(image_rect.min + r.min.to_vec2() * scale, image_rect.min + r.max.to_vec2() * scale)
        };

        let mut cursor = if resp.hovered() { Some(egui::CursorIcon::Crosshair) } else { None };
        match self.tool {
            Tool::Wand => {
                if resp.clicked() && !space
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let p = to_image(pos);
                    if self.bounds().contains(p) && p.x < size.x && p.y < size.y {
                        let m = ui.input(|i| i.modifiers);
                        let combine = if m.shift {
                            Combine::Add
                        } else if m.alt || m.command {
                            Combine::Subtract
                        } else {
                            Combine::Replace
                        };
                        self.pick(Pick::Point(p.x as u32, p.y as u32), combine);
                    }
                }
            }
            Tool::Crop => {
                let box_screen = to_screen(self.crop.unwrap_or(self.bounds()));
                let locked = self.aspect.ratio(size).is_some();
                if let Some(pos) = resp.hover_pos()
                    && self.grab.is_none()
                    && !space
                {
                    cursor = Some(grab_cursor(hit(box_screen, pos, locked)));
                }
                if resp.double_clicked() {
                    self.reset_crop();
                } else if resp.drag_started_by(egui::PointerButton::Primary) && !space
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let start = self.crop.unwrap_or(self.bounds());
                    let p = clamp(to_image(pos), self.bounds());
                    self.grab = Some(match hit(box_screen, pos, locked) {
                        (0, 0, true) => Grab::Move { start, from: to_image(pos) },
                        (0, 0, false) => Grab::New(p),
                        (dx, dy, _) => Grab::Edges { start, dx, dy },
                    });
                    self.push_undo();
                }
                if let Some(grab) = self.grab
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    self.crop = Some(self.dragged(grab, to_image(pos)));
                    if let Grab::Move { .. } = grab {
                        cursor = Some(egui::CursorIcon::Grabbing);
                    }
                }
                if self.grab.is_some() && resp.drag_stopped() {
                    self.grab = None;
                    self.finish_crop();
                }
            }
        }
        if space {
            cursor = Some(egui::CursorIcon::Grab);
        }
        if let Some(c) = cursor {
            ui.ctx().set_cursor_icon(c);
        }

        self.upload(ui.ctx());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
        paint_checker(&painter, checker, image_rect);
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        for tex in [&self.image_tex, &self.overlay_tex].into_iter().flatten() {
            painter.image(tex.id(), image_rect, uv, egui::Color32::WHITE);
        }
        if self.crop.is_some() || self.tool == Tool::Crop {
            let crop = to_screen(self.crop.unwrap_or(self.bounds()));
            paint_crop(&painter, image_rect, crop, self.tool == Tool::Crop, self.aspect.ratio(size).is_some());
        }
    }

    /// The crop box while `grab` is dragged to `p` (image pixels, may be outside the image).
    fn dragged(&self, grab: Grab, p: egui::Pos2) -> egui::Rect {
        let bounds = self.bounds();
        let ratio = self.aspect.ratio(self.size());
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
                    let anchor = egui::pos2(
                        if dx < 0 { start.max.x } else { start.min.x },
                        if dy < 0 { start.max.y } else { start.min.y },
                    );
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

    /// Snap the box to whole pixels once a drag ends; a click without a drag changes nothing.
    fn finish_crop(&mut self) {
        let Some(r) = self.crop else { return };
        let r = snap(r, self.bounds());
        if r.width() < 1.0 || r.height() < 1.0 {
            if let Some(prev) = self.undo.pop() {
                self.crop = prev.crop;
            }
            return;
        }
        self.crop = (r != self.bounds()).then_some(r);
    }

    fn shortcuts(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        // Typing into a slider's number field must not erase the image.
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        use egui::{Key, Modifiers};
        let pressed = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        if pressed(Modifiers::NONE, Key::Delete) || pressed(Modifiers::NONE, Key::Backspace) {
            self.erase();
        }
        // Shift+Z first: Ctrl+Z also matches when Shift is held.
        if pressed(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || pressed(Modifiers::COMMAND, Key::Y) {
            self.redo();
        }
        if pressed(Modifiers::COMMAND, Key::Z) {
            self.undo();
        }
        if pressed(Modifiers::COMMAND, Key::I) {
            self.invert();
        }
        if pressed(Modifiers::COMMAND, Key::A) {
            self.set_selection(vec![255; self.selection.len()]);
        }
        if pressed(Modifiers::NONE, Key::Escape) || pressed(Modifiers::COMMAND, Key::D) {
            self.set_selection(vec![0; self.selection.len()]);
        }
        if pressed(Modifiers::NONE, Key::W) {
            self.tool = Tool::Wand;
        }
        if pressed(Modifiers::NONE, Key::C) {
            self.tool = Tool::Crop;
        }
        if pressed(Modifiers::NONE, Key::Enter) {
            *outcome = self.done();
        }
    }

    fn run(&self, pick: Pick) -> Vec<u8> {
        match pick {
            Pick::Point(x, y) => edit::magic_wand(&self.source, x, y, &self.settings),
            Pick::Background => edit::select_background(&self.source, &self.settings),
        }
    }

    fn pick(&mut self, pick: Pick, combine: Combine) {
        let base = if combine == Combine::Replace { vec![0; self.selection.len()] } else { self.selection.clone() };
        self.selection = combine_with(&base, self.run(pick), combine);
        self.base = base;
        self.last = Some((pick, combine));
        self.overlay_dirty = true;
    }

    /// Replace the selection outright (select all, invert, deselect); sliders no longer redo a pick.
    fn set_selection(&mut self, selection: Vec<u8>) {
        self.selection = selection;
        self.last = None;
        self.overlay_dirty = true;
    }

    fn invert(&mut self) {
        let inverted = self.selection.iter().map(|&v| 255 - v).collect();
        self.set_selection(inverted);
    }

    fn erase(&mut self) {
        if self.selection.iter().all(|&v| v == 0) {
            return;
        }
        self.push_undo();
        for (k, &s) in self.keep.iter_mut().zip(&self.selection) {
            *k = ((*k as u32 * (255 - s as u32) + 127) / 255) as u8;
        }
        self.set_selection(vec![0; self.selection.len()]);
        self.image_dirty = true;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { keep: Rle::encode(&self.keep), crop: self.crop }
    }

    fn restore(&mut self, s: Snapshot) {
        self.keep = s.keep.decode();
        self.crop = s.crop;
        self.image_dirty = true;
    }

    fn push_undo(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(prev);
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(next);
        }
    }

    fn done(&self) -> Outcome {
        let (w, h) = self.source.dimensions();
        let mut ops = Vec::new();
        if self.keep.iter().any(|&v| v < 255) {
            ops.push(EditOp::Erase { frame: 0, mask: Mask::new(w, h, self.keep.clone()) });
        }
        let region = self.region();
        if let Some(r) = region {
            ops.push(EditOp::Crop(r));
        }
        if ops.is_empty() {
            return Outcome::Done { edits: None, thumb: None };
        }
        let mut result = edit::composite(&self.source, &self.keep);
        if let Some(r) = &region {
            result = edit::crop(&result, r);
        }
        let thumb = color_image(&edit::thumbnail(&result, THUMB_SIDE));
        Outcome::Done { edits: Some(Edits { ops }), thumb: Some(thumb) }
    }

    /// Rebuild the textures that changed.
    fn upload(&mut self, ctx: &egui::Context) {
        let (w, h) = self.source.dimensions();
        // Crisp pixels when zoomed in, smooth when zoomed out.
        let options = egui::TextureOptions { magnification: egui::TextureFilter::Nearest, ..egui::TextureOptions::LINEAR };
        if self.image_dirty {
            let img = color_image(&edit::composite(&self.source, &self.keep));
            set_texture(ctx, &mut self.image_tex, "editor-image", img, options);
            self.image_dirty = false;
        }
        if self.overlay_dirty {
            let (ws, hs) = (w as usize, h as usize);
            let sel = &self.selection;
            let mut rgba = vec![0u8; ws * hs * 4];
            for y in 0..hs {
                for x in 0..ws {
                    let i = y * ws + x;
                    let v = sel[i];
                    if v == 0 {
                        continue;
                    }
                    // Outline the selection so it reads even over similar colours.
                    let inside = |j: usize| sel[j] >= 128;
                    let edge = v >= 128
                        && ((x > 0 && !inside(i - 1))
                            || (x + 1 < ws && !inside(i + 1))
                            || (y > 0 && !inside(i - ws))
                            || (y + 1 < hs && !inside(i + ws)));
                    let alpha = if edge { 230 } else { (v as u32 * 110 / 255) as u8 };
                    rgba[i * 4..i * 4 + 4].copy_from_slice(&[SELECTION[0], SELECTION[1], SELECTION[2], alpha]);
                }
            }
            let img = egui::ColorImage::from_rgba_unmultiplied([ws, hs], &rgba);
            set_texture(ctx, &mut self.overlay_tex, "editor-selection", img, options);
            self.overlay_dirty = false;
        }
    }
}

fn combine_with(base: &[u8], new: Vec<u8>, combine: Combine) -> Vec<u8> {
    match combine {
        Combine::Replace => new,
        Combine::Add => base.iter().zip(new).map(|(&b, n)| b.max(n)).collect(),
        Combine::Subtract => base.iter().zip(new).map(|(&b, n)| b.min(255 - n)).collect(),
    }
}

fn clamp(p: egui::Pos2, bounds: egui::Rect) -> egui::Pos2 {
    p.clamp(bounds.min, bounds.max)
}

/// Round a box to whole pixels inside `bounds`.
fn snap(r: egui::Rect, bounds: egui::Rect) -> egui::Rect {
    let r = egui::Rect::from_min_max(r.min.round(), r.max.round());
    r.intersect(bounds)
}

/// The box with one corner at `anchor` and the other towards `p`, kept to `ratio` (width
/// over height) and inside `bounds` when given.
fn box_from(anchor: egui::Pos2, p: egui::Pos2, ratio: Option<f32>, bounds: egui::Rect) -> egui::Rect {
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
fn hit(r: egui::Rect, pos: egui::Pos2, locked: bool) -> (i8, i8, bool) {
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

fn grab_cursor((dx, dy, inside): (i8, i8, bool)) -> egui::CursorIcon {
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
fn paint_crop(painter: &egui::Painter, image: egui::Rect, crop: egui::Rect, handles: bool, locked: bool) {
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

fn set_texture(ctx: &egui::Context, slot: &mut Option<egui::TextureHandle>, name: &str, img: egui::ColorImage, options: egui::TextureOptions) {
    match slot {
        Some(tex) => tex.set(img, options),
        None => *slot = Some(ctx.load_texture(name, img, options)),
    }
}

pub fn color_image(img: &Rgba8Image) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}

/// A 2×2 checkerboard tile, repeated to show transparency.
pub fn checker_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let (a, b) = (egui::Color32::from_gray(204), egui::Color32::from_gray(153));
    let img = egui::ColorImage::new([2, 2], vec![a, b, b, a]);
    let options = egui::TextureOptions {
        magnification: egui::TextureFilter::Nearest,
        minification: egui::TextureFilter::Nearest,
        wrap_mode: egui::TextureWrapMode::Repeat,
        ..Default::default()
    };
    ctx.load_texture("checker", img, options)
}

/// Fill `rect` with the checkerboard in 8-point squares, fixed to the screen.
pub fn paint_checker(painter: &egui::Painter, checker: &egui::TextureHandle, rect: egui::Rect) {
    let cell = 16.0; // one 2×2 tile
    let uv = egui::Rect::from_min_max((rect.min.to_vec2() / cell).to_pos2(), (rect.max.to_vec2() / cell).to_pos2());
    painter.image(checker.id(), rect, uv, egui::Color32::WHITE);
}

/// Run-length encoded mask for the undo history (masks are mostly long runs).
struct Rle(Vec<(u8, u32)>);

impl Rle {
    fn encode(data: &[u8]) -> Self {
        let mut runs: Vec<(u8, u32)> = Vec::new();
        for &v in data {
            match runs.last_mut() {
                Some((last, n)) if *last == v => *n += 1,
                _ => runs.push((v, 1)),
            }
        }
        Self(runs)
    }

    fn decode(&self) -> Vec<u8> {
        self.0.iter().flat_map(|&(v, n)| std::iter::repeat_n(v, n as usize)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(w: u32, h: u32) -> Canvas {
        Canvas::new(Rgba8Image::from_pixel(w, h, [9; 4].into()), (w * 2, h * 2), None)
    }

    #[test]
    fn rle_round_trips() {
        let data = [255, 255, 0, 0, 0, 7, 255];
        assert_eq!(Rle::encode(&data).decode(), data);
        assert_eq!(Rle::encode(&[]).decode(), Vec::<u8>::new());
    }

    #[test]
    fn erase_undo_redo() {
        let img = Rgba8Image::from_fn(4, 1, |x, _| if x < 2 { [0, 0, 0, 255].into() } else { [255; 4].into() });
        let mut c = Canvas::new(img, (4, 1), None);
        c.settings.smooth = false;
        c.pick(Pick::Point(0, 0), Combine::Replace);
        assert_eq!(c.selection, [255, 255, 0, 0]);
        c.erase();
        assert_eq!(c.keep, [0, 0, 255, 255]);
        assert!(c.selection.iter().all(|&v| v == 0), "erasing clears the selection");
        c.undo();
        assert_eq!(c.keep, [255; 4]);
        c.redo();
        assert_eq!(c.keep, [0, 0, 255, 255]);

        // Subtracting the white half from "select all" leaves the black half.
        c.set_selection(vec![255; 4]);
        c.pick(Pick::Point(3, 0), Combine::Subtract);
        assert_eq!(c.selection, [255, 255, 0, 0]);
        let Outcome::Done { edits: Some(edits), thumb: Some(_) } = c.done() else { panic!("expected edits") };
        assert_eq!(&*edits.mask().unwrap().data, &[0, 0, 255, 255]);
        assert!(edits.crop().is_none());
    }

    #[test]
    fn earlier_edits_are_restored_at_the_working_size() {
        let crop = Region { x: 2, y: 2, width: 4, height: 4, of_width: 8, of_height: 8 };
        let edits = Edits { ops: vec![EditOp::Erase { frame: 0, mask: Mask::new(4, 4, vec![0; 16]) }, EditOp::Crop(crop)] };
        let c = Canvas::new(Rgba8Image::from_pixel(4, 4, [9; 4].into()), (4, 4), Some(&edits));
        assert!(c.keep.iter().all(|&v| v == 0));
        assert_eq!(c.crop, Some(egui::Rect::from_min_size(egui::pos2(1.0, 1.0), egui::vec2(2.0, 2.0))));
    }

    #[test]
    fn crop_drags_stay_inside_and_keep_their_shape() {
        let mut c = canvas(100, 50);
        // A new box dragged past the edge stops at it.
        let r = c.dragged(Grab::New(egui::pos2(10.0, 10.0)), egui::pos2(500.0, 30.0));
        assert_eq!(r, egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(100.0, 30.0)));

        // Square: follows the longer drag, then shrinks to fit the image.
        c.aspect = Aspect::Ratio(1, 1);
        let r = c.dragged(Grab::New(egui::pos2(10.0, 10.0)), egui::pos2(90.0, 20.0));
        assert_eq!(r, egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(50.0, 50.0)));

        // Moving slides back inside without shrinking.
        let start = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(30.0, 30.0));
        let r = c.dragged(Grab::Move { start, from: egui::pos2(20.0, 20.0) }, egui::pos2(200.0, -50.0));
        assert_eq!(r, egui::Rect::from_min_max(egui::pos2(80.0, 0.0), egui::pos2(100.0, 20.0)));

        // A free edge drag never flips the box inside out.
        c.aspect = Aspect::Free;
        let r = c.dragged(Grab::Edges { start, dx: -1, dy: 0 }, egui::pos2(60.0, 0.0));
        assert_eq!((r.min.x, r.max.x, r.min.y), (29.0, 30.0, 10.0));
    }

    #[test]
    fn crop_becomes_an_edit_with_real_pixel_size() {
        let mut c = canvas(100, 50);
        c.push_undo();
        c.crop = Some(egui::Rect::from_min_max(egui::pos2(10.2, 0.0), egui::pos2(60.0, 49.6)));
        c.finish_crop();
        assert_eq!(c.crop, Some(egui::Rect::from_min_max(egui::pos2(10.0, 0.0), egui::pos2(60.0, 50.0))));
        assert_eq!(c.crop_size(), (100, 100), "shown in the full image's pixels");
        let Outcome::Done { edits: Some(edits), thumb: Some(thumb) } = c.done() else { panic!("expected a crop") };
        assert!(!edits.erases());
        assert_eq!(edits.crop().unwrap().scaled(200, 100), (20, 0, 100, 100));
        assert_eq!(thumb.size, [50, 50]);

        // Undo brings back the uncropped image; a crop covering everything is no crop.
        c.undo();
        assert_eq!(c.crop, None);
        c.push_undo();
        c.crop = Some(c.bounds());
        c.finish_crop();
        assert_eq!(c.crop, None);
        assert!(matches!(c.done(), Outcome::Done { edits: None, .. }));
    }

    #[test]
    fn grabbing_edges_corners_and_inside() {
        let r = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(200.0, 200.0));
        assert_eq!(hit(r, egui::pos2(101.0, 150.0), false), (-1, 0, true));
        assert_eq!(hit(r, egui::pos2(198.0, 203.0), false), (1, 1, false));
        assert_eq!(hit(r, egui::pos2(150.0, 150.0), false), (0, 0, true));
        assert_eq!(hit(r, egui::pos2(101.0, 150.0), true), (0, 0, true), "edges are locked with a fixed shape");
        assert_eq!(hit(r, egui::pos2(300.0, 150.0), false), (0, 0, false));
    }
}
