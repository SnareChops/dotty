//! The image editor: a modal over the file list for quick touch-ups before a conversion
//! (removing a background, drawing with a pen, adding text, cropping). It works on an 8-bit preview (capped in size) and
//! produces [`Edits`] that the conversion scales to the full image, so the source file is
//! never changed.

use codecs_image::caption;
use codecs_image::edit::{self, Rgba8Image, WandSettings};
use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use crate::crop_box::{self, ASPECTS, Aspect, Grab, clamp, grab_cursor, hit, paint_crop, snap};
use dotty_core::{Caption, CaptionFont, Captions, Drawing, EditOp, Edits, Format, JobId, Mask, Options, Region, Stroke};

/// Longest side the editor works at. Bigger images are edited scaled down and the edits are
/// stretched back up, which keeps every click and slider drag fast.
pub const MAX_SIDE: u32 = 3072;
/// Pixels across all frames of an animation, so long ones fit in memory (~256 MB of RGBA).
pub const MAX_PIXELS: u64 = 64 << 20;
/// Height of the frame strip shown for animations and multi-page images.
const STRIP: f32 = 64.0;
/// Frame thumbnails built per screen refresh, so opening a long animation stays responsive.
const THUMBS_PER_FRAME: usize = 6;
/// Undo steps kept.
const HISTORY: usize = 50;
/// Longest side of the row thumbnail texture.
pub const THUMB_SIDE: u32 = 192;
/// Tint for selected pixels.
const SELECTION: [u8; 3] = [40, 140, 255];
/// Quick pen colours; any other comes from the colour picker.
/// Caption changes this close together (seconds) are one undo step, so typing a word
/// or dragging a caption doesn't fill the history.
const CAPTION_UNDO_GAP: f64 = 1.0;
const SWATCHES: [(u8, u8, u8, &str); 6] = [
    (230, 30, 40, "Red"),
    (255, 200, 0, "Yellow"),
    (40, 180, 70, "Green"),
    (30, 110, 230, "Blue"),
    (0, 0, 0, "Black"),
    (255, 255, 255, "White"),
];

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

/// The decoded working copies (one per frame or page) and the first image's full size.
pub type Loaded = Result<(Vec<Rgba8Image>, (u32, u32)), String>;

