fn main() {
    let path = std::env::args().nth(1).unwrap();
    let reflow = std::env::args().nth(2).is_none_or(|a| a != "keep");
    let pdf = codecs_doc::pdfread::open(std::path::Path::new(&path)).unwrap();
    let (t, empty) = codecs_doc::pdfread::text(&pdf, reflow, |_, _| {});
    println!("{t}\n---\n{empty} pages without text");
}
