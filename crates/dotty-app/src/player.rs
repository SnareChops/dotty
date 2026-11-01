//! Plays the audio editor's preview on the default output device.
//!
//! A producer thread runs the same stages the converter uses (`codecs_audio::edit::preview`)
//! into a lock-free ring buffer; the device callback only copies samples out of it.

use codecs_audio::StreamInfo;
use codecs_audio::edit::{Measurement, Plan, preview};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use dotty_core::AudioEdits;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// Seconds of audio buffered ahead of the device.
const AHEAD: f64 = 0.25;

/// What one play request needs.
pub struct Request {
    pub samples: Arc<[f32]>,
    pub info: StreamInfo,
    pub edits: AudioEdits,
    /// The level the volume stage normalizes from, if measured yet.
    pub measured: Option<Measurement>,
    /// Source frame to start at.
    pub from: u64,
    /// Start over from the kept range's beginning on reaching its end.
    pub looping: bool,
}

struct Playing {
    _stream: cpal::Stream,
    stop: Arc<AtomicBool>,
    /// Device frames handed to the device from the ring (silence not counted).
    played: Arc<AtomicU64>,
    /// The producer has nothing more to give and the ring is empty.
    finished: Arc<AtomicBool>,
    device_rate: u32,
    source_rate: u32,
    from: u64,
    plan: Plan,
    total: u64,
    /// Device frames at which each loop pass after the first begins (passes can be longer
    /// than the kept part: echo and reverb tails).
    passes: Arc<Mutex<Vec<u64>>>,
}

#[derive(Default)]
pub struct Player {
    playing: Option<Playing>,
}

impl Drop for Playing {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Player {
    pub fn is_playing(&self) -> bool {
        self.playing.as_ref().is_some_and(|p| !p.finished.load(Ordering::Relaxed))
    }

    /// Stop if the end was reached; returns whether it just stopped that way.
    pub fn reap(&mut self) -> bool {
        let ended = self.playing.as_ref().is_some_and(|p| p.finished.load(Ordering::Relaxed));
        if ended {
            self.playing = None;
        }
        ended
    }

    pub fn stop(&mut self) {
        self.playing = None;
    }

    /// The source frame being heard now, while playing.
    pub fn position(&self) -> Option<u64> {
        let p = self.playing.as_ref()?;
        let played = p.played.load(Ordering::Relaxed);
        let pass = p.passes.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().find(|&&b| b <= played).copied();
        let to_source = |frames: u64| (frames as f64 * p.source_rate as f64 / p.device_rate as f64) as u64;
        let end = p.plan.end.unwrap_or(p.total);
        let len = end.saturating_sub(p.plan.start).max(1);
        // Work in output order (which runs backwards through the source when reversed);
        // during an effect tail the playhead waits at the end.
        let at = match pass {
            Some(b) => to_source(played - b),
            None => p.plan.output_index(p.from, end) + to_source(played),
        }
        .min(len - 1);
        Some(if p.plan.reverse { end - 1 - at } else { p.plan.start + at })
    }

