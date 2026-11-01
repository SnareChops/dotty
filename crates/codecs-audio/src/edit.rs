//! Audio edits as pipeline stages. The converter and the editor's preview build the same
//! stages, so what plays in the editor is what gets written.
//!
//! Order: trim → reverse → speed/pitch → channels → filters → fades → echo → reverb → volume (with a limiter). Fades
//! come before the effects so a fade-out lets echoes and reverb ring out naturally (and the
//! short fades on cut points keep clicks out of them). Normalizing needs the level of the
//! audio as it reaches the volume stage, so callers measure first ([`shape`] + [`measure`])
//! and pass the [`Measurement`] in.

use crate::stages::{Remix, Resample};
use crate::{AudioConverter, AudioSource, StreamInfo, Tags};
use dotty_core::{AudioEdits, Channels, Echo, Error, Format, Result, Reverb, Volume};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Seconds of fade put on a cut point so it doesn't click.
pub const CUT_FADE: f64 = 0.005;
/// Frames handed out per chunk by the in-memory sources.
const CHUNK: usize = 4096;
/// The limiter keeps peaks at or below this (−0.1 dBFS).
pub const CEILING: f32 = 0.988_553;
/// How far ahead the limiter looks, and how fast it lets go, in seconds.
const LOOKAHEAD: f64 = 0.005;
const RELEASE: f64 = 0.1;
/// Most a volume change may boost or cut, in dB (normalizing near-silence stays sane).
const MAX_BOOST: f32 = 40.0;
const MAX_CUT: f32 = -60.0;

/// [`AudioEdits`] resolved to frames for a stream at one rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// First source frame kept.
    pub start: u64,
    /// One past the last source frame kept, if known.
    pub end: Option<u64>,
    pub reverse: bool,
    /// Source frames per output frame.
    pub speed: f64,
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
        let cut_start = start > 0;
        let cut_end = end.is_some_and(|e| total.is_none_or(|t| e < t));
        // Reversed, the source's end is heard first.
        let (cut_first, cut_last) = if edits.reverse { (cut_end, cut_start) } else { (cut_start, cut_end) };
        let mut fade_in = frames(edits.fade_in);
        if cut_first {
            fade_in = fade_in.max(cut_fade);
        }
        let mut fade_out = frames(edits.fade_out);
        if cut_last {
            fade_out = fade_out.max(cut_fade);
        }
        let speed = edits.speed.clamp(AudioEdits::MIN_SPEED, AudioEdits::MAX_SPEED);
        Self { start, end, reverse: edits.reverse, speed, fade_in, fade_out }
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

    /// Where kept source frame `frame` lands in the output, when the kept part ends at `end`.
    pub fn output_index(&self, frame: u64, end: u64) -> u64 {
        let from_start = if self.reverse { end.saturating_sub(frame + 1) } else { frame.saturating_sub(self.start) };
        (from_start as f64 / self.speed) as u64
    }

    /// The source frame heard at output frame `i` (the inverse of [`Plan::output_index`]).
    pub fn source_frame(&self, i: u64, end: u64) -> u64 {
        let from_start = (i as f64 * self.speed) as u64;
        if self.reverse { end.saturating_sub(from_start + 1).max(self.start) } else { (self.start + from_start).min(end.saturating_sub(1)) }
    }

    /// Output length of `kept` source frames, before any effect tail.
    pub fn output_len(&self, kept: u64) -> u64 {
        ((kept as f64 / self.speed).round() as u64).max(1)
    }
}

/// Fade shape for `t` in 0..1: a raised cosine, so a fade starts and ends gently.
fn curve(t: f64) -> f32 {
    (0.5 - 0.5 * (std::f64::consts::PI * t.clamp(0.0, 1.0)).cos()) as f32
}

/// How loud audio is, measured where the volume stage will see it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Highest absolute sample value.
    pub peak: f32,
    /// Integrated loudness in LUFS; −∞ for silence.
    pub loudness: f64,
}

/// The linear gain `volume` asks for, given the measured audio (needed for normalizing).
pub fn volume_gain(volume: Volume, measured: Option<&Measurement>) -> f32 {
    let db = match volume {
        Volume::Gain(db) => db,
        Volume::Peak(target) => measured.filter(|m| m.peak > 0.0).map_or(0.0, |m| target - 20.0 * m.peak.log10()),
        Volume::Loudness(target) => measured.filter(|m| m.loudness.is_finite()).map_or(0.0, |m| target - m.loudness as f32),
    };
    10f32.powf(db.clamp(MAX_CUT, MAX_BOOST) / 20.0)
}

/// Whether the limiter is armed for `gain`. Normalizing has a measurement, so it's armed only
/// when the peak would pass [`CEILING`]. A plain gain change isn't measured (the converter
/// reads the file once), so it's armed for any boost and for edits that can raise peaks.
/// Armed or not, it changes nothing below the ceiling; the decision only depends on what both
/// the converter and the editor know, so they always agree.
pub fn limits(edits: &AudioEdits, source_channels: u16, gain: f32, measured: Option<&Measurement>) -> bool {
    if edits.volume.measures() {
        measured.is_none_or(|m| m.peak * gain > CEILING)
    } else {
        gain > 1.0 || raises_peaks(edits, source_channels)
    }
}

/// Edits that can push peaks above where the source had them: effects add sound, filters
/// overshoot on sharp edges, resampling and overlap-adding ring, widening adds a copy.
fn raises_peaks(edits: &AudioEdits, source_channels: u16) -> bool {
    edits.echo.is_some()
        || edits.reverb.is_some()
        || edits.low_cut.is_some()
        || edits.high_cut.is_some()
        || edits.speed != 1.0
        || edits.pitch != 0.0
        || matches!(edits.channels, Channels::Stereo { width } if width > 0.0 && source_channels == 1)
}

/// Channels after the editor's channel setting, for a source with `source` channels.
pub fn edited_channels(edits: &AudioEdits, source: u16) -> u16 {
    match edits.channels {
        Channels::Keep => source,
        Channels::Mono => 1,
        Channels::Stereo { .. } => 2,
    }
}

/// Every stage before the volume: trim, reverse, filters, fades, effects. Measure this to
/// normalize.
pub fn shape<'a>(src: Box<dyn AudioSource + 'a>, edits: &AudioEdits) -> Result<Box<dyn AudioSource + 'a>> {
    let plan = stream_plan(edits, src.info().sample_rate);
    let mut src = src;
    if plan.start > 0 || plan.end.is_some() {
        src = Box::new(Trim::new(src, plan.start, plan.end));
    }
    if edits.reverse {
        src = Box::new(Reverse::new(src));
    }
    after_pick(src, edits, &plan, 0)
}

/// The stages after the kept part is picked (and reversed): filters, fades (for output
/// starting `offset` frames in), effects.
fn after_pick<'a>(src: Box<dyn AudioSource + 'a>, edits: &AudioEdits, plan: &Plan, offset: u64) -> Result<Box<dyn AudioSource + 'a>> {
    let mut src = src;
    let speed = speed_plan(edits, src.info().sample_rate);
    if speed.stretch != 1.0 {
        src = Box::new(Stretch::new(src, speed.stretch));
    }
    if speed.to != src.info().sample_rate {
        src = Box::new(Repitch::new(src, speed.to)?);
    }
    src = with_channels(src, edits);
    let biquads = filters(edits, src.info().sample_rate);
    if !biquads.is_empty() {
        src = Box::new(Filters::new(src, biquads));
    }
    if plan.fade_in > 0 || plan.fade_out > 0 {
        src = Box::new(Fade::new(src, plan, offset));
    }
    if edits.echo.is_some() || edits.reverb.is_some() {
        src = Box::new(Effects::new(src, edits));
    }
    Ok(src)
}

