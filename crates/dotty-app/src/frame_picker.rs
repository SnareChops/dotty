//! Picking which frame of a video (or which range of frames) becomes an image: a preview,
//! a timeline, frame stepping, and for animations the range, frame rate and size with the
//! same size budget the conversion uses. "Edit picture…" hands the picked frames to the
//! image editor.

use crate::audio_editor::{fmt_time, time_field};
use codecs_video::convert::{default_side, plan_range, wants_range};
use codecs_video::VideoInfo;
use dotty_core::{EditOp, Edits, Format, FrameSelection, JobId, Options};
use eframe::egui;
use codecs_video::image::{self, RgbImage};
use crate::range_bar;
use crate::video_worker::{Cmd, FrameQueue, Msg, Worker};
use std::path::PathBuf;
use std::sync::Arc;

pub enum Outcome {
    Open,
    Cancelled,
    /// `edits` is `None` when nothing was picked (the defaults).
    Done { edits: Option<Edits>, thumb: Option<egui::ColorImage> },
    /// Open the image editor on the frames `sel` picks.
    EditPicture { sel: FrameSelection },
}

/// A playback in progress.
struct Playing {
    run: u64,
    /// The video time it started at, and when.
    t0: f64,
    started: std::time::Instant,
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
    // Playback: the animation (or video) in motion.
    play: Option<Playing>,
    run: u64,
    queue: FrameQueue,
    shown: Option<f64>,
    /// The video's frames ran out during playback.
    video_ended: bool,
    /// The range handle being dragged on the timeline.
    grab: Option<range_bar::Handle>,
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
            worker: Worker::start(ctx, path, from, None),
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
            play: None,
            run: 0,
            queue: FrameQueue::default(),
            shown: None,
            video_ended: false,
            grab: None,
        }
    }

    fn playing(&self) -> bool {
        self.play.is_some()
    }

    fn texture(&mut self, ctx: &egui::Context, t: f64, img: RgbImage) {
        let color = egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw());
        let tex = ctx.load_texture("frame-preview", color, egui::TextureOptions::LINEAR);
        self.preview = Some((t, tex, img));
    }

    /// The part that plays: the range for an animation (looping), else the whole video.
    fn play_bounds(&self) -> (f64, f64) {
        match (self.range, self.plan()) {
            (true, Some(p)) => (p.start, p.end),
            _ => (0.0, self.duration()),
        }
    }

    fn stop_play(&mut self) {
        self.play = None;
        self.queue.clear();
        self.shown = None;
        self.video_ended = false;
        // Show the exact frame at the playhead again.
        self.asked = None;
    }

    fn start_play(&mut self, t: f64) {
        let (lo, hi) = self.play_bounds();
        // Outside the part that plays (or at its very end): from its start.
        let t = if t < lo || t >= hi - self.frame_step() { lo } else { t };
        self.run += 1;
        self.queue.clear();
        self.shown = None;
        self.video_ended = false;
        self.cursor = t;
        let _ = self.worker.requests.send(Cmd::Play(t, self.run, 1.0));
        self.play = Some(Playing { run: self.run, t0: t, started: std::time::Instant::now() });
    }

    fn toggle_play(&mut self) {
        if self.playing() {
            self.stop_play();
        } else {
            self.start_play(self.cursor);
        }
    }

    /// Show the frame due now. An animation plays at its own frame rate (so you see how
    /// choppy it will be) and loops over its range; a single-frame pick plays on to the end.
    fn update_playback(&mut self, ctx: &egui::Context) {
        let Some(pb) = &self.play else { return };
        let (run, t0, started) = (pb.run, pb.t0, pb.started);
        while let Ok((r, pts, img)) = self.worker.play.try_recv() {
            if r == run {
                self.queue.push(pts, img);
            }
        }
        let clock = t0 + started.elapsed().as_secs_f64();
        let (lo, hi) = self.play_bounds();
        if clock >= hi || (self.video_ended && self.queue.is_empty()) {
            if self.range {
                self.start_play(lo);
            } else {
                self.stop_play();
            }
            ctx.request_repaint();
            return;
        }
        let shown_clock = match (self.range, self.plan()) {
            (true, Some(p)) => lo + ((clock - lo) * p.fps as f64).floor() / p.fps as f64,
            _ => clock,
        };
        if let Some((pts, img)) = self.queue.at(shown_clock)
            && self.shown != Some(pts)
        {
            let img = img.clone();
            self.shown = Some(pts);
            self.texture(ctx, pts, img);
        }
        self.cursor = clock.clamp(0.0, self.duration());
        ctx.request_repaint();
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
                Msg::Ready(Ok(r)) => self.info = r.video,
                Msg::Ready(Err(e)) => self.error = Some(e),
                Msg::Failed(e) => {
                    self.stop_play();
                    self.error = Some(e);
                }
                Msg::Frame(img) => {
                    // A still asked for before playback began must not replace its frames.
                    if self.play.is_none() {
                        self.texture(ctx, self.asked.unwrap_or(self.cursor), img);
                    }
                }
                Msg::Ended(run) => self.video_ended |= self.play.as_ref().is_some_and(|p| p.run == run),
                Msg::Picture(..) => {}
            }
        }
        self.update_playback(ctx);
        if self.info.is_some() && self.play.is_none() && self.asked != Some(self.cursor) {
            self.asked = Some(self.cursor);
            let _ = self.worker.requests.send(Cmd::Seek(self.cursor));
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
        if !self.playing() && self.preview.as_ref().is_none_or(|p| p.0 != self.cursor) {
            ui.painter().text(rect.right_top() + egui::vec2(-8.0, 8.0), egui::Align2::RIGHT_TOP, "…", egui::FontId::proportional(20.0), egui::Color32::WHITE);
        }
        ui.add_space(4.0);

        // Timeline: the range (with draggable diamonds) in range mode, and the playhead.
        let step = self.frame_step();
        let cursor_before = self.cursor;
        ui.horizontal(|ui| {
            let label = if self.playing() { "⏸ Pause" } else { "▶ Play" };
            let tip = if self.range { "Play the animation as it will look (Space)" } else { "Play the video to find the frame (Space)" };
            if ui.button(label).on_hover_text(tip).clicked() {
                self.toggle_play();
            }
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
            let range = if self.range { self.plan().map(|p| (p.start, p.end)) } else { None };
            let width = ui.available_width();
            let out = range_bar::show(ui, width, duration, range, Some(self.cursor), step, &mut self.grab, |_, _| {});
            if let Some(r) = range
                && out.range != r
            {
                self.sel.start = out.range.0;
                self.sel.end = Some(out.range.1);
            }
            if let Some(t) = out.playhead {
                self.cursor = t;
            }
        });
        if !ui.ctx().egui_wants_keyboard_input() {
            let (left, right, space) = ui.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight), i.key_pressed(egui::Key::Space)));
            if left {
                self.cursor = (self.cursor - step).max(0.0);
            }
            if right {
                self.cursor = (self.cursor + step).min((duration - step).max(0.0));
            }
            if space {
                self.toggle_play();
            }
        }
        // Moving the playhead while playing carries on from the new place.
        if self.playing() && (self.cursor - cursor_before).abs() > 1e-9 {
            self.start_play(self.cursor);
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
            Msg::Ready(Ok(r)) => p.info = r.video,
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

    /// Run the picker's playback for about `secs`, as the UI would each frame.
    fn run_for(p: &mut FramePicker, ctx: &egui::Context, secs: f64) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        while std::time::Instant::now() < until {
            while let Ok(m) = p.worker.replies.try_recv() {
                if let Msg::Ended(run) = m {
                    p.video_ended |= p.play.as_ref().is_some_and(|pl| pl.run == run);
                }
            }
            p.update_playback(ctx);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    #[test]
    fn an_animation_plays_at_its_own_rate_and_loops_over_its_range() {
        let ctx = egui::Context::default();
        let mut p = FramePicker::open(&ctx, 1, fixture(), Format::Mkv, Format::Gif, Options::default(), None);
        ready(&mut p);
        assert!(p.range, "a GIF is a range");
        // 0.1–0.4 s at 5 fps: frames at 0.1, 0.3 (the source has them every 0.1 s).
        p.sel = FrameSelection { start: 0.1, end: Some(0.4), fps: Some(5.0), ..Default::default() };
        assert_eq!(p.play_bounds(), (0.1, 0.4));
        p.toggle_play();
        assert!(p.playing());
        let mut seen = std::collections::BTreeSet::new();
        let first_run = p.run;
        let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(1.2);
        while std::time::Instant::now() < until {
            run_for(&mut p, &ctx, 0.02);
            if let Some(t) = p.shown {
                seen.insert((t * 10.0).round() as i64);
                assert!((0.09..=0.41).contains(&t), "only the range is shown: {t}");
            }
        }
        assert!(p.run > first_run, "it looped back to the start of the range");
        assert!(seen.len() >= 2, "different frames were shown: {seen:?}");
        assert!(p.cursor >= 0.1 - 1e-9 && p.cursor <= 0.4 + 0.05, "the playhead stays in the range: {}", p.cursor);
        p.toggle_play();
        assert!(!p.playing());
    }

    #[test]
    fn a_single_frame_pick_plays_the_video_on_and_stops_at_the_end() {
        let ctx = egui::Context::default();
        let mut p = FramePicker::open(&ctx, 1, fixture(), Format::Mkv, Format::Jpeg, Options::default(), None);
        ready(&mut p);
        assert!(!p.range);
        p.cursor = 0.1;
        p.toggle_play();
        run_for(&mut p, &ctx, 1.0);
        assert!(!p.playing(), "stopped by itself at the end of the video");
        // Playing from the end starts over; moving the playhead mid-play carries on from there.
        p.cursor = p.duration();
        p.toggle_play();
        assert_eq!(p.cursor, 0.0);
        p.stop_play();
    }

    #[test]
    fn dragging_the_range_handles_changes_the_selection() {
        let ctx = egui::Context::default();
        let mut p = FramePicker::open(&ctx, 1, fixture(), Format::Mkv, Format::Gif, Options::default(), None);
        ready(&mut p);
        // Draw the picker once headlessly: it must lay out with the bar and handles.
        for t in [0.0, 1.0] {
            let input = egui::RawInput { time: Some(t), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let _ = p.ui(ui);
            });
            out.textures_delta.clear();
        }
        let plan = p.plan().unwrap();
        assert_eq!(plan.start, 0.0);
        assert!((plan.end - p.duration()).abs() < 0.01, "by default the whole (short) video: {} of {}", plan.end, p.duration());
    }
}
