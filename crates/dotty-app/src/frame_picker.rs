//! Picking which frame of a video (or which range of frames) becomes an image: a preview,
//! a timeline, frame stepping, and for animations the range, frame rate and size with the
//! same size budget the conversion uses. "Edit picture…" hands the picked frames to the
//! image editor.

use crate::audio_editor::{fmt_time, time_field};
use codecs_video::convert::{default_side, plan_range, wants_range};
use codecs_video::{VideoInfo, VideoReader};
use dotty_core::{EditOp, Edits, Format, FrameSelection, JobId, Options};
use eframe::egui;
use codecs_video::image::{self, RgbImage};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

/// Longest side of preview frames.
const PREVIEW_SIDE: u32 = 960;

pub enum Outcome {
    Open,
    Cancelled,
    /// `edits` is `None` when nothing was picked (the defaults).
    Done { edits: Option<Edits>, thumb: Option<egui::ColorImage> },
    /// Open the image editor on the frames `sel` picks.
    EditPicture { sel: FrameSelection },
}

enum Msg {
    Info(Result<VideoInfo, String>),
    Frame(f64, RgbImage),
    Failed(String),
}

/// A thread that owns the video and decodes the frames asked for, newest request first.
struct Worker {
    requests: mpsc::Sender<f64>,
    replies: mpsc::Receiver<Msg>,
}

impl Worker {
    fn start(ctx: &egui::Context, path: PathBuf, from: Format) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<f64>();
        let (tx, replies) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let mut reader = match VideoReader::open(&path, from, 4).and_then(|mut r| r.keyframes().map(|_| r)) {
                Ok(r) => r,
                Err(e) => return send(Msg::Info(Err(e.to_string()))),
            };
            send(Msg::Info(Ok(reader.info().clone())));
            while let Ok(mut t) = req_rx.recv() {
                // Only the latest request matters (the slider may have moved on).
                while let Ok(newer) = req_rx.try_recv() {
                    t = newer;
                }
                match reader.frame_at(t) {
                    Ok(Some(img)) => {
                        let img = if img.width().max(img.height()) > PREVIEW_SIDE {
                            let k = PREVIEW_SIDE as f32 / img.width().max(img.height()) as f32;
                            image::imageops::thumbnail(&img, (img.width() as f32 * k) as u32, (img.height() as f32 * k) as u32)
                        } else {
                            img
                        };
                        send(Msg::Frame(t, img));
                    }
                    Ok(None) => {}
                    Err(e) => send(Msg::Failed(e.to_string())),
                }
            }
        });
        Self { requests: req_tx, replies }
    }
}

pub struct FramePicker {
    pub entry: JobId,
    title: String,
    to: Format,
    worker: Worker,
    info: Option<VideoInfo>,
    error: Option<String>,
    sel: FrameSelection,
    /// Picking a range (animation / every frame) rather than one frame.
    range: bool,
    /// The target could be either (animated PNG/WebP, TIFF pages): offer the choice.
    can_choose: bool,
    /// The time being previewed, and the newest one asked for.
    cursor: f64,
    asked: Option<f64>,
    preview: Option<(f64, egui::TextureHandle, RgbImage)>,
    /// Image edits made earlier (kept with the selection).
    image_ops: Vec<EditOp>,
    start_text: String,
    end_text: String,
    cursor_text: String,
}