/// The editor's channel setting as a stage (nothing if it changes nothing).
pub fn with_channels<'a>(src: Box<dyn AudioSource + 'a>, edits: &AudioEdits) -> Box<dyn AudioSource + 'a> {
    let channels = src.info().channels;
    match edits.channels {
        Channels::Mono if channels != 1 => Box::new(Remix::new(src, 1)),
        Channels::Stereo { width } if channels == 1 && width > 0.0 => Box::new(Widen::new(src, width)),
        Channels::Stereo { .. } if channels != 2 => Box::new(Remix::new(src, 2)),
        _ => src,
    }
}

/// The volume stage.
fn with_volume<'a>(
    src: Box<dyn AudioSource + 'a>,
    edits: &AudioEdits,
    source_channels: u16,
    measured: Option<&Measurement>,
) -> Box<dyn AudioSource + 'a> {
    let gain = volume_gain(edits.volume, measured);
    let limit = limits(edits, source_channels, gain, measured);
    if gain != 1.0 || limit { Box::new(Gain::new(src, gain, limit)) } else { src }
}

/// [`shape`], then mixed to the output's `channels`. Loudness depends on the layout (the
/// same sound on two speakers measures 3 LU louder than on one), so normalizing measures
/// this: the layout that gets written.
pub fn shape_to<'a>(src: Box<dyn AudioSource + 'a>, edits: &AudioEdits, channels: u16) -> Result<Box<dyn AudioSource + 'a>> {
    let src = shape(src, edits)?;
    Ok(if src.info().channels != channels { Box::new(Remix::new(src, channels.clamp(1, 2))) } else { src })
}

/// Applies `edits` to a stream starting at its first frame (the converter's case), writing
/// `channels` channels. `measured` is [`measure`] of [`shape_to`] of the same stream, needed
/// when normalizing.
pub fn apply_to<'a>(
    src: Box<dyn AudioSource + 'a>,
    edits: &AudioEdits,
    measured: Option<&Measurement>,
    channels: u16,
) -> Result<Box<dyn AudioSource + 'a>> {
    let source = src.info().channels;
    Ok(with_volume(shape_to(src, edits, channels)?, edits, source, measured))
}

/// [`apply_to`] keeping the layout the edits give.
pub fn apply<'a>(src: Box<dyn AudioSource + 'a>, edits: &AudioEdits, measured: Option<&Measurement>) -> Result<Box<dyn AudioSource + 'a>> {
    let channels = edited_channels(edits, src.info().channels);
    apply_to(src, edits, measured, channels)
}

/// Reads `src` to the end and measures it. Stops early if `cancel` is set.
pub fn measure(mut src: Box<dyn AudioSource + '_>, cancel: Option<&AtomicBool>) -> Result<Measurement> {
    let info = src.info().clone();
    let mode = ebur128::Mode::I | ebur128::Mode::HISTOGRAM;
    let mut meter = ebur128::EbuR128::new(info.channels as u32, info.sample_rate, mode).ok();
    let mut peak = 0.0f32;
    while let Some(chunk) = src.next_chunk()? {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        peak = chunk.iter().fold(peak, |p, s| p.max(s.abs()));
        if let Some(m) = &mut meter
            && m.add_frames_f32(chunk).is_err()
        {
            meter = None;
        }
    }
    let loudness = meter.and_then(|m| m.loudness_global().ok()).filter(|l| l.is_finite()).unwrap_or(f64::NEG_INFINITY);
    Ok(Measurement { peak, loudness })
}

/// The plan for a stream read from a decoder. Its length isn't passed on: for some formats
/// (MP3 without a Xing header, for one) it's only an estimate, and trimming to an estimate
/// that's short would cut off the end. Only an explicit trim end stops the stream early.
pub fn stream_plan(edits: &AudioEdits, rate: u32) -> Plan {
    Plan::new(edits, rate, None)
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
    /// Stops at `end` if given, otherwise at the end of the stream (whatever its declared length).
    pub fn new(inner: S, start: u64, end: Option<u64>) -> Self {
        let mut info = inner.info().clone();
        // Only a length hint from here on.
        info.total_frames = end.or(info.total_frames).map(|e| e.saturating_sub(start));
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

/// Plays the stream backwards. It has to read all of it first; the editor only takes files
/// it can hold in memory, so edited streams are bounded.
pub struct Reverse<'a> {
    inner: Box<dyn AudioSource + 'a>,
    read: Option<ReversedMemory>,
}

impl<'a> Reverse<'a> {
    pub fn new(inner: Box<dyn AudioSource + 'a>) -> Self {
        Self { inner, read: None }
    }
}

impl AudioSource for Reverse<'_> {
    fn info(&self) -> &StreamInfo {
        self.inner.info()
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        if self.read.is_none() {
            let mut all = Vec::new();
            while let Some(chunk) = self.inner.next_chunk()? {
                all.extend_from_slice(chunk);
            }
            let frames = (all.len() / self.inner.info().channels.max(1) as usize) as u64;
            self.read = Some(ReversedMemory::new(all.into(), self.inner.info(), 0, frames));
        }
        self.read.as_mut().map_or(Ok(None), |r| r.next_chunk())
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Interleaved samples held in memory, read backwards from frame `hi - 1` down to `lo`.
pub struct ReversedMemory {
    samples: Arc<[f32]>,
    info: StreamInfo,
    tags: Tags,
    lo: usize,
    hi: usize,
    out: Vec<f32>,
}

impl ReversedMemory {
    pub fn new(samples: Arc<[f32]>, info: &StreamInfo, lo: u64, hi: u64) -> Self {
        let ch = info.channels.max(1) as usize;
        let hi = (hi as usize).min(samples.len() / ch);
        let lo = (lo as usize).min(hi);
        let mut info = info.clone();
        info.total_frames = Some((hi - lo) as u64);
        Self { samples, info, tags: Tags::default(), lo, hi, out: Vec::new() }
    }
}

impl AudioSource for ReversedMemory {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        &self.tags
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        if self.hi <= self.lo {
            return Ok(None);
        }
        let ch = self.info.channels as usize;
        let from = self.hi.saturating_sub(CHUNK).max(self.lo);
        self.out.clear();
        for f in self.samples[from * ch..self.hi * ch].chunks_exact(ch).rev() {
            self.out.extend_from_slice(f);
        }
        self.hi = from;
        Ok(Some(&self.out))
    }
}

/// One second-order filter section (RBJ cookbook, Butterworth Q: 12 dB/octave).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    /// One sample through the filter (transposed direct form II); `z` is its state.
    fn process(&self, x: f64, z: &mut [f64; 2]) -> f64 {
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }

    fn new(freq: f32, rate: u32, high_pass: bool) -> Self {
        let w0 = 2.0 * std::f64::consts::PI * freq as f64 / rate as f64;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high_pass { ((1.0 + cos) / 2.0, -(1.0 + cos)) } else { ((1.0 - cos) / 2.0, 1.0 - cos) };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b0 / a0, a1: -2.0 * cos / a0, a2: (1.0 - alpha) / a0 }
    }
}

/// The filter sections `edits` ask for at `rate`; cutoffs the rate can't carry are skipped.
fn filters(edits: &AudioEdits, rate: u32) -> Vec<Biquad> {
    let nyquist = rate as f32 / 2.0;
    let mut out = Vec::new();
    if let Some(f) = edits.low_cut.filter(|&f| f > 0.0 && f < nyquist * 0.9) {
        out.push(Biquad::new(f, rate, true));
    }
    if let Some(f) = edits.high_cut.filter(|&f| f > 0.0 && f < nyquist * 0.95) {
        out.push(Biquad::new(f, rate, false));
    }
    out
}

/// Runs every channel through the same chain of [`Biquad`]s.
struct Filters<S> {
    inner: S,
    biquads: Vec<Biquad>,
    /// Two state values per biquad per channel.
    state: Vec<[f64; 2]>,
    buf: Vec<f32>,
}

