//! Video reading: MP4/MOV, MKV/WebM (through symphonia) and AVI (ours), decoded with
//! AV1 (`rav1d`), VP9 (`filmcraft-vp9`) or Motion JPEG, into pictures in display order,
//! with seeking by time. Pictures convert to RGB images (rotation applied) for the image
//! pipeline, which is how videos become stills, animations and frame folders.
#![deny(unsafe_code)] // except the rav1d wrapper in `decode::av1`

pub mod convert;
mod decode;
mod iso;

pub use convert::VideoConverter;
/// The `image` crate, as this crate uses it (for `RgbImage` and resizing).
pub use image;
pub use decode::Matrix;

use codecs_audio::avi::{AviReader, AviStreamKind};
use decode::Decoder;
use dotty_core::{Error, Format, Registry, Result};
use image::RgbImage;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::video::well_known::*;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(VideoConverter::default()));
}

/// The video codecs we decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    Vp9,
    Mjpeg,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Av1 => "AV1",
            Codec::Vp9 => "VP9",
            Codec::Mjpeg => "Motion JPEG",
        }
    }
}

/// A decoded picture: planar YUV as decoded, or RGB (Motion JPEG).
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Presentation time in seconds.
    pub pts: f64,
    pub planes: Planes,
}

pub enum Planes {
    Yuv { bits: u8, cw: u32, ch: u32, data: [Vec<u16>; 3], matrix: Matrix, full_range: bool },
    Rgb(RgbImage),
}

impl Picture {
    /// 8-bit RGB, before any rotation.
    pub fn to_rgb8(&self) -> RgbImage {
        match &self.planes {
            Planes::Rgb(img) => img.clone(),
            Planes::Yuv { bits, cw, ch, data, matrix, full_range } => {
                yuv_to_rgb8(self.width, self.height, *bits, *cw, *ch, data, *matrix, *full_range)
            }
        }
    }
}

