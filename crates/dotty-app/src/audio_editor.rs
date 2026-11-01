//! The audio editor: a modal over the file list for quick changes before a conversion
//! (trim, fades). It decodes the file into memory, plays a preview through the same stages
//! the converter uses, and produces [`Edits`] for the job, so the source is never changed.

use crate::player::{Player, Request};
use codecs_audio::StreamInfo;
use codecs_audio::edit::{Plan, decode_all};
use dotty_core::{AudioEdits, EditOp, Edits, Format, JobId};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

/// Longest file the editor takes (it holds the whole decode in memory).
const MAX_SECS: u64 = 30 * 60;
/// Frames per block in the two peak levels the waveform is drawn from.
const BLOCK: usize = 256;
const BIG_BLOCK: usize = BLOCK * 64;
/// Changes this close together (seconds) are one undo step (a drag, a slider).
const UNDO_GAP: f64 = 0.8;
const HISTORY: usize = 100;
/// How close (in points) the pointer must be to grab a handle.
const GRAB: f32 = 7.0;
const RULER: f32 = 18.0;
/// Row thumbnail size.
pub const THUMB: [usize; 2] = [192, 40];
const WAVE_RGB: [u8; 3] = [40, 140, 255];

pub enum Outcome {
    Open,
    Cancelled,
    /// `edits` is `None` when nothing changed; `length` is the result's length in seconds.
    Done { edits: Option<Edits>, thumb: Option<egui::ColorImage>, length: f64 },
}

pub struct AudioEditor {
    pub entry: JobId,
    title: String,
    state: State,
    cancel: Arc<AtomicBool>,
}

type Loaded = Result<(Arc<[f32]>, StreamInfo, Peaks), String>;

enum State {
    Loading(mpsc::Receiver<Loaded>, Option<AudioEdits>),
    Failed(String),
    Ready(Box<Session>),
}