impl<S: AudioSource> Filters<S> {
    fn new(inner: S, biquads: Vec<Biquad>) -> Self {
        let state = vec![[0.0; 2]; biquads.len() * inner.info().channels as usize];
        Self { inner, biquads, state, buf: Vec::new() }
    }
}

impl<S: AudioSource> AudioSource for Filters<S> {
    fn info(&self) -> &StreamInfo {
        self.inner.info()
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let ch = self.inner.info().channels as usize;
        let Some(chunk) = self.inner.next_chunk()? else { return Ok(None) };
        self.buf.clear();
        self.buf.extend_from_slice(chunk);
        for frame in self.buf.chunks_exact_mut(ch) {
            for (c, s) in frame.iter_mut().enumerate() {
                let mut x = *s as f64;
                for (k, q) in self.biquads.iter().enumerate() {
                    x = q.process(x, &mut self.state[k * ch + c]);
                }
                *s = x as f32;
            }
        }
        Ok(Some(&self.buf))
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Multiplies by a gain, with an optional lookahead limiter that keeps peaks under
/// [`CEILING`]: the gain needed for each frame is held over the lookahead window and
/// averaged over it, so reduction ramps in smoothly and is complete by the time the peak
/// arrives; it then recovers with a [`RELEASE`] time constant.
struct Gain<S> {
    inner: S,
    gain: f32,
    limiter: Option<Limiter>,
    /// One frame after the gain, on its way into the limiter.
    frame: Vec<f32>,
    out: Vec<f32>,
    done: bool,
}

impl<S: AudioSource> Gain<S> {
    fn new(inner: S, gain: f32, limit: bool) -> Self {
        let info = inner.info();
        let limiter = limit.then(|| Limiter::new(info.channels as usize, info.sample_rate));
        let frame = vec![0.0; info.channels.max(1) as usize];
        Self { inner, gain, limiter, frame, out: Vec::new(), done: false }
    }
}

impl<S: AudioSource> AudioSource for Gain<S> {
    fn info(&self) -> &StreamInfo {
        self.inner.info()
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let ch = self.inner.info().channels as usize;
        while !self.done {
            self.out.clear();
            match self.inner.next_chunk()? {
                Some(chunk) => match &mut self.limiter {
                    None => self.out.extend(chunk.iter().map(|s| s * self.gain)),
                    Some(l) => {
                        for f in chunk.chunks_exact(ch) {
                            self.frame.iter_mut().zip(f).for_each(|(o, s)| *o = s * self.gain);
                            l.push(Some(&self.frame), &mut self.out);
                        }
                    }
                },
                None => {
                    self.done = true;
                    if let Some(l) = &mut self.limiter {
                        l.flush(&mut self.out);
                    }
                }
            }
            if !self.out.is_empty() {
                return Ok(Some(&self.out));
            }
        }
        Ok(None)
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

struct Limiter {
    channels: usize,
    look: usize,
    release: f32,
    /// Frames waiting for the lookahead to fill, interleaved.
    delay: VecDeque<f32>,
    /// `(frame, gain needed)`, increasing gains: the window's minimum is at the front.
    needed: VecDeque<(u64, f32)>,
    /// The last `look` held gains and their sum, for the moving average.
    held: VecDeque<f32>,
    held_sum: f64,
    frames: u64,
    env: f32,
}

impl Limiter {
    fn new(channels: usize, rate: u32) -> Self {
        let look = ((LOOKAHEAD * rate as f64) as usize).max(1);
        let release = 1.0 - (-1.0 / (RELEASE * rate as f64)).exp() as f32;
        Self {
            channels,
            look,
            release,
            delay: VecDeque::new(),
            needed: VecDeque::new(),
            held: std::iter::repeat_n(1.0, look).collect(),
            held_sum: look as f64,
            frames: 0,
            env: 1.0,
        }
    }

    /// Take in one frame (`None` past the end) and give out the frame `look` behind it.
    fn push(&mut self, frame: Option<&[f32]>, out: &mut Vec<f32>) {
        let n = self.frames;
        self.frames += 1;
        let need = frame.map_or(1.0, |f| {
            let peak = f.iter().fold(0.0f32, |p, s| p.max(s.abs()));
            if peak > CEILING { CEILING / peak } else { 1.0 }
        });
        while self.needed.back().is_some_and(|b| b.1 >= need) {
            self.needed.pop_back();
        }
        self.needed.push_back((n, need));
        if let Some(f) = frame {
            self.delay.extend(f);
        }
        let Some(k) = (n + 1).checked_sub(self.look as u64) else { return };
        while self.needed.front().is_some_and(|f| f.0 < k) {
            self.needed.pop_front();
        }
        let hold = self.needed.front().map_or(1.0, |f| f.1);
        self.held.push_back(hold);
        self.held_sum += hold as f64 - self.held.pop_front().unwrap_or(1.0) as f64;
        let target = (self.held_sum / self.look as f64) as f32;
        self.env = if target < self.env { target } else { self.env + (target - self.env) * self.release };
        if self.delay.len() >= self.channels {
            for s in self.delay.drain(..self.channels) {
                out.push((s * self.env).clamp(-CEILING, CEILING));
            }
        }
    }

    fn flush(&mut self, out: &mut Vec<f32>) {
        while !self.delay.is_empty() {
            self.push(None, out);
        }
    }
}

/// The editor's preview: `samples` with `edits` applied, heard from source frame `from`
/// (clamped into the kept range; when reversed it plays from there towards the start),
/// adapted to a device's channel count (1 or 2) and rate.
pub fn preview<'a>(
    samples: Arc<[f32]>,
    info: &StreamInfo,
    edits: &AudioEdits,
    measured: Option<&Measurement>,
    from: u64,
    channels: u16,
    rate: u32,
) -> Result<Box<dyn AudioSource + 'a>> {
    let total = (samples.len() / info.channels.max(1) as usize) as u64;
    let plan = Plan::new(edits, info.sample_rate, Some(total));
    let end = plan.end.unwrap_or(total);
    let from = from.clamp(plan.start, end.saturating_sub(1));
    // Start a little earlier and throw that away, so filters, the limiter and effect tails
    // are where the full render has them (starting cold, the first moments would differ).
    let preroll = (preroll_secs(edits, info.sample_rate) * plan.speed * info.sample_rate as f64) as u64;
    let begin = if plan.reverse { (from + preroll).min(end - 1) } else { from.saturating_sub(preroll).max(plan.start) };
    let src: Box<dyn AudioSource> = if plan.reverse {
        Box::new(ReversedMemory::new(samples, info, plan.start, begin + 1))
    } else {
        Box::new(MemorySource::new(samples, info, begin, Some(end)))
    };
    let offset = plan.output_index(begin, end);
    let chain = with_volume(after_pick(src, edits, &plan, offset)?, edits, info.channels, measured);
    let mut chain: Box<dyn AudioSource> = Box::new(Skip::new(chain, plan.output_index(from, end) - offset));
    if channels != chain.info().channels {
        chain = Box::new(Remix::new(chain, channels.clamp(1, 2)));
    }
    if rate != info.sample_rate {
        chain = Box::new(Resample::new(chain, rate)?);
    }
    Ok(chain)
}

/// How much audio (in output seconds) the preview runs before the playhead so that every
/// stage with a memory has settled: the limiter's release, the stretcher's segments, filter
/// ringing, and the echo and reverb tails (capped, so a near-endless echo doesn't delay
/// playback much).
fn preroll_secs(edits: &AudioEdits, rate: u32) -> f64 {
    let mut secs = 0.5;
    if let Some(e) = &edits.echo {
        secs += EchoFx::new(e, 1, rate).tail() as f64 / rate as f64;
    }
    if let Some(r) = &edits.reverb {
        secs += ReverbFx::new(r, 1, rate).tail() as f64 / rate as f64;
    }
    secs.min(10.0)
}

/// Drops the first `frames` frames.
struct Skip<'a> {
    inner: Box<dyn AudioSource + 'a>,
    frames: u64,
    buf: Vec<f32>,
}

impl<'a> Skip<'a> {
    fn new(inner: Box<dyn AudioSource + 'a>, frames: u64) -> Self {
        Self { inner, frames, buf: Vec::new() }
    }
}

impl AudioSource for Skip<'_> {
    fn info(&self) -> &StreamInfo {
        self.inner.info()
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        if self.frames == 0 {
            return self.inner.next_chunk();
        }
        let ch = self.inner.info().channels as usize;
        loop {
            let frames = std::mem::take(&mut self.frames);
            let Some(chunk) = self.inner.next_chunk()? else { return Ok(None) };
            let n = (chunk.len() / ch) as u64;
            if frames >= n {
                self.frames = frames - n;
                continue;
            }
            self.buf.clear();
            self.buf.extend_from_slice(&chunk[frames as usize * ch..]);
            return Ok(Some(&self.buf));
        }
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// How a speed and pitch change is carried out: time-stretch by `stretch` (output length over
/// input length, pitch untouched), then resample to `to` Hz while still calling it the
/// original rate, which scales both pitch and length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeedPlan {
    pub stretch: f64,
    pub to: u32,
}

/// Stretches within this of 1 are skipped (tape-style speed changes, where rounding the
/// resample rate leaves a hair of stretch that WSOLA would only make worse).
const NO_STRETCH: f64 = 0.005;

pub fn speed_plan(edits: &AudioEdits, rate: u32) -> SpeedPlan {
    let speed = edits.speed.clamp(AudioEdits::MIN_SPEED, AudioEdits::MAX_SPEED);
    let pitch = (if edits.keep_pitch { 1.0 } else { speed }) * 2f64.powf(edits.pitch.clamp(-12.0, 12.0) as f64 / 12.0);
    let to = if (pitch - 1.0).abs() < 1e-9 {
        rate
    } else {
        // A multiple of 25 Hz keeps the resampler's FFTs small (they scale with the rates
        // divided by their common factor); the pitch is then within about a cent.
        let exact = rate as f64 / pitch;
        let step = if rate.is_multiple_of(25) { 25.0 } else { 1.0 };
        (((exact / step).round() * step) as u32).max(25)
    };
    // The pitch the rounded rate gives, and the stretch that makes the length come out right.
    let actual = rate as f64 / to as f64;
    let mut stretch = actual / speed;
    if (stretch - 1.0).abs() < NO_STRETCH {
        stretch = 1.0;
    }
    SpeedPlan { stretch, to }
}

/// Time-stretch by WSOLA (waveform-similarity overlap-add): the output is built from
/// Hann-windowed input segments at half-overlap; each segment is taken from near where the
/// stretch says it should come from, nudged (within ±10 ms) to where the input best matches
/// how the previous segment would have carried on, so the waveforms line up and don't
/// phase-cancel. Pitch is untouched. Good on speech; dense music can sound a bit warbly,
/// and extreme stretches smear or stutter.
struct Stretch<S> {
    inner: S,
    info: StreamInfo,
    ch: usize,
    /// Output length over input length.
    stretch: f64,
    /// Segment length, half of it (the output hop), and the search range.
    n: usize,
    hop: usize,
    search: usize,
    window: Vec<f32>,
    /// Buffered input, interleaved and mixed to mono (for matching); `base` is the absolute
    /// frame number of their first frame.
    input: Vec<f32>,
    mono: Vec<f32>,
    base: u64,
    read: u64,
    ended: bool,
    /// Segments placed so far, and where the last one came from.
    step: u64,
    prev: u64,
    /// Overlap-add accumulator, `n` frames.
    ola: Vec<f32>,
    out: Vec<f32>,
    emitted: u64,
}

impl<S: AudioSource> Stretch<S> {
    fn new(inner: S, stretch: f64) -> Self {
        let mut info = inner.info().clone();
        let ch = info.channels.max(1) as usize;
        let n = ((info.sample_rate as f64 * 0.04) as usize).max(64) & !1;
        let hop = n / 2;
        let window = (0..n).map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()) as f32).collect();
        info.total_frames = info.total_frames.map(|t| (t as f64 * stretch).round() as u64);
        Self {
            inner,
            info,
            ch,
            stretch,
            n,
            hop,
            search: n / 4,
            window,
            input: Vec::new(),
            mono: Vec::new(),
            base: 0,
            read: 0,
            ended: false,
            step: 0,
            prev: 0,
            ola: vec![0.0; n * ch],
            out: Vec::new(),
            emitted: 0,
        }
    }

    /// Read until the input reaches frame `upto` (or ends).
    fn fill(&mut self, upto: u64) -> Result<()> {
        while !self.ended && self.read < upto {
            match self.inner.next_chunk()? {
                Some(chunk) => {
                    self.input.extend_from_slice(chunk);
                    self.mono.extend(chunk.chunks_exact(self.ch).map(|f| f.iter().sum::<f32>() / self.ch as f32));
                    self.read += (chunk.len() / self.ch) as u64;
                }
                None => self.ended = true,
            }
        }
        Ok(())
    }

    fn mono_at(&self, frame: u64) -> f32 {
        frame.checked_sub(self.base).and_then(|i| self.mono.get(i as usize)).copied().unwrap_or(0.0)
    }

    /// How alike the input at `cand` is to the natural continuation of the previous segment,
    /// comparing every `stride`-th frame over half a segment (normalized cross-correlation).
    fn similarity(&self, cand: u64, stride: usize) -> f32 {
        let target = self.prev + self.hop as u64;
        let (mut dot, mut energy) = (0.0f32, 1e-9f32);
        for i in (0..self.hop).step_by(stride) {
            let a = self.mono_at(cand + i as u64);
            dot += a * self.mono_at(target + i as u64);
            energy += a * a;
        }
        dot / energy.sqrt()
    }

    /// Where the next segment comes from.
    fn pick(&self, nominal: u64) -> u64 {
        if self.step == 0 {
            return 0;
        }
        let lo = nominal.saturating_sub(self.search as u64);
        let hi = nominal + self.search as u64;
        // Coarse search on every 8th position and frame, then refine around the best.
        let best_of = |range: &mut dyn Iterator<Item = u64>, stride: usize| {
            range.map(|c| (c, self.similarity(c, stride))).fold((nominal, f32::NEG_INFINITY), |b, x| if x.1 > b.1 { x } else { b }).0
        };
        let coarse = best_of(&mut (lo..=hi).step_by(8), 8);
        best_of(&mut (coarse.saturating_sub(7).max(lo)..=(coarse + 7).min(hi)), 2)
    }

    /// Length of the output, once the input length is known.
    fn target(&self) -> Option<u64> {
        self.ended.then(|| (self.read as f64 * self.stretch).round() as u64)
    }
}

