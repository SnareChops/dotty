//! Decoding through the `image` crate: stills with orientation baked in, every frame
//! of animated GIF / WebP / APNG, every page of a TIFF and every size in an ICO.

use crate::color::HIGHLIGHTS;
use crate::{Frame, ImageData, ImageDecoder, codec_err, io_err};
use image::{AnimationDecoder, DynamicImage, ImageBuffer, ImageDecoder as _, ImageFormat, ImageReader, Limits};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Duration;
use dotty_core::{Choice, ConvertCtx, Error, Format, Result};

/// Refuse decompression bombs: 8 GiB of pixel data is far beyond any real photo.
const MAX_ALLOC: u64 = 8 << 30;
/// Total size of all decoded frames or pages.
pub(crate) const MAX_FRAMES_BYTES: u64 = 4 << 30;

/// Any format the `image` crate reads (registered once per format).
pub struct ImageCrateDecoder(pub Format);

impl ImageDecoder for ImageCrateDecoder {
    fn format(&self) -> Format {
        self.0
    }

    fn choices(&self) -> Vec<Choice> {
        if matches!(self.0, Format::Hdr | Format::Exr) { vec![HIGHLIGHTS] } else { Vec::new() }
    }

    fn multi_image(&self, path: &Path) -> bool {
        match self.0 {
            Format::Gif | Format::Webp | Format::Png => is_animated(path, self.0),
            Format::Tiff => open(path)
                .ok()
                .and_then(|r| tiff::decoder::Decoder::new(r).ok())
                .is_some_and(|d| d.more_images()),
            Format::Ico => head(path, 6).get(4..6).is_some_and(|b| u16::from_le_bytes([b[0], b[1]]) > 1),
            _ => false,
        }
    }

    fn decode(&self, path: &Path, max_frames: usize, ctx: &ConvertCtx) -> Result<ImageData> {
        let mut data = decode_still(path, self.0).map_err(|e| match (self.0, e) {
            (Format::Dds, Error::Codec(m)) => {
                Error::Codec(format!("{m} (only DXT1/3/5 DDS textures with sides divisible by 4 can be read)"))
            }
            (_, e) => e,
        })?;
        match self.0 {
            Format::Gif | Format::Webp | Format::Png if is_animated(path, self.0) => {
                data.loops = loop_count(path, self.0);
                if max_frames <= 1 {
                    data.more_frames = true;
                } else {
                    data.frames = decode_frames(path, self.0, max_frames)?;
                }
            }
            Format::Tiff => {
                data.pages = true;
                tiff_pages(path, &mut data, max_frames, ctx)?;
            }
            Format::Ico => {
                data.pages = true;
                ico_sizes(path, &mut data, max_frames)?;
            }
            _ => {}
        }
        Ok(data)
    }
}

fn image_format(f: Format) -> ImageFormat {
    match f {
        Format::Png => ImageFormat::Png,
        Format::Jpeg => ImageFormat::Jpeg,
        Format::Gif => ImageFormat::Gif,
        Format::Bmp => ImageFormat::Bmp,
        Format::Tiff => ImageFormat::Tiff,
        Format::Ico => ImageFormat::Ico,
        Format::Webp => ImageFormat::WebP,
        Format::Tga => ImageFormat::Tga,
        Format::Qoi => ImageFormat::Qoi,
        Format::Pnm => ImageFormat::Pnm,
        Format::Hdr => ImageFormat::Hdr,
        Format::Exr => ImageFormat::OpenExr,
        Format::Dds => ImageFormat::Dds,
        _ => unreachable!("not an image-crate format"),
    }
}

fn open(path: &Path) -> Result<BufReader<File>> {
    File::open(path).map(BufReader::new).map_err(|e| io_err(path, e))
}

/// The first (or only) image, plus EXIF / ICC.
fn decode_still(src: &Path, from: Format) -> Result<ImageData> {
    let mut reader = ImageReader::with_format(open(src)?, image_format(from));
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);

    let mut decoder = reader.into_decoder().map_err(codec_err)?;
    let orientation = decoder.orientation().map_err(codec_err)?;
    let mut exif = decoder.exif_metadata().map_err(codec_err)?;
    let icc = decoder.icc_profile().map_err(codec_err)?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(codec_err)?;

    // Bake rotation into the pixels and neutralise the tag so viewers don't rotate twice.
    img.apply_orientation(orientation);
    if let Some(chunk) = exif.as_mut() {
        let _ = image::metadata::Orientation::remove_from_exif_chunk(chunk);
    }
    Ok(ImageData { exif, icc, ..ImageData::still(img) })
}

