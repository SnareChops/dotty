//! A thread that owns an opened video (or a picture, or a song's picture) for the editors:
//! still frames on request (newest wins), and on `Play` frames in order into a small bounded
//! queue so decoding runs only a little ahead of the clock. Shared by the video editor and
//! the frame picker.

use codecs_audio::{AudioConverter, AudioTrack};
use codecs_video::image::{self, RgbImage};
use codecs_video::{VideoInfo, VideoReader};
use dotty_core::Format;
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc;

/// Longest side of preview frames (the editors never need the full size).
pub(crate) const PREVIEW_SIDE: u32 = 960;
/// How long a picture lasts as a video when no sound is added (mirrors the conversion).
pub(crate) const STILL_SECS: f64 = codecs_video::output::STILL_SECS;

pub(crate) struct Ready {
    /// `None` for a picture or a song.
    pub(crate) video: Option<VideoInfo>,
    /// A song becoming a video: a picture whose length is the song's.
    pub(crate) song: bool,
    /// Coded size and the file's own rotation.
    pub(crate) size: (u32, u32),
    pub(crate) rotation: u16,
    pub(crate) duration: f64,
    pub(crate) fps: f64,
    pub(crate) tracks: Vec<AudioTrack>,
}

pub(crate) enum Msg {
    Ready(Result<Ready, String>),
    Frame(RgbImage),
    /// A song's picture was changed: its full size and the picture.
    Picture((u32, u32), RgbImage),
    /// A playback ran out of frames (its generation).
    Ended(u64),
    Failed(String),
}

pub(crate) enum Cmd {
    /// Decode the frame on screen at this time.
    Seek(f64),
    /// A song's video: use this picture (`None` = its cover art).
    Picture(Option<PathBuf>),
    /// Decode frames in order from this time, for playback number `run`, at this speed.
    Play(f64, u64, f64),
}

/// Sizes (longest side) frames are decoded at during playback: as large as the stills when the
/// machine keeps up, smaller when it doesn't.
pub(crate) const PLAY_SIDES: [u32; 3] = [480, 720, PREVIEW_SIDE];

/// The size to decode the next frames at, given what a frame has been costing (decode and
/// colour conversion, seconds) and the time available for one (`interval`).
pub(crate) fn play_side(current: u32, cost: f64, interval: f64) -> u32 {
    let i = PLAY_SIDES.iter().position(|&s| s == current).unwrap_or(PLAY_SIDES.len() - 1);
    if cost > interval * 0.85 && i > 0 {
        return PLAY_SIDES[i - 1];
    }
    if let Some(&bigger) = PLAY_SIDES.get(i + 1) {
        // The next size up costs about its pixel ratio more (decoding itself doesn't change,
        // so this errs on the cautious side).
        let k = (bigger as f64 / current as f64).powi(2);
        if cost * k < interval * 0.6 {
            return bigger;
        }
    }
    current
}

/// A frame of a running playback: its generation, source time and picture.
pub(crate) type PlayFrame = (u64, f64, RgbImage);

/// A thread owning the video: still frames on request (newest wins) and, on `Play`, frames
/// in order into a small bounded queue (so decoding runs only a little ahead).
pub(crate) struct Worker {
    pub(crate) requests: mpsc::Sender<Cmd>,
    pub(crate) replies: mpsc::Receiver<Msg>,
    pub(crate) play: mpsc::Receiver<PlayFrame>,
}
pub(crate) fn shrink(img: RgbImage) -> RgbImage {
    let longest = img.width().max(img.height());
    if longest <= PREVIEW_SIDE {
        return img;
    }
    let k = PREVIEW_SIDE as f32 / longest as f32;
    image::imageops::thumbnail(&img, ((img.width() as f32 * k) as u32).max(1), ((img.height() as f32 * k) as u32).max(1))
}