enum State {
    /// Decoding in the background; edits made earlier wait for the image.
    Loading(mpsc::Receiver<Loaded>, Option<Arc<Edits>>),
    Failed(String),
    Ready(Box<Canvas>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Wand,
    Pen,
    Text,
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



/// One undo step. Unchanged frames share their compressed masks.
struct Snapshot {
    keeps: Vec<Arc<Rle>>,
    strokes: Vec<Stroke>,
    captions: Vec<Caption>,
    crop: Option<egui::Rect>,
}

/// One animation frame or page. All frames share the working size.
struct Frame {
    source: Rgba8Image,
    /// 255 keeps a pixel, 0 has removed it.
    keep: Vec<u8>,
    /// `keep` compressed for the undo history; `None` after it changes.
    rle: Option<Arc<Rle>>,
    /// For the frame strip; `None` after `keep` changes.
    thumb: Option<egui::TextureHandle>,
}

impl Frame {
    fn new(source: Rgba8Image, keep: Vec<u8>) -> Self {
        Self { source, keep, rle: None, thumb: None }
    }

    /// Multiply the kept amount by the inverse of `selection`.
    fn erase(&mut self, selection: &[u8]) {
        for (k, &s) in self.keep.iter_mut().zip(selection) {
            *k = ((*k as u32 * (255 - s as u32) + 127) / 255) as u8;
        }
        self.changed();
    }

    fn changed(&mut self) {
        self.rle = None;
        self.thumb = None;
    }

    fn erased(&self) -> bool {
        self.keep.iter().any(|&v| v < 255)
    }

    fn rle(&mut self) -> Arc<Rle> {
        self.rle.get_or_insert_with(|| Arc::new(Rle::encode(&self.keep))).clone()
    }
}

struct Canvas {
    frames: Vec<Frame>,
    /// The frame being shown and edited.
    current: usize,
    /// Bring the current frame's thumbnail into view.
    scroll_strip: bool,
    /// The image's real size, for showing crop sizes in its pixels.
    full_size: (u32, u32),
    tool: Tool,
    /// Kept when switching frames, so one selection can be erased from several.
    selection: Vec<u8>,
    /// The selection before the last pick, so changing a tool setting can redo that pick.
    base: Vec<u8>,
    last: Option<(Pick, Combine)>,
    settings: WandSettings,
    /// Finished pen strokes, in working-image pixels.
    strokes: Vec<Stroke>,
    /// The stroke being drawn right now.
    drawing: Option<Stroke>,
    pen_color: [u8; 3],
    /// Pen width in the image's real pixels.
    pen_width: f32,
    /// Text placed on the image, in working-image pixels.
    captions: Vec<Caption>,
    selected_caption: Option<usize>,
    /// Put the cursor in the text box on the next frame (a caption was just added or picked).
    focus_caption: bool,
    /// Dragging caption `.0` that started at `.1`, from pointer position `.2` (image pixels).
    caption_grab: Option<(usize, egui::Pos2, egui::Pos2)>,
    /// The style the next caption gets: the last one edited.
    caption_style: Caption,
    /// Rendered captions by look (not position), so moving one doesn't re-render it.
    caption_cache: HashMap<String, (egui::TextureHandle, egui::Vec2)>,
    last_caption_change: f64,
    /// Undo/redo just replaced the captions; that's not a change to record.
    captions_restored: bool,
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
        Self::open_with(ctx, entry, title, existing, move || {
            edit::decode_for_edit(&path, from, &options, MAX_SIDE, MAX_PIXELS).map_err(|e| e.to_string())
        })
    }

    /// Edit images made by `load` (on a background thread), e.g. frames picked from a video.
    pub fn open_with(
        ctx: &egui::Context,
        entry: JobId,
        title: String,
        existing: Option<Arc<Edits>>,
        load: impl FnOnce() -> Loaded + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load());
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
    fn new(sources: Vec<Rgba8Image>, full_size: (u32, u32), existing: Option<&Edits>) -> Self {
        let (w, h) = sources[0].dimensions();
        let n = (w * h) as usize;
        let frames = sources
            .into_iter()
            .enumerate()
            .map(|(i, source)| {
                let mask = existing.and_then(|e| e.frame_mask(i));
                Frame::new(source, mask.map_or_else(|| vec![255; n], |m| edit::resize_mask(m, w, h)))
            })
            .collect();
        let strokes = existing.and_then(Edits::drawing).map_or_else(Vec::new, |d| {
            // Drawn at this working size before, so this is normally a no-op.
            let (sx, sy) = (w as f32 / d.of_width as f32, h as f32 / d.of_height as f32);
            d.strokes
                .iter()
                .map(|s| Stroke {
                    points: s.points.iter().map(|&(x, y)| (x * sx, y * sy)).collect(),
                    width: s.width * (sx + sy) / 2.0,
                    color: s.color,
                })
                .collect()
        });
        let captions = existing.and_then(Edits::captions).map_or_else(Vec::new, |c| {
            let (sx, sy) = (w as f32 / c.of_width as f32, h as f32 / c.of_height as f32);
            c.items
                .iter()
                .map(|c| Caption { x: c.x * sx, y: c.y * sy, size: c.size * (sx + sy) / 2.0, ..c.clone() })
                .collect()
        });
        let caption_style = Caption {
            text: String::new(),
            x: 0.0,
            y: 0.0,
            size: (h as f32 * 0.1).max(8.0),
            font: CaptionFont::Meme,
            uppercase: true,
            fill: [255, 255, 255, 255],
            outline: [0, 0, 0, 255],
            outline_width: 0.08,
        };
        let crop = existing.and_then(Edits::crop).map(|r| {
            let (x, y, cw, ch) = r.scaled(w, h);
            egui::Rect::from_min_size(egui::pos2(x as f32, y as f32), egui::vec2(cw as f32, ch as f32))
        });
        Self {
            frames,
            current: 0,
            scroll_strip: false,
            full_size,
            tool: Tool::Wand,
            selection: vec![0; n],
            base: vec![0; n],
            last: None,
            settings: WandSettings::default(),
            strokes,
            drawing: None,
            pen_color: [SWATCHES[0].0, SWATCHES[0].1, SWATCHES[0].2],
            pen_width: 8.0,
            captions,
            selected_caption: None,
            focus_caption: false,
            caption_grab: None,
            caption_style,
            caption_cache: HashMap::new(),
            last_caption_change: f64::NEG_INFINITY,
            captions_restored: false,
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

    fn source(&self) -> &Rgba8Image {
        &self.frames[self.current].source
    }

    fn size(&self) -> egui::Vec2 {
        egui::vec2(self.source().width() as f32, self.source().height() as f32)
    }

    fn multi(&self) -> bool {
        self.frames.len() > 1
    }

    fn show_frame(&mut self, i: usize) {
        if i < self.frames.len() && i != self.current {
            self.current = i;
            self.image_dirty = true;
            // Sliders would otherwise redo the last pick on the new frame's pixels.
            self.last = None;
        }
    }

    fn bounds(&self) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, self.size())
    }

    fn ui(&mut self, ui: &mut egui::Ui, title: &str, checker: &egui::TextureHandle) -> Outcome {
        let mut outcome = Outcome::Open;
        let before = self.settings;
        let captions_before = self.captions.clone();
        ui.horizontal_wrapped(|ui| {
            ui.strong(title);
            ui.separator();
            ui.selectable_value(&mut self.tool, Tool::Wand, "🪄 Magic wand").on_hover_text(
                "Click to select an area of similar colour, then press Delete to erase it. Shift+click adds to the selection, Alt+click or Ctrl+click removes from it.",
            );
            ui.selectable_value(&mut self.tool, Tool::Pen, "✏ Pen")
                .on_hover_text("Draw freehand, e.g. to circle something or add a quick note. Hold Shift for a straight line.");
            ui.selectable_value(&mut self.tool, Tool::Text, "🔤 Text")
                .on_hover_text("Click the image to add text, or use \"Add top/bottom text\". Drag text to move it.");
            ui.selectable_value(&mut self.tool, Tool::Crop, "⛶ Crop")
                .on_hover_text("Drag a box around the part to keep. Drag its edges or corners to adjust it, or inside it to move it.");
            ui.separator();
            match self.tool {
                Tool::Wand => self.wand_controls(ui),
                Tool::Pen => self.pen_controls(ui),
                Tool::Text => self.text_controls(ui),
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

        let footer = 28.0 + if self.multi() { STRIP + 8.0 } else { 0.0 };
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
        self.record_caption_changes(ui, captions_before);
        if self.multi() {
            self.frame_strip(ui);
        }

        ui.horizontal(|ui| {
            let hint = match self.tool {
                Tool::Wand => "Click: select · Shift: add · Alt/Ctrl: subtract · Del: erase",
                Tool::Pen => "Drag: draw · Shift+drag: straight line · Click: dot",
                Tool::Text => "Click: add text · Click text: edit · Drag text: move · Del: delete text",
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

    /// Thumbnails of every frame; click one to edit it. A dot marks frames with erased parts.
    fn frame_strip(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut built = 0;
        let mut show = None;
        ui.horizontal(|ui| {
            if ui.small_button("◀").on_hover_text("Previous frame (Left arrow)").clicked() {
                show = Some(self.current.saturating_sub(1));
            }
            ui.label(format!("Frame {} / {}", self.current + 1, self.frames.len()));
            if ui.small_button("▶").on_hover_text("Next frame (Right arrow)").clicked() {
                show = Some(self.current + 1);
            }
            egui::ScrollArea::horizontal().id_salt("frame-strip").max_height(STRIP).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, f) in self.frames.iter_mut().enumerate() {
                        let aspect = f.source.width() as f32 / f.source.height() as f32;
                        let size = egui::vec2(((STRIP - 12.0) * aspect).clamp(24.0, 160.0), STRIP - 12.0);
                        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                        if f.thumb.is_none() && built < THUMBS_PER_FRAME && ui.is_rect_visible(rect) {
                            let img = edit::thumbnail(&edit::composite(&f.source, &f.keep), 128);
                            f.thumb = Some(ctx.load_texture(format!("frame-{i}"), color_image(&img), egui::TextureOptions::LINEAR));
                            built += 1;
                        }
                        let painter = ui.painter();
                        painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                        if let Some(t) = &f.thumb {
                            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                            painter.image(t.id(), rect, uv, egui::Color32::WHITE);
                        }
                        let stroke = if i == self.current {
                            egui::Stroke::new(2.5, ui.visuals().selection.stroke.color)
                        } else {
                            egui::Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color)
                        };
                        painter.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Outside);
                        if f.erased() {
                            painter.circle_filled(rect.right_top() + egui::vec2(-5.0, 5.0), 3.5, ui.visuals().selection.stroke.color);
                        }
                        if i == self.current && std::mem::take(&mut self.scroll_strip) {
                            resp.scroll_to_me(None);
                        }
                        if resp.on_hover_text(format!("Frame {}", i + 1)).clicked() {
                            show = Some(i);
                        }
                    }
                });
            });
        });
        if built > 0 {
            ctx.request_repaint(); // keep building the rest
        }
        if let Some(i) = show {
            self.show_frame(i.min(self.frames.len() - 1));
            self.scroll_strip = true;
        }
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
        if self.multi() {
            ui.separator();
            if ui.add_enabled(any, egui::Button::new("Erase in all frames"))
                .on_hover_text("Erase the current selection from every frame (works well when the background doesn't move)")
                .clicked()
            {
                self.erase_all_frames();
            }
            if ui.button("Remove background in all frames")
                .on_hover_text("Select the background of each frame with the current settings, and erase it")
                .clicked()
            {
                self.remove_background_all_frames();
            }
        }
        let removed = self.frames.iter().any(Frame::erased);
        let restore = if self.multi() { "Restore all frames" } else { "Restore all" };
        if ui.add_enabled(removed, egui::Button::new(restore)).on_hover_text("Bring back every erased pixel").clicked() {
            self.push_undo();
            for f in self.frames.iter_mut().filter(|f| f.erased()) {
                f.keep.fill(255);
                f.changed();
            }
            self.image_dirty = true;
        }
    }

    fn pen_controls(&mut self, ui: &mut egui::Ui) {
        for (r, g, b, name) in SWATCHES {
            let color = [r, g, b];
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
            let picked = self.pen_color == color;
            ui.painter().rect_filled(rect, 3.0, egui::Color32::from_rgb(r, g, b));
            let outline = if picked { ui.visuals().selection.stroke } else { ui.visuals().widgets.inactive.bg_stroke };
            ui.painter().rect_stroke(rect, 3.0, egui::Stroke::new(if picked { 2.5 } else { 1.0 }, outline.color), egui::StrokeKind::Outside);
            if resp.on_hover_text(name).clicked() {
                self.pen_color = color;
            }
        }
        ui.color_edit_button_srgb(&mut self.pen_color).on_hover_text("Any colour");
        ui.label("Width");
        ui.add(egui::Slider::new(&mut self.pen_width, 1.0..=100.0).logarithmic(true).integer().suffix(" px"))
            .on_hover_text("Line width in the image's own pixels.");
        if ui.add_enabled(!self.strokes.is_empty(), egui::Button::new("Clear drawing")).on_hover_text("Remove every pen stroke").clicked() {
            self.push_undo();
            self.strokes.clear();
            self.image_dirty = true;
        }
    }

    /// The pen width in working-image pixels.
    fn pen_width_working(&self) -> f32 {
        self.pen_width * self.source().width() as f32 / self.full_size.0.max(1) as f32
    }

    fn text_controls(&mut self, ui: &mut egui::Ui) {
        let (w, h) = (self.source().width() as f32, self.source().height() as f32);
        let margin = h * 0.04 + self.caption_style.size * 0.45;
        if ui.button("Add top text").clicked() {
            self.add_caption(egui::pos2(w / 2.0, margin));
        }
        if ui.button("Add bottom text").clicked() {
            self.add_caption(egui::pos2(w / 2.0, h - margin));
        }
        ui.separator();
        let Some(i) = self.selected_caption.filter(|&i| i < self.captions.len()) else {
            ui.weak("Click the image to add text, or click some text to edit it");
            return;
        };
        let real = self.full_size.0 as f32 / w;
        let c = &mut self.captions[i];
        let text = ui.add(egui::TextEdit::multiline(&mut c.text).hint_text("Type your text").desired_rows(1).desired_width(220.0));
        if std::mem::take(&mut self.focus_caption) {
            text.request_focus();
        }
        egui::ComboBox::from_id_salt("caption-font")
            .selected_text(match c.font {
                CaptionFont::Meme => "Meme",
                CaptionFont::Sans => "Sans",
            })
            .width(64.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut c.font, CaptionFont::Meme, "Meme").on_hover_text("Bold, condensed (Anton)");
                ui.selectable_value(&mut c.font, CaptionFont::Sans, "Sans").on_hover_text("Your system's bold sans-serif");
            });
        ui.checkbox(&mut c.uppercase, "ALL CAPS");
        ui.label("Size");
        let mut size = c.size * real;
        if ui.add(egui::Slider::new(&mut size, 4.0..=2000.0).logarithmic(true).integer().suffix(" px")).changed() {
            c.size = size / real;
        }
        ui.label("Fill");
        rgb_button(ui, &mut c.fill);
        ui.label("Outline");
        rgb_button(ui, &mut c.outline);
        let mut outline = c.outline_width * 100.0;
        if ui.add(egui::Slider::new(&mut outline, 0.0..=20.0).integer().suffix("%")).on_hover_text("Outline thickness, relative to the text size").changed() {
            c.outline_width = outline / 100.0;
        }
        // New captions take on the look of the last one edited.
        self.caption_style = Caption { text: String::new(), x: 0.0, y: 0.0, ..c.clone() };
        if ui.button("Delete").on_hover_text("Remove this text (Delete)").clicked() {
            self.delete_caption();
        }
    }

    fn add_caption(&mut self, at: egui::Pos2) {
        self.captions.push(Caption { x: at.x, y: at.y, ..self.caption_style.clone() });
        self.selected_caption = Some(self.captions.len() - 1);
        self.focus_caption = true;
        self.tool = Tool::Text;
    }

    fn delete_caption(&mut self) {
        if let Some(i) = self.selected_caption.take().filter(|&i| i < self.captions.len()) {
            self.captions.remove(i);
        }
    }

    /// Undo for captions: a burst of changes (typing, dragging) becomes one step.
    fn record_caption_changes(&mut self, ui: &egui::Ui, before: Vec<Caption>) {
        if std::mem::take(&mut self.captions_restored) || self.captions == before {
            return;
        }
        let now = ui.input(|i| i.time);
        if now - self.last_caption_change > CAPTION_UNDO_GAP {
            let mut snapshot = self.snapshot();
            snapshot.captions = before;
            self.push_snapshot(snapshot);
        }
        self.last_caption_change = now;
    }

    /// The caption's texture and its size in working pixels, rendered at `quality` times
    /// the working size (sharper when zoomed in). `None` for empty text.
    fn caption_texture(&mut self, ctx: &egui::Context, i: usize, quality: f32) -> Option<(egui::TextureId, egui::Vec2)> {
        let c = &self.captions[i];
        let key = format!("{quality}|{:?}", Caption { x: 0.0, y: 0.0, ..c.clone() });
        if !self.caption_cache.contains_key(&key) {
            let img = caption::render(c, quality)?;
            let color = egui::ColorImage::from_rgba_premultiplied([img.width as usize, img.height as usize], &img.premultiplied);
            let tex = ctx.load_texture(format!("caption-{i}"), color, egui::TextureOptions::LINEAR);
            let size = egui::vec2(img.width as f32, img.height as f32) / quality;
            self.caption_cache.insert(key.clone(), (tex, size));
        }
        self.caption_cache.get(&key).map(|(t, s)| (t.id(), *s))
    }

    /// Where each caption sits, in image pixels, with its texture (if it has visible text).
    fn caption_layout(&mut self, ctx: &egui::Context, quality: f32) -> Vec<(egui::Rect, Option<egui::TextureId>)> {
        let mut used = Vec::new();
        let layout = (0..self.captions.len())
            .map(|i| {
                let c = &self.captions[i];
                let center = egui::pos2(c.x, c.y);
                // Empty captions get a placeholder box to click and drag.
                let placeholder = egui::vec2(c.size * 4.0, c.size);
                match self.caption_texture(ctx, i, quality) {
                    Some((tex, size)) => {
                        used.push(tex);
                        (egui::Rect::from_center_size(center, size), Some(tex))
                    }
                    None => (egui::Rect::from_center_size(center, placeholder), None),
                }
            })
            .collect();
        // Drop renders nothing uses any more (old text, other zoom levels).
        self.caption_cache.retain(|_, (t, _)| used.contains(&t.id()));
        layout
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
        let (w, h) = self.source().dimensions();
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
        let mut pen_cursor = None;
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
            Tool::Pen => {
                let width = self.pen_width_working();
                let color = [self.pen_color[0], self.pen_color[1], self.pen_color[2], 255];
                let shift = ui.input(|i| i.modifiers.shift);
                if resp.clicked() && !space
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    // A press that egui also saw as a tiny drag must not draw twice.
                    let p = to_image(pos);
                    let stroke = self.drawing.take().unwrap_or(Stroke { points: vec![(p.x, p.y)], width, color });
                    self.add_stroke(stroke);
                } else if resp.drag_started_by(egui::PointerButton::Primary) && !space
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let p = to_image(pos);
                    self.drawing = Some(Stroke { points: vec![(p.x, p.y)], width, color });
                }
                if let Some(stroke) = &mut self.drawing
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let p = to_image(pos);
                    if shift {
                        stroke.points.truncate(1);
                        stroke.points.push((p.x, p.y));
                    } else if let Some(&(lx, ly)) = stroke.points.last()
                        && (egui::pos2(lx, ly) - p).length() * scale >= 1.5
                    {
                        // Skip sub-point moves: they add points but nothing visible.
                        stroke.points.push((p.x, p.y));
                    }
                }
                if resp.drag_stopped()
                    && let Some(stroke) = self.drawing.take()
                {
                    self.add_stroke(stroke);
                }
                if let Some(pos) = resp.hover_pos()
                    && !space
                {
                    // Show the pen's size where it would draw.
                    pen_cursor = Some((pos, width * scale / 2.0));
                }
            }
            Tool::Text => {
                let quality = scale.clamp(1.0, 4.0).log2().ceil().exp2();
                let layout = self.caption_layout(ui.ctx(), quality);
                let hit_caption = |pos: egui::Pos2| layout.iter().rposition(|(r, _)| to_screen(*r).expand(4.0).contains(pos));
                if let Some(pos) = resp.hover_pos()
                    && !space
                {
                    cursor = Some(if hit_caption(pos).is_some() { egui::CursorIcon::Move } else { egui::CursorIcon::Text });
                }
                if resp.clicked() && !space
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let p = to_image(pos);
                    if let Some(i) = hit_caption(pos) {
                        self.selected_caption = Some(i);
                        self.focus_caption = true;
                    } else if self.bounds().contains(p) {
                        self.add_caption(p);
                    } else {
                        self.selected_caption = None;
                    }
                } else if resp.drag_started_by(egui::PointerButton::Primary) && !space
                    && let Some(pos) = resp.interact_pointer_pos()
                    && let Some(i) = hit_caption(pos)
                {
                    self.selected_caption = Some(i);
                    let c = &self.captions[i];
                    self.caption_grab = Some((i, egui::pos2(c.x, c.y), to_image(pos)));
                }
                if let Some((i, start, from)) = self.caption_grab
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    let at = start + (to_image(pos) - from);
                    let c = &mut self.captions[i];
                    (c.x, c.y) = (at.x, at.y);
                    cursor = Some(egui::CursorIcon::Grabbing);
                }
                if resp.drag_stopped() {
                    self.caption_grab = None;
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
        let quality = scale.clamp(1.0, 4.0).log2().ceil().exp2();
        let layout = self.caption_layout(ui.ctx(), quality);
        for (i, (r, tex)) in layout.iter().enumerate() {
            let screen = to_screen(*r);
            if let Some(tex) = tex {
                painter.image(*tex, screen, uv, egui::Color32::WHITE);
            }
            if self.tool != Tool::Text {
                continue;
            }
            let selected = self.selected_caption == Some(i);
            let color = if selected { ui.visuals().selection.stroke.color } else { egui::Color32::from_white_alpha(110) };
            let frame = screen.expand(3.0);
            let shapes = egui::Shape::dashed_line(
                &[frame.left_top(), frame.right_top(), frame.right_bottom(), frame.left_bottom(), frame.left_top()],
                egui::Stroke::new(if selected { 1.5 } else { 1.0 }, color),
                6.0,
                4.0,
            );
            painter.extend(shapes);
            if tex.is_none() {
                painter.text(screen.center(), egui::Align2::CENTER_CENTER, "Type your text", egui::FontId::proportional(14.0), color);
            }
        }
        if let Some(stroke) = &self.drawing {
            // The finished stroke is rendered properly; this is just the live preview.
            let [r, g, b, _] = stroke.color;
            let color = egui::Color32::from_rgb(r, g, b);
            let points: Vec<egui::Pos2> = stroke.points.iter().map(|&(x, y)| image_rect.min + egui::vec2(x, y) * scale).collect();
            let radius = stroke.width * scale / 2.0;
            painter.add(egui::Shape::line(points.clone(), egui::Stroke::new(radius * 2.0, color)));
            // Round joins and ends, like the final rendering.
            for p in points {
                painter.circle_filled(p, radius, color);
            }
        }
        if self.crop.is_some() || self.tool == Tool::Crop {
            let crop = to_screen(self.crop.unwrap_or(self.bounds()));
            paint_crop(&painter, image_rect, crop, self.tool == Tool::Crop, self.aspect.ratio(size).is_some());
        }
        if let Some((pos, radius)) = pen_cursor {
            let [r, g, b] = self.pen_color;
            painter.circle_stroke(pos, radius.max(1.0), egui::Stroke::new(1.5, egui::Color32::from_black_alpha(140)));
            painter.circle_stroke(pos, radius.max(1.0), egui::Stroke::new(1.0, egui::Color32::from_rgb(r, g, b)));
        }
    }

    fn add_stroke(&mut self, stroke: Stroke) {
        self.push_undo();
        self.strokes.push(stroke);
        self.image_dirty = true;
    }

    /// Captions with visible text.
    fn captions_edit(&self) -> Option<Captions> {
        let (w, h) = self.source().dimensions();
        let items: Vec<Caption> = self.captions.iter().filter(|c| !c.text.trim().is_empty()).cloned().collect();
        (!items.is_empty()).then_some(Captions { items, of_width: w, of_height: h })
    }

    fn drawing_edit(&self) -> Option<Drawing> {
        let (w, h) = self.source().dimensions();
        (!self.strokes.is_empty()).then(|| Drawing { strokes: self.strokes.clone(), of_width: w, of_height: h })
    }

    /// The crop box while `grab` is dragged to `p` (image pixels, may be outside the image).
    fn dragged(&self, grab: Grab, p: egui::Pos2) -> egui::Rect {
        crop_box::dragged(grab, p, self.bounds(), self.aspect.ratio(self.size()))
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
            if self.tool == Tool::Text && self.selected_caption.is_some() {
                self.delete_caption();
            } else {
                self.erase();
            }
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
            self.selected_caption = None;
        }
        if pressed(Modifiers::NONE, Key::ArrowLeft) && self.current > 0 {
            self.show_frame(self.current - 1);
            self.scroll_strip = true;
        }
        if pressed(Modifiers::NONE, Key::ArrowRight) {
            self.show_frame(self.current + 1);
            self.scroll_strip = true;
        }
        if pressed(Modifiers::NONE, Key::T) {
            self.tool = Tool::Text;
        }
        if pressed(Modifiers::NONE, Key::W) {
            self.tool = Tool::Wand;
        }
        if pressed(Modifiers::NONE, Key::P) {
            self.tool = Tool::Pen;
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
            Pick::Point(x, y) => edit::magic_wand(self.source(), x, y, &self.settings),
            Pick::Background => edit::select_background(self.source(), &self.settings),
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
        self.frames[self.current].erase(&self.selection);
        self.set_selection(vec![0; self.selection.len()]);
        self.image_dirty = true;
    }

    /// Erase the selection from every frame (they all share the working size).
    fn erase_all_frames(&mut self) {
        if self.selection.iter().all(|&v| v == 0) {
            return;
        }
        self.push_undo();
        for f in &mut self.frames {
            f.erase(&self.selection);
        }
        self.set_selection(vec![0; self.selection.len()]);
        self.image_dirty = true;
    }

    /// "Select background" and erase, on each frame by its own pixels, using every core.
    fn remove_background_all_frames(&mut self) {
        self.push_undo();
        let settings = self.settings;
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
        let per_thread = self.frames.len().div_ceil(threads);
        std::thread::scope(|scope| {
            for chunk in self.frames.chunks_mut(per_thread) {
                scope.spawn(move || {
                    for f in chunk {
                        let sel = edit::select_background(&f.source, &settings);
                        f.erase(&sel);
                    }
                });
            }
        });
        self.set_selection(vec![0; self.selection.len()]);
        self.image_dirty = true;
    }

    fn snapshot(&mut self) -> Snapshot {
        Snapshot {
            keeps: self.frames.iter_mut().map(Frame::rle).collect(),
            strokes: self.strokes.clone(),
            captions: self.captions.clone(),
            crop: self.crop,
        }
    }

    fn restore(&mut self, s: Snapshot) {
        for (f, rle) in self.frames.iter_mut().zip(s.keeps) {
            if !f.rle.as_ref().is_some_and(|r| Arc::ptr_eq(r, &rle)) {
                f.keep = rle.decode();
                f.thumb = None;
                f.rle = Some(rle);
            }
        }
        self.strokes = s.strokes;
        if self.captions != s.captions {
            self.captions = s.captions;
            self.captions_restored = true;
            self.selected_caption = self.selected_caption.filter(|&i| i < self.captions.len());
        }
        self.crop = s.crop;
        self.image_dirty = true;
    }

    fn push_undo(&mut self) {
        let snapshot = self.snapshot();
        self.push_snapshot(snapshot);
    }

    fn push_snapshot(&mut self, snapshot: Snapshot) {
        self.undo.push(snapshot);
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            let now = self.snapshot();
            self.redo.push(now);
            self.restore(prev);
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            let now = self.snapshot();
            self.undo.push(now);
            self.restore(next);
        }
    }

    fn done(&self) -> Outcome {
        let (w, h) = self.source().dimensions();
        let mut ops: Vec<EditOp> = (self.frames.iter().enumerate())
            .filter(|(_, f)| f.erased())
            .map(|(i, f)| EditOp::Erase { frame: i, mask: Mask::new(w, h, f.keep.clone()) })
            .collect();
        let drawing = self.drawing_edit();
        if let Some(d) = &drawing {
            ops.push(EditOp::Draw(d.clone()));
        }
        let captions = self.captions_edit();
        if let Some(c) = &captions {
            ops.push(EditOp::Text(c.clone()));
        }
        let region = self.region();
        if let Some(r) = region {
            ops.push(EditOp::Crop(r));
        }
        if ops.is_empty() {
            return Outcome::Done { edits: None, thumb: None };
        }
        // The row shows the first frame, as most viewers do.
        let mut result = edit::composite(&self.frames[0].source, &self.frames[0].keep);
        if let Some(d) = &drawing {
            edit::draw_rgba(&mut result, d);
        }
        if let Some(c) = &captions {
            edit::captions_rgba(&mut result, c);
        }
        if let Some(r) = &region {
            result = edit::crop(&result, r);
        }
        let thumb = color_image(&edit::thumbnail(&result, THUMB_SIDE));
        Outcome::Done { edits: Some(Edits { ops }), thumb: Some(thumb) }
    }

    /// Rebuild the textures that changed.
    fn upload(&mut self, ctx: &egui::Context) {
        let (w, h) = self.source().dimensions();
        // Crisp pixels when zoomed in, smooth when zoomed out.
        let options = egui::TextureOptions { magnification: egui::TextureFilter::Nearest, ..egui::TextureOptions::LINEAR };
        if self.image_dirty {
            let frame = &self.frames[self.current];
            let mut img = edit::composite(&frame.source, &frame.keep);
            if let Some(d) = self.drawing_edit() {
                edit::draw_rgba(&mut img, &d);
            }
            let img = color_image(&img);
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

/// A colour button for an RGBA colour's RGB part.
fn rgb_button(ui: &mut egui::Ui, color: &mut [u8; 4]) {
    let mut rgb = [color[0], color[1], color[2]];
    if ui.color_edit_button_srgb(&mut rgb).changed() {
        color[..3].copy_from_slice(&rgb);
    }
}

fn combine_with(base: &[u8], new: Vec<u8>, combine: Combine) -> Vec<u8> {
    match combine {
        Combine::Replace => new,
        Combine::Add => base.iter().zip(new).map(|(&b, n)| b.max(n)).collect(),
        Combine::Subtract => base.iter().zip(new).map(|(&b, n)| b.min(255 - n)).collect(),
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
        Canvas::new(vec![Rgba8Image::from_pixel(w, h, [9; 4].into())], (w * 2, h * 2), None)
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
        let mut c = Canvas::new(vec![img], (4, 1), None);
        c.settings.smooth = false;
        c.pick(Pick::Point(0, 0), Combine::Replace);
        assert_eq!(c.selection, [255, 255, 0, 0]);
        c.erase();
        assert_eq!(c.frames[0].keep, [0, 0, 255, 255]);
        assert!(c.selection.iter().all(|&v| v == 0), "erasing clears the selection");
        c.undo();
        assert_eq!(c.frames[0].keep, [255; 4]);
        c.redo();
        assert_eq!(c.frames[0].keep, [0, 0, 255, 255]);

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
        let c = Canvas::new(vec![Rgba8Image::from_pixel(4, 4, [9; 4].into())], (4, 4), Some(&edits));
        assert!(c.frames[0].keep.iter().all(|&v| v == 0));
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
    fn pen_strokes_become_an_edit_and_undo() {
        let mut c = canvas(100, 50);
        c.pen_width = 10.0; // real pixels; the working copy is half size
        assert_eq!(c.pen_width_working(), 5.0);
        c.add_stroke(Stroke { points: vec![(10.0, 10.0), (90.0, 10.0)], width: c.pen_width_working(), color: [255, 0, 0, 255] });
        c.add_stroke(Stroke { points: vec![(50.0, 40.0)], width: 5.0, color: [0, 0, 255, 255] });
        c.push_undo();
        c.crop = Some(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(50.0, 50.0)));
        let Outcome::Done { edits: Some(edits), .. } = c.done() else { panic!("expected edits") };
        // Draw comes before the crop so strokes line up with the uncropped picture.
        assert!(matches!(edits.ops[..], [EditOp::Draw(_), EditOp::Crop(_)]));
        assert_eq!(edits.drawing().unwrap().strokes.len(), 2);
        assert!(!edits.erases(), "drawing doesn't need transparency");

        c.undo();
        c.undo();
        assert_eq!((c.strokes.len(), c.crop), (1, None));
        c.redo();
        assert_eq!(c.strokes.len(), 2);

        // Reopening restores the strokes.
        let again = Canvas::new(vec![Rgba8Image::from_pixel(100, 50, [9; 4].into())], (200, 100), Some(&edits));
        assert_eq!(again.strokes, c.strokes);
    }

    #[test]
    fn captions_become_an_edit_in_order_and_skip_empty_text() {
        let mut c = canvas(100, 50);
        c.add_caption(egui::pos2(50.0, 10.0));
        assert_eq!((c.selected_caption, c.focus_caption, c.tool == Tool::Text), (Some(0), true, true));
        c.captions[0].text = "TOP".into();
        c.add_caption(egui::pos2(50.0, 40.0)); // left empty
        assert_eq!(c.captions[1].size, c.captions[0].size, "new captions copy the style");
        c.add_stroke(Stroke { points: vec![(1.0, 1.0)], width: 2.0, color: [0, 0, 0, 255] });
        c.push_undo();
        c.crop = Some(egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(60.0, 50.0)));
        let Outcome::Done { edits: Some(edits), thumb: Some(_) } = c.done() else { panic!("expected edits") };
        assert!(matches!(edits.ops[..], [EditOp::Draw(_), EditOp::Text(_), EditOp::Crop(_)]));
        assert_eq!(edits.captions().unwrap().items.len(), 1, "empty captions are dropped");

        // Reopening restores the captions.
        let again = Canvas::new(vec![Rgba8Image::from_pixel(100, 50, [9; 4].into())], (200, 100), Some(&edits));
        assert_eq!(again.captions[0].text, "TOP");
        assert_eq!(again.captions[0].x, 50.0);
    }

    #[test]
    fn caption_edits_undo_in_bursts() {
        let ctx = egui::Context::default();
        let mut c = canvas(100, 50);
        let frame = |c: &mut Canvas, time: f64, change: &dyn Fn(&mut Canvas)| {
            let input = egui::RawInput { time: Some(time), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                let before = c.captions.clone();
                change(c);
                c.record_caption_changes(ui, before);
            });
            // No renderer takes the texture updates (the font atlas on the first frame), and
            // egui panics in debug builds if they're dropped unapplied.
            output.textures_delta.clear();
        };
        frame(&mut c, 0.0, &|c| c.add_caption(egui::pos2(10.0, 10.0)));
        frame(&mut c, 0.3, &|c| c.captions[0].text.push('A'));
        frame(&mut c, 0.6, &|c| c.captions[0].text.push('B'));
        frame(&mut c, 5.0, &|c| c.captions[0].text.push('C'));
        assert_eq!(c.undo.len(), 2, "add + typing is one step, the later edit another");
        c.undo();
        assert_eq!(c.captions[0].text, "AB");
        frame(&mut c, 5.1, &|_| {}); // the restore itself isn't recorded
        assert_eq!(c.undo.len(), 1);
        c.undo();
        assert!(c.captions.is_empty());
        assert_eq!(c.selected_caption, None, "selection dropped with the caption");
    }

    /// Three 4x1 frames: black on the left, a frame-specific colour on the right.
    fn animation() -> Canvas {
        let frame = |c: u8| Rgba8Image::from_fn(4, 1, |x, _| if x < 2 { [0, 0, 0, 255].into() } else { [c, c, 255, 255].into() });
        let mut c = Canvas::new(vec![frame(10), frame(120), frame(240)], (4, 1), None);
        c.settings.smooth = false;
        c
    }

    #[test]
    fn frames_are_erased_separately_and_saved_per_frame() {
        let mut c = animation();
        c.show_frame(1);
        c.pick(Pick::Point(0, 0), Combine::Replace);
        c.erase();
        assert_eq!(c.frames[1].keep, [0, 0, 255, 255]);
        assert!(!c.frames[0].erased() && !c.frames[2].erased());

        // A selection survives switching frames, so it can be erased there too.
        c.pick(Pick::Point(3, 0), Combine::Replace);
        c.show_frame(2);
        c.erase();
        assert_eq!(c.frames[2].keep, [255, 255, 0, 0]);

        let Outcome::Done { edits: Some(edits), .. } = c.done() else { panic!("expected edits") };
        assert!(edits.frame_mask(0).is_none());
        assert_eq!(&*edits.frame_mask(1).unwrap().data, &[0, 0, 255, 255]);
        assert_eq!(&*edits.frame_mask(2).unwrap().data, &[255, 255, 0, 0]);

        // Reopening puts every frame's mask back.
        let sources = c.frames.iter().map(|f| f.source.clone()).collect();
        let again = Canvas::new(sources, (4, 1), Some(&edits));
        assert_eq!(again.frames.iter().map(|f| f.keep.clone()).collect::<Vec<_>>(), c.frames.iter().map(|f| f.keep.clone()).collect::<Vec<_>>());
    }

    #[test]
    fn all_frame_actions_and_undo() {
        let mut c = animation();
        c.pick(Pick::Point(0, 0), Combine::Replace);
        c.erase_all_frames();
        assert!(c.frames.iter().all(|f| f.keep == [0, 0, 255, 255]));
        c.undo();
        assert!(c.frames.iter().all(|f| !f.erased()), "one undo step for all frames");

        // Each frame is searched on its own: the black surround goes, the dot stays.
        let mostly_black = |c: u8| Rgba8Image::from_fn(5, 3, |x, y| if x == 2 && y == 1 { [c, 0, 0, 255].into() } else { [0, 0, 0, 255].into() });
        let mut c = Canvas::new(vec![mostly_black(200), mostly_black(250)], (5, 3), None);
        c.settings.smooth = false;
        c.remove_background_all_frames();
        for f in &c.frames {
            assert_eq!(f.keep.iter().filter(|&&k| k == 255).count(), 1, "only the centre pixel stays");
        }
        c.undo();
        assert!(c.frames.iter().all(|f| !f.erased()));
        c.redo();
        assert!(c.frames.iter().all(Frame::erased));
    }

    #[test]
    fn undo_shares_unchanged_frames() {
        let mut c = animation();
        let first = c.snapshot();
        c.show_frame(1);
        c.pick(Pick::Point(0, 0), Combine::Replace);
        c.erase();
        let second = c.snapshot();
        assert!(Arc::ptr_eq(&first.keeps[0], &second.keeps[0]), "untouched frames aren't re-encoded");
        assert!(!Arc::ptr_eq(&first.keeps[1], &second.keeps[1]));
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