/// True for multi-frame GIF / WebP / APNG files.
fn is_animated(src: &Path, from: Format) -> bool {
    let Ok(reader) = open(src) else { return false };
    match from {
        Format::Gif => image::codecs::gif::GifDecoder::new(reader)
            .map(|d| d.into_frames().take(2).count() > 1)
            .unwrap_or(false),
        Format::Webp => image::codecs::webp::WebPDecoder::new(reader).map(|d| d.has_animation()).unwrap_or(false),
        Format::Png => image::codecs::png::PngDecoder::new(reader).and_then(|d| d.is_apng()).unwrap_or(false),
        _ => false,
    }
}

fn head(path: &Path, n: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Ok(f) = File::open(path) {
        let _ = f.take(n as u64).read_to_end(&mut buf);
    }
    buf
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// How many times the animation plays (0 = forever), read from the container.
fn loop_count(path: &Path, from: Format) -> u32 {
    let le16 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) as u32;
    match from {
        // NETSCAPE2.0 application extension: repeats after the first play (0 = forever).
        // Without it a GIF plays once.
        Format::Gif => {
            let h = head(path, 1 << 16);
            match find(&h, b"NETSCAPE2.0").and_then(|i| h.get(i + 13..i + 15)).map(le16) {
                None => 1,
                Some(0) => 0,
                Some(n) => n + 1,
            }
        }
        // ANIM chunk: background colour (4 bytes), then the loop count.
        Format::Webp => {
            let h = head(path, 1 << 16);
            find(&h, b"ANIM").and_then(|i| h.get(i + 12..i + 14)).map_or(0, le16)
        }
        // acTL chunk: frame count, then number of plays.
        Format::Png => {
            let h = head(path, 1 << 16);
            find(&h, b"acTL")
                .and_then(|i| h.get(i + 8..i + 12))
                .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        }
        _ => 0,
    }
}

/// Every frame, composited onto the full canvas.
fn decode_frames(src: &Path, from: Format, max_frames: usize) -> Result<Vec<Frame>> {
    let reader = open(src)?;
    let frames = match from {
        Format::Gif => image::codecs::gif::GifDecoder::new(reader).map_err(codec_err)?.into_frames(),
        Format::Webp => image::codecs::webp::WebPDecoder::new(reader).map_err(codec_err)?.into_frames(),
        Format::Png => image::codecs::png::PngDecoder::new(reader).map_err(codec_err)?.apng().map_err(codec_err)?.into_frames(),
        _ => unreachable!("only GIF, WebP and PNG animate"),
    };
    let mut out = Vec::new();
    let mut bytes = 0u64;
    for frame in frames.take(max_frames) {
        let frame = frame.map_err(codec_err)?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay = Duration::from_secs_f64(num as f64 / den.max(1) as f64 / 1000.0);
        let buf = frame.into_buffer();
        bytes += buf.as_raw().len() as u64;
        if bytes > MAX_FRAMES_BYTES {
            return Err(Error::Codec(format!("animation is too large to convert (over {} frames)", out.len())));
        }
        out.push(Frame { image: DynamicImage::ImageRgba8(buf), delay });
    }
    if out.is_empty() {
        return Err(Error::Codec("animation has no frames".into()));
    }
    Ok(out)
}

