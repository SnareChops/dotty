//! Decoders outside the `image` crate: AVIF (`oxideav-avif`), JPEG XL (`jxl-oxide`),
//! SVG (`resvg`) and PSD (our own reader of the flattened image).

use crate::decode::MAX_FRAMES_BYTES;
use crate::{Frame, ImageData, ImageDecoder, io_err};
use image::{DynamicImage, ImageBuffer};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use dotty_core::{Choice, ConvertCtx, Error, Format, Result};

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| io_err(path, e))
}

fn too_big() -> Error {
    Error::Codec("image is too large to convert".into())
}

pub struct AvifDecoder;

impl ImageDecoder for AvifDecoder {
    fn format(&self) -> Format {
        Format::Avif
    }

    fn multi_image(&self, path: &Path) -> bool {
        read(path).ok().and_then(|b| oxideav_avif::info(&b).ok()).is_some_and(|i| i.is_sequence && i.frames > 1)
    }

    fn decode(&self, path: &Path, max_frames: usize, _: &ConvertCtx) -> Result<ImageData> {
        let codec = |e: oxideav_avif::AvifError| Error::Codec(format!("avif: {e}"));
        let bytes = read(path)?;
        let info = oxideav_avif::info(&bytes).map_err(codec)?;
        if (info.width as u64) * (info.height as u64) > 1 << 28 {
            return Err(too_big());
        }
        let to_image = |img: &oxideav_avif::AvifImage| -> Result<DynamicImage> {
            let rgba = img.try_to_rgba8().map_err(codec)?;
            let buf = ImageBuffer::from_raw(img.width(), img.height(), rgba).ok_or_else(too_big)?;
            let img = DynamicImage::ImageRgba8(buf);
            Ok(if img.color().has_alpha() && !info.has_alpha { DynamicImage::ImageRgb8(img.to_rgb8()) } else { img })
        };

        if info.is_sequence && info.frames > 1 {
            if max_frames <= 1 {
                let first = oxideav_avif::decode(&bytes).map_err(codec)?;
                let mut data = ImageData::still(to_image(&first)?);
                data.more_frames = true;
                return Ok(with_avif_metadata(data, &first));
            }
            let frames = oxideav_avif::decode_all(&bytes).map_err(codec)?;
            let mut out = Vec::new();
            let mut total = 0u64;
            for f in frames.iter().take(max_frames) {
                let image = to_image(&f.image)?;
                total += image.as_bytes().len() as u64;
                if total > MAX_FRAMES_BYTES {
                    return Err(Error::Codec(format!("animation is too large to convert (over {} frames)", out.len())));
                }
                out.push(Frame { image, delay: f.delay.unwrap_or(Duration::from_millis(100)) });
            }
            let first = frames.first().ok_or_else(|| Error::Codec("avif: no frames".into()))?;
            let mut data = with_avif_metadata(ImageData::still(to_image(&first.image)?), &first.image);
            data.frames = out;
            return Ok(data);
        }
        let img = oxideav_avif::decode(&bytes).map_err(codec)?;
        Ok(with_avif_metadata(ImageData::still(to_image(&img)?), &img))
    }
}

fn with_avif_metadata(mut data: ImageData, img: &oxideav_avif::AvifImage) -> ImageData {
    data.icc = img.metadata.icc.clone();
    data.exif = img.metadata.exif.clone();
    data
}

pub struct JxlDecoder;

impl JxlDecoder {
    fn open(path: &Path) -> Result<jxl_oxide::JxlImage> {
        jxl_oxide::JxlImage::builder()
            .pool(jxl_oxide::JxlThreadPool::none())
            .open(path)
            .map_err(|e| Error::Codec(format!("jpeg xl: {e}")))
    }
}

impl ImageDecoder for JxlDecoder {
    fn format(&self) -> Format {
        Format::Jxl
    }

    fn multi_image(&self, path: &Path) -> bool {
        Self::open(path).is_ok_and(|img| img.image_header().metadata.animation.is_some() && img.num_loaded_keyframes() > 1)
    }

    /// A JPEG XL made from a JPEG keeps enough data to rebuild that exact JPEG.
    fn transcode(&self, path: &Path, to: Format, ctx: &ConvertCtx) -> Result<Option<Vec<u8>>> {
        if to != Format::Jpeg {
            return Ok(None);
        }
        let img = Self::open(path)?;
        if img.jpeg_reconstruction_status() != jxl_oxide::JpegReconstructionStatus::Available {
            return Ok(None);
        }
        let mut buf = Vec::new();
        if img.reconstruct_jpeg(&mut buf).is_err() {
            return Ok(None); // fall back to decoding pixels
        }
        ctx.note("Rebuilt the original JPEG exactly (no quality lost)");
        Ok(Some(buf))
    }