impl Worker {
    pub(crate) fn start(ctx: &egui::Context, path: PathBuf, from: Format, chosen: Option<PathBuf>) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Cmd>();
        let (tx, replies) = mpsc::channel();
        let (play_tx, play) = mpsc::sync_channel::<PlayFrame>(3);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            if from.kind() == dotty_core::Kind::Audio {
                // A song: its picture (chosen, cover art or plain) and its length.
                let (options, cancel, progress) = (dotty_core::Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
                let cx = dotty_core::ConvertCtx::new(&options, &cancel, &progress);
                let load = |file: Option<&std::path::Path>| codecs_video::output::song_picture(&path, from, file, &cx).map(|p| p.0).map_err(|e| e.to_string());
                let secs = match AudioConverter::default().open(&path, from) {
                    Ok(mut src) => {
                        let rate = src.info().sample_rate as f64;
                        match src.info().total_frames {
                            Some(t) => t as f64 / rate,
                            // Length not in the file: count it.
                            None => {
                                let (mut n, ch) = (0u64, src.info().channels as u64);
                                while let Ok(Some(c)) = src.next_chunk() {
                                    n += c.len() as u64 / ch;
                                }
                                n as f64 / rate
                            }
                        }
                    }
                    Err(e) => return send(Msg::Ready(Err(e.to_string()))),
                };
                let img = match load(chosen.as_deref()) {
                    Ok(i) => i,
                    Err(e) => return send(Msg::Ready(Err(e))),
                };
                send(Msg::Ready(Ok(Ready { video: None, song: true, size: (img.width(), img.height()), rotation: 0, duration: secs, fps: 5.0, tracks: Vec::new() })));
                let mut shown = shrink(img);
                while let Ok(cmd) = req_rx.recv() {
                    match cmd {
                        Cmd::Picture(file) => match load(file.as_deref()) {
                            Ok(img) => {
                                let size = (img.width(), img.height());
                                shown = shrink(img);
                                send(Msg::Picture(size, shown.clone()));
                            }
                            Err(e) => send(Msg::Failed(e)),
                        },
                        _ => send(Msg::Frame(shown.clone())),
                    }
                }
                return;
            }
            if from.kind() == dotty_core::Kind::Image {
                // A picture: decoded once, shown at every time.
                let (options, cancel, progress) = (dotty_core::Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
                let cx = dotty_core::ConvertCtx::new(&options, &cancel, &progress);
                let img = match codecs_image::ImageConverter::default().decode(&path, from, 1, &cx) {
                    Ok(d) => d.first().to_rgb8(),
                    Err(e) => return send(Msg::Ready(Err(e.to_string()))),
                };
                send(Msg::Ready(Ok(Ready { video: None, song: false, size: (img.width(), img.height()), rotation: 0, duration: STILL_SECS, fps: 5.0, tracks: Vec::new() })));
                let img = shrink(img);
                while req_rx.recv().is_ok() {
                    send(Msg::Frame(img.clone()));
                }
                return;
            }
            let mut reader = match VideoReader::open(&path, from, 4).and_then(|mut r| r.keyframes().map(|_| r)) {
                Ok(r) => r,
                Err(e) => return send(Msg::Ready(Err(e.to_string()))),
            };
            let info = reader.info().clone();
            let tracks = AudioConverter::default().tracks(&path, from);
            send(Msg::Ready(Ok(Ready {
                song: false,
                size: (info.width, info.height),
                rotation: info.rotation,
                duration: info.duration.unwrap_or(0.0),
                fps: info.fps().unwrap_or(30.0),
                tracks: if tracks.len() > 1 { tracks } else { Vec::new() },
                video: Some(info.clone()),
            })));
            let mut held: Option<Cmd> = None;
            // Playback starts at the stills' size and adapts.
            let mut side = PREVIEW_SIDE;
            loop {
                let mut cmd = match held.take().map(Ok).unwrap_or_else(|| req_rx.recv()) {
                    Ok(c) => c,
                    Err(_) => return,
                };
                // Only the newest request matters (the slider may have moved on).
                while let Ok(newer) = req_rx.try_recv() {
                    cmd = newer;
                }
                match cmd {
                    Cmd::Picture(_) => {}
                    Cmd::Seek(t) => match reader.seek(t).and_then(|_| reader.next_picture()) {
                        Ok(Some(p)) => send(Msg::Frame(shrink(p.to_rgb8_fit(PREVIEW_SIDE)))),
                        Ok(None) => {}
                        Err(e) => send(Msg::Failed(e.to_string())),
                    },
                    Cmd::Play(t, run, speed) => {
                        if let Err(e) = reader.seek(t) {
                            send(Msg::Failed(e.to_string()));
                            continue;
                        }
                        // Seconds the clock gives us per frame; the cost so far (smoothed).
                        let interval = 1.0 / (info.fps().unwrap_or(30.0) * speed.max(0.1));
                        let mut cost: Option<f64> = None;
                        let mut frames = 0u32;
                        'play: loop {
                            let began = std::time::Instant::now();
                            match reader.next_picture() {
                                Ok(Some(p)) => {
                                    let mut item = (run, p.pts, p.to_rgb8_fit(side));
                                    let took = began.elapsed().as_secs_f64();
                                    cost = Some(cost.map_or(took, |c| c * 0.8 + took * 0.2));
                                    frames += 1;
                                    if frames.is_multiple_of(8) {
                                        side = play_side(side, cost.unwrap_or(0.0), interval);
                                    }
                                    loop {
                                        match play_tx.try_send(item) {
                                            Ok(()) => break,
                                            Err(mpsc::TrySendError::Full(back)) => {
                                                item = back;
                                                // Full: wait for the clock to catch up, but heed new requests.
                                                if let Ok(c) = req_rx.try_recv() {
                                                    held = Some(c);
                                                    break 'play;
                                                }
                                                std::thread::sleep(std::time::Duration::from_millis(4));
                                            }
                                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                                        }
                                    }
                                    if let Ok(c) = req_rx.try_recv() {
                                        held = Some(c);
                                        break;
                                    }
                                }
                                Ok(None) => {
                                    send(Msg::Ended(run));
                                    break;
                                }
                                Err(e) => {
                                    send(Msg::Failed(e.to_string()));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
        Self { requests: req_tx, replies, play }
    }
}

/// Pictures decoded ahead of the clock.
#[derive(Default)]
pub(crate) struct FrameQueue {
    pub(crate) frames: std::collections::VecDeque<(f64, RgbImage)>,
}

impl FrameQueue {
    pub(crate) fn push(&mut self, pts: f64, img: RgbImage) {
        self.frames.push_back((pts, img));
    }

    /// The picture on screen at `clock` (source seconds): the latest one due, dropping the
    /// ones it replaced; `None` until the first comes due.
    pub(crate) fn at(&mut self, clock: f64) -> Option<(f64, &RgbImage)> {
        while self.frames.len() >= 2 && self.frames[1].0 <= clock {
            self.frames.pop_front();
        }
        self.frames.front().filter(|f| f.0 <= clock + 0.02).map(|f| (f.0, &f.1))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }
}
