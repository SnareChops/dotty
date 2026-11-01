//! Video → images: one frame as a still, or a range of frames as an animation or a folder
//! of images, handed to the image pipeline (which also applies image-editor edits).

use crate::{VideoReader, rotate};
use codecs_image::{Frame, ImageConverter, ImageData};
use dotty_core::{Choice, ConvertCtx, Converter, Error, Format, FrameSelection, Kind, Result};
use image::DynamicImage;
use std::path::Path;
use std::time::Duration;

/// Formats we read video from.
pub const VIDEO_FORMATS: [Format; 5] = [Format::Mp4, Format::Mov, Format::Mkv, Format::Webm, Format::Avi];

/// Most pixels all frames of an animation may hold together: the image editor's working
/// limit, so a whole clip can be edited at full working size.
pub const BUDGET_PIXELS: u64 = 64 << 20;
/// Default frame rate of animations (GIF's practical rate).
pub const DEFAULT_FPS: f32 = 15.0;
/// "Fit longer clip" lowers the frame rate to this, then the size to this.
pub const MIN_FIT_FPS: f32 = 8.0;
pub const MIN_FIT_SIDE: u32 = 320;
/// Longest range "Fit longer clip" takes, in seconds.
pub const MAX_FIT_SECS: f64 = 60.0;

/// Default longest side of an animation: GIF files get big fast.
pub fn default_side(to: Format) -> u32 {
    if to == Format::Gif { 640 } else { 960 }
}

/// What a range of frames will come out as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangePlan {
    pub start: f64,
    pub end: f64,
    pub fps: f32,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    /// The range was cut short to fit the budget.
    pub shortened: bool,
}

impl RangePlan {
    pub fn pixels(&self) -> u64 {
        self.frames * self.width as u64 * self.height as u64
    }
}

/// Plan a range: the frame rate and size asked for (or the defaults), the budget, and
/// "fit longer clip". `display` is the video's size as shown, `source_fps` its frame rate.
pub fn plan_range(sel: &FrameSelection, to: Format, display: (u32, u32), duration: f64, source_fps: Option<f64>) -> RangePlan {
    let longest = display.0.max(display.1).max(1);
    let mut fps = sel.fps.unwrap_or(DEFAULT_FPS).max(0.1);
    if let Some(src) = source_fps {
        fps = fps.min(src as f32);
    }
    let mut side = sel.max_side.unwrap_or_else(|| default_side(to)).min(longest).max(1);
    let start = sel.start.clamp(0.0, duration.max(0.0));
    let mut end = sel.end.unwrap_or(duration).clamp(start, duration.max(start));
    let size = |side: u32| {
        let k = side as f64 / longest as f64;
        (((display.0 as f64 * k).round() as u32).max(1), ((display.1 as f64 * k).round() as u32).max(1))
    };
    // (The tolerance keeps 0.4 − 0.1 = 0.30000000000000004 s at 10 fps at 3 frames.)
    let frames = |fps: f32, from: f64, to: f64| (((to - from) * fps as f64 - 1e-6).ceil() as u64).max(1);
    let pixels = |fps: f32, side: u32, end: f64| {
        let (w, h) = size(side);
        frames(fps, start, end) * w as u64 * h as u64
    };
    if sel.fit_longer {
        end = end.min(start + MAX_FIT_SECS);
        while pixels(fps, side, end) > BUDGET_PIXELS && fps > MIN_FIT_FPS {
            fps = (fps - 1.0).max(MIN_FIT_FPS);
        }
        while pixels(fps, side, end) > BUDGET_PIXELS && side > MIN_FIT_SIDE {
            side = ((side as f32 * 0.9) as u32).max(MIN_FIT_SIDE);
        }
    }
    let (w, h) = size(side);
    let per_frame = w as u64 * h as u64;
    let mut shortened = false;
    if frames(fps, start, end) * per_frame > BUDGET_PIXELS {
        let fit = (BUDGET_PIXELS / per_frame).max(1);
        end = start + fit as f64 / fps as f64;
        shortened = true;
    }
    RangePlan { start, end, fps, width: w, height: h, frames: frames(fps, start, end), shortened }
}

#[derive(Default)]
pub struct VideoConverter {
    images: ImageConverter,
}

/// Whether converting to `to` takes a range of frames rather than a single frame: a GIF
/// (made to be an animation), "export every frame", or a range picked in the editor for a
/// format that can hold one (animated WebP/PNG, TIFF pages). A video going to PNG or WebP
/// is otherwise a still, as people most often mean.
pub fn wants_range(to: Format, options: &dotty_core::Options, sel: &FrameSelection) -> bool {
    let choice = if to.paged_output() { codecs_image::PAGES } else { codecs_image::ANIMATION };
    let keeps = (to.animated_output() || to.paged_output()) && options.choice(to, &choice) == "all";
    options.split_frames || (keeps && (to == Format::Gif || sel.end.is_some()))
}

fn scale(img: DynamicImage, w: u32, h: u32) -> DynamicImage {
    if (img.width(), img.height()) == (w, h) { img } else { img.resize_exact(w, h, image::imageops::FilterType::Triangle) }
}

