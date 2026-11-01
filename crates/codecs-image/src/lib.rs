//! Image conversion. Every decoder produces an [`ImageData`] (frames as
//! `DynamicImage`s with orientation applied, plus EXIF/ICC) and every encoder
//! consumes one, so each format is implemented once in each direction.
//!
//! Several images in one file are either an animation or a set of pages (TIFF
//! pages, ICO sizes). They survive when the target can hold them; otherwise only
//! the first is kept, or, with `Options::split_frames`, each becomes its own file.
//!
//! Pixel convention: float images are linear light (HDR / EXR), integer images are
//! sRGB-encoded. The pipeline converts between the two for the encoder.

pub mod caption;
mod color;
mod decode;
mod decode_extra;
pub mod edit;
mod encode;
mod encode_multi;
mod psd;
mod webp_meta;

pub use decode::ImageCrateDecoder;
pub use decode_extra::{AvifDecoder, JxlDecoder, PsdDecoder, SvgDecoder};
pub use encode::{
    AvifEncoder, BmpEncoder, ExrEncoder, HdrEncoder, IcoEncoder, JpegEncoder, JxlEncoder, PnmEncoder, QoiEncoder,
    TgaEncoder,
};
pub use encode_multi::{GifEncoder, PngEncoder, TiffEncoder, WebpEncoder};

use image::DynamicImage;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use dotty_core::{Choice, ConvertCtx, Converter, Error, Format, Registry, Result, WriteSeek, output};

#[derive(Clone)]
pub struct Frame {
    pub image: DynamicImage,
    /// How long the frame shows in an animation; zero for still images and pages.
    pub delay: Duration,
}

/// The image intermediate.
#[derive(Clone)]
pub struct ImageData {
    /// Never empty. Animation frames are full canvases of one size; pages may differ.
    pub frames: Vec<Frame>,
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    /// The source holds more images than were decoded.
    pub more_frames: bool,
    /// The images are separate pictures (pages, icon sizes), not an animation.
    pub pages: bool,
    /// Times an animation plays; 0 means forever.
    pub loops: u32,
}

impl ImageData {
    pub fn still(image: DynamicImage) -> Self {
        Self {
            frames: vec![Frame { image, delay: Duration::ZERO }],
            exif: None,
            icc: None,
            more_frames: false,
            pages: false,
            loops: 0,
        }
    }

    pub fn first(&self) -> &DynamicImage {
        &self.frames[0].image
    }
}

