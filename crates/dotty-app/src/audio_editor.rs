//! The audio editor: a modal over the file list for quick changes before a conversion
//! (trim, reverse, speed, pitch, filters, fades, echo, reverb, volume). It decodes the file into memory, plays a preview through the same stages
//! the converter uses, and produces [`Edits`] for the job, so the source is never changed.

use crate::player::{Player, Request};
use codecs_audio::StreamInfo;
use codecs_audio::{AudioConverter, AudioSource, AudioTrack};
use codecs_audio::edit::{CEILING, Measurement, MemorySource, Plan, decode_all, limits, measure_memory, volume_gain, with_channels};
use dotty_core::{AudioEdits, Channels, EditOp, Edits, Format, JobId, Reverb, Volume};
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
    path: PathBuf,
    from: Format,
    state: State,
    cancel: Arc<AtomicBool>,
    /// The output format's channel setting (`None` = keep), so normalizing measures the
    /// layout that gets written.
    output_channels: Option<u16>,
    /// The file's audio tracks when it has several (videos often do), else empty.
    tracks: Vec<AudioTrack>,
}

/// The file's tracks (listed even if the picked one fails) and the decoded track.
type Loaded = (Vec<AudioTrack>, Result<(Arc<[f32]>, StreamInfo, Peaks), String>);

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
    pub fn open(
        ctx: &egui::Context,
        entry: JobId,
        path: PathBuf,
        from: Format,
        existing: Option<Arc<Edits>>,
        output_channels: Option<u16>,
    ) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let existing = existing.and_then(|e| e.audio().copied());
        let (rx, cancel) = Self::load(ctx, path.clone(), from, existing.and_then(|e| e.track));
        Self { entry, title, path, from, state: State::Loading(rx, existing), cancel, output_channels, tracks: Vec::new() }
    }

    /// Decode `track` of the file (and list its tracks) in the background.
    fn load(ctx: &egui::Context, path: PathBuf, from: Format, track: Option<u32>) -> (mpsc::Receiver<Loaded>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (ctx, stop) = (ctx.clone(), cancel.clone());
        std::thread::spawn(move || {
            let tracks = AudioConverter::default().tracks(&path, from);
            let tracks = if tracks.len() > 1 { tracks } else { Vec::new() };
            let result = decode_all(&path, from, track, MAX_SECS, &stop).map(|(samples, info)| {
                let peaks = Peaks::new(&samples, info.channels as usize);
                (samples, info, peaks)
            });
            let _ = tx.send((tracks, result.map_err(|e| e.to_string())));
            ctx.request_repaint();
        });
        (rx, cancel)
    }

    /// Reload with another audio track, keeping the other edits.
    fn switch_track(&mut self, ctx: &egui::Context, mut edits: AudioEdits, track: u32) {
        let default = self.tracks.iter().position(|t| t.default).unwrap_or(0) as u32;
        edits.track = (track != default).then_some(track);
        self.cancel.store(true, Ordering::Relaxed);
        let (rx, cancel) = Self::load(ctx, self.path.clone(), self.from, edits.track);
        self.cancel = cancel;
        self.state = State::Loading(rx, Some(edits));
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        if let State::Loading(rx, existing) = &mut self.state {
            match rx.try_recv() {
                Ok((tracks, Ok((samples, info, peaks)))) => {
                    self.tracks = tracks;
                    let mut session = Session::new(samples, info, peaks, existing.unwrap_or_default());
                    session.output_channels = self.output_channels;
                    session.tracks = self.tracks.clone();
                    self.state = State::Ready(Box::new(session))
                }
                Ok((tracks, Err(e))) => {
                    self.tracks = tracks;
                    self.state = State::Failed(e);
                }
                Err(mpsc::TryRecvError::Disconnected) => self.state = State::Failed("couldn't open the file".into()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let screen = ctx.content_rect();
        let size = (screen.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(320.0, 240.0));
        let size = egui::vec2(size.x, size.y.min(560.0));
        let mut outcome = Outcome::Open;
        let mut switch = None;
        let tracks = self.tracks.clone();
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
                    // Another track may work (a video's default track can be AAC).
                    if !tracks.is_empty() {
                        ui.horizontal(|ui| {
                            ui.label("Try another audio track:");
                            if let Some(t) = track_picker(ui, &tracks, None) {
                                switch = Some((AudioEdits::default(), t));
                            }
                        });
                    }
                    if ui.button("Close").clicked() {
                        outcome = Outcome::Cancelled;
                    }
                }
                State::Ready(session) => {
                    outcome = session.ui(ui, &self.title);
                    if let Some(t) = session.switch_track.take() {
                        switch = Some((session.edits, t));
                    }
                }
            }
        });
        if let Some((edits, track)) = switch {
            self.switch_track(ctx, edits, track);
        }
        outcome
    }
}

/// Min/max of every channel's samples per block, at two block sizes, so the waveform can
/// be drawn at any zoom without scanning the whole file each frame.
pub(crate) struct Peaks {
    small: Vec<(f32, f32)>,
    big: Vec<(f32, f32)>,
}

impl Peaks {
    pub(crate) fn new(samples: &[f32], channels: usize) -> Self {
        let small: Vec<(f32, f32)> = samples.chunks(BLOCK * channels).map(|b| min_max(b.iter().copied())).collect();
        let big = small.chunks(BIG_BLOCK / BLOCK).map(|b| merge(b.iter().copied())).collect();
        Self { small, big }
    }

