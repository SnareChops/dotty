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
                    Ok(()) if to.is_pseudo() || Format::detect(&dst) == Some(to) => {}
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

/// Every camera RAW sample in `testdata/raw` becomes a JPEG: the camera's own picture, or (Sony
/// A6000-series) a full-resolution development. Two samples have no full-size picture inside
/// and say so.
#[test]
fn raw_samples_become_pictures() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/raw");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("testdata/raw missing; run scripts/fetch-testdata.sh");
        return;
    };
    let out = std::env::temp_dir().join(format!("tf-raw-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let converter = ImageConverter::default();
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let (mut done, mut failures) = (0, Vec::new());
    // (file, expected size when known)
    let known = [("pentax-k5ii.PEF", (4928, 3264)), ("nikon-1j2.NEF", (3872, 2592)), ("canon-r6m3.CR3", (4320, 2880)), ("sony-a6400.ARW", (6048, 4024)), ("sony-rx0.ARW", (1616, 1080))];
    for entry in entries.flatten() {
        let src = entry.path();
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let from = Format::detect(&src).unwrap_or_else(|| panic!("{name}: format not detected"));
        if from != Format::Raw {
            failures.push(format!("{name}: detected as {from}, not RAW"));
            continue;
        }
        let options = Options::default();
        let ctx = ConvertCtx::new(&options, &cancel, &progress);
        let dst = out.join(format!("{name}.jpg"));
        match converter.convert(&src, &dst, from, Format::Jpeg, &ctx) {
            Ok(()) => {
                done += 1;
                let size = image::image_dimensions(&dst).unwrap();
                if let Some((_, want)) = known.iter().find(|(n, _)| *n == name)
                    && size != *want
                {
                    failures.push(format!("{name}: {size:?}, expected {want:?}"));
                }
                if size.0.max(size.1) < 400 {
                    failures.push(format!("{name}: only {size:?}"));
                }
            }
            Err(e) if (name.contains("a410") || name.contains("c5050z")) && e.to_string().contains("no full-size picture") => {}
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    std::fs::remove_dir_all(&out).ok();
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(done >= 9, "only {done} RAW files converted");
}