/// Reads one format into an [`ImageData`].
pub trait ImageDecoder: Send + Sync {
    fn format(&self) -> Format;
    /// Decode at most `max_frames` images (at least one); set `more_frames` if some were skipped.
    fn decode(&self, path: &Path, max_frames: usize, ctx: &ConvertCtx) -> Result<ImageData>;
    /// Quick check whether the file holds several images.
    fn multi_image(&self, _path: &Path) -> bool {
        false
    }
    /// Settings for reading this format (e.g. SVG render size).
    fn choices(&self) -> Vec<Choice> {
        Vec::new()
    }
    /// The finished `to` file made straight from the source without decoding pixels, when
    /// the source allows a better result that way (JPEG XL → the original JPEG).
    fn transcode(&self, _path: &Path, _to: Format, _ctx: &ConvertCtx) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// Writes one format from an [`ImageData`].
pub trait ImageEncoder: Send + Sync {
    fn format(&self) -> Format;
    /// Writes every frame of an animation. Otherwise the pipeline passes a single frame.
    fn animated(&self) -> bool {
        false
    }
    /// Writes several pages (animation frames become pages too).
    fn pages(&self) -> bool {
        false
    }
    /// Takes linear-light float pixels; integer images are converted for it.
    fn float(&self) -> bool {
        false
    }
    /// Format-specific settings, besides the animation/pages choice the pipeline adds.
    fn choices(&self) -> Vec<Choice> {
        Vec::new()
    }
    fn encode(&self, img: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()>;
}

pub const ANIMATION: Choice = Choice {
    key: "animation",
    label: "Animation",
    help: "Keep every frame of an animated source, or save only its first frame as a still image.",
    values: &[("all", "Keep animation"), ("first", "First frame only")],
    multi_image_only: true,
};

pub const PAGES: Choice = Choice {
    key: "pages",
    label: "Pages",
    help: "Keep every page (or frame) of the source, or only the first.",
    values: &[("all", "All pages"), ("first", "First page only")],
    multi_image_only: true,
};

/// Register every image route (any supported format → any other).
pub fn register(registry: &mut Registry) {
    registry.register(Box::new(ImageConverter::default()));
}

/// Any registered decoder → [`ImageData`] → any registered encoder.
pub struct ImageConverter {
    decoders: Vec<Box<dyn ImageDecoder>>,
    encoders: Vec<Box<dyn ImageEncoder>>,
}

impl Default for ImageConverter {
    /// Every codec this crate provides.
    fn default() -> Self {
        use Format::*;
        let mut c = Self::empty();
        for f in [Png, Jpeg, Gif, Bmp, Tiff, Ico, Webp, Tga, Qoi, Pnm, Hdr, Exr, Dds] {
            c.add_decoder(ImageCrateDecoder(f));
        }
        c.add_decoder(AvifDecoder);
        c.add_decoder(JxlDecoder);
        c.add_decoder(SvgDecoder);
        c.add_decoder(PsdDecoder);

        c.add_encoder(PngEncoder);
        c.add_encoder(JpegEncoder);
        c.add_encoder(GifEncoder);
        c.add_encoder(BmpEncoder);
        c.add_encoder(TiffEncoder);
        c.add_encoder(IcoEncoder);
        c.add_encoder(WebpEncoder);
        c.add_encoder(AvifEncoder);
        c.add_encoder(JxlEncoder);
        c.add_encoder(TgaEncoder);
        c.add_encoder(QoiEncoder);
        c.add_encoder(PnmEncoder);
        c.add_encoder(HdrEncoder);
        c.add_encoder(ExrEncoder);
        c
    }
}

impl ImageConverter {
    pub fn empty() -> Self {
        Self { decoders: Vec::new(), encoders: Vec::new() }
    }

    pub fn add_decoder(&mut self, d: impl ImageDecoder + 'static) {
        self.decoders.push(Box::new(d));
    }

    pub fn add_encoder(&mut self, e: impl ImageEncoder + 'static) {
        self.encoders.push(Box::new(e));
    }

    fn encoder(&self, to: Format) -> Option<&dyn ImageEncoder> {
        self.encoders.iter().find(|e| e.format() == to).map(|e| e.as_ref())
    }
}

/// The "keep all frames/pages" choice an encoder gets from the pipeline, if any.
fn multi_choice(encoder: &dyn ImageEncoder) -> Option<Choice> {
    if encoder.pages() {
        Some(PAGES)
    } else if encoder.animated() {
        Some(ANIMATION)
    } else {
        None
    }
}

impl Converter for ImageConverter {
    /// Includes `(f, f)`: re-saving an edited image in its own format.
    fn routes(&self) -> Vec<(Format, Format)> {
        self.decoders
            .iter()
            .flat_map(|d| self.encoders.iter().map(move |e| (d.format(), e.format())))
            .collect()
    }

    fn choices(&self, to: Format) -> Vec<Choice> {
        let Some(e) = self.encoder(to) else { return Vec::new() };
        multi_choice(e).into_iter().chain(e.choices()).collect()
    }

    fn multi_image(&self, path: &Path, from: Format) -> bool {
        self.decoders.iter().find(|d| d.format() == from).is_some_and(|d| d.multi_image(path))
    }