impl Converter for VideoConverter {
    fn routes(&self) -> Vec<(Format, Format)> {
        let targets: Vec<Format> = Format::ALL
            .into_iter()
            .filter(|&t| t.kind() == Kind::Image && self.images.routes().iter().any(|r| r.1 == t))
            .collect();
        VIDEO_FORMATS.iter().flat_map(|&f| targets.iter().map(move |&t| (f, t))).collect()
    }

    fn choices(&self, to: Format) -> Vec<Choice> {
        self.images.choices(to)
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        (ctx.progress)(0.0);
        let sel = ctx.edits.and_then(|e| e.frames()).copied().unwrap_or_default();
        let img = video_frames(src, from, to, &sel, ctx)?;
        ctx.check_cancelled()?;
        (ctx.progress)(0.5);
        self.images.write(img, from, to, dst, ctx)?;
        (ctx.progress)(1.0);
        Ok(())
    }
}

/// The frames `sel` picks for converting to `to`, as the image pipeline takes them: the
/// conversion uses this, and so does the editor (so edits line up with what's written).
pub fn video_frames(src: &Path, from: Format, to: Format, sel: &FrameSelection, ctx: &ConvertCtx) -> Result<ImageData> {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
    let mut r = VideoReader::open(src, from, threads)?;
    let rotation = r.info().rotation;
    let sel = *sel;
    {
        let img = if wants_range(to, ctx.options, &sel) {
            r.keyframes()?; // also fills in the duration and frame count
            let info = r.info().clone();
            let duration = info.duration.unwrap_or(0.0);
            let plan = plan_range(&sel, to, info.display_size(), duration, info.fps());
            if plan.shortened {
                ctx.note(format!(
                    "Only {:.1} s (from {:.1} s) fit the size limit for an animation: pick a shorter range, or turn on \"Fit longer clip\" in Edit",
                    plan.end - plan.start,
                    plan.start
                ));
            }
            let frames = sample(&mut r, &plan, rotation, ctx)?;
            if frames.is_empty() {
                return Err(Error::Codec("no frames in the picked range".into()));
            }
            ImageData { frames, exif: None, icc: None, more_frames: false, pages: false, loops: 0 }
        } else {
            let pic = r.frame_at(sel.start)?.ok_or_else(|| Error::Codec("this video has no frame at that time".into()))?;
            let mut img = DynamicImage::ImageRgb8(pic);
            if let Some(side) = sel.max_side.filter(|&s| s < img.width().max(img.height())) {
                img = img.resize(side, side, image::imageops::FilterType::CatmullRom);
            }
            ImageData::still(img)
        };
        Ok(img)
    }
}

