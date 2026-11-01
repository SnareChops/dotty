//! Encoders that can hold several images: animated PNG, GIF and WebP, multi-page TIFF.

use crate::encode::write;
use crate::{ImageData, ImageEncoder, codec_err, webp_meta};
use image::codecs::{gif, png};
use image::DynamicImage;
use transformer_core::{Choice, ConvertCtx, Error, Format, Preset, Result, WriteSeek};

use DynamicImage::*;

fn write_err(e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("write failed: {e}"))
}

/// Progress across the encode half of the bar.
fn frame_progress(ctx: &ConvertCtx, i: usize, n: usize) {
    (ctx.progress)(0.5 + 0.5 * i as f32 / n as f32);
}

pub struct PngEncoder;

impl ImageEncoder for PngEncoder {
    fn format(&self) -> Format {
        Format::Png
    }
    fn animated(&self) -> bool {
        true
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        if data.frames.len() > 1 {
            return apng(data, ctx, out);
        }
        let img = match data.first() {
            img @ (ImageLuma8(_) | ImageLumaA8(_) | ImageRgb8(_) | ImageRgba8(_) | ImageLuma16(_) | ImageLumaA16(_)
            | ImageRgb16(_) | ImageRgba16(_)) => img.clone(),
            other => ImageRgba8(other.to_rgba8()),
        };
        write(&img, png::PngEncoder::new_with_quality(out, compression(ctx), png::FilterType::Adaptive), data)
    }
}

fn compression(ctx: &ConvertCtx) -> png::CompressionType {
    match ctx.options.preset {
        Preset::Low => png::CompressionType::Fast,
        Preset::Medium => png::CompressionType::Default,
        Preset::High | Preset::Lossless => png::CompressionType::Best,
    }
}

/// Animated PNG: every frame as a full RGBA canvas.
fn apng(data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
    let (w, h) = (data.first().width(), data.first().height());
    let mut info = ::png::Info::with_size(w, h);
    info.color_type = ::png::ColorType::Rgba;
    info.bit_depth = ::png::BitDepth::Eight;
    info.icc_profile = data.icc.as_deref().map(Into::into);
    info.exif_metadata = data.exif.as_deref().map(Into::into);
    let mut enc = ::png::Encoder::with_info(out, info).map_err(write_err)?;
    enc.set_compression(match ctx.options.preset {
        Preset::Low => ::png::Compression::Fast,
        Preset::Medium => ::png::Compression::Balanced,
        Preset::High | Preset::Lossless => ::png::Compression::High,
    });
    enc.set_animated(data.frames.len() as u32, data.loops).map_err(write_err)?;
    let mut writer = enc.write_header().map_err(write_err)?;
    for (i, frame) in data.frames.iter().enumerate() {
        ctx.check_cancelled()?;
        let ms = frame.delay.as_millis().min(u32::MAX as u128) as u32;
        let (num, den) = if ms <= u16::MAX as u32 { (ms as u16, 1000) } else { ((ms / 100).min(u16::MAX as u32) as u16, 10) };
        writer.set_frame_delay(num, den).map_err(write_err)?;
        writer.write_image_data(frame.image.to_rgba8().as_raw()).map_err(write_err)?;
        frame_progress(ctx, i, data.frames.len());
    }
    writer.finish().map_err(write_err)
}

pub struct GifEncoder;

impl ImageEncoder for GifEncoder {
    fn format(&self) -> Format {
        Format::Gif
    }
    fn animated(&self) -> bool {
        true
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let speed = match ctx.options.preset {
            Preset::Low => 30,
            Preset::Medium => 15,
            Preset::High | Preset::Lossless => 1,
        };
        // Palette search runs per frame; the slowest setting makes long animations crawl.
        let speed = if data.frames.len() > 1 { speed.max(10) } else { speed };
        let mut enc = gif::GifEncoder::new_with_speed(out, speed);
        if data.frames.len() == 1 {
            return enc.encode_frame(image::Frame::new(data.first().to_rgba8())).map_err(codec_err);
        }
        let repeat = match data.loops {
            0 => gif::Repeat::Infinite,
            // GIF counts repeats after the first play.
            n => gif::Repeat::Finite((n - 1).min(u16::MAX as u32) as u16),
        };
        enc.set_repeat(repeat).map_err(codec_err)?;
        for (i, frame) in data.frames.iter().enumerate() {
            ctx.check_cancelled()?;
            let delay = image::Delay::from_saturating_duration(frame.delay);
            enc.encode_frame(image::Frame::from_parts(frame.image.to_rgba8(), 0, 0, delay)).map_err(codec_err)?;
            frame_progress(ctx, i, data.frames.len());
        }
        Ok(())
    }
}

pub const WEBP_COMPRESSION: Choice = Choice {
    key: "compression",
    label: "Compression",
    help: "Lossy makes much smaller files; lossless keeps every pixel exact. \"Match quality\" uses lossless only at the Lossless quality setting.",
    values: &[("auto", "Match quality"), ("lossy", "Lossy"), ("lossless", "Lossless")],
    multi_image_only: false,
};

pub struct WebpEncoder;

impl WebpEncoder {
    fn lossless(ctx: &ConvertCtx) -> bool {
        match ctx.options.choice(Format::Webp, &WEBP_COMPRESSION) {
            "lossy" => false,
            "lossless" => true,
            _ => ctx.options.preset == Preset::Lossless,
        }
    }

