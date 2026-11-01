//! Streaming WAV writer (16/24-bit integer or 32-bit float) with optional `LIST/INFO` tags.

use crate::tags::Tags;
use crate::{AudioEncoder, AudioSource, SampleFormat, quantize, sample_format};
use std::io::{Read, Seek, SeekFrom, Write};
use dotty_core::{Choice, ConvertCtx, Format, WriteSeek};

pub const WAV_DEPTH: Choice = Choice {
    key: "depth",
    label: "Bit depth",
    help: "Match source keeps 16-bit audio at 16 bits and stores anything deeper at 24 bits. 32-bit float never clips and suits audio editors.",
    values: &[("auto", "Match source"), ("16", "16-bit"), ("24", "24-bit"), ("32f", "32-bit float")],
    multi_image_only: false,
};

pub struct WavEncoder;

impl AudioEncoder for WavEncoder {
    fn format(&self) -> Format {
        Format::Wav
    }

    fn choices(&self) -> Vec<Choice> {
        vec![WAV_DEPTH]
    }

    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> dotty_core::Result<()> {
        let info = src.info().clone();
        let format = sample_format(ctx.options.choice(Format::Wav, &WAV_DEPTH), info.source_bits);
        let mut wav = WavWriter::new(out, info.channels, info.sample_rate, format, tags)?;
        let mut ints = Vec::new();
        while let Some(chunk) = src.next_chunk()? {
            match format {
                SampleFormat::Float => wav.write_f32(chunk)?,
                SampleFormat::Int(bits) => {
                    ints.clear();
                    ints.extend(chunk.iter().map(|&s| quantize(s, bits)));
                    wav.write(&ints)?;
                }
            }
        }
        wav.finish()
    }
}

pub struct WavWriter<W: Write + Seek> {
    out: W,
    bytes_per_sample: usize,
    data_bytes: u64,
    data_len_pos: u64,
    /// Where the `fact` chunk's frame count goes (float files only).
    fact_pos: Option<u64>,
    block_align: u64,
    buf: Vec<u8>,
}

fn io(e: std::io::Error) -> dotty_core::Error {
    dotty_core::Error::Codec(format!("write failed: {e}"))
}