    /// Min and max over frames `a..b`, scanning `samples` for short spans.
    pub(crate) fn range(&self, samples: &[f32], channels: usize, a: usize, b: usize) -> (f32, f32) {
        let b = b.max(a + 1);
        if b - a <= BLOCK * 2 {
            return scan(samples, channels, a, b);
        }
        self.blocks(a, b)
    }

    /// Min and max over frames `a..b` from the blocks alone (may look a block wider).
    fn blocks(&self, a: usize, b: usize) -> (f32, f32) {
        let b = b.max(a + 1);
        let (level, size) = if b - a <= BIG_BLOCK * 2 { (&self.small, BLOCK) } else { (&self.big, BIG_BLOCK) };
        let (lo, hi) = (a / size, b.div_ceil(size).min(level.len()));
        if lo < hi { merge(level[lo..hi].iter().copied()) } else { (0.0, 0.0) }
    }
}

/// Min and max over frames `a..b` of interleaved `samples`.
fn scan(samples: &[f32], channels: usize, a: usize, b: usize) -> (f32, f32) {
    let (lo, hi) = (a * channels, (b * channels).min(samples.len()));
    if lo < hi { min_max(samples[lo..hi].iter().copied()) } else { (0.0, 0.0) }
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
    /// The fade grip on the left or right of the kept part (fade-in and fade-out swap
    /// sides when reversed).
    FadeLeft,
    FadeRight,
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
    /// The level of the audio reaching the volume stage, for the edits it was measured with
    /// (only the ones before that stage matter).
    measured: Option<(AudioEdits, Measurement)>,
    measuring: Option<Background<AudioEdits, Measurement>>,
    /// Waveform peaks of the whole file after the channel setting (a mono mixdown can
    /// cancel, widening raises peaks), for the setting they were made with.
    display: Option<(Channels, Arc<Peaks>)>,
    display_job: Option<Background<Channels, Peaks>>,
    /// The visible stretch after the channel setting, for close zoom levels.
    slice: Option<Slice>,
    /// Height the controls under the waveform took last frame (they wrap on narrow windows).
    controls_height: f32,
    /// See [`AudioEditor::output_channels`].
    output_channels: Option<u16>,
    /// The mono → stereo width last set, kept while another channel setting is picked.
    width: f32,
    /// The file's audio tracks when it has several, and one picked to switch to.
    tracks: Vec<AudioTrack>,
    switch_track: Option<u32>,
}

/// A measurement running in the background; dropping it cancels it.
/// Frames `start..end` rendered with channel setting `setting`.
struct Slice {
    start: u64,
    end: u64,
    setting: Channels,
    data: Arc<[f32]>,
    channels: usize,
}

/// Work running on another thread for the edits in `key`; dropping it cancels it.
struct Background<K, T> {
    key: K,
    rx: mpsc::Receiver<T>,
    cancel: Arc<AtomicBool>,
}

impl<K, T: Send + 'static> Background<K, T> {
    fn spawn(ctx: &egui::Context, key: K, work: impl FnOnce(&AtomicBool) -> Option<T> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (stop, ctx) = (cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            if let Some(v) = work(&stop) {
                let _ = tx.send(v);
                ctx.request_repaint();
            }
        });
        Self { key, rx, cancel }
    }
}

