//! Audio edits as pipeline stages. The converter and the editor's preview build the same
//! stages, so what plays in the editor is what gets written.

use crate::stages::{Remix, Resample};
use crate::{AudioConverter, AudioSource, StreamInfo, Tags};
use dotty_core::{AudioEdits, Error, Format, Result};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Seconds of fade put on a cut point so it doesn't click.
pub const CUT_FADE: f64 = 0.005;
/// Frames handed out per chunk by [`MemorySource`].
const CHUNK: usize = 4096;

/// [`AudioEdits`] resolved to frames for a stream at one rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// First source frame kept.
    pub start: u64,
    /// One past the last source frame kept, if known.
    pub end: Option<u64>,
    /// Fade lengths in output frames, including the short ones on cut points.
    pub fade_in: u64,
    pub fade_out: u64,
}

impl Plan {
    /// `total` is the source length in frames, if known.
    pub fn new(edits: &AudioEdits, rate: u32, total: Option<u64>) -> Self {
        let (start, end) = edits.trim_frames(rate, total);
        let frames = |s: f64| (s.max(0.0) * rate as f64).round() as u64;
        let cut_fade = frames(CUT_FADE);
        let mut fade_in = frames(edits.fade_in);
        if start > 0 {
            fade_in = fade_in.max(cut_fade);
        }
        let mut fade_out = frames(edits.fade_out);
        if end.is_some_and(|e| total.is_none_or(|t| e < t)) {
            fade_out = fade_out.max(cut_fade);
        }
        Self { start, end, fade_in, fade_out }
    }

    /// Gain at output frame `i` of an output `len` frames long.
    pub fn gain(&self, i: u64, len: u64) -> f32 {
        let mut g = 1.0;
        if i < self.fade_in {
            g *= curve(i as f64 / self.fade_in as f64);
        }
        let from_end = len.saturating_sub(i + 1);
        if from_end < self.fade_out {
            g *= curve(from_end as f64 / self.fade_out as f64);
        }
        g
    }
}

/// Fade shape for `t` in 0..1: a raised cosine, so a fade starts and ends gently.
fn curve(t: f64) -> f32 {
    (0.5 - 0.5 * (std::f64::consts::PI * t.clamp(0.0, 1.0)).cos()) as f32
}

/// Applies `edits` to a stream starting at its first frame (the converter's case).
pub fn apply<'a>(src: Box<dyn AudioSource + 'a>, edits: &AudioEdits) -> Box<dyn AudioSource + 'a> {
    let info = src.info();
    let plan = Plan::new(edits, info.sample_rate, info.total_frames);
    let src = if plan.start > 0 || plan.end.is_some() { Box::new(Trim::new(src, plan.start, plan.end)) } else { src };
    if plan.fade_in > 0 || plan.fade_out > 0 { Box::new(Fade::new(src, &plan, 0)) } else { src }
}

/// Keeps source frames `start..end` and drops the rest.
pub struct Trim<S> {
    inner: S,
    info: StreamInfo,
    start: u64,
    end: Option<u64>,
    pos: u64,
    buf: Vec<f32>,
}

impl<S: AudioSource> Trim<S> {
    pub fn new(inner: S, start: u64, end: Option<u64>) -> Self {
        let mut info = inner.info().clone();
        let end = match (end, info.total_frames) {
            (Some(e), Some(t)) => Some(e.min(t)),
            (e, t) => e.or(t),
        };
        info.total_frames = end.map(|e| e.saturating_sub(start));
        Self { inner, info, start, end, pos: 0, buf: Vec::new() }
    }
}

impl<S: AudioSource> AudioSource for Trim<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let ch = self.info.channels as u64;
        loop {
            if self.end.is_some_and(|e| self.pos >= e) {
                return Ok(None);
            }
            let Some(chunk) = self.inner.next_chunk()? else { return Ok(None) };
            let n = chunk.len() as u64 / ch;
            let (from, to) = (self.pos, self.pos + n);
            self.pos = to;
            let lo = self.start.clamp(from, to) - from;
            let hi = self.end.map_or(to, |e| e.clamp(from, to)) - from;
            if hi > lo {
                self.buf.clear();
                self.buf.extend_from_slice(&chunk[(lo * ch) as usize..(hi * ch) as usize]);
                return Ok(Some(&self.buf));
            }
        }
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Fades the start and end in. The end needs no known length: the last `fade_out` frames
/// are held back until the stream ends, then faded and released.
pub struct Fade<S> {
    inner: S,
    plan: Plan,
    /// Output frames already produced before this stage started (preview from mid-way).
    pos: u64,
    held: Vec<f32>,
    out: Vec<f32>,
    done: bool,
}

