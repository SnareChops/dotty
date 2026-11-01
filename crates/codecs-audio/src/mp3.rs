//! MP3 output via the pure-Rust `rusty_mp3` encoder: streaming encode with an ID3v2
//! header and a gapless Info/LAME tag patched in at the end. Takes at most stereo at a
//! rate MPEG audio can carry; the pipeline adapts everything else.

use crate::id3;
use crate::tags::Tags;
use crate::{AudioEncoder, AudioSource};
use rusty_mp3::{Mp3Encoder, Mp3EncoderConfig, vbr_quality_index};
use std::io::SeekFrom;
use transformer_core::{Choice, ConvertCtx, Error, Format, Preset, Result, WriteSeek};

/// Every sample rate MPEG-1/2/2.5 Layer III can carry.
const MP3_RATES: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

pub const MP3_MODE: Choice = Choice {
    key: "mode",
    label: "Bitrate",
    help: "Variable bitrate gives the best quality per byte. Constant bitrate suits old players and streaming that expect a fixed rate.",
    values: &[("vbr", "Variable"), ("cbr", "Constant")],
    multi_image_only: false,
};

/// Quality presets: LAME-style VBR (or matching constant rates) for the lossy tiers, 320 kbps
/// CBR for "Lossless" (MP3 has no lossless mode, so that means "as good as the format allows").
fn config(preset: Preset, constant: bool) -> Mp3EncoderConfig {
    let vbr = |q| Mp3EncoderConfig { bitrate_kbps: 0, vbr_quality: Some(vbr_quality_index(q)) };
    let cbr = |kbps| Mp3EncoderConfig { bitrate_kbps: kbps, vbr_quality: None };
    match (preset, constant) {
        (Preset::Low, false) => vbr(6.0),
        (Preset::Medium, false) => vbr(4.0),
        (Preset::High, false) => vbr(2.0),
        (Preset::Low, true) => cbr(128),
        (Preset::Medium, true) => cbr(192),
        (Preset::High, true) => cbr(256),
        (Preset::Lossless, _) => cbr(320),
    }
}

/// The rate to encode at: the source's own if MP3 supports it, otherwise the nearest
/// supported rate that doesn't discard audible bandwidth.
fn target_rate(rate: u32) -> u32 {
    if MP3_RATES.contains(&rate) {
        rate
    } else if rate > 48000 {
        if rate.is_multiple_of(44100) { 44100 } else { 48000 }
    } else {
        MP3_RATES.iter().copied().find(|&r| r >= rate).unwrap_or(48000)
    }
}

fn io(e: std::io::Error) -> Error {
    Error::Codec(format!("write failed: {e}"))
}

fn enc_err(e: rusty_mp3::Error) -> Error {
    Error::Codec(format!("mp3 encode: {e}"))
}

fn drain(enc: &mut Mp3Encoder, out: &mut dyn WriteSeek) -> Result<()> {
    while let Ok(p) = enc.next_packet() {
        out.write_all(&p).map_err(io)?;
    }
    Ok(())
}

pub struct Mp3;

impl AudioEncoder for Mp3 {
    fn format(&self) -> Format {
        Format::Mp3
    }
    fn max_channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self, rate: u32) -> u32 {
        target_rate(rate)
    }
    fn choices(&self) -> Vec<Choice> {
        vec![MP3_MODE]
    }

    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let rate = src.info().sample_rate;
        let channels = src.info().channels;
        let id3 = id3::id3v2(tags);
        out.write_all(&id3).map_err(io)?;

        let constant = ctx.options.choice(Format::Mp3, &MP3_MODE) == "cbr";
        let mut enc = Mp3Encoder::new(config(ctx.options.preset, constant));
        let mut pushed = false;
        while let Some(pcm) = src.next_chunk()? {
            enc.push_pcm_f32(pcm, channels, rate).map_err(enc_err)?;
            pushed = true;
            drain(&mut enc, out)?;
        }
        if !pushed {
            return Err(Error::Codec("no audio samples to encode".into()));
        }
        enc.finish();
        drain(&mut enc, out)?;

        // Replace the placeholder Info frame with the finished one (frame count, TOC, gapless data).
        if let Some(tag) = enc.lame_tag_frame() {
            out.seek(SeekFrom::Start(id3.len() as u64)).map_err(io)?;
            out.write_all(tag).map_err(io)?;
            out.seek(SeekFrom::End(0)).map_err(io)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn picks_sensible_target_rates() {
        assert_eq!(target_rate(44100), 44100);
        assert_eq!(target_rate(96000), 48000);
        assert_eq!(target_rate(88200), 44100);
        assert_eq!(target_rate(192000), 48000);
        assert_eq!(target_rate(37800), 44100);
        assert_eq!(target_rate(5512), 8000);
    }
}
