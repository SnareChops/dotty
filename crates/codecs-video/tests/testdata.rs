//! Every video in `testdata/video` becomes a still and a GIF. Skipped when the folder is
//! absent; fetch it with `scripts/fetch-testdata.sh`.

use codecs_video::VideoConverter;
use dotty_core::{ConvertCtx, Converter, Format, Options};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[test]
fn sample_videos_become_stills_and_gifs() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/video");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("testdata/video missing; run scripts/fetch-testdata.sh");
        return;
    };
    let out = std::env::temp_dir().join(format!("tf-video-frames-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let (cancel, progress, options) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    let mut failures = Vec::new();
    for entry in entries.flatten() {
        let src = entry.path();
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let from = Format::detect(&src).unwrap();
        for to in [Format::Png, Format::Gif] {
            let dst = out.join(format!("{name}.{}", to.extension()));
            let ctx = ConvertCtx::new(&options, &cancel, &progress);
            let t = std::time::Instant::now();
            match VideoConverter::default().convert(&src, &dst, from, to, &ctx) {
                Err(e) => failures.push(format!("{name} → {to}: {e}")),
                Ok(()) => {
                    let notes = ctx.take_notes();
                    let size = image::image_dimensions(&dst).unwrap();
                    eprintln!("{name} → {to}: {size:?} in {:.2?} {notes:?}", t.elapsed());
                    // The 90 s film is too long for one GIF: shortened, with a note saying so.
                    if to == Format::Gif && name.starts_with("caminandes") && !notes.iter().any(|n| n.contains("fit the size limit")) {
                        failures.push(format!("{name} → GIF: expected a 'shortened' note"));
                    }
                }
            }
        }
    }
    std::fs::remove_dir_all(&out).ok();
    assert!(failures.is_empty(), "{failures:#?}");
}
