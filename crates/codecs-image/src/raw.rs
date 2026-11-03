//! Camera RAW files (CR2, CR3, NEF, ARW, DNG, ORF, RW2, RAF, PEF, …): the full-size JPEG
//! picture the camera stored inside the file. Nearly every RAW format carries one, whatever
//! the maker. It shows the camera's own colours and is often the sensor's full resolution
//! (some cameras store a smaller one). Sony ARW files can also be developed from the sensor
//! data (`rawkit`), which gives full resolution on the cameras it understands.

use crate::{ImageData, ImageDecoder, io_err};
use dotty_core::{Choice, ConvertCtx, Error, Format, Result};
use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader};
use std::io::{BufReader, Cursor};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

/// Pictures smaller than this on their long side are thumbnails, not the photo.
const MIN_SIDE: u32 = 400;

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub start: usize,
    pub end: usize,
    pub width: u32,
    pub height: u32,
}

impl Candidate {
    fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// Where the JPEG that starts at `start` ends, and its size, if it's a complete baseline or
/// progressive one. (RAW files also hold lossless JPEG streams of sensor data, and thumbnails
/// inside metadata; those aren't candidates.)
fn jpeg_at(b: &[u8], start: usize) -> Option<Candidate> {
    let (mut pos, mut size) = (start + 2, None);
    loop {
        // Markers may be padded with extra 0xFF.
        while b.get(pos) == Some(&0xFF) && b.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        if *b.get(pos)? != 0xFF {
            return None;
        }
        let marker = *b.get(pos + 1)?;
        match marker {
            0xD9 => return size.map(|(width, height)| Candidate { start, end: pos + 2, width, height }),
            0x00 | 0x01 | 0xD0..=0xD8 => pos += 2,
            _ => {
                let len = u16::from_be_bytes([*b.get(pos + 2)?, *b.get(pos + 3)?]) as usize;
                if len < 2 {
                    return None;
                }
                match marker {
                    // Baseline, extended and progressive DCT. (C3 is lossless: sensor data.)
                    0xC0..=0xC2 => {
                        let h = u16::from_be_bytes([*b.get(pos + 5)?, *b.get(pos + 6)?]) as u32;
                        let w = u16::from_be_bytes([*b.get(pos + 7)?, *b.get(pos + 8)?]) as u32;
                        size = Some((w, h));
                    }
                    0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
                    _ => {}
                }
                pos += 2 + len;
                if marker == 0xDA {
                    // Compressed data runs until the next real marker.
                    loop {
                        let f = b.get(pos..)?.iter().position(|&x| x == 0xFF)?;
                        pos += f;
                        match b.get(pos + 1)? {
                            0x00 | 0xD0..=0xD7 | 0xFF => pos += if b[pos + 1] == 0xFF { 1 } else { 2 },
                            _ => break,
                        }
                    }
                }
            }
        }
    }
}

/// Every complete JPEG picture inside `b`, in file order.
pub fn candidates(b: &[u8]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        let Some(p) = b[i..].windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) else { break };
        let start = i + p;
        match jpeg_at(b, start) {
            Some(c) => {
                i = c.end;
                out.push(c);
            }
            None => i = start + 1,
        }
    }
    out
}

/// The picture to use: the biggest.
pub fn best(c: &[Candidate]) -> Option<&Candidate> {
    c.iter().filter(|c| c.width.max(c.height) >= MIN_SIDE).max_by_key(|c| c.pixels())
}

pub const RAW_MODE: Choice = Choice {
    key: "raw_mode",
    label: "RAW photos",
    help: "Sony ARW files can be developed from the sensor data (full resolution, plain colours) on the cameras that are understood. Other RAW files, and the others, use the full-size picture the camera stored inside the file.",
    values: &[("auto", "Develop when possible"), ("preview", "The camera's own picture")],
    multi_image_only: false,
};

pub struct RawDecoder;

