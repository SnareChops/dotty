//! Streaming AIFF writer (16/24-bit big-endian PCM). Tags go in an `ID3 ` chunk, which
//! iTunes, foobar2000, Audacity and most taggers read.

use crate::tags::Tags;
use crate::{AudioEncoder, AudioSource, PCM_DEPTH, SampleFormat, id3, quantize, sample_format};
use std::io::SeekFrom;
use dotty_core::{Choice, ConvertCtx, Error, Format, Result, WriteSeek};

fn io(e: std::io::Error) -> Error {
    Error::Codec(format!("write failed: {e}"))
}

/// A sample rate as the 80-bit IEEE 754 extended float AIFF's COMM chunk uses.
fn extended(rate: u32) -> [u8; 10] {
    let mut out = [0u8; 10];
    if rate == 0 {
        return out;
    }
    let shift = 31 - rate.leading_zeros(); // position of the top bit
    let exponent = 16383 + shift as u16;
    let mantissa = (rate as u64) << (63 - shift);
    out[..2].copy_from_slice(&exponent.to_be_bytes());
    out[2..].copy_from_slice(&mantissa.to_be_bytes());
    out
}

pub struct AiffEncoder;

impl AudioEncoder for AiffEncoder {
    fn format(&self) -> Format {
        Format::Aiff
    }

    fn choices(&self) -> Vec<Choice> {
        vec![PCM_DEPTH]
    }

    fn encode(&self, src: &mut dyn AudioSource, tags: &Tags, ctx: &ConvertCtx, out: &mut dyn WriteSeek) -> Result<()> {
        let info = src.info().clone();
        let SampleFormat::Int(bits) = sample_format(ctx.options.choice(Format::Aiff, &PCM_DEPTH), info.source_bits) else {
            unreachable!("AIFF offers integer depths only")
        };
        let bytes = (bits / 8) as usize;

        let mut h = Vec::with_capacity(64);
        h.extend_from_slice(b"FORM\0\0\0\0AIFF");
        h.extend_from_slice(b"COMM");
        h.extend_from_slice(&18u32.to_be_bytes());
        h.extend_from_slice(&info.channels.to_be_bytes());
        let frames_pos = h.len() as u64;
        h.extend_from_slice(&0u32.to_be_bytes());
        h.extend_from_slice(&(bits as u16).to_be_bytes());
        h.extend_from_slice(&extended(info.sample_rate));
        let id3 = id3::id3v2(tags);
        if !id3.is_empty() {
            h.extend_from_slice(b"ID3 ");
            h.extend_from_slice(&(id3.len() as u32).to_be_bytes());
            h.extend_from_slice(&id3);
            if id3.len() % 2 == 1 {
                h.push(0);
            }
        }
        h.extend_from_slice(b"SSND");
        let ssnd_pos = h.len() as u64;
        h.extend_from_slice(&[0; 12]); // size, offset, block size
        out.write_all(&h).map_err(io)?;

        let mut data_bytes = 0u64;
        let mut buf = Vec::new();
        while let Some(chunk) = src.next_chunk()? {
            buf.clear();
            for &s in chunk {
                buf.extend_from_slice(&quantize(s, bits).to_be_bytes()[4 - bytes..]);
            }
            data_bytes += buf.len() as u64;
            out.write_all(&buf).map_err(io)?;
        }
        if data_bytes % 2 == 1 {
            out.write_all(&[0]).map_err(io)?;
        }
        let end = out.stream_position().map_err(io)?;
        if end > u32::MAX as u64 {
            return Err(Error::Codec("AIFF output would exceed 4 GiB".into()));
        }
        let frames = data_bytes / (bytes as u64 * info.channels as u64);
        for (pos, value) in [(4, end - 8), (frames_pos, frames), (ssnd_pos, data_bytes + 8)] {
            out.seek(SeekFrom::Start(pos)).map_err(io)?;
            out.write_all(&(value as u32).to_be_bytes()).map_err(io)?;
        }
        out.seek(SeekFrom::End(0)).map_err(io)?;
        Ok(())
    }
}

#[cfg(test)]
mod unit {
    #[test]
    fn sample_rates_encode_as_80_bit_floats() {
        // 44100 Hz is the classic example from the AIFF spec.
        assert_eq!(super::extended(44100), [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        assert_eq!(super::extended(48000), [0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]);
    }
}