    pub fn play(&mut self, req: Request) -> Result<(), String> {
        self.stop();
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device found")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        // Play at the file's own rate when the device takes it, so nothing is resampled.
        let rate = device
            .supported_output_configs()
            .ok()
            .and_then(|mut cs| {
                cs.find(|c| {
                    c.channels() == supported.channels()
                        && c.sample_format() == supported.sample_format()
                        && (c.min_sample_rate()..=c.max_sample_rate()).contains(&req.info.sample_rate)
                })
            })
            .map_or(supported.sample_rate(), |_| req.info.sample_rate);
        let device_channels = supported.channels() as usize;
        let channels = (device_channels.min(2)) as u16;
        let config = cpal::StreamConfig { channels: supported.channels(), sample_rate: rate, buffer_size: cpal::BufferSize::Default };

        let capacity = ((rate as f64 * AHEAD) as usize).max(4096) * channels as usize;
        let (mut tx, rx) = rtrb::RingBuffer::<f32>::new(capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let played = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let produced_all = Arc::new(AtomicBool::new(false));
        let passes = Arc::new(Mutex::new(Vec::new()));

        let total = (req.samples.len() / req.info.channels.max(1) as usize) as u64;
        let plan = Plan::new(&req.edits, req.info.sample_rate, Some(total));
        let from = req.from.clamp(plan.start, plan.end.unwrap_or(total).saturating_sub(1));
        let source_rate = req.info.sample_rate;

        let feed = Feed { rx, channels: channels as usize, played: played.clone(), produced_all: produced_all.clone(), finished: finished.clone() };
        use cpal::SampleFormat as F;
        let stream = match supported.sample_format() {
            F::F32 => build::<f32>(&device, &config, feed, device_channels),
            F::I16 => build::<i16>(&device, &config, feed, device_channels),
            F::I32 => build::<i32>(&device, &config, feed, device_channels),
            F::U16 => build::<u16>(&device, &config, feed, device_channels),
            f => return Err(format!("unsupported output sample format {f}")),
        }?;

        {
            let (stop, samples, info, edits, measured, looping) =
                (stop.clone(), req.samples, req.info, req.edits, req.measured, req.looping);
            // Where the result begins, for looping back.
            let restart = if plan.reverse { plan.end.unwrap_or(total).saturating_sub(1) } else { plan.start };
            let (passes, ring_channels) = (passes.clone(), channels as u64);
            thread::spawn(move || {
                let mut start = from;
                let mut pushed = 0u64;
                loop {
                    let Ok(mut src) = preview(samples.clone(), &info, &edits, measured.as_ref(), start, channels, rate) else { break };
                    while let Ok(Some(chunk)) = src.next_chunk() {
                        let mut rest = chunk;
                        while !rest.is_empty() {
                            if stop.load(Ordering::Relaxed) {
                                return;
                            }
                            let n = tx.slots().min(rest.len());
                            if n == 0 {
                                thread::sleep(Duration::from_millis(5));
                                continue;
                            }
                            if let Ok(chunk) = tx.write_chunk_uninit(n) {
                                pushed += chunk.fill_from_iter(rest[..n].iter().copied()) as u64;
                            }
                            rest = &rest[n..];
                        }
                    }
                    if !looping || stop.load(Ordering::Relaxed) {
                        break;
                    }
                    start = restart;
                    passes.lock().unwrap_or_else(|e| e.into_inner()).push(pushed / ring_channels);
                }
                produced_all.store(true, Ordering::Relaxed);
            });
        }
        stream.play().map_err(|e| e.to_string())?;
        self.playing = Some(Playing {
            _stream: stream,
            stop,
            played,
            finished,
            device_rate: rate,
            source_rate,
            from,
            plan,
            total,
            passes,
        });
        Ok(())
    }
}

/// The device callback's side of the ring.
struct Feed {
    rx: rtrb::Consumer<f32>,
    /// Channels in the ring (1 or 2).
    channels: usize,
    played: Arc<AtomicU64>,
    produced_all: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut feed: Feed,
    device_channels: usize,
) -> Result<cpal::Stream, String> {
    let callback = move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
        let mut frames = 0u64;
        for frame in out.chunks_exact_mut(device_channels) {
            if feed.rx.slots() < feed.channels {
                frame.fill(T::EQUILIBRIUM);
                continue;
            }
            for (c, s) in frame.iter_mut().enumerate() {
                *s = if c < feed.channels { T::from_sample(feed.rx.pop().unwrap_or(0.0)) } else { T::EQUILIBRIUM };
            }
            frames += 1;
        }
        feed.played.fetch_add(frames, Ordering::Relaxed);
        if feed.produced_all.load(Ordering::Relaxed) && feed.rx.slots() < feed.channels {
            feed.finished.store(true, Ordering::Relaxed);
        }
    };
    device
        .build_output_stream(*config, callback, |e| eprintln!("audio output: {e}"), None)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a sound device, so it only runs on request: `cargo test -p dotty-app -- --ignored player`.
    #[test]
    #[ignore]
    fn player_plays_to_the_end_and_reports_position() {
        let rate = 44_100u32;
        let samples: Arc<[f32]> = (0..rate as usize / 2).map(|i| (i as f32 * 0.06).sin() * 0.05).collect();
        let info = StreamInfo { sample_rate: rate, channels: 1, source_bits: Some(16), total_frames: Some(rate as u64 / 2), source_rate: rate };
        let edits = AudioEdits { trim_start: 0.1, fade_out: 0.1, ..Default::default() };
        let mut p = Player::default();
        p.play(Request { samples, info, edits, measured: None, from: 0, looping: false }).unwrap();
        assert!(p.is_playing());
        thread::sleep(Duration::from_millis(200));
        let pos = p.position().unwrap();
        assert!(pos > 4410 + 2000, "playback starts at the trim and moves: {pos}");
        thread::sleep(Duration::from_millis(600));
        assert!(!p.is_playing() && p.reap(), "finished");
    }

    #[test]
    #[ignore]
    fn reverb_tail_keeps_playing_with_the_playhead_at_the_end() {
        let rate = 44_100u32;
        let frames = rate as usize / 4;
        let samples: Arc<[f32]> = (0..frames).map(|i| (i as f32 * 0.06).sin() * 0.05).collect();
        let info = StreamInfo { sample_rate: rate, channels: 1, source_bits: Some(16), total_frames: Some(frames as u64), source_rate: rate };
        let edits = AudioEdits { reverb: Some(dotty_core::Reverb::HALL), ..Default::default() };
        let mut p = Player::default();
        p.play(Request { samples, info, edits, measured: None, from: 0, looping: false }).unwrap();
        thread::sleep(Duration::from_millis(700));
        assert!(p.is_playing(), "still ringing well after the 250 ms clip");
        assert_eq!(p.position(), Some(frames as u64 - 1));
    }
}