impl<S: AudioSource> AudioSource for Stretch<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let (n, hop, ch) = (self.n, self.hop, self.ch);
        self.out.clear();
        while self.out.len() < CHUNK * ch {
            if self.target().is_some_and(|t| self.emitted >= t) {
                break;
            }
            let nominal = (self.step as f64 * hop as f64 / self.stretch).round() as u64;
            self.fill(nominal.max(self.prev + hop as u64) + (self.search + n) as u64)?;
            let pos = self.pick(nominal);
            for i in 0..n {
                // The first segment starts at full level, not faded in by its window.
                let w = if self.step == 0 && i < hop { 1.0 } else { self.window[i] };
                let at = (pos + i as u64).checked_sub(self.base).map(|f| f as usize * ch);
                for c in 0..ch {
                    let s = at.and_then(|a| self.input.get(a + c)).copied().unwrap_or(0.0);
                    self.ola[i * ch + c] += w * s;
                }
            }
            let mut take = hop as u64;
            if let Some(t) = self.target() {
                take = take.min(t - self.emitted);
            }
            self.out.extend_from_slice(&self.ola[..take as usize * ch]);
            self.emitted += take;
            self.ola.copy_within(hop * ch.., 0);
            self.ola[(n - hop) * ch..].fill(0.0);
            self.prev = pos;
            self.step += 1;
            // Forget input no segment can reach any more.
            let keep_from = self.prev.min(nominal.saturating_sub(self.search as u64));
            let drop = keep_from.saturating_sub(self.base) as usize;
            if drop > 1 << 16 {
                self.input.drain(..drop * ch);
                self.mono.drain(..drop);
                self.base += drop as u64;
            }
        }
        Ok(if self.out.is_empty() { None } else { Some(&self.out) })
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Mono to pseudo-stereo by complementary comb filtering: a delayed copy, high-passed so the
/// bass stays centred, is added to the left and subtracted from the right. The two sides
/// differ (it sounds wider) but still add up to the original, so it plays back correctly
/// in mono.
struct Widen<S> {
    inner: S,
    info: StreamInfo,
    amount: f32,
    delay: Vec<f32>,
    pos: usize,
    high_pass: Biquad,
    state: [f64; 2],
    buf: Vec<f32>,
}