impl Drop for AudioEditor {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl AudioEditor {
    pub fn open(ctx: &egui::Context, entry: JobId, path: PathBuf, from: Format, existing: Option<Arc<Edits>>) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (ctx, stop) = (ctx.clone(), cancel.clone());
        std::thread::spawn(move || {
            let result = decode_all(&path, from, MAX_SECS, &stop).map(|(samples, info)| {
                let peaks = Peaks::new(&samples, info.channels as usize);
                (samples, info, peaks)
            });
            let _ = tx.send(result.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
        let existing = existing.and_then(|e| e.audio().copied());
        Self { entry, title, state: State::Loading(rx, existing), cancel }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        if let State::Loading(rx, existing) = &mut self.state {
            match rx.try_recv() {
                Ok(Ok((samples, info, peaks))) => {
                    self.state = State::Ready(Box::new(Session::new(samples, info, peaks, existing.unwrap_or_default())))
                }
                Ok(Err(e)) => self.state = State::Failed(e),
                Err(mpsc::TryRecvError::Disconnected) => self.state = State::Failed("couldn't open the file".into()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let screen = ctx.content_rect();
        let size = (screen.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(320.0, 240.0));
        let size = egui::vec2(size.x, size.y.min(560.0));
        let mut outcome = Outcome::Open;
        egui::Modal::new(egui::Id::unique("audio-editor")).show(ctx, |ui| {
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
                    ui.colored_label(ui.visuals().error_fg_color, format!("✖ Couldn't open this file for editing: {msg}"));
                    if ui.button("Close").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                }
                State::Ready(session) => outcome = session.ui(ui, &self.title),
            }
        });
        outcome
    }
}

/// Min/max of every channel's samples per block, at two block sizes, so the waveform can
/// be drawn at any zoom without scanning the whole file each frame.
struct Peaks {
    small: Vec<(f32, f32)>,
    big: Vec<(f32, f32)>,
}

impl Peaks {
    fn new(samples: &[f32], channels: usize) -> Self {
        let small: Vec<(f32, f32)> = samples.chunks(BLOCK * channels).map(|b| min_max(b.iter().copied())).collect();
        let big = small.chunks(BIG_BLOCK / BLOCK).map(|b| merge(b.iter().copied())).collect();
        Self { small, big }
    }

    /// Min and max over frames `a..b`.
    fn range(&self, samples: &[f32], channels: usize, a: usize, b: usize) -> (f32, f32) {
        let b = b.max(a + 1);
        if b - a <= BLOCK * 2 {
            let (lo, hi) = (a * channels, (b * channels).min(samples.len()));
            return if lo < hi { min_max(samples[lo..hi].iter().copied()) } else { (0.0, 0.0) };
        }
        let (level, size) = if b - a <= BIG_BLOCK * 2 { (&self.small, BLOCK) } else { (&self.big, BIG_BLOCK) };
        let (lo, hi) = (a / size, b.div_ceil(size).min(level.len()));
        if lo < hi { merge(level[lo..hi].iter().copied()) } else { (0.0, 0.0) }
    }
}

fn min_max(it: impl Iterator<Item = f32>) -> (f32, f32) {
    it.fold((0.0f32, 0.0f32), |(lo, hi), s| (lo.min(s), hi.max(s)))
}

fn merge(it: impl Iterator<Item = (f32, f32)>) -> (f32, f32) {
    it.fold((0.0f32, 0.0f32), |(lo, hi), (a, b)| (lo.min(a), hi.max(b)))
}

/// What a drag on the waveform is doing.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Start,
    End,
    FadeIn,
    FadeOut,
    /// Drawing a new kept range from this frame.
    New(u64),
    Pan,
}

struct Session {
    samples: Arc<[f32]>,
    info: StreamInfo,
    peaks: Peaks,
    /// Frames in the file.
    total: u64,
    edits: AudioEdits,
    undo: Vec<AudioEdits>,
    redo: Vec<AudioEdits>,
    last_change: f64,
    player: Player,
    /// Where playback starts (source frame), and where the playhead rests when stopped.
    cursor: u64,
    looping: bool,
    /// Edits changed while playing: restart once the pointer is released.
    replay: bool,
    error: Option<String>,
    /// First frame shown and how many frames the view spans.
    view_start: f64,
    view_len: f64,
    drag: Option<Drag>,
    /// The edits when the current drag began.
    before_drag: AudioEdits,
    start_text: String,
    end_text: String,
    silence_db: f32,
}

impl Session {
    fn new(samples: Arc<[f32]>, info: StreamInfo, peaks: Peaks, edits: AudioEdits) -> Self {
        let total = (samples.len() / info.channels.max(1) as usize) as u64;
        let mut s = Self {
            samples,
            info,
            peaks,
            total,
            edits,
            undo: Vec::new(),
            redo: Vec::new(),
            last_change: f64::NEG_INFINITY,
            player: Player::default(),
            cursor: 0,
            looping: false,
            replay: false,
            error: None,
            view_start: 0.0,
            view_len: total.max(1) as f64,
            drag: None,
            before_drag: AudioEdits::default(),
            start_text: String::new(),
            end_text: String::new(),
            silence_db: -50.0,
        };
        s.cursor = s.plan().start;
        s
    }

    fn rate(&self) -> f64 {
        self.info.sample_rate as f64
    }

    fn duration(&self) -> f64 {
        self.total as f64 / self.rate()
    }

    fn plan(&self) -> Plan {
        Plan::new(&self.edits, self.info.sample_rate, Some(self.total))
    }

    /// Kept range in frames.
    fn kept(&self) -> (u64, u64) {
        let p = self.plan();
        (p.start, p.end.unwrap_or(self.total))
    }

    fn kept_secs(&self) -> f64 {
        let (a, b) = self.kept();
        (b - a) as f64 / self.rate()
    }

    fn set_trim(&mut self, start: f64, end: f64) {
        let d = self.duration();
        let min = 1.0 / self.rate();
        let start = start.clamp(0.0, (d - min).max(0.0));
        let end = end.clamp(start + min, d);
        self.edits.trim_start = if start * self.rate() < 0.5 { 0.0 } else { start };
        self.edits.trim_end = (end < d - 0.5 / self.rate()).then_some(end);
        self.clamp_fades();
    }

    fn clamp_fades(&mut self) {
        let len = self.kept_secs();
        self.edits.fade_in = self.edits.fade_in.clamp(0.0, len);
        self.edits.fade_out = self.edits.fade_out.clamp(0.0, len - self.edits.fade_in);
    }

    fn position(&self) -> u64 {
        self.player.position().unwrap_or(self.cursor)
    }

    fn play_from(&mut self, from: u64) {
        let req = Request { samples: self.samples.clone(), info: self.info.clone(), edits: self.edits, from, looping: self.looping };
        self.error = self.player.play(req).err();
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.stop();
        } else {
            let (a, b) = self.kept();
            let from = if (a..b.saturating_sub(1)).contains(&self.cursor) { self.cursor } else { a };
            self.cursor = from;
            self.play_from(from);
        }
    }

    fn seek(&mut self, frame: u64) {
        self.cursor = frame.min(self.total.saturating_sub(1));
        if self.player.is_playing() {
            self.play_from(self.cursor);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, title: &str) -> Outcome {
        let mut outcome = Outcome::Open;
        let before = self.edits;
        if self.player.reap() {
            // Reached the end: back to where it started, like most players.
        }
        if self.player.is_playing() {
            ui.ctx().request_repaint();
        }

        ui.horizontal_wrapped(|ui| {
            ui.strong(title);
            ui.separator();
            let playing = self.player.is_playing();
            if ui.button(if playing { "⏸ Pause" } else { "▶ Play" }).on_hover_text("Play the result (Space)").clicked() {
                self.toggle_play();
            }
            if ui.button("⏮").on_hover_text("Back to the start (Home)").clicked() {
                self.seek(self.kept().0);
            }
            if ui.toggle_value(&mut self.looping, "🔁 Loop").on_hover_text("Play the kept part over and over (L)").changed() && playing {
                self.play_from(self.position());
            }
            ui.monospace(format!("{} / {}", fmt_time(self.position() as f64 / self.rate()), fmt_time(self.kept_secs())));
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("↶")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                self.undo();
            }
            if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("↷")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked() {
                self.redo();
            }
            if ui.button("Fit").on_hover_text("Show the whole file").clicked() {
                (self.view_start, self.view_len) = (0.0, self.total.max(1) as f64);
            }
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, format!("✖ Can't play: {e}"));
            }
        });
        ui.add_space(4.0);

