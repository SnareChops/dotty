//! Ogg Vorbis output via the pure-Rust `oxideav-vorbis` encoder (block switching,
//! psychoacoustic floors, channel coupling).
//!
//! The encoder is a whole-stream design (it trains codebooks on the entire file) and is
//! single-threaded and slow, roughly 2-3x slower than real time. It runs on a helper
//! thread so cancellation and the progress bar stay responsive.

use crate::tags::Tags;
use crate::{AudioEncoder, AudioSource};
use oxideav_vorbis::comment::VorbisCommentHeader;
use oxideav_vorbis::encoder::write_comment_header;
use oxideav_vorbis::oggfile::{StreamEncoderConfig, encode_pcm_to_packets, mux_vorbis_stream};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};
use transformer_core::{ConvertCtx, Error, Format, Preset, Result, WriteSeek};

/// Quality knob for the encoder (0..1). Its floor is already ~70-130 kbps; there is no
/// smaller-than-that setting short of the (much slower) bitrate-targeting mode.
fn quality(preset: Preset) -> f32 {
    match preset {
        Preset::Low => 0.0,
        Preset::Medium => 0.1,
        Preset::High => 0.2,
        Preset::Lossless => 0.9,
    }
}

/// Expected encode time relative to audio length, only used to pace the progress bar.
const ENCODE_SLOWDOWN: f32 = 2.5;

/// Share of the progress bar for decoding the input; the encode reports the rest.
const READ_SHARE: f32 = 0.25;

pub struct Vorbis;

impl AudioEncoder for Vorbis {
    fn format(&self) -> Format {
        Format::Ogg
    }
    fn max_channels(&self) -> u16 {
        2
    }
    fn read_share(&self) -> f32 {
        READ_SHARE
    }
    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        encode(src, tags, ctx, out)
    }
}

fn encode(src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
    let rate = src.info().sample_rate;
    let ch = src.info().channels as usize;

    // Decode everything first: the encoder needs the whole stream.
    let mut interleaved = Vec::new();
    while let Some(chunk) = src.next_chunk()? {
        interleaved.extend_from_slice(chunk);
    }
    let n = interleaved.len() / ch;
    if n == 0 {
        return Err(Error::Codec("no audio samples to encode".into()));
    }
    let rows: Vec<Vec<f32>> = (0..ch).map(|c| interleaved.iter().skip(c).step_by(ch).copied().collect()).collect();
    drop(interleaved);

    let mut cfg = StreamEncoderConfig::new(rate, ch as u8);
    cfg.quality = quality(ctx.options.preset);
    cfg.training_iterations = 0; // ~35% faster for <1% larger files
    // The amplitude-band residue ladder search costs ~5x the encode time (superlinear in
    // file length) for ~2% smaller output; not worth it. Hidden "A/B lever" field, so the
    // crate version is pinned exactly in Cargo.toml.
    cfg.residue_bands = false;

    // Encode on a helper thread; poll so cancel and progress keep working.
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(encode_pcm_to_packets(&rows, &cfg));
    });
    let started = Instant::now();
    let expected = (n as f32 / rate as f32 * ENCODE_SLOWDOWN).max(0.5);
    let stream = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(r) => break r.map_err(|e| Error::Codec(format!("vorbis encode: {e:?}")))?,
            Err(RecvTimeoutError::Timeout) => {
                // A cancelled job abandons the helper thread, which finishes in the background.
                ctx.check_cancelled()?;
                (ctx.progress)(READ_SHARE + 0.7 * (started.elapsed().as_secs_f32() / expected).min(0.97));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::Codec("vorbis encoder thread failed".into()));
            }
        }
    };

    let comments = tags.vorbis_fields().into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    let comment = write_comment_header(&VorbisCommentHeader { vendor: "Transformer".into(), comments })
        .map_err(|e| Error::Codec(format!("vorbis comment header: {e:?}")))?;
    let serial = 0x5446_0001 ^ (n as u32);
    let ogg = mux_vorbis_stream(serial, &stream.identification, &comment, &stream.setup, &stream.audio)
        .map_err(|e| Error::Codec(format!("ogg mux: {e:?}")))?;
    out.write_all(&ogg).map_err(|e| Error::Codec(format!("write failed: {e}")))
}