    fn source_choices(&self, from: Format) -> Vec<Choice> {
        self.decoders.iter().find(|d| d.format() == from).map(|d| d.choices()).unwrap_or_default()
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        let decoder = self.decoders.iter().find(|d| d.format() == from).ok_or(Error::NoRoute { from, to })?;
        let encoder = self.encoder(to).ok_or(Error::NoRoute { from, to })?;
        (ctx.progress)(0.0);

        // Transcoding skips the pixels, so it can't carry edits.
        if !ctx.options.strip_metadata
            && ctx.edits.is_none()
            && let Some(bytes) = decoder.transcode(src, to, ctx)?
        {
            std::fs::write(dst, bytes).map_err(|e| io_err(dst, e))?;
            (ctx.progress)(1.0);
            return Ok(());
        }

        let keep_wanted = multi_choice(encoder).is_some_and(|c| ctx.options.choice(to, &c) == "all");
        let max_frames = if keep_wanted || ctx.options.split_frames { usize::MAX } else { 1 };
        let mut img = decoder.decode(src, max_frames, ctx)?;
        ctx.check_cancelled()?;
        (ctx.progress)(0.5);
        if ctx.options.strip_metadata {
            img.exif = None;
            img.icc = None;
        }
        if let Some(edits) = ctx.edits {
            edit::apply(&mut img, edits);
        }

        let multi = img.frames.len() > 1 || img.more_frames;
        let can_keep = if img.pages { encoder.pages() } else { encoder.animated() || encoder.pages() };
        if multi && !(can_keep && keep_wanted) {
            if !can_keep && ctx.options.split_frames && img.frames.len() > 1 {
                return write_frames(img, from, dst, encoder, ctx);
            }
            if !can_keep {
                ctx.note(first_only_note(from, &img));
            }
            img.frames.truncate(1);
            img.more_frames = false;
        }
        let img = color::for_encoder(img, encoder.float(), from, ctx.options);
        write_file(&img, dst, encoder, ctx)?;
        (ctx.progress)(1.0);
        Ok(())
    }
}

fn first_only_note(from: Format, img: &ImageData) -> &'static str {
    if from == Format::Ico {
        "This icon holds several sizes: only the largest was converted (turn on \"Export every frame\" to get them all)"
    } else if img.pages {
        "Multi-page image: only the first page was converted (turn on \"Export every frame\" to get them all)"
    } else {
        "Animated image: only the first frame was converted (turn on \"Export every frame\" to get them all)"
    }
}

fn create(dst: &Path) -> Result<BufWriter<File>> {
    File::create(dst).map(BufWriter::new).map_err(|e| io_err(dst, e))
}

fn write_file(img: &ImageData, dst: &Path, encoder: &dyn ImageEncoder, ctx: &ConvertCtx) -> Result<()> {
    let mut out = create(dst)?;
    encoder.encode(img, ctx, &mut out)?;
    out.flush().map_err(|e| io_err(dst, e))
}

/// One file per image in a new `name-frames` folder next to `dst`.
fn write_frames(img: ImageData, from: Format, dst: &Path, encoder: &dyn ImageEncoder, ctx: &ConvertCtx) -> Result<()> {
    let dir = output::frames_dir(dst);
    std::fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let count = img.frames.len();
    let result = (|| {
        let stem = dst.file_stem().and_then(|s| s.to_str()).unwrap_or("frame");
        let ext = encoder.format().extension();
        let width = count.to_string().len().max(3);
        let mut first = None;
        let ImageData { frames, exif, icc, pages, .. } = img;
        for (i, frame) in frames.into_iter().enumerate() {
            ctx.check_cancelled()?;
            let path = dir.join(format!("{stem}-{:0width$}.{ext}", i + 1));
            let single = ImageData { frames: vec![frame], exif: exif.clone(), icc: icc.clone(), more_frames: false, pages, loops: 0 };
            let single = color::for_encoder(single, encoder.float(), from, ctx.options);
            write_file(&single, &path, encoder, ctx)?;
            first.get_or_insert(path);
            (ctx.progress)(0.5 + 0.5 * (i + 1) as f32 / count as f32);
        }
        Ok::<PathBuf, Error>(first.expect("at least one frame"))
    })();
    match result {
        Ok(first) => {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ctx.note(format!("Saved {count} images to the folder \"{name}\""));
            ctx.set_output(first);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir); // don't leave partial output
            Err(e)
        }
    }
}

pub(crate) fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io { path: path.into(), source }
}

pub(crate) fn codec_err(e: image::ImageError) -> Error {
    Error::Codec(e.to_string())
}

#[cfg(test)]
mod tests;
