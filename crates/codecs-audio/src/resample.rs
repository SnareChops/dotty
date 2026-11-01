//! Streaming sample-rate conversion on top of rubato's FFT resampler.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use transformer_core::{Error, Result};

fn err(e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("resampler: {e}"))
}

pub struct StreamResampler {
    fft: Fft<f32>,
    ch: usize,
    ratio: f64,
    /// Input not yet consumed (interleaved).
    pending: Vec<f32>,
    out_buf: Vec<f32>,
    /// Frames of start-up silence still to drop from the output.
    to_trim: usize,
    frames_in: u64,
    frames_out: u64,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32, ch: usize) -> Result<Self> {
        let fft = Fft::<f32>::new(from as usize, to as usize, 1024, ch, FixedSync::Input).map_err(err)?;
        let out_buf = vec![0.0; fft.output_frames_max() * ch];
        let to_trim = fft.output_delay();
        Ok(Self { fft, ch, ratio: to as f64 / from as f64, pending: Vec::new(), out_buf, to_trim, frames_in: 0, frames_out: 0 })
    }

    /// Feed interleaved input; resampled output is appended to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.frames_in += (input.len() / self.ch) as u64;
        self.pending.extend_from_slice(input);
        loop {
            let need = self.fft.input_frames_next();
            if self.pending.len() / self.ch < need {
                return Ok(());
            }
            self.run(None, out)?;
        }
    }

    /// Flush the tail; afterwards the total output length is exactly `ceil(ratio * input)`.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let expected = (self.ratio * self.frames_in as f64).ceil() as u64;
        let left = self.pending.len() / self.ch;
        if left > 0 {
            self.run(Some(left), out)?;
        }
        // Pump zeros until the delayed tail has come out.
        while self.frames_out < expected {
            self.run(Some(0), out)?;
        }
        let excess = (self.frames_out - expected) as usize;
        out.truncate(out.len() - excess * self.ch);
        self.frames_out = expected;
        Ok(())
    }

    fn run(&mut self, partial: Option<usize>, out: &mut Vec<f32>) -> Result<()> {
        let have = self.pending.len() / self.ch;
        let input = InterleavedSlice::new(&self.pending, self.ch, have).map_err(err)?;
        let cap = self.out_buf.len() / self.ch;
        let mut output = InterleavedSlice::new_mut(&mut self.out_buf, self.ch, cap).map_err(err)?;
        let indexing = Indexing { input_offset: 0, output_offset: 0, active_channels_mask: None, partial_len: partial };
        let (nin, nout) = self.fft.process_into_buffer(&input, &mut output, Some(&indexing)).map_err(err)?;
        self.pending.drain(..nin.min(have) * self.ch);
        let skip = self.to_trim.min(nout);
        self.to_trim -= skip;
        out.extend_from_slice(&self.out_buf[skip * self.ch..nout * self.ch]);
        self.frames_out += (nout - skip) as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * 0.5).collect()
    }

    #[test]
    fn length_and_frequency_are_preserved_across_odd_chunking() {
        for (from, to) in [(44100u32, 48000u32), (96000, 48000), (22050, 24000)] {
            let input = sine(1000.0, from, from as usize * 2 + 37);
            let mut rs = StreamResampler::new(from, to, 1).unwrap();
            let mut out = Vec::new();
            for chunk in input.chunks(777) {
                rs.push(chunk, &mut out).unwrap();
            }
            rs.finish(&mut out).unwrap();
            let expected = (input.len() as f64 * to as f64 / from as f64).ceil() as usize;
            assert_eq!(out.len(), expected, "{from}->{to}");

            // Compare against an ideal sine at the new rate away from the edges.
            let ideal = sine(1000.0, to, out.len());
            let err: f32 = out[2000..out.len() - 2000].iter().zip(&ideal[2000..]).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(err < 0.01, "{from}->{to}: max error {err}");
        }
    }

    #[test]
    fn stereo_channels_stay_separate() {
        let mut inter = Vec::new();
        for i in 0..20000 {
            inter.push((i as f32 * 0.05).sin() * 0.5);
            inter.push(0.0);
        }
        let mut rs = StreamResampler::new(44100, 48000, 2).unwrap();
        let mut out = Vec::new();
        rs.push(&inter, &mut out).unwrap();
        rs.finish(&mut out).unwrap();
        assert!(out.iter().skip(1).step_by(2).all(|s| s.abs() < 1e-4));
    }
}
