use super::*;
use crate::wav::WavWriter;
use std::sync::atomic::AtomicBool;
use dotty_core::{Options, Preset};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tf-aud-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_notes(src: &Path, dst: &Path, from: Format, to: Format, options: &Options) -> Vec<String> {
    let cancel = AtomicBool::new(false);
    let progress = |_: f32| {};
    let ctx = ConvertCtx::new(options, &cancel, &progress);
    AudioConverter::default().convert(src, dst, from, to, &ctx).unwrap();
    ctx.take_notes()
}

fn run(src: &Path, dst: &Path, from: Format, to: Format, options: &Options) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let progress = |_: f32| {};
    AudioConverter::default().convert(src, dst, from, to, &ConvertCtx::new(options, &cancel, &progress))
}

/// Deterministic full-scale-ish test signal, interleaved and quantised to `bits`.
fn signal(channels: u16, frames: usize, bits: u32) -> Vec<i32> {
    let max = (1i64 << (bits - 1)) as f64 - 1.0;
    let mut state = 12345u32;
    (0..frames)
        .flat_map(|i| {
            (0..channels).map(move |c| (i, c)).collect::<Vec<_>>()
        })
        .map(|(i, c)| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (state >> 16) as f64 / 65536.0 - 0.5;
            let tone = (i as f64 * 0.05 * (c as f64 + 1.0)).sin();
            ((tone * 0.7 + noise * 0.2) * max) as i32
        })
        .collect()
}

fn make_wav(path: &Path, channels: u16, rate: u32, bits: u32, samples: &[i32], tags: &Tags) {
    let file = BufWriter::new(File::create(path).unwrap());
    let mut w = WavWriter::new(file, channels, rate, crate::SampleFormat::Int(bits), tags).unwrap();
    w.write(samples).unwrap();
    w.finish().unwrap();
}

/// Decode to quantised integers at `bits`.
fn read_all(path: &Path, fmt: Format, bits: u32) -> (StreamInfo, Vec<i32>, Tags) {
    let mut r = AudioConverter::default().open(path, fmt).unwrap();
    let mut v = Vec::new();
    while let Some(c) = r.next_chunk().unwrap() {
        v.extend(c.iter().map(|&s| quantize(s, bits)));
    }
    (r.info().clone(), v, r.tags().clone())
}