impl<K, T> Drop for Background<K, T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
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
            measured: None,
            measuring: None,
            display: None,
            display_job: None,
            slice: None,
            controls_height: 180.0,
            output_channels: None,
            tracks: Vec::new(),
            switch_track: None,
            width: match edits.channels {
                Channels::Stereo { width } => width,
                _ => 0.0,
            },
        };
        s.cursor = s.result_start();
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

    /// Length of the result in seconds (after speed changes, before any effect tail).
    fn result_secs(&self) -> f64 {
        let (a, b) = self.kept();
        self.plan().output_len(b - a) as f64 / self.rate()
    }

    /// Where the playhead is in the result, in seconds.
    fn result_position(&self) -> f64 {
        let (_, b) = self.kept();
        self.plan().output_index(self.position().clamp(self.kept().0, b - 1), b) as f64 / self.rate()
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
        let len = self.result_secs();
        self.edits.fade_in = self.edits.fade_in.clamp(0.0, len);
        self.edits.fade_out = self.edits.fade_out.clamp(0.0, len - self.edits.fade_in);
    }

    /// The source frame the result begins with (the kept part's end when reversed).
    fn result_start(&self) -> u64 {
        let (a, b) = self.kept();
        if self.edits.reverse { b - 1 } else { a }
    }

    fn result_end(&self) -> u64 {
        let (a, b) = self.kept();
        if self.edits.reverse { a } else { b - 1 }
    }

    /// The fade on the left / right of the kept part, in seconds.
    fn fade_sides(&self) -> (f64, f64) {
        let e = &self.edits;
        if e.reverse { (e.fade_out, e.fade_in) } else { (e.fade_in, e.fade_out) }
    }

    fn set_fade_side(&mut self, left: bool, secs: f64) {
        let (l, r) = self.fade_sides();
        let secs = secs.clamp(0.0, (self.result_secs() - if left { r } else { l }).max(0.0));
        let fade_in_side = left != self.edits.reverse;
        if fade_in_side {
            self.edits.fade_in = secs;
        } else {
            self.edits.fade_out = secs;
        }
    }

    /// The edits that change what the volume stage measures: all but the volume.
    fn shape_key(&self) -> AudioEdits {
        AudioEdits { volume: Volume::Gain(0.0), ..self.edits }
    }

    fn has_tail(&self) -> bool {
        self.edits.echo.is_some() || self.edits.reverb.is_some()
    }

    fn measurement(&self) -> Option<&Measurement> {
        let key = self.shape_key();
        self.measured.as_ref().filter(|(k, _)| *k == key).map(|(_, m)| m)
    }

    /// Linear gain of the volume stage, and whether its limiter will be working.
    fn volume(&self) -> (f32, bool) {
        let m = self.measurement();
        let g = volume_gain(self.edits.volume, m);
        (g, limits(&self.edits, self.info.channels, g, m))
    }

    /// Collect a finished measurement, and start one when the edits it depends on changed
    /// (not mid-drag, so dragging a handle doesn't start dozens).
    /// The channel setting, if it changes what the waveform looks like.
    fn channel_view(&self) -> Option<Channels> {
        let c = self.edits.without_noops(self.info.channels).channels;
        (c != Channels::Keep).then_some(c)
    }

    /// Frames `lo..hi` after the channel setting `c`, interleaved, and their channel count.
    fn render_channels(&self, c: Channels, lo: u64, hi: u64, cancel: &AtomicBool) -> Option<(Vec<f32>, usize)> {
        render_channels(self.samples.clone(), &self.info, c, lo, hi, cancel)
    }

    /// Collect finished waveform peaks for the channel setting, and start them when it changes.
    fn update_display(&mut self, ctx: &egui::Context, pointer_down: bool) {
        if let Some(job) = &self.display_job
            && let Ok(peaks) = job.rx.try_recv()
        {
            self.display = Some((job.key, Arc::new(peaks)));
            self.display_job = None;
        }
        let Some(key) = self.channel_view() else {
            self.display_job = None;
            return;
        };
        let ready = self.display.as_ref().is_some_and(|d| d.0 == key);
        if pointer_down || ready || self.display_job.as_ref().is_some_and(|j| j.key == key) {
            return;
        }
        let (samples, info, total) = (self.samples.clone(), self.info.clone(), self.total);
        self.display_job = Some(Background::spawn(ctx, key, move |stop| {
            let (v, ch) = render_channels(samples, &info, key, 0, total, stop)?;
            Some(Peaks::new(&v, ch))
        }));
    }

    /// The visible frames `a..b` after the channel setting, cached until the view or the
    /// setting changes.
    fn visible_slice(&mut self, c: Channels, a: u64, b: u64) -> Option<(u64, Arc<[f32]>, usize)> {
        if let Some(s) = &self.slice
            && (s.start, s.end, s.setting) == (a, b, c)
        {
            return Some((a, s.data.clone(), s.channels));
        }
        let (v, channels) = self.render_channels(c, a, b, &AtomicBool::new(false))?;
        let data: Arc<[f32]> = v.into();
        self.slice = Some(Slice { start: a, end: b, setting: c, data: data.clone(), channels });
        Some((a, data, channels))
    }

    fn update_measurement(&mut self, ctx: &egui::Context, pointer_down: bool) {
        if let Some(m) = &self.measuring {
            match m.rx.try_recv() {
                Ok(v) => {
                    self.measured = Some((m.key, v));
                    self.measuring = None;
                    // Normalizing depends on it, so what's playing changes.
                    self.replay |= self.edits.volume.measures() && self.player.is_playing();
                }
                Err(mpsc::TryRecvError::Disconnected) => self.measuring = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let key = self.shape_key();
        if pointer_down || self.measurement().is_some() || self.measuring.as_ref().is_some_and(|m| m.key == key) {
            return;
        }
        let (samples, info, output_channels) = (self.samples.clone(), self.info.clone(), self.output_channels);
        self.measuring = Some(Background::spawn(ctx, key, move |stop| measure_memory(samples, &info, &key, output_channels, stop).ok()));
    }

    fn position(&self) -> u64 {
        self.player.position().unwrap_or(self.cursor)
    }

    fn play_from(&mut self, from: u64) {
        let req = Request {
            samples: self.samples.clone(),
            info: self.info.clone(),
            edits: self.edits,
            measured: self.measurement().copied(),
            from,
            looping: self.looping,
        };
        self.error = self.player.play(req).err();
    }

    fn toggle_play(&mut self) {
        if self.player.is_playing() {
            self.player.stop();
        } else {
            let (a, b) = self.kept();
            // From the playhead if there's something left to hear after it, else from the top.
            let rest = if self.edits.reverse { a + 1..b } else { a..b - 1 };
            let from = if rest.contains(&self.cursor) { self.cursor } else { self.result_start() };
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
        // Reaching the end leaves the playhead where playback started, like most players.
        self.player.reap();
        if let Some(e) = self.player.take_error() {
            self.error = Some(e);
        }
        let pointer_down = ui.input(|i| i.pointer.any_down());
        self.update_measurement(ui.ctx(), pointer_down);
        self.update_display(ui.ctx(), pointer_down);
        if self.player.is_playing() {
            ui.ctx().request_repaint();
        }

        ui.horizontal_wrapped(|ui| {
            ui.strong(title);
            if !self.tracks.is_empty() {
                ui.separator();
                let current = self.edits.track.or_else(|| self.tracks.iter().position(|t| t.default).map(|i| i as u32));
                if let Some(t) = track_picker(ui, &self.tracks, current) {
                    self.player.stop();
                    self.switch_track = Some(t);
                }
            }
            ui.separator();
            let playing = self.player.is_playing();
            if ui.button(if playing { "⏸ Pause" } else { "▶ Play" }).on_hover_text("Play the result (Space)").clicked() {
                self.toggle_play();
            }
            if ui.button("⏮").on_hover_text("Back to the start (Home)").clicked() {
                self.seek(self.result_start());
            }
            if ui.toggle_value(&mut self.looping, "🔁 Loop").on_hover_text("Play the kept part over and over (L)").changed() && playing {
                self.play_from(self.position());
            }
            ui.monospace(format!("{} / {}", fmt_time(self.result_position()), fmt_time(self.result_secs())));
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

        let wave_size = ui.available_size() - egui::vec2(0.0, self.controls_height + 34.0);
        let (rect, resp) = ui.allocate_exact_size(wave_size.max(egui::vec2(64.0, 80.0)), egui::Sense::click_and_drag());
        self.waveform(ui, rect, &resp);
        ui.add_space(6.0);
        let top = ui.cursor().top();
        self.controls(ui);
        let height = ui.cursor().top() - top;
        if (height - self.controls_height).abs() > 0.5 {
            self.controls_height = height;
            ui.ctx().request_repaint();
        }
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
        let (kept_a, kept_b) = self.kept();

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
        let (left, right) = self.fade_sides();
        let speed = self.plan().speed;
        let x_fade_left = self.frame_to_x(rect, (kept_a as f64 + left * rate * speed).min(kept_b as f64));
        let x_fade_right = self.frame_to_x(rect, (kept_b as f64 - right * rate * speed).max(kept_a as f64));
        let grip_y = rect.top() + 8.0;
        let grip = |x: f32| egui::pos2(x, grip_y);
        let hit = |pos: egui::Pos2| -> Option<Drag> {
            if pos.distance(grip(x_fade_left)) < GRAB + 2.0 {
                Some(Drag::FadeLeft)
            } else if pos.distance(grip(x_fade_right)) < GRAB + 2.0 {
                Some(Drag::FadeRight)
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
                Some(Drag::Start | Drag::End | Drag::FadeLeft | Drag::FadeRight) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal),
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
                // Fades are in output time.
                Drag::FadeLeft => self.set_fade_side(true, (secs - a) / speed),
                Drag::FadeRight => self.set_fade_side(false, (b - secs) / speed),
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
                    self.cursor = self.result_start();
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
        let out_len = plan.output_len(kept_len);
        let (vol, limited) = self.volume();
        // What a sample sounds like after fades and volume (filters aren't drawn).
        let level = |s: f32, f: u64| -> f32 {
            if !(kept_a..kept_b).contains(&f) {
                return s;
            }
            let v = s * plan.gain(plan.output_index(f, kept_b), out_len) * vol;
            if limited { v.clamp(-CEILING, CEILING) } else { v }
        };
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
        // The samples to draw from: the source, or after the channel setting (a Mono
        // mixdown can cancel, widening raises peaks). Close up, the visible stretch is
        // rendered; further out, whole-file peaks made in the background are used.
        let visible = (self.view_start.floor() as u64, ((self.view_start + self.view_len).ceil() as u64 + 1).min(self.total));
        let view = self.channel_view();
        let close = per_px <= (BLOCK * 2) as f64;
        let slice = view.filter(|_| close).and_then(|c| self.visible_slice(c, visible.0, visible.1));
        let display = view.and_then(|c| self.display.as_ref().filter(|d| d.0 == c).map(|d| d.1.clone()));
        let range = |a: u64, b: u64| -> (f32, f32) {
            match (&slice, &display) {
                (Some((start, data, sch)), _) => scan(data, *sch, (a - start) as usize, (b.max(a + 1) - start) as usize),
                (None, Some(p)) => p.blocks(a as usize, b as usize),
                _ => self.peaks.range(&self.samples, ch, a as usize, b as usize),
            }
        };
        if per_px < 1.0 {
            // Closer than one frame per pixel: join the samples instead.
            let (a, b) = visible;
            let (data, start, dch): (&[f32], u64, usize) = match &slice {
                Some((start, data, sch)) => (data, *start, *sch),
                None => (&self.samples, 0, ch),
            };
            let points: Vec<egui::Pos2> = (a..b)
                .map(|f| {
                    let i = (f - start) as usize * dch;
                    let s = data.get(i..i + dch).map_or(0.0, |v| v.iter().sum::<f32>() / dch as f32);
                    egui::pos2(self.frame_to_x(rect, f as f64 + 0.5), mid - level(s, f) * half)
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
                let (lo, hi) = range(a, b.min(visible.1).max(a + 1));
                let centre = (a + b) / 2;
                let inside = (kept_a..kept_b).contains(&centre);
                let x = rect.left() + px as f32 + 0.5;
                let (top, bottom) = (mid - level(hi, centre) * half, mid - level(lo, centre) * half);
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
        let (left_frames, right_frames) = if plan.reverse { (plan.fade_out, plan.fade_in) } else { (plan.fade_in, plan.fade_out) };
        // Fade lengths are output frames; the waveform is in source frames.
        let on_source = |frames: u64| ((frames as f64 * plan.speed) as u64).min(kept_len);
        for (from, to) in [(kept_a, kept_a + on_source(left_frames)), (kept_b.saturating_sub(on_source(right_frames)), kept_b)] {
            if to > from {
                let points: Vec<egui::Pos2> = (0..=48)
                    .map(|k| {
                        let f = (from + (to - from) * k / 48).min(kept_b - 1);
                        egui::pos2(self.frame_to_x(rect, f as f64), env_y(plan.gain(plan.output_index(f, kept_b), out_len)))
                    })
                    .collect();
                painter.add(egui::Shape::line(points, env));
            }
        }
        let (left, right) = self.fade_sides();
        let fade_left_x = self.frame_to_x(rect, (kept_a as f64 + left * rate * plan.speed).min(kept_b as f64));
        let fade_right_x = self.frame_to_x(rect, (kept_b as f64 - right * rate * plan.speed).max(kept_a as f64));
        for x in [fade_left_x, fade_right_x] {
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
            let len = self.result_secs();
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
            let tail = if self.has_tail() { " + effect tail" } else { "" };
            ui.weak(format!("Original {} · Result {}{tail}", fmt_time(self.duration()), fmt_time(len)));
        });
        self.speed_controls(ui);
        self.filter_controls(ui);
        self.effect_controls(ui);
        self.volume_controls(ui);
    }

    fn speed_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Speed");
            let e = &mut self.edits;
            ui.add(egui::Slider::new(&mut e.speed, AudioEdits::MIN_SPEED..=AudioEdits::MAX_SPEED).logarithmic(true).suffix("×").fixed_decimals(2))
                .on_hover_text("Slower or faster: 0.5× takes twice as long, 2× half as long");
            for preset in [0.5, 0.75, 1.0, 1.25, 1.5, 2.0] {
                if ui.selectable_label(e.speed == preset, format!("{preset}×")).clicked() {
                    e.speed = preset;
                }
            }
            ui.checkbox(&mut e.keep_pitch, "Keep pitch").on_hover_text(
                "On: change speed without changing pitch (time-stretch; very large changes can sound smeared or stuttery). Off: like a tape or record played faster or slower, pitch follows speed.",
            );
            ui.separator();
            ui.label("Pitch");
            ui.add(egui::Slider::new(&mut e.pitch, -12.0..=12.0).suffix(" st").fixed_decimals(2))
                .on_hover_text("Semitones up or down (12 is an octave; fractions are cents). Doesn't change the speed.");
            if ui.add_enabled(e.pitch != 0.0, egui::Button::new("0")).on_hover_text("Back to the original pitch").clicked() {
                e.pitch = 0.0;
            }
        });
        self.channel_controls(ui);
    }

    fn channel_controls(&mut self, ui: &mut egui::Ui) {
        let source = self.info.channels;
        let stereo_name = if source == 1 { "Mono → stereo" } else { "Stereo" };
        ui.horizontal_wrapped(|ui| {
            ui.strong("Channels");
            let e = &mut self.edits;
            // Remembered while another setting is picked, so switching back keeps it.
            if let Channels::Stereo { width } = e.channels {
                self.width = width;
            }
            let label = match e.channels {
                Channels::Stereo { .. } => stereo_name,
                Channels::Mono => "Mono",
                Channels::Keep => "Keep",
            };
            let stereo_help = if source == 1 {
                "Copy the mono sound to both sides, so echo and reverb can spread out; Width makes it sound wider"
            } else {
                "Two channels (surround is folded down to stereo)"
            };
            egui::ComboBox::from_id_salt("channels").selected_text(label).show_ui(ui, |ui| {
                if ui.selectable_label(e.channels == Channels::Keep, "Keep").on_hover_text("As in the source (the output format's channel setting still applies)").clicked() {
                    e.channels = Channels::Keep;
                }
                if ui.selectable_label(e.channels == Channels::Mono, "Mono").on_hover_text("Mix everything down to one channel").clicked() {
                    e.channels = Channels::Mono;
                }
                let stereo = matches!(e.channels, Channels::Stereo { .. });
                if ui.selectable_label(stereo, stereo_name).on_hover_text(stereo_help).clicked() && !stereo {
                    e.channels = Channels::Stereo { width: self.width };
                }
            });
            if source == 1 && matches!(e.channels, Channels::Stereo { .. }) {
                ui.add(egui::Slider::new(&mut self.width, 0.0..=1.0).text("width").fixed_decimals(2)).on_hover_text(
                    "0 puts the same sound on both sides. More spreads it out (pseudo-stereo); it still sounds right played back in mono, and the bass stays in the middle.",
                );
                e.channels = Channels::Stereo { width: self.width };
            }
        });
    }

    fn effect_controls(&mut self, ui: &mut egui::Ui) {
        let mut indent = 0.0;
        ui.horizontal_wrapped(|ui| {
            indent = ui.strong("Effects").rect.width() + ui.spacing().item_spacing.x;
            let mut on = self.edits.echo.is_some();
            ui.checkbox(&mut on, "Echo").on_hover_text("Repeats of the sound, each quieter than the last");
            let mut echo = self.edits.echo.unwrap_or_default();
            ui.add_enabled_ui(on, |ui| {
                ui.add(egui::Slider::new(&mut echo.delay_ms, 20.0..=2000.0).logarithmic(true).suffix(" ms").integer())
                    .on_hover_text("Time between repeats");
                ui.add(egui::Slider::new(&mut echo.feedback, 0.0..=0.9).text("repeats").fixed_decimals(2))
                    .on_hover_text("How much of each repeat comes back again: higher rings on longer");
                ui.add(egui::Slider::new(&mut echo.mix, 0.0..=1.0).text("level").fixed_decimals(2))
                    .on_hover_text("How loud the repeats are");
            });
            self.edits.echo = on.then_some(echo);
        });
        ui.horizontal_wrapped(|ui| {
            // Line up under "Echo".
            ui.add_space(indent - ui.spacing().item_spacing.x);
            let mut on = self.edits.reverb.is_some();
            ui.checkbox(&mut on, "Reverb").on_hover_text("The sound of a room");
            let mut reverb = self.edits.reverb.unwrap_or_default();
            ui.add_enabled_ui(on, |ui| {
                for (name, preset) in [("Room", Reverb::ROOM), ("Hall", Reverb::HALL), ("Cathedral", Reverb::CATHEDRAL)] {
                    if ui.selectable_label(reverb == preset, name).clicked() {
                        reverb = preset;
                    }
                }
                ui.add(egui::Slider::new(&mut reverb.size, 0.0..=1.0).text("size").fixed_decimals(2))
                    .on_hover_text("Small room to huge hall: bigger rings longer");
                ui.add(egui::Slider::new(&mut reverb.damping, 0.0..=1.0).text("damping").fixed_decimals(2))
                    .on_hover_text("How soon the highs die away: soft furnishings (high) or bare stone (low)");
                ui.add(egui::Slider::new(&mut reverb.mix, 0.0..=1.0).text("mix").fixed_decimals(2))
                    .on_hover_text("From the dry original (0) to only the reverb (1)");
            });
            self.edits.reverb = on.then_some(reverb);
        });
    }

    fn volume_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Volume");
            let label = match self.edits.volume {
                Volume::Gain(_) => "Adjust",
                Volume::Peak(_) => "Normalize peak",
                Volume::Loudness(_) => "Normalize loudness",
            };
            egui::ComboBox::from_id_salt("volume-mode").selected_text(label).show_ui(ui, |ui| {
                let v = &mut self.edits.volume;
                if ui.selectable_label(matches!(v, Volume::Gain(_)), "Adjust").on_hover_text("Turn it up or down by a fixed amount").clicked() {
                    *v = Volume::Gain(0.0);
                }
                if ui.selectable_label(matches!(v, Volume::Peak(_)), "Normalize peak").on_hover_text("Scale so the loudest moment reaches a set level").clicked() {
                    *v = Volume::Peak(-1.0);
                }
                if ui.selectable_label(matches!(v, Volume::Loudness(_)), "Normalize loudness").on_hover_text("Scale to a set perceived loudness (EBU R128), so files sound equally loud").clicked() {
                    *v = Volume::Loudness(-14.0);
                }
            });
            match &mut self.edits.volume {
                Volume::Gain(db) => {
                    ui.add(egui::Slider::new(db, -24.0..=24.0).suffix(" dB").fixed_decimals(1));
                }
                Volume::Peak(db) => {
                    ui.add(egui::Slider::new(db, -12.0..=0.0).suffix(" dBFS").fixed_decimals(1))
                        .on_hover_text("−1 dBFS leaves a little room for lossy formats");
                }
                Volume::Loudness(lufs) => {
                    ui.add(egui::Slider::new(lufs, -30.0..=-8.0).suffix(" LUFS").fixed_decimals(0))
                        .on_hover_text("−14 suits music streaming, −16 podcasts, −23 broadcast (EBU R128)");
                }
            }
            ui.separator();
            match self.measurement() {
                None => {
                    ui.spinner();
                    ui.weak("measuring…");
                }
                Some(m) => {
                    let peak = if m.peak > 0.0 { format!("{:.1} dBFS", 20.0 * m.peak.log10()) } else { "silent".into() };
                    let loud = if m.loudness.is_finite() { format!("{:.1} LUFS", m.loudness) } else { "– LUFS".into() };
                    let (gain, limited) = self.volume();
                    let mut text = format!("Peak {peak} · {loud}");
                    if self.edits.volume.measures() {
                        text += &format!(" → {:+.1} dB", 20.0 * gain.log10());
                    }
                    ui.weak(text).on_hover_text("Measured with every other change applied, before the volume change");
                    // Armed isn't the same as working: say so only when it will turn peaks down.
                    if limited && m.peak * gain > CEILING {
                        ui.colored_label(ui.visuals().warn_fg_color, "⚠ peaks limited").on_hover_text(
                            "This would push peaks past 0 dB, so a limiter turns them down just enough to keep them from clipping.",
                        );
                    }
                }
            }
        });
    }

    fn filter_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Filters");
            let cut = |ui: &mut egui::Ui, value: &mut Option<f32>, name: &str, help: &str, default: f32, range: std::ops::RangeInclusive<f32>| {
                let mut on = value.is_some();
                let mut hz = value.unwrap_or(default);
                ui.checkbox(&mut on, name).on_hover_text(help);
                ui.add_enabled(on, egui::Slider::new(&mut hz, range).logarithmic(true).suffix(" Hz").integer());
                *value = on.then_some(hz);
            };
            cut(ui, &mut self.edits.low_cut, "Low-cut", "Remove low rumble and hum below this pitch", 80.0, 20.0..=1000.0);
            cut(ui, &mut self.edits.high_cut, "High-cut", "Remove hiss and harsh highs above this pitch", 8000.0, 1000.0..=20000.0);
            if ui.button("🗣 Voice").on_hover_text("Low-cut at 100 Hz and high-cut at 8 kHz: keeps speech, drops rumble and hiss").clicked() {
                (self.edits.low_cut, self.edits.high_cut) = (Some(100.0), Some(8000.0));
            }
            ui.separator();
            if ui.toggle_value(&mut self.edits.reverse, "⇆ Reverse").on_hover_text("Play the kept part backwards").changed() {
                self.cursor = self.result_start();
            }
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
        self.cursor = self.result_start();
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
            self.seek(self.result_start());
        }
        if pressed(Modifiers::NONE, Key::End) {
            self.seek(self.result_end());
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
        let length = self.result_secs();
        // A channel setting that matches the source isn't an edit.
        let edits = self.edits.without_noops(self.info.channels);
        if edits.is_noop() {
            return Outcome::Done { edits: None, thumb: None, length };
        }
        let thumb = self.thumbnail();
        Outcome::Done { edits: Some(Edits { ops: vec![EditOp::Audio(edits)] }), thumb: Some(thumb), length }
    }

    /// The result's waveform, for the row.
    fn thumbnail(&self) -> egui::ColorImage {
        let [w, h] = THUMB;
        let mut img = egui::ColorImage::filled(THUMB, egui::Color32::TRANSPARENT);
        let plan = self.plan();
        let (a, b) = self.kept();
        // After the channel setting, if it changes the picture (done once, on Done).
        let rendered = self.channel_view().and_then(|c| self.render_channels(c, a, b, &AtomicBool::new(false)));
        let len = plan.output_len(b - a);
        let ch = self.info.channels as usize;
        let color = egui::Color32::from_rgb(WAVE_RGB[0], WAVE_RGB[1], WAVE_RGB[2]);
        let mid = h as f32 / 2.0;
        let (vol, limited) = self.volume();
        for x in 0..w {
            // Output frames o0..o1, in the order they'll be heard.
            let o0 = len * x as u64 / w as u64;
            let o1 = (len * (x as u64 + 1) / w as u64).max(o0 + 1);
            let (s0, s1) = ((o0 as f64 * plan.speed) as u64, ((o1 as f64 * plan.speed) as u64).min(b - a));
            let (f0, f1) = if plan.reverse { (b - s1.max(s0 + 1), b - s0) } else { (a + s0, a + s1.max(s0 + 1)) };
            let (lo, hi) = match &rendered {
                Some((v, rch)) => scan(v, *rch, (f0 - a) as usize, (f1 - a) as usize),
                None => self.peaks.range(&self.samples, ch, f0 as usize, f1 as usize),
            };
            let g = plan.gain((o0 + o1) / 2, len) * vol;
            let scale = |v: f32| if limited { (v * g).clamp(-CEILING, CEILING) } else { (v * g).clamp(-1.0, 1.0) };
            let top = (mid - scale(hi) * mid).floor().max(0.0) as usize;
            let bottom = (mid - scale(lo) * mid).ceil().min(h as f32 - 1.0) as usize;
            for y in top.min(h / 2)..=bottom.max(h / 2) {
                img[(x, y)] = color;
            }
        }
        img
    }
}

