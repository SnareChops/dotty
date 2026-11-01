//! AVI (RIFF) reading: stream headers and the data chunks in file order. Shared by the
//! audio side (PCM soundtracks) and the video side (Motion JPEG).
//!
//! Handles AVI 1.0 and OpenDML AVI 2.0 (data continues in further `RIFF AVIX` chunks),
//! `LIST rec ` groups, and odd-sized chunks (padded to even). Indexes aren't needed for
//! reading in order and are ignored.

use dotty_core::{Error, Result};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

fn err(msg: &str) -> Error {
    Error::Codec(format!("avi: {msg}"))
}

/// What a stream holds.
#[derive(Debug, Clone, PartialEq)]
pub enum AviStreamKind {
    Video {
        /// `BITMAPINFOHEADER.biCompression`, e.g. `MJPG`.
        fourcc: [u8; 4],
        width: u32,
        /// Positive: bottom-up rows (uncompressed); the absolute value is used.
        height: u32,
    },
    Audio {
        /// `WAVEFORMATEX.wFormatTag` (1 = PCM, 3 = float; the sub-format for extensible).
        format_tag: u16,
        channels: u16,
        sample_rate: u32,
        block_align: u16,
        bits: u16,
    },
    Other,
}

#[derive(Debug, Clone)]
pub struct AviStream {
    pub kind: AviStreamKind,
    /// `dwRate / dwScale`: frames per second for video, blocks per second for audio.
    pub rate: f64,
    /// `dwLength` in units of `1/rate`: frames for video, blocks (sample frames) for PCM.
    pub length: u64,
}

pub struct AviReader {
    r: BufReader<File>,
    pub streams: Vec<AviStream>,
    /// Where data reading continues, and where the current list of chunks ends.
    pos: u64,
    end: u64,
    /// The `LIST movi` lists still to read: `(start, end)`.
    pending: Vec<(u64, u64)>,
}

/// A data chunk: which stream it belongs to and its payload.
pub struct AviChunk {
    pub stream: usize,
    pub data: Vec<u8>,
}