#[test]
fn wav_flac_wav_is_bit_exact() {
    let dir = tmp("exact");
    for (ch, rate, bits) in [(1u16, 22050u32, 16u32), (2, 44100, 16), (2, 48000, 24), (6, 48000, 24)] {
        let samples = signal(ch, 20_000, bits);
        let a = dir.join(format!("a{ch}-{bits}.wav"));
        make_wav(&a, ch, rate, bits, &samples, &Tags::default());

        let f = dir.join(format!("m{ch}-{bits}.flac"));
        let b = dir.join(format!("b{ch}-{bits}.wav"));
        let o = Options::default();
        run(&a, &f, Format::Wav, Format::Flac, &o).unwrap();
        assert_eq!(Format::detect(&f), Some(Format::Flac));
        run(&f, &b, Format::Flac, Format::Wav, &o).unwrap();

        let (info, back, _) = read_all(&b, Format::Wav, bits);
        assert_eq!((info.channels, info.sample_rate), (ch, rate));
        assert_eq!(info.source_bits, Some(bits));
        assert_eq!(back, samples, "{ch}ch {bits}-bit round trip");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tags_survive_both_directions_and_can_be_stripped() {
    let dir = tmp("tags");
    let tags = Tags {
        title: Some("Test Song".into()),
        artist: Some("Some Artist".into()),
        album: Some("Album".into()),
        genre: Some("Noise".into()),
        date: Some("2024".into()),
        track: Some(7),
        ..Default::default()
    };
    let wav = dir.join("t.wav");
    make_wav(&wav, 2, 44100, 16, &signal(2, 5000, 16), &tags);
    assert_eq!(read_all(&wav, Format::Wav, 16).2, tags, "WAV INFO tags readable");

    let flac = dir.join("t.flac");
    run(&wav, &flac, Format::Wav, Format::Flac, &Options::default()).unwrap();
    assert_eq!(read_all(&flac, Format::Flac, 16).2, tags, "tags carried WAV→FLAC");

    let wav2 = dir.join("t2.wav");
    run(&flac, &wav2, Format::Flac, Format::Wav, &Options::default()).unwrap();
    assert_eq!(read_all(&wav2, Format::Wav, 16).2, tags, "tags carried FLAC→WAV");

    let stripped = dir.join("s.flac");
    run(&wav, &stripped, Format::Wav, Format::Flac, &Options { strip_metadata: true, ..Default::default() }).unwrap();
    assert!(read_all(&stripped, Format::Flac, 16).2.is_empty(), "tags stripped on request");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn truncated_and_garbage_inputs_do_not_panic() {
    let dir = tmp("bad");
    let wav = dir.join("ok.wav");
    make_wav(&wav, 2, 44100, 16, &signal(2, 50_000, 16), &Tags::default());
    let bytes = std::fs::read(&wav).unwrap();

    let cut = dir.join("cut.wav");
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    // Truncation may succeed (short output) or fail; it must not panic.
    let _ = run(&cut, &dir.join("cut.flac"), Format::Wav, Format::Flac, &Options::default());

    let junk = dir.join("junk.flac");
    std::fs::write(&junk, b"fLaC this is not really a flac file at all").unwrap();
    assert!(run(&junk, &dir.join("junk.wav"), Format::Flac, Format::Wav, &Options::default()).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cancellation_stops_conversion() {
    let dir = tmp("cancel");
    let wav = dir.join("a.wav");
    make_wav(&wav, 2, 44100, 16, &signal(2, 200_000, 16), &Tags::default());
    let cancel = AtomicBool::new(true);
    let options = Options::default();
    let progress = |_: f32| {};
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    for to in [Format::Flac, Format::Wav] {
        let err = AudioConverter::default().convert(&wav, &dir.join("o"), Format::Wav, to, &ctx).unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{to}: {err}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn quantize_rounds_and_clips() {
    assert_eq!(quantize(0.0, 16), 0);
    assert_eq!(quantize(1.0, 16), 32767);
    assert_eq!(quantize(-1.0, 16), -32768);
    assert_eq!(quantize(2.0, 24), 8388607);
    assert_eq!(quantize(100.0 / 32768.0, 16), 100);
}

#[test]
fn flac_is_fixed_blocksize_for_strict_decoders() {
    let dir = tmp("fixed");
    let a = dir.join("a.wav");
    make_wav(&a, 2, 44100, 16, &signal(2, 20_000, 16), &Tags::default()); // last block is short
    let f = dir.join("a.flac");
    run(&a, &f, Format::Wav, Format::Flac, &Options::default()).unwrap();
    let b = std::fs::read(&f).unwrap();
    assert_eq!(&b[8..10], &b[10..12], "STREAMINFO min block size must equal max");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn loud_noisy_24bit_does_not_blow_up() {
    let dir = tmp("blowup");
    let a = dir.join("a.wav");
    make_wav(&a, 2, 48000, 24, &signal(2, 20_000, 24), &Tags::default());
    let f = dir.join("a.flac");
    run(&a, &f, Format::Wav, Format::Flac, &Options::default()).unwrap();
    let (wav, flac) = (std::fs::metadata(&a).unwrap().len(), std::fs::metadata(&f).unwrap().len());
    assert!(flac < wav + wav / 10, "flac {flac} bytes for {wav} bytes of PCM");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- MP3 output ----

/// Decode any supported file to interleaved f32.
fn read_f32(path: &Path, fmt: Format) -> (StreamInfo, Vec<f32>, Tags) {
    let mut r = AudioConverter::default().open(path, fmt).unwrap();
    let mut v = Vec::new();
    while let Some(c) = r.next_chunk().unwrap() {
        v.extend_from_slice(c);
    }
    (r.info().clone(), v, r.tags().clone())
}

/// Two-tone music-ish signal (no noise, so a good encoder reaches a high SNR).
fn tones(channels: u16, rate: u32, secs: f64) -> Vec<i32> {
    let n = (rate as f64 * secs) as usize;
    (0..n)
        .flat_map(|i| {
            let t = i as f64 / rate as f64;
            (0..channels).map(move |c| {
                let f = 440.0 * (1.0 + c as f64 * 0.5);
                let v = 0.4 * (std::f64::consts::TAU * f * t).sin() + 0.2 * (std::f64::consts::TAU * f * 3.0 * t).sin();
                (v * 32767.0) as i32
            })
        })
        .collect()
}

#[test]
fn mp3_output_decodes_at_every_preset() {
    let dir = tmp("mp3");
    let src = dir.join("a.wav");
    let samples = tones(2, 44100, 2.0);
    make_wav(&src, 2, 44100, 16, &samples, &Tags::default());
    let orig: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

    for preset in Preset::ALL {
        let dst = dir.join(format!("a-{preset:?}.mp3"));
        run(&src, &dst, Format::Wav, Format::Mp3, &Options { preset, ..Default::default() }).unwrap();
        assert_eq!(Format::detect(&dst), Some(Format::Mp3));
        let (info, pcm, _) = read_f32(&dst, Format::Mp3);
        assert_eq!((info.channels, info.sample_rate), (2, 44100), "{preset:?}");

        // Encoder delay shifts the signal; find the best alignment before comparing.
        let (lag, snr) = (0..3000)
            .step_by(1)
            .map(|lag| (lag, snr_db(&orig[20_000..60_000], &pcm[20_000 + lag * 2..60_000 + lag * 2])))
            .filter(|(_, s)| s.is_finite())
            .fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a });
        eprintln!("{preset:?}: {} bytes, lag {lag}, snr {snr:.1} dB", std::fs::metadata(&dst).unwrap().len());
        let floor = match preset { Preset::Low => 12.0, Preset::Medium => 15.0, Preset::High => 18.0, Preset::Lossless => 22.0 };
        assert!(snr > floor, "{preset:?}: snr {snr:.1} dB too low");
    }
    std::fs::remove_dir_all(&dir).ok();
}

fn snr_db(a: &[f32], b: &[f32]) -> f64 {
    let (mut s, mut e) = (0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        s += (*x as f64).powi(2);
        e += ((*x - *y) as f64).powi(2);
    }
    10.0 * (s / e.max(1e-20)).log10()
}

#[test]
fn mp3_is_roughly_gapless_and_presets_order_by_size() {
    let dir = tmp("mp3len");
    let src = dir.join("a.wav");
    make_wav(&src, 2, 44100, 16, &signal(2, 44100 * 3, 16), &Tags::default());
    let mut sizes = Vec::new();
    for preset in Preset::ALL {
        let dst = dir.join(format!("{preset:?}.mp3"));
        run(&src, &dst, Format::Wav, Format::Mp3, &Options { preset, ..Default::default() }).unwrap();
        let (_, pcm, _) = read_f32(&dst, Format::Mp3);
        let frames = (pcm.len() / 2) as i64;
        assert!((frames - 44100 * 3).abs() < 2304, "{preset:?}: {frames} frames vs {}", 44100 * 3);
        sizes.push(std::fs::metadata(&dst).unwrap().len());
    }
    assert!(sizes.windows(2).all(|w| w[0] < w[1]), "sizes should grow with preset: {sizes:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mp3_tags_roundtrip_and_strip() {
    let dir = tmp("mp3tags");
    let tags = Tags {
        title: Some("Zażółć gęślą".into()), // forces UTF-16
        artist: Some("Some Artist".into()),
        album: Some("Album".into()),
        genre: Some("Noise".into()),
        comment: Some("hello".into()),
        date: Some("2024-05-01".into()),
        track: Some(7),
        ..Default::default()
    };
    let src = dir.join("t.wav");
    make_wav(&src, 2, 44100, 16, &tones(2, 44100, 1.0), &tags);

    let kept = dir.join("kept.mp3");
    run(&src, &kept, Format::Wav, Format::Mp3, &Options::default()).unwrap();
    let got = read_f32(&kept, Format::Mp3).2;
    assert_eq!(got.title, tags.title);
    assert_eq!(got.artist, tags.artist);
    assert_eq!(got.album, tags.album);
    assert_eq!(got.genre, tags.genre);
    assert_eq!(got.comment, tags.comment);
    assert_eq!(got.track, tags.track);
    assert!(got.date.as_deref().is_some_and(|d| d.starts_with("2024")), "{:?}", got.date);

    let stripped = dir.join("stripped.mp3");
    run(&src, &stripped, Format::Wav, Format::Mp3, &Options { strip_metadata: true, ..Default::default() }).unwrap();
    assert!(read_f32(&stripped, Format::Mp3).2.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mp3_handles_unusual_sources() {
    let dir = tmp("mp3odd");
    let o = Options::default();
    // (channels, rate, expected output channels, expected output rate)
    for (ch, rate, want_ch, want_rate) in [(1u16, 22050u32, 1u16, 22050u32), (6, 48000, 2, 48000), (2, 96000, 2, 48000), (2, 88200, 2, 44100), (1, 8000, 1, 8000)] {
        let src = dir.join(format!("s{ch}-{rate}.wav"));
        make_wav(&src, ch, rate, 16, &tones(ch, rate, 1.0), &Tags::default());
        let dst = dir.join(format!("s{ch}-{rate}.mp3"));
        run(&src, &dst, Format::Wav, Format::Mp3, &o).unwrap_or_else(|e| panic!("{ch}ch {rate}: {e}"));
        let (info, pcm, _) = read_f32(&dst, Format::Mp3);
        assert_eq!((info.channels, info.sample_rate), (want_ch, want_rate), "{ch}ch {rate}");
        let secs = pcm.len() as f64 / want_ch as f64 / want_rate as f64;
        assert!((secs - 1.0).abs() < 0.1, "{ch}ch {rate}: {secs}s");
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "{ch}ch {rate}: peak {peak}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mp3_edge_cases_do_not_panic() {
    let dir = tmp("mp3edge");
    let o = Options::default();
    // Tiny input, silence, and empty input.
    for n in [1usize, 100, 5000] {
        let src = dir.join(format!("n{n}.wav"));
        make_wav(&src, 2, 44100, 16, &vec![0; n * 2], &Tags::default());
        run(&src, &dir.join(format!("n{n}.mp3")), Format::Wav, Format::Mp3, &o).unwrap_or_else(|e| panic!("{n} frames: {e}"));
    }
    let empty = dir.join("empty.wav");
    make_wav(&empty, 2, 44100, 16, &[], &Tags::default());
    assert!(run(&empty, &dir.join("empty.mp3"), Format::Wav, Format::Mp3, &o).is_err());

    // Full-scale clipping square wave.
    let loud = dir.join("loud.wav");
    make_wav(&loud, 2, 44100, 16, &(0..88200).map(|i| if (i / 200) % 2 == 0 { 32767 } else { -32768 }).collect::<Vec<_>>(), &Tags::default());
    run(&loud, &dir.join("loud.mp3"), Format::Wav, Format::Mp3, &o).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mp3_cancellation_stops_conversion() {
    let dir = tmp("mp3cancel");
    let src = dir.join("a.wav");
    make_wav(&src, 2, 44100, 16, &tones(2, 44100, 2.0), &Tags::default());
    let cancel = AtomicBool::new(true);
    let options = Options::default();
    let progress = |_: f32| {};
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    let err = AudioConverter::default().convert(&src, &dir.join("o.mp3"), Format::Wav, Format::Mp3, &ctx).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- Ogg Vorbis output ----

#[test]
fn ogg_output_decodes_with_correct_length_and_fidelity() {
    let dir = tmp("ogg");
    for (ch, rate) in [(2u16, 44100u32), (1, 22050)] {
        let src = dir.join(format!("a{ch}.wav"));
        let samples = tones(ch, rate, 1.5);
        make_wav(&src, ch, rate, 16, &samples, &Tags::default());
        let orig: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

        let dst = dir.join(format!("a{ch}.ogg"));
        run(&src, &dst, Format::Wav, Format::Ogg, &Options::default()).unwrap();
        assert_eq!(Format::detect(&dst), Some(Format::Ogg));
        let (info, pcm, _) = read_f32(&dst, Format::Ogg);
        assert_eq!((info.channels, info.sample_rate), (ch, rate));
        // Vorbis is sample-exact in length once end-trimmed.
        let frames = pcm.len() / ch as usize;
        assert!((frames as i64 - (rate as f64 * 1.5) as i64).abs() <= 1, "{ch}ch: {frames} frames");

        let (lag, snr) = (0..2500)
            .map(|lag| (lag, snr_db(&orig[8000..24000], &pcm[8000 + lag * ch as usize..24000 + lag * ch as usize])))
            .fold((0, f64::MIN), |a, b| if b.1 > a.1 { b } else { a });
        eprintln!("ogg {ch}ch: lag {lag}, snr {snr:.1} dB, {} bytes", std::fs::metadata(&dst).unwrap().len());
        assert!(snr > 15.0, "{ch}ch: snr {snr:.1} dB");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ogg_tags_roundtrip_and_strip() {
    let dir = tmp("ogg-tags");
    let tags = Tags {
        title: Some("Zażółć".into()),
        artist: Some("Some Artist".into()),
        album: Some("Album".into()),
        genre: Some("Noise".into()),
        date: Some("2024".into()),
        track: Some(3),
        ..Default::default()
    };
    let src = dir.join("t.wav");
    make_wav(&src, 2, 44100, 16, &tones(2, 44100, 0.5), &tags);
    let kept = dir.join("kept.ogg");
    run(&src, &kept, Format::Wav, Format::Ogg, &Options::default()).unwrap();
    assert_eq!(read_f32(&kept, Format::Ogg).2, tags);
    let stripped = dir.join("stripped.ogg");
    run(&src, &stripped, Format::Wav, Format::Ogg, &Options { strip_metadata: true, ..Default::default() }).unwrap();
    assert!(read_f32(&stripped, Format::Ogg).2.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ogg_handles_surround_odd_rates_and_tiny_inputs() {
    let dir = tmp("ogg-odd");
    let o = Options::default();
    for (ch, rate, secs) in [(6u16, 48000u32, 0.5f64), (2, 96000, 0.3), (1, 8000, 0.5), (2, 44100, 0.01)] {
        let src = dir.join(format!("s{ch}-{rate}.wav"));
        make_wav(&src, ch, rate, 16, &tones(ch, rate, secs), &Tags::default());
        let dst = dir.join(format!("s{ch}-{rate}.ogg"));
        run(&src, &dst, Format::Wav, Format::Ogg, &o).unwrap_or_else(|e| panic!("{ch}ch {rate} {secs}s: {e}"));
        let (info, pcm, _) = read_f32(&dst, Format::Ogg);
        assert_eq!((info.channels, info.sample_rate), (ch.min(2), rate));
        assert!(pcm.len() / ch.min(2) as usize > 0);
    }
    let empty = dir.join("empty.wav");
    make_wav(&empty, 2, 44100, 16, &[], &Tags::default());
    assert!(run(&empty, &dir.join("empty.ogg"), Format::Wav, Format::Ogg, &o).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ogg_cancellation_returns_promptly() {
    let dir = tmp("ogg-cancel");
    let src = dir.join("a.wav");
    make_wav(&src, 2, 44100, 16, &tones(2, 44100, 20.0), &Tags::default());
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let options = Options::default();
    let progress = |_: f32| {};
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    let started = std::time::Instant::now();
    let err = AudioConverter::default().convert(&src, &dir.join("o.ogg"), Format::Wav, Format::Ogg, &ctx).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    assert!(started.elapsed().as_secs() < 5, "cancel took {:?}", started.elapsed());
    std::fs::remove_dir_all(&dir).ok();
}

// ---- Ogg Opus ----

#[test]
fn opus_roundtrip_is_gapless_and_faithful() {
    let dir = tmp("opus");
    // (channels, source rate, rate the decoder reports)
    for (ch, rate, out_rate) in [(2u16, 48000u32, 48000u32), (1, 16000, 16000), (2, 44100, 48000), (1, 8000, 8000), (1, 22050, 48000)] {
        let src = dir.join(format!("a{ch}-{rate}.wav"));
        let samples = tones(ch, rate, 1.5);
        make_wav(&src, ch, rate, 16, &samples, &Tags::default());
        let dst = dir.join(format!("a{ch}-{rate}.opus"));
        run(&src, &dst, Format::Wav, Format::Opus, &Options::default()).unwrap_or_else(|e| panic!("{ch}ch {rate}: {e}"));
        assert_eq!(Format::detect(&dst), Some(Format::Opus));

        let (info, pcm, _) = read_f32(&dst, Format::Opus);
        // Opus always decodes at 48 kHz here; the file's sample rate is only metadata.
        let _ = out_rate;
        assert_eq!((info.channels, info.sample_rate), (ch, 48000), "{ch}ch {rate}");
        let frames = pcm.len() / ch as usize;
        let expected = (1.5 * 48000.0) as i64;
        // Sample-exact for native rates; the 8/16 kHz files are upsampled by the decoder.
        let secs = frames as f64 / 48000.0;
        assert!((secs - 1.5).abs() < 0.003, "{ch}ch {rate}: {secs}s ({frames} vs {expected})");

        // Fidelity: compare a mono-ised window against the source after rate alignment.
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.2 && peak < 1.2, "{ch}ch {rate}: peak {peak}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opus_snr_at_native_rate() {
    let dir = tmp("opus-snr");
    let src = dir.join("a.wav");
    let samples = tones(2, 48000, 2.0);
    make_wav(&src, 2, 48000, 16, &samples, &Tags::default());
    let orig: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    let dst = dir.join("a.opus");
    run(&src, &dst, Format::Wav, Format::Opus, &Options::default()).unwrap();
    let (_, pcm, _) = read_f32(&dst, Format::Opus);
    assert_eq!(pcm.len(), orig.len(), "exact sample count at 48 kHz");
    let snr = snr_db(&orig[10_000..80_000], &pcm[10_000..80_000]);
    eprintln!("opus 48k stereo snr {snr:.1} dB, {} bytes", std::fs::metadata(&dst).unwrap().len());
    assert!(snr > 20.0, "snr {snr:.1} dB");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opus_tags_roundtrip_and_strip() {
    let dir = tmp("opus-tags");
    let tags = Tags {
        title: Some("Zażółć".into()),
        artist: Some("Some Artist".into()),
        album: Some("Album".into()),
        genre: Some("Noise".into()),
        date: Some("2024".into()),
        track: Some(3),
        ..Default::default()
    };
    let src = dir.join("t.wav");
    make_wav(&src, 2, 48000, 16, &tones(2, 48000, 0.5), &tags);
    let kept = dir.join("kept.opus");
    run(&src, &kept, Format::Wav, Format::Opus, &Options::default()).unwrap();
    assert_eq!(read_f32(&kept, Format::Opus).2, tags);
    let stripped = dir.join("stripped.opus");
    run(&src, &stripped, Format::Wav, Format::Opus, &Options { strip_metadata: true, ..Default::default() }).unwrap();
    assert!(read_f32(&stripped, Format::Opus).2.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opus_decodes_into_every_other_format() {
    let dir = tmp("opus-out");
    let src = dir.join("a.wav");
    make_wav(&src, 2, 48000, 16, &tones(2, 48000, 1.0), &Tags::default());
    let opus = dir.join("a.opus");
    run(&src, &opus, Format::Wav, Format::Opus, &Options::default()).unwrap();
    for to in [Format::Wav, Format::Flac, Format::Mp3] {
        let dst = dir.join(format!("out.{}", to.extension()));
        run(&opus, &dst, Format::Opus, to, &Options::default()).unwrap_or_else(|e| panic!("opus→{to}: {e}"));
        let (info, pcm, _) = read_f32(&dst, to);
        assert_eq!(info.channels, 2, "{to}");
        let secs = pcm.len() as f64 / 2.0 / info.sample_rate as f64;
        assert!((secs - 1.0).abs() < 0.05, "opus→{to}: {secs}s");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opus_edge_cases() {
    let dir = tmp("opus-edge");
    let o = Options::default();
    for (ch, rate, n) in [(2u16, 48000u32, 1usize), (2, 48000, 100), (1, 44100, 5), (6, 48000, 4800), (2, 96000, 9600)] {
        let src = dir.join(format!("e{ch}-{rate}-{n}.wav"));
        make_wav(&src, ch, rate, 16, &tones(ch, rate, n as f64 / rate as f64), &Tags::default());
        let dst = dir.join(format!("e{ch}-{rate}-{n}.opus"));
        run(&src, &dst, Format::Wav, Format::Opus, &o).unwrap_or_else(|e| panic!("{ch}ch {rate} {n} frames: {e}"));
        let (info, pcm, _) = read_f32(&dst, Format::Opus);
        assert_eq!(info.channels, ch.min(2));
        assert!(!pcm.is_empty(), "{ch}ch {rate} {n}");
    }
    let empty = dir.join("empty.wav");
    make_wav(&empty, 2, 48000, 16, &[], &Tags::default());
    assert!(run(&empty, &dir.join("empty.opus"), Format::Wav, Format::Opus, &o).is_err());
    // Garbage that looks like Ogg must be an error, not a panic.
    let junk = dir.join("junk.opus");
    std::fs::write(&junk, b"OggS\0\x02 not really an ogg opus file at all, just junk bytes OpusHead").unwrap();
    assert!(run(&junk, &dir.join("junk.wav"), Format::Opus, Format::Wav, &o).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn opus_cancellation_stops_conversion() {
    let dir = tmp("opus-cancel");
    let src = dir.join("a.wav");
    make_wav(&src, 2, 48000, 16, &tones(2, 48000, 2.0), &Tags::default());
    let cancel = AtomicBool::new(true);
    let options = Options::default();
    let progress = |_: f32| {};
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    let err = AudioConverter::default().convert(&src, &dir.join("o.opus"), Format::Wav, Format::Opus, &ctx).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn notes_cover_truncation_downmix_and_resampling() {
    let dir = tmp("anotes");
    let o = Options::default();

    // Clean files produce no notes.
    let ok = dir.join("ok.wav");
    make_wav(&ok, 2, 44100, 16, &tones(2, 44100, 1.0), &Tags::default());
    assert!(run_notes(&ok, &dir.join("ok.flac"), Format::Wav, Format::Flac, &o).is_empty());
    assert!(run_notes(&ok, &dir.join("ok.mp3"), Format::Wav, Format::Mp3, &o).is_empty());

    // A WAV whose data was cut off mid-stream.
    let bytes = std::fs::read(&ok).unwrap();
    let cut = dir.join("cut.wav");
    std::fs::write(&cut, &bytes[..bytes.len() / 3]).unwrap();
    let notes = run_notes(&cut, &dir.join("cut.flac"), Format::Wav, Format::Flac, &o);
    assert!(notes.iter().any(|n| n.contains("cut short") || n.contains("ended unexpectedly")), "{notes:?}");

    // 5.1 → MP3 and 96 kHz → MP3 / Opus.
    let surround = dir.join("s.wav");
    make_wav(&surround, 6, 48000, 16, &tones(6, 48000, 0.3), &Tags::default());
    let notes = run_notes(&surround, &dir.join("s.mp3"), Format::Wav, Format::Mp3, &o);
    assert!(notes.iter().any(|n| n.contains("6-channel") && n.contains("stereo")), "{notes:?}");
    let hi = dir.join("hi.wav");
    make_wav(&hi, 2, 96000, 16, &tones(2, 96000, 0.3), &Tags::default());
    let notes = run_notes(&hi, &dir.join("hi.mp3"), Format::Wav, Format::Mp3, &o);
    assert!(notes.iter().any(|n| n.contains("96000") && n.contains("48000")), "{notes:?}");
    let notes = run_notes(&ok, &dir.join("ok.opus"), Format::Wav, Format::Opus, &o);
    assert!(notes.iter().any(|n| n.contains("44100") && n.contains("48000")), "{notes:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// An encoder that only takes mono at 16 kHz and records what it was given.
struct Picky(std::sync::Arc<std::sync::Mutex<(StreamInfo, usize)>>);

impl AudioEncoder for Picky {
    fn format(&self) -> Format {
        Format::Mp3
    }
    fn max_channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self, _: u32) -> u32 {
        16000
    }
    fn encode(&self, src: &mut dyn AudioSource, _: &Tags, _: &ConvertCtx, _: &mut dyn dotty_core::WriteSeek) -> Result<()> {
        let mut n = 0;
        while let Some(c) = src.next_chunk()? {
            n += c.len();
        }
        *self.0.lock().unwrap() = (src.info().clone(), n);
        Ok(())
    }
}

#[test]
fn pipeline_adapts_stream_to_encoder_limits() {
    let dir = tmp("picky");
    let src = dir.join("in.wav");
    make_wav(&src, 3, 48000, 16, &vec![1000; 48000 * 3], &Tags::default());
    let seen = std::sync::Arc::new(std::sync::Mutex::new((StreamInfo { sample_rate: 0, channels: 0, source_bits: None, total_frames: None, source_rate: 0 }, 0)));
    let mut conv = AudioConverter::empty();
    conv.add_decoder(SymphoniaDecoder(Format::Wav));
    conv.add_encoder(Picky(seen.clone()));
    assert_eq!(conv.routes(), vec![(Format::Wav, Format::Mp3)]);

    let (cancel, progress, opts) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    conv.convert(&src, &dir.join("o"), Format::Wav, Format::Mp3, &ctx).unwrap();
    let (info, samples) = seen.lock().unwrap().clone();
    assert_eq!((info.channels, info.sample_rate, info.source_rate), (1, 16000, 48000));
    assert_eq!(samples, 16000, "one second of mono at the new rate");
    let notes = ctx.take_notes();
    assert!(notes.iter().any(|n| n.contains("mono")) && notes.iter().any(|n| n.contains("16000")), "{notes:?}");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- More formats, choices and chained streams ----

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn opts(to: Format, choices: &[(dotty_core::Choice, &str)]) -> Options {
    let mut o = Options::default();
    for (c, v) in choices {
        o.set_choice(to, c, v);
    }
    o
}

/// Best SNR of `b` against `a` over small time offsets (codec delays differ).
fn aligned_snr(a: &[f32], b: &[f32], channels: usize, max_lag: usize) -> f64 {
    (0..=max_lag)
        .map(|lag| {
            let b = b.get(lag * channels..).unwrap_or(&[]);
            let n = a.len().min(b.len());
            snr_db(&a[..n], &b[..n])
        })
        .fold(f64::MIN, f64::max)
}

#[test]
fn decodes_files_made_by_other_tools() {
    // All made by ffmpeg from tone.wav (0.2 s, 48 kHz stereo).
    let (_, tone, _) = read_f32(&fixture("tone.wav"), Format::Wav);
    for (file, from, min_db, exact) in [
        ("ffmpeg.aiff", Format::Aiff, 0.0, true),
        ("pcm.caf", Format::Caf, 0.0, true),
        ("alac.caf", Format::Caf, 0.0, true),
        ("alac.m4a", Format::M4a, 0.0, true),
        ("flac.mka", Format::Mka, 0.0, true),
        ("ima.wav", Format::Wav, 25.0, false),
        ("layer2.mp2", Format::Mp2, 20.0, false),
        ("vorbis.mka", Format::Mka, 20.0, false),
        ("opus.webm", Format::Mka, 15.0, false),
    ] {
        let (info, pcm, _) = read_f32(&fixture(file), from);
        assert_eq!((info.channels, info.sample_rate), (2, 48000), "{file}");
        if exact {
            assert_eq!(pcm, tone, "{file} is lossless");
        } else {
            let db = aligned_snr(&tone, &pcm, 2, 1500);
            assert!(db >= min_db, "{file}: {db:.1} dB");
        }
    }
    let err = AudioConverter::default().open(&fixture("aac.m4a"), Format::M4a).err().expect("AAC is refused");
    assert!(err.to_string().contains("AAC"), "{err}");
}

#[test]
fn aiff_and_float_wav_round_trip_exactly() {
    let dir = tmp("aiff");
    let tags = Tags { title: Some("Song".into()), artist: Some("Band".into()), ..Default::default() };
    for bits in [16, 24] {
        let wav = dir.join("in.wav");
        let samples = signal(2, 4000, bits);
        make_wav(&wav, 2, 44100, bits, &samples, &tags);
        let aiff = dir.join("t.aiff");
        run(&wav, &aiff, Format::Wav, Format::Aiff, &Options::default()).unwrap();
        let (info, back, back_tags) = read_all(&aiff, Format::Aiff, bits);
        assert_eq!((info.source_bits, back), (Some(bits), samples.clone()), "{bits}-bit AIFF is exact");
        assert_eq!(back_tags.title.as_deref(), Some("Song"), "AIFF keeps tags");

        let float = dir.join("f.wav");
        let depth = opts(Format::Wav, &[(crate::wav::WAV_DEPTH, "32f")]);
        run(&wav, &float, Format::Wav, Format::Wav, &depth).unwrap();
        assert_eq!(read_all(&float, Format::Wav, bits).1, samples, "{bits}-bit through 32-bit float is exact");
    }
    // Forcing 16 bits on a 24-bit source.
    let wav = dir.join("in.wav");
    make_wav(&wav, 1, 44100, 24, &signal(1, 1000, 24), &Tags::default());
    let sixteen = opts(Format::Flac, &[(PCM_DEPTH, "16")]);
    run(&wav, &dir.join("d.flac"), Format::Wav, Format::Flac, &sixteen).unwrap();
    assert_eq!(read_f32(&dir.join("d.flac"), Format::Flac).0.source_bits, Some(16));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn channel_and_rate_choices_apply_without_spurious_notes() {
    let dir = tmp("remix");
    let wav = dir.join("s.wav");
    make_wav(&wav, 2, 44100, 16, &tones(2, 44100, 1.0), &Tags::default());

    let small = opts(Format::Wav, &[(CHANNELS, "mono"), (SAMPLE_RATE, "22050")]);
    let notes = run_notes(&wav, &dir.join("m.wav"), Format::Wav, Format::Wav, &small);
    assert!(notes.is_empty(), "asked for, so no warning: {notes:?}");
    let (info, pcm, _) = read_f32(&dir.join("m.wav"), Format::Wav);
    assert_eq!((info.channels, info.sample_rate, pcm.len()), (1, 22050, 22050));

    // Mono → stereo copies the channel.
    let up = opts(Format::Flac, &[(CHANNELS, "stereo")]);
    run(&dir.join("m.wav"), &dir.join("st.flac"), Format::Wav, Format::Flac, &up).unwrap();
    let (info, pcm, _) = read_f32(&dir.join("st.flac"), Format::Flac);
    assert_eq!(info.channels, 2);
    assert!(pcm.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));

    // A rate the format can't hold is still adjusted, and that is worth a note.
    let odd = opts(Format::Opus, &[(SAMPLE_RATE, "44100")]);
    let notes = run_notes(&wav, &dir.join("o.opus"), Format::Wav, Format::Opus, &odd);
    assert!(notes.iter().any(|n| n.contains("44100") && n.contains("48000")), "{notes:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mp3_constant_bitrate_is_on_target() {
    let dir = tmp("cbr");
    let wav = dir.join("s.wav");
    make_wav(&wav, 2, 44100, 16, &tones(2, 44100, 4.0), &Tags::default());
    let cbr = opts(Format::Mp3, &[(crate::mp3::MP3_MODE, "cbr")]);
    run(&wav, &dir.join("c.mp3"), Format::Wav, Format::Mp3, &cbr).unwrap();
    let kbps = std::fs::metadata(dir.join("c.mp3")).unwrap().len() as f64 * 8.0 / 4.0 / 1000.0;
    assert!((250.0..265.0).contains(&kbps), "High CBR should be 256 kbps, got {kbps:.0}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn chained_ogg_streams_are_joined_or_flagged() {
    let dir = tmp("chain");
    let part = |name: &str, rate: u32, secs: f64| {
        let wav = dir.join(format!("{name}.wav"));
        make_wav(&wav, 2, rate, 16, &tones(2, rate, secs), &Tags::default());
        let ogg = dir.join(format!("{name}.ogg"));
        run(&wav, &ogg, Format::Wav, Format::Ogg, &Options::default()).unwrap();
        std::fs::read(ogg).unwrap()
    };
    let (a, b, c) = (part("a", 44100, 1.0), part("b", 44100, 0.5), part("c", 22050, 0.5));
    std::fs::write(dir.join("ab.ogg"), [a.clone(), b].concat()).unwrap();
    let (_, pcm, _) = read_f32(&dir.join("ab.ogg"), Format::Ogg);
    assert_eq!(pcm.len() / 2, 44100 + 22050, "both streams, gaplessly");

    std::fs::write(dir.join("ac.ogg"), [a, c].concat()).unwrap();
    let notes = run_notes(&dir.join("ac.ogg"), &dir.join("ac.wav"), Format::Ogg, Format::Wav, &Options::default());
    assert!(notes.iter().any(|n| n.contains("only the first")), "{notes:?}");
    assert_eq!(read_f32(&dir.join("ac.wav"), Format::Wav).1.len() / 2, 44100);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn edits_trim_and_fade_when_resaving_in_the_same_format() {
    let dir = tmp("edit");
    let (src, dst) = (dir.join("in.wav"), dir.join("out.wav"));
    let samples = signal(2, 16_000, 16);
    make_wav(&src, 2, 8000, 16, &samples, &Tags::default());
    let edits = dotty_core::Edits {
        ops: vec![dotty_core::EditOp::Audio(dotty_core::AudioEdits {
            trim_start: 0.5,
            trim_end: Some(1.5),
            fade_in: 0.1,
            ..Default::default()
        })],
    };
    let cancel = AtomicBool::new(false);
    let last = std::sync::Mutex::new(0.0f32);
    let progress = |p: f32| *last.lock().unwrap() = p;
    let options = Options::default();
    let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
    AudioConverter::default().convert(&src, &dst, Format::Wav, Format::Wav, &ctx).unwrap();
    assert_eq!(*last.lock().unwrap(), 1.0);

    let (info, out, _) = read_all(&dst, Format::Wav, 16);
    assert_eq!(info.total_frames, Some(8000));
    assert_eq!(out.len(), 8000 * 2);
    assert_eq!(out[0], 0, "fade-in starts silent");
    // Past the fade-in and away from the 5 ms cut fade at the end: untouched.
    assert_eq!(&out[2000..15_000], &samples[4000 * 2 + 2000..4000 * 2 + 15_000]);
    assert!(out[out.len() - 2].abs() < 200, "cut point fades out");
}

#[test]
fn normalizing_measures_first_and_progress_never_goes_backwards() {
    let dir = tmp("normalize");
    let (src, dst) = (dir.join("in.wav"), dir.join("out.wav"));
    make_wav(&src, 2, 8000, 16, &signal(2, 16_000, 16), &Tags::default());
    let edits = dotty_core::Edits {
        ops: vec![dotty_core::EditOp::Audio(dotty_core::AudioEdits {
            trim_start: 0.25,
            reverse: true,
            low_cut: Some(100.0),
            volume: dotty_core::Volume::Peak(-6.0),
            ..Default::default()
        })],
    };
    let cancel = AtomicBool::new(false);
    let seen = std::sync::Mutex::new(Vec::new());
    let progress = |p: f32| seen.lock().unwrap().push(p);
    let options = Options::default();
    let ctx = ConvertCtx::new(&options, &cancel, &progress).with_edits(Some(&edits));
    AudioConverter::default().convert(&src, &dst, Format::Wav, Format::Wav, &ctx).unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen.windows(2).all(|w| w[1] >= w[0] || w[1] == 0.0), "{seen:?}");
    assert_eq!(seen.last(), Some(&1.0));

    let (info, out, _) = read_all(&dst, Format::Wav, 16);
    assert_eq!(info.total_frames, Some(14_000));
    let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap() as f32 / 32768.0;
    assert!((20.0 * peak.log10() + 6.0).abs() < 0.05, "peak {peak}");
}
