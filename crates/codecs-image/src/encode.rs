//! Single-image encoders. Each converts the frame to a colour type its format accepts,
//! losing as little as possible, and carries EXIF/ICC where the format can.

use crate::{ImageData, ImageEncoder, codec_err};
use image::codecs::{bmp, hdr, ico, jpeg, openexr, pnm, qoi, tga};
use image::{DynamicImage, ExtendedColorType};
use transformer_core::{Choice, ConvertCtx, Error, Format, Preset, Result, WriteSeek};

use DynamicImage::*;

pub struct JpegEncoder;

impl ImageEncoder for JpegEncoder {
    fn format(&self) -> Format {
        Format::Jpeg
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let img = data.first();
        let img = if img.color().has_alpha() {
            ImageRgb8(flatten_on_white(img, "JPEG", ctx))
        } else if img.color().has_color() {
            ImageRgb8(img.to_rgb8())
        } else {
            ImageLuma8(img.to_luma8())
        };
        // JPEG has no true lossless mode; "Lossless" means maximum quality.
        let quality = match ctx.options.preset {
            Preset::Low => 60,
            Preset::Medium => 78,
            Preset::High => 90,
            Preset::Lossless => 100,
        };
        write(&img, jpeg::JpegEncoder::new_with_quality(out, quality), data)
    }
}

pub struct BmpEncoder;

impl ImageEncoder for BmpEncoder {
    fn format(&self) -> Format {
        Format::Bmp
    }
    fn encode(&self, data: &ImageData, _: &ConvertCtx, mut out: &mut dyn WriteSeek) -> Result<()> {
        write(&rgb_or_rgba(data.first()), bmp::BmpEncoder::new(&mut out), data)
    }
}

pub const ICO_SIZES: Choice = Choice {
    key: "sizes",
    label: "Icon sizes",
    help: "Windows picks the best size from the icon for each place it's shown. A set of sizes looks sharpest everywhere; one size makes the smallest file.",
    values: &[("set", "16 to 256 px"), ("single", "One size")],
    multi_image_only: false,
};

pub struct IcoEncoder;

impl ImageEncoder for IcoEncoder {
    fn format(&self) -> Format {
        Format::Ico
    }
    fn choices(&self) -> Vec<Choice> {
        vec![ICO_SIZES]
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        // ICO entries are at most 256×256.
        let img = data.first();
        let largest = if img.width() > 256 || img.height() > 256 {
            ctx.note("Scaled down to fit ICO's 256\u{d7}256 pixel limit");
            img.resize(256, 256, image::imageops::FilterType::Lanczos3)
        } else {
            img.clone()
        };
        let mut sizes = vec![largest];
        if ctx.options.choice(Format::Ico, &ICO_SIZES) == "set" {
            let side = sizes[0].width().max(sizes[0].height());
            for s in [128u32, 64, 48, 32, 24, 16].into_iter().filter(|&s| s < side) {
                sizes.push(sizes[0].resize(s, s, image::imageops::FilterType::Lanczos3));
            }
        }
        let frames = sizes
            .iter()
            .map(|i| {
                let rgba = i.to_rgba8();
                ico::IcoFrame::as_png(rgba.as_raw(), i.width(), i.height(), ExtendedColorType::Rgba8).map_err(codec_err)
            })
            .collect::<Result<Vec<_>>>()?;
        ico::IcoEncoder::new(out).encode_images(&frames).map_err(codec_err)
    }
}

pub const AVIF_SPEED: Choice = Choice {
    key: "speed",
    label: "Encoding",
    help: "AVIF encoding is slow. Slower settings make smaller files at the same quality.",
    values: &[("balanced", "Balanced"), ("small", "Smallest file (slow)"), ("fast", "Fast")],
    multi_image_only: false,
};

pub struct AvifEncoder;

impl ImageEncoder for AvifEncoder {
    fn format(&self) -> Format {
        Format::Avif
    }
    fn choices(&self) -> Vec<Choice> {
        vec![AVIF_SPEED]
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        // AVIF via rav1e has no lossless mode; "Lossless" means maximum quality.
        let quality = match ctx.options.preset {
            Preset::Low => 50,
            Preset::Medium => 65,
            Preset::High => 80,
            Preset::Lossless => 100,
        };
        let speed = match ctx.options.choice(Format::Avif, &AVIF_SPEED) {
            "small" => 3,
            "fast" => 9,
            _ => 6,
        };
        let img = rgb_or_rgba(data.first());
        write(&img, image::codecs::avif::AvifEncoder::new_with_speed_quality(out, speed, quality), data)
    }
}

pub struct JxlEncoder;

