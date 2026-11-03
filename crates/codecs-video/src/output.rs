//! Writing videos: WebM / MKV (our writer) and MP4 (`muxide`), VP9 or AV1 with an Opus
//! soundtrack. Sources are videos, images (stills and animations) and audio files.

use crate::encode::{Av1Encoder, Packet, VideoEncoder, Vp9Encoder, Yuv420};
use crate::mkv::{self, MkvWriter};
use codecs_audio::AudioConverter;
use codecs_audio::opus::OpusPackets;
use dotty_core::{Choice, ConvertCtx, Error, Format, Result, VideoEdits, WriteSeek};
use image::{Rgb, RgbImage};
use std::path::Path;

/// Formats we write video in.
pub const VIDEO_OUTPUTS: [Format; 3] = [Format::Webm, Format::Mkv, Format::Mp4];

pub const VIDEO_CODEC: Choice = Choice {
    key: "vcodec",
    label: "Video codec",
    help: "VP9 plays almost everywhere and is quick to make. AV1 makes files about a third smaller but takes far longer to convert (and older devices can't play it).",
    values: &[("vp9", "VP9"), ("av1", "AV1 (smaller, much slower)")],
    multi_image_only: false,
};

/// A still picture made into a video lasts this long (seconds), at this frame rate (low:
/// nothing moves, and players seek fine at 5 fps).
pub const STILL_SECS: f64 = 5.0;
pub const STILL_FPS: f64 = 5.0;
/// Largest side of a video made from a picture (cover art, a still).
pub const MAX_PICTURE_SIDE: u32 = 1920;
/// Background of a song's video when it has no cover art.
pub const NO_ART: (u32, u32, [u8; 3]) = (1280, 720, [32, 32, 32]);

fn io(e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("write failed: {e}"))
}

enum Sink<'a> {
    Mkv(MkvWriter<'a>),
    Mp4(Box<muxide::api::Muxer<&'a mut dyn WriteSeek>>),
}

/// Encodes frames and writes them, with the soundtrack interleaved by time.
pub struct Writer<'a> {
    encoder: Box<dyn VideoEncoder>,
    sink: Sink<'a>,
    audio: Option<OpusPackets>,
    /// Next audio packet to write.
    next_audio: usize,
    /// Seconds the audio's block times are shifted back by (Matroska: the encoder delay).
    audio_shift: f64,
    size: (u32, u32),
    last_pts: f64,
    wrote_video: bool,
}

impl<'a> Writer<'a> {
    pub fn new(out: &'a mut dyn WriteSeek, to: Format, ctx: &ConvertCtx, size: (u32, u32), fps: f64, audio: Option<OpusPackets>) -> Result<Self> {
        let (w, h) = (size.0 & !1, size.1 & !1);
        let av1 = ctx.options.choice(to, &VIDEO_CODEC) == "av1";
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 8);
        let mut encoder: Box<dyn VideoEncoder> = if av1 {
            Box::new(Av1Encoder::new(ctx.options.preset, fps, w, h, threads)?)
        } else {
            Box::new(Vp9Encoder::new(ctx.options.preset, fps)?)
        };
        let (sink, audio_shift) = match to {
            Format::Webm | Format::Mkv => {
                let matrix = if crate::iso::uses_709(h) { 1 } else { 6 };
                let video = mkv::VideoTrack { codec: if av1 { "V_AV1" } else { "V_VP9" }, width: w, height: h, private: encoder.config(), matrix };
                let track = audio.as_ref().map(|a| mkv::AudioTrack {
                    channels: a.channels,
                    private: a.head(),
                    delay_ns: a.pre_skip as u64 * 1_000_000_000 / 48000,
                });
                let shift = audio.as_ref().map_or(0.0, |a| a.pre_skip as f64 / 48000.0);
                (Sink::Mkv(MkvWriter::new(out, to == Format::Webm, &video, track.as_ref())?), shift)
            }
            Format::Mp4 => {
                use muxide::api::{AudioCodec, MuxerBuilder, VideoCodec};
                let codec = if av1 { VideoCodec::Av1 } else { VideoCodec::Vp9 };
                let mut b = MuxerBuilder::new(out).video(codec, w, h, fps).with_fast_start(true);
                if let Some(a) = &audio {
                    b = b.audio(AudioCodec::Opus, 48000, a.channels as u16);
                }
                (Sink::Mp4(Box::new(b.build().map_err(io)?)), 0.0)
            }
            _ => return Err(Error::Codec(format!("can't write video as {to}"))),
        };
        Ok(Self { encoder, sink, audio, next_audio: 0, audio_shift, size: (w, h), last_pts: 0.0, wrote_video: false })
    }

    /// The frame size every frame must have.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    fn audio_pts(&self, i: usize) -> f64 {
        i as f64 * OpusPackets::PACKET_SECS - self.audio_shift
    }

    /// Write audio up to (and including) time `t`.
    fn audio_until(&mut self, t: f64) -> Result<()> {
        while let Some(a) = &self.audio {
            let i = self.next_audio;
            if i >= a.packets.len() || self.audio_pts(i) > t {
                break;
            }
            let pts = self.audio_pts(i);
            let data = std::mem::take(&mut self.audio.as_mut().expect("checked").packets[i]);
            match &mut self.sink {
                Sink::Mkv(m) => m.write(mkv::AUDIO, (pts * 1000.0).round() as i64, true, &data)?,
                Sink::Mp4(m) => m.write_audio(pts.max(0.0), &data).map_err(io)?,
            }
            self.next_audio += 1;
        }
        Ok(())
    }

    fn put(&mut self, packets: Vec<Packet>) -> Result<()> {
        for p in packets {
            match &mut self.sink {
                // Matroska blocks in time order: the sound up to this frame first.
                Sink::Mkv(_) => {
                    self.audio_until(p.pts)?;
                    if let Sink::Mkv(m) = &mut self.sink {
                        m.write(mkv::VIDEO, (p.pts * 1000.0).round() as i64, p.key, &p.data)?;
                    }
                }
                // muxide wants a video frame before any sound; it orders samples itself.
                Sink::Mp4(m) => {
                    m.write_video(p.pts, &p.data, p.key).map_err(io)?;
                    self.audio_until(p.pts)?;
                }
            }
            self.wrote_video = true;
            self.last_pts = self.last_pts.max(p.pts);
        }
        Ok(())
    }

    /// Add a frame shown from `pts` seconds.
    pub fn push(&mut self, frame: &Yuv420, pts: f64) -> Result<()> {
        if (frame.width, frame.height) != self.size {
            return Err(Error::Codec("internal: frame size changed".into()));
        }
        let packets = self.encoder.encode(frame, pts)?;
        self.put(packets)
    }

    /// Flush everything; `end` is when the last frame stops showing (the video's length).
    pub fn finish(mut self, end: f64) -> Result<()> {
        let packets = self.encoder.finish()?;
        self.put(packets)?;
        if !self.wrote_video {
            return Err(Error::Codec("no frames to write".into()));
        }
        self.audio_until(f64::INFINITY)?;
        let duration = end.max(self.last_pts).max(self.audio.as_ref().map_or(0.0, |a| a.duration()));
        match self.sink {
            Sink::Mkv(m) => m.finish(duration * 1000.0),
            Sink::Mp4(m) => m.finish().map_err(io),
        }
    }
}