    fn decode(&self, path: &Path, max_frames: usize, _: &ConvertCtx) -> Result<ImageData> {
        let codec = |e: Box<dyn std::error::Error + Send + Sync>| Error::Codec(format!("jpeg xl: {e}"));
        let img = Self::open(path)?;
        let (w, h) = (img.width(), img.height());
        if (w as u64) * (h as u64) > 1 << 28 {
            return Err(too_big());
        }
        let format = img.pixel_format();
        if format.has_black() {
            return Err(Error::Codec("CMYK JPEG XL images aren't supported yet".into()));
        }
        let deep = img.image_header().metadata.bit_depth.bits_per_sample() > 8;
        let anim = img.image_header().metadata.animation.as_ref();
        let tick = anim.map_or(0.0, |a| a.tps_denominator as f64 / a.tps_numerator.max(1) as f64);
        let loops = anim.map_or(0, |a| a.num_loops);

        let keyframes = img.num_loaded_keyframes();
        let mut frames = Vec::new();
        let mut total = 0u64;
        for k in 0..keyframes.min(max_frames) {
            let render = img.render_frame(k).map_err(codec)?;
            let mut stream = render.stream();
            let channels = stream.channels() as usize;
            let image = if deep {
                let mut buf = vec![0u16; w as usize * h as usize * channels];
                stream.write_to_buffer(&mut buf);
                pixels16(w, h, channels, buf)
            } else {
                let mut buf = vec![0u8; w as usize * h as usize * channels];
                stream.write_to_buffer(&mut buf);
                pixels8(w, h, channels, buf)
            }
            .ok_or_else(|| Error::Codec("jpeg xl: unexpected channel layout".into()))?;
            total += image.as_bytes().len() as u64;
            if total > MAX_FRAMES_BYTES {
                return Err(Error::Codec(format!("animation is too large to convert (over {} frames)", frames.len())));
            }
            frames.push(Frame { image, delay: Duration::from_secs_f64(render.duration() as f64 * tick) });
        }
        if frames.is_empty() {
            return Err(Error::Codec("jpeg xl: no frames".into()));
        }
        let mut data = ImageData::still(frames[0].image.clone());
        data.frames = frames;
        data.more_frames = keyframes > max_frames;
        data.loops = loops;
        data.icc = Some(img.rendered_icc()).filter(|p| !p.is_empty());
        Ok(data)
    }
}

fn pixels8(w: u32, h: u32, channels: usize, buf: Vec<u8>) -> Option<DynamicImage> {
    use DynamicImage::*;
    Some(match channels {
        1 => ImageLuma8(ImageBuffer::from_raw(w, h, buf)?),
        2 => ImageLumaA8(ImageBuffer::from_raw(w, h, buf)?),
        3 => ImageRgb8(ImageBuffer::from_raw(w, h, buf)?),
        4 => ImageRgba8(ImageBuffer::from_raw(w, h, buf)?),
        _ => return None,
    })
}

fn pixels16(w: u32, h: u32, channels: usize, buf: Vec<u16>) -> Option<DynamicImage> {
    use DynamicImage::*;
    Some(match channels {
        1 => ImageLuma16(ImageBuffer::from_raw(w, h, buf)?),
        2 => ImageLumaA16(ImageBuffer::from_raw(w, h, buf)?),
        3 => ImageRgb16(ImageBuffer::from_raw(w, h, buf)?),
        4 => ImageRgba16(ImageBuffer::from_raw(w, h, buf)?),
        _ => return None,
    })
}

pub const SVG_SIZE: Choice = Choice {
    key: "size",
    label: "Render size",
    help: "SVG drawings have no fixed pixel size. Pick how large the picture should be; the longest side is used for the pixel sizes.",
    values: &[
        ("1x", "As designed"),
        ("2x", "2\u{d7} larger"),
        ("4x", "4\u{d7} larger"),
        ("512", "512 px"),
        ("1024", "1024 px"),
        ("2048", "2048 px"),
        ("4096", "4096 px"),
    ],
    multi_image_only: false,
};

/// Largest side an SVG is rendered at, whatever its declared size.
const SVG_MAX_SIDE: f32 = 16384.0;

pub struct SvgDecoder;

/// System fonts, loaded once: SVG text needs them and scanning takes a moment.
fn fonts() -> Arc<resvg::usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = resvg::usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

impl ImageDecoder for SvgDecoder {
    fn format(&self) -> Format {
        Format::Svg
    }

    fn choices(&self) -> Vec<Choice> {
        vec![SVG_SIZE]
    }

    fn decode(&self, path: &Path, _: usize, ctx: &ConvertCtx) -> Result<ImageData> {
        use resvg::{tiny_skia, usvg};
        let bytes = read(path)?;
        let options = usvg::Options { resources_dir: path.parent().map(Path::to_path_buf), fontdb: fonts(), ..Default::default() };
        let tree = usvg::Tree::from_data(&bytes, &options).map_err(|e| Error::Codec(format!("svg: {e}")))?;
        let size = tree.size();
        let longest = size.width().max(size.height());
        let scale = match ctx.options.choice(Format::Svg, &SVG_SIZE) {
            "2x" => 2.0,
            "4x" => 4.0,
            "1x" => 1.0,
            px => px.parse::<f32>().unwrap_or(longest) / longest,
        };
        let scale = scale.min(SVG_MAX_SIDE / longest);
        let (w, h) = ((size.width() * scale).ceil().max(1.0) as u32, (size.height() * scale).ceil().max(1.0) as u32);
        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or_else(too_big)?;
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        let rgba: Vec<u8> = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        let buf = ImageBuffer::from_raw(w, h, rgba).ok_or_else(too_big)?;
        Ok(ImageData::still(DynamicImage::ImageRgba8(buf)))
    }
}

pub struct PsdDecoder;

impl ImageDecoder for PsdDecoder {
    fn format(&self) -> Format {
        Format::Psd
    }

    /// The flattened composite image Photoshop stores alongside the layers.
    fn decode(&self, path: &Path, _: usize, _: &ConvertCtx) -> Result<ImageData> {
        Ok(ImageData::still(crate::psd::decode(&read(path)?)?))
    }
}