impl<W: Write + Seek> WavWriter<W> {
    pub fn new(mut out: W, channels: u16, sample_rate: u32, format: SampleFormat, tags: &Tags) -> dotty_core::Result<Self> {
        let (bits, float) = match format {
            SampleFormat::Int(b) => (b as u16, false),
            SampleFormat::Float => (32, true),
        };
        let bps = (bits / 8) as u32;
        let block_align = channels as u32 * bps;
        let extensible = channels > 2;
        let tag = if float { 3u16 } else { 1 };
        let mut h = Vec::with_capacity(96);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        h.extend_from_slice(&(if extensible { 40u32 } else { 16 }).to_le_bytes());
        h.extend_from_slice(&(if extensible { 0xFFFEu16 } else { tag }).to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&(sample_rate * block_align).to_le_bytes());
        h.extend_from_slice(&(block_align as u16).to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        if extensible {
            h.extend_from_slice(&22u16.to_le_bytes()); // extension size
            h.extend_from_slice(&bits.to_le_bytes()); // valid bits
            h.extend_from_slice(&0u32.to_le_bytes()); // channel mask: unspecified
            // KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT
            h.extend_from_slice(&tag.to_le_bytes());
            h.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
        }
        // Non-integer formats carry a `fact` chunk with the frame count.
        let fact_pos = float.then(|| {
            h.extend_from_slice(b"fact");
            h.extend_from_slice(&4u32.to_le_bytes());
            h.extend_from_slice(&0u32.to_le_bytes());
            h.len() as u64 - 4
        });
        // Tags go before the audio so players that stop at `data` still see them.
        h.extend_from_slice(&info_chunk(tags));
        h.extend_from_slice(b"data\0\0\0\0");
        out.write_all(&h).map_err(io)?;
        let data_len_pos = h.len() as u64 - 4;
        Ok(Self {
            out,
            bytes_per_sample: bps as usize,
            data_bytes: 0,
            data_len_pos,
            fact_pos,
            block_align: block_align as u64,
            buf: Vec::new(),
        })
    }

    /// Write interleaved float samples (float writers only).
    pub fn write_f32(&mut self, samples: &[f32]) -> dotty_core::Result<()> {
        self.buf.clear();
        for &s in samples {
            self.buf.extend_from_slice(&s.to_le_bytes());
        }
        self.data_bytes += self.buf.len() as u64;
        self.out.write_all(&self.buf).map_err(io)
    }

    /// Write interleaved samples already quantised to the writer's bit depth.
    pub fn write(&mut self, samples: &[i32]) -> dotty_core::Result<()> {
        self.buf.clear();
        for &s in samples {
            self.buf.extend_from_slice(&s.to_le_bytes()[..self.bytes_per_sample]);
        }
        self.data_bytes += self.buf.len() as u64;
        self.out.write_all(&self.buf).map_err(io)
    }

    pub fn finish(mut self) -> dotty_core::Result<()> {
        if self.data_bytes % 2 == 1 {
            self.out.write_all(&[0]).map_err(io)?; // chunk padding
        }
        let end = self.out.stream_position().map_err(io)?;
        if end > u32::MAX as u64 {
            return Err(dotty_core::Error::Codec("WAV output would exceed 4 GiB".into()));
        }
        self.out.seek(SeekFrom::Start(4)).map_err(io)?;
        self.out.write_all(&((end - 8) as u32).to_le_bytes()).map_err(io)?;
        self.out.seek(SeekFrom::Start(self.data_len_pos)).map_err(io)?;
        self.out.write_all(&(self.data_bytes as u32).to_le_bytes()).map_err(io)?;
        if let Some(pos) = self.fact_pos {
            self.out.seek(SeekFrom::Start(pos)).map_err(io)?;
            self.out.write_all(&((self.data_bytes / self.block_align) as u32).to_le_bytes()).map_err(io)?;
        }
        self.out.flush().map_err(io)
    }
}

fn info_chunk(tags: &Tags) -> Vec<u8> {
    let mut list = Vec::new();
    for (id, val) in info_fields(tags) {
        let mut bytes = val.into_bytes();
        bytes.push(0);
        list.extend_from_slice(id);
        list.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        list.extend_from_slice(&bytes);
        if bytes.len() % 2 == 1 {
            list.push(0);
        }
    }
    if list.is_empty() {
        return list;
    }
    let mut chunk = Vec::with_capacity(list.len() + 12);
    chunk.extend_from_slice(b"LIST");
    chunk.extend_from_slice(&(list.len() as u32 + 4).to_le_bytes());
    chunk.extend_from_slice(b"INFO");
    chunk.extend_from_slice(&list);
    chunk
}

fn info_fields(t: &Tags) -> Vec<(&'static [u8; 4], String)> {
    let mut v = Vec::new();
    let mut add = |id: &'static [u8; 4], s: Option<String>| {
        if let Some(s) = s {
            v.push((id, s));
        }
    };
    add(b"INAM", t.title.clone());
    add(b"IART", t.artist.clone());
    add(b"IPRD", t.album.clone());
    add(b"IGNR", t.genre.clone());
    add(b"ICMT", t.comment.clone());
    add(b"ICRD", t.date.clone());
    add(b"IMUS", t.composer.clone());
    add(b"ITRK", t.track.map(|n| n.to_string()));
    v
}

/// Collect `LIST/INFO` text tags from anywhere in a RIFF/WAVE file. Best effort: any
/// malformed structure simply ends the scan.
pub fn read_info_tags(path: &std::path::Path) -> Tags {
    let mut tags = Tags::default();
    let _ = scan_info(path, &mut tags);
    tags
}

fn scan_info(path: &std::path::Path, tags: &mut Tags) -> std::io::Result<()> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut head = [0u8; 12];
    f.read_exact(&mut head)?;
    if &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
        return Ok(());
    }
    loop {
        let mut ch = [0u8; 8];
        if f.read_exact(&mut ch).is_err() {
            return Ok(());
        }
        let len = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
        let padded = len + (len & 1);
        if &ch[..4] == b"LIST" && (4..(1 << 24)).contains(&len) {
            let mut body = vec![0u8; len as usize];
            f.read_exact(&mut body)?;
            if len & 1 == 1 {
                f.seek_relative(1)?;
            }
            if &body[..4] == b"INFO" {
                parse_info_entries(&body[4..], tags);
            }
        } else {
            f.seek_relative(padded as i64)?;
        }
    }
}

fn parse_info_entries(mut b: &[u8], tags: &mut Tags) {
    while b.len() >= 8 {
        let len = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let Some(raw) = b.get(8..8 + len) else { return };
        let text = String::from_utf8_lossy(raw).trim_end_matches('\0').trim().to_string();
        if !text.is_empty() {
            match &b[..4] {
                b"INAM" => tags.title = Some(text),
                b"IART" => tags.artist = Some(text),
                b"IPRD" => tags.album = Some(text),
                b"IGNR" => tags.genre = Some(text),
                b"ICMT" => tags.comment = Some(text),
                b"ICRD" => tags.date = Some(text),
                b"IMUS" => tags.composer = Some(text),
                b"ITRK" => tags.track = text.parse().ok(),
                _ => {}
            }
        }
        b = b.get(8 + len + (len & 1)..).unwrap_or(&[]);
    }
}
