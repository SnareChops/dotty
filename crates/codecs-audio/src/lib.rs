//! Audio conversion: decode WAV / FLAC / MP3 / Ogg Vorbis (via `symphonia`) and Opus (`opus-pure`),
//! encode WAV, FLAC, MP3, Ogg Vorbis and Ogg Opus.
//!
//! Every decoder produces an [`AudioSource`] and every encoder consumes one, so each
//! format only has to be implemented once in each direction.

mod aiff;
mod decode;
pub mod edit;
mod flac;
mod id3;
mod mp3;
mod opus;
mod resample;
mod stages;
mod tags;
mod vorbis;
mod wav;

pub use decode::{OggOpusDecoder, SymphoniaDecoder};
pub use tags::Tags;

use stages::{Metered, Remix, Resample};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use dotty_core::{Choice, ConvertCtx, Converter, Error, Format, Registry, Result, WriteSeek};

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    /// Bit depth of the source samples if it has a fixed one (PCM/FLAC); `None` for lossy codecs.
    pub source_bits: Option<u32>,
    pub total_frames: Option<u64>,
    /// Sample rate of the original file, before any resampling stage.
    pub source_rate: u32,
}

/// The audio intermediate: a stream of interleaved `f32` PCM in [-1, 1].
///
/// `f32` represents every ≤24-bit integer sample exactly, so lossless → lossless
/// conversions stay bit-exact.
pub trait AudioSource {
    fn info(&self) -> &StreamInfo;
    fn tags(&self) -> &Tags;
    /// Next run of whole interleaved frames, or `None` at end of stream.
    fn next_chunk(&mut self) -> Result<Option<&[f32]>>;
    /// After the stream is exhausted: a warning if the input looked damaged.
    fn end_note(&self) -> Option<String> {
        None
    }
}

impl<S: AudioSource + ?Sized> AudioSource for &mut S {
    fn info(&self) -> &StreamInfo {
        (**self).info()
    }
    fn tags(&self) -> &Tags {
        (**self).tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        (**self).next_chunk()
    }
    fn end_note(&self) -> Option<String> {
        (**self).end_note()
    }
}

impl<S: AudioSource + ?Sized> AudioSource for Box<S> {
    fn info(&self) -> &StreamInfo {
        (**self).info()
    }
    fn tags(&self) -> &Tags {
        (**self).tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        (**self).next_chunk()
    }
    fn end_note(&self) -> Option<String> {
        (**self).end_note()
    }
}

/// Opens one container/codec as an [`AudioSource`].
pub trait AudioDecoder: Send + Sync {
    fn format(&self) -> Format;
    /// The file's default audio track.
    fn open(&self, path: &Path) -> Result<Box<dyn AudioSource>>;
    /// Audio track `track` (in container order, as [`AudioDecoder::tracks`] lists them).
    fn open_track(&self, path: &Path, track: u32) -> Result<Box<dyn AudioSource>> {
        if track == 0 { self.open(path) } else { Err(Error::Codec(format!("this file has no audio track {}", track + 1))) }
    }
    /// The file's audio tracks, for a picker; empty when the format only holds one.
    fn tracks(&self, _path: &Path) -> Vec<AudioTrack> {
        Vec::new()
    }
}

/// One audio track of a file, as offered in the editor's track picker.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrack {
    /// "Opus, stereo, English".
    pub label: String,
    /// The one used unless another is picked.
    pub default: bool,
    /// Whether it can be decoded (an AAC track can't).
    pub supported: bool,
}

/// Writes one format from an [`AudioSource`]. The pipeline adapts the stream to the
/// limits declared here (channel count, sample rate) before `encode` sees it.
pub trait AudioEncoder: Send + Sync {
    fn format(&self) -> Format;
    /// Most channels the format can carry; more are mixed down to stereo first.
    fn max_channels(&self) -> u16 {
        u16::MAX
    }
    /// The rate to encode a `rate` Hz source at; anything else means resampling.
    fn sample_rate(&self, rate: u32) -> u32 {
        rate
    }
    /// Format-specific settings, besides the channel and sample-rate choices every audio
    /// output gets.
    fn choices(&self) -> Vec<Choice> {
        Vec::new()
    }
    /// Part of the progress bar covered by reading the input. Encoders that only start
    /// work once everything is read report the rest themselves.
    fn read_share(&self) -> f32 {
        1.0
    }
    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()>;
}

