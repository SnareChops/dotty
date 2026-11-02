//! Decoders: everything `symphonia` reads (WAV, AIFF, CAF, FLAC, MP1/2/3, Ogg Vorbis,
//! ALAC in MP4, Matroska/WebM), with Opus packets from Matroska handed to `opus-pure`;
//! Ogg Opus files go straight through `opus-pure`.

use crate::tags::Tags;
use crate::{AudioDecoder, AudioSource, AudioTrack, StreamInfo};
use opus_pure::{MAX_PACKET_SAMPLES, OggOpusReader, OggPacket, OpusDecoder, Trim};
use std::fs::File;
use std::io::BufReader;
use std::ops::Range;
use std::path::Path;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::well_known::*;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder as SymDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track, TrackType};
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
        Ok(Box::new(SymphoniaSource::open(path, self.0, None)?))
    }
    fn open_track(&self, path: &Path, track: u32) -> Result<Box<dyn AudioSource>> {
        Ok(Box::new(SymphoniaSource::open(path, self.0, Some(track as usize))?))
    }
    fn tracks(&self, path: &Path) -> Vec<AudioTrack> {
        let Ok(format) = probe(path, self.0) else { return Vec::new() };
        let default = format.default_track(TrackType::Audio).map(|t| t.id);
        audio_tracks(format.as_ref())
            .into_iter()
            .map(|t| {
                let params = t.codec_params.as_ref().and_then(|p| p.audio());
                let codec = params.map(|p| p.codec).unwrap_or_default();
                let mut label = codec_name(codec).to_string();
                if let Some(n) = params.and_then(|p| p.channels.as_ref()).map(|c| c.count()) {
                    label += match n {
                        1 => ", mono",
                        2 => ", stereo",
                        _ => "",
                    };
                    if n > 2 {
                        label += &format!(", {n} channels");
                    }
                }
                if let Some(lang) = t.language.as_deref().filter(|l| !l.is_empty() && *l != "und") {
                    label += &format!(", {lang}");
                }
                let supported = params.is_some_and(|p| unsupported_reason(p.codec).is_none());
                AudioTrack { label, default: Some(t.id) == default, supported }
            })
            .collect()
    }
}

/// The audio tracks of a container, in order.
fn audio_tracks(format: &dyn FormatReader) -> Vec<&Track> {
    format.tracks().iter().filter(|t| matches!(t.codec_params, Some(CodecParameters::Audio(_)))).collect()
}

fn codec_name(id: symphonia::core::codecs::audio::AudioCodecId) -> &'static str {
    match id {
        CODEC_ID_OPUS => "Opus",
        CODEC_ID_VORBIS => "Vorbis",
        CODEC_ID_FLAC => "FLAC",
        CODEC_ID_ALAC => "ALAC",
        CODEC_ID_MP3 => "MP3",
        CODEC_ID_MP2 => "MP2",
        CODEC_ID_MP1 => "MP1",
        CODEC_ID_AAC => "AAC",
        CODEC_ID_AC3 => "AC-3",
        CODEC_ID_EAC3 => "E-AC-3",
        // The PCM family sits in 0x100..=0x125 (no accessor for the number).
        _ if (CODEC_ID_PCM_S32LE..=CODEC_ID_PCM_MULAW).contains(&id) => "PCM",
        _ => "Other",
    }
}

/// Why a codec can't be decoded, if it can't.
fn unsupported_reason(id: symphonia::core::codecs::audio::AudioCodecId) -> Option<&'static str> {
    match id {
        CODEC_ID_AAC => Some("AAC, which isn't supported (AAC still requires a patent licence)"),
        CODEC_ID_AC3 | CODEC_ID_EAC3 => Some("Dolby Digital (AC-3), which isn't supported yet"),
        _ => None,
    }
}

fn probe(path: &Path, from: Format) -> Result<Box<dyn FormatReader>> {
    let file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(from.extension());
    symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).map_err(sym_err)
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
        if let Some(why) = unsupported_reason(params.codec) {
            return Err(Error::Codec(format!("this file's sound is {why}")));
        }
        if params.codec == CODEC_ID_OPUS {
            // OpusHead: magic, version, channels, pre-skip (LE), rate, gain, mapping family.
            let head = params.extra_data.as_deref().filter(|h| h.len() >= 19 && h.starts_with(b"OpusHead"));
            let channels = head.map_or(2, |h| h[9] as usize);
            if head.is_some_and(|h| h[18] != 0) || channels > 2 {
                return Err(Error::Codec("surround Opus (more than 2 channels) is not supported yet".into()));
            }
            // Ogg/Matroska carry an OpusHead (version 1, little-endian). From MP4, symphonia
            // passes the `dOps` box behind the same magic, but that box is version 0 and
            // big-endian, so read the pre-skip accordingly.
            let skip = head.map_or(0, |h| {
                let b = [h[10], h[11]];
                (if h[8] == 0 { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) }) as usize
            });
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
    /// `track` picks an audio track by position (`None` = the default one).
    fn open(path: &Path, from: Format, track: Option<usize>) -> Result<Self> {
        let mut format = probe(path, from)?;
        let none = || {
            Error::Codec(if from.kind() == dotty_core::Kind::Video { "this video has no sound".into() } else { "no audio track found".into() })
        };
        let track = match track {
            Some(i) => *audio_tracks(format.as_ref()).get(i).ok_or_else(none)?,
            None => format.default_track(TrackType::Audio).ok_or_else(none)?,
        };
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

/// The picture embedded in an audio file (the front cover if it says which), as encoded
/// image bytes (usually JPEG or PNG).
pub fn cover_art(path: &Path, from: Format) -> Option<Vec<u8>> {
    let mut format = probe(path, from).ok()?;
    let mut meta = format.metadata();
    let mut found: Option<Vec<u8>> = None;
    loop {
        if let Some(rev) = meta.current() {
            let front = rev.media.visuals.iter().find(|v| v.usage == Some(symphonia::core::meta::StandardVisualKey::FrontCover));
            if let Some(v) = front.or(rev.media.visuals.first()) {
                found = Some(v.data.to_vec());
            }
        }
        if meta.is_latest() || found.is_some() {
            break;
        }
        meta.pop();
    }
    found
}