thread_local! {
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` so that a panic inside it is caught without the usual message (the hook installed
/// once passes every other thread's and every other panic on to the previous hook).
fn guarded<R>(f: impl FnOnce() -> R) -> Option<R> {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.with(std::cell::Cell::get) {
                previous(info);
            }
        }));
    });
    QUIET.with(|q| q.set(true));
    let result = catch_unwind(AssertUnwindSafe(f)).ok();
    QUIET.with(|q| q.set(false));
    result
}

/// A Sony ARW developed from its sensor data, or `None` when this camera's files aren't
/// understood (the library stops with a panic on some, so it runs behind a guard).
fn develop(path: &Path) -> Option<DynamicImage> {
    let mut file = BufReader::new(std::fs::File::open(path).ok()?);
    let raw = guarded(|| rawkit::RawImage::decode(&mut file))?.ok()?;
    let (w, h) = (raw.width, raw.height);
    if w == 0 || h == 0 || w > 20_000 || h > 20_000 {
        return None;
    }
    let img = guarded(|| raw.process_8bit())?;
    if img.channels != 3 || img.width * img.height * 3 != img.data.len() {
        return None;
    }
    image::RgbImage::from_raw(img.width as u32, img.height as u32, img.data).map(DynamicImage::ImageRgb8)
}

fn is_arw(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

impl ImageDecoder for RawDecoder {
    fn format(&self) -> Format {
        Format::Raw
    }

    fn choices(&self) -> Vec<Choice> {
        vec![RAW_MODE]
    }

    fn decode(&self, path: &Path, _max_frames: usize, ctx: &ConvertCtx) -> Result<ImageData> {
        let bytes = std::fs::read(path).map_err(|e| io_err(path, e))?;
        ctx.check_cancelled()?;
        let all = candidates(&bytes);
        let mut developed = None;
        if ctx.options.choice(Format::Raw, &RAW_MODE) == "auto" && is_arw(path) {
            developed = develop(path);
            ctx.check_cancelled()?;
        }
        if let Some(img) = developed {
            // The camera's own picture still holds the metadata.
            let exif = best(&all).and_then(|p| ImageReader::with_format(Cursor::new(&bytes[p.start..p.end]), ImageFormat::Jpeg).into_decoder().ok()).and_then(|mut d| d.exif_metadata().ok().flatten());
            ctx.note("Developed from the sensor data with plain colour processing (no camera styling); choose \"The camera's own picture\" for the camera's look");
            return Ok(ImageData { exif, ..ImageData::still(img) });
        }
        let pick = best(&all).ok_or_else(|| Error::Codec("no full-size picture was found inside this RAW file (only the raw sensor data)".into()))?;
        let jpeg = &bytes[pick.start..pick.end];
        let mut decoder = ImageReader::with_format(Cursor::new(jpeg), ImageFormat::Jpeg).into_decoder().map_err(|e| Error::Codec(e.to_string()))?;
        let own = decoder.orientation().map_err(|e| Error::Codec(e.to_string()))?;
        let mut exif = decoder.exif_metadata().map_err(|e| Error::Codec(e.to_string()))?;
        let icc = decoder.icc_profile().map_err(|e| Error::Codec(e.to_string()))?;
        let mut img = DynamicImage::from_decoder(decoder).map_err(|e| Error::Codec(e.to_string()))?;
        // The camera's own turn (the RAW file's header) applies unless the picture says so itself.
        let turn = if own == image::metadata::Orientation::NoTransforms { file_orientation(&bytes).unwrap_or(own) } else { own };
        img.apply_orientation(turn);
        if let Some(chunk) = exif.as_mut() {
            let _ = image::metadata::Orientation::remove_from_exif_chunk(chunk);
        }
        ctx.note(if is_arw(path) && ctx.options.choice(Format::Raw, &RAW_MODE) == "auto" {
            format!("This camera's files can't be developed from the sensor data yet; the camera's own {}×{} picture was used", pick.width, pick.height)
        } else {
            format!("The camera's own {}×{} picture was used; the sensor data isn't developed", pick.width, pick.height)
        });
        Ok(ImageData { exif, icc, ..ImageData::still(img) })
    }
}

/// The turn recorded in a TIFF-style RAW file's own header, when it has one.
fn file_orientation(bytes: &[u8]) -> Option<image::metadata::Orientation> {
    if !(bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*")) {
        return None;
    }
    image::metadata::Orientation::from_exif_chunk(&bytes[..bytes.len().min(1 << 16)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotty_core::Converter as _;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 251) as u8, (y % 241) as u8, 80]));
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).unwrap();
        out
    }

    #[test]
    fn jpegs_are_found_inside_other_bytes_and_the_biggest_is_chosen() {
        let (thumb, photo) = (jpeg(160, 120), jpeg(800, 600));
        // Junk that contains the start of a JPEG signature, a thumbnail, a fake lossless stream, the photo.
        let mut file = vec![0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x04, 0, 0];
        file.extend_from_slice(b"II*\0 padding ");
        file.extend_from_slice(&thumb);
        file.extend_from_slice(&[1, 2, 3]);
        let photo_at = file.len();
        file.extend_from_slice(&photo);
        file.extend_from_slice(b"tail");
        let all = candidates(&file);
        assert_eq!(all.len(), 2, "{all:?}");
        assert_eq!((all[0].width, all[0].height), (160, 120));
        let b = best(&all).unwrap();
        assert_eq!((b.start, b.end, b.width, b.height), (photo_at, photo_at + photo.len(), 800, 600));
        assert_eq!(best(&all[..1]), None, "a thumbnail alone is not the photo");
    }

    #[test]
    fn a_picture_inside_a_jpegs_metadata_is_not_a_separate_candidate() {
        // An EXIF segment holding a whole thumbnail JPEG, inside a larger JPEG.
        let thumb = jpeg(100, 80);
        let outer = jpeg(600, 500);
        let mut with_exif = outer[..2].to_vec();
        let seg_len = (2 + 6 + thumb.len()) as u16;
        with_exif.extend_from_slice(&[0xFF, 0xE1]);
        with_exif.extend_from_slice(&seg_len.to_be_bytes());
        with_exif.extend_from_slice(b"Exif\0\0");
        with_exif.extend_from_slice(&thumb);
        with_exif.extend_from_slice(&outer[2..]);
        let all = candidates(&with_exif);
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].width, all[0].height), (600, 500));
    }

    #[test]
    fn progressive_pictures_and_cut_off_ones() {
        let mut out = Vec::new();
        let img = image::RgbImage::from_pixel(500, 400, image::Rgb([9, 99, 199]));
        // `image` writes baseline only; truncating gives a cut-off one.
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&img).unwrap();
        assert_eq!(candidates(&out).len(), 1);
        assert!(candidates(&out[..out.len() - 10]).is_empty(), "no end marker: not complete");
    }

    #[test]
    fn a_raw_file_is_recognised_converted_and_turned_upright() {
        let dir = std::env::temp_dir().join(format!("tf-rawsynth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A TIFF-style file whose header says "rotate 90° clockwise", with a 800×600 JPEG inside.
        let mut file = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0];
        file.extend_from_slice(&[0; 40]);
        file.extend_from_slice(&jpeg(800, 600));
        let nef = dir.join("shot.NEF");
        std::fs::write(&nef, &file).unwrap();
        assert_eq!(Format::detect(&nef), Some(Format::Raw), "a TIFF by content, a RAW by its name");

        let png = dir.join("shot.png");
        let (opts, cancel, progress) = (dotty_core::Options::default(), std::sync::atomic::AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        crate::ImageConverter::default().convert(&nef, &png, Format::Raw, Format::Png, &ctx).unwrap();
        assert_eq!(image::image_dimensions(&png).unwrap(), (600, 800), "turned like the camera said");
        assert!(ctx.take_notes().iter().any(|n| n.contains("800×600")));

        // A RAW file with only sensor data (no full-size picture) says so.
        let bare = dir.join("bare.CR2");
        std::fs::write(&bare, [b"II*\0\x10\0\0\0CR\x02\0".as_slice(), &[7u8; 100]].concat()).unwrap();
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let err = crate::ImageConverter::default().convert(&bare, &dir.join("bare.png"), Format::Raw, Format::Png, &ctx).unwrap_err().to_string();
        assert!(err.contains("no full-size picture"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
