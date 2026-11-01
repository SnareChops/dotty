//! Stages the pipeline puts between a decoder and an encoder: progress/cancel metering,
//! channel remixing and sample-rate conversion. Each is itself an [`AudioSource`].

use crate::resample::StreamResampler;
use crate::{AudioSource, StreamInfo, Tags};
use dotty_core::{ConvertCtx, Result};

/// Reports progress as frames come out of the decoder and stops when cancelled.
pub(crate) struct Metered<'a, 'c, S> {
    inner: S,
    ctx: &'a ConvertCtx<'c>,
    /// Fraction of the progress bar that reading the whole input represents.
    share: f32,
    frames: u64,
}

impl<'a, 'c, S: AudioSource> Metered<'a, 'c, S> {
    pub fn new(inner: S, ctx: &'a ConvertCtx<'c>, share: f32) -> Self {
        Self { inner, ctx, share, frames: 0 }
    }
}

impl<S: AudioSource> AudioSource for Metered<'_, '_, S> {
    fn info(&self) -> &StreamInfo {
        self.inner.info()
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        self.ctx.check_cancelled()?;
        let channels = self.inner.info().channels as u64;
        let total = self.inner.info().total_frames.filter(|&t| t > 0);
        let Some(chunk) = self.inner.next_chunk()? else { return Ok(None) };
        self.frames += chunk.len() as u64 / channels;
        if let Some(total) = total {
            (self.ctx.progress)((self.share * self.frames as f32 / total as f32).min(0.99));
        }
        Ok(Some(chunk))
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Mixes any channel count to mono or stereo (mono becomes stereo by copying).
pub(crate) struct Remix<S> {
    inner: S,
    info: StreamInfo,
    src_channels: usize,
    buf: Vec<f32>,
}

impl<S: AudioSource> Remix<S> {
    /// `channels` is 1 or 2.
    pub fn new(inner: S, channels: u16) -> Self {
        let mut info = inner.info().clone();
        let src_channels = info.channels as usize;
        info.channels = channels;
        Self { inner, info, src_channels, buf: Vec::new() }
    }
}

impl<S: AudioSource> AudioSource for Remix<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let Some(chunk) = self.inner.next_chunk()? else { return Ok(None) };
        self.buf.clear();
        if self.info.channels == 1 {
            let n = self.src_channels;
            self.buf.extend(chunk.chunks_exact(n).map(|f| f.iter().sum::<f32>() / n as f32));
        } else if self.src_channels == 1 {
            self.buf.extend(chunk.iter().flat_map(|&s| [s, s]));
        } else {
            fold(chunk, self.src_channels, &mut self.buf);
        }
        Ok(Some(&self.buf))
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Fold any channel count to 1 or 2 (mono and stereo pass through).
pub(crate) fn fold(chunk: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        1 | 2 => out.extend_from_slice(chunk),
        6 => {
            // 5.1 (L R C LFE Ls Rs): ITU-R BS.775 downmix, LFE dropped, scaled to avoid clipping.
            const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
            let norm = 1.0 / (1.0 + 2.0 * K);
            for f in chunk.as_chunks::<6>().0 {
                out.push((f[0] + K * f[2] + K * f[4]) * norm);
                out.push((f[1] + K * f[2] + K * f[5]) * norm);
            }
        }
        n => {
            // Unknown layout: even channels to the left, odd to the right.
            for f in chunk.chunks_exact(n) {
                let (mut l, mut r) = (0.0, 0.0);
                for (i, s) in f.iter().enumerate() {
                    if i % 2 == 0 { l += s } else { r += s }
                }
                out.push(l / n.div_ceil(2) as f32);
                out.push(r / (n / 2) as f32);
            }
        }
    }
}

/// Streams the input through a [`StreamResampler`] to a new rate.
pub(crate) struct Resample<S> {
    inner: S,
    info: StreamInfo,
    resampler: StreamResampler,
    buf: Vec<f32>,
    finished: bool,
}

impl<S: AudioSource> Resample<S> {
    pub fn new(inner: S, rate: u32) -> Result<Self> {
        let mut info = inner.info().clone();
        let resampler = StreamResampler::new(info.sample_rate, rate, info.channels as usize)?;
        info.total_frames = info.total_frames.map(|t| (t as f64 * rate as f64 / info.sample_rate as f64).ceil() as u64);
        info.sample_rate = rate;
        Ok(Self { inner, info, resampler, buf: Vec::new(), finished: false })
    }
}

impl<S: AudioSource> AudioSource for Resample<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        // The resampler works in blocks, so several input chunks may produce nothing.
        while !self.finished {
            self.buf.clear();
            match self.inner.next_chunk()? {
                Some(chunk) => self.resampler.push(chunk, &mut self.buf)?,
                None => {
                    self.resampler.finish(&mut self.buf)?;
                    self.finished = true;
                }
            }
            if !self.buf.is_empty() {
                return Ok(Some(&self.buf));
            }
        }
        Ok(None)
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn folds_surround_to_stereo_without_clipping() {
        let mut out = Vec::new();
        fold(&[1.0, 1.0, 1.0, 0.0, 1.0, 1.0], 6, &mut out);
        assert!(out.iter().all(|&s| s <= 1.0 + 1e-6), "{out:?}");
        let mut mono = Vec::new();
        fold(&[0.5, -0.5], 1, &mut mono);
        assert_eq!(mono, [0.5, -0.5]);
    }
}