/// Frames `lo..hi` of `samples` after the channel setting `c`, interleaved, with their
/// channel count. A little audio before `lo` is run first so the widening's delay and filter
/// have settled, as they have in the full render.
fn render_channels(samples: Arc<[f32]>, info: &StreamInfo, c: Channels, lo: u64, hi: u64, cancel: &AtomicBool) -> Option<(Vec<f32>, usize)> {
    let warm = (info.sample_rate as u64 / 20).min(lo);
    let edits = AudioEdits { channels: c, ..Default::default() };
    let mut src = with_channels(Box::new(MemorySource::new(samples, info, lo - warm, Some(hi))), &edits);
    let ch = src.info().channels as usize;
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = src.next_chunk() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        out.extend_from_slice(chunk);
    }
    out.drain(..(warm as usize * ch).min(out.len()));
    Some((out, ch))
}

/// A combo box of audio tracks; returns a newly picked one.
fn track_picker(ui: &mut egui::Ui, tracks: &[AudioTrack], current: Option<u32>) -> Option<u32> {
    let name = |i: usize, t: &AudioTrack| format!("Track {}: {}", i + 1, t.label);
    let shown = current.and_then(|c| tracks.get(c as usize).map(|t| name(c as usize, t))).unwrap_or_else(|| "Pick a track".into());
    let mut picked = None;
    egui::ComboBox::from_id_salt("audio-track")
        .selected_text(shown)
        .show_ui(ui, |ui| {
            for (i, t) in tracks.iter().enumerate() {
                let label = if t.supported { name(i, t) } else { format!("{} (can't be read)", name(i, t)) };
                let resp = ui.add_enabled(t.supported, egui::Button::selectable(current == Some(i as u32), label));
                if resp.clicked() && current != Some(i as u32) {
                    picked = Some(i as u32);
                }
            }
        })
        .response
        .on_hover_text("This file has several audio tracks (languages, commentary…): pick the one to use");
    picked
}