impl<S: AudioSource> Fade<S> {
    /// `offset` is how many output frames come before the first one this sees.
    pub fn new(inner: S, plan: &Plan, offset: u64) -> Self {
        Self { inner, plan: *plan, pos: offset, held: Vec::new(), out: Vec::new(), done: false }
    }
}

impl<S: AudioSource> AudioSource for Fade<S> {
    fn info(&self) -> &StreamInfo {
        self.inner.info()
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let ch = self.inner.info().channels as usize;
        let keep = self.plan.fade_out as usize * ch;
        while !self.done {
            match self.inner.next_chunk()? {
                Some(chunk) => {
                    let first = self.held.len();
                    self.held.extend_from_slice(chunk);
                    // Fade in, by output position.
                    let at = self.pos + (first / ch) as u64;
                    for (i, frame) in (at..self.plan.fade_in).zip(self.held[first..].chunks_exact_mut(ch)) {
                        let g = curve(i as f64 / self.plan.fade_in as f64);
                        frame.iter_mut().for_each(|s| *s *= g);
                    }
                    if self.held.len() > keep {
                        let release = self.held.len() - keep;
                        self.out.clear();
                        self.out.extend(self.held.drain(..release));
                        self.pos += (release / ch) as u64;
                        return Ok(Some(&self.out));
                    }
                }
                None => {
                    self.done = true;
                    let n = self.held.len() / ch;
                    for (k, frame) in self.held.chunks_exact_mut(ch).enumerate() {
                        let from_end = (n - 1 - k) as u64;
                        if from_end < self.plan.fade_out {
                            let g = curve(from_end as f64 / self.plan.fade_out as f64);
                            frame.iter_mut().for_each(|s| *s *= g);
                        }
                    }
                    self.out = std::mem::take(&mut self.held);
                    if !self.out.is_empty() {
                        return Ok(Some(&self.out));
                    }
                }
            }
        }
        Ok(None)
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Interleaved samples held in memory, read from frame `start` to `end`.
pub struct MemorySource {
    samples: Arc<[f32]>,
    info: StreamInfo,
    tags: Tags,
    pos: usize,
    end: usize,
}

impl MemorySource {
    pub fn new(samples: Arc<[f32]>, info: &StreamInfo, start: u64, end: Option<u64>) -> Self {
        let ch = info.channels.max(1) as usize;
        let len = samples.len() / ch;
        let end = end.map_or(len, |e| (e as usize).min(len));
        let pos = (start as usize).min(end);
        let mut info = info.clone();
        info.total_frames = Some((end - pos) as u64);
        Self { samples, info, tags: Tags::default(), pos, end }
    }
}

impl AudioSource for MemorySource {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        if self.pos >= self.end {
            return Ok(None);
        }
        let ch = self.info.channels as usize;
        let to = (self.pos + CHUNK).min(self.end);
        let chunk = &self.samples[self.pos * ch..to * ch];
        self.pos = to;
        Ok(Some(chunk))
    }
}

/// The editor's preview: `samples` with `edits` applied, starting at source frame `from`
/// (clamped into the kept range), adapted to a device's channel count (1 or 2) and rate.
pub fn preview<'a>(
    samples: Arc<[f32]>,
    info: &StreamInfo,
    edits: &AudioEdits,
    from: u64,
    channels: u16,
    rate: u32,
) -> Result<Box<dyn AudioSource + 'a>> {
    let total = (samples.len() / info.channels.max(1) as usize) as u64;
    let plan = Plan::new(edits, info.sample_rate, Some(total));
    let end = plan.end.unwrap_or(total);
    let from = from.clamp(plan.start, end.saturating_sub(1));
    let mut chain: Box<dyn AudioSource> = Box::new(MemorySource::new(samples, info, from, Some(end)));
    if plan.fade_in > 0 || plan.fade_out > 0 {
        chain = Box::new(Fade::new(chain, &plan, from - plan.start));
    }
    if channels != info.channels {
        chain = Box::new(Remix::new(chain, channels.clamp(1, 2)));
    }
    if rate != info.sample_rate {
        chain = Box::new(Resample::new(chain, rate)?);
    }
    Ok(chain)
}

