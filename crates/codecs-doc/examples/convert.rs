//! `cargo run -p codecs-doc --example convert -- in.md out.pdf [a4|letter] [small|medium|large]`
//! Converts one file with the default settings, for looking at the result.

use codecs_doc::DocConverter;
use dotty_core::{Converter, ConvertCtx, Format, Options};
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (src, dst) = (Path::new(&args[1]), Path::new(&args[2]));
    let from = Format::detect(src).or_else(|| src.extension().and_then(|e| e.to_str()).and_then(|e| if e == "md" { Some(Format::Md) } else { None })).expect("format");
    let to = Format::detect(dst).unwrap_or(Format::Pdf);
    let to = if dst.extension().is_some_and(|e| e == "pdf") { Format::Pdf } else if dst.extension().is_some_and(|e| e == "html") { Format::Html } else { to };
    let mut options = Options::default();
    if let Some(p) = args.get(3) {
        options.set_choice(Format::Pdf, &codecs_doc::PDF_PAGE, p);
    }
    if let Some(t) = args.get(4) {
        options.set_choice(Format::Pdf, &codecs_doc::PDF_TEXT, t);
    }
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let ctx = ConvertCtx::new(&options, &cancel, &progress);
    let t = std::time::Instant::now();
    DocConverter.convert(src, dst, from, to, &ctx).expect("conversion");
    println!("{} → {} in {:.2?}; notes: {:?}", src.display(), dst.display(), t.elapsed(), ctx.take_notes());
}
