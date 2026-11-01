//! Decoders: everything `symphonia` reads (WAV, AIFF, CAF, FLAC, MP1/2/3, Ogg Vorbis,
//! ALAC in MP4, Matroska/WebM), with Opus packets from Matroska handed to `opus-pure`;
//! Ogg Opus files go straight through `opus-pure`.

use crate::tags::Tags;
use crate::{AudioDecoder, AudioSource, StreamInfo};
use opus_pure::{MAX_PACKET_SAMPLES, OggOpusReader, OggPacket, OpusDecoder, Trim};
use std::fs::File;
use std::io::BufReader;
use std::ops::Range;
use std::path::Path;
use symphonia::core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_OPUS};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder as SymDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use dotty_core::{Error, Format, Result};

/// Any format `symphonia` reads (registered once per format).
pub struct SymphoniaDecoder(pub Format);

impl AudioDecoder for SymphoniaDecoder {
    fn format(&self) -> Format {
        self.0
    }
    fn open(&self, path: &Path) -> Result<Box<dyn AudioSource>> {
        Ok(Box::new(SymphoniaSource::open(path, self.0)?))
    }
}

pub struct OggOpusDecoder;

impl AudioDecoder for OggOpusDecoder {
    fn format(&self) -> Format {
        Format::Opus
    }
    fn open(&self, path: &Path) -> Result<Box<dyn AudioSource>> {
        let file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
        Ok(Box::new(OpusSource::open(file)?))
    }
}

/// Turns a container's packets into samples.
enum PacketDecoder {
    Symphonia(Box<dyn SymDecoder>),
    /// Opus inside Matroska/WebM, which `symphonia` can demux but not decode.
    Opus { decoder: Box<OpusDecoder>, channels: usize, skip: usize },
}

impl PacketDecoder {
    fn new(params: &AudioCodecParameters) -> Result<Self> {
        if params.codec == CODEC_ID_AAC {
            return Err(Error::Codec(
                "this file's audio is AAC, which isn't supported (AAC still requires a patent licence)".into(),
            ));
        }
        if params.codec == CODEC_ID_OPUS {
            // OpusHead: magic, version, channels, pre-skip (LE), rate, gain, mapping family.
            let head = params.extra_data.as_deref().filter(|h| h.len() >= 19 && h.starts_with(b"OpusHead"));
            let channels = head.map_or(2, |h| h[9] as usize);
            if head.is_some_and(|h| h[18] != 0) || channels > 2 {
                return Err(Error::Codec("surround Opus (more than 2 channels) is not supported yet".into()));
            }
            let skip = head.map_or(0, |h| u16::from_le_bytes([h[10], h[11]]) as usize);
            let decoder = OpusDecoder::new(48000, channels).map_err(|e| Error::Codec(format!("opus: {e}")))?;
            return Ok(Self::Opus { decoder: Box::new(decoder), channels, skip });
        }
        symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map(Self::Symphonia)
            .map_err(|e| match e {
                SymError::Unsupported(_) => Error::Codec("this file's audio codec isn't supported".into()),
                e => sym_err(e),
            })
    }
}

struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: PacketDecoder,
    track_id: u32,
    scratch: Vec<f32>,
    /// Set when a chained stream couldn't be joined onto the first.
    chain_note: Option<String>,
    info: StreamInfo,
    tags: Tags,
    /// Which format this was opened as (decides how far to trust `info.total_frames`).
    from: Format,
    frames_read: u64,
    /// The stream ended with an I/O error instead of a clean end.
    cut_short: bool,
}

struct OpusSource {
    reader: OggOpusReader<BufReader<File>>,
    decoder: OpusDecoder,
    trim: Trim,
    packet: OggPacket,
    block: Vec<f32>,
    channels: usize,
    info: StreamInfo,
    tags: Tags,
}
fn sym_err(e: SymError) -> Error {
    Error::Codec(e.to_string())
}

