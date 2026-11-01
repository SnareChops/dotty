//! Dev helper: `cargo run --release -p codecs-audio --example convert -- in.flac out.mp3 [low|medium|high|lossless] [--strip] [key=value ...]`
//!
//! `key=value` sets a format choice for the target format.

use codecs_audio::AudioConverter;
use std::sync::atomic::AtomicBool;
use transformer_core::{ConvertCtx, Converter, Format, Options, Preset};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (src, dst) = (std::path::Path::new(&args[0]), std::path::Path::new(&args[1]));
    let preset = match args.get(2).map(String::as_str) {
        Some("low") => Preset::Low,
        Some("medium") => Preset::Medium,
        Some("lossless") => Preset::Lossless,
        _ => Preset::High,
    };
    let from = Format::detect(src).expect("unknown input format");
    let to = Format::detect(dst).expect("unknown output extension");
    let converter = AudioConverter::default();
    let mut options = Options { preset, strip_metadata: args.iter().any(|a| a == "--strip"), ..Default::default() };
    for (key, value) in args.iter().skip(2).filter_map(|a| a.split_once('=')) {
        let choice = converter.choices(to).into_iter().find(|c| c.key == key).unwrap_or_else(|| panic!("no choice {key}"));
        options.set_choice(to, &choice, value);
    }
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    converter.convert(src, dst, from, to, &ctx).unwrap_or_else(|e| panic!("conversion failed: {e}"));
    for note in ctx.take_notes() {
        eprintln!("note: {note}");
    }
}
