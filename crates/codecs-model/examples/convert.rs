//! `cargo run --release -p codecs-model --example convert -- in.glb out.png [view]`
use codecs_model::ModelConverter;
use dotty_core::{ConvertCtx, Converter, Format, Options};
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (src, dst) = (Path::new(&args[1]), Path::new(&args[2]));
    let from = Format::detect(src).or_else(|| match src.extension()?.to_str()? {
        "obj" => Some(Format::Obj),
        "gltf" => Some(Format::Gltf),
        "stl" => Some(Format::Stl),
        _ => None,
    });
    let from = from.expect("format");
    let to = match dst.extension().and_then(|e| e.to_str()) {
        Some("png") => Format::Png,
        Some("glb") => Format::Glb,
        Some("stl") => Format::Stl,
        Some("obj") => Format::Obj,
        Some("ply") => Format::Ply,
        Some("gltf") => Format::Gltf,
        _ => panic!("unknown output"),
    };
    let mut options = Options::default();
    if let Some(v) = args.get(3) {
        options.set_choice(Format::Png, &codecs_model::MODEL_VIEW, v);
    }
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    let t = std::time::Instant::now();
    ModelConverter.convert(src, dst, from, to, &ctx).expect("conversion");
    println!("{} → {} in {:.2?} {:?}", src.display(), dst.display(), t.elapsed(), ctx.take_notes());
}
