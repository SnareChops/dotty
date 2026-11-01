//! FLAC encoding on top of `flacenc`, fed incrementally from an [`AudioSource`],
//! plus a VORBIS_COMMENT block spliced in after STREAMINFO for tags.

use crate::tags::Tags;
use crate::{AudioEncoder, AudioSource, PCM_DEPTH, SampleFormat, quantize, sample_format};
use flacenc::component::BitRepr;
use flacenc::error::{SourceError, SourceErrorReason, Verify};
use flacenc::source::{Fill, Source};
use std::collections::VecDeque;
use transformer_core::{Choice, ConvertCtx, Error, Format, Result, WriteSeek};

struct ReaderSource<'a> {
    reader: &'a mut dyn AudioSource,
    bits: u32,
    pending: VecDeque<f32>,
    eof: bool,
    /// Real error, since `SourceError` can't carry ours.
    failure: Option<Error>,
}

impl Source for ReaderSource<'_> {
    fn channels(&self) -> usize {
        self.reader.info().channels as usize
    }
    fn bits_per_sample(&self) -> usize {
        self.bits as usize
    }
    fn sample_rate(&self) -> usize {
        self.reader.info().sample_rate as usize
    }
    fn len_hint(&self) -> Option<usize> {
        self.reader.info().total_frames.map(|n| n as usize)
    }

    fn read_samples<F: Fill>(&mut self, block_size: usize, dest: &mut F) -> std::result::Result<usize, SourceError> {
        let ch = self.channels();
        let want = block_size * ch;
        let fail = |this: &mut Self, e: Error| {
            this.failure = Some(e);
            SourceError::by_reason(SourceErrorReason::IO(None))
        };
        while !self.eof && self.pending.len() < want {
            match self.reader.next_chunk() {
                Ok(Some(chunk)) => self.pending.extend(chunk.iter().copied()),
                Ok(None) => self.eof = true,
                Err(e) => return Err(fail(self, e)),
            }
        }
        let take = want.min(self.pending.len()) / ch * ch;
        let block: Vec<i32> = self.pending.drain(..take).map(|s| quantize(s, self.bits)).collect();
        dest.fill_interleaved(&block)?;
        Ok(take / ch)
    }
}

pub struct FlacEncoder;

impl AudioEncoder for FlacEncoder {
    fn format(&self) -> Format {
        Format::Flac
    }

    fn choices(&self) -> Vec<Choice> {
        vec![PCM_DEPTH]
    }

    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let SampleFormat::Int(bits) = sample_format(ctx.options.choice(Format::Flac, &PCM_DEPTH), src.info().source_bits)
        else {
            unreachable!("FLAC offers integer depths only")
        };
        encode(src, bits, tags, out)
    }
}

fn encode(reader: &mut dyn AudioSource, bits: u32, tags: &Tags, out: &mut dyn WriteSeek) -> Result<()> {
    let mut config = flacenc::config::Encoder::default();
    config.multithread = false; // files are already converted in parallel
    // flacenc's fixed predictors can pick pathological encodings for loud, noisy 24-bit
    // material (output 100x larger than the input). LPC covers the same ground, and on
    // real recordings dropping them costs <0.1% in size.
    config.subframe_coding.use_fixed = false;
    let config = config.into_verified().map_err(|e| Error::Codec(format!("flac config: {e:?}")))?;
    let block_size = config.block_size;

    let mut src = ReaderSource { reader, bits, pending: VecDeque::new(), eof: false, failure: None };
    let stream = flacenc::encode_with_fixed_block_size(&config, &mut src, block_size);
    if let Some(e) = src.failure.take() {
        return Err(e);
    }
    let stream = stream.map_err(|e| Error::Codec(format!("flac encode: {e}")))?;

    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).map_err(|e| Error::Codec(format!("flac write: {e:?}")))?;
    let mut bytes = sink.as_slice().to_vec();
    fix_streaminfo(&mut bytes);
    let bytes = splice_vorbis_comment(&bytes, tags);
    out.write_all(&bytes).map_err(|e| Error::Codec(format!("write failed: {e}")))
}

/// Repairs two STREAMINFO quirks of `flacenc`.
///
/// It lowers STREAMINFO's min block size to the (shorter) final frame's size.
/// Decoders treat min != max as a variable-block-size stream and then misread the
/// frame numbers as sample numbers, so restore min = max like the reference encoder.
fn fix_streaminfo(flac: &mut [u8]) {
    if flac.len() >= STREAMINFO_END && &flac[..4] == b"fLaC" {
        let (min, max) = flac[8..12].split_at_mut(2);
        min.copy_from_slice(max);

        // flacenc's min/max *frame size* (bytes) can come out inconsistent for some
        // multi-channel streams (min > max). 0 means "unknown", which decoders accept.
        let size = |b: &[u8]| u32::from_be_bytes([0, b[0], b[1], b[2]]);
        if size(&flac[12..15]) > size(&flac[15..18]) {
            flac[12..18].fill(0);
        }
    }
}

/// STREAMINFO is always the first block: "fLaC" + 4-byte header + 34-byte body.
const STREAMINFO_END: usize = 4 + 4 + 34;

fn splice_vorbis_comment(flac: &[u8], tags: &Tags) -> Vec<u8> {
    let fields = tags.vorbis_fields();
    if fields.is_empty() || flac.len() < STREAMINFO_END || &flac[..4] != b"fLaC" {
        return flac.to_vec();
    }
    let vendor = b"Transformer";
    let mut body = Vec::new();
    body.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    body.extend_from_slice(vendor);
    body.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    for (k, v) in &fields {
        let entry = format!("{k}={v}");
        body.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        body.extend_from_slice(entry.as_bytes());
    }
    if body.len() >= 1 << 24 {
        return flac.to_vec(); // block length is 24-bit; drop absurdly large tags
    }

    let mut out = Vec::with_capacity(flac.len() + body.len() + 4);
    out.extend_from_slice(&flac[..STREAMINFO_END]);
    out[4] &= 0x7F; // STREAMINFO is no longer the last metadata block
    out.push(0x80 | 4); // last-block flag + type 4 (VORBIS_COMMENT)
    out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&flac[STREAMINFO_END..]);
    out
}