/// Pages after the first, read with the `tiff` crate (the `image` crate only reads one).
fn tiff_pages(path: &Path, data: &mut ImageData, max_frames: usize, ctx: &ConvertCtx) -> Result<()> {
    let tiff_err = |e: tiff::TiffError| Error::Codec(format!("tiff: {e}"));
    let mut dec = tiff::decoder::Decoder::new(open(path)?).map_err(tiff_err)?;
    let mut bytes = data.first().as_bytes().len() as u64;
    let mut page = 1;
    while dec.more_images() {
        if data.frames.len() >= max_frames {
            data.more_frames = true;
            break;
        }
        dec.next_image().map_err(tiff_err)?;
        page += 1;
        let img = (|| {
            let (w, h) = dec.dimensions().ok()?;
            let color = dec.colortype().ok()?;
            let pixels = dec.read_image().ok()?;
            tiff_to_image(w, h, color, pixels)
        })();
        let Some(img) = img else {
            ctx.note(format!("TIFF page {page} uses a layout that isn't supported yet and was skipped"));
            continue;
        };
        bytes += img.as_bytes().len() as u64;
        if bytes > MAX_FRAMES_BYTES {
            return Err(Error::Codec(format!("TIFF is too large to convert (over {} pages)", data.frames.len())));
        }
        data.frames.push(Frame { image: img, delay: Duration::ZERO });
    }
    Ok(())
}

fn tiff_to_image(w: u32, h: u32, color: tiff::ColorType, pixels: tiff::decoder::DecodingResult) -> Option<DynamicImage> {
    use DynamicImage::*;
    use tiff::ColorType as C;
    use tiff::decoder::DecodingResult as D;
    Some(match (color, pixels) {
        (C::Gray(8), D::U8(v)) => ImageLuma8(ImageBuffer::from_raw(w, h, v)?),
        (C::GrayA(8), D::U8(v)) => ImageLumaA8(ImageBuffer::from_raw(w, h, v)?),
        (C::RGB(8), D::U8(v)) => ImageRgb8(ImageBuffer::from_raw(w, h, v)?),
        (C::RGBA(8), D::U8(v)) => ImageRgba8(ImageBuffer::from_raw(w, h, v)?),
        (C::Gray(16), D::U16(v)) => ImageLuma16(ImageBuffer::from_raw(w, h, v)?),
        (C::GrayA(16), D::U16(v)) => ImageLumaA16(ImageBuffer::from_raw(w, h, v)?),
        (C::RGB(16), D::U16(v)) => ImageRgb16(ImageBuffer::from_raw(w, h, v)?),
        (C::RGBA(16), D::U16(v)) => ImageRgba16(ImageBuffer::from_raw(w, h, v)?),
        (C::RGB(32), D::F32(v)) => ImageRgb32F(ImageBuffer::from_raw(w, h, v)?),
        (C::RGBA(32), D::F32(v)) => ImageRgba32F(ImageBuffer::from_raw(w, h, v)?),
        (C::CMYK(8), D::U8(v)) => ImageRgb8(ImageBuffer::from_fn(w, h, |x, y| {
            let i = (y * w + x) as usize * 4;
            let k = 255 - v.get(i + 3).copied().unwrap_or(0) as u32;
            let ch = |c: u8| ((255 - c as u32) * k / 255) as u8;
            image::Rgb([ch(v[i]), ch(v[i + 1]), ch(v[i + 2])])
        })),
        _ => return None,
    })
}

/// Every image stored in an ICO, largest first (the `image` crate only reads the best one).
fn ico_sizes(path: &Path, data: &mut ImageData, max_frames: usize) -> Result<()> {
    let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
    let count = bytes.get(4..6).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize);
    if count <= 1 {
        return Ok(());
    }
    if max_frames <= 1 {
        data.more_frames = true;
        return Ok(());
    }
    let mut images: Vec<DynamicImage> = Vec::new();
    for i in 0..count {
        let Some(entry) = bytes.get(6 + i * 16..6 + i * 16 + 16) else { break };
        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let Some(payload) = bytes.get(offset..offset.saturating_add(size)) else { continue };
        // Re-wrap the entry as a one-image ICO so the `image` crate can decode it.
        let mut one = vec![0, 0, 1, 0, 1, 0];
        one.extend_from_slice(&entry[..12]);
        one.extend_from_slice(&22u32.to_le_bytes());
        one.extend_from_slice(payload);
        if let Ok(img) = image::load_from_memory_with_format(&one, ImageFormat::Ico) {
            images.push(img);
        }
    }
    // Largest first; for repeated sizes keep the first (usually the deepest colour).
    images.sort_by_key(|i| std::cmp::Reverse(i.width() * i.height()));
    images.dedup_by_key(|i| (i.width(), i.height()));
    if images.len() > 1 {
        data.frames = images.into_iter().take(max_frames).map(|image| Frame { image, delay: Duration::ZERO }).collect();
    }
    Ok(())
}
