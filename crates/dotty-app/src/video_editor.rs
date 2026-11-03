//! The video editor: a modal for quick changes before converting a video (or a picture
//! that becomes one): trim, rotate/flip, crop, size, speed, frame rate, fades, text, and the
//! sound (mute, volume, another audio track or file). The preview shows the picked frame with
//! every picture edit applied, using the same code the conversion uses (`codecs_video::vedit`).

use crate::audio_editor::{Peaks, fmt_time, time_field};
use crate::crop_box::{self, ASPECTS, Aspect, Grab, clamp, grab_cursor, hit, paint_crop, snap};
use crate::player::{Player, Request};
use crate::range_bar;
use crate::video_worker::{Cmd, FrameQueue, Msg, Ready, STILL_SECS, Worker};
use codecs_audio::edit::decode_all;
use codecs_audio::{AudioTrack, StreamInfo};
use codecs_video::image::{self, RgbImage};
use codecs_video::vedit::{self, Overlays};
use dotty_core::{AudioEdits, Caption, CaptionFont, EditOp, Edits, Format, JobId, Region, VideoEdits, VideoText, Volume};
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

/// Result frame width that text positions and sizes are measured on.
const TEXT_SPACE: u32 = 1000;
/// Changes closer together than this (seconds) are one undo step; steps kept.
const UNDO_GAP: f64 = 0.8;
const HISTORY: usize = 100;

pub enum Outcome {
    Open,
    Cancelled,
    Done { edits: Option<Edits>, thumb: Option<egui::ColorImage> },
}

/// A text's box on the picture shown in `shown` (screen points): a close estimate from its
/// size, line count and longest line (the font isn't measured), for picking it up.
fn text_box(t: &VideoText, shown: egui::Rect) -> egui::Rect {
    let (kx, ky) = (shown.width() / t.of_width.max(1) as f32, shown.height() / t.of_height.max(1) as f32);
    let size = t.caption.size * (kx + ky) / 2.0;
    let lines = t.caption.text.lines().count().max(1);
    let longest = t.caption.text.lines().map(|l| l.chars().count()).max().unwrap_or(1).max(1);
    let half = egui::vec2(longest as f32 * size * 0.3 + 6.0, lines as f32 * size * 0.58 + 4.0);
    egui::Rect::from_center_size(egui::pos2(shown.min.x + t.caption.x * kx, shown.min.y + t.caption.y * ky), half * 2.0)
}


/// Which sound the editor is playing (and the file it came from).
#[derive(Clone, PartialEq, Debug)]
struct SoundKey {
    /// The replacement file, or the editor's own source.
    file: Option<PathBuf>,
    track: Option<u32>,
}

/// A decoded sound and its format, or why it couldn't be.
type Decoded = Result<(Arc<[f32]>, StreamInfo, Arc<Peaks>), String>;

enum Sound {
    None,
    Loading(SoundKey, mpsc::Receiver<Decoded>),
    Ready(SoundKey, Arc<[f32]>, StreamInfo, Arc<Peaks>),
    Failed(SoundKey),
}

/// A playback in progress.
struct Playback {
    run: u64,
    /// Source time it started at, and when.
    t0: f64,
    started: std::time::Instant,
    /// The sound is playing (so its position is the clock).
    audio: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    View,
    Crop,
}

pub struct VideoEditor {
    pub entry: JobId,
    title: String,
    worker: Worker,
    ready: Option<Ready>,
    error: Option<String>,
    ve: VideoEdits,
    /// The source time being previewed, and the newest one asked for.
    cursor: f64,
    asked: Option<f64>,
    /// The latest source frame (preview size, unrotated) and what it was rendered as.
    frame: Option<RgbImage>,
    rendered: Option<egui::TextureHandle>,
    dirty: bool,
    overlays: Overlays,
    tool: Tool,
    /// What a drag on the crop box is doing, and the shape it's locked to.
    crop_grab: Option<Grab>,
    aspect: Aspect,
    /// A text being dragged on the picture: its index and where the pointer holds it
    /// (relative to its centre, in the text's own units).
    text_drag: Option<(usize, egui::Vec2)>,
    selected_text: Option<usize>,
    /// Undo history: the video edits and (for a song) its sound's edits at each step.
    undo: Vec<(VideoEdits, AudioEdits)>,
    redo: Vec<(VideoEdits, AudioEdits)>,
    last_change: f64,
    /// The sound range handle being dragged.
    sound_grab: Option<range_bar::Handle>,
    /// Text boxes for the time fields (start, end, cursor, and per text start/end).
    time_texts: [String; 5],
    text_times: Vec<(String, String)>,
    /// The audio file's name, for display.
    audio_name: Option<String>,
    /// The trim handle being dragged on the timeline.
    grab: Option<range_bar::Handle>,
    /// The video's frames ran out during playback.
    video_ended: bool,
    // Playback.
    player: Player,
    sound: Sound,
    play: Option<Playback>,
    /// Playback number, so frames of an earlier run are ignored.
    run: u64,
    queue: FrameQueue,
    /// The source time of the frame on screen during playback.
    shown: Option<f64>,
    /// Play was pressed while the sound was still loading.
    pending_play: bool,
    path: PathBuf,
    from: Format,
    /// Something to tell the user (sound that couldn't be loaded or played).
    notice: Option<String>,
    /// For a song: the sound edits it will have (they set its length), and the picture
    /// the worker has loaded.
    song_edits: AudioEdits,
    loaded_picture: Option<PathBuf>,
}