/// Larger chunks than this are corrupt (or hostile).
const MAX_CHUNK: u64 = 256 << 20;
const MAX_ITEMS: usize = 1 << 16;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl AviReader {
    pub fn open(path: &Path) -> Result<Self> {
        let f = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
        let len = f.metadata().map_err(|source| Error::Io { path: path.into(), source })?.len();
        let mut r = BufReader::new(f);
        let mut head = [0u8; 12];
        r.read_exact(&mut head).map_err(|_| err("file is too short"))?;
        if &head[..4] != b"RIFF" || &head[8..12] != b"AVI " {
            return Err(err("not an AVI file"));
        }
        let mut reader = Self { r, streams: Vec::new(), pos: 0, end: 0, pending: Vec::new() };
        // Top level: RIFF AVI , then any number of RIFF AVIX (OpenDML) holding more data.
        let mut riff_start = 12u64;
        let mut riff_end = (8 + u32_at(&head, 4) as u64).min(len);
        let mut first = true;
        for _ in 0..MAX_ITEMS {
            reader.scan_riff(riff_start, riff_end, first)?;
            first = false;
            // Next top-level RIFF chunk, if any.
            let next = riff_end + (riff_end & 1);
            if next + 12 > len {
                break;
            }
            reader.r.seek(SeekFrom::Start(next)).map_err(|_| err("seek failed"))?;
            let mut h = [0u8; 12];
            if reader.r.read_exact(&mut h).is_err() || &h[..4] != b"RIFF" || &h[8..12] != b"AVIX" {
                break;
            }
            riff_start = next + 12;
            riff_end = (next + 8 + u32_at(&h, 4) as u64).min(len);
        }
        if reader.streams.is_empty() {
            return Err(err("no stream headers"));
        }
        reader.pending.reverse();
        Ok(reader)
    }

    /// Collect stream headers (from the first RIFF) and `movi` lists.
    fn scan_riff(&mut self, start: u64, end: u64, headers: bool) -> Result<()> {
        let mut pos = start;
        for _ in 0..MAX_ITEMS {
            if pos + 8 > end {
                return Ok(());
            }
            let (id, size) = self.chunk_header(pos)?;
            let body = pos + 8;
            let next = body + size + (size & 1);
            if &id == b"LIST" && size >= 4 {
                let mut kind = [0u8; 4];
                self.r.read_exact(&mut kind).map_err(|_| err("truncated list"))?;
                match &kind {
                    b"hdrl" if headers => self.read_hdrl(body + 4, (body + size).min(end))?,
                    b"movi" => self.pending.push((body + 4, (body + size).min(end))),
                    _ => {}
                }
            }
            pos = next;
        }
        Err(err("too many chunks"))
    }

    fn chunk_header(&mut self, pos: u64) -> Result<([u8; 4], u64)> {
        self.r.seek(SeekFrom::Start(pos)).map_err(|_| err("seek failed"))?;
        let mut h = [0u8; 8];
        self.r.read_exact(&mut h).map_err(|_| err("truncated chunk"))?;
        Ok(([h[0], h[1], h[2], h[3]], u32_at(&h, 4) as u64))
    }

    fn read_body(&mut self, size: u64) -> Result<Vec<u8>> {
        if size > MAX_CHUNK {
            return Err(err("chunk is implausibly large"));
        }
        let mut v = vec![0u8; size as usize];
        self.r.read_exact(&mut v).map_err(|_| err("truncated chunk"))?;
        Ok(v)
    }

    fn read_hdrl(&mut self, start: u64, end: u64) -> Result<()> {
        let mut pos = start;
        for _ in 0..MAX_ITEMS {
            if pos + 8 > end {
                return Ok(());
            }
            let (id, size) = self.chunk_header(pos)?;
            if &id == b"LIST" && size >= 4 {
                let mut kind = [0u8; 4];
                self.r.read_exact(&mut kind).map_err(|_| err("truncated list"))?;
                if &kind == b"strl" {
                    self.read_strl(pos + 12, (pos + 8 + size).min(end))?;
                }
            }
            pos += 8 + size + (size & 1);
        }
        Err(err("too many header chunks"))
    }

    fn read_strl(&mut self, start: u64, end: u64) -> Result<()> {
        let (mut strh, mut strf) = (None, None);
        let mut pos = start;
        while pos + 8 <= end {
            let (id, size) = self.chunk_header(pos)?;
            match &id {
                b"strh" => strh = Some(self.read_body(size.min(4096))?),
                b"strf" => strf = Some(self.read_body(size.min(4096))?),
                _ => {}
            }
            pos += 8 + size + (size & 1);
        }
        let strh = strh.filter(|h| h.len() >= 36).ok_or_else(|| err("stream without a header"))?;
        let (scale, rate, length) = (u32_at(&strh, 20), u32_at(&strh, 24), u32_at(&strh, 32));
        let strf = strf.unwrap_or_default();
        let kind = match &strh[..4] {
            b"vids" if strf.len() >= 20 => AviStreamKind::Video {
                fourcc: [strf[16], strf[17], strf[18], strf[19]],
                width: u32_at(&strf, 4),
                height: (u32_at(&strf, 8) as i32).unsigned_abs(),
            },
            b"auds" if strf.len() >= 16 => {
                let mut tag = u16_at(&strf, 0);
                // WAVE_FORMAT_EXTENSIBLE: the real format is the sub-format GUID's first two bytes.
                if tag == 0xFFFE && strf.len() >= 26 {
                    tag = u16_at(&strf, 24);
                }
                AviStreamKind::Audio {
                    format_tag: tag,
                    channels: u16_at(&strf, 2),
                    sample_rate: u32_at(&strf, 4),
                    block_align: u16_at(&strf, 12),
                    bits: u16_at(&strf, 14),
                }
            }
            _ => AviStreamKind::Other,
        };
        let rate = if scale > 0 { rate as f64 / scale as f64 } else { 0.0 };
        self.streams.push(AviStream { kind, rate, length: length as u64 });
        Ok(())
    }

    /// The next data chunk of any stream, in file order.
    pub fn next_chunk(&mut self) -> Result<Option<AviChunk>> {
        loop {
            if self.pos + 8 > self.end {
                let Some((s, e)) = self.pending.pop() else { return Ok(None) };
                (self.pos, self.end) = (s, e);
                continue;
            }
            let (id, size) = self.chunk_header(self.pos)?;
            let body = self.pos + 8;
            if &id == b"LIST" {
                // `LIST rec ` groups chunks: step inside.
                self.pos = body + 4;
                continue;
            }
            self.pos = body + size + (size & 1);
            // Data chunks are named "NNxx": stream number, then dc/db/wb/…
            let stream = (id[0] as char).to_digit(10).zip((id[1] as char).to_digit(10)).map(|(a, b)| (a * 10 + b) as usize);
            let Some(stream) = stream.filter(|&s| s < self.streams.len()) else { continue };
            if &id[2..] == b"pc" {
                continue; // palette change
            }
            let data = self.read_body(size)?;
            return Ok(Some(AviChunk { stream, data }));
        }
    }

    /// Index of the first stream of this kind.
    pub fn first(&self, video: bool) -> Option<usize> {
        self.streams.iter().position(|s| matches!((&s.kind, video), (AviStreamKind::Video { .. }, true) | (AviStreamKind::Audio { .. }, false)))
    }
}

/// The soundtrack of an AVI: uncompressed PCM (integer or float), which is what cameras
/// that record Motion JPEG write.
pub struct AviAudioSource {
    reader: AviReader,
    stream: usize,
    info: crate::StreamInfo,
    tags: crate::Tags,
    /// Bytes per sample, and whether samples are floats.
    width: usize,
    float: bool,
    /// Leftover bytes of a sample split across chunks.
    carry: Vec<u8>,
    buf: Vec<f32>,
}

