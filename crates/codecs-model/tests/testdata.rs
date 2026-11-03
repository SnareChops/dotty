//! Every real model in `testdata/model` converts to every other format and keeps its shape.
//! Skipped when the folder is absent; fetch it with `scripts/fetch-testdata.sh`.

use codecs_model::{ModelConverter, read};
use dotty_core::{ConvertCtx, Converter, Format, Options};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[test]
fn real_models_convert_between_all_formats_and_keep_their_shape() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/model");
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("testdata/model missing; run scripts/fetch-testdata.sh");
        return;
    };
    let out = std::env::temp_dir().join(format!("tf-model-testdata-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let (cancel, progress, options) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    // (file, triangles)
    let triangles = [("duck.glb", 4212), ("milk-truck.glb", 3624), ("box-vertex-colors.gltf", 12), ("spider-ascii.stl", 1368), ("spider-binary.stl", 1368), ("spider.obj", 1368), ("cube.ply", 12), ("cube-binary.ply", 12)];
    let (mut failures, mut converted) = (Vec::new(), 0);
    for entry in entries.flatten() {
        let src = entry.path();
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        let from = Format::detect(&src).or_else(|| match src.extension().and_then(|e| e.to_str()) {
            Some("obj") => Some(Format::Obj),
            Some("gltf") => Some(Format::Gltf),
            _ => None,
        });
        let Some(from) = from else {
            failures.push(format!("{name}: format not detected"));
            continue;
        };
        let original = match read(&src, from) {
            Ok(l) => l.mesh,
            Err(e) => {
                failures.push(format!("{name}: {e}"));
                continue;
            }
        };
        if let Some((_, n)) = triangles.iter().find(|(f, _)| *f == name)
            && original.triangles.len() != *n
        {
            failures.push(format!("{name}: {} triangles, expected {n}", original.triangles.len()));
        }
        let height = |m: &codecs_model::mesh::Mesh| if m.up == codecs_model::mesh::Up::Z { m.size()[2] } else { m.size()[1] };
        for to in [Format::Stl, Format::Obj, Format::Ply, Format::Gltf, Format::Glb].into_iter().filter(|t| *t != from) {
            let dst = out.join(format!("{name}.{}", to.extension()));
            let ctx = ConvertCtx::new(&options, &cancel, &progress);
            if let Err(e) = ModelConverter.convert(&src, &dst, from, to, &ctx) {
                failures.push(format!("{name} → {to}: {e}"));
                continue;
            }
            converted += 1;
            match read(&dst, to) {
                Ok(back) => {
                    let back = back.mesh;
                    // Upright height and triangle count survive (the same way up, whatever the format's own).
                    if back.triangles.len() != original.triangles.len() || (height(&back) - height(&original)).abs() > 1e-3 * height(&original).abs().max(1.0) {
                        failures.push(format!("{name} → {to}: {} triangles, height {} (was {}, {})", back.triangles.len(), height(&back), original.triangles.len(), height(&original)));
                    }
                }
                Err(e) => failures.push(format!("{name} → {to}: output unreadable: {e}")),
            }
        }
    }
    std::fs::remove_dir_all(&out).ok();
    assert!(failures.is_empty(), "{failures:#?}");
    assert!(converted >= 30, "only {converted} conversions ran");
}