/// A time text box: shows `value` until edited; returns a newly typed value on Enter or
/// leaving the box.
pub(crate) fn time_field(ui: &mut egui::Ui, text: &mut String, value: f64) -> Option<f64> {
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

    #[test]
    fn reversing_swaps_where_the_result_starts_and_which_fade_is_where() {
        let mut s = session(5);
        s.set_trim(1.0, 3.0);
        assert_eq!((s.result_start(), s.result_end()), (1000, 2999));
        s.edits.reverse = true;
        assert_eq!((s.result_start(), s.result_end()), (2999, 1000));
        // The left grip is now the fade-out (heard last), and fades can't overlap.
        s.set_fade_side(true, 0.5);
        s.set_fade_side(false, 5.0);
        assert_eq!((s.edits.fade_out, s.edits.fade_in), (0.5, 1.5));
    }

    #[test]
    fn normalizing_uses_a_background_measurement_of_the_kept_part() {
        let ctx = egui::Context::default();
        let mut s = session(5);
        s.edits.volume = Volume::Peak(-6.0);
        assert_eq!(s.volume(), (1.0, true), "no measurement yet: unchanged level, limiter armed to be safe");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.measurement().is_none() {
            s.update_measurement(&ctx, false);
            assert!(std::time::Instant::now() < deadline, "measurement never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (gain, limited) = s.volume();
        assert!((0.5 * gain - 10f32.powf(-6.0 / 20.0)).abs() < 1e-4 && !limited);
        // Boosting past the ceiling engages the limiter.
        s.edits.volume = Volume::Gain(12.0);
        assert!(s.volume().1);
        // Changing the volume keeps it; any other change (an effect, a trim) needs a new one.
        s.edits.volume = Volume::Loudness(-20.0);
        assert!(s.measurement().is_some());
        s.edits.reverb = Some(Reverb::HALL);
        assert!(s.measurement().is_none() && s.has_tail());
        s.edits.reverb = None;
        assert!(s.measurement().is_some());
        s.set_trim(1.5, 2.5);
        assert!(s.measurement().is_none());
    }

    #[test]
    fn speed_changes_the_result_length_and_fades_stay_in_output_time() {
        let mut s = session(5);
        s.set_trim(1.0, 3.0);
        s.edits.speed = 2.0;
        assert!((s.result_secs() - 1.0).abs() < 1e-9);
        // A fade can't be longer than the (now shorter) result.
        s.set_fade_side(true, 5.0);
        assert!((s.edits.fade_in - 1.0).abs() < 1e-9);
        s.cursor = 2000;
        assert!((s.result_position() - 0.5).abs() < 1e-9, "1 s into the kept part is 0.5 s into the result");
        let Outcome::Done { length, thumb: Some(t), .. } = s.done() else { panic!("expected edits") };
        assert!((length - 1.0).abs() < 1e-9);
        assert_eq!(t.size, THUMB);
    }

    #[test]
    fn picking_the_layout_a_file_already_has_is_not_an_edit() {
        let mut s = session(5);
        s.edits.channels = Channels::Stereo { width: 0.0 };
        assert!(matches!(s.done(), Outcome::Done { edits: None, .. }), "the test file is already stereo");
        s.edits.channels = Channels::Mono;
        let Outcome::Done { edits: Some(e), .. } = s.done() else { panic!("mono is a change") };
        assert_eq!(e.audio().unwrap().channels, Channels::Mono);
    }

    /// Rows of the thumbnail with any waveform pixel in them.
    fn inked_rows(img: &egui::ColorImage) -> usize {
        let [w, h] = img.size;
        (0..h).filter(|&y| (0..w).any(|x| img[(x, y)].a() > 0)).count()
    }

    #[test]
    fn the_waveform_and_thumbnail_follow_the_channel_setting() {
        // The loud part is +0.5 left and -0.5 right: mixed to mono it cancels out.
        let mut s = session(5);
        s.set_trim(1.0, 3.0);
        assert!(inked_rows(&s.thumbnail()) > THUMB[1] / 2, "as stereo it's loud");
        s.edits.channels = Channels::Mono;
        assert!(inked_rows(&s.thumbnail()) <= 2, "mixed to mono it's silent");

        // Close up, the visible stretch is rendered (and cached).
        let (start, data, ch) = s.visible_slice(Channels::Mono, 1500, 1600).unwrap();
        assert_eq!((start, data.len(), ch), (1500, 100, 1));
        assert!(data.iter().all(|v| v.abs() < 1e-6));
        let (_, again, _) = s.visible_slice(Channels::Mono, 1500, 1600).unwrap();
        assert!(Arc::ptr_eq(&data, &again), "same view, same render");

        // Further out, whole-file peaks come from the background.
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !s.display.as_ref().is_some_and(|d| d.0 == Channels::Mono) {
            s.update_display(&ctx, false);
            assert!(std::time::Instant::now() < deadline, "peaks never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (lo, hi) = s.display.as_ref().unwrap().1.blocks(1000, 3000);
        assert!(lo.abs() < 1e-6 && hi.abs() < 1e-6);
        // Back to stereo (already the source's layout): the source waveform again.
        s.edits.channels = Channels::Stereo { width: 0.0 };
        assert_eq!(s.channel_view(), None);
    }

    #[test]
    fn widening_a_mono_file_shows_its_raised_peaks() {
        let rate = 8000u32;
        let samples: Vec<f32> = (0..rate as usize).map(|i| (i as f32 * 0.9).sin() * 0.5).collect();
        let info = StreamInfo { sample_rate: rate, channels: 1, source_bits: Some(16), total_frames: Some(rate as u64), source_rate: rate };
        let peaks = Peaks::new(&samples, 1);
        let mut s = Session::new(samples.into(), info, peaks, AudioEdits::default());
        s.edits.channels = Channels::Stereo { width: 1.0 };
        let (_, data, ch) = s.visible_slice(Channels::Stereo { width: 1.0 }, 2000, 4000).unwrap();
        assert_eq!(ch, 2);
        let (_, hi) = scan(&data, 2, 0, 2000);
        assert!(hi > 0.6, "widening adds to one side: peak {hi}");
    }

    #[test]
    fn the_loader_lists_a_videos_tracks_and_decodes_the_picked_one() {
        let ctx = egui::Context::default();
        let path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-audio/tests/fixtures/video_two_tracks.mkv"));
        let (rx, _cancel) = AudioEditor::load(&ctx, path, Format::Mkv, Some(1));
        let (tracks, decoded) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(tracks.len(), 2);
        let (samples, info, _) = decoded.unwrap();
        let ups = samples.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let hz = ups as f64 * info.sample_rate as f64 / samples.len() as f64;
        assert!((hz - 880.0).abs() < 15.0, "track 2 is the 880 Hz one, got {hz}");
    }
}