/// YUV → RGB (8-bit out). Chroma is sampled at the nearest position.
#[allow(clippy::too_many_arguments)]
fn yuv_to_rgb8(w: u32, h: u32, bits: u8, cw: u32, ch: u32, data: &[Vec<u16>; 3], m: Matrix, full: bool) -> RgbImage {
    let (kr, kb) = match m {
        Matrix::Bt601 => (0.299f32, 0.114f32),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let scale = (1u32 << (bits - 8)) as f32;
    let max = ((1u32 << bits) - 1) as f32;
    let (y_off, y_range, c_range) = if full { (0.0, max, max) } else { (16.0 * scale, 219.0 * scale, 224.0 * scale) };
    let c_mid = (1u32 << (bits - 1)) as f32;
    let (sx, sy) = (cw as f32 / w as f32, ch as f32 / h as f32);
    let mut out = RgbImage::new(w, h);
    for (y, row) in out.rows_mut().enumerate() {
        let cy = ((y as f32 * sy) as usize).min(ch as usize - 1) * cw as usize;
        let yr = y * w as usize;
        for (x, px) in row.enumerate() {
            let c = cy + ((x as f32 * sx) as usize).min(cw as usize - 1);
            let yy = (data[0][yr + x] as f32 - y_off) / y_range;
            let cb = (data[1][c] as f32 - c_mid) / c_range;
            let cr = (data[2][c] as f32 - c_mid) / c_range;
            let r = yy + 2.0 * (1.0 - kr) * cr;
            let b = yy + 2.0 * (1.0 - kb) * cb;
            let g = yy - (2.0 * kb * (1.0 - kb) / kg) * cb - (2.0 * kr * (1.0 - kr) / kg) * cr;
            let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            px.0 = [q(r), q(g), q(b)];
        }
    }
    out
}

/// Rotate clockwise by 0/90/180/270 degrees.
pub fn rotate(img: RgbImage, degrees: u16) -> RgbImage {
    match degrees {
        90 => image::imageops::rotate90(&img),
        180 => image::imageops::rotate180(&img),
        270 => image::imageops::rotate270(&img),
        _ => img,
    }
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub codec: Codec,
    /// Coded size (before rotation).
    pub width: u32,
    pub height: u32,
    /// Clockwise rotation to apply for display.
    pub rotation: u16,
    /// Seconds: from the container, or (after [`VideoReader::keyframes`]) from the last
    /// frame's time.
    pub duration: Option<f64>,
    /// Frames in the video, once the file has been scanned ([`VideoReader::keyframes`]).
    pub frames: Option<u64>,
}

impl VideoInfo {
    /// Frames per second, once the file has been scanned.
    pub fn fps(&self) -> Option<f64> {
        Some(self.frames? as f64 / self.duration.filter(|&d| d > 0.0)?)
    }
}

impl VideoInfo {
    /// Size as displayed (after rotation).
    pub fn display_size(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 { (self.height, self.width) } else { (self.width, self.height) }
    }
}

/// A keyframe: its time in seconds, its timestamp in the track's units, and how many
/// video packets come before it.
#[derive(Debug, Clone, Copy)]
struct Key {
    secs: f64,
    ts: i64,
    ordinal: u64,
}

enum Demux {
    Sym { reader: Box<dyn FormatReader>, track: u32, tb: f64 },
    Avi { reader: AviReader, stream: usize, rate: f64, n: u64 },
}

impl Demux {
    /// The next video packet: (data, seconds, raw timestamp).
    fn next(&mut self) -> Result<Option<(Vec<u8>, f64, i64)>> {
        match self {
            Demux::Sym { reader, track, tb } => loop {
                match reader.next_packet() {
                    Ok(Some(p)) if p.track_id == *track => return Ok(Some((p.data.to_vec(), p.pts.get() as f64 * *tb, p.pts.get()))),
                    Ok(Some(_)) => {}
                    Ok(None) => return Ok(None),
                    Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                    Err(e) => return Err(Error::Codec(e.to_string())),
                }
            },
            Demux::Avi { reader, stream, rate, n } => loop {
                let Some(c) = reader.next_chunk()? else { return Ok(None) };
                if c.stream == *stream {
                    *n += 1;
                    // Empty chunks are dropped frames: they take time but show nothing new.
                    if c.data.is_empty() {
                        continue;
                    }
                    let i = (*n - 1) as i64;
                    return Ok(Some((c.data, i as f64 / *rate, i)));
                }
            },
        }
    }
}

/// Reads a video's pictures in display order, from the start or from any time.
pub struct VideoReader {
    path: PathBuf,
    from: Format,
    info: VideoInfo,
    demux: Demux,
    decoder: Box<dyn Decoder>,
    /// Sequence header / config OBUs fed before the first packet (AV1 in MP4/MKV).
    config: Vec<u8>,
    pending: VecDeque<Picture>,
    eof: bool,
    /// Video packets read so far (for seeking by position).
    ordinal: u64,
    keys: Option<Vec<Key>>,
    /// After a seek: return the picture showing at this time first.
    target: Option<f64>,
    threads: usize,
}

/// What a container calls the codecs we can't decode.
fn unsupported(name: &str, patent: bool) -> Error {
    Error::Codec(if patent {
        format!("this video is {name}, which isn't supported (it still requires a patent licence)")
    } else {
        format!("this video is {name}, which isn't supported yet")
    })
}

impl VideoReader {
    /// `threads` for the decoders that can use them.
    pub fn open(path: &Path, from: Format, threads: usize) -> Result<Self> {
        let (demux, codec, mut info, config) = Self::open_demux(path, from)?;
        if let Some(iso) = matches!(from, Format::Mp4 | Format::Mov).then(|| iso::video_track(path)).flatten() {
            info.rotation = iso.rotation;
        }
        let decoder: Box<dyn Decoder> = match codec {
            Codec::Av1 => Box::new(decode::Av1::new(threads)?),
            Codec::Vp9 => Box::new(decode::Vp9::new(threads)),
            Codec::Mjpeg => Box::new(decode::Mjpeg),
        };
        Ok(Self {
            path: path.to_path_buf(),
            from,
            info,
            demux,
            decoder,
            config,
            pending: VecDeque::new(),
            eof: false,
            ordinal: 0,
            keys: None,
            target: None,
            threads,
        })
    }

    fn open_demux(path: &Path, from: Format) -> Result<(Demux, Codec, VideoInfo, Vec<u8>)> {
        if from == Format::Avi {
            let reader = AviReader::open(path)?;
            let stream = reader.first(true).ok_or_else(|| Error::Codec("this file has no video".into()))?;
            let s = &reader.streams[stream];
            let AviStreamKind::Video { fourcc, width, height } = s.kind else { unreachable!() };
            let codec = match &fourcc.map(|b| b.to_ascii_uppercase()) {
                b"MJPG" | b"AVRN" | b"DMB1" | b"JPEG" => Codec::Mjpeg,
                b"H264" | b"X264" | b"AVC1" => return Err(unsupported("H.264", true)),
                b"XVID" | b"DIVX" | b"DX50" | b"FMP4" | b"MP4V" => return Err(unsupported("MPEG-4 (DivX/Xvid)", false)),
                _ => return Err(unsupported(&format!("in a format Dotty doesn't know ({})", String::from_utf8_lossy(&fourcc)), false)),
            };
            let rate = if s.rate > 0.0 { s.rate } else { 25.0 };
            let duration = (s.length > 0).then(|| s.length as f64 / rate);
            let info = VideoInfo { codec, width, height, rotation: 0, duration, frames: (s.length > 0).then_some(s.length) };
            return Ok((Demux::Avi { reader, stream, rate, n: 0 }, codec, info, Vec::new()));
        }
        let file = std::fs::File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(from.extension());
        let reader = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| Error::Codec(e.to_string()))?;
        let iso = matches!(from, Format::Mp4 | Format::Mov).then(|| iso::video_track(path)).flatten();
        // The video track: symphonia's params when it knows the codec; our MP4 reader
        // for the sample entries it doesn't (AV1, VP9, JPEG).
        let track = reader
            .tracks()
            .iter()
            .find(|t| match &t.codec_params {
                Some(CodecParameters::Video(_)) => true,
                None => iso.as_ref().is_some_and(|i| i.track_id == t.id),
                _ => false,
            })
            .ok_or_else(|| Error::Codec("this file has no video".into()))?;
        let tb = track.time_base.map_or(1.0 / 1000.0, |tb| tb.numer.get() as f64 / tb.denom.get() as f64);
        let duration = track.duration.map(|d| d.get() as f64 * tb).filter(|&d| d > 0.0);
        let (mut width, mut height, mut extra, mut id) = (0u32, 0u32, Vec::new(), None);
        if let Some(CodecParameters::Video(v)) = &track.codec_params {
            width = v.width.unwrap_or(0) as u32;
            height = v.height.unwrap_or(0) as u32;
            extra = v.extra_data.first().map(|e| e.data.to_vec()).unwrap_or_default();
            id = Some(v.codec);
        }
        let codec = match id {
            Some(CODEC_ID_AV1) => Codec::Av1,
            Some(CODEC_ID_VP9) => Codec::Vp9,
            Some(CODEC_ID_MJPEG) => Codec::Mjpeg,
            Some(CODEC_ID_H264) => return Err(unsupported("H.264", true)),
            Some(CODEC_ID_HEVC) => return Err(unsupported("HEVC (H.265)", true)),
            Some(CODEC_ID_VP8) => return Err(unsupported("VP8", false)),
            _ => match iso.as_ref().map(|i| &i.fourcc) {
                Some(b"av01") => Codec::Av1,
                Some(b"vp09") => Codec::Vp9,
                Some(b"jpeg" | b"mjpa" | b"mjpb" | b"AVDJ" | b"dmb1") => Codec::Mjpeg,
                Some(b"avc1" | b"avc3") => return Err(unsupported("H.264", true)),
                Some(b"hvc1" | b"hev1") => return Err(unsupported("HEVC (H.265)", true)),
                Some(b"vp08") => return Err(unsupported("VP8", false)),
                Some(b"mp4v") => return Err(unsupported("MPEG-4 Part 2", false)),
                Some(f) => return Err(unsupported(&format!("in a format Dotty doesn't know ({})", String::from_utf8_lossy(f)), false)),
                None => return Err(Error::Codec("this video's format isn't supported".into())),
            },
        };
        if let Some(iso) = iso {
            if extra.is_empty() {
                extra = iso.config;
            }
            if width == 0 || height == 0 {
                (width, height) = (iso.width, iso.height);
            }
        }
        // AV1's configuration record ends with the sequence header OBUs; dav1d takes those.
        let config = if codec == Codec::Av1 && extra.len() > 4 && extra[0] & 0x80 != 0 { extra[4..].to_vec() } else { Vec::new() };
        let id = track.id;
        let info = VideoInfo { codec, width, height, rotation: 0, duration, frames: None };
        Ok((Demux::Sym { reader, track: id, tb }, codec, info, config))
    }

    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    /// The next picture in display order, or `None` at the end.
    pub fn next_picture(&mut self) -> Result<Option<Picture>> {
        loop {
            if let Some(t) = self.target {
                // After a seek: hold the latest picture at or before `t` until one after
                // it (or the end) shows it's the one on screen at `t`.
                if self.pending.len() >= 2 && self.pending[1].pts <= t + 1e-6 {
                    self.pending.pop_front();
                    continue;
                }
                if self.pending.len() >= 2 || (self.eof && !self.pending.is_empty()) {
                    self.target = None;
                    return Ok(self.pending.pop_front());
                }
            } else if let Some(p) = self.pending.pop_front() {
                return Ok(Some(p));
            }
            if self.eof {
                return Ok(None);
            }
            match self.demux.next()? {
                Some((data, pts, _)) => {
                    if self.ordinal == 0 && !self.config.is_empty() {
                        let config = std::mem::take(&mut self.config);
                        // Sequence header only: no picture comes out.
                        let _ = self.decoder.push(&config, pts);
                        self.config = config;
                    }
                    self.ordinal += 1;
                    let pics = self.decoder.push(&data, pts)?;
                    self.pending.extend(pics);
                }
                None => {
                    self.eof = true;
                    let pics = self.decoder.flush()?;
                    self.pending.extend(pics);
                }
            }
        }
    }

    /// The keyframe times (seconds), scanning the file once (packets only, no decoding).
    pub fn keyframes(&mut self) -> Result<Vec<f64>> {
        Ok(self.key_index()?.iter().map(|k| k.secs).collect())
    }

    fn key_index(&mut self) -> Result<&[Key]> {
        if self.keys.is_none() {
            let (mut demux, codec, _, _) = Self::open_demux(&self.path, self.from)?;
            let mut keys = Vec::new();
            let mut ordinal = 0u64;
            let (mut first, mut last) = (None, 0f64);
            while let Some((data, secs, ts)) = demux.next()? {
                if decode::is_keyframe(codec, &data) {
                    keys.push(Key { secs, ts, ordinal });
                }
                first.get_or_insert(secs);
                last = last.max(secs);
                ordinal += 1;
            }
            self.info.frames = Some(ordinal);
            // The last frame lasts as long as an average one.
            if self.info.duration.is_none() && ordinal > 1 {
                let first = first.unwrap_or(0.0);
                self.info.duration = Some(last + (last - first) / (ordinal - 1) as f64);
            }
            self.keys = Some(keys);
        }
        Ok(self.keys.as_deref().unwrap_or(&[]))
    }

    /// Position so the next picture is the one on screen at `t` seconds: decoding starts
    /// at the last keyframe at or before it.
    pub fn seek(&mut self, t: f64) -> Result<()> {
        let keys = self.key_index()?;
        let Some(key) = keys.iter().rev().find(|k| k.secs <= t + 1e-6).or(keys.first()).copied() else {
            return Err(Error::Codec("this video has no frames that can be shown".into()));
        };
        self.pending.clear();
        self.eof = false;
        self.decoder.reset();
        self.target = Some(t);
        // Fast path: the container's own seek, checked to land exactly on the keyframe.
        let sought = match &mut self.demux {
            Demux::Sym { reader, track, .. } => {
                reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(key.ts), track_id: *track }).is_ok()
            }
            Demux::Avi { .. } => false,
        };
        if sought {
            // Skip anything before the keyframe (a seek may land early), never after; any
            // surprise (some containers' seeks leave the reader unusable) falls back below.
            loop {
                match self.demux.next() {
                    Ok(Some((data, _, ts))) if ts == key.ts => {
                        self.ordinal = key.ordinal + 1;
                        self.feed_config(key.secs);
                        let pics = self.decoder.push(&data, key.secs)?;
                        self.pending.extend(pics);
                        return Ok(());
                    }
                    Ok(Some((_, _, ts))) if ts < key.ts => continue,
                    _ => break,
                }
            }
        }
        // Slow path: from the start, skipping packets (not decoding them) up to the keyframe.
        let (demux, _, _, _) = Self::open_demux(&self.path, self.from)?;
        self.demux = demux;
        self.decoder.reset();
        self.ordinal = 0;
        while self.ordinal < key.ordinal {
            if self.demux.next()?.is_none() {
                break;
            }
            self.ordinal += 1;
        }
        self.feed_config(key.secs);
        Ok(())
    }

    fn feed_config(&mut self, pts: f64) {
        if !self.config.is_empty() {
            let config = std::mem::take(&mut self.config);
            let _ = self.decoder.push(&config, pts);
            self.config = config;
        }
    }

    /// The picture on screen at `t`, as an RGB image with rotation applied.
    pub fn frame_at(&mut self, t: f64) -> Result<Option<RgbImage>> {
        self.seek(t)?;
        Ok(self.next_picture()?.map(|p| rotate(p.to_rgb8(), self.info.rotation)))
    }

    /// Threads the decoder was opened with.
    pub fn threads(&self) -> usize {
        self.threads
    }
}

#[cfg(test)]
mod tests;