impl AviAudioSource {
    /// `track` picks among the audio streams (`None` = the first).
    pub fn open(path: &Path, track: Option<u32>) -> Result<Self> {
        let reader = AviReader::open(path)?;
        let audio: Vec<usize> = (0..reader.streams.len()).filter(|&i| matches!(reader.streams[i].kind, AviStreamKind::Audio { .. })).collect();
        let stream = *audio.get(track.unwrap_or(0) as usize).ok_or_else(|| Error::Codec("this video has no sound".into()))?;
        let s = &reader.streams[stream];
        let AviStreamKind::Audio { format_tag, channels, sample_rate, bits, .. } = s.kind else { unreachable!() };
        let float = match format_tag {
            1 => false,
            3 => true,
            0x55 => return Err(Error::Codec("this AVI's sound is MP3 inside AVI, which isn't supported yet".into())),
            0x11 | 0x02 => return Err(Error::Codec("this AVI's sound is ADPCM, which isn't supported yet".into())),
            0x2000 => return Err(Error::Codec("this file's sound is Dolby Digital (AC-3), which isn't supported yet".into())),
            t => return Err(Error::Codec(format!("this AVI's sound uses a format (0x{t:04x}) that isn't supported yet"))),
        };
        let width = (bits as usize).div_ceil(8);
        if channels == 0 || sample_rate == 0 || !matches!((float, width), (false, 1..=4) | (true, 4 | 8)) {
            return Err(Error::Codec("this AVI's sound has an unusual layout that isn't supported".into()));
        }
        let info = crate::StreamInfo {
            sample_rate,
            source_rate: sample_rate,
            channels,
            source_bits: (!float).then_some(bits as u32),
            total_frames: (s.length > 0).then_some(s.length),
        };
        Ok(Self { reader, stream, info, tags: crate::Tags::default(), width, float, carry: Vec::new(), buf: Vec::new() })
    }

    fn sample(&self, b: &[u8]) -> f32 {
        match (self.float, self.width) {
            (true, 4) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            (true, _) => f64::from_le_bytes(b[..8].try_into().unwrap()) as f32,
            // 8-bit WAV-style PCM is unsigned.
            (false, 1) => (b[0] as f32 - 128.0) / 128.0,
            (false, 2) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            (false, 3) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            (false, _) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    }
}

impl crate::AudioSource for AviAudioSource {
    fn info(&self) -> &crate::StreamInfo {
        &self.info
    }
    fn tags(&self) -> &crate::Tags {
        &self.tags
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let frame = self.width * self.info.channels as usize;
        loop {
            let Some(chunk) = self.reader.next_chunk()? else { return Ok(None) };
            if chunk.stream != self.stream {
                continue;
            }
            let mut bytes = std::mem::take(&mut self.carry);
            bytes.extend_from_slice(&chunk.data);
            let whole = bytes.len() / frame * frame;
            self.carry = bytes[whole..].to_vec();
            self.buf.clear();
            for s in bytes[..whole].chunks_exact(self.width) {
                let v = self.sample(s);
                self.buf.push(v);
            }
            if !self.buf.is_empty() {
                return Ok(Some(&self.buf));
            }
        }
    }
}

/// AVI soundtracks for the audio converter.
pub struct AviAudioDecoder;

impl crate::AudioDecoder for AviAudioDecoder {
    fn format(&self) -> dotty_core::Format {
        dotty_core::Format::Avi
    }
    fn open(&self, path: &Path) -> Result<Box<dyn crate::AudioSource>> {
        Ok(Box::new(AviAudioSource::open(path, None)?))
    }
    fn open_track(&self, path: &Path, track: u32) -> Result<Box<dyn crate::AudioSource>> {
        Ok(Box::new(AviAudioSource::open(path, Some(track))?))
    }
    fn tracks(&self, path: &Path) -> Vec<crate::AudioTrack> {
        let Ok(r) = AviReader::open(path) else { return Vec::new() };
        let audio: Vec<_> = r.streams.iter().filter_map(|s| match s.kind {
            AviStreamKind::Audio { format_tag, channels, .. } => Some((format_tag, channels)),
            _ => None,
        }).collect();
        if audio.len() < 2 {
            return Vec::new();
        }
        audio
            .iter()
            .enumerate()
            .map(|(i, &(tag, ch))| crate::AudioTrack {
                label: format!("{}, {}", if matches!(tag, 1 | 3) { "PCM" } else { "Other" }, if ch == 1 { "mono".into() } else { format!("{ch} channels") }),
                default: i == 0,
                supported: matches!(tag, 1 | 3),
            })
            .collect()
    }
}