impl FramePicker {
    pub fn open(ctx: &egui::Context, entry: JobId, path: PathBuf, from: Format, to: Format, options: Options, existing: Option<Arc<Edits>>) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let sel = existing.as_ref().and_then(|e| e.frames().copied()).unwrap_or_default();
        let image_ops = existing.map(|e| e.ops.iter().filter(|op| !matches!(op, EditOp::Frames(_))).cloned().collect()).unwrap_or_default();
        let range = wants_range(to, &options, &sel);
        let animatable = to.animated_output() || to.paged_output();
        let can_choose = animatable && to != Format::Gif && !options.split_frames;
        Self {
            entry,
            title,
            to,
            worker: Worker::start(ctx, path, from),
            info: None,
            error: None,
            sel,
            range,
            can_choose,
            cursor: sel.start,
            asked: None,
            preview: None,
            image_ops,
            start_text: String::new(),
            end_text: String::new(),
            cursor_text: String::new(),
        }
    }

    fn duration(&self) -> f64 {
        self.info.as_ref().and_then(|i| i.duration).unwrap_or(0.0)
    }

    fn frame_step(&self) -> f64 {
        1.0 / self.info.as_ref().and_then(|i| i.fps()).unwrap_or(25.0)
    }

    fn plan(&self) -> Option<codecs_video::convert::RangePlan> {
        let info = self.info.as_ref()?;
        Some(plan_range(&self.sel, self.to, info.display_size(), self.duration(), info.fps()))
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        while let Ok(m) = self.worker.replies.try_recv() {
            match m {
                Msg::Info(Ok(info)) => self.info = Some(info),
                Msg::Info(Err(e)) | Msg::Failed(e) => self.error = Some(e),
                Msg::Frame(t, img) => {
                    let color = egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw());
                    let tex = ctx.load_texture("frame-preview", color, egui::TextureOptions::LINEAR);
                    self.preview = Some((t, tex, img));
                }
            }
        }
        if self.info.is_some() && self.asked != Some(self.cursor) {
            self.asked = Some(self.cursor);
            let _ = self.worker.requests.send(self.cursor);
        }
        let screen = ctx.content_rect();
        let size = (screen.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(360.0, 300.0));
        let mut outcome = Outcome::Open;
        egui::Modal::new(egui::Id::unique("frame-picker")).show(ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            outcome = self.ui(ui);
        });
        outcome
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Outcome {
        let mut outcome = Outcome::Open;
        ui.horizontal_wrapped(|ui| {
            ui.strong(&self.title);
            ui.separator();
            if self.can_choose {
                let before = self.range;
                ui.selectable_value(&mut self.range, false, "Single frame").on_hover_text("Save the frame on screen as a picture");
                ui.selectable_value(&mut self.range, true, "Animation").on_hover_text("Save a stretch of the video as an animated picture");
                if self.range && !before && self.sel.end.is_none() {
                    // Start the range at the frame on screen.
                    self.sel.start = self.cursor;
                }
            } else {
                ui.label(if self.range { "Pick the part to keep" } else { "Pick the frame" });
            }
        });
        let Some(info) = self.info.clone() else {
            match &self.error {
                Some(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, format!("✖ Couldn't open this video: {e}"));
                    if ui.button("Close").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                }
                None => {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
            }
            return outcome;
        };
        let duration = self.duration();
        let footer = 120.0;
        let area = ui.available_size() - egui::vec2(0.0, footer);
        let (rect, _) = ui.allocate_exact_size(area.max(egui::vec2(64.0, 64.0)), egui::Sense::hover());
        ui.painter().rect_filled(rect, 4.0, egui::Color32::BLACK);
        if let Some((_, tex, _)) = &self.preview {
            let s = tex.size_vec2();
            let k = (rect.width() / s.x).min(rect.height() / s.y);
            let shown = egui::Rect::from_center_size(rect.center(), s * k);
            ui.painter().image(tex.id(), shown, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        if self.preview.as_ref().is_none_or(|p| p.0 != self.cursor) {
            ui.painter().text(rect.right_top() + egui::vec2(-8.0, 8.0), egui::Align2::RIGHT_TOP, "…", egui::FontId::proportional(20.0), egui::Color32::WHITE);
        }
        ui.add_space(4.0);

        // Timeline: the range in range mode, the playhead.
        let step = self.frame_step();
        ui.horizontal(|ui| {
            if ui.small_button("◀").on_hover_text("Previous frame (Left arrow)").clicked() {
                self.cursor = (self.cursor - step).max(0.0);
            }
            if ui.small_button("▶").on_hover_text("Next frame (Right arrow)").clicked() {
                self.cursor = (self.cursor + step).min((duration - step).max(0.0));
            }
            if let Some(t) = time_field(ui, &mut self.cursor_text, self.cursor) {
                self.cursor = t.clamp(0.0, duration);
            }
            ui.label(format!("/ {}", fmt_time(duration)));
            let width = ui.available_width();
            let (bar, resp) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::click_and_drag());
            let x = |t: f64| bar.left() + (t / duration.max(1e-9)) as f32 * bar.width();
            let painter = ui.painter();
            painter.rect_filled(bar.shrink2(egui::vec2(0.0, 7.0)), 3.0, ui.visuals().extreme_bg_color);
            if self.range
                && let Some(plan) = self.plan()
            {
                let r = egui::Rect::from_x_y_ranges(x(plan.start)..=x(plan.end), bar.y_range()).shrink2(egui::vec2(0.0, 5.0));
                painter.rect_filled(r, 3.0, ui.visuals().selection.bg_fill);
            }
            painter.line_segment([egui::pos2(x(self.cursor), bar.top()), egui::pos2(x(self.cursor), bar.bottom())], egui::Stroke::new(2.0, ui.visuals().strong_text_color()));
            if let Some(p) = resp.interact_pointer_pos() {
                self.cursor = (((p.x - bar.left()) / bar.width()) as f64 * duration).clamp(0.0, duration);
            }
        });
        if !ui.ctx().egui_wants_keyboard_input() {
            let (left, right) = ui.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight)));
            if left {
                self.cursor = (self.cursor - step).max(0.0);
            }
            if right {
                self.cursor = (self.cursor + step).min((duration - step).max(0.0));
            }
        }

        if self.range {
            self.range_controls(ui, &info);
        } else {
            self.sel.start = self.cursor;
            let (w, h) = info.display_size();
            ui.horizontal(|ui| {
                ui.weak(format!("The frame at {} · {w}×{h} · {}", fmt_time(self.cursor), info.codec.name()));
            });
        }

        ui.horizontal(|ui| {
            if !self.image_ops.is_empty() {
                ui.weak("Picture edits are kept (Edit picture… to change them)");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Keep this pick (Enter)").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    outcome = self.done();
                }
                if ui.button("Cancel").clicked() {
                    outcome = Outcome::Cancelled;
                }
                if ui.button("✏ Edit picture…").on_hover_text("Remove the background, draw, add text or crop the picked frame(s)").clicked() {
                    outcome = Outcome::EditPicture { sel: self.selection() };
                }
            });
        });
        outcome
    }

    fn range_controls(&mut self, ui: &mut egui::Ui, info: &VideoInfo) {
        let duration = self.duration();
        let plan = self.plan();
        ui.horizontal_wrapped(|ui| {
            ui.strong("Range");
            ui.label("Start");
            if let Some(t) = time_field(ui, &mut self.start_text, self.sel.start) {
                self.sel.start = t.clamp(0.0, duration);
            }
            ui.label("End");
            let end = plan.map_or(duration, |p| p.end);
            if let Some(t) = time_field(ui, &mut self.end_text, end) {
                self.sel.end = Some(t.clamp(self.sel.start, duration));
            }
            if ui.button("⇤ Start here").on_hover_text("Start at the frame on screen (I)").clicked() || ui.input(|i| !i.focused && i.key_pressed(egui::Key::I)) {
                self.sel.start = self.cursor;
                if self.sel.end.is_some_and(|e| e < self.cursor) {
                    self.sel.end = None;
                }
            }
            if ui.button("End here ⇥").on_hover_text("End at the frame on screen (O)").clicked() || ui.input(|i| !i.focused && i.key_pressed(egui::Key::O)) {
                self.sel.end = Some(self.cursor.max(self.sel.start));
            }
            ui.separator();
            let src_fps = info.fps().unwrap_or(30.0) as f32;
            let mut fps = self.sel.fps.unwrap_or(codecs_video::convert::DEFAULT_FPS).min(src_fps);
            if ui.add(egui::DragValue::new(&mut fps).range(1.0..=src_fps.max(1.0)).speed(0.2).suffix(" fps").max_decimals(1)).on_hover_text("Frames per second: fewer makes a smaller file").changed() {
                self.sel.fps = Some(fps);
            }
            let longest = info.display_size().0.max(info.display_size().1);
            let mut side = self.sel.max_side.unwrap_or_else(|| default_side(self.to)).min(longest);
            if ui.add(egui::DragValue::new(&mut side).range(64..=longest).speed(4.0).suffix(" px")).on_hover_text("Longest side: smaller makes a smaller file").changed() {
                self.sel.max_side = Some(side);
            }
            ui.checkbox(&mut self.sel.fit_longer, "Fit longer clip (lower quality)").on_hover_text(
                "Lower the frame rate, then the size, so a longer stretch fits (up to a minute)",
            );
        });
        if let Some(p) = plan {
            ui.horizontal_wrapped(|ui| {
                let used = p.pixels() as f64 / codecs_video::convert::BUDGET_PIXELS as f64 * 100.0;
                ui.weak(format!("{} frames · {}×{} · {:.1} fps · {:.1} s · {used:.0}% of the size limit", p.frames, p.width, p.height, p.fps, p.end - p.start));
                if p.shortened {
                    ui.colored_label(ui.visuals().warn_fg_color, "⚠ shortened to fit")
                        .on_hover_text("This much doesn't fit in an animation: pick a shorter range, lower the frame rate or size, or turn on \"Fit longer clip\".");
                }
            });
        }
    }

    /// The selection to save: in single-frame mode only the time matters.
    fn selection(&self) -> FrameSelection {
        if self.range {
            // Picking a range with the default "to the end" for PNG/WebP still needs an end,
            // which is what marks it as an animation.
            let mut sel = self.sel;
            if self.can_choose && sel.end.is_none() {
                sel.end = Some(self.duration());
            }
            sel
        } else {
            FrameSelection { start: self.cursor, ..Default::default() }
        }
    }

    fn done(&self) -> Outcome {
        let sel = self.selection();
        let thumb = self.preview.as_ref().map(|(_, _, img)| {
            let k = (crate::editor::THUMB_SIDE as f32 / img.width().max(img.height()) as f32).min(1.0);
            let (w, h) = (((img.width() as f32 * k) as u32).max(1), ((img.height() as f32 * k) as u32).max(1));
            let t = image::imageops::thumbnail(img, w, h);
            egui::ColorImage::from_rgb([t.width() as usize, t.height() as usize], t.as_raw())
        });
        let mut ops = Vec::new();
        if sel != FrameSelection::default() || !self.image_ops.is_empty() {
            ops.push(EditOp::Frames(sel));
            ops.extend(self.image_ops.iter().cloned());
        }
        if ops.is_empty() {
            return Outcome::Done { edits: None, thumb: None };
        }
        Outcome::Done { edits: Some(Edits { ops }), thumb }
    }
}