    /// One complete WebP file for `img`, with metadata only if asked for.
    fn single(img: &DynamicImage, ctx: &ConvertCtx, icc: Option<&[u8]>, exif: Option<&[u8]>) -> Result<Vec<u8>> {
        if Self::lossless(ctx) {
            let codec = |e: rusty_webp::EncodingError| Error::Codec(format!("webp: {e}"));
            let mut buf = Vec::new();
            let mut enc = rusty_webp::WebPEncoder::new(&mut buf);
            if let Some(exif) = exif {
                enc.set_exif_metadata(exif.to_vec());
            }
            if let Some(icc) = icc {
                enc.set_icc_profile(icc.to_vec());
            }
            if img.color().has_alpha() {
                enc.encode(img.to_rgba8().as_raw(), img.width(), img.height(), rusty_webp::ColorType::Rgba8).map_err(codec)?;
            } else {
                enc.encode(img.to_rgb8().as_raw(), img.width(), img.height(), rusty_webp::ColorType::Rgb8).map_err(codec)?;
            }
            return Ok(buf);
        }
        let quality = match ctx.options.preset {
            Preset::Low => 50,
            Preset::Medium => 70,
            Preset::High => 85,
            Preset::Lossless => 95,
        };
        let rgba = img.to_rgba8();
        let webp = tola_webp::encode_rgba(rgba.as_raw(), img.width(), img.height(), quality)
            .map_err(|e| Error::Codec(format!("webp: {e}")))?;
        Ok(webp_meta::embed(webp, img.width(), img.height(), icc, exif))
    }
}

impl ImageEncoder for WebpEncoder {
    fn format(&self) -> Format {
        Format::Webp
    }
    fn animated(&self) -> bool {
        true
    }
    fn choices(&self) -> Vec<Choice> {
        vec![WEBP_COMPRESSION]
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let (w, h) = (data.first().width(), data.first().height());
        if w > 16383 || h > 16383 {
            return Err(Error::Codec("WebP supports at most 16383 pixels per side".into()));
        }
        let bytes = if data.frames.len() == 1 {
            Self::single(data.first(), ctx, data.icc.as_deref(), data.exif.as_deref())?
        } else {
            let mut frames = Vec::with_capacity(data.frames.len());
            let mut alpha = false;
            for (i, f) in data.frames.iter().enumerate() {
                ctx.check_cancelled()?;
                alpha |= f.image.to_rgba8().pixels().any(|p| p[3] < 255);
                let duration_ms = f.delay.as_millis().min(u32::MAX as u128) as u32;
                frames.push(webp_meta::AnimFrame { webp: Self::single(&f.image, ctx, None, None)?, duration_ms });
                frame_progress(ctx, i, data.frames.len());
            }
            let loops = data.loops.min(u16::MAX as u32) as u16;
            webp_meta::animate(&frames, w, h, loops, alpha, data.icc.as_deref(), data.exif.as_deref())
                .ok_or_else(|| Error::Codec("webp: encoder produced an unexpected file".into()))?
        };
        out.write_all(&bytes).map_err(write_err)
    }
}

pub const TIFF_COMPRESSION: Choice = Choice {
    key: "compression",
    label: "Compression",
    help: "All options are lossless. LZW opens everywhere; Deflate is usually smaller; None is largest but simplest.",
    values: &[("lzw", "LZW"), ("deflate", "Deflate"), ("none", "None")],
    multi_image_only: false,
};

pub struct TiffEncoder;

impl ImageEncoder for TiffEncoder {
    fn format(&self) -> Format {
        Format::Tiff
    }
    fn pages(&self) -> bool {
        true
    }
    fn choices(&self) -> Vec<Choice> {
        vec![TIFF_COMPRESSION]
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        use tiff::encoder::{Compression, DeflateLevel, colortype as ct};
        use tiff::tags::Tag;
        let compression = match ctx.options.choice(Format::Tiff, &TIFF_COMPRESSION) {
            "deflate" => Compression::Deflate(DeflateLevel::Balanced),
            "none" => Compression::Uncompressed,
            _ => Compression::Lzw,
        };
        let mut enc = tiff::encoder::TiffEncoder::new(out).map_err(write_err)?.with_compression(compression);
        for (i, frame) in data.frames.iter().enumerate() {
            ctx.check_cancelled()?;
            let img = &frame.image;
            let (w, h) = (img.width(), img.height());
            // ICC goes on the first page, which is what readers look at.
            let icc = data.icc.as_deref().filter(|_| i == 0);
            macro_rules! page {
                ($ct:ty, $buf:expr) => {{
                    let buf = $buf;
                    let mut page = enc.new_image::<$ct>(w, h).map_err(write_err)?;
                    if let Some(icc) = icc {
                        page.encoder().write_tag(Tag::Unknown(34675), icc).map_err(write_err)?;
                    }
                    page.write_data(&buf).map_err(write_err)?;
                }};
            }
            let deep = matches!(img, ImageLuma16(_) | ImageLumaA16(_) | ImageRgb16(_) | ImageRgba16(_));
            match (img.color().has_color(), img.color().has_alpha(), deep) {
                (false, false, false) => page!(ct::Gray8, img.to_luma8().into_raw()),
                (false, false, true) => page!(ct::Gray16, img.to_luma16().into_raw()),
                (_, false, false) => page!(ct::RGB8, img.to_rgb8().into_raw()),
                (_, false, true) => page!(ct::RGB16, img.to_rgb16().into_raw()),
                (_, true, false) => page!(ct::RGBA8, img.to_rgba8().into_raw()),
                (_, true, true) => page!(ct::RGBA16, img.to_rgba16().into_raw()),
            }
            frame_progress(ctx, i, data.frames.len());
        }
        Ok(())
    }
}
