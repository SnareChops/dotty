fn main() {
    let path = std::env::args().nth(1).unwrap();
    let pdf = codecs_doc::pdfread::open(std::path::Path::new(&path)).unwrap();
    println!("{} pages", pdf.pages().len());
    for g in codecs_doc::pdfread::debug_glyphs(&pdf, 0).iter().take(60) {
        println!("{:8.2} {:8.2} {:6.3} {:?}", g.0, g.1, g.2, g.3);
    }
}