/// Decode a whole file into memory for the editor. Fails if it's longer than `max_secs`.
pub fn decode_all(path: &Path, from: Format, max_secs: u64, cancel: &AtomicBool) -> Result<(Arc<[f32]>, StreamInfo)> {
    let mut src = AudioConverter::default().open(path, from)?;
    let info = src.info().clone();
    let limit = max_secs * info.sample_rate as u64 * info.channels as u64;
    let too_long = || Error::Codec(format!("longer than {} minutes, too long to edit", max_secs / 60));
    if info.total_frames.is_some_and(|t| t * info.channels as u64 > limit) {
        return Err(too_long());
    }
    let mut out = Vec::with_capacity(info.total_frames.map_or(0, |t| (t * info.channels as u64) as usize));
    while let Some(chunk) = src.next_chunk()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        out.extend_from_slice(chunk);
        if out.len() as u64 > limit {
            return Err(too_long());
        }
    }
    let mut info = info;
    info.total_frames = Some((out.len() / info.channels.max(1) as usize) as u64);
    Ok((out.into(), info))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(channels: u16, frames: u64) -> StreamInfo {
        StreamInfo { sample_rate: 1000, channels, source_bits: None, total_frames: Some(frames), source_rate: 1000 }
    }

    fn drain(mut s: impl AudioSource) -> Vec<f32> {
        let mut v = Vec::new();
        while let Some(c) = s.next_chunk().unwrap() {
            v.extend_from_slice(c);
        }
        v
    }

    /// Ramp 0, 1, 2, … per frame, duplicated across channels.
    fn ramp(channels: u16, frames: usize) -> Arc<[f32]> {
        (0..frames).flat_map(|i| std::iter::repeat_n(i as f32, channels as usize)).collect()
    }

    #[test]
    fn trims_across_chunk_boundaries() {
        let samples = ramp(2, 10_000);
        let src = MemorySource::new(samples, &info(2, 10_000), 0, None);
        let out = drain(Trim::new(src, 4000, Some(9000)));
        assert_eq!(out.len(), 5000 * 2);
        assert_eq!((out[0], out[out.len() - 1]), (4000.0, 8999.0));
    }

    #[test]
    fn fades_reach_silence_at_both_ends_without_a_known_length() {
        let samples: Arc<[f32]> = vec![1.0; 9000].into();
        let mut i = info(1, 9000);
        i.total_frames = None;
        let plan = Plan { start: 0, end: None, fade_in: 1000, fade_out: 2000 };
        let out = drain(Fade::new(MemorySource::new(samples, &i, 0, None), &plan, 0));
        assert_eq!(out.len(), 9000);
        assert!(out[0] < 1e-3 && out[8999] < 1e-3);
        assert_eq!(out[4500], 1.0);
        assert!((out[500] - plan.gain(500, 9000)).abs() < 1e-6);
        assert!((out[8000] - plan.gain(8000, 9000)).abs() < 1e-6);
        assert!(out[6000..].windows(2).all(|w| w[1] <= w[0]));
    }

    #[test]
    fn cut_points_get_a_short_fade_but_file_ends_do_not() {
        let e = AudioEdits { trim_start: 1.0, trim_end: Some(2.0), ..Default::default() };
        let p = Plan::new(&e, 1000, Some(5000));
        assert_eq!((p.start, p.end, p.fade_in, p.fade_out), (1000, Some(2000), 5, 5));
        let whole = Plan::new(&AudioEdits { trim_end: Some(9.0), ..Default::default() }, 1000, Some(5000));
        assert_eq!((whole.end, whole.fade_in, whole.fade_out), (Some(5000), 0, 0));
    }

    #[test]
    fn preview_from_mid_way_matches_the_full_render() {
        let samples: Arc<[f32]> = vec![0.5; 4000].into();
        let i = info(1, 4000);
        let e = AudioEdits { trim_start: 0.5, fade_in: 1.0, ..Default::default() };
        let full = drain(apply(Box::new(MemorySource::new(samples.clone(), &i, 0, None)), &e));
        let part = drain(preview(samples, &i, &e, 1000, 1, 1000).unwrap());
        assert_eq!(full.len(), 3500);
        assert_eq!(&full[500..], &part[..]);
    }
}