/// The frames of `plan`: at each tick of its frame rate, the picture on screen then.
fn sample(r: &mut VideoReader, plan: &RangePlan, rotation: u16, ctx: &ConvertCtx) -> Result<Vec<Frame>> {
    r.seek(plan.start)?;
    let delay = Duration::from_secs_f64(1.0 / plan.fps as f64);
    let mut out = Vec::with_capacity(plan.frames as usize);
    let mut shown: Option<crate::Picture> = None;
    let mut next = r.next_picture()?;
    for k in 0..plan.frames {
        ctx.check_cancelled()?;
        let t = plan.start + k as f64 / plan.fps as f64;
        // Move on to the latest picture at or before `t`.
        while let Some(p) = next.take() {
            if p.pts <= t + 1e-6 || shown.is_none() {
                shown = Some(p);
                next = r.next_picture()?;
            } else {
                next = Some(p);
                break;
            }
        }
        let Some(p) = &shown else { break };
        let img = DynamicImage::ImageRgb8(rotate(p.to_rgb8(), rotation));
        out.push(Frame { image: scale(img, plan.width, plan.height), delay });
        (ctx.progress)(0.5 * (k + 1) as f32 / plan.frames as f32);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_clips_fit_whole_and_long_ones_are_shortened_or_fitted() {
        let sel = FrameSelection::default();
        // 3 s of 1080p at 30 fps → GIF: 640×360 at 15 fps, all of it.
        let p = plan_range(&sel, Format::Gif, (1920, 1080), 3.0, Some(30.0));
        assert_eq!((p.width, p.height, p.fps, p.frames, p.shortened), (640, 360, 15.0, 45, false));
        // WebP gets 960 px; never upscales a small source; never above the source frame rate.
        let p = plan_range(&sel, Format::Webp, (320, 240), 2.0, Some(10.0));
        assert_eq!((p.width, p.height, p.fps), (320, 240, 10.0));
        // 90 s at 640×360×15 fps is over budget: cut to what fits (64 MP / 230 400 px = 291 frames).
        let p = plan_range(&sel, Format::Gif, (1920, 1080), 90.0, Some(30.0));
        assert!(p.shortened && p.frames == 291 && (p.end - 19.4).abs() < 0.05, "{p:?}");
        assert!(p.pixels() <= BUDGET_PIXELS);
        // Fit longer: lower fps first, then size, capped at 60 s.
        let fit = FrameSelection { fit_longer: true, ..sel };
        let p = plan_range(&fit, Format::Gif, (1920, 1080), 90.0, Some(30.0));
        assert_eq!(p.end, 60.0);
        assert!(p.fps == MIN_FIT_FPS && p.width < 640 && p.width >= MIN_FIT_SIDE, "{p:?}");
        assert!(p.pixels() <= BUDGET_PIXELS && !p.shortened, "{p:?}");
        // A picked range and rate.
        let pick = FrameSelection { start: 10.0, end: Some(12.0), fps: Some(10.0), ..sel };
        let p = plan_range(&pick, Format::Gif, (1920, 1080), 90.0, Some(30.0));
        assert_eq!((p.start, p.end, p.frames), (10.0, 12.0, 20));
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../codecs-audio/tests/fixtures").join(name)
    }

    fn convert(src: &Path, from: Format, to: Format, options: &dotty_core::Options, edits: Option<&dotty_core::Edits>) -> (std::path::PathBuf, Vec<String>) {
        let dir = std::env::temp_dir().join(format!("tf-video-conv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dotty_core::output::output_path(&dir.join(src.file_name().unwrap()), to, options).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let progress = |_: f32| {};
        let ctx = ConvertCtx::new(options, &cancel, &progress).with_edits(edits);
        VideoConverter::default().convert(src, &dst, from, to, &ctx).unwrap();
        let out = ctx.take_output().unwrap_or(dst);
        (out, ctx.take_notes())
    }

    #[test]
    fn a_picked_frame_becomes_a_still_with_image_edits() {
        let src = fixture("video_opus.mp4");
        let mut reader = VideoReader::open(&src, Format::Mp4, 1).unwrap();
        let want = reader.frame_at(0.2).unwrap().unwrap();
        let edits = dotty_core::Edits {
            ops: vec![
                dotty_core::EditOp::Frames(FrameSelection { start: 0.2, ..Default::default() }),
                dotty_core::EditOp::Crop(dotty_core::Region { x: 0, y: 0, width: 32, height: 16, of_width: 64, of_height: 64 }),
            ],
        };
        let (out, _) = convert(&src, Format::Mp4, Format::Png, &dotty_core::Options::default(), Some(&edits));
        let got = image::open(&out).unwrap().to_rgb8();
        assert_eq!(got.dimensions(), (32, 16), "cropped");
        assert_eq!(got, image::imageops::crop_imm(&want, 0, 0, 32, 16).to_image(), "the frame at 0.2 s");
    }

    #[test]
    fn a_whole_short_clip_becomes_an_animation_or_a_frame_folder() {
        let src = fixture("video_two_tracks.mkv");
        let (gif, notes) = convert(&src, Format::Mkv, Format::Gif, &dotty_core::Options::default(), None);
        assert!(notes.is_empty(), "{notes:?}");
        let frames = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(std::fs::File::open(&gif).unwrap())).unwrap();
        use image::AnimationDecoder;
        let frames: Vec<_> = frames.into_frames().collect::<std::result::Result<_, _>>().unwrap();
        assert_eq!(frames.len(), 5, "0.5 s at the source's 10 fps (below the default 15)");
        assert_eq!(frames[0].delay().numer_denom_ms(), (100, 1));

        let split = dotty_core::Options { split_frames: true, ..Default::default() };
        // (JPEG can't hold several images, so "export every frame" makes a folder; PNG would
        // keep them together as an animated PNG.)
        let (first, _) = convert(&src, Format::Mkv, Format::Jpeg, &split, None);
        let folder = first.parent().unwrap();
        assert!(folder.file_name().unwrap().to_string_lossy().ends_with("-frames"), "{}", folder.display());
        assert_eq!(std::fs::read_dir(folder).unwrap().count(), 5);
    }

    #[test]
    fn rotated_videos_come_out_upright() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rotated.mp4");
        let (out, _) = convert(&src, Format::Mp4, Format::Jpeg, &dotty_core::Options::default(), None);
        let img = image::open(&out).unwrap();
        assert_eq!((img.width(), img.height()), (32, 64));
    }

    #[test]
    fn png_and_webp_are_stills_unless_a_range_is_picked() {
        let src = fixture("video_two_tracks.mkv");
        let (png, _) = convert(&src, Format::Mkv, Format::Png, &dotty_core::Options::default(), None);
        let decoder = image::codecs::png::PngDecoder::new(std::io::BufReader::new(std::fs::File::open(&png).unwrap())).unwrap();
        assert!(!decoder.is_apng().unwrap(), "a still PNG");
        let range = dotty_core::Edits { ops: vec![dotty_core::EditOp::Frames(FrameSelection { start: 0.1, end: Some(0.4), ..Default::default() })] };
        let (apng, _) = convert(&src, Format::Mkv, Format::Png, &dotty_core::Options::default(), Some(&range));
        let decoder = image::codecs::png::PngDecoder::new(std::io::BufReader::new(std::fs::File::open(&apng).unwrap())).unwrap();
        assert!(decoder.is_apng().unwrap(), "a picked range makes an animated PNG");
        use image::AnimationDecoder;
        assert_eq!(decoder.apng().unwrap().into_frames().count(), 3, "0.1–0.4 s at 10 fps");
    }
}