pub const CHANNELS: Choice = Choice {
    key: "channels",
    label: "Channels",
    help: "Keep the source's channels, or mix to stereo or mono. Formats that hold at most two channels mix surround down to stereo anyway.",
    values: &[("keep", "Keep"), ("stereo", "Stereo"), ("mono", "Mono")],
    multi_image_only: false,
};

pub const SAMPLE_RATE: Choice = Choice {
    key: "rate",
    label: "Sample rate",
    help: "Keep the source's sample rate, or convert. 44.1 kHz is the CD rate, 48 kHz is used for video; lower rates make smaller files for speech.",
    values: &[
        ("keep", "Keep"),
        ("48000", "48 kHz"),
        ("44100", "44.1 kHz"),
        ("32000", "32 kHz"),
        ("22050", "22.05 kHz"),
        ("16000", "16 kHz"),
    ],
    multi_image_only: false,
};

/// Bit depth for lossless outputs that only store integers.
pub const PCM_DEPTH: Choice = Choice {
    key: "depth",
    label: "Bit depth",
    help: "Match source keeps 16-bit audio at 16 bits and stores anything deeper at 24 bits.",
    values: &[("auto", "Match source"), ("16", "16-bit"), ("24", "24-bit")],
    multi_image_only: false,
};

/// How samples are stored by a PCM output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    Int(u32),
    Float,
}

/// The sample format picked with a bit-depth choice ("auto", "16", "24" or "32f").
pub(crate) fn sample_format(picked: &str, source_bits: Option<u32>) -> SampleFormat {
    match picked {
        "16" => SampleFormat::Int(16),
        "24" => SampleFormat::Int(24),
        "32f" => SampleFormat::Float,
        _ => SampleFormat::Int(output_bits(source_bits)),
    }
}

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(AudioConverter::default()));
}

/// Any registered decoder → [`AudioSource`] → (remix, resample) → any registered encoder.
pub struct AudioConverter {
    decoders: Vec<Box<dyn AudioDecoder>>,
    encoders: Vec<Box<dyn AudioEncoder>>,
}

impl Default for AudioConverter {
    /// Every codec this crate provides.
    fn default() -> Self {
        let mut c = Self::empty();
        use Format::*;
        // Videos too: their soundtrack converts like any audio file.
        for f in [Wav, Flac, Mp3, Ogg, Aiff, Caf, M4a, Mka, Mp2, Mp4, Mov, Mkv, Webm] {
            c.add_decoder(SymphoniaDecoder(f));
        }
        c.add_decoder(OggOpusDecoder);
        c.add_encoder(wav::WavEncoder);
        c.add_encoder(aiff::AiffEncoder);
        c.add_encoder(flac::FlacEncoder);
        c.add_encoder(mp3::Mp3);
        c.add_encoder(vorbis::Vorbis);
        c.add_encoder(opus::Opus);
        c
    }
}

impl AudioConverter {
    pub fn empty() -> Self {
        Self { decoders: Vec::new(), encoders: Vec::new() }
    }

    pub fn add_decoder(&mut self, d: impl AudioDecoder + 'static) {
        self.decoders.push(Box::new(d));
    }

    pub fn add_encoder(&mut self, e: impl AudioEncoder + 'static) {
        self.encoders.push(Box::new(e));
    }

    /// Open `path` with the decoder registered for `from`.
    pub fn open(&self, path: &Path, from: Format) -> Result<Box<dyn AudioSource>> {
        self.open_track(path, from, None)
    }

    /// Open audio track `track` (`None` = the default one).
    pub fn open_track(&self, path: &Path, from: Format, track: Option<u32>) -> Result<Box<dyn AudioSource>> {
        let d = self.decoders.iter().find(|d| d.format() == from).ok_or(Error::UnknownFormat)?;
        match track {
            Some(t) => d.open_track(path, t),
            None => d.open(path),
        }
    }

    /// The file's audio tracks (empty when there's only ever one).
    pub fn tracks(&self, path: &Path, from: Format) -> Vec<AudioTrack> {
        self.decoders.iter().find(|d| d.format() == from).map(|d| d.tracks(path)).unwrap_or_default()
    }
}

impl Converter for AudioConverter {
    /// Includes `(f, f)`: re-saving an edited file in its own format.
    fn routes(&self) -> Vec<(Format, Format)> {
        self.decoders
            .iter()
            .flat_map(|d| self.encoders.iter().map(move |e| (d.format(), e.format())))
            .collect()
    }