/// Delay of the widening copy, and the frequency below which it stays out.
const WIDEN_DELAY: f64 = 0.012;
const WIDEN_FROM_HZ: f32 = 300.0;

impl<S: AudioSource> Widen<S> {
    fn new(inner: S, width: f32) -> Self {
        let mut info = inner.info().clone();
        info.channels = 2;
        let rate = info.sample_rate;
        let len = ((WIDEN_DELAY * rate as f64) as usize).max(1);
        Self {
            inner,
            info,
            amount: width.clamp(0.0, 1.0) * 0.7,
            delay: vec![0.0; len],
            pos: 0,
            high_pass: Biquad::new(WIDEN_FROM_HZ.min(rate as f32 * 0.4), rate, true),
            state: [0.0; 2],
            buf: Vec::new(),
        }
    }
}

impl<S: AudioSource> AudioSource for Widen<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let Some(chunk) = self.inner.next_chunk()? else { return Ok(None) };
        self.buf.clear();
        for &x in chunk {
            let delayed = std::mem::replace(&mut self.delay[self.pos], x);
            self.pos = (self.pos + 1) % self.delay.len();
            let side = self.amount * self.high_pass.process(delayed as f64, &mut self.state) as f32;
            self.buf.extend([x + side, x - side]);
        }
        Ok(Some(&self.buf))
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Resamples to `to` Hz but keeps calling it the original rate, so it plays back at a
/// different pitch and speed (a tape running faster or slower).
struct Repitch<S> {
    inner: S,
    info: StreamInfo,
    resampler: crate::resample::StreamResampler,
    buf: Vec<f32>,
    finished: bool,
}

impl<S: AudioSource> Repitch<S> {
    fn new(inner: S, to: u32) -> Result<Self> {
        let mut info = inner.info().clone();
        let resampler = crate::resample::StreamResampler::new(info.sample_rate, to, info.channels as usize)?;
        info.total_frames = info.total_frames.map(|t| (t as f64 * to as f64 / info.sample_rate as f64).ceil() as u64);
        Ok(Self { inner, info, resampler, buf: Vec::new(), finished: false })
    }
}

impl<S: AudioSource> AudioSource for Repitch<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
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

/// One per-frame effect with a memory, so it keeps sounding after its input stops.
trait Effect {
    fn process(&mut self, frame: &mut [f32]);
    /// Longest it can keep sounding after the input ends, in frames.
    fn tail(&self) -> u64;
    /// Longest stretch of silence it can produce mid-tail (an echo's gap), in frames.
    fn gap(&self) -> u64 {
        0
    }
}

/// A delay line per channel: each repeat is fed back at `feedback`.
struct EchoFx {
    buf: Vec<f32>,
    len: usize,
    pos: usize,
    feedback: f32,
    mix: f32,
}

impl EchoFx {
    fn new(e: &Echo, channels: usize, rate: u32) -> Self {
        let len = ((e.delay_ms.max(1.0) / 1000.0 * rate as f32) as usize).max(1);
        Self { buf: vec![0.0; len * channels], len, pos: 0, feedback: e.feedback.clamp(0.0, 0.95), mix: e.mix.clamp(0.0, 1.0) }
    }
}

impl Effect for EchoFx {
    fn process(&mut self, frame: &mut [f32]) {
        let ch = frame.len();
        let at = &mut self.buf[self.pos * ch..(self.pos + 1) * ch];
        for (s, d) in frame.iter_mut().zip(at) {
            let delayed = *d;
            *d = flush(*s + self.feedback * delayed);
            *s += self.mix * delayed;
        }
        self.pos = (self.pos + 1) % self.len;
    }
    fn tail(&self) -> u64 {
        // Repeats until they're 80 dB down.
        let repeats = if self.feedback > 0.0 { (1e-4f32.ln() / self.feedback.ln()).ceil() as u64 + 1 } else { 1 };
        self.len as u64 * repeats
    }
    fn gap(&self) -> u64 {
        self.len as u64
    }
}

/// Freeverb (Jezar at Dreampoint, public domain): eight damped comb filters in parallel,
/// then four all-pass filters in series, per channel; channels are offset slightly for width.
struct ReverbFx {
    channels: Vec<FreeverbChannel>,
    feedback: f32,
    damp: f32,
    mix: f32,
    longest: usize,
}

struct FreeverbChannel {
    combs: Vec<(Vec<f32>, usize, f32)>,
    allpasses: Vec<(Vec<f32>, usize)>,
}

const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
/// Extra delay per channel (Freeverb's stereo spread).
const SPREAD: usize = 23;

impl ReverbFx {
    fn new(r: &Reverb, channels: usize, rate: u32) -> Self {
        // Freeverb's tunings are for 44.1 kHz.
        let scale = |n: usize| ((n as f64 * rate as f64 / 44_100.0) as usize).max(1);
        let channels: Vec<FreeverbChannel> = (0..channels)
            .map(|c| FreeverbChannel {
                combs: COMBS.iter().map(|&n| (vec![0.0; scale(n + SPREAD * c)], 0, 0.0)).collect(),
                allpasses: ALLPASSES.iter().map(|&n| (vec![0.0; scale(n + SPREAD * c)], 0)).collect(),
            })
            .collect();
        let longest = scale(COMBS[7] + SPREAD * channels.len());
        Self {
            channels,
            feedback: r.size.clamp(0.0, 0.98) * 0.28 + 0.7,
            damp: r.damping.clamp(0.0, 1.0) * 0.4,
            mix: r.mix.clamp(0.0, 1.0),
            longest,
        }
    }
}

impl Effect for ReverbFx {
    fn process(&mut self, frame: &mut [f32]) {
        // Every channel's reverb is fed the mix of all of them, like Freeverb's stereo input.
        let input = frame.iter().sum::<f32>() * 0.015 * 2.0 / frame.len() as f32;
        for (s, ch) in frame.iter_mut().zip(&mut self.channels) {
            let mut wet = 0.0;
            for (buf, i, store) in &mut ch.combs {
                let out = buf[*i];
                *store = flush(out * (1.0 - self.damp) + *store * self.damp);
                buf[*i] = input + *store * self.feedback;
                *i = (*i + 1) % buf.len();
                wet += out;
            }
            for (buf, i) in &mut ch.allpasses {
                let delayed = buf[*i];
                buf[*i] = flush(wet + delayed * 0.5);
                *i = (*i + 1) % buf.len();
                wet = delayed - wet;
            }
            *s = *s * (1.0 - self.mix) + wet * 3.0 * self.mix;
        }
    }
    fn tail(&self) -> u64 {
        // Comb passes until 80 dB down, plus the all-passes.
        let passes = (1e-4f32.ln() / self.feedback.ln()).ceil() as u64;
        self.longest as u64 * passes + ALLPASSES.iter().sum::<usize>() as u64
    }
}

/// Flush values too small to hear to zero: decaying feedback otherwise ends up in
/// denormal floats, which are very slow on some CPUs.
fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 { 0.0 } else { x }
}