/// A picture as an even-sized frame, no larger than `max_side`.
pub fn picture_frame(img: &RgbImage, max_side: u32) -> Yuv420 {
    let longest = img.width().max(img.height());
    if longest > max_side {
        let k = max_side as f64 / longest as f64;
        let resized = image::imageops::resize(img, ((img.width() as f64 * k) as u32).max(2), ((img.height() as f64 * k) as u32).max(2), image::imageops::FilterType::CatmullRom);
        Yuv420::from_rgb(&resized)
    } else {
        Yuv420::from_rgb(img)
    }
}

/// A frame resized to `size` if it isn't already (sources whose frames change size).
pub fn fit(frame: Yuv420, size: (u32, u32), rgb: impl FnOnce() -> RgbImage) -> Yuv420 {
    if (frame.width, frame.height) == size {
        return frame;
    }
    let img = image::imageops::resize(&rgb(), size.0, size.1, image::imageops::FilterType::Triangle);
    Yuv420::from_rgb(&img)
}

/// A file's sound for a video, or `None` (with a note when there was sound we couldn't use).
pub fn soundtrack(src: &Path, from: Format, ctx: &ConvertCtx) -> Result<Option<OpusPackets>> {
    let track = ctx.edits.and_then(|e| e.audio()).and_then(|a| a.track);
    soundtrack_with(src, from, track, ctx.edits.and_then(|e| e.audio()).copied(), ctx)
}