impl ImageEncoder for JxlEncoder {
    fn format(&self) -> Format {
        Format::Jxl
    }
    /// Lossless only: the permissively licensed encoders available don't do lossy JPEG XL yet.
    fn encode(&self, data: &ImageData, _: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        use zune_core::bit_depth::BitDepth;
        use zune_core::colorspace::ColorSpace;
        let img = data.first();
        let (w, h) = (img.width() as usize, img.height() as usize);
        if w < 2 || h < 2 {
            return Err(Error::Codec("JPEG XL output needs an image at least 2 pixels wide and tall".into()));
        }
        let deep = matches!(img, ImageLuma16(_) | ImageLumaA16(_) | ImageRgb16(_) | ImageRgba16(_));
        let (space, img) = match (img.color().has_color(), img.color().has_alpha(), deep) {
            (false, false, false) => (ColorSpace::Luma, ImageLuma8(img.to_luma8())),
            (false, true, false) => (ColorSpace::LumaA, ImageLumaA8(img.to_luma_alpha8())),
            (true, false, false) => (ColorSpace::RGB, ImageRgb8(img.to_rgb8())),
            (true, true, false) => (ColorSpace::RGBA, ImageRgba8(img.to_rgba8())),
            (false, false, true) => (ColorSpace::Luma, ImageLuma16(img.to_luma16())),
            (false, true, true) => (ColorSpace::LumaA, ImageLumaA16(img.to_luma_alpha16())),
            (true, false, true) => (ColorSpace::RGB, ImageRgb16(img.to_rgb16())),
            (true, true, true) => (ColorSpace::RGBA, ImageRgba16(img.to_rgba16())),
        };
        let depth = if deep { BitDepth::Sixteen } else { BitDepth::Eight };
        let options = zune_core::options::EncoderOptions::new(w, h, space, depth);
        let mut buf = Vec::new();
        zune_jpegxl::JxlSimpleEncoder::new(img.as_bytes(), options)
            .encode(&mut buf)
            .map_err(|e| Error::Codec(format!("jpeg xl: {e:?}")))?;
        out.write_all(&buf).map_err(|e| Error::Codec(format!("write failed: {e}")))
    }
}

pub struct TgaEncoder;

impl ImageEncoder for TgaEncoder {
    fn format(&self) -> Format {
        Format::Tga
    }
    fn encode(&self, data: &ImageData, _: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        write(&rgb_or_rgba(data.first()), tga::TgaEncoder::new(out), data)
    }
}

pub struct QoiEncoder;

impl ImageEncoder for QoiEncoder {
    fn format(&self) -> Format {
        Format::Qoi
    }
    fn encode(&self, data: &ImageData, _: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        write(&rgb_or_rgba(data.first()), qoi::QoiEncoder::new(out), data)
    }
}

pub struct PnmEncoder;

impl ImageEncoder for PnmEncoder {
    fn format(&self) -> Format {
        Format::Pnm
    }
    /// Binary PPM: RGB at 8 or 16 bits, no transparency.
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let img = data.first();
        let deep = matches!(img, ImageLuma16(_) | ImageLumaA16(_) | ImageRgb16(_) | ImageRgba16(_));
        let img = if img.color().has_alpha() {
            ImageRgb8(flatten_on_white(img, "PPM", ctx))
        } else if deep {
            ImageRgb16(img.to_rgb16())
        } else {
            ImageRgb8(img.to_rgb8())
        };
        let subtype = pnm::PnmSubtype::Pixmap(pnm::SampleEncoding::Binary);
        write(&img, pnm::PnmEncoder::new(out).with_subtype(subtype), data)
    }
}

pub struct HdrEncoder;

impl ImageEncoder for HdrEncoder {
    fn format(&self) -> Format {
        Format::Hdr
    }
    fn float(&self) -> bool {
        true
    }
    fn encode(&self, data: &ImageData, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let img = data.first();
        if img.color().has_alpha() {
            ctx.note("Radiance HDR has no transparency: the alpha channel was dropped");
        }
        write(&ImageRgb32F(img.to_rgb32f()), hdr::HdrEncoder::new(out), data)
    }
}

pub struct ExrEncoder;

impl ImageEncoder for ExrEncoder {
    fn format(&self) -> Format {
        Format::Exr
    }
    fn float(&self) -> bool {
        true
    }
    fn encode(&self, data: &ImageData, _: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let img = data.first();
        let img = if img.color().has_alpha() { ImageRgba32F(img.to_rgba32f()) } else { ImageRgb32F(img.to_rgb32f()) };
        write(&img, openexr::OpenExrEncoder::new(out), data)
    }
}

pub(crate) fn rgb_or_rgba(img: &DynamicImage) -> DynamicImage {
    if img.color().has_alpha() { ImageRgba8(img.to_rgba8()) } else { ImageRgb8(img.to_rgb8()) }
}

/// Composite onto white for formats without transparency, noting it if anything was see-through.
pub(crate) fn flatten_on_white(img: &DynamicImage, format: &str, ctx: &ConvertCtx) -> image::RgbImage {
    let rgba = img.to_rgba8();
    if rgba.pixels().any(|p| p[3] < 255) {
        ctx.note(format!("{format} has no transparency: transparent areas were filled with white"));
    }
    let mut out = image::RgbImage::new(rgba.width(), rgba.height());
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        let mix = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        *o = image::Rgb([mix(p[0]), mix(p[1]), mix(p[2])]);
    }
    out
}

pub(crate) fn write(img: &DynamicImage, mut enc: impl image::ImageEncoder, data: &ImageData) -> Result<()> {
    // Formats without metadata support return an error here; that just means "nothing to carry".
    if let Some(exif) = data.exif.clone() {
        let _ = enc.set_exif_metadata(exif);
    }
    if let Some(icc) = data.icc.clone() {
        let _ = enc.set_icc_profile(icc);
    }
    let color: ExtendedColorType = img.color().into();
    enc.write_image(img.as_bytes(), img.width(), img.height(), color).map_err(codec_err)
}