        let controls = 96.0;
        let wave_size = ui.available_size() - egui::vec2(0.0, controls);
        let (rect, resp) = ui.allocate_exact_size(wave_size.max(egui::vec2(64.0, 80.0)), egui::Sense::click_and_drag());
        self.waveform(ui, rect, &resp);
        ui.add_space(6.0);
        self.controls(ui);
        self.shortcuts(ui, &mut outcome);

        ui.horizontal(|ui| {
            ui.weak("Click: move playhead · Drag: pick the part to keep · Drag edges or ◆: trim and fades · Scroll: zoom · Right-drag: pan");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Keep these changes (Enter)").clicked() {
                    outcome = self.done();
                }
                if ui.button("Cancel").on_hover_text("Discard the changes made since opening the editor").clicked() {
                    outcome = Outcome::Cancelled;
                }
            });
        });

        if self.edits != before {
            let now = ui.input(|i| i.time);
            if now - self.last_change > UNDO_GAP {
                self.undo.push(before);
                if self.undo.len() > HISTORY {
                    self.undo.remove(0);
                }
                self.redo.clear();
            }
            self.last_change = now;
            self.replay |= self.player.is_playing();
        }
        if self.replay && !ui.input(|i| i.pointer.any_down()) {
            self.replay = false;
            if self.player.is_playing() {
                self.play_from(self.position());
            }
        }
        outcome
    }

    fn x_to_frame(&self, rect: egui::Rect, x: f32) -> f64 {
        self.view_start + ((x - rect.left()) / rect.width()) as f64 * self.view_len
    }

    fn frame_to_x(&self, rect: egui::Rect, f: f64) -> f32 {
        rect.left() + ((f - self.view_start) / self.view_len) as f32 * rect.width()
    }

    fn clamp_view(&mut self) {
        let total = self.total.max(1) as f64;
        self.view_len = self.view_len.clamp(64.0f64.min(total), total);
        self.view_start = self.view_start.clamp(0.0, total - self.view_len);
    }

    fn waveform(&mut self, ui: &mut egui::Ui, full: egui::Rect, resp: &egui::Response) {
        let ruler = egui::Rect::from_min_max(full.min, egui::pos2(full.right(), full.top() + RULER));
        let rect = egui::Rect::from_min_max(egui::pos2(full.left(), ruler.bottom()), full.max);
        let rate = self.rate();
        let plan = self.plan();
        let (kept_a, kept_b) = self.kept();
        let kept_len = kept_b - kept_a;

        // Zoom about the pointer, pan with horizontal scroll.
        if let Some(pos) = resp.hover_pos() {
            let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            let factor = (-scroll.y as f64 / 200.0).exp() / zoom as f64;
            if factor != 1.0 {
                let anchor = self.x_to_frame(rect, pos.x);
                self.view_len *= factor;
                self.clamp_view();
                self.view_start = anchor - ((pos.x - rect.left()) / rect.width()) as f64 * self.view_len;
            }
            if scroll.x != 0.0 {
                self.view_start -= scroll.x as f64 / rect.width() as f64 * self.view_len;
            }
            self.clamp_view();
        }

        // Handles: trim edges, and fade grips along the top.
        let x_start = self.frame_to_x(rect, kept_a as f64);
        let x_end = self.frame_to_x(rect, kept_b as f64);
        let x_fade_in = self.frame_to_x(rect, (kept_a + plan.fade_in.min(kept_len)) as f64);
        let x_fade_out = self.frame_to_x(rect, kept_b.saturating_sub(plan.fade_out.min(kept_len)) as f64);
        let grip_y = rect.top() + 8.0;
        let grip = |x: f32| egui::pos2(x, grip_y);
        let hit = |pos: egui::Pos2| -> Option<Drag> {
            if pos.distance(grip(x_fade_in)) < GRAB + 2.0 {
                Some(Drag::FadeIn)
            } else if pos.distance(grip(x_fade_out)) < GRAB + 2.0 {
                Some(Drag::FadeOut)
            } else if (pos.x - x_start).abs() < GRAB {
                Some(Drag::Start)
            } else if (pos.x - x_end).abs() < GRAB {
                Some(Drag::End)
            } else {
                None
            }
        };
        if let Some(pos) = resp.hover_pos() {
            match self.drag.or_else(|| hit(pos)) {
                Some(Drag::Start | Drag::End | Drag::FadeIn | Drag::FadeOut) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal),
                Some(Drag::Pan) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing),
                _ => {}
            }
        }
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let secondary = ui.input(|i| i.pointer.secondary_down() || i.pointer.middle_down());
            let at = self.x_to_frame(rect, pos.x).clamp(0.0, self.total as f64) as u64;
            self.drag = Some(if secondary { Drag::Pan } else { hit(pos).unwrap_or(Drag::New(at)) });
            self.before_drag = self.edits;
        }
        if let Some(drag) = self.drag
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let f = self.x_to_frame(rect, pos.x).clamp(0.0, self.total as f64);
            let secs = f / rate;
            let (a, b) = (kept_a as f64 / rate, kept_b as f64 / rate);
            match drag {
                Drag::Start => self.set_trim(secs.min(b - 1.0 / rate), b),
                Drag::End => self.set_trim(a, secs.max(a + 1.0 / rate)),
                Drag::FadeIn => {
                    self.edits.fade_in = (secs - a).max(0.0);
                    self.clamp_fades();
                }
                Drag::FadeOut => {
                    self.edits.fade_out = (b - secs).clamp(0.0, (b - a - self.edits.fade_in).max(0.0));
                }
                Drag::New(anchor) => {
                    let anchor = anchor as f64 / rate;
                    self.set_trim(anchor.min(secs), anchor.max(secs));
                }
                Drag::Pan => {
                    self.view_start -= resp.drag_delta().x as f64 / rect.width() as f64 * self.view_len;
                    self.clamp_view();
                }
            }
        }
        if resp.drag_stopped() {
            if let Some(Drag::New(anchor)) = self.drag {
                let (a, b) = self.kept();
                if self.frame_to_x(rect, b as f64) - self.frame_to_x(rect, a as f64) < 3.0 {
                    // Barely moved: that was a click, so move the playhead instead.
                    self.edits = self.before_drag;
                    self.seek(anchor);
                } else {
                    self.cursor = a;
                }
            }
            self.drag = None;
        }
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let f = self.x_to_frame(rect, pos.x).clamp(0.0, self.total as f64) as u64;
            self.seek(f);
        }
        if resp.double_clicked() {
            self.set_trim(0.0, self.duration());
        }

        // Re-read after interaction so this frame draws the new state.
        let plan = self.plan();
        let (kept_a, kept_b) = self.kept();
        let kept_len = kept_b - kept_a;
        let visuals = ui.visuals().clone();
        let painter = ui.painter_at(full);
        painter.rect_filled(rect, 2.0, visuals.extreme_bg_color);

        // Ruler.
        let step = nice_step(self.view_len / rate / (rect.width() as f64 / 90.0));
        let first = (self.view_start / rate / step).floor() * step;
        let mut t = first;
        while t * rate <= self.view_start + self.view_len {
            let x = self.frame_to_x(rect, t * rate);
            if x >= rect.left() {
                painter.line_segment([egui::pos2(x, ruler.bottom() - 5.0), egui::pos2(x, ruler.bottom())], visuals.widgets.noninteractive.fg_stroke);
                painter.text(egui::pos2(x + 3.0, ruler.top()), egui::Align2::LEFT_TOP, fmt_ruler(t, step), egui::FontId::monospace(10.0), visuals.weak_text_color());
            }
            t += step;
        }

        // Waveform, scaled by the fades so they're visible as they'll sound.
        let ch = self.info.channels as usize;
        let mid = rect.center().y;
        let half = rect.height() / 2.0 - 4.0;
        let accent = egui::Color32::from_rgb(WAVE_RGB[0], WAVE_RGB[1], WAVE_RGB[2]);
        let dim = visuals.weak_text_color().gamma_multiply(0.6);
        let width = rect.width().max(1.0) as usize;
        let per_px = self.view_len / width as f64;
        if per_px < 1.0 {
            // Closer than one frame per pixel: join the samples instead.
            let a = self.view_start.floor() as u64;
            let b = ((self.view_start + self.view_len).ceil() as u64 + 1).min(self.total);
            let points: Vec<egui::Pos2> = (a..b)
                .map(|f| {
                    let s = self.samples[f as usize * ch..f as usize * ch + ch].iter().sum::<f32>() / ch as f32;
                    let g = if (kept_a..kept_b).contains(&f) { plan.gain(f - kept_a, kept_len) } else { 1.0 };
                    egui::pos2(self.frame_to_x(rect, f as f64 + 0.5), mid - s * g * half)
                })
                .collect();
            painter.add(egui::Shape::line(points, egui::Stroke::new(1.5, accent)));
        } else {
            for px in 0..width {
                let a = (self.view_start + px as f64 * per_px) as u64;
                let b = (self.view_start + (px + 1) as f64 * per_px) as u64;
                if a >= self.total {
                    break;
                }
                let (lo, hi) = self.peaks.range(&self.samples, ch, a as usize, b as usize);
                let centre = (a + b) / 2;
                let inside = (kept_a..kept_b).contains(&centre);
                let g = if inside { plan.gain(centre - kept_a, kept_len) } else { 1.0 };
                let x = rect.left() + px as f32 + 0.5;
                let (top, bottom) = (mid - hi * g * half, mid - lo * g * half);
                painter.line_segment([egui::pos2(x, top.min(mid - 0.5)), egui::pos2(x, bottom.max(mid + 0.5))], egui::Stroke::new(1.0, if inside { accent } else { dim }));
            }
        }
        painter.line_segment([egui::pos2(rect.left(), mid), egui::pos2(rect.right(), mid)], egui::Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.3)));

        // Shade what's cut, then the handles.
        let x_start = self.frame_to_x(rect, kept_a as f64);
        let x_end = self.frame_to_x(rect, kept_b as f64);
        let shade = egui::Color32::from_black_alpha(if visuals.dark_mode { 110 } else { 50 });
        if x_start > rect.left() {
            painter.rect_filled(egui::Rect::from_min_max(rect.min, egui::pos2(x_start.min(rect.right()), rect.bottom())), 0.0, shade);
        }
        if x_end < rect.right() {
            painter.rect_filled(egui::Rect::from_min_max(egui::pos2(x_end.max(rect.left()), rect.top()), rect.max), 0.0, shade);
        }
        let edge = egui::Stroke::new(2.0, visuals.selection.stroke.color);
        for x in [x_start, x_end] {
            painter.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], edge);
        }
        // Fade envelopes and their grips.
        let env_y = |g: f32| rect.top() + 8.0 + (1.0 - g) * (rect.height() - 16.0);
        let env = egui::Stroke::new(1.5, visuals.warn_fg_color);
        for (from, to) in [(kept_a, kept_a + plan.fade_in.min(kept_len)), (kept_b.saturating_sub(plan.fade_out.min(kept_len)), kept_b)] {
            if to > from {
                let points: Vec<egui::Pos2> = (0..=48)
                    .map(|k| {
                        let f = from + (to - from) * k / 48;
                        egui::pos2(self.frame_to_x(rect, f as f64), env_y(plan.gain((f - kept_a).min(kept_len - 1), kept_len)))
                    })
                    .collect();
                painter.add(egui::Shape::line(points, env));
            }
        }
        let fade_in_x = self.frame_to_x(rect, (kept_a + (self.edits.fade_in * rate) as u64).min(kept_b) as f64);
        let fade_out_x = self.frame_to_x(rect, kept_b.saturating_sub((self.edits.fade_out * rate) as u64).max(kept_a) as f64);
        for x in [fade_in_x, fade_out_x] {
            let c = egui::pos2(x, rect.top() + 8.0);
            painter.add(egui::Shape::convex_polygon(
                vec![c + egui::vec2(0.0, -5.0), c + egui::vec2(5.0, 0.0), c + egui::vec2(0.0, 5.0), c + egui::vec2(-5.0, 0.0)],
                visuals.warn_fg_color,
                egui::Stroke::NONE,
            ));
        }
        // Playhead.
        let x = self.frame_to_x(rect, self.position() as f64);
        painter.line_segment([egui::pos2(x, ruler.top()), egui::pos2(x, rect.bottom())], egui::Stroke::new(1.5, visuals.strong_text_color()));
        // Keep the playhead in view while playing.
        if self.player.is_playing() && self.drag.is_none() && (x < rect.left() || x > rect.right()) {
            self.view_start = self.position() as f64 - self.view_len * 0.1;
            self.clamp_view();
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let (a, b) = self.kept();
        let rate = self.rate();
        ui.horizontal_wrapped(|ui| {
            ui.strong("Trim");
            ui.label("Start");
            if let Some(t) = time_field(ui, &mut self.start_text, a as f64 / rate) {
                self.set_trim(t, b as f64 / rate);
            }
            ui.label("End");
            if let Some(t) = time_field(ui, &mut self.end_text, b as f64 / rate) {
                self.set_trim(a as f64 / rate, t);
            }
            if ui.button("⇤ Start here").on_hover_text("Start the kept part at the playhead (I)").clicked() {
                self.trim_at_playhead(true);
            }
            if ui.button("End here ⇥").on_hover_text("End the kept part at the playhead (O)").clicked() {
                self.trim_at_playhead(false);
            }
            ui.separator();
            if ui.button("✂ Trim silence").on_hover_text("Cut quiet parts at the start and end").clicked() {
                self.trim_silence();
            }
            ui.add(egui::Slider::new(&mut self.silence_db, -80.0..=-20.0).suffix(" dB").integer())
                .on_hover_text("How quiet counts as silence");
            ui.separator();
            if ui.add_enabled(self.edits.trim_start > 0.0 || self.edits.trim_end.is_some(), egui::Button::new("Keep all")).on_hover_text("Undo the trim (or double-click the waveform)").clicked() {
                self.set_trim(0.0, self.duration());
            }
        });
        ui.horizontal_wrapped(|ui| {
            let len = self.kept_secs();
            ui.strong("Fades");
            ui.label("In");
            let max_in = (len - self.edits.fade_out).max(0.0);
            ui.add(egui::Slider::new(&mut self.edits.fade_in, 0.0..=max_in.clamp(0.001, 30.0)).suffix(" s").logarithmic(true).max_decimals(2))
                .on_hover_text("Fade in from silence over this long");
            ui.label("Out");
            let max_out = (len - self.edits.fade_in).max(0.0);
            ui.add(egui::Slider::new(&mut self.edits.fade_out, 0.0..=max_out.clamp(0.001, 30.0)).suffix(" s").logarithmic(true).max_decimals(2))
                .on_hover_text("Fade out to silence over this long");
            self.clamp_fades();
            ui.separator();
            ui.weak(format!("Original {} · Result {}", fmt_time(self.duration()), fmt_time(len)));
        });
    }

    fn trim_at_playhead(&mut self, start: bool) {
        let (a, b) = self.kept();
        let p = self.position() as f64 / self.rate();
        if start {
            self.set_trim(p, b as f64 / self.rate());
        } else {
            self.set_trim(a as f64 / self.rate(), p);
        }
    }

    fn trim_silence(&mut self) {
        let threshold = 10f32.powf(self.silence_db / 20.0);
        let ch = self.info.channels as usize;
        let loud = |f: &[f32]| f.iter().any(|s| s.abs() > threshold);
        let frames: Vec<&[f32]> = self.samples.chunks_exact(ch).collect();
        let Some(first) = frames.iter().position(|f| loud(f)) else { return };
        let last = frames.iter().rposition(|f| loud(f)).unwrap_or(first);
        let pad = 0.01;
        let rate = self.rate();
        self.set_trim(first as f64 / rate - pad, (last + 1) as f64 / rate + pad);
        self.cursor = self.kept().0;
    }

    fn shortcuts(&mut self, ui: &mut egui::Ui, outcome: &mut Outcome) {
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        use egui::{Key, Modifiers};
        let pressed = |m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        if pressed(Modifiers::NONE, Key::Space) {
            self.toggle_play();
        }
        if pressed(Modifiers::NONE, Key::Home) {
            self.seek(self.kept().0);
        }
        if pressed(Modifiers::NONE, Key::End) {
            self.seek(self.kept().1.saturating_sub(1));
        }
        if pressed(Modifiers::NONE, Key::I) {
            self.trim_at_playhead(true);
        }
        if pressed(Modifiers::NONE, Key::O) {
            self.trim_at_playhead(false);
        }
        if pressed(Modifiers::NONE, Key::L) {
            self.looping = !self.looping;
            if self.player.is_playing() {
                self.play_from(self.position());
            }
        }
        if pressed(Modifiers::NONE, Key::Escape) {
            self.player.stop();
        }
        if pressed(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || pressed(Modifiers::COMMAND, Key::Y) {
            self.redo();
        }
        if pressed(Modifiers::COMMAND, Key::Z) {
            self.undo();
        }
        if pressed(Modifiers::NONE, Key::Enter) {
            *outcome = self.done();
        }
    }

    fn undo(&mut self) {
        if let Some(e) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.edits, e));
            self.last_change = f64::NEG_INFINITY;
        }
    }

    fn redo(&mut self) {
        if let Some(e) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.edits, e));
            self.last_change = f64::NEG_INFINITY;
        }
    }

    fn done(&mut self) -> Outcome {
        self.player.stop();
        let length = self.kept_secs();
        if self.edits.is_noop() {
            return Outcome::Done { edits: None, thumb: None, length };
        }
        let thumb = self.thumbnail();
        Outcome::Done { edits: Some(Edits { ops: vec![EditOp::Audio(self.edits)] }), thumb: Some(thumb), length }
    }

    /// The result's waveform, for the row.
    fn thumbnail(&self) -> egui::ColorImage {
        let [w, h] = THUMB;
        let mut img = egui::ColorImage::filled(THUMB, egui::Color32::TRANSPARENT);
        let plan = self.plan();
        let (a, b) = self.kept();
        let len = (b - a).max(1);
        let ch = self.info.channels as usize;
        let color = egui::Color32::from_rgb(WAVE_RGB[0], WAVE_RGB[1], WAVE_RGB[2]);
        let mid = h as f32 / 2.0;
        for x in 0..w {
            let f0 = a + len * x as u64 / w as u64;
            let f1 = a + len * (x as u64 + 1) / w as u64;
            let (lo, hi) = self.peaks.range(&self.samples, ch, f0 as usize, f1 as usize);
            let g = plan.gain((f0 + f1) / 2 - a, len);
            let top = (mid - hi * g * mid).floor().max(0.0) as usize;
            let bottom = (mid - lo * g * mid).ceil().min(h as f32 - 1.0) as usize;
            for y in top.min(h / 2)..=bottom.max(h / 2) {
                img[(x, y)] = color;
            }
        }
        img
    }
}

