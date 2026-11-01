//! Ogg Opus output via the pure-Rust `opus-pure` codec (libopus-compatible).
//!
//! Streams at most stereo PCM at a rate Opus takes natively (the pipeline downmixes and
//! resamples to 48 kHz otherwise), encoded in 20 ms frames. The file is gapless: the
//! header carries the encoder's pre-skip and the last packet carries the end trim.

use crate::tags::Tags;
use crate::{AudioEncoder, AudioSource};
use opus_pure::{Application, MAX_PACKET_BYTES, OggOpusWriter, OpusEncoder, OpusHead, OpusTags, RateControl};
use std::io::Write;
use dotty_core::{Choice, ConvertCtx, Error, Format, Preset, Result, WriteSeek};

/// Sample rates the Opus encoder accepts directly.
const NATIVE_RATES: [u32; 5] = [8000, 12000, 16000, 24000, 48000];

/// Bits per second per channel. Opus is transparent for most music at ~64 kbps per
/// channel; "Lossless" gives the most headroom the format is worth spending.
fn bitrate_per_channel(preset: Preset) -> i32 {
    match preset {
        Preset::Low => 32_000,
        Preset::Medium => 48_000,
        Preset::High => 64_000,
        Preset::Lossless => 128_000,
    }
}

fn codec(e: opus_pure::Error) -> Error {
    Error::Codec(format!("opus: {e}"))
}

pub const OPUS_CONTENT: Choice = Choice {
    key: "content",
    label: "Tuned for",
    help: "Speech tuning keeps voices clearer at low bitrates (podcasts, voice notes); music tuning reproduces everything else more faithfully.",
    values: &[("music", "Music"), ("speech", "Speech")],
    multi_image_only: false,
};

pub struct Opus;

impl AudioEncoder for Opus {
    fn format(&self) -> Format {
        Format::Opus
    }
    fn max_channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self, rate: u32) -> u32 {
        if NATIVE_RATES.contains(&rate) { rate } else { 48000 }
    }
    fn choices(&self) -> Vec<Choice> {
        vec![OPUS_CONTENT]
    }
    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        encode(src, tags, ctx, out)
    }
}

fn encode(src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
    let rate = src.info().sample_rate;
    let ch = src.info().channels as usize;
    let frame = rate as usize / 50; // 20 ms, per channel
    let ticks = 48000 / rate as usize; // 48 kHz granule ticks per encoder sample

    let application = match ctx.options.choice(Format::Opus, &OPUS_CONTENT) {
        "speech" => Application::Voip,
        _ => Application::Audio,
    };
    let mut enc = OpusEncoder::new(rate as i32, ch, application).map_err(codec)?;
    enc.bitrate_bps = bitrate_per_channel(ctx.options.preset) * ch as i32;
    enc.rate_control = RateControl::Vbr;
    let head = OpusHead::for_encoder(&enc, src.info().source_rate);
    let pre_skip = head.pre_skip as usize;

    let mut opus_tags = OpusTags::new();
    opus_tags.vendor = "Dotty".into();
    for (k, v) in tags.vorbis_fields() {
        opus_tags.push(k, &v).map_err(codec)?;
    }
    let mut writer = OggOpusWriter::with_tags(out, head, opus_tags).map_err(codec)?;

    let mut buf: Vec<f32> = Vec::new();
    let mut sink = FrameSink { packet: vec![0u8; MAX_PACKET_BYTES], held: None, frame, ch, encoded: 0 };
    let mut total = 0u64; // real sample frames fed to the encoder
    while let Some(chunk) = src.next_chunk()? {
        total += (chunk.len() / ch) as u64;
        buf.extend_from_slice(chunk);
        sink.drain(&mut buf, &mut enc, &mut writer)?;
    }
    if total == 0 {
        return Err(Error::Codec("no audio samples to encode".into()));
    }

    // The encoder runs `pre_skip` behind its input: feed enough silence to flush the tail,
    // rounded up to whole frames, then tell the container where the real audio ends.
    let frames_total = (total as usize + pre_skip.div_ceil(ticks)).div_ceil(frame);
    buf.resize((frames_total - sink.encoded) * frame * ch, 0.0);
    sink.drain(&mut buf, &mut enc, &mut writer)?;

    let final_granule = pre_skip as u64 + total * ticks as u64;
    if let Some(last) = sink.held.take() {
        let duration = final_granule.saturating_sub(writer.granule() as u64);
        writer.write_packet_with_duration(&last, duration as u32).map_err(codec)?;
    }
    writer.finish().map_err(codec)?;
    Ok(())
}

/// Encodes whole frames and writes them one packet behind, because the last packet has to
/// carry the end-trim duration and nobody knows it is the last until the input ends.
struct FrameSink {
    packet: Vec<u8>,
    held: Option<Vec<u8>>,
    frame: usize,
    ch: usize,
    encoded: usize,
}

impl FrameSink {
    fn drain<W: Write>(&mut self, buf: &mut Vec<f32>, enc: &mut OpusEncoder, writer: &mut OggOpusWriter<W>) -> Result<()> {
        let per = self.frame * self.ch;
        let mut used = 0;
        while buf.len() - used >= per {
            let n = enc.encode(&buf[used..used + per], self.frame, &mut self.packet).map_err(codec)?;
            if let Some(prev) = self.held.replace(self.packet[..n].to_vec()) {
                writer.write_packet(&prev).map_err(codec)?;
            }
            used += per;
            self.encoded += 1;
        }
        buf.drain(..used);
        Ok(())
    }
}
