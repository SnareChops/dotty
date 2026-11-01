//! Converts every real-world sample in `testdata/image` to every other format.
//! Skipped when the folder is absent; fetch it with `scripts/fetch-testdata.sh`.

use codecs_image::ImageConverter;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use dotty_core::{ConvertCtx, Converter, Format, Options, Preset};

#[test]
fn converts_all_sample_images_to_all_formats() {
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
        for preset in [Preset::Low, Preset::Lossless] {
            let options = Options { preset, ..Default::default() };
            let ctx = ConvertCtx::new(&options, &cancel, &progress);
            for (a, to) in converter.routes() {
                if a != from {
                    continue;
                }
                let dst = out.join(format!("{}-{preset:?}.{}", src.file_name().unwrap().to_string_lossy(), to.extension()));
                count += 1;
                match converter.convert(&src, &dst, from, to, &ctx) {
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
