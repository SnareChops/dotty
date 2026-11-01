//! Converts real-world samples in `testdata/audio` and checks the result decodes
//! with the same rate/channels/length. Skipped when the folder is absent; fetch it
//! with `scripts/fetch-testdata.sh`.

use codecs_audio::{AudioConverter, AudioSource};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use transformer_core::{ConvertCtx, Converter, Format, Options};

fn frames(path: &std::path::Path, fmt: Format) -> (u32, u16, u64) {
    let mut r = AudioConverter::default().open(path, fmt).unwrap();
    let (rate, channels) = (r.info().sample_rate, r.info().channels);
    let mut n = 0u64;
    while let Some(c) = r.next_chunk().unwrap() {
        n += (c.len() / channels as usize) as u64;
    }
    (rate, channels, n)
}

#[test]
fn converts_all_sample_audio_and_preserves_length() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/audio");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("testdata/audio missing; run scripts/fetch-testdata.sh");
        return;
    };
    let out = std::env::temp_dir().join(format!("tf-audio-testdata-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let (cancel, progress, options) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    let ctx = ConvertCtx::new(&options, &cancel, &progress);

    let mut failures = Vec::new();
    let (mut converted, mut skipped) = (0, 0);
    for entry in entries.flatten() {
        let src = entry.path();
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let from = Format::detect(&src).expect("format detected");
        let targets: Vec<_> = AudioConverter::default().routes().into_iter().filter(|r| r.0 == from).map(|r| r.1).collect();
        if targets.is_empty() {
            skipped += 1; // no decoder yet (e.g. Opus), covered in M3
            continue;
        }
        let expected = frames(&src, from);
        for to in targets {
            let dst = out.join(format!("{name}.{}", to.extension()));
            if let Err(e) = AudioConverter::default().convert(&src, &dst, from, to, &ctx) {
                failures.push(format!("{name} → {to}: {e}"));
                continue;
            }
            converted += 1;
            if Format::detect(&dst) != Some(to) {
                failures.push(format!("{name} → {to}: output not recognised"));
                continue;
            }
            let got = frames(&dst, to);
            // Opus always decodes at 48 kHz, so its length scales with the rate change.
            let want = if to == Format::Opus {
                (48000, expected.1, (expected.2 as f64 * 48000.0 / expected.0 as f64).round() as u64)
            } else {
                expected
            };
            let off = got.2.abs_diff(want.2);
            if (got.0, got.1) != (want.0, want.1) || off > 2 {
                failures.push(format!("{name} → {to}: expected {want:?}, got {got:?}"));
            }
        }
    }
    std::fs::remove_dir_all(&out).ok();
    eprintln!("{converted} conversions, {skipped} files without a decoder yet");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