impl SymphoniaSource {
    fn open(path: &Path, from: Format) -> Result<Self> {
        let file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(from.extension());

        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(sym_err)?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| Error::Codec("no audio track found".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| Error::Codec("missing audio parameters".into()))?;
        let decoder = PacketDecoder::new(params)?;

        let (sample_rate, channels) = match &decoder {
            // Opus always decodes at 48 kHz here.
            PacketDecoder::Opus { channels, .. } => (48000, *channels as u16),
            PacketDecoder::Symphonia(_) => (
                params.sample_rate.ok_or_else(|| Error::Codec("unknown sample rate".into()))?,
                params.channels.as_ref().map(|c| c.count() as u16).ok_or_else(|| Error::Codec("unknown channel layout".into()))?,
            ),
        };
        let lossy = from.is_lossy() || matches!(decoder, PacketDecoder::Opus { .. });
        let info = StreamInfo {
            sample_rate,
            source_rate: sample_rate,
            channels,
            source_bits: params.bits_per_sample.filter(|_| !lossy),
            total_frames: track.num_frames.filter(|_| !matches!(decoder, PacketDecoder::Opus { .. })),
        };
        let track_id = track.id;
        // symphonia 0.6.1's WAV reader parses LIST/INFO but drops it, so read it ourselves.
        let tags = if from == Format::Wav { crate::wav::read_info_tags(path) } else { read_tags(format.as_mut()) };
        Ok(Self {
            format,
            decoder,
            track_id,
            scratch: Vec::new(),
            chain_note: None,
            info,
            tags,
            from,
            frames_read: 0,
            cut_short: false,
        })
    }

    /// The next logical stream of a chained Ogg file, joined on if it has the same layout.
    fn next_link(&mut self) -> Result<bool> {
        let Some(track) = self.format.default_track(TrackType::Audio) else { return Ok(false) };
        let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()) else { return Ok(false) };
        let same = params.sample_rate == Some(self.info.sample_rate)
            && params.channels.as_ref().map(|c| c.count() as u16) == Some(self.info.channels);
        if !same {
            self.chain_note = Some(
                "This file holds several streams with different audio formats: only the first was converted".into(),
            );
            return Ok(false);
        }
        self.track_id = track.id;
        self.decoder = PacketDecoder::new(params)?;
        Ok(true)
    }

    /// Corrupt packets are skipped rather than aborting the whole file.
    fn next_samples(&mut self) -> Result<Option<&[f32]>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                // Chained Ogg: another logical stream follows.
                Err(SymError::ResetRequired) => {
                    if self.next_link()? {
                        continue;
                    }
                    return Ok(None);
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.cut_short = true;
                    return Ok(None);
                }
                Err(e) => return Err(sym_err(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match &mut self.decoder {
                PacketDecoder::Symphonia(decoder) => match decoder.decode(&packet) {
                    Ok(buf) => {
                        self.scratch.resize(buf.samples_interleaved(), 0.0);
                        buf.copy_to_slice_interleaved(&mut self.scratch[..]);
                    }
                    Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
                    Err(e) => return Err(sym_err(e)),
                },
                PacketDecoder::Opus { decoder, channels, skip } => {
                    self.scratch.resize(MAX_PACKET_SAMPLES * *channels, 0.0);
                    let Ok(n) = decoder.decode(&packet.data, MAX_PACKET_SAMPLES, &mut self.scratch) else { continue };
                    // Drop the encoder's start-up delay ("pre-skip").
                    let dropped = n.min(*skip);
                    *skip -= dropped;
                    self.scratch.truncate(n * *channels);
                    self.scratch.drain(..dropped * *channels);
                }
            }
            if !self.scratch.is_empty() {
                self.frames_read += (self.scratch.len() / self.info.channels as usize) as u64;
                return Ok(Some(&self.scratch));
            }
        }
    }
}