impl VideoEditor {
    pub fn open(ctx: &egui::Context, entry: JobId, path: PathBuf, from: Format, existing: Option<Arc<Edits>>) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ve = existing.as_ref().and_then(|e| e.video().cloned()).unwrap_or_default();
        let song_edits = existing.as_ref().and_then(|e| e.audio().copied()).unwrap_or_default();
        let loaded_picture = ve.picture_file.clone();
        let audio_name = ve.audio_file.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        let text_times = vec![Default::default(); ve.texts.len()];
        Self {
            entry,
            title,
            worker: Worker::start(ctx, path.clone(), from, ve.picture_file.clone()),
            ready: None,
            error: None,
            cursor: ve.trim_start,
            ve,
            asked: None,
            frame: None,
            rendered: None,
            dirty: true,
            overlays: Overlays::default(),
            tool: Tool::View,
            crop_grab: None,
            aspect: Aspect::Free,
            text_drag: None,
            selected_text: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last_change: f64::NEG_INFINITY,
            sound_grab: None,
            time_texts: Default::default(),
            text_times,
            audio_name,
            grab: None,
            video_ended: false,
            player: Player::default(),
            sound: Sound::None,
            play: None,
            run: 0,
            queue: FrameQueue::default(),
            shown: None,
            pending_play: false,
            path,
            from,
            notice: None,
            song_edits,
            loaded_picture,
        }
    }

    fn picture(&self) -> bool {
        self.ready.as_ref().is_some_and(|r| r.video.is_none())
    }

    fn song(&self) -> bool {
        self.ready.as_ref().is_some_and(|r| r.song)
    }

    /// For a song: how long the video will be, after the sound edits' trim and speed.
    fn song_secs(&self) -> f64 {
        let raw = self.ready.as_ref().map_or(0.0, |r| r.duration);
        let a = self.song_edits.trim_start.clamp(0.0, raw);
        let b = self.song_edits.trim_end.unwrap_or(raw).clamp(a, raw);
        ((b - a) / self.song_edits.speed.max(0.01)).max(0.04)
    }

    /// How long a picture lasts: the part picked of its sound, else the length set.
    fn picture_secs(&self) -> f64 {
        if self.ve.audio_file.is_some() {
            return self.sound_range().map_or(STILL_SECS, |(a, b)| (b - a).max(0.04));
        }
        self.ve.still_secs.unwrap_or(STILL_SECS)
    }

    fn duration(&self) -> f64 {
        if self.song() {
            return self.song_secs();
        }
        if self.picture() {
            return self.picture_secs();
        }
        self.ready.as_ref().map_or(0.0, |r| r.duration)
    }

    fn kept(&self) -> (f64, f64) {
        let d = self.duration();
        let start = self.ve.trim_start.clamp(0.0, d);
        (start, self.ve.trim_end.unwrap_or(d).clamp(start, d))
    }

    fn result_secs(&self) -> f64 {
        if self.song() {
            return self.song_secs();
        }
        if self.picture() {
            return self.picture_secs();
        }
        let (a, b) = self.kept();
        (b - a) / self.ve.speed.clamp(VideoEdits::MIN_SPEED, VideoEdits::MAX_SPEED)
    }

    /// The whole length of the sound whose part is picked, once known.
    fn raw_sound_secs(&self) -> Option<f64> {
        if self.song() {
            return self.ready.as_ref().map(|r| r.duration);
        }
        if let (Some(key), Sound::Ready(have, samples, info, _)) = (self.sound_key(), &self.sound)
            && *have == key
        {
            return Some(samples.len() as f64 / info.channels.max(1) as f64 / info.sample_rate as f64);
        }
        None
    }

    /// The part of the sound that's used, if there's a sound to pick a part of.
    fn sound_range(&self) -> Option<(f64, f64)> {
        let raw = self.raw_sound_secs()?;
        let (start, end) = if self.song() {
            (self.song_edits.trim_start, self.song_edits.trim_end)
        } else if self.ve.audio_file.is_some() {
            (self.ve.audio_start, self.ve.audio_end)
        } else {
            return None;
        };
        let a = start.clamp(0.0, raw);
        Some((a, end.unwrap_or(raw).clamp(a, raw)))
    }

    fn set_sound_range(&mut self, a: f64, b: f64) {
        let Some(raw) = self.raw_sound_secs() else { return };
        let (start, end) = (if a < 1e-6 { 0.0 } else { a }, (b < raw - 1e-6).then_some(b));
        if self.song() {
            (self.song_edits.trim_start, self.song_edits.trim_end) = (start, end);
        } else {
            (self.ve.audio_start, self.ve.audio_end) = (start, end);
        }
    }

    fn frame_step(&self) -> f64 {
        1.0 / self.ready.as_ref().map_or(25.0, |r| r.fps).max(1.0)
    }

    /// The result's pixel size.
    fn result_size(&self) -> (u32, u32) {
        let Some(r) = &self.ready else { return (0, 0) };
        vedit::output_size(r.size, r.rotation, &self.ve)
    }

    fn change(&mut self) {
        self.dirty = true;
    }

    fn text_changed(&mut self) {
        self.overlays = Overlays::default();
        self.dirty = true;
    }

    /// Put back an earlier (or later) state of the edits, and what's derived from them.
    fn restore(&mut self, (ve, song): (VideoEdits, AudioEdits)) {
        self.ve = ve;
        self.song_edits = song;
        self.audio_name = self.ve.audio_file.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        self.text_times.resize(self.ve.texts.len(), Default::default());
        self.selected_text = self.selected_text.filter(|&i| i < self.ve.texts.len());
        self.sync_picture();
        self.last_change = f64::NEG_INFINITY;
        if self.playing() {
            self.stop_play();
        }
        self.text_changed();
    }

    /// Remember `before` as an undo step, unless it's part of one already under way (changes
    /// close together, like a drag or a slider, are one step).
    fn record(&mut self, before: (VideoEdits, AudioEdits), now: f64) {
        if now - self.last_change > UNDO_GAP {
            self.undo.push(before);
            if self.undo.len() > HISTORY {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.last_change = now;
    }

    /// A song's picture follows `ve.picture_file` (also when undo changes it).
    fn sync_picture(&mut self) {
        if self.song() && self.ve.picture_file != self.loaded_picture {
            self.loaded_picture = self.ve.picture_file.clone();
            let _ = self.worker.requests.send(Cmd::Picture(self.loaded_picture.clone()));
        }
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push((self.ve.clone(), self.song_edits));
            self.restore(prev);
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push((self.ve.clone(), self.song_edits));
            self.restore(next);
        }
    }

    // ---- sound and playback ----

    /// The sound that goes with the result, if there is one: another file's, or the video's own.
    fn sound_key(&self) -> Option<SoundKey> {
        if self.song() {
            return Some(SoundKey { file: None, track: None });
        }
        if let Some(f) = &self.ve.audio_file {
            return Some(SoundKey { file: Some(f.clone()), track: None });
        }
        (!self.ve.mute && !self.picture()).then_some(SoundKey { file: None, track: self.ve.track })
    }

    /// Load the sound for [`Self::sound_key`] in the background when it isn't the loaded one.
    /// Only done once playback is wanted (decoding a long soundtrack takes a moment and memory).
    fn ensure_sound(&mut self, ctx: &egui::Context) {
        let Some(key) = self.sound_key() else { return };
        let have = match &self.sound {
            Sound::Loading(k, _) | Sound::Ready(k, ..) | Sound::Failed(k) => *k == key,
            Sound::None => false,
        };
        if have {
            return;
        }
        let (path, from) = match &key.file {
            Some(f) => match Format::detect(f) {
                Some(fmt) if matches!(fmt.kind(), dotty_core::Kind::Audio | dotty_core::Kind::Video) => (f.clone(), fmt),
                _ => {
                    self.notice = Some("That isn't a sound or video file Dotty can read".into());
                    self.sound = Sound::Failed(key);
                    return;
                }
            },
            None => (self.path.clone(), self.from),
        };
        let (tx, rx) = mpsc::channel();
        let (track, ctx) = (key.track, ctx.clone());
        std::thread::spawn(move || {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let r = decode_all(&path, from, track, 30 * 60, &cancel)
                .map(|(samples, info)| {
                    let peaks = Arc::new(Peaks::new(&samples, info.channels as usize));
                    (samples, info, peaks)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        self.sound = Sound::Loading(key, rx);
    }

    /// Collect a finished sound load; start a waiting playback.
    fn update_sound(&mut self) {
        if let Sound::Loading(key, rx) = &self.sound {
            match rx.try_recv() {
                Ok(Ok((samples, info, peaks))) => self.sound = Sound::Ready(key.clone(), samples, info, peaks),
                Ok(Err(e)) => {
                    self.notice = Some(format!("The sound can't be played in the preview: {e}"));
                    self.sound = Sound::Failed(key.clone());
                }
                Err(mpsc::TryRecvError::Disconnected) => self.sound = Sound::Failed(key.clone()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    /// The sound edits the preview plays with: the video's own sound follows the trim, speed,
    /// fades and volume; another file starts with the result and follows fades and volume.
    fn sound_edits(&self) -> AudioEdits {
        if self.song() {
            self.song_edits
        } else if self.ve.audio_file.is_some() {
            // The part picked, and no longer than a video is.
            let longest = (!self.picture()).then(|| self.ve.audio_start + self.result_secs());
            let trim_end = match (self.ve.audio_end, longest) {
                (Some(e), Some(l)) => Some(e.min(l)),
                (e, l) => e.or(l),
            };
            AudioEdits { trim_start: self.ve.audio_start, trim_end, fade_in: self.ve.fade_in, fade_out: self.ve.fade_out, volume: Volume::Gain(self.ve.volume_db), ..Default::default() }
        } else {
            self.ve.audio()
        }
    }

    /// Source seconds the sound is at, while it plays. A replacement sound counts result time.
    fn audio_clock(&self) -> Option<f64> {
        let Sound::Ready(_, _, info, _) = &self.sound else { return None };
        if !self.player.is_playing() {
            return None;
        }
        let secs = self.player.position()? as f64 / info.sample_rate as f64;
        if self.song() {
            // The song's own time to the result's (after its trim and speed).
            return Some((secs - self.song_edits.trim_start) / self.song_edits.speed.max(0.01));
        }
        Some(if self.ve.audio_file.is_some() { self.kept().0 + (secs - self.ve.audio_start) * self.ve.speed } else { secs })
    }

    /// Source seconds now, during playback: the sound's position, else the wall clock.
    fn clock(&self, pb: &Playback) -> f64 {
        if pb.audio
            && let Some(t) = self.audio_clock()
        {
            return t;
        }
        pb.t0 + pb.started.elapsed().as_secs_f64() * self.ve.speed
    }

    fn playing(&self) -> bool {
        self.play.is_some() || self.pending_play
    }

    fn stop_play(&mut self) {
        self.player.stop();
        self.play = None;
        self.pending_play = false;
        self.queue.clear();
        self.shown = None;
        self.video_ended = false;
        // Show the exact frame at the playhead again.
        self.asked = None;
    }

    fn toggle_play(&mut self, ctx: &egui::Context) {
        if self.playing() {
            self.stop_play();
        } else {
            self.start_play(ctx, self.cursor);
        }
    }

    fn start_play(&mut self, ctx: &egui::Context, t: f64) {
        let (a, b) = self.kept();
        let t = if t >= b - self.frame_step() || t < a { a } else { t };
        self.cursor = t;
        self.ensure_sound(ctx);
        // Wait for the sound rather than starting the picture ahead of it.
        if matches!(self.sound, Sound::Loading(..)) && self.sound_key().is_some() {
            self.pending_play = true;
            return;
        }
        self.pending_play = false;
        self.notice = None;
        self.player.stop();
        let mut audio = false;
        if let (Some(key), Sound::Ready(have, samples, info, _)) = (self.sound_key(), &self.sound)
            && *have == key
        {
            let rate = info.sample_rate as f64;
            // Where in the sound's own file to start.
            let from = if self.song() {
                (self.song_edits.trim_start + t * self.song_edits.speed) * rate
            } else if self.ve.audio_file.is_some() {
                (self.ve.audio_start + (t - a) / self.ve.speed) * rate
            } else {
                t * rate
            };
            let req = Request { samples: samples.clone(), info: info.clone(), edits: self.sound_edits(), measured: None, from: from.max(0.0) as u64, looping: false };
            match self.player.play(req) {
                Ok(()) => audio = true,
                Err(e) => self.notice = Some(format!("No sound in the preview: {e}")),
            }
        }
        self.run += 1;
        self.queue.clear();
        self.shown = None;
        self.video_ended = false;
        if !self.picture() {
            let _ = self.worker.requests.send(Cmd::Play(t, self.run, self.ve.speed));
        }
        self.play = Some(Playback { run: self.run, t0: t, started: std::time::Instant::now(), audio });
    }

    /// Start a waiting playback; during playback show the frame due now and stop at the end.
    fn update_playback(&mut self, ctx: &egui::Context) {
        if self.pending_play && !matches!(self.sound, Sound::Loading(..)) {
            self.start_play(ctx, self.cursor);
        }
        if let Some(e) = self.player.take_error() {
            self.notice = Some(format!("The sound stopped: {e}"));
        }
        let Some(pb) = &self.play else { return };
        let (run, audio) = (pb.run, pb.audio);
        while let Ok((g, pts, img)) = self.worker.play.try_recv() {
            if g == run {
                self.queue.push(pts, img);
            }
        }
        let clock = self.clock(self.play.as_ref().expect("checked"));
        let (a, b) = self.kept();
        if let Some((pts, img)) = self.queue.at(clock)
            && self.shown != Some(pts)
        {
            let img = img.clone();
            self.shown = Some(pts);
            self.frame = Some(img);
            self.dirty = true;
        }
        self.cursor = clock.clamp(a, b);
        self.dirty = true;
        // The end: the kept part is over, or (a picture) its sound finished.
        let sound_over = audio && !self.player.is_playing() && clock > a + 0.2;
        let over = if self.picture() { sound_over || (!audio && clock >= self.result_secs() - 1e-3) } else { clock >= b - 1e-3 || (self.video_ended && self.queue.frames.len() <= 1 && clock >= b - self.frame_step() * 2.0) };
        if over {
            let end = self.cursor;
            self.stop_play();
            self.cursor = end.min((b - self.frame_step()).max(a));
            return;
        }
        ctx.request_repaint();
    }

    /// Apply what the worker has sent.
    fn drain_replies(&mut self) {
        while let Ok(m) = self.worker.replies.try_recv() {
            match m {
                Msg::Ready(Ok(r)) => {
                    // Keep the saved range inside the real duration.
                    self.cursor = self.cursor.clamp(0.0, r.duration);
                    self.ready = Some(r);
                }
                Msg::Ready(Err(e)) => self.error = Some(e),
                Msg::Failed(e) => {
                    self.stop_play();
                    self.notice = Some(e);
                }
                Msg::Picture(size, img) => {
                    if let Some(r) = &mut self.ready {
                        r.size = size;
                    }
                    // The crop was drawn on the old picture.
                    self.ve.crop = None;
                    self.frame = Some(img);
                    self.dirty = true;
                }
                Msg::Frame(img) => {
                    // A still requested before playback began must not replace its frames.
                    if self.play.is_none() {
                        self.frame = Some(img);
                        self.dirty = true;
                    }
                }
                Msg::Ended(run) => {
                    if self.play.as_ref().is_some_and(|p| p.run == run) {
                        self.video_ended = true;
                    }
                }
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        self.drain_replies();
        self.update_sound();
        self.update_playback(ctx);
        if self.ready.is_some() && self.play.is_none() && self.asked != Some(self.cursor) {
            self.asked = Some(self.cursor);
            let _ = self.worker.requests.send(Cmd::Seek(self.cursor));
        }
        if self.dirty {
            self.render(ctx);
        }
        let screen = ctx.content_rect();
        let size = (screen.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(480.0, 360.0));
        let mut outcome = Outcome::Open;
        egui::Modal::new(egui::Id::unique("video-editor")).show(ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            outcome = self.ui(ui);
        });
        outcome
    }

    /// Draw the preview: the frame with the picture edits (everything but the crop while the
    /// crop tool is open, so the box can be drawn on the whole picture).
    fn render(&mut self, ctx: &egui::Context) {
        self.dirty = false;
        let (Some(frame), Some(r)) = (&self.frame, &self.ready) else { return };
        let t_out = ((self.cursor - self.kept().0) / self.ve.speed.max(0.01)).max(0.0);
        let img = if self.tool == Tool::Crop {
            let uncropped = VideoEdits { crop: None, max_side: None, ..self.ve.clone() };
            vedit::geometry(frame.clone(), r.rotation, &uncropped)
        } else {
            let mut img = vedit::geometry(frame.clone(), r.rotation, &self.ve);
            let secs = self.result_secs();
            self.overlays.apply(&mut img, &self.ve, t_out, secs);
            img
        };
        let color = egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw());
        self.rendered = Some(ctx.load_texture("video-preview", color, egui::TextureOptions::LINEAR));
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Outcome {
        let mut outcome = Outcome::Open;
        // Undo and redo first, so what they change isn't taken for a new edit below.
        let typing = ui.ctx().egui_wants_keyboard_input();
        let (undo_key, redo_key) = ui.input_mut(|i| {
            use egui::{Key, Modifiers};
            let redo = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y);
            (!redo && i.consume_key(Modifiers::COMMAND, Key::Z), redo)
        });
        if !typing && undo_key {
            self.undo();
        }
        if !typing && redo_key {
            self.redo();
        }
        // A sound to pick a part of is shown as soon as it's there, not only at Play.
        self.ensure_sound(&ui.ctx().clone());
        ui.horizontal_wrapped(|ui| {
            ui.strong(&self.title);
            if let Some(r) = &self.ready {
                ui.separator();
                let (w, h) = self.result_size();
                ui.weak(match &r.video {
                    Some(v) => format!("{} · {}×{} → {w}×{h}", v.codec.name(), r.size.0, r.size.1),
                    None if r.song => format!("song's picture {}×{} → {w}×{h}", r.size.0, r.size.1),
                    None => format!("picture {}×{} → {w}×{h}", r.size.0, r.size.1),
                });
            }
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("↶")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                self.undo();
            }
            if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("↷")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked() {
                self.redo();
            }
        });
        let before = (self.ve.clone(), self.song_edits);
        if self.ready.is_none() {
            match &self.error {
                Some(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, format!("✖ Couldn't open this for editing: {e}"));
                    if ui.button("Close").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                }
                None => {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
            }
            return outcome;
        }
        let controls = 250.0;
        let area = ui.available_size() - egui::vec2(0.0, controls);
        let (rect, resp) = ui.allocate_exact_size(area.max(egui::vec2(120.0, 120.0)), egui::Sense::click_and_drag());
        self.canvas(ui, rect, &resp);
        ui.add_space(4.0);
        let cursor_before = self.cursor;
        if !self.picture() {
            self.timeline(ui);
        } else {
            ui.horizontal(|ui| {
                let can_play = self.sound_key().is_some();
                let label = if self.playing() { "⏸ Pause" } else { "▶ Play" };
                if ui.add_enabled(can_play, egui::Button::new(label)).on_hover_text("Play the picture with its sound (Space)").clicked() {
                    self.toggle_play(&ui.ctx().clone());
                }
                if !can_play {
                    ui.weak("Add a sound below to play it with the picture");
                }
            });
        }
        // Moving the playhead while playing carries on from the new place.
        if self.play.is_some() && (self.cursor - cursor_before).abs() > 1e-9 {
            let t = self.cursor;
            self.start_play(&ui.ctx().clone(), t);
        }
        egui::ScrollArea::vertical().id_salt("video-controls").max_height(controls - 60.0).show(ui, |ui| {
            if !self.picture() {
                self.trim_controls(ui);
            }
            if self.song() {
                self.sound_range_bar(ui);
            }
            self.picture_controls(ui);
            if !self.picture() {
                self.time_controls(ui);
            }
            self.text_controls(ui);
            // A song's sound is edited in the sound editor (Edit on its row).
            if !self.song() {
                self.sound_controls(ui);
            }
        });
        // A crop or turn can change the picture's shape: texts follow (kept in place).
        let size = self.result_size();
        if size.0 > 0 && vedit::fit_texts(&mut self.ve, size) {
            self.text_changed();
        }
        if self.ve != before.0 || self.song_edits != before.1 {
            self.change();
            let now = ui.input(|i| i.time);
            self.record(before, now);
            self.sync_picture();
            // A change mid-playback would leave the sound and picture out of step: pause.
            if self.playing() {
                self.stop_play();
            }
        }
        if !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::Space)) && (!self.picture() || self.sound_key().is_some()) {
            self.toggle_play(&ui.ctx().clone());
        }
        ui.horizontal(|ui| {
            let (w, h) = self.result_size();
            let secs = if self.picture() && self.ve.audio_file.is_some() { "as long as the sound".to_string() } else { fmt_time(self.result_secs()) };
            ui.weak(format!("Result: {w}×{h}, {secs}"));
            if let Some(n) = &self.notice {
                ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {n}"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Keep these changes").clicked() {
                    outcome = self.done();
                }
                if ui.button("Cancel").on_hover_text("Discard the changes made since opening the editor").clicked() {
                    outcome = Outcome::Cancelled;
                }
                if ui.add_enabled(!self.ve.is_noop(), egui::Button::new("Reset all")).clicked() {
                    self.ve = VideoEdits::default();
                    self.audio_name = None;
                    self.text_times.clear();
                    self.selected_text = None;
                    self.tool = Tool::View;
                    self.text_changed();
                }
            });
        });
        outcome
    }

    /// The preview; the crop box with its handles while the crop tool is on, and otherwise
    /// the texts showing now, which can be dragged.
    fn canvas(&mut self, ui: &mut egui::Ui, rect: egui::Rect, resp: &egui::Response) {
        ui.painter().rect_filled(rect, 4.0, egui::Color32::BLACK);
        let Some(tex) = self.rendered.clone() else { return };
        let s = tex.size_vec2();
        let k = (rect.width() / s.x).min(rect.height() / s.y);
        let shown = egui::Rect::from_center_size(rect.center(), s * k);
        ui.painter().image(tex.id(), shown, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        match self.tool {
            Tool::Crop => self.crop_canvas(ui, resp, shown, s),
            Tool::View => self.text_canvas(ui, resp, shown),
        }
    }

    /// Draw, move or resize the crop box on the whole (uncropped) picture `s` pixels large.
    fn crop_canvas(&mut self, ui: &mut egui::Ui, resp: &egui::Response, shown: egui::Rect, s: egui::Vec2) {
        let k = shown.width() / s.x;
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, s);
        let to_screen = |r: egui::Rect| egui::Rect::from_min_max(shown.min + r.min.to_vec2() * k, shown.min + r.max.to_vec2() * k);
        let to_image = |p: egui::Pos2| ((p - shown.min) / k).to_pos2();
        let current = match &self.ve.crop {
            Some(c) => {
                let (x, y, w, h) = c.scaled(s.x as u32, s.y as u32);
                egui::Rect::from_min_size(egui::pos2(x as f32, y as f32), egui::vec2(w as f32, h as f32))
            }
            None => bounds,
        };
        let ratio = self.aspect.ratio(s);
        let locked = ratio.is_some();
        let box_screen = to_screen(current);
        if let Some(pos) = resp.hover_pos()
            && self.crop_grab.is_none()
        {
            ui.ctx().set_cursor_icon(grab_cursor(hit(box_screen, pos, locked)));
        }
        if resp.double_clicked() {
            self.ve.crop = None;
        } else if resp.drag_started_by(egui::PointerButton::Primary)
            && let Some(pos) = resp.interact_pointer_pos()
        {
            self.crop_grab = Some(match hit(box_screen, pos, locked) {
                (0, 0, true) => Grab::Move { start: current, from: to_image(pos) },
                (0, 0, false) => Grab::New(clamp(to_image(pos), bounds)),
                (dx, dy, _) => Grab::Edges { start: current, dx, dy },
            });
        }
        let mut shown_box = box_screen;
        if let Some(grab) = self.crop_grab
            && let Some(pos) = resp.interact_pointer_pos()
        {
            let r = snap(crop_box::dragged(grab, to_image(pos), bounds, ratio), bounds);
            if r.width() >= 2.0 && r.height() >= 2.0 {
                self.ve.crop = (r != bounds).then(|| Region { x: r.min.x as u32, y: r.min.y as u32, width: r.width() as u32, height: r.height() as u32, of_width: s.x as u32, of_height: s.y as u32 });
                shown_box = to_screen(r);
            }
            if let Grab::Move { .. } = grab {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
        }
        if resp.drag_stopped() {
            self.crop_grab = None;
        }
        paint_crop(ui.painter(), shown, shown_box, true, locked);
    }

    /// Texts showing at the playhead can be picked up and moved on the picture.
    fn text_canvas(&mut self, ui: &mut egui::Ui, resp: &egui::Response, shown: egui::Rect) {
        let t_out = ((self.cursor - self.kept().0) / self.ve.speed.max(0.01)).max(0.0);
        let boxes: Vec<(usize, egui::Rect)> = self
            .ve
            .texts
            .iter()
            .enumerate()
            .filter(|(_, t)| t_out >= t.start && t.end.is_none_or(|e| t_out < e))
            .map(|(i, t)| (i, text_box(t, shown)))
            .collect();
        let over = resp.hover_pos().and_then(|p| boxes.iter().rev().find(|b| b.1.contains(p)).map(|b| b.0));
        if self.text_drag.is_none() && over.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if resp.drag_started_by(egui::PointerButton::Primary)
            && let Some(pos) = resp.interact_pointer_pos()
            && let Some(&(i, _)) = boxes.iter().rev().find(|b| b.1.contains(pos))
        {
            let t = &self.ve.texts[i];
            let (kx, ky) = (shown.width() / t.of_width as f32, shown.height() / t.of_height as f32);
            let at = egui::vec2((pos.x - shown.min.x) / kx, (pos.y - shown.min.y) / ky);
            self.text_drag = Some((i, at - egui::vec2(t.caption.x, t.caption.y)));
            self.selected_text = Some(i);
        }
        if let Some((i, held)) = self.text_drag {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if let Some(pos) = resp.interact_pointer_pos()
                && let Some(t) = self.ve.texts.get_mut(i)
            {
                let (kx, ky) = (shown.width() / t.of_width as f32, shown.height() / t.of_height as f32);
                let at = egui::vec2((pos.x - shown.min.x) / kx, (pos.y - shown.min.y) / ky) - held;
                t.caption.x = at.x.clamp(0.0, t.of_width as f32);
                t.caption.y = at.y.clamp(0.0, t.of_height as f32);
                self.text_changed();
            }
            if resp.drag_stopped() {
                self.text_drag = None;
            }
        }
        for &(i, r) in &boxes {
            if over == Some(i) || self.selected_text == Some(i) || self.text_drag.is_some_and(|d| d.0 == i) {
                ui.painter().rect_stroke(r, 2.0, egui::Stroke::new(1.5, ui.visuals().selection.stroke.color), egui::StrokeKind::Outside);
            }
        }
    }

    /// Playhead and trim handles on one bar.
    fn timeline(&mut self, ui: &mut egui::Ui) {
        let d = self.duration().max(1e-6);
        let step = self.frame_step();
        ui.horizontal(|ui| {
            let label = if self.playing() { "⏸ Pause" } else { "▶ Play" };
            if ui.button(label).on_hover_text("Play with sound (Space)").clicked() {
                self.toggle_play(&ui.ctx().clone());
            }
            if ui.small_button("◀").on_hover_text("Previous frame (Left arrow)").clicked() {
                self.cursor = (self.cursor - step).max(0.0);
                self.change();
            }
            if ui.small_button("▶").on_hover_text("Next frame (Right arrow)").clicked() {
                self.cursor = (self.cursor + step).min((d - step).max(0.0));
                self.change();
            }
            if let Some(t) = time_field(ui, &mut self.time_texts[2], self.cursor) {
                self.cursor = t.clamp(0.0, d);
                self.change();
            }
            let (a, b) = self.kept();
            let width = ui.available_width();
            let out = range_bar::show(ui, width, d, Some((a, b)), Some(self.cursor), step, &mut self.grab, |_, _| {});
            if out.range != (a, b) {
                self.ve.trim_start = if out.range.0 < 1e-6 { 0.0 } else { out.range.0 };
                self.ve.trim_end = (out.range.1 < d - 1e-6).then_some(out.range.1);
                self.change();
            }
            if let Some(t) = out.playhead {
                self.cursor = t;
                self.change();
            }
        });
        if !ui.ctx().egui_wants_keyboard_input() {
            let (left, right) = ui.input(|i| (i.key_pressed(egui::Key::ArrowLeft), i.key_pressed(egui::Key::ArrowRight)));
            if left {
                self.cursor = (self.cursor - step).max(0.0);
                self.change();
            }
            if right {
                self.cursor = (self.cursor + step).min((d - step).max(0.0));
                self.change();
            }
        }
    }

    /// The sound's waveform with its range: the part used (a song's, a video's replacement
    /// sound, or the sound that sets a picture's length).
    fn sound_range_bar(&mut self, ui: &mut egui::Ui) {
        let (Some((a, b)), Some(raw)) = (self.sound_range(), self.raw_sound_secs()) else {
            if self.sound_key().is_some() && !matches!(self.sound, Sound::Failed(_)) {
                ui.weak("Loading the sound…");
            }
            return;
        };
        let wave = match (&self.sound, self.sound_key()) {
            (Sound::Ready(have, samples, info, peaks), Some(key)) if *have == key => Some((samples.clone(), info.channels.max(1) as usize, info.sample_rate as f64, peaks.clone())),
            _ => None,
        };
        let what = if self.picture() && !self.song() { "Sound (sets how long the video is)" } else { "Sound used" };
        ui.horizontal_wrapped(|ui| {
            ui.strong(what);
            ui.label("From");
            if let Some(t) = time_field(ui, &mut self.time_texts[3], a) {
                self.set_sound_range(t.clamp(0.0, b), b);
            }
            ui.label("to");
            if let Some(t) = time_field(ui, &mut self.time_texts[4], b) {
                self.set_sound_range(a, t.clamp(a, raw));
            }
            if ui.add_enabled(a > 0.0 || b < raw - 1e-6, egui::Button::new("Whole sound")).clicked() {
                self.set_sound_range(0.0, raw);
            }
            ui.weak(format!("of {}", fmt_time(raw)));
        });
        let width = ui.available_width();
        let out = range_bar::show(ui, width, raw, Some((a, b)), None, 0.1_f64.min(raw), &mut self.sound_grab, |painter, track| {
            let Some((samples, ch, rate, peaks)) = &wave else { return };
            let frames = (raw * rate) as usize;
            let mid = track.center().y;
            let half = track.height() * 0.45;
            let colour = egui::Color32::from_gray(150);
            let columns = track.width().max(1.0) as usize;
            for x in 0..columns {
                let f0 = x * frames / columns;
                let f1 = ((x + 1) * frames / columns).max(f0 + 1);
                let (lo, hi) = peaks.range(samples, *ch, f0, f1);
                let px = track.left() + x as f32 + 0.5;
                painter.line_segment([egui::pos2(px, mid - hi.clamp(-1.0, 1.0) * half), egui::pos2(px, mid - lo.clamp(-1.0, 1.0) * half)], egui::Stroke::new(1.0, colour));
            }
        });
        if out.range != (a, b) {
            self.set_sound_range(out.range.0, out.range.1);
        }
    }

    fn trim_controls(&mut self, ui: &mut egui::Ui) {
        let d = self.duration();
        let (a, b) = self.kept();
        ui.horizontal_wrapped(|ui| {
            ui.strong("Trim");
            ui.label("Start");
            if let Some(t) = time_field(ui, &mut self.time_texts[0], a) {
                self.ve.trim_start = t.clamp(0.0, d);
            }
            ui.label("End");
            if let Some(t) = time_field(ui, &mut self.time_texts[1], b) {
                self.ve.trim_end = Some(t.clamp(self.ve.trim_start, d));
            }
            if ui.button("⇤ Start here").on_hover_text("Start at the frame on screen").clicked() {
                self.ve.trim_start = self.cursor.min(b);
            }
            if ui.button("End here ⇥").on_hover_text("End at the frame on screen").clicked() {
                self.ve.trim_end = Some(self.cursor.max(a));
            }
            if ui.add_enabled(self.ve.trim_start > 0.0 || self.ve.trim_end.is_some(), egui::Button::new("Keep all")).clicked() {
                self.ve.trim_start = 0.0;
                self.ve.trim_end = None;
            }
        });
    }

    fn picture_controls(&mut self, ui: &mut egui::Ui) {
        let size = self.ready.as_ref().map_or((0, 0), |r| r.size);
        let longest = size.0.max(size.1).max(64);
        ui.horizontal_wrapped(|ui| {
            ui.strong("Picture");
            if self.picture() && !self.song() && self.ve.audio_file.is_none() {
                let mut secs = self.ve.still_secs.unwrap_or(STILL_SECS);
                if ui.add(egui::DragValue::new(&mut secs).range(0.5..=3600.0).speed(0.1).prefix("Length ").suffix(" s")).on_hover_text("How long the video lasts (add a sound to use its length instead)").changed() {
                    self.ve.still_secs = ((secs - STILL_SECS).abs() > 1e-6).then_some(secs);
                }
                ui.separator();
            }
            if self.song() {
                if ui.button("🖼 Choose…").on_hover_text("Show this picture with the song instead of its cover art").clicked()
                    && let Some(file) = rfd::FileDialog::new()
                        .set_title("Pick a picture to show with the song")
                        .add_filter("Pictures", &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "avif", "jxl", "qoi", "tga", "svg"])
                        .pick_file()
                {
                    self.ve.picture_file = Some(file);
                }
                if ui.add_enabled(self.ve.picture_file.is_some(), egui::Button::new("Cover art")).on_hover_text("Go back to the song's own cover art (or a plain background)").clicked() {
                    self.ve.picture_file = None;
                }
                ui.separator();
            }
            if ui.button("⟲").on_hover_text("Rotate a quarter turn left").clicked() {
                self.ve.turn_view(false);
            }
            if ui.button("⟳").on_hover_text("Rotate a quarter turn right").clicked() {
                self.ve.turn_view(true);
            }
            // The crop box follows the picture through turns and flips.
            let (mut h, mut v) = (self.ve.flip_h, self.ve.flip_v);
            let h_clicked = ui.toggle_value(&mut h, "⇋").on_hover_text("Flip left to right").clicked();
            let v_clicked = ui.toggle_value(&mut v, "⇅").on_hover_text("Flip top to bottom").clicked();
            if h_clicked {
                self.ve.flip_view(true);
            }
            if v_clicked {
                self.ve.flip_view(false);
            }
            ui.separator();
            let mut crop = self.tool == Tool::Crop;
            if ui.toggle_value(&mut crop, "⛶ Crop").on_hover_text("Drag on the picture to draw the part to keep, or pull its edges and corners; double-click to clear").changed() {
                self.tool = if crop { Tool::Crop } else { Tool::View };
                self.change();
            }
            if self.tool == Tool::Crop {
                egui::ComboBox::from_id_salt("video-crop-aspect").selected_text(self.aspect.label()).width(80.0).show_ui(ui, |ui| {
                    for a in ASPECTS {
                        ui.selectable_value(&mut self.aspect, a, a.label());
                    }
                });
            }
            if ui.add_enabled(self.ve.crop.is_some(), egui::Button::new("Clear crop")).clicked() {
                self.ve.crop = None;
            }
            ui.separator();
            ui.label("Size");
            let mut side = self.ve.max_side.unwrap_or(longest).min(longest);
            if ui.add(egui::DragValue::new(&mut side).range(64..=longest).speed(4.0).suffix(" px")).on_hover_text("Longest side of the result: smaller makes a smaller file").changed() {
                self.ve.max_side = (side < longest).then_some(side);
            }
            for (label, k) in [("¾", 0.75), ("½", 0.5), ("¼", 0.25)] {
                if ui.small_button(label).on_hover_text("Scale down").clicked() {
                    self.ve.max_side = Some(((longest as f32 * k) as u32).max(64));
                }
            }
            if ui.add_enabled(self.ve.max_side.is_some(), egui::Button::new("Original")).clicked() {
                self.ve.max_side = None;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Fade in");
            let max = (self.result_secs() / 2.0).max(0.1);
            ui.add(egui::Slider::new(&mut self.ve.fade_in, 0.0..=max).suffix(" s").max_decimals(1)).on_hover_text("From black (and silence)");
            ui.label("out");
            ui.add(egui::Slider::new(&mut self.ve.fade_out, 0.0..=max).suffix(" s").max_decimals(1)).on_hover_text("To black (and silence)");
        });
    }

    fn time_controls(&mut self, ui: &mut egui::Ui) {
        let src_fps = self.ready.as_ref().map_or(30.0, |r| r.fps) as f32;
        ui.horizontal_wrapped(|ui| {
            ui.strong("Time");
            ui.label("Speed");
            ui.add(egui::Slider::new(&mut self.ve.speed, VideoEdits::MIN_SPEED..=VideoEdits::MAX_SPEED).logarithmic(true).suffix("×").fixed_decimals(2))
                .on_hover_text("Slower or faster; the sound keeps its pitch");
            for preset in [0.5, 1.0, 2.0] {
                if ui.selectable_label(self.ve.speed == preset, format!("{preset}×")).clicked() {
                    self.ve.speed = preset;
                }
            }
            ui.separator();
            ui.label("Frame rate");
            let mut fps = self.ve.fps.unwrap_or(src_fps).min(src_fps);
            if ui.add(egui::DragValue::new(&mut fps).range(1.0..=src_fps.max(1.0)).speed(0.2).suffix(" fps").max_decimals(1)).on_hover_text("Fewer frames per second makes a smaller file").changed() {
                self.ve.fps = (fps < src_fps - 0.05).then_some(fps);
            }
        });
    }

    fn text_controls(&mut self, ui: &mut egui::Ui) {
        let (rw, rh) = self.result_size();
        let space = (TEXT_SPACE, (TEXT_SPACE as f64 * rh as f64 / rw.max(1) as f64).round().max(1.0) as u32);
        let result_secs = self.result_secs();
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.strong("Text");
            ui.weak("Drag a text on the picture while it's showing to move it");
            if ui.button("➕ Add text").on_hover_text("A caption over the picture, for as long as you like").clicked() {
                let caption = Caption {
                    text: "TEXT".into(),
                    x: space.0 as f32 / 2.0,
                    y: space.1 as f32 * 0.85,
                    size: space.0 as f32 * 0.07,
                    font: CaptionFont::Meme,
                    uppercase: true,
                    fill: [255, 255, 255, 255],
                    outline: [0, 0, 0, 255],
                    outline_width: 0.08,
                };
                self.ve.texts.push(VideoText { caption, of_width: space.0, of_height: space.1, start: 0.0, end: None });
                self.text_times.push(Default::default());
                self.selected_text = Some(self.ve.texts.len() - 1);
                changed = true;
            }
        });
        let mut remove = None;
        for i in 0..self.ve.texts.len() {
            if self.text_times.len() <= i {
                self.text_times.push(Default::default());
            }
            let (times, t) = (&mut self.text_times[i], &mut self.ve.texts[i]);
            ui.horizontal_wrapped(|ui| {
                if self.selected_text == Some(i) {
                    ui.strong(format!("▶ {}.", i + 1));
                } else {
                    ui.label(format!("{}.", i + 1));
                }
                changed |= ui.add(egui::TextEdit::multiline(&mut t.caption.text).desired_rows(1).desired_width(160.0)).changed();
                let mut size = t.caption.size / t.of_width as f32 * 100.0;
                changed |= ui.add(egui::DragValue::new(&mut size).range(1.0..=40.0).speed(0.1).suffix("% size")).changed();
                t.caption.size = size / 100.0 * t.of_width as f32;
                let (mut x, mut y) = (t.caption.x / t.of_width as f32 * 100.0, t.caption.y / t.of_height as f32 * 100.0);
                changed |= ui.add(egui::DragValue::new(&mut x).range(0.0..=100.0).speed(0.3).suffix("% across")).changed();
                changed |= ui.add(egui::DragValue::new(&mut y).range(0.0..=100.0).speed(0.3).suffix("% down")).changed();
                (t.caption.x, t.caption.y) = (x / 100.0 * t.of_width as f32, y / 100.0 * t.of_height as f32);
                ui.label("from");
                if let Some(v) = time_field(ui, &mut times.0, t.start) {
                    t.start = v.clamp(0.0, result_secs);
                    changed = true;
                }
                ui.label("to");
                if let Some(v) = time_field(ui, &mut times.1, t.end.unwrap_or(result_secs)) {
                    t.end = (v < result_secs - 0.01).then_some(v.max(t.start));
                    changed = true;
                }
                ui.color_edit_button_srgb(&mut [t.caption.fill[0], t.caption.fill[1], t.caption.fill[2]]).on_hover_text("Text colour (white with a black outline)");
                if ui.small_button("✕").on_hover_text("Remove this text").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.ve.texts.remove(i);
            self.text_times.remove(i);
            changed = true;
        }
        if changed {
            self.text_changed();
        }
    }

    fn sound_controls(&mut self, ui: &mut egui::Ui) {
        #[derive(Clone, Copy, PartialEq)]
        enum Mode {
            Original,
            File,
            None,
        }
        let picture = self.picture();
        let tracks = self.ready.as_ref().map(|r| r.tracks.clone()).unwrap_or_default();
        let mode = if self.ve.audio_file.is_some() {
            Mode::File
        } else if picture || self.ve.mute {
            Mode::None
        } else {
            Mode::Original
        };
        let mut pick = None;
        ui.horizontal_wrapped(|ui| {
            ui.strong("Sound");
            let label = match mode {
                Mode::Original => "The video's own sound",
                Mode::File => "Another sound file",
                Mode::None => "No sound",
            };
            egui::ComboBox::from_id_salt("video-sound-mode").selected_text(label).width(190.0).show_ui(ui, |ui| {
                if !picture && ui.selectable_label(mode == Mode::Original, "The video's own sound").clicked() {
                    pick = Some(Mode::Original);
                }
                if ui.selectable_label(mode == Mode::File, "Another sound file…").on_hover_text("Replace the sound with a music or sound file").clicked() {
                    pick = Some(Mode::File);
                }
                if ui.selectable_label(mode == Mode::None, "No sound").clicked() {
                    pick = Some(Mode::None);
                }
            });
            if mode != Mode::None {
                ui.label("Volume");
                ui.add(egui::Slider::new(&mut self.ve.volume_db, -24.0..=12.0).suffix(" dB").fixed_decimals(1)).on_hover_text("Louder or quieter (peaks are limited, not clipped)");
            }
            if mode == Mode::Original && !tracks.is_empty() {
                let current = self.ve.track.or_else(|| tracks.iter().position(|t| t.default).map(|i| i as u32)).unwrap_or(0);
                let name = |i: usize, t: &AudioTrack| format!("Track {}: {}", i + 1, t.label);
                egui::ComboBox::from_id_salt("video-audio-track")
                    .selected_text(tracks.get(current as usize).map(|t| name(current as usize, t)).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for (i, t) in tracks.iter().enumerate() {
                            if ui.add_enabled(t.supported, egui::Button::selectable(current == i as u32, name(i, t))).clicked() {
                                self.ve.track = (!t.default).then_some(i as u32);
                            }
                        }
                    })
                    .response
                    .on_hover_text("This video has several audio tracks");
            }
            if mode == Mode::File {
                let name = self.audio_name.clone().unwrap_or_default();
                ui.label(format!("♪ {name}"));
                if ui.button("Change…").clicked() {
                    pick = Some(Mode::File);
                }
                if !picture {
                    ui.weak("(the picture stays as long as the video)");
                }
            }
        });
        if mode == Mode::File {
            self.sound_range_bar(ui);
        }
        match pick {
            Some(Mode::Original) => {
                self.ve.mute = false;
                self.ve.audio_file = None;
                self.audio_name = None;
            }
            Some(Mode::None) => {
                self.ve.mute = true;
                self.ve.audio_file = None;
                self.audio_name = None;
            }
            Some(Mode::File) => {
                if let Some(file) = rfd::FileDialog::new().set_title("Pick a sound file").pick_file() {
                    self.audio_name = file.file_name().map(|n| n.to_string_lossy().into_owned());
                    self.ve.audio_file = Some(file);
                    self.ve.mute = false;
                }
            }
            None => {}
        }
    }

    fn done(&self) -> Outcome {
        let song_edits = self.song() && !self.song_edits.is_noop();
        if self.ve.is_noop() && !song_edits {
            return Outcome::Done { edits: None, thumb: None };
        }
        let thumb = self.rendered.as_ref().and_then(|_| {
            let frame = self.frame.as_ref()?;
            let r = self.ready.as_ref()?;
            let img = vedit::geometry(frame.clone(), r.rotation, &self.ve);
            let k = (crate::editor::THUMB_SIDE as f32 / img.width().max(img.height()) as f32).min(1.0);
            let t = image::imageops::thumbnail(&img, ((img.width() as f32 * k) as u32).max(1), ((img.height() as f32 * k) as u32).max(1));
            Some(egui::ColorImage::from_rgb([t.width() as usize, t.height() as usize], t.as_raw()))
        });
        let mut ops = Vec::new();
        if !self.ve.is_noop() {
            ops.push(EditOp::Video(self.ve.clone()));
        }
        if song_edits {
            ops.push(EditOp::Audio(self.song_edits));
        }
        Outcome::Done { edits: Some(Edits { ops }), thumb }
    }

    #[cfg(test)]
    fn tracks_len(&self) -> usize {
        self.ready.as_ref().map_or(0, |r| r.tracks.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_worker::play_side;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-audio/tests/fixtures/")).join(name)
    }

    fn open(name: &str, from: Format) -> (egui::Context, VideoEditor) {
        let ctx = egui::Context::default();
        let mut e = VideoEditor::open(&ctx, 1, fixture(name), from, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while e.ready.is_none() || e.frame.is_none() {
            while let Ok(m) = e.worker.replies.try_recv() {
                match m {
                    Msg::Ready(Ok(r)) => e.ready = Some(r),
                    Msg::Frame(img) => e.frame = Some(img),
                    Msg::Ended(_) | Msg::Picture(..) => {}
                    Msg::Ready(Err(m)) | Msg::Failed(m) => panic!("{m}"),
                }
            }
            if e.ready.is_some() && e.asked.is_none() {
                e.asked = Some(0.0);
                e.worker.requests.send(Cmd::Seek(0.0)).unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "the editor never loaded");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (ctx, e)
    }

    #[test]
    fn a_video_loads_and_edits_become_the_saved_result() {
        let (_ctx, mut e) = open("video_two_tracks.mkv", Format::Mkv);
        assert!(!e.picture());
        assert_eq!(e.tracks_len(), 2);
        assert!((e.duration() - 0.5).abs() < 0.02);
        assert!(matches!(e.done(), Outcome::Done { edits: None, .. }), "untouched is no edit");

        e.ve.trim_start = 0.1;
        e.ve.speed = 2.0;
        e.ve.rotate = 90;
        assert!((e.result_secs() - 0.2).abs() < 0.02, "0.4 s kept at 2×");
        assert_eq!(e.result_size(), (64, 64));
        let Outcome::Done { edits: Some(edits), .. } = e.done() else { panic!("expected edits") };
        let v = edits.video().unwrap();
        assert_eq!((v.trim_start, v.speed, v.rotate), (0.1, 2.0, 90));
    }

    #[test]
    fn text_positions_survive_changes_to_the_crop() {
        let (_ctx, mut e) = open("video_opus.mp4", Format::Mp4);
        let t = VideoText {
            caption: Caption {
                text: "HELLO".into(),
                x: 500.0,
                y: 500.0,
                size: 70.0,
                font: CaptionFont::Meme,
                uppercase: true,
                fill: [255; 4],
                outline: [0, 0, 0, 255],
                outline_width: 0.08,
            },
            of_width: 1000,
            of_height: 1000,
            start: 0.0,
            end: None,
        };
        e.ve.texts.push(t);
        e.text_changed();
        let mut img = vedit::geometry(e.frame.clone().unwrap(), 0, &e.ve);
        e.overlays.apply(&mut img, &e.ve, 0.0, 0.5);
        let plain = vedit::geometry(e.frame.clone().unwrap(), 0, &e.ve);
        assert!(img.pixels().zip(plain.pixels()).any(|(a, b)| a != b), "the text is drawn");
        e.ve.crop = Some(Region { x: 0, y: 0, width: 32, height: 64, of_width: 64, of_height: 64 });
        assert_eq!(e.result_size(), (32, 64));
    }

    #[test]
    fn a_picture_gets_sound_and_a_still_length() {
        let png = std::env::temp_dir().join(format!("tf-vedit-{}.png", std::process::id()));
        image::RgbImage::from_pixel(40, 30, image::Rgb([5, 5, 5])).save(&png).unwrap();
        let ctx = egui::Context::default();
        let mut e = VideoEditor::open(&ctx, 1, png.clone(), Format::Png, None);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while e.ready.is_none() {
            if let Ok(Msg::Ready(Ok(r))) = e.worker.replies.recv_timeout(std::time::Duration::from_millis(50)) {
                e.ready = Some(r);
            }
            assert!(std::time::Instant::now() < deadline);
        }
        assert!(e.picture());
        assert_eq!((e.result_size(), e.result_secs()), ((40, 30), STILL_SECS));
        e.ve.audio_file = Some(fixture("video_opus.mp4"));
        let Outcome::Done { edits: Some(edits), .. } = e.done() else { panic!("expected edits") };
        assert!(edits.video().unwrap().audio_file.is_some());
        std::fs::remove_file(png).ok();
    }

    fn pump(e: &mut VideoEditor, ctx: &egui::Context, until: impl Fn(&VideoEditor) -> bool, secs: f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        while !until(e) && std::time::Instant::now() < deadline {
            while let Ok(m) = e.worker.replies.try_recv() {
                match m {
                    Msg::Frame(img) if e.play.is_none() => e.frame = Some(img),
                    Msg::Ended(run) if e.play.as_ref().is_some_and(|p| p.run == run) => e.video_ended = true,
                    _ => {}
                }
            }
            e.update_sound();
            e.update_playback(ctx);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    #[test]
    fn playback_picks_the_largest_size_the_machine_keeps_up_with() {
        let interval = 1.0 / 30.0;
        // Plenty of time: up to the stills' size.
        assert_eq!(play_side(480, 0.002, interval), 720);
        assert_eq!(play_side(720, 0.002, interval), 960);
        assert_eq!(play_side(960, 0.002, interval), 960, "no larger size");
        // Barely keeping up: stay.
        assert_eq!(play_side(720, 0.020, interval), 720);
        // Falling behind: step down, one size at a time, down to the smallest.
        assert_eq!(play_side(960, 0.032, interval), 720);
        assert_eq!(play_side(720, 0.032, interval), 480);
        assert_eq!(play_side(480, 0.5, interval), 480);
        // A cost that would break the budget at the next size isn't risked.
        assert_eq!(play_side(480, 0.009, interval), 480, "2.25× the pixels would not fit");
    }

    #[test]
    fn the_frame_queue_shows_the_latest_due_picture_and_drops_the_rest() {
        let img = |v: u8| RgbImage::from_pixel(1, 1, image::Rgb([v, 0, 0]));
        let mut q = FrameQueue::default();
        assert!(q.at(0.0).is_none());
        for (t, v) in [(0.0, 0), (0.1, 1), (0.2, 2), (0.3, 3)] {
            q.push(t, img(v));
        }
        assert_eq!(q.at(0.0).map(|f| f.0), Some(0.0));
        assert_eq!(q.at(0.25).map(|f| f.0), Some(0.2), "0.0 and 0.1 were skipped");
        assert_eq!(q.frames.len(), 2);
        assert_eq!(q.at(0.21).map(|f| f.0), Some(0.2));
        q.clear();
        q.push(1.0, img(9));
        assert!(q.at(0.5).is_none(), "not due yet");
    }

    #[test]
    fn the_worker_streams_frames_in_order_and_says_when_they_end() {
        let (_ctx, e) = open("video_two_tracks.mkv", Format::Mkv);
        e.worker.requests.send(Cmd::Play(0.0, 7, 1.0)).unwrap();
        let mut got = Vec::new();
        let mut ended = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ended && std::time::Instant::now() < deadline {
            while let Ok((run, pts, _)) = e.worker.play.try_recv() {
                assert_eq!(run, 7);
                got.push(pts);
            }
            while let Ok(m) = e.worker.replies.try_recv() {
                ended |= matches!(m, Msg::Ended(7));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        while let Ok((_, pts, _)) = e.worker.play.try_recv() {
            got.push(pts);
        }
        assert!(ended, "the end was reported");
        assert_eq!(got.len(), 5, "{got:?}");
        assert!(got.windows(2).all(|w| w[1] > w[0]));
        // A new run from the middle starts there.
        e.worker.requests.send(Cmd::Play(0.3, 8, 1.0)).unwrap();
        let (run, pts, _) = e.worker.play.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(run, 8);
        assert!((pts - 0.3).abs() < 0.02, "{pts}");
    }

    #[test]
    fn playback_without_sound_follows_the_wall_clock_to_the_end() {
        let (ctx, mut e) = open("video_two_tracks.mkv", Format::Mkv);
        e.ve.mute = true;
        e.start_play(&ctx, 0.0);
        assert!(e.play.is_some() && !e.play.as_ref().unwrap().audio);
        let mut seen = std::collections::BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while e.play.is_some() && std::time::Instant::now() < deadline {
            pump(&mut e, &ctx, |e| e.play.is_none(), 0.01);
            if let Some(t) = e.shown {
                seen.insert((t * 100.0).round() as i64);
            }
        }
        assert!(e.play.is_none(), "it stops by itself");
        assert!(seen.len() >= 3, "several different frames were shown: {seen:?}");
        assert!(e.cursor > 0.35 && e.cursor <= 0.5, "left near the end: {}", e.cursor);

        // Pausing keeps the place; playing from the end starts over.
        e.start_play(&ctx, 0.0);
        pump(&mut e, &ctx, |_| false, 0.15);
        e.stop_play();
        assert!(e.play.is_none() && e.cursor > 0.05);
        let paused = e.cursor;
        e.start_play(&ctx, paused);
        assert!((e.cursor - paused).abs() < 1e-9, "resumes from where it paused");
        e.stop_play();
        e.start_play(&ctx, 0.5);
        assert_eq!(e.cursor, 0.0, "at the end it starts over");
        e.stop_play();

        // Twice the speed takes half as long.
        e.ve.speed = 2.0;
        let t = std::time::Instant::now();
        e.start_play(&ctx, 0.0);
        pump(&mut e, &ctx, |e| e.play.is_none(), 5.0);
        let took = t.elapsed().as_secs_f64();
        assert!(took < 0.45, "0.5 s of video at 2× took {took:.2} s");
    }

    #[test]
    fn the_preview_sound_follows_the_edits() {
        let (_ctx, mut e) = open("video_two_tracks.mkv", Format::Mkv);
        e.ve.trim_start = 0.1;
        e.ve.speed = 2.0;
        e.ve.volume_db = -3.0;
        let own = e.sound_edits();
        assert_eq!((own.trim_start, own.speed, own.volume), (0.1, 2.0, Volume::Gain(-3.0)), "the video's own sound is edited like the video");
        assert_eq!(e.sound_key(), Some(SoundKey { file: None, track: None }));
        e.ve.audio_file = Some(fixture("video_opus.mp4"));
        let other = e.sound_edits();
        assert_eq!((other.trim_start, other.speed), (0.0, 1.0), "another file starts with the result, at normal speed");
        assert!((other.trim_end.unwrap() - 0.2).abs() < 0.02, "and ends with it: {:?}", other.trim_end);
        assert_eq!(e.sound_key().unwrap().file, e.ve.audio_file);
        e.ve.audio_file = None;
        e.ve.mute = true;
        assert!(e.sound_key().is_none());
    }

    /// Needs a sound device: `cargo test -p dotty-app -- --ignored video_editor`.
    #[test]
    #[ignore]
    fn playback_with_sound_is_clocked_by_the_audio() {
        let (ctx, mut e) = open("video_two_tracks.mkv", Format::Mkv);
        e.start_play(&ctx, 0.0);
        assert!(e.pending_play, "waits for the sound to load");
        pump(&mut e, &ctx, |e| e.play.is_some(), 10.0);
        assert!(e.play.as_ref().is_some_and(|p| p.audio), "the sound is playing");
        pump(&mut e, &ctx, |e| e.cursor > 0.15, 3.0);
        assert!(e.cursor > 0.15, "the playhead follows the sound: {}", e.cursor);
        pump(&mut e, &ctx, |e| e.play.is_none(), 5.0);
        assert!(e.play.is_none(), "stops at the end");
    }

    fn text(x: f32, y: f32, words: &str, size: f32) -> VideoText {
        VideoText {
            caption: Caption { text: words.into(), x, y, size, font: CaptionFont::Meme, uppercase: true, fill: [255; 4], outline: [0, 0, 0, 255], outline_width: 0.08 },
            of_width: 1000,
            of_height: 500,
            start: 0.0,
            end: None,
        }
    }

    #[test]
    fn a_text_is_picked_up_where_it_is_drawn() {
        let shown = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(500.0, 250.0)); // half the text space
        let t = text(500.0, 250.0, "HELLO", 100.0);
        let b = text_box(&t, shown);
        assert_eq!(b.center(), egui::pos2(350.0, 175.0), "the middle of the picture");
        // 5 letters at a size of 50 points: about 75 points wide each side, 29 tall.
        assert!(b.width() > 100.0 && b.width() < 200.0 && b.height() > 40.0 && b.height() < 90.0, "{b:?}");
        let two_lines = text_box(&text(500.0, 250.0, "HELLO\nWORLD", 100.0), shown);
        assert!(two_lines.height() > b.height() * 1.6, "a second line makes it taller");
        assert!(text_box(&text(0.0, 0.0, "X", 40.0), shown).contains(shown.min), "a text in the corner");
    }

    #[test]
    fn undo_and_redo_step_back_through_grouped_changes() {
        let (_ctx, mut e) = open("video_two_tracks.mkv", Format::Mkv);
        let mut ve = e.ve.clone();
        // A drag's worth of changes in quick succession is one step.
        for (i, side) in [200, 150, 100].into_iter().enumerate() {
            let before = ve.clone();
            ve.max_side = Some(side);
            e.ve = ve.clone();
            e.record((before, e.song_edits), 1.0 + i as f64 * 0.1);
        }
        assert_eq!(e.undo.len(), 1);
        // A later change is another.
        let before = (e.ve.clone(), e.song_edits);
        e.ve.mute = true;
        e.ve.texts.push(text(10.0, 10.0, "A", 30.0));
        e.record(before, 5.0);
        assert_eq!(e.undo.len(), 2);

        e.undo();
        assert!(!e.ve.mute && e.ve.texts.is_empty() && e.ve.max_side == Some(100), "back to before the second step");
        assert_eq!(e.text_times.len(), 0, "the per-text boxes follow");
        e.undo();
        assert_eq!(e.ve.max_side, None, "back to untouched");
        assert!(e.undo.is_empty());
        e.redo();
        assert_eq!(e.ve.max_side, Some(100));
        e.redo();
        assert!(e.ve.mute && e.ve.texts.len() == 1 && e.text_times.len() == 1);
        // A new change clears what could be redone.
        e.undo();
        let before = (e.ve.clone(), e.song_edits);
        e.ve.flip_h = true;
        e.record(before, 9.0);
        assert!(e.redo.is_empty());
    }

    #[test]
    fn the_whole_editor_draws_without_trouble() {
        let (ctx, mut e) = open("video_two_tracks.mkv", Format::Mkv);
        e.ve.texts.push(text(500.0, 250.0, "HI", 80.0));
        e.ve.crop = Some(Region { x: 0, y: 0, width: 32, height: 32, of_width: 64, of_height: 64 });
        for tool in [Tool::View, Tool::Crop] {
            e.tool = tool;
            e.dirty = true;
            for t in [0.0, 1.0] {
                let input = egui::RawInput { time: Some(t), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))), ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| {
                    e.render(ui.ctx());
                    let _ = e.ui(ui);
                });
                out.textures_delta.clear();
            }
        }
        assert!(e.rendered.is_some());
    }

    fn open_song(edits: Option<Edits>) -> (egui::Context, VideoEditor) {
        let ctx = egui::Context::default();
        let song = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-video/tests/fixtures/song_with_cover.mp3"));
        let mut e = VideoEditor::open(&ctx, 1, song, Format::Mp3, edits.map(Arc::new));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while e.ready.is_none() || e.frame.is_none() {
            while let Ok(m) = e.worker.replies.try_recv() {
                match m {
                    Msg::Ready(Ok(r)) => e.ready = Some(r),
                    Msg::Frame(img) => e.frame = Some(img),
                    Msg::Ready(Err(m)) | Msg::Failed(m) => panic!("{m}"),
                    Msg::Ended(_) | Msg::Picture(..) => {}
                }
            }
            if e.ready.is_some() && e.asked.is_none() {
                e.asked = Some(0.0);
                e.worker.requests.send(Cmd::Seek(0.0)).unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "the song never loaded");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (ctx, e)
    }

    #[test]
    fn a_song_opens_with_its_cover_and_length_and_a_chosen_picture_replaces_it() {
        let (_ctx, mut e) = open_song(None);
        assert!(e.picture() && e.song());
        assert_eq!(e.ready.as_ref().unwrap().size, (120, 90), "the cover art");
        assert!((e.duration() - 1.0).abs() < 0.05, "the song's length: {}", e.duration());
        assert_eq!(e.sound_key(), Some(SoundKey { file: None, track: None }), "the song itself is the sound");
        assert!(matches!(e.done(), Outcome::Done { edits: None, .. }));

        // Choosing a picture loads it and clears a crop made on the old one.
        let pic = std::env::temp_dir().join(format!("tf-vedit-song-{}.png", std::process::id()));
        image::RgbImage::from_pixel(50, 20, image::Rgb([1, 2, 3])).save(&pic).unwrap();
        e.ve.crop = Some(Region { x: 0, y: 0, width: 10, height: 10, of_width: 120, of_height: 90 });
        e.ve.picture_file = Some(pic.clone());
        e.sync_picture();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while e.ready.as_ref().unwrap().size != (50, 20) {
            e.drain_replies();
            assert!(std::time::Instant::now() < deadline, "the picture never changed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(e.ve.crop.is_none() && e.result_size() == (50, 20));
        let Outcome::Done { edits: Some(edits), .. } = e.done() else { panic!("expected edits") };
        assert_eq!(edits.video().unwrap().picture_file.as_ref(), Some(&pic));
        assert!(edits.audio().is_none(), "the sound edits are the sound editor's");
        std::fs::remove_file(pic).ok();
    }

    #[test]
    fn a_songs_length_follows_its_sound_edits() {
        let sound = AudioEdits { trim_start: 0.2, trim_end: Some(0.8), speed: 2.0, ..Default::default() };
        let (_ctx, e) = open_song(Some(Edits { ops: vec![EditOp::Audio(sound)] }));
        assert!((e.duration() - 0.3).abs() < 0.02, "0.6 s kept at 2×: {}", e.duration());
        assert_eq!(e.sound_edits(), sound, "the preview plays the song as the sound editor left it");
    }

    #[test]
    fn the_song_editor_draws_in_both_tool_modes() {
        let (ctx, mut e) = open_song(None);
        e.ve.texts.push(text(500.0, 300.0, "SONG", 90.0));
        for tool in [Tool::View, Tool::Crop] {
            e.tool = tool;
            e.dirty = true;
            for t in [0.0, 1.0] {
                let input = egui::RawInput { time: Some(t), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))), ..Default::default() };
                let mut out = ctx.run_ui(input, |ui| {
                    e.render(ui.ctx());
                    let _ = e.ui(ui);
                });
                out.textures_delta.clear();
            }
        }
        assert!(e.rendered.is_some());
    }

    fn draw(ctx: &egui::Context, e: &mut VideoEditor) {
        let input = egui::RawInput { time: Some(1.0), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            e.render(ui.ctx());
            let _ = e.ui(ui);
        });
        out.textures_delta.clear();
    }

    #[test]
    fn a_songs_part_is_picked_on_its_timeline_and_saved_as_sound_edits() {
        let (ctx, mut e) = open_song(None);
        draw(&ctx, &mut e); // starts loading the sound for the waveform
        pump(&mut e, &ctx, |e| matches!(e.sound, Sound::Ready(..)), 10.0);
        assert!(matches!(e.sound, Sound::Ready(..)), "the sound loads without Play");
        let raw = e.raw_sound_secs().unwrap();
        assert!((raw - 1.0).abs() < 0.05);
        assert_eq!(e.sound_range(), Some((0.0, raw)));

        e.set_sound_range(0.2, 0.7);
        assert!((e.duration() - 0.5).abs() < 0.02, "the video is as long as the part: {}", e.duration());
        draw(&ctx, &mut e);
        let Outcome::Done { edits: Some(edits), .. } = e.done() else { panic!("expected edits") };
        let sound = edits.audio().expect("the song's range is saved as its sound edits");
        assert_eq!((sound.trim_start, sound.trim_end), (0.2, Some(0.7)));
        assert!(edits.video().is_none(), "no picture changes");

        // The whole sound again is no edit; undo brings the part back.
        e.set_sound_range(0.0, raw);
        assert!(matches!(e.done(), Outcome::Done { edits: None, .. }));
        e.record((e.ve.clone(), *sound), 100.0);
        e.undo();
        assert_eq!(e.song_edits.trim_start, 0.2, "undo restores the song's range");
    }

    #[test]
    fn a_pictures_length_is_set_or_follows_the_part_picked_of_its_sound() {
        let png = std::env::temp_dir().join(format!("tf-vedit-len-{}.png", std::process::id()));
        image::RgbImage::from_pixel(40, 30, image::Rgb([5, 5, 5])).save(&png).unwrap();
        let ctx = egui::Context::default();
        let mut e = VideoEditor::open(&ctx, 1, png.clone(), Format::Png, None);
        pump_ready(&mut e);
        // No sound: a length of its own.
        assert_eq!(e.result_secs(), STILL_SECS);
        e.ve.still_secs = Some(12.0);
        assert_eq!((e.duration(), e.result_secs()), (12.0, 12.0));
        draw(&ctx, &mut e);
        let Outcome::Done { edits: Some(edits), .. } = e.done() else { panic!("expected edits") };
        assert_eq!(edits.video().unwrap().still_secs, Some(12.0));

        // With a sound: the part picked sets the length.
        e.ve.audio_file = Some(fixture("video_opus.mp4"));
        draw(&ctx, &mut e);
        pump(&mut e, &ctx, |e| matches!(e.sound, Sound::Ready(..)), 10.0);
        let raw = e.raw_sound_secs().expect("the added sound loaded");
        let (a, b) = (raw * 0.25, raw * 0.75);
        e.set_sound_range(a, b);
        assert!((e.result_secs() - (b - a)).abs() < 1e-9, "{}", e.result_secs());
        assert_eq!((e.ve.audio_start, e.ve.audio_end), (a, Some(b)));
        let sound = e.sound_edits();
        assert_eq!((sound.trim_start, sound.trim_end), (a, Some(b)));
        draw(&ctx, &mut e);
        std::fs::remove_file(png).ok();
    }

    fn pump_ready(e: &mut VideoEditor) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while e.ready.is_none() {
            if let Ok(Msg::Ready(Ok(r))) = e.worker.replies.recv_timeout(std::time::Duration::from_millis(50)) {
                e.ready = Some(r);
            }
            assert!(std::time::Instant::now() < deadline);
        }
    }

    /// Needs a sound device: `cargo test -p dotty-app -- --ignored video_editor`.
    #[test]
    #[ignore]
    fn a_song_plays_with_its_picture_clocked_by_the_sound() {
        let (ctx, mut e) = open_song(None);
        e.ve.texts.push(VideoText { start: 0.3, ..text(500.0, 300.0, "LATE", 90.0) });
        e.start_play(&ctx, 0.0);
        pump(&mut e, &ctx, |e| e.play.is_some(), 10.0);
        assert!(e.play.as_ref().is_some_and(|p| p.audio), "the song is playing");
        pump(&mut e, &ctx, |e| e.cursor > 0.4, 3.0);
        assert!(e.cursor > 0.4, "the playhead follows the song: {}", e.cursor);
        pump(&mut e, &ctx, |e| e.play.is_none(), 5.0);
        assert!(e.play.is_none(), "stops when the song ends");
    }
}