/// Runs echo and reverb, then keeps going after the input ends until they've died away
/// (silent for longer than any echo gap) or their longest tail is over.
struct Effects<S> {
    inner: S,
    info: StreamInfo,
    fx: Vec<Box<dyn Effect>>,
    buf: Vec<f32>,
    ended: bool,
    tail_left: u64,
    quiet: u64,
    quiet_enough: u64,
}

/// Below this (−80 dBFS) a tail counts as silent.
const SILENT: f32 = 1e-4;
/// Most an effect tail may add, in seconds.
const MAX_TAIL: f64 = 30.0;

impl<S: AudioSource> Effects<S> {
    fn new(inner: S, edits: &AudioEdits) -> Self {
        let mut info = inner.info().clone();
        let (ch, rate) = (info.channels as usize, info.sample_rate);
        let mut fx: Vec<Box<dyn Effect>> = Vec::new();
        if let Some(e) = &edits.echo {
            fx.push(Box::new(EchoFx::new(e, ch, rate)));
        }
        if let Some(r) = &edits.reverb {
            fx.push(Box::new(ReverbFx::new(r, ch, rate)));
        }
        // Echoes of reverb (or reverb of echoes) last about as long as both together.
        let tail = fx.iter().map(|f| f.tail()).sum::<u64>().min((MAX_TAIL * rate as f64) as u64);
        let quiet_enough = fx.iter().map(|f| f.gap()).max().unwrap_or(0) + rate as u64 / 20;
        info.total_frames = None;
        Self { inner, info, fx, buf: Vec::new(), ended: false, tail_left: tail, quiet: 0, quiet_enough }
    }
}

impl<S: AudioSource> AudioSource for Effects<S> {
    fn info(&self) -> &StreamInfo {
        &self.info
    }
    fn tags(&self) -> &Tags {
        self.inner.tags()
    }
    fn next_chunk(&mut self) -> Result<Option<&[f32]>> {
        let ch = self.info.channels as usize;
        self.buf.clear();
        if !self.ended {
            match self.inner.next_chunk()? {
                Some(chunk) => self.buf.extend_from_slice(chunk),
                None => self.ended = true,
            }
        }
        if self.ended {
            if self.tail_left == 0 || self.quiet >= self.quiet_enough {
                return Ok(None);
            }
            let n = (CHUNK as u64).min(self.tail_left);
            self.tail_left -= n;
            self.buf.resize(n as usize * ch, 0.0);
        }
        for frame in self.buf.chunks_exact_mut(ch) {
            for f in &mut self.fx {
                f.process(frame);
            }
            if self.ended {
                let loud = frame.iter().any(|s| s.abs() >= SILENT);
                self.quiet = if loud { 0 } else { self.quiet + 1 };
            }
        }
        Ok(Some(&self.buf))
    }
    fn end_note(&self) -> Option<String> {
        self.inner.end_note()
    }
}