/// A time text box: shows `value` until edited; returns a newly typed value on Enter or
/// leaving the box.
fn time_field(ui: &mut egui::Ui, text: &mut String, value: f64) -> Option<f64> {
    let id = ui.next_auto_id();
    let focused = ui.memory(|m| m.has_focus(id));
    if !focused {
        *text = fmt_time(value);
    }
    let resp = ui.add(egui::TextEdit::singleline(text).desired_width(78.0).font(egui::TextStyle::Monospace))
        .on_hover_text("Type a time like 1:23.5 or 83.5");
    if resp.lost_focus() { parse_time(text) } else { None }
}

/// `m:ss.mmm`, or `h:mm:ss.mmm` from an hour.
pub fn fmt_time(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{ms:03}") } else { format!("{m}:{s:02}.{ms:03}") }
}

/// Parses `ss.s`, `m:ss.s` or `h:mm:ss.s`.
fn parse_time(s: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in s.trim().split(':') {
        let v: f64 = part.trim().parse().ok()?;
        if v.is_nan() || v < 0.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

/// The tidy step (1, 2, 5 × 10ⁿ seconds, or minute multiples) nearest above `rough`.
fn nice_step(rough: f64) -> f64 {
    const STEPS: [f64; 19] =
        [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0];
    STEPS.into_iter().find(|&s| s >= rough).unwrap_or(600.0)
}

fn fmt_ruler(t: f64, step: f64) -> String {
    let m = (t / 60.0).floor();
    let s = t - m * 60.0;
    let decimals = if step >= 1.0 { 0 } else if step >= 0.1 { 1 } else if step >= 0.01 { 2 } else { 3 };
    if m > 0.0 { format!("{m}:{s:0w$.decimals$}", w = if decimals > 0 { decimals + 3 } else { 2 }) } else { format!("{s:.decimals$}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(secs: u64) -> Session {
        let rate = 1000;
        let frames = (secs * rate) as usize;
        let mut samples = vec![0.0f32; frames * 2];
        // Loud from 1 s to 3 s, silent around it.
        for f in rate as usize..3 * rate as usize {
            samples[f * 2] = 0.5;
            samples[f * 2 + 1] = -0.5;
        }
        let info = StreamInfo { sample_rate: rate as u32, channels: 2, source_bits: Some(16), total_frames: Some(frames as u64), source_rate: rate as u32 };
        let peaks = Peaks::new(&samples, 2);
        Session::new(samples.into(), info, peaks, AudioEdits::default())
    }

    #[test]
    fn times_format_and_parse() {
        assert_eq!(fmt_time(83.5), "1:23.500");
        assert_eq!(fmt_time(3723.004), "1:02:03.004");
        assert_eq!(parse_time("1:23.5"), Some(83.5));
        assert_eq!(parse_time("83.5"), Some(83.5));
        assert_eq!(parse_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_time("x"), None);
        assert_eq!(parse_time("-1"), None);
    }

    #[test]
    fn peaks_match_a_direct_scan_at_every_level() {
        let s = session(60);
        let direct = |a: usize, b: usize| min_max(s.samples[a * 2..b * 2].iter().copied());
        for (a, b) in [(0, 100), (900, 1300), (0, 60_000), (2_000, 40_000)] {
            let (lo, hi) = s.peaks.range(&s.samples, 2, a, b);
            let (dlo, dhi) = direct(a, b);
            // Coarse levels may look a block wider, never narrower.
            assert!(lo <= dlo && hi >= dhi, "{a}..{b}");
        }
    }

    #[test]
    fn trim_silence_keeps_the_loud_part_with_a_little_room() {
        let mut s = session(5);
        s.trim_silence();
        assert!((s.edits.trim_start - 0.99).abs() < 1e-9, "{:?}", s.edits);
        assert!((s.edits.trim_end.unwrap() - 3.01).abs() < 1e-9);
    }

    #[test]
    fn trimming_to_everything_is_no_edit_and_fades_fit_the_kept_part() {
        let mut s = session(5);
        s.set_trim(1.0, 2.0);
        s.edits.fade_in = 5.0;
        s.clamp_fades();
        assert_eq!(s.edits.fade_in, 1.0);
        s.set_trim(0.0, 5.0);
        s.edits.fade_in = 0.0;
        assert!(s.edits.is_noop());
        assert!(matches!(s.done(), Outcome::Done { edits: None, .. }));
        s.set_trim(0.5, 9.0);
        let Outcome::Done { edits: Some(e), thumb: Some(t), length } = s.done() else { panic!("expected edits") };
        assert_eq!(e.audio().unwrap().trim_end, None);
        assert_eq!(t.size, THUMB);
        assert!((length - 4.5).abs() < 1e-9);
    }
}