    fn choices(&self, to: Format) -> Vec<Choice> {
        let Some(e) = self.encoders.iter().find(|e| e.format() == to) else { return Vec::new() };
        e.choices().into_iter().chain([CHANNELS, SAMPLE_RATE]).collect()
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        let encoder = self.encoders.iter().find(|e| e.format() == to).ok_or(Error::NoRoute { from, to })?;
        (ctx.progress)(0.0);
        let track = ctx.edits.and_then(|e| e.audio()).and_then(|a| a.track);
        let mut decoded = self.open_track(src, from, track)?;
        let info = decoded.info().clone();
        let tags = if ctx.options.strip_metadata { Tags::default() } else { decoded.tags().clone() };

        let file = File::create(dst).map_err(|source| Error::Io { path: dst.into(), source })?;
        let mut out = BufWriter::new(file);
        {
            let audio_edits = ctx.edits.and_then(|e| e.audio()).map(|a| a.without_noops(info.channels)).filter(|a| !a.is_noop());
            let audio_edits = audio_edits.as_ref();
            // The layout written: what the edits give, then the output format's setting.
            let have = audio_edits.map_or(info.channels, |a| edit::edited_channels(a, info.channels));
            let wanted = match ctx.options.choice(to, &CHANNELS) {
                "mono" => 1,
                "stereo" => 2,
                _ => have,
            };
            let mut channels = wanted.min(encoder.max_channels());
            if channels != have {
                channels = channels.min(2);
                if channels != wanted {
                    let layout = if channels == 1 { "mono" } else { "stereo" };
                    ctx.note(format!("{have}-channel audio was mixed down to {layout}"));
                }
            }
            let plan = audio_edits.map(|a| edit::stream_plan(a, info.sample_rate));
            let until = plan.and_then(|p| p.end);
            // Normalizing reads the file twice: once to measure, once to write.
            let measuring = audio_edits.is_some_and(|a| a.volume.measures());
            let share = encoder.read_share();
            let (pass1, pass2) = if measuring { (share * 0.3, share * 0.7) } else { (0.0, share) };
            let measured = match audio_edits {
                Some(a) if measuring => {
                    let mut again = self.open_track(src, from, track)?;
                    let metered = Metered::new(&mut again, ctx, pass1).until(until);
                    Some(edit::measure(edit::shape_to(Box::new(metered), a, channels)?, None)?)
                }
                _ => None,
            };
            let metered = Metered::new(&mut decoded, ctx, pass2).offset(pass1).until(until);
            let mut chain: Box<dyn AudioSource + '_> = Box::new(metered);
            match audio_edits {
                // Mixed to the output layout before the volume, as it was measured.
                Some(a) => chain = edit::apply_to(chain, a, measured.as_ref(), channels)?,
                None if channels != have => chain = Box::new(Remix::new(chain, channels)),
                None => {}
            }
            let wanted = match ctx.options.choice(to, &SAMPLE_RATE) {
                "keep" => info.sample_rate,
                r => r.parse().unwrap_or(info.sample_rate),
            };
            let rate = encoder.sample_rate(wanted);
            if rate != wanted {
                ctx.note(format!("{to} can't store {wanted} Hz audio: resampled to {rate} Hz"));
            }
            if rate != info.sample_rate {
                chain = Box::new(Resample::new(chain, rate)?);
            }
            encoder.encode(&mut *chain, &tags, ctx, &mut out)?;
        }
        out.flush().map_err(|source| Error::Io { path: dst.into(), source })?;
        if let Some(note) = decoded.end_note() {
            ctx.note(note);
        }
        (ctx.progress)(1.0);
        Ok(())
    }
}

/// Keep the source's depth when it's 16 or 24-bit PCM; anything deeper becomes 24,
/// anything shallower or lossy becomes 16.
pub(crate) fn output_bits(source: Option<u32>) -> u32 {
    match source {
        Some(b) if b > 16 => 24,
        _ => 16,
    }
}

/// f32 in [-1, 1] → signed integer of `bits` width, rounded and clipped.
pub(crate) fn quantize(x: f32, bits: u32) -> i32 {
    let scale = (1i64 << (bits - 1)) as f32;
    (x * scale).round().clamp(-scale, scale - 1.0) as i32
}

#[cfg(test)]
mod tests;
