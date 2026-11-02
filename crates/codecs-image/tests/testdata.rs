//! Converts every real-world sample in `testdata/image` to every other format.
//! Skipped when the folder is absent; fetch it with `scripts/fetch-testdata.sh`.

use codecs_image::ImageConverter;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use dotty_core::{ConvertCtx, Converter, Format, Options, Preset};

/// Quick pass: low quality only, no big sources, and AVIF (a slow encoder: seconds per
/// picture even when small) only from the tiniest samples.
#[test]
fn converts_sample_images_to_all_formats_quickly() {
    converts_samples(true);
}

/// Everything: both quality presets, every sample, every format. Takes minutes.
/// Run with `cargo test -p codecs-image --test testdata -- --ignored`.
#[test]
#[ignore = "slow (minutes): AVIF encoding of every sample"]
fn converts_all_sample_images_to_all_formats() {
    converts_samples(false);
}

fn converts_samples(fast: bool) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/image");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("testdata/image missing; run scripts/fetch-testdata.sh");
        return;
    };
    let out = std::env::temp_dir().join(format!("tf-testdata-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();

    let converter = ImageConverter::default();
    let cancel = AtomicBool::new(false);
    let progress = |_: f32| {};
    let mut failures = Vec::new();
    let mut count = 0;

    for entry in entries.flatten() {
        let src = entry.path();
        let Some(from) = Format::detect(&src) else {
            failures.push(format!("{}: format not detected", src.display()));
            continue;
        };
        let size = std::fs::metadata(&src).map_or(0, |m| m.len());
        if fast && size > 400_000 {
            continue;
        }
        let presets: &[Preset] = if fast { &[Preset::Low] } else { &[Preset::Low, Preset::Lossless] };
        for &preset in presets {
            let options = Options { preset, ..Default::default() };
            let ctx = ConvertCtx::new(&options, &cancel, &progress);
            for (a, to) in converter.routes() {
                if a != from || (fast && to == Format::Avif && size > 10_000) {
                    continue;
                }
                let dst = out.join(format!("{}-{preset:?}.{}", src.file_name().unwrap().to_string_lossy(), to.extension()));
                count += 1;
                let t = std::time::Instant::now();
                let result = converter.convert(&src, &dst, from, to, &ctx);
                if t.elapsed().as_secs_f32() > 1.0 {
                    eprintln!("slow: {} → {to} ({preset:?}) took {:.1?}", src.file_name().unwrap().to_string_lossy(), t.elapsed());
                }
                match result {
                    Ok(()) if Format::detect(&dst) == Some(to) => {}
                    Ok(()) => failures.push(format!("{} → {to}: output not recognised", src.display())),
                    Err(e) => failures.push(format!("{} → {to}: {e}", src.display())),
                }
            }
        }
    }
    std::fs::remove_dir_all(&out).ok();
    eprintln!("{count} conversions");
    assert!(failures.is_empty(), "{} of {count} failed:\n{}", failures.len(), failures.join("\n"));
}