/// Measures the editor's in-memory copy as the volume stage will see it, mixed to the
/// output format's channel setting if it has one (`None` = keep the edited layout).
pub fn measure_memory(
    samples: Arc<[f32]>,
    info: &StreamInfo,
    edits: &AudioEdits,
    output_channels: Option<u16>,
    cancel: &AtomicBool,
) -> Result<Measurement> {
    let channels = output_channels.unwrap_or_else(|| edited_channels(edits, info.channels));
    measure(shape_to(Box::new(MemorySource::new(samples, info, 0, None)), edits, channels)?, Some(cancel))
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
        let plan = Plan { start: 0, end: None, reverse: false, speed: 1.0, fade_in: 1000, fade_out: 2000 };
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
        let full = drain(apply(Box::new(MemorySource::new(samples.clone(), &i, 0, None)), &e, None).unwrap());
        let part = drain(preview(samples, &i, &e, None, 1000, 1, 1000).unwrap());
        assert_eq!(full.len(), 3500);
        assert_eq!(&full[500..], &part[..]);
    }

    fn sine(freq: f32, amp: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize).map(|i| (i as f32 * freq * std::f32::consts::TAU / rate as f32).sin() * amp).collect()
    }

    fn info_at(rate: u32, channels: u16, frames: usize) -> StreamInfo {
        StreamInfo { sample_rate: rate, channels, source_bits: None, total_frames: Some(frames as u64), source_rate: rate }
    }

    fn render(samples: Vec<f32>, rate: u32, edits: &AudioEdits) -> Vec<f32> {
        let i = info_at(rate, 1, samples.len());
        let samples: Arc<[f32]> = samples.into();
        let cancel = AtomicBool::new(false);
        let m = edits.volume.measures().then(|| measure_memory(samples.clone(), &i, edits, None, &cancel).unwrap());
        drain(apply(Box::new(MemorySource::new(samples, &i, 0, None)), edits, m.as_ref()).unwrap())
    }

    fn rms(v: &[f32]) -> f32 {
        // Skip the filters' settling time.
        let v = &v[v.len() / 4..];
        (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt()
    }

    #[test]
    fn low_and_high_cut_remove_their_side_and_keep_the_other() {
        let rate = 48_000;
        let low = AudioEdits { low_cut: Some(200.0), ..Default::default() };
        assert!(rms(&render(sine(40.0, 0.5, rate, 1.0), rate, &low)) < 0.5 / 2f32.sqrt() * 0.05);
        assert!((rms(&render(sine(2000.0, 0.5, rate, 1.0), rate, &low)) / (0.5 / 2f32.sqrt()) - 1.0).abs() < 0.02);
        let high = AudioEdits { high_cut: Some(2000.0), ..Default::default() };
        assert!(rms(&render(sine(16_000.0, 0.5, rate, 1.0), rate, &high)) < 0.5 / 2f32.sqrt() * 0.05);
        assert!((rms(&render(sine(200.0, 0.5, rate, 1.0), rate, &high)) / (0.5 / 2f32.sqrt()) - 1.0).abs() < 0.02);
        // Cutoffs the rate can't carry are skipped rather than blowing up.
        let silly = AudioEdits { high_cut: Some(30_000.0), ..Default::default() };
        assert!(filters(&silly, rate).is_empty());
    }

    #[test]
    fn limiter_holds_the_ceiling_and_leaves_quiet_parts_alone() {
        let rate = 48_000;
        let mut v = sine(300.0, 0.05, rate, 1.0);
        for s in &mut v[24_000..24_480] {
            *s *= 18.0; // a 0.9 burst
        }
        let n = v.len();
        let out = render(v.clone(), rate, &AudioEdits { volume: Volume::Gain(12.0), ..Default::default() });
        assert_eq!(out.len(), n, "lookahead doesn't change the length");
        assert!(out.iter().all(|s| s.abs() <= CEILING), "peak {}", out.iter().fold(0.0f32, |p, s| p.max(s.abs())));
        let g = 10f32.powf(12.0 / 20.0);
        // Before the burst (lookahead is only 5 ms) the gain is exact; the release recovers
        // exponentially (100 ms), so ~0.5 s after it the gain is back within 1%.
        for i in [1000, 20_000] {
            assert!((out[i] - v[i] * g).abs() < 1e-4, "frame {i}: {} vs {}", out[i], v[i] * g);
        }
        assert!((out[47_000] / (v[47_000] * g) - 1.0).abs() < 0.01);
    }

    #[test]
    fn normalizes_to_a_peak_and_to_a_loudness() {
        let rate = 48_000;
        let peaked = render(sine(1000.0, 0.25, rate, 2.0), rate, &AudioEdits { volume: Volume::Peak(-1.0), ..Default::default() });
        let peak = peaked.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!((20.0 * peak.log10() + 1.0).abs() < 0.01, "{peak}");

        let target = -23.0;
        let loud = render(sine(1000.0, 0.05, rate, 4.0), rate, &AudioEdits { volume: Volume::Loudness(target), ..Default::default() });
        let i = info_at(rate, 1, loud.len());
        let m = measure(Box::new(MemorySource::new(loud.into(), &i, 0, None)), None).unwrap();
        assert!((m.loudness - target as f64).abs() < 0.1, "{m:?}");
        // Silence can't be normalized: left alone.
        assert_eq!(volume_gain(Volume::Loudness(-14.0), Some(&Measurement { peak: 0.0, loudness: f64::NEG_INFINITY })), 1.0);
    }

    #[test]
    fn reverse_plays_the_kept_part_backwards_and_previews_from_the_playhead() {
        let samples = ramp(2, 10_000);
        let i = info(2, 10_000);
        let e = AudioEdits { trim_start: 1.0, trim_end: Some(9.0), reverse: true, fade_out: 0.5, ..Default::default() };
        let full = drain(apply(Box::new(MemorySource::new(samples.clone(), &i, 0, None)), &e, None).unwrap());
        assert_eq!(full.len(), 8000 * 2);
        // The cut at 9 s is heard first, so it gets the short cut fade; the 0.5 s fade-out ends at 1 s.
        let plan = Plan::new(&e, 1000, Some(10_000));
        assert_eq!((plan.fade_in, plan.fade_out), (5, 500));
        assert_eq!(full[2 * 100], 8899.0, "frame 100 of the output is source frame 8899");
        assert!(full[full.len() - 1] < 1.0);
        let part = drain(preview(samples, &i, &e, None, 5000, 2, 1000).unwrap());
        assert_eq!(part.len(), 4001 * 2);
        assert_eq!(&full[full.len() - part.len()..], &part[..]);
        assert_eq!(plan.output_index(5000, 9000), 3999);
    }

    /// A half-scale click (full scale would be trimmed by the limiter that effects arm).
    fn impulse(channels: u16, frames: usize) -> Arc<[f32]> {
        let mut v = vec![0.0; frames * channels as usize];
        v[..channels as usize].fill(0.5);
        v.into()
    }

    #[test]
    fn echo_repeats_at_the_delay_and_rings_on_past_the_end() {
        let e = AudioEdits { echo: Some(Echo { delay_ms: 100.0, feedback: 0.5, mix: 0.8 }), ..Default::default() };
        let out = drain(apply(Box::new(MemorySource::new(impulse(1, 50), &info(1, 50), 0, None)), &e, None).unwrap());
        assert_eq!(out[0], 0.5);
        assert!((out[100] - 0.4).abs() < 1e-6 && (out[200] - 0.2).abs() < 1e-6 && (out[300] - 0.1).abs() < 1e-6);
        assert!(out[1..100].iter().all(|&s| s == 0.0));
        // Stops once the repeats are below −80 dB, rather than running to the cap.
        let last = out.iter().rposition(|s| s.abs() > 0.0).unwrap();
        assert!(out.len() > 1300 && out.len() < 1500 + 4096, "{} frames, last sound at {last}", out.len());
    }

    #[test]
    fn a_long_echo_gap_is_not_mistaken_for_the_end() {
        let e = AudioEdits { echo: Some(Echo { delay_ms: 2000.0, feedback: 0.3, mix: 0.5 }), ..Default::default() };
        let out = drain(apply(Box::new(MemorySource::new(impulse(1, 10), &info(1, 10), 0, None)), &e, None).unwrap());
        assert!((out[2000] - 0.25).abs() < 1e-6 && (out[4000] - 0.075).abs() < 1e-6, "second repeat survives the 2 s gap");
    }

    #[test]
    fn reverb_decays_after_a_fade_out_and_spreads_the_channels() {
        let rate = 44_100;
        let samples: Arc<[f32]> = sine(440.0, 0.5, rate, 0.5).into_iter().flat_map(|s| [s, s]).collect();
        let i = info_at(rate, 2, samples.len() / 2);
        let e = AudioEdits { reverb: Some(Reverb::HALL), fade_out: 0.1, ..Default::default() };
        let out = drain(apply(Box::new(MemorySource::new(samples.clone(), &i, 0, None)), &e, None).unwrap());
        let n = samples.len();
        assert!(out.len() > n + rate as usize * 2, "a hall rings for seconds: {} extra frames", (out.len() - n) / 2);
        assert!(out.len() < n + rate as usize * 2 * MAX_TAIL as usize);
        let tail = &out[n..];
        assert!(tail[..2000].iter().any(|s| s.abs() > 1e-3), "the tail carries on after the faded end");
        assert!(tail[tail.len() - 2000..].iter().all(|s| s.abs() < 1e-3), "and dies away");
        assert!(out.as_chunks::<2>().0.iter().any(|f| (f[0] - f[1]).abs() > 1e-3), "left and right differ");
        // Preview from the start is the same render.
        let p = drain(preview(samples, &i, &e, None, 0, 2, rate).unwrap());
        assert_eq!(p, out);
    }

    /// Frequency from upward zero crossings over the middle half.
    fn freq(v: &[f32], rate: u32) -> f32 {
        let mid = &v[v.len() / 4..v.len() * 3 / 4];
        let ups = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        ups as f32 * rate as f32 / mid.len() as f32
    }

    fn rms_mid(v: &[f32]) -> f32 {
        let mid = &v[v.len() / 4..v.len() * 3 / 4];
        (mid.iter().map(|s| s * s).sum::<f32>() / mid.len() as f32).sqrt()
    }

    #[test]
    fn speed_plans_split_into_stretch_and_resample() {
        let p = |speed, keep_pitch, pitch| speed_plan(&AudioEdits { speed, keep_pitch, pitch, ..Default::default() }, 48_000);
        assert_eq!(p(2.0, false, 0.0), SpeedPlan { stretch: 1.0, to: 24_000 }, "tape: resample only");
        assert_eq!(p(2.0, true, 0.0), SpeedPlan { stretch: 0.5, to: 48_000 }, "keep pitch: stretch only");
        assert_eq!(p(1.0, true, 12.0), SpeedPlan { stretch: 2.0, to: 24_000 }, "octave up at the same speed");
        assert_eq!(p(1.0, true, 0.0), SpeedPlan { stretch: 1.0, to: 48_000 });
        // A semitone: the rate is rounded to 25 Hz, within ~1 cent, and the stretch makes up the length.
        let s = p(1.0, true, 1.0);
        assert_eq!(s.to % 25, 0);
        let cents = 1200.0 * (48_000.0 / s.to as f64).log2();
        assert!((cents - 100.0).abs() < 1.0, "{cents}");
        assert!((s.stretch / (48_000.0 / s.to as f64) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn time_stretch_keeps_pitch_and_level_and_hits_the_length() {
        let rate = 16_000;
        let input = sine(440.0, 0.5, rate, 1.0);
        for speed in [AudioEdits::MIN_SPEED, 0.5, 0.8, 1.25, 2.0, AudioEdits::MAX_SPEED] {
            let e = AudioEdits { speed, ..Default::default() };
            let out = render(input.clone(), rate, &e);
            let want = (input.len() as f64 / speed).round() as usize;
            assert!(out.len().abs_diff(want) <= 1, "speed {speed}: {} frames, wanted {want}", out.len());
            let f = freq(&out, rate);
            assert!((f - 440.0).abs() < 8.0, "speed {speed}: {f} Hz");
            let level = rms_mid(&out) / rms_mid(&input);
            assert!((level - 1.0).abs() < 0.1, "speed {speed}: level {level} (segments cancelling?)");
        }
        // The very start comes through untouched rather than faded in by the window.
        let out = render(input.clone(), rate, &AudioEdits { speed: 0.5, ..Default::default() });
        assert!(out[..200].iter().zip(&input).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn tape_speed_and_pitch_shift_move_the_frequency() {
        let rate = 16_000;
        let input = sine(440.0, 0.5, rate, 1.0);
        let tape = render(input.clone(), rate, &AudioEdits { speed: 2.0, keep_pitch: false, ..Default::default() });
        assert!(tape.len().abs_diff(8000) <= 1 && (freq(&tape, rate) - 880.0).abs() < 10.0);
        let up = render(input.clone(), rate, &AudioEdits { pitch: 12.0, ..Default::default() });
        assert!(up.len().abs_diff(16_000) <= 2, "{}", up.len());
        assert!((freq(&up, rate) - 880.0).abs() < 10.0, "{}", freq(&up, rate));
        let down = render(input.clone(), rate, &AudioEdits { pitch: -7.0, speed: 1.5, ..Default::default() });
        assert!(down.len().abs_diff(10_667) <= 2, "{}", down.len());
        let want = 440.0 * 2f32.powf(-7.0 / 12.0);
        assert!((freq(&down, rate) - want).abs() < 6.0, "{} vs {want}", freq(&down, rate));
    }

    #[test]
    fn output_positions_follow_the_speed() {
        let e = AudioEdits { trim_start: 1.0, trim_end: Some(5.0), speed: 2.0, ..Default::default() };
        let p = Plan::new(&e, 1000, Some(10_000));
        assert_eq!((p.output_index(3000, 5000), p.output_len(4000)), (1000, 2000));
        assert_eq!(p.source_frame(1000, 5000), 3000);
        let r = Plan { reverse: true, ..p };
        assert_eq!((r.output_index(4999, 5000), r.output_index(1000, 5000)), (0, 1999));
        assert_eq!(r.source_frame(0, 5000), 4999);
    }

    fn render_channels(samples: Vec<f32>, channels: u16, rate: u32, edits: &AudioEdits) -> (u16, Vec<f32>) {
        let i = info_at(rate, channels, samples.len() / channels as usize);
        let src = apply(Box::new(MemorySource::new(samples.into(), &i, 0, None)), edits, None).unwrap();
        let ch = src.info().channels;
        (ch, drain(src))
    }

    #[test]
    fn mono_to_stereo_widens_but_still_adds_up_to_the_original() {
        let rate = 48_000;
        let input: Vec<f32> = sine(3000.0, 0.3, rate, 0.5).iter().zip(sine(55.0, 0.3, rate, 0.5)).map(|(a, b)| a + b).collect();
        let copy = AudioEdits { channels: Channels::Stereo { width: 0.0 }, ..Default::default() };
        let (ch, out) = render_channels(input.clone(), 1, rate, &copy);
        assert_eq!(ch, 2);
        assert!(out.as_chunks::<2>().0.iter().zip(&input).all(|(f, &x)| f[0] == x && f[1] == x), "width 0 is a plain copy");

        let wide = AudioEdits { channels: Channels::Stereo { width: 1.0 }, ..Default::default() };
        let (_, out) = render_channels(input.clone(), 1, rate, &wide);
        let frames = out.as_chunks::<2>().0;
        assert_eq!(frames.len(), input.len());
        assert!(frames.iter().zip(&input).all(|(f, &x)| ((f[0] + f[1]) / 2.0 - x).abs() < 1e-5), "mono-compatible");
        let side: Vec<f32> = frames.iter().map(|f| (f[0] - f[1]) / 2.0).collect();
        let at = |freq: f32| {
            // Side-signal level at one frequency (correlate with a sine and cosine).
            let (mut c, mut q) = (0.0f32, 0.0f32);
            for (n, s) in side.iter().enumerate().skip(4800) {
                let ph = n as f32 * freq * std::f32::consts::TAU / rate as f32;
                c += s * ph.cos();
                q += s * ph.sin();
            }
            (c * c + q * q).sqrt() * 2.0 / (side.len() - 4800) as f32
        };
        assert!(at(3000.0) > 0.1, "highs spread: {}", at(3000.0));
        assert!(at(55.0) < 0.01, "bass stays centred: {}", at(55.0));
    }

    #[test]
    fn channel_settings_mix_down_and_fold_up() {
        let stereo: Vec<f32> = (0..2000).flat_map(|i| [i as f32 / 4000.0, -(i as f32) / 4000.0]).collect();
        let (ch, out) = render_channels(stereo.clone(), 2, 1000, &AudioEdits { channels: Channels::Mono, ..Default::default() });
        assert_eq!((ch, out.len()), (1, 2000));
        assert!(out.iter().all(|&s| s.abs() < 1e-6), "opposite channels cancel in the mix");
        // Stereo on a stereo source changes nothing, whatever the width says.
        let (ch, out) = render_channels(stereo.clone(), 2, 1000, &AudioEdits { channels: Channels::Stereo { width: 0.8 }, ..Default::default() });
        assert_eq!((ch, &out), (2, &stereo));
    }

    #[test]
    fn the_limiter_decision_does_not_depend_on_having_a_measurement() {
        let m = Measurement { peak: 0.5, loudness: -20.0 };
        for e in [
            AudioEdits::default(),
            AudioEdits { reverb: Some(Reverb::HALL), ..Default::default() },
            AudioEdits { channels: Channels::Stereo { width: 0.5 }, ..Default::default() },
            AudioEdits { volume: Volume::Gain(3.0), ..Default::default() },
            AudioEdits { volume: Volume::Gain(-3.0), ..Default::default() },
        ] {
            let g = volume_gain(e.volume, None);
            assert_eq!(limits(&e, 1, g, None), limits(&e, 1, g, Some(&m)), "{e:?}");
        }
        assert!(!limits(&AudioEdits::default(), 2, 1.0, None), "untouched audio is never limited");
        // Filters and speed/pitch can overshoot, so they arm it too.
        for e in [
            AudioEdits { low_cut: Some(200.0), ..Default::default() },
            AudioEdits { speed: 1.5, ..Default::default() },
            AudioEdits { pitch: -2.0, ..Default::default() },
        ] {
            assert!(limits(&e, 2, 1.0, None), "{e:?}");
        }
        // Widening only happens to mono sources.
        let wide = AudioEdits { channels: Channels::Stereo { width: 0.8 }, ..Default::default() };
        assert!(limits(&wide, 1, 1.0, None) && !limits(&wide, 2, 1.0, None));
    }

    #[test]
    fn a_short_length_estimate_does_not_cut_the_end_off() {
        // A decoder that underestimates its length, like an MP3 without a Xing header.
        let samples = ramp(1, 5000);
        let mut i = info(1, 5000);
        i.total_frames = Some(4000);
        let src = MemorySource { info: i.clone(), ..MemorySource::new(samples, &info(1, 5000), 0, None) };
        let e = AudioEdits { trim_start: 1.0, ..Default::default() };
        let out = drain(apply(Box::new(src), &e, None).unwrap());
        assert_eq!(out.len(), 4000, "kept from 1 s to the real end at 5 s");
        assert_eq!(out[out.len() - 1], 4999.0);
    }

    #[test]
    fn preview_from_mid_way_matches_the_render_even_with_memory_in_the_chain() {
        let rate = 16_000;
        let samples: Arc<[f32]> = sine(330.0, 0.3, rate, 6.0).into();
        let i = info_at(rate, 1, samples.len());
        let e = AudioEdits {
            trim_start: 0.5,
            low_cut: Some(150.0),
            channels: Channels::Stereo { width: 0.7 },
            echo: Some(Echo { delay_ms: 200.0, feedback: 0.3, mix: 0.4 }),
            reverb: Some(Reverb::ROOM),
            volume: Volume::Gain(6.0),
            ..Default::default()
        };
        let full = drain(apply(Box::new(MemorySource::new(samples.clone(), &i, 0, None)), &e, None).unwrap());
        // Play from 4 s in: 3.5 s into the result.
        let part = drain(preview(samples, &i, &e, None, 4 * rate as u64, 2, rate).unwrap());
        let skip = (3.5 * rate as f64) as usize * 2;
        assert_eq!(part.len(), full.len() - skip);
        let worst = part.iter().zip(&full[skip..]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "differs by up to {worst}");
    }
}