impl OpusSource {
    fn open(file: File) -> Result<Self> {
        let codec = |e: opus_pure::Error| Error::Codec(format!("opus: {e}"));
        let reader = OggOpusReader::new(BufReader::new(file)).map_err(codec)?;
        let head = reader.head().clone();
        if head.mapping_family != 0 {
            return Err(Error::Codec("surround Opus (more than 2 channels) is not supported yet".into()));
        }
        let channels = head.channel_count as usize;
        // Opus decodes at any rate; 48 kHz is its native one and avoids resampling.
        let decoder = head.decoder(48000).map_err(codec)?;
        let trim = Trim::new(&head, 48000, channels).map_err(codec)?;
        let tags = Tags::from_vorbis_comments(reader.tags().comments.iter().filter_map(|c| c.split_once('=')));
        let info = StreamInfo { sample_rate: 48000, source_rate: 48000, channels: channels as u16, source_bits: None, total_frames: None };
        Ok(Self {
            reader,
            decoder,
            trim,
            packet: OggPacket::default(),
            block: vec![0.0; MAX_PACKET_SAMPLES * channels],
            channels,
            info,
            tags,
        })
    }
}

impl AudioSource for OpusSource {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    /// Corrupt packets are skipped rather than aborting the whole file.
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let codec = |e: opus_pure::Error| Error::Codec(format!("opus: {e}"));
        loop {
            if !self.reader.read_packet_into(&mut self.packet).map_err(codec)? {
                return Ok(None);
            }
            let n = match self.decoder.decode(&self.packet.data, MAX_PACKET_SAMPLES, &mut self.block) {
                Ok(n) => n,
                Err(_) => continue, // skip a corrupt packet
            };
            let range: Range<usize> = self.trim.keep_range(&self.packet, n * self.channels);
            if range.is_empty() {
                continue;
            }
            return Ok(Some(&self.block[range]));
        }
    }
}

impl AudioSource for SymphoniaSource {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        self.next_samples()
    }

    /// A warning if the file seems to end early or had a stream that couldn't be joined.
    fn end_note(&self) -> Option<String> {
        if let Some(note) = &self.chain_note {
            return Some(note.clone());
        }
        // Only WAV and FLAC declare an exact length; MP3/Vorbis totals are estimates.
        let exact = matches!(self.from, Format::Wav | Format::Flac);
        let short = exact
            && self.info.total_frames.is_some_and(|t| t > 0 && self.frames_read * 100 < t * 95);
        (self.cut_short || short).then(|| {
            let rate = self.info.sample_rate as f64;
            match self.info.total_frames {
                Some(t) if short => format!(
                    "The input file is cut short: only {:.1} of {:.1} seconds could be read",
                    self.frames_read as f64 / rate,
                    t as f64 / rate
                ),
                _ => "The input file ended unexpectedly; the output may be cut short".to_string(),
            }
        })
    }
}

fn read_tags(format: &mut dyn FormatReader) -> Tags {
    let mut tags = Tags::default();
    // Merge every revision seen so far; later ones win.
    let mut meta = format.metadata();
    loop {
        if let Some(rev) = meta.current() {
            for tag in &rev.media.tags {
                match tag.std.as_ref() {
                    Some(StandardTag::TrackTitle(v)) => tags.title = Some(v.to_string()),
                    Some(StandardTag::Artist(v)) => tags.artist = Some(v.to_string()),
                    Some(StandardTag::Album(v)) => tags.album = Some(v.to_string()),
                    Some(StandardTag::AlbumArtist(v)) => tags.album_artist = Some(v.to_string()),
                    Some(StandardTag::Genre(v)) => tags.genre = Some(v.to_string()),
                    Some(StandardTag::Comment(v)) => tags.comment = Some(v.to_string()),
                    Some(StandardTag::Composer(v)) => tags.composer = Some(v.to_string()),
                    Some(StandardTag::RecordingDate(v)) | Some(StandardTag::ReleaseDate(v)) => {
                        tags.date = Some(v.to_string())
                    }
                    Some(StandardTag::RecordingYear(y)) | Some(StandardTag::ReleaseYear(y)) => {
                        // A full date, if one was already seen, is more precise.
                        tags.date.get_or_insert_with(|| y.to_string());
                    }
                    Some(StandardTag::TrackNumber(n)) => tags.track = Some(*n as u32),
                    Some(StandardTag::DiscNumber(n)) => tags.disc = Some(*n as u32),
                    _ => {}
                }
            }
        }
        if meta.is_latest() {
            break;
        }
        meta.pop();
    }
    tags
}