/// [`soundtrack`] with explicit audio edits.
pub fn soundtrack_with(src: &Path, from: Format, track: Option<u32>, edits: Option<dotty_core::AudioEdits>, ctx: &ConvertCtx) -> Result<Option<OpusPackets>> {
    match AudioConverter::default().soundtrack_with(src, from, track, edits, ctx) {
        Ok(p) => Ok(Some(p)),
        Err(Error::Cancelled) => Err(Error::Cancelled),
        Err(Error::Codec(m)) if m == "this video has no sound" || m == "no audio track found" => Ok(None),
        Err(Error::Codec(m)) => {
            ctx.note(format!("The sound was left out: {m}"));
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The sound for a video made from a video or picture: the edits' own audio file, the video's
/// own track (trimmed, sped up…) or none.
fn edited_sound(src: &Path, from: Format, ve: &VideoEdits, result_secs: Option<f64>, ctx: &ConvertCtx) -> Result<Option<OpusPackets>> {
    if ve.mute {
        return Ok(None);
    }
    if let Some(file) = &ve.audio_file {
        let Some(format) = Format::detect(file).filter(|f| matches!(f.kind(), dotty_core::Kind::Audio | dotty_core::Kind::Video)) else {
            return Err(Error::Codec(format!("{} isn't a sound or video file Dotty can read", file.display())));
        };
        // A replacement soundtrack starts with the video: the part picked (from `audio_start`),
        // no longer than the video if its length is known; fades and volume apply.
        let longest = result_secs.map(|r| ve.audio_start + r);
        let trim_end = match (ve.audio_end, longest) {
            (Some(e), Some(l)) => Some(e.min(l)),
            (e, l) => e.or(l),
        };
        let edits = dotty_core::AudioEdits {
            trim_start: ve.audio_start,
            trim_end,
            fade_in: ve.fade_in,
            fade_out: ve.fade_out,
            volume: dotty_core::Volume::Gain(ve.volume_db),
            ..Default::default()
        };
        return soundtrack_with(file, format, None, Some(edits), ctx);
    }
    if from.kind() == dotty_core::Kind::Image {
        return Ok(None);
    }
    soundtrack_with(src, from, ve.track, Some(ve.audio()), ctx)
}

/// Video → video: the kept part re-encoded with the edits applied (rotation baked in), at
/// the result's frame rate, and the soundtrack as Opus.
pub fn from_video(src: &Path, from: Format, ctx: &ConvertCtx, out: &mut dyn WriteSeek, to: Format) -> Result<()> {
    let ve = ctx.edits.and_then(|e| e.video()).cloned().unwrap_or_default();
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
    let mut r = crate::VideoReader::open(src, from, threads)?;
    r.keyframes()?;
    let info = r.info().clone();
    let total = info.duration.unwrap_or(0.0).max(0.001);
    let start = ve.trim_start.clamp(0.0, total);
    let end = ve.trim_end.unwrap_or(total).clamp(start, total);
    if end - start < 0.001 {
        return Err(Error::Codec("the trimmed part of the video is empty".into()));
    }
    let speed = ve.speed.clamp(VideoEdits::MIN_SPEED, VideoEdits::MAX_SPEED);
    let result_secs = (end - start) / speed;
    let src_fps = info.fps().unwrap_or(30.0);
    let fps = ve.fps.map_or(src_fps, |f| (f as f64).min(src_fps)).max(0.1);
    let audio = edited_sound(src, from, &VideoEdits { trim_end: Some(end), trim_start: start, ..ve.clone() }, Some(result_secs), ctx)?;

    // Pictures that need no change go to the encoder as they are; edits go through RGB.
    let plain = !ve.changes_picture();
    let size = if plain { ((info.display_size().0 & !1).max(2), (info.display_size().1 & !1).max(2)) } else { crate::vedit::output_size((info.width, info.height), info.rotation, &ve) };
    let mut w = Writer::new(out, to, ctx, size, fps, audio)?;
    let mut overlays = crate::vedit::Overlays::default();

    r.seek(start)?;
    let frames = ((result_secs * fps - 1e-6).ceil() as u64).max(1);
    // Tolerance of half a step: a frame a hair after its tick still belongs to it.
    let slack = 0.5 * speed / fps;
    let mut shown: Option<crate::Picture> = None;
    let mut next = r.next_picture()?;
    for k in 0..frames {
        ctx.check_cancelled()?;
        let t = k as f64 / fps;
        let t_src = start + t * speed;
        while let Some(p) = next.take() {
            if p.pts <= t_src + slack || shown.is_none() {
                shown = Some(p);
                next = r.next_picture()?;
            } else {
                next = Some(p);
                break;
            }
        }
        let Some(p) = &shown else { break };
        let frame = if plain {
            fit(Yuv420::from_picture(p, info.rotation), size, || crate::rotate(p.to_rgb8(), info.rotation))
        } else {
            let mut img = crate::vedit::geometry(p.to_rgb8(), info.rotation, &ve);
            overlays.apply(&mut img, &ve, t, result_secs);
            Yuv420::from_rgb(&img)
        };
        w.push(&frame, t)?;
        (ctx.progress)((0.98 * (k + 1) as f32 / frames as f32).min(0.98));
    }
    w.finish(result_secs)
}

/// White behind transparent pixels (as for JPEG).
fn flatten(img: &image::DynamicImage) -> RgbImage {
    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        o.0 = [0, 1, 2].map(|c| ((p[c] as u32 * a + 255 * (255 - a) + 127) / 255) as u8);
    }
    out
}

/// Image → video: a still lasts [`STILL_SECS`]; an animation keeps its timing (once).
pub fn from_image(src: &Path, from: Format, ctx: &ConvertCtx, out: &mut dyn WriteSeek, to: Format) -> Result<()> {
    let images = codecs_image::ImageConverter::default();
    let mut img = images.decode(src, from, usize::MAX, ctx)?;
    // Edits from the image editor (crop, text, drawing…) apply before the pictures become frames.
    if let Some(edits) = ctx.edits {
        codecs_image::edit::apply(&mut img, edits);
    }
    ctx.check_cancelled()?;
    let first = picture_frame(&flatten(&img.frames[0].image), 3840);
    let size = (first.width, first.height);
    let ve = ctx.edits.and_then(|e| e.video()).cloned().unwrap_or_default();
    if img.frames.len() == 1 {
        // With a soundtrack added in the editor, the picture shows for as long as it plays.
        let audio = edited_sound(src, from, &ve, None, ctx)?;
        let secs = audio.as_ref().map_or(ve.still_secs.unwrap_or(STILL_SECS), |a| a.duration());
        let mut w = Writer::new(out, to, ctx, size, STILL_FPS, audio)?;
        let n = ((secs * STILL_FPS).ceil() as usize).max(1);
        for k in 0..n {
            ctx.check_cancelled()?;
            w.push(&first, k as f64 / STILL_FPS)?;
            (ctx.progress)(0.98 * (k + 1) as f32 / n as f32);
        }
        return w.finish(secs);
    }
    // GIF delays of 0 (or nearly) mean "as fast as the viewer likes": browsers use 0.1 s.
    let delays: Vec<f64> = img.frames.iter().map(|f| f.delay.as_secs_f64()).map(|d| if d < 0.02 { 0.1 } else { d }).collect();
    let total: f64 = delays.iter().sum();
    let mut w = Writer::new(out, to, ctx, size, img.frames.len() as f64 / total, None)?;
    let mut t = 0.0;
    for (i, (f, d)) in img.frames.iter().zip(&delays).enumerate() {
        ctx.check_cancelled()?;
        let rgb = flatten(&f.image);
        let frame = if i == 0 { first.clone() } else { fit(picture_frame(&rgb, 3840), size, || rgb.clone()) };
        w.push(&frame, t)?;
        t += d;
        (ctx.progress)(0.98 * (i + 1) as f32 / img.frames.len() as f32);
    }
    w.finish(total)
}

/// The picture a song is shown with: the one chosen, else its cover art, else a plain
/// background (`true` in the second field then). The editor's preview uses this too.
pub fn song_picture(src: &Path, from: Format, chosen: Option<&Path>, ctx: &ConvertCtx) -> Result<(RgbImage, bool)> {
    let art = match chosen {
        Some(file) => {
            let format = Format::detect(file).filter(|f| f.kind() == dotty_core::Kind::Image).ok_or_else(|| Error::Codec(format!("{} isn't a picture Dotty can read", file.display())))?;
            Some(codecs_image::ImageConverter::default().decode(file, format, 1, ctx)?.frames.swap_remove(0).image)
        }
        None => codecs_audio::cover_art(src, from).and_then(|bytes| image::load_from_memory(&bytes).ok()),
    };
    Ok(match &art {
        Some(img) => (flatten(img), false),
        None => (RgbImage::from_pixel(NO_ART.0, NO_ART.1, Rgb(NO_ART.2)), true),
    })
}

/// A picture no larger than [`MAX_PICTURE_SIDE`].
fn capped(img: RgbImage) -> RgbImage {
    let longest = img.width().max(img.height());
    if longest <= MAX_PICTURE_SIDE {
        return img;
    }
    let k = MAX_PICTURE_SIDE as f64 / longest as f64;
    image::imageops::resize(&img, ((img.width() as f64 * k) as u32).max(2), ((img.height() as f64 * k) as u32).max(2), image::imageops::FilterType::CatmullRom)
}

/// Audio → video: the sound with its picture (chosen, cover art or a plain background) for its
/// length, with the video edits' crop/turns/size, text and fades applied to the picture.
pub fn from_audio(src: &Path, from: Format, ctx: &ConvertCtx, out: &mut dyn WriteSeek, to: Format) -> Result<()> {
    let audio = AudioConverter::default().soundtrack(src, from, None, ctx)?;
    (ctx.progress)(0.3);
    let ve = ctx.edits.and_then(|e| e.video()).cloned().unwrap_or_default();
    let (picture, plain) = song_picture(src, from, ve.picture_file.as_deref(), ctx)?;
    if plain && from.kind() == dotty_core::Kind::Audio && ve.picture_file.is_none() {
        ctx.note("This file has no cover art: the video shows a plain background");
    }
    let base = crate::vedit::geometry(capped(picture), 0, &ve);
    let duration = audio.duration();
    let still = Yuv420::from_rgb(&base);
    let mut w = Writer::new(out, to, ctx, (still.width, still.height), STILL_FPS, Some(audio))?;
    let n = ((duration * STILL_FPS).ceil() as usize).max(1);
    // Text and fades change the picture only at some moments: re-encode it only then.
    let changes = !ve.texts.is_empty() || ve.fade_in > 0.0 || ve.fade_out > 0.0;
    let mut overlays = crate::vedit::Overlays::default();
    let mut frame = still;
    let mut last = None;
    for k in 0..n {
        ctx.check_cancelled()?;
        let t = k as f64 / STILL_FPS;
        if changes {
            let showing = ve.texts.iter().enumerate().fold(0u64, |m, (i, x)| if t >= x.start && x.end.is_none_or(|e| t < e) { m | 1 << (i % 64) } else { m });
            let key = (showing, (crate::vedit::fade_level(t, duration, &ve) * 64.0).round() as u32);
            if last != Some(key) {
                let mut img = base.clone();
                overlays.apply(&mut img, &ve, t, duration);
                frame = Yuv420::from_rgb(&img);
                last = Some(key);
            }
        }
        w.push(&frame, t)?;
        (ctx.progress)(0.3 + 0.68 * (k + 1) as f32 / n as f32);
    }
    w.finish(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Codec, VideoReader};
    use dotty_core::{Converter, Options};
    use std::path::PathBuf;

    fn audio_fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../codecs-audio/tests/fixtures").join(name)
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tf-video-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn convert(src: &Path, from: Format, to: Format, name: &str, options: &Options) -> (PathBuf, Vec<String>) {
        let dst = tmp(name);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let progress = |_: f32| {};
        let ctx = ConvertCtx::new(options, &cancel, &progress);
        crate::VideoConverter::default().convert(src, &dst, from, to, &ctx).unwrap_or_else(|e| panic!("{name}: {e}"));
        (dst, ctx.take_notes())
    }

    /// Frames (pts) and size of a written video, read back with our own reader.
    fn read_back(path: &Path, as_format: Format) -> (Codec, (u32, u32), Vec<f64>) {
        assert_eq!(Format::detect(path), Some(as_format), "{} detected", path.display());
        let mut r = VideoReader::open(path, as_format, 1).unwrap();
        let codec = r.info().codec;
        let mut pts = Vec::new();
        let mut size = (0, 0);
        while let Some(p) = r.next_picture().unwrap() {
            size = (p.width, p.height);
            pts.push(p.pts);
        }
        (codec, size, pts)
    }

    /// Seconds of sound and its pitch (upward zero crossings per second, first channel).
    fn sound(path: &Path, as_format: Format) -> (f64, f64) {
        let mut src = AudioConverter::default().open(path, as_format).unwrap();
        let (rate, ch) = (src.info().sample_rate as f64, src.info().channels as usize);
        let mut samples = Vec::new();
        while let Some(c) = src.next_chunk().unwrap() {
            samples.extend(c.iter().step_by(ch).copied());
        }
        let mid = &samples[samples.len() / 4..samples.len() * 3 / 4];
        let ups = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        (samples.len() as f64 / rate, ups as f64 * rate / mid.len() as f64)
    }

    #[test]
    fn videos_convert_to_webm_mkv_and_mp4_with_their_sound() {
        let src = audio_fixture("video_two_tracks.mkv");
        let av1 = {
            let mut o = Options::default();
            o.set_choice(Format::Mkv, &VIDEO_CODEC, "av1");
            o
        };
        for (to, codec, options) in [(Format::Webm, Codec::Vp9, Options::default()), (Format::Mp4, Codec::Vp9, Options::default()), (Format::Mkv, Codec::Av1, av1)] {
            let (out, notes) = convert(&src, Format::Mkv, to, &format!("clip.{}", to.extension()), &options);
            assert!(notes.is_empty(), "{to}: {notes:?}");
            let (got, size, pts) = read_back(&out, to);
            assert_eq!((got, size, pts.len()), (codec, (64, 64), 5), "{to}");
            assert!(pts.windows(2).all(|w| (w[1] - w[0] - 0.1).abs() < 0.002), "{to}: {pts:?}");
            let (secs, hz) = sound(&out, to);
            assert!((secs - 0.5).abs() < 0.03, "{to}: {secs} s of sound");
            assert!((hz - 440.0).abs() < 10.0, "{to}: the first track's 440 Hz, got {hz}");
        }
    }

    #[test]
    fn a_still_lasts_five_seconds_and_an_animation_keeps_its_timing() {
        let dir = tmp("pictures");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("still.png");
        image::RgbImage::from_pixel(40, 30, image::Rgb([200, 30, 30])).save(&png).unwrap();
        let (out, _) = convert(&png, Format::Png, Format::Webm, "still.webm", &Options::default());
        let (_, size, pts) = read_back(&out, Format::Webm);
        assert_eq!((size, pts.len()), ((40, 30), 25), "5 s at 5 fps");

        // A three-frame GIF: 0.2 s, 0.5 s, 0.3 s; the middle frame half transparent.
        let gif = dir.join("anim.gif");
        {
            use image::codecs::gif::GifEncoder;
            let mut enc = GifEncoder::new(std::fs::File::create(&gif).unwrap());
            for (ms, a) in [(200, 255u8), (500, 0), (300, 255)] {
                let img = image::RgbaImage::from_pixel(20, 20, image::Rgba([0, 0, 255, a]));
                enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(ms, 1))).unwrap();
            }
        }
        let (out, _) = convert(&gif, Format::Gif, Format::Mp4, "anim.mp4", &Options::default());
        let (_, size, pts) = read_back(&out, Format::Mp4);
        assert_eq!(size, (20, 20));
        assert_eq!(pts.len(), 3);
        assert!((pts[1] - 0.2).abs() < 0.01 && (pts[2] - 0.7).abs() < 0.01, "{pts:?}");
        let mut r = VideoReader::open(&out, Format::Mp4, 1).unwrap();
        let middle = r.frame_at(0.3).unwrap().unwrap();
        assert!(middle.get_pixel(10, 10).0.iter().all(|&c| c > 230), "transparent → white: {:?}", middle.get_pixel(10, 10));
    }

    fn convert_with(src: &Path, from: Format, to: Format, name: &str, ve: VideoEdits) -> (PathBuf, Vec<String>) {
        let edits = dotty_core::Edits { ops: vec![dotty_core::EditOp::Video(ve)] };
        let dst = tmp(name);
        let (options, cancel, progress) = (Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
        crate::VideoConverter::default().convert(src, &dst, from, to, &ctx).unwrap_or_else(|e| panic!("{name}: {e}"));
        (dst, ctx.take_notes())
    }

    #[test]
    fn trim_speed_and_frame_rate_change_what_is_written() {
        let src = audio_fixture("video_two_tracks.mkv"); // 5 frames at 10 fps, 0.5 s of 440 Hz
        // Keep 0.2–0.5 s (3 frames) at twice the speed: 0.15 s, 3 source frames → 1.5 → 2 frames at 10 fps.
        let ve = VideoEdits { trim_start: 0.2, speed: 2.0, ..Default::default() };
        let (out, notes) = convert_with(&src, Format::Mkv, Format::Webm, "fast.webm", ve);
        assert!(notes.is_empty(), "{notes:?}");
        let (_, _, pts) = read_back(&out, Format::Webm);
        assert_eq!(pts.len(), 2, "{pts:?}");
        let (secs, hz) = sound(&out, Format::Webm);
        assert!((secs - 0.15).abs() < 0.03, "{secs} s of sound");
        assert!((hz - 440.0).abs() < 15.0, "speed keeps the pitch: {hz} Hz");

        // Half the frame rate keeps every other frame; the sound is untouched.
        let (out, _) = convert_with(&src, Format::Mkv, Format::Webm, "slow-fps.webm", VideoEdits { fps: Some(5.0), ..Default::default() });
        let (_, _, pts) = read_back(&out, Format::Webm);
        assert_eq!(pts.len(), 3, "0.5 s at 5 fps: {pts:?}");
        assert!((pts[1] - 0.2).abs() < 0.01);
        assert!((sound(&out, Format::Webm).0 - 0.5).abs() < 0.03);

        // Mute: no sound track at all.
        let (out, _) = convert_with(&src, Format::Mkv, Format::Mp4, "mute.mp4", VideoEdits { mute: true, ..Default::default() });
        assert!(AudioConverter::default().open(&out, Format::Mp4).is_err(), "no sound");
        assert_eq!(read_back(&out, Format::Mp4).2.len(), 5);
    }

    #[test]
    fn picture_edits_reach_the_written_frames() {
        let src = audio_fixture("video_opus.mp4"); // 64×64
        let crop = dotty_core::Region { x: 0, y: 0, width: 32, height: 16, of_width: 64, of_height: 64 };
        let ve = VideoEdits { crop: Some(crop), rotate: 90, flip_h: true, max_side: Some(16), ..Default::default() };
        // 64×64 rotated 90° is 64×64; the crop takes its top 32×16; limiting to 16 px gives 16×8.
        let (out, _) = convert_with(&src, Format::Mp4, Format::Webm, "geometry.webm", ve);
        assert_eq!(read_back(&out, Format::Webm).1, (16, 8));

        // A fade to black: the middle frame is darker than the first full-level frame would be.
        let (plain, _) = convert_with(&src, Format::Mp4, Format::Webm, "unfaded.webm", VideoEdits::default());
        let (faded, _) = convert_with(&src, Format::Mp4, Format::Webm, "faded.webm", VideoEdits { fade_in: 0.5, ..Default::default() });
        let bright = |p: &Path| -> f64 {
            let mut r = VideoReader::open(p, Format::Webm, 1).unwrap();
            let f = r.frame_at(0.0).unwrap().unwrap();
            f.as_raw().iter().map(|&b| b as f64).sum::<f64>() / f.as_raw().len() as f64
        };
        assert!(bright(&faded) < 20.0 && bright(&plain) > 40.0, "{} vs {}", bright(&faded), bright(&plain));
    }

    #[test]
    fn another_sound_file_replaces_a_videos_own_sound() {
        let src = audio_fixture("video_opus.mp4"); // 0.5 s of 440 Hz
        let tone = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone660.wav"); // 0.3 s of 660 Hz
        let ve = VideoEdits { audio_file: Some(tone), volume_db: -6.0, ..Default::default() };
        let (out, notes) = convert_with(&src, Format::Mp4, Format::Webm, "replaced.webm", ve);
        assert!(notes.is_empty(), "{notes:?}");
        let (secs, hz) = sound(&out, Format::Webm);
        assert!((hz - 660.0).abs() < 15.0, "the new sound, not the original 440: {hz} Hz");
        assert!((secs - 0.3).abs() < 0.05, "as long as the new file: {secs} s");
        assert_eq!(read_back(&out, Format::Webm).2.len(), 5, "the picture keeps its full length");
        // A file that isn't sound says so instead of silently dropping it.
        let not_sound = tmp("not-sound.txt");
        std::fs::write(&not_sound, "hello").unwrap();
        let edits = dotty_core::Edits { ops: vec![dotty_core::EditOp::Video(VideoEdits { audio_file: Some(not_sound), ..Default::default() })] };
        let (options, cancel, progress) = (Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
        let err = crate::VideoConverter::default().convert(&src, &tmp("never.webm"), Format::Mp4, Format::Webm, &ctx).unwrap_err().to_string();
        assert!(err.contains("isn't a sound or video file"), "{err}");
    }

    #[test]
    fn a_picture_with_an_audio_file_lasts_as_long_as_the_sound() {
        let png = tmp("cover-for-audio.png");
        image::RgbImage::from_pixel(32, 32, image::Rgb([90, 90, 200])).save(&png).unwrap();
        let audio = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/song_with_cover.mp3"); // 1 s of 440 Hz
        let ve = VideoEdits { audio_file: Some(audio), ..Default::default() };
        let (out, _) = convert_with(&png, Format::Png, Format::Webm, "picture-and-song.webm", ve);
        let (secs, hz) = sound(&out, Format::Webm);
        assert!((secs - 1.0).abs() < 0.05 && (hz - 440.0).abs() < 10.0, "{secs} s, {hz} Hz");
        let frames = read_back(&out, Format::Webm).2.len();
        assert!((frames as f64 / STILL_FPS - 1.0).abs() < 0.25, "about a second of frames, not the default five: {frames}");
    }

    #[test]
    fn mp4_files_say_which_colour_matrix_the_pictures_use() {
        let src = audio_fixture("video_opus.mp4"); // 64×64: BT.601
        let (out, _) = convert(&src, Format::Mp4, Format::Mp4, "tagged.mp4", &Options::default());
        let track = crate::iso::video_track(&out).unwrap();
        assert_eq!(&track.fourcc, b"vp09");
        // vpcC: version/flags, profile, level, depth/chroma/range, then primaries, transfer, matrix.
        assert_eq!(&track.config[7..10], &[6, 6, 6], "SMPTE 170M, not muxide's 'reserved' transfer and identity matrix");
        assert!(crate::iso::uses_709(720) && !crate::iso::uses_709(576));
    }

    #[test]
    fn the_part_of_a_sound_picked_sets_a_pictures_length() {
        let png = tmp("lengths.png");
        image::RgbImage::from_pixel(32, 32, image::Rgb([90, 90, 200])).save(&png).unwrap();
        let song = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/song_with_cover.mp3"); // 1 s of 440 Hz
        // 0.2–0.7 s of the sound: half a second of video.
        let ve = VideoEdits { audio_file: Some(song), audio_start: 0.2, audio_end: Some(0.7), ..Default::default() };
        let (out, _) = convert_with(&png, Format::Png, Format::Webm, "range.webm", ve);
        let (secs, hz) = sound(&out, Format::Webm);
        assert!((secs - 0.5).abs() < 0.05 && (hz - 440.0).abs() < 12.0, "{secs} s, {hz} Hz");
        let frames = read_back(&out, Format::Webm).2.len();
        assert!((frames as f64 / STILL_FPS - 0.5).abs() < 0.25, "about half a second of picture: {frames} frames");

        // No sound: the length asked for.
        let (out, _) = convert_with(&png, Format::Png, Format::Webm, "twelve.webm", VideoEdits { still_secs: Some(2.0), ..Default::default() });
        assert_eq!(read_back(&out, Format::Webm).2.len(), 10, "2 s at 5 fps");
    }

    #[test]
    fn a_video_can_start_its_replacement_sound_part_way_in() {
        let src = audio_fixture("video_opus.mp4"); // 0.5 s
        let tone = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone660.wav"); // 0.3 s
        // From 0.1 s into the tone: 0.2 s of it, then silence to the end of the video.
        let ve = VideoEdits { audio_file: Some(tone), audio_start: 0.1, ..Default::default() };
        let (out, _) = convert_with(&src, Format::Mp4, Format::Webm, "offset.webm", ve);
        let (secs, hz) = sound(&out, Format::Webm);
        assert!((secs - 0.2).abs() < 0.05 && (hz - 660.0).abs() < 20.0, "{secs} s, {hz} Hz");
        assert_eq!(read_back(&out, Format::Webm).2.len(), 5, "the video keeps its length");
    }

    #[test]
    fn our_opus_delay_matches_the_one_muxide_writes_into_mp4() {
        let (options, cancel, progress) = (Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&options, &cancel, &progress);
        let packets = soundtrack(&audio_fixture("video_opus.mp4"), Format::Mp4, &ctx).unwrap().unwrap();
        // muxide's dOps box always says 312 samples; the sound would shift if ours differed.
        assert_eq!(packets.pre_skip, 312);
    }

    #[test]
    fn picture_edits_apply_before_a_picture_becomes_a_video() {
        let png = tmp("edited.png");
        image::RgbImage::from_pixel(40, 30, image::Rgb([10, 200, 10])).save(&png).unwrap();
        let edits = dotty_core::Edits {
            ops: vec![dotty_core::EditOp::Crop(dotty_core::Region { x: 0, y: 0, width: 20, height: 10, of_width: 40, of_height: 30 })],
        };
        let dst = tmp("edited.webm");
        let (options, cancel, progress) = (Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
        crate::VideoConverter::default().convert(&png, &dst, Format::Png, Format::Webm, &ctx).unwrap();
        assert_eq!(read_back(&dst, Format::Webm).1, (20, 10));
    }

    #[test]
    fn a_song_becomes_a_video_of_its_cover() {
        let song = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/song_with_cover.mp3");
        let (out, notes) = convert(&song, Format::Mp3, Format::Webm, "song.webm", &Options::default());
        assert!(notes.is_empty(), "{notes:?}");
        let (_, size, pts) = read_back(&out, Format::Webm);
        assert_eq!(size, (120, 90), "the cover's size");
        assert!((pts.len() as f64 / STILL_FPS - 1.0).abs() < 0.25, "about 1 s of frames: {}", pts.len());
        let (secs, hz) = sound(&out, Format::Webm);
        assert!((secs - 1.0).abs() < 0.05 && (hz - 440.0).abs() < 10.0, "{secs} s, {hz} Hz");

        // No cover: a plain background, and a note saying so.
        let wav = audio_fixture("video_pcm.avi");
        let (_, notes) = convert(&wav, Format::Avi, Format::Webm, "plain.webm", &Options::default());
        assert!(notes.is_empty(), "a video source isn't a song: {notes:?}");
        let flac = tmp("tone.flac");
        convert_audio(&audio_fixture("video_pcm.avi"), &flac);
        let (out, notes) = convert(&flac, Format::Flac, Format::Mkv, "tone.mkv", &Options::default());
        assert!(notes.iter().any(|n| n.contains("no cover art")), "{notes:?}");
        assert_eq!(read_back(&out, Format::Mkv).1, (NO_ART.0, NO_ART.1));
    }

    fn convert_audio(src: &Path, dst: &Path) {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let progress = |_: f32| {};
        let options = Options::default();
        AudioConverter::default().convert(src, dst, Format::Avi, Format::Flac, &ConvertCtx::new(&options, &cancel, &progress)).unwrap();
    }

    #[test]
    fn a_chosen_picture_replaces_the_cover_art() {
        let song = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/song_with_cover.mp3"); // cover is 120×90
        let pic = tmp("chosen.png");
        image::RgbImage::from_pixel(64, 48, image::Rgb([200, 40, 40])).save(&pic).unwrap();
        let edits = dotty_core::Edits { ops: vec![dotty_core::EditOp::Video(VideoEdits { picture_file: Some(pic), ..Default::default() })] };
        let dst = tmp("chosen.webm");
        let (options, cancel, progress) = (Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
        crate::VideoConverter::default().convert(&song, &dst, Format::Mp3, Format::Webm, &ctx).unwrap();
        assert_eq!(read_back(&dst, Format::Webm).1, (64, 48), "the chosen picture, not the 120×90 cover");
        assert!((sound(&dst, Format::Webm).0 - 1.0).abs() < 0.05);
        // Something that isn't a picture is refused clearly.
        let bad = tmp("bad.txt");
        std::fs::write(&bad, "x").unwrap();
        let edits = dotty_core::Edits { ops: vec![dotty_core::EditOp::Video(VideoEdits { picture_file: Some(bad), ..Default::default() })] };
        let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
        let err = crate::VideoConverter::default().convert(&song, &tmp("bad.webm"), Format::Mp3, Format::Webm, &ctx).unwrap_err().to_string();
        assert!(err.contains("isn't a picture"), "{err}");
    }

    #[test]
    fn a_songs_picture_gets_edits_text_and_fades() {
        let song = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/song_with_cover.mp3"); // 1 s, cover 120×90
        let caption = dotty_core::Caption {
            text: "HI".into(),
            x: 500.0,
            y: 250.0,
            size: 300.0,
            font: dotty_core::CaptionFont::Meme,
            uppercase: true,
            fill: [255; 4],
            outline: [0, 0, 0, 255],
            outline_width: 0.08,
        };
        let ve = VideoEdits {
            crop: Some(dotty_core::Region { x: 0, y: 0, width: 60, height: 90, of_width: 120, of_height: 90 }),
            fade_in: 0.4,
            texts: vec![dotty_core::VideoText { caption, of_width: 1000, of_height: 1500, start: 0.6, end: None }],
            ..Default::default()
        };
        let (out, _) = convert_with(&song, Format::Mp3, Format::Webm, "song-edited.webm", ve);
        assert_eq!(read_back(&out, Format::Webm).1, (60, 90), "cropped");
        let at = |t: f64| VideoReader::open(&out, Format::Webm, 1).unwrap().frame_at(t).unwrap().unwrap();
        let mean = |f: &RgbImage| f.as_raw().iter().map(|&b| b as f64).sum::<f64>() / f.as_raw().len() as f64;
        assert!(mean(&at(0.0)) < 20.0, "starts black");
        assert!(mean(&at(0.4)) > 40.0, "faded in");
        // The picture is otherwise still (the fade is over by 0.4 s), so what changes at 0.6 s is the text.
        let (before, after) = (at(0.4), at(0.8));
        let changed = before.pixels().zip(after.pixels()).filter(|(a, b)| a != b).count();
        assert!(changed > 30, "the text is drawn from 0.6 s: {changed} pixels changed");
        assert!((sound(&out, Format::Webm).0 - 1.0).abs() < 0.05);
    }

    #[test]
    fn sound_that_cant_be_read_is_left_out_with_a_note() {
        let (out, notes) = convert(&audio_fixture("video_aac.mp4"), Format::Mp4, Format::Webm, "aac.webm", &Options::default());
        assert!(notes.iter().any(|n| n.contains("sound was left out") && n.contains("AAC")), "{notes:?}");
        assert_eq!(read_back(&out, Format::Webm).2.len(), 5);
        assert_eq!(Format::detect(&out), Some(Format::Webm));
    }
}