/// The frames `sel` picks, as the image editor's working copies (the same frames the
/// conversion will write).
pub fn load_for_editor(path: PathBuf, from: Format, to: Format, sel: FrameSelection, options: Options) -> crate::editor::Loaded {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let progress = |_: f32| {};
    let ctx = dotty_core::ConvertCtx::new(&options, &cancel, &progress);
    let img = codecs_video::convert::video_frames(&path, from, to, &sel, &ctx).map_err(|e| e.to_string())?;
    Ok(codecs_image::edit::fit_for_edit(img, from, &options, crate::editor::MAX_SIDE, crate::editor::MAX_PIXELS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-audio/tests/fixtures/video_two_tracks.mkv"))
    }

    fn ready(p: &mut FramePicker) {
        match p.worker.replies.recv_timeout(std::time::Duration::from_secs(10)).unwrap() {
            Msg::Info(Ok(info)) => p.info = Some(info),
            _ => panic!("expected the video's info"),
        }
    }

    #[test]
    fn the_editor_gets_exactly_the_frames_the_conversion_writes() {
        let one = FrameSelection { start: 0.2, ..Default::default() };
        let (frames, size) = load_for_editor(fixture(), Format::Mkv, Format::Png, one, Options::default()).unwrap();
        assert_eq!((frames.len(), size), (1, (64, 64)));
        let range = FrameSelection { start: 0.1, end: Some(0.4), ..Default::default() };
        let (frames, _) = load_for_editor(fixture(), Format::Mkv, Format::Gif, range, Options::default()).unwrap();
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn done_saves_the_pick_and_keeps_picture_edits() {
        let ctx = egui::Context::default();
        let mut p = FramePicker::open(&ctx, 1, fixture(), Format::Mkv, Format::Jpeg, Options::default(), None);
        ready(&mut p);
        assert!(!p.range && !p.can_choose, "JPEG: one frame");
        assert!(matches!(p.done(), Outcome::Done { edits: None, .. }), "the first frame is the default");
        p.cursor = 0.3;
        let Outcome::Done { edits: Some(e), .. } = p.done() else { panic!("expected a pick") };
        assert_eq!(e.frames().unwrap().start, 0.3);

        // Earlier picture edits are kept with a new pick.
        let crop = EditOp::Crop(dotty_core::Region { x: 0, y: 0, width: 8, height: 8, of_width: 64, of_height: 64 });
        let existing = Arc::new(Edits { ops: vec![EditOp::Frames(FrameSelection { start: 0.1, ..Default::default() }), crop.clone()] });
        let mut p = FramePicker::open(&ctx, 1, fixture(), Format::Mkv, Format::Webp, Options::default(), Some(existing));
        ready(&mut p);
        assert!(p.can_choose && !p.range, "WebP: a still unless Animation is picked");
        assert_eq!(p.cursor, 0.1);
        p.range = true;
        let Outcome::Done { edits: Some(e), .. } = p.done() else { panic!("expected a pick") };
        assert!(e.frames().unwrap().end.is_some(), "an animation for WebP needs an end");
        assert_eq!(e.crop(), Some(match crop { EditOp::Crop(r) => r, _ => unreachable!() }));
    }
}
