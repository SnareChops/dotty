//! Documents: Markdown, HTML, plain text, EPUB and MOBI read into one model and written as
//! HTML, Markdown, text, EPUB or PDF; pictures to PDF; PDFs to pictures and text.
//! The PDF is laid out by [`layout`] (one column, built-in fonts) and written by [`pdf`];
//! PDFs are read by [`pdfread`].

pub mod epub;
pub mod fonts;
pub mod html;
pub mod layout;
pub mod markdown;
pub mod model;
pub mod pdf;
pub mod pdfread;
pub mod resources;
pub mod text;
pub mod xhtml;

use codecs_image::{Frame, ImageConverter, ImageData};
use dotty_core::{Bundle, BundleMode, Choice, ConvertCtx, Converter, Error, Format, Kind, Options, Registry, Result};
use image::DynamicImage;
use layout::{Picture, PictureData, Settings};
use model::{Doc, Inline};
use resources::Resources;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(DocConverter));
}

pub const PDF_PAGE: Choice = Choice {
    key: "pdf_page",
    label: "PDF page",
    help: "Paper size. \"As big as the picture\" gives a picture its own page size (pictures only; documents use A4)",
    values: &[("a4", "A4"), ("letter", "Letter"), ("fit", "As big as the picture")],
    multi_image_only: false,
};

pub const PDF_TEXT: Choice = Choice {
    key: "pdf_text",
    label: "PDF text size",
    help: "Size of body text in a PDF made from a document",
    values: &[("medium", "Medium"), ("small", "Small"), ("large", "Large")],
    multi_image_only: false,
};

pub const PDF_MARGINS: Choice = Choice {
    key: "pdf_margins",
    label: "PDF margins",
    help: "Space around the text",
    values: &[("normal", "Normal"), ("narrow", "Narrow"), ("wide", "Wide")],
    multi_image_only: false,
};

pub const PDF_NUMBERS: Choice = Choice {
    key: "pdf_numbers",
    label: "Page numbers",
    help: "Number the pages of a PDF made from a document",
    values: &[("on", "On"), ("off", "Off")],
    multi_image_only: false,
};

pub const HTML_WRAP: Choice = Choice {
    key: "html_wrap",
    label: "HTML",
    help: "A complete page with styling, or only the converted markup to paste into your own page",
    values: &[("page", "Complete page"), ("fragment", "Markup only")],
    multi_image_only: false,
};

pub const PDF_DPI: Choice = Choice {
    key: "pdf_dpi",
    label: "PDF → picture detail",
    help: "How sharp the pictures of a PDF's pages are (dots per inch): 150 suits the screen, 300 printing",
    values: &[("150", "150 dpi"), ("100", "100 dpi"), ("200", "200 dpi"), ("300", "300 dpi")],
    multi_image_only: false,
};

pub const PDF_PAGES: Choice = Choice {
    key: "pdf_pages",
    label: "PDF → picture pages",
    help: "Every page of the PDF (as a folder or zip of pictures), or just the first",
    values: &[("all", "Every page"), ("first", "First page only")],
    multi_image_only: false,
};

pub const PDF_LINES: Choice = Choice {
    key: "pdf_lines",
    label: "PDF → text lines",
    help: "Join the lines of each paragraph, or keep the lines as they are laid out in the PDF",
    values: &[("reflow", "Join paragraphs"), ("keep", "Keep line breaks")],
    multi_image_only: false,
};

pub struct DocConverter;

const TEXT_SOURCES: [Format; 5] = [Format::Md, Format::Html, Format::Txt, Format::Epub, Format::Mobi];
const TEXT_TARGETS: [Format; 5] = [Format::Md, Format::Html, Format::Txt, Format::Pdf, Format::Epub];

fn read_text(path: &Path, ctx: &ConvertCtx) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => {
            ctx.note("The file isn't UTF-8; it was read as Windows-1252 (Western European)");
            Ok(bytes.iter().map(|&b| b as char).collect())
        }
    }
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|source| Error::Io { path: path.into(), source })
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|source| Error::Io { path: path.into(), source })
}

/// Page counts of PDFs seen, so asking again (the app asks often) doesn't read the file again.
fn cached_pages(path: &Path) -> Option<usize> {
    type Seen = std::collections::HashMap<PathBuf, (std::time::SystemTime, u64, usize)>;
    static CACHE: OnceLock<Mutex<Seen>> = OnceLock::new();
    let meta = std::fs::metadata(path).ok()?;
    let key = (meta.modified().ok()?, meta.len());
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    if let Some((t, l, n)) = cache.get(path)
        && (*t, *l) == key
    {
        return Some(*n);
    }
    let n = pdfread::page_count(path).ok()?;
    cache.insert(path.to_path_buf(), (key.0, key.1, n));
    Some(n)
}

impl Converter for DocConverter {
    fn routes(&self) -> Vec<(Format, Format)> {
        let mut r: Vec<(Format, Format)> = TEXT_SOURCES.iter().flat_map(|&a| TEXT_TARGETS.iter().filter(move |&&b| b != a).map(move |&b| (a, b))).collect();
        let images = ImageConverter::default();
        r.extend(images.readable().into_iter().filter(|f| f.kind() == Kind::Image).map(|f| (f, Format::Pdf)));
        r.push((Format::Pdf, Format::Txt));
        r.extend(images.writable().into_iter().map(|f| (Format::Pdf, f)));
        r
    }

    fn choices(&self, to: Format) -> Vec<Choice> {
        match to {
            Format::Pdf => vec![PDF_PAGE, PDF_TEXT, PDF_MARGINS, PDF_NUMBERS],
            Format::Html => vec![HTML_WRAP],
            _ => Vec::new(),
        }
    }

    fn source_choices(&self, from: Format) -> Vec<Choice> {
        if from == Format::Pdf { vec![PDF_DPI, PDF_PAGES, PDF_LINES] } else { Vec::new() }
    }

    fn many_files(&self, src: &Path, from: Format, to: Format, options: &Options) -> bool {
        from == Format::Pdf && to.kind() == Kind::Image && options.choice(Format::Pdf, &PDF_PAGES) == "all" && cached_pages(src).is_some_and(|n| n > 1)
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        (ctx.progress)(0.0);
        match (from, to) {
            (Format::Pdf, Format::Txt) => pdf_to_text(src, dst, ctx)?,
            (Format::Pdf, t) if t.kind() == Kind::Image => pdf_to_pictures(src, dst, t, ctx)?,
            (f, Format::Pdf) if f.kind() == Kind::Image => pictures_to_pdf(src, dst, f, ctx)?,
            (Format::Md, Format::Html) => {
                let text = read_text(src, ctx)?;
                let wrap = ctx.options.choice(Format::Html, &HTML_WRAP) == "page";
                let doc = markdown::parse(&text);
                let title = src.file_stem().map(|s| s.to_string_lossy().into_owned());
                let html = markdown::to_html(&text, wrap, doc.display_title().or(title).as_deref());
                if text.contains("![") {
                    ctx.note("Pictures are linked by their original paths: keep the HTML next to them");
                }
                write_text(dst, &html)?;
            }
            (f, t) if TEXT_SOURCES.contains(&f) && TEXT_TARGETS.contains(&t) => text_conversion(src, dst, f, t, ctx)?,
            _ => return Err(Error::NoRoute { from, to }),
        }
        (ctx.progress)(1.0);
        Ok(())
    }
}

// ---- document to document ----

fn read_doc(src: &Path, from: Format, ctx: &ConvertCtx) -> Result<(Doc, Resources)> {
    let dir = src.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(match from {
        Format::Md => (markdown::parse(&read_text(src, ctx)?), Resources::files(&dir)),
        Format::Html => (html::parse_document_str(&read_text(src, ctx)?), Resources::files(&dir)),
        Format::Txt => (text::parse(&read_text(src, ctx)?), Resources::files(&dir)),
        Format::Epub => epub::read(src)?,
        Format::Mobi => {
            ctx.note("Pictures in MOBI books aren't included");
            (epub::read_mobi(src)?, Resources::files(&dir))
        }
        _ => return Err(Error::NoRoute { from, to: Format::Txt }),
    })
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/svg+xml"
    }
}

/// Pictures that live inside an EPUB can't stay as paths: they leave the text.
fn replace_book_pictures(doc: &mut Doc) -> usize {
    let mut n = 0;
    model::visit_inlines(&mut doc.blocks, &mut |i| {
        if let Inline::Image { src, alt } = i
            && src.starts_with(resources::EPUB_PREFIX)
        {
            n += 1;
            *i = Inline::Text(if alt.is_empty() { String::new() } else { format!("[{alt}]") }, model::Style { italic: true, ..Default::default() });
        }
    });
    n
}

fn text_conversion(src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
    let (mut doc, res) = read_doc(src, from, ctx)?;
    (ctx.progress)(0.3);
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for m in &doc.dropped.other {
        ctx.note(m.clone());
    }
    match to {
        Format::Pdf => return document_to_pdf(&doc, &res, dst, &stem, ctx),
        Format::Epub => {
            let missing = epub::write(&doc, &res, dst, &stem)?;
            if missing > 0 {
                ctx.note(format!("{missing} picture{} couldn't be found and {} left out", plural(missing), if missing == 1 { "was" } else { "were" }));
            }
            let web = model::images(&doc.blocks).iter().filter(|s| Resources::is_remote(s)).count();
            if web > 0 {
                ctx.note(format!("{web} picture{} from the web {} not downloaded", plural(web), if web == 1 { "was" } else { "were" }));
            }
            return Ok(());
        }
        _ => {}
    }
    let out = match to {
        Format::Html => {
            let mut embedded = 0;
            let mut map = |s: &str| -> Option<String> {
                if let Some(bytes) = s.starts_with(resources::EPUB_PREFIX).then(|| res.bytes(s)).flatten() {
                    embedded += 1;
                    return Some(format!("data:{};base64,{}", mime(&bytes), base64(&bytes)));
                }
                (!s.starts_with(resources::EPUB_PREFIX)).then(|| s.to_string())
            };
            let body = xhtml::blocks(&doc.blocks, &mut map);
            let title = doc.display_title().unwrap_or(stem);
            if embedded > 0 {
                ctx.note(format!("{embedded} picture{} from the book {} stored inside the HTML file", plural(embedded), if embedded == 1 { "is" } else { "are" }));
            }
            if ctx.options.choice(Format::Html, &HTML_WRAP) == "page" { xhtml::page(&title, &body) } else { body }
        }
        Format::Md | Format::Txt => {
            let left_out = replace_book_pictures(&mut doc);
            if left_out > 0 {
                ctx.note(format!("{left_out} picture{} in the book {} not included (text formats can't hold them)", plural(left_out), if left_out == 1 { "was" } else { "were" }));
            }
            if to == Format::Md { markdown::write(&doc) } else { text::write(&doc) }
        }
        _ => return Err(Error::NoRoute { from, to }),
    };
    write_text(dst, &out)
}

// ---- documents to PDF ----

fn settings(ctx: &ConvertCtx) -> Settings {
    let o = ctx.options;
    let page = match o.choice(Format::Pdf, &PDF_PAGE) {
        "letter" => Settings::LETTER,
        _ => Settings::A4,
    };
    let margin = match o.choice(Format::Pdf, &PDF_MARGINS) {
        "narrow" => 36.0,
        "wide" => 72.0,
        _ => 54.0,
    };
    let font_size = match o.choice(Format::Pdf, &PDF_TEXT) {
        "small" => 10.0,
        "large" => 13.0,
        _ => 11.5,
    };
    Settings { page, margin, font_size }
}

/// Longest side, in pixels, of a picture inside a document (enough for print at 150 dpi).
const DOC_PICTURE_SIDE: u32 = 2000;

/// A picture file as a PDF picture.
fn load_path(path: &Path, images: &ImageConverter, ctx: &ConvertCtx) -> Option<Picture> {
    let fmt = Format::detect(path).filter(|f| f.kind() == Kind::Image)?;
    let data = images.decode(path, fmt, 1, ctx).ok()?;
    picture(data.first(), (fmt == Format::Jpeg).then_some(path), Some(DOC_PICTURE_SIDE), false)
}

/// A picture as written in a document: a file beside it, or an entry of the book.
fn load_source(res: &Resources, src: &str, images: &ImageConverter, ctx: &ConvertCtx) -> Option<Picture> {
    if let Some(path) = res.file_path(src) {
        return load_path(&path, images, ctx);
    }
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let bytes = res.bytes(src)?;
    let ext = match mime(&bytes) {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "svg",
    };
    let tmp = std::env::temp_dir().join(format!("dotty-doc-{}-{}.{ext}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::write(&tmp, &bytes).ok()?;
    let pic = load_path(&tmp, images, ctx);
    let _ = std::fs::remove_file(&tmp);
    pic
}

fn document_to_pdf(doc: &Doc, res: &Resources, dst: &Path, stem: &str, ctx: &ConvertCtx) -> Result<()> {
    let s = settings(ctx);
    let images = ImageConverter::default();
    let laid = layout::layout(doc, s.clone(), |src| load_source(res, src, &images, ctx));
    ctx.check_cancelled()?;
    (ctx.progress)(0.7);
    let title = doc.display_title().or_else(|| Some(stem.to_string()));
    let numbers = ctx.options.choice(Format::Pdf, &PDF_NUMBERS) == "on";
    let bytes = pdf::render(&laid, s.page, s.margin, numbers, title.as_deref())?;
    write_bytes(dst, &bytes)?;

    let remote = laid.remote_images;
    if remote > 0 {
        ctx.note(format!("{remote} picture{} from the web {} not downloaded (Dotty works offline); {} shown as text", plural(remote), if remote == 1 { "was" } else { "were" }, if remote == 1 { "it is" } else { "they are" }));
    }
    for f in laid.failed_images.iter().take(3) {
        ctx.note(format!("The picture \"{f}\" couldn't be found or read"));
    }
    if laid.failed_images.len() > 3 {
        ctx.note(format!("{} more pictures couldn't be found or read", laid.failed_images.len() - 3));
    }
    if laid.missing_glyphs > 0 {
        ctx.note("Some characters aren't in the built-in font (emoji, Chinese, Japanese, Korean, Arabic…) and are shown as empty boxes");
    }
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ---- pictures ----

/// A JPEG's EXIF orientation says to turn it; embedding the file as it is would show it wrong.
fn jpeg_is_upright(bytes: &[u8]) -> bool {
    let mut i = 2;
    while i + 4 <= bytes.len() && bytes[i] == 0xFF {
        let marker = bytes[i + 1];
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if marker == 0xE1 && bytes.get(i + 4..i + 10) == Some(b"Exif\0\0") {
            let tiff = bytes.get(i + 10..i + 2 + len).unwrap_or(&[]);
            return !matches!(image::metadata::Orientation::from_exif_chunk(tiff), Some(o) if o != image::metadata::Orientation::NoTransforms);
        }
        if marker == 0xDA {
            break;
        }
        i += 2 + len;
    }
    true
}

/// A picture for the PDF. JPEG sources are embedded as they are when nothing changed (and
/// re-encoded as JPEG otherwise, which keeps photos small); everything else is lossless.
/// `max_side` shrinks big pictures; `changed` says edits were applied to the pixels.
fn picture(img: &DynamicImage, jpeg_file: Option<&Path>, max_side: Option<u32>, changed: bool) -> Option<Picture> {
    let (w, h) = (img.width(), img.height());
    if let (Some(path), false) = (jpeg_file, changed)
        && let Ok(bytes) = std::fs::read(path)
    {
        let raw = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok());
        let small = max_side.is_none_or(|m| w.max(h) <= m);
        if raw == Some((w, h)) && small && jpeg_is_upright(&bytes) {
            return Some(Picture { width: w, height: h, data: PictureData::Jpeg(bytes) });
        }
    }
    let img = match max_side {
        Some(m) if w.max(h) > m => img.resize(m, m, image::imageops::FilterType::Lanczos3),
        _ => img.clone(),
    };
    let (w, h) = (img.width(), img.height());
    if jpeg_file.is_some() && !img.color().has_alpha() {
        let mut out = Vec::new();
        let rgb = img.to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 88).encode_image(&rgb).ok()?;
        return Some(Picture { width: w, height: h, data: PictureData::Jpeg(out) });
    }
    Some(Picture { width: w, height: h, data: PictureData::Rgba(img.to_rgba8().into_raw()) })
}

fn pictures_to_pdf(src: &Path, dst: &Path, from: Format, ctx: &ConvertCtx) -> Result<()> {
    let images = ImageConverter::default();
    let max_frames = if from.paged_input() { usize::MAX } else { 1 };
    let mut data = images.decode(src, from, max_frames, ctx)?;
    ctx.check_cancelled()?;
    (ctx.progress)(0.4);
    let changed = ctx.edits.is_some();
    if let Some(edits) = ctx.edits {
        codecs_image::edit::apply(&mut data, edits);
    }
    if data.more_frames || (data.frames.len() > 1 && !data.pages) {
        ctx.note(if from == Format::Gif || from == Format::Webp || from == Format::Png { "Animated picture: only the first frame was put in the PDF" } else { "Only the first picture was put in the PDF" });
    }
    let frames: Vec<&DynamicImage> = if data.pages { data.frames.iter().map(|f| &f.image).collect() } else { vec![data.first()] };
    let fit = ctx.options.choice(Format::Pdf, &PDF_PAGE);
    let mut pages = Vec::new();
    for img in frames {
        let Some(pic) = picture(img, (from == Format::Jpeg).then_some(src), None, changed) else { continue };
        let (pw, ph) = (pic.width as f32, pic.height as f32);
        let (page, rect) = if fit == "fit" {
            // The picture's own size at 96 dpi, kept within what PDF viewers handle.
            let k = (0.75f32).min(14_000.0 / pw.max(ph));
            let (w, h) = (pw * k, ph * k);
            ((w, h), (0.0, 0.0, w, h))
        } else {
            let base = if fit == "letter" { Settings::LETTER } else { Settings::A4 };
            let page = if pw > ph { (base.1, base.0) } else { base };
            let margin = 24.0;
            let k = ((page.0 - 2.0 * margin) / pw).min((page.1 - 2.0 * margin) / ph);
            let (w, h) = (pw * k, ph * k);
            (page, ((page.0 - w) / 2.0, (page.1 - h) / 2.0, w, h))
        };
        pages.push(pdf::PicturePage { size: page, at: rect, picture: pic });
    }
    if pages.is_empty() {
        return Err(Error::Codec("nothing to put in the PDF".into()));
    }
    let bytes = pdf::pictures(&pages)?;
    write_bytes(dst, &bytes)?;
    if from.paged_input() && pages.len() > 1 {
        ctx.note(format!("{} pages: one for each picture in the file", pages.len()));
    }
    Ok(())
}

// ---- PDF to pictures and text ----

fn pdf_to_pictures(src: &Path, dst: &Path, to: Format, ctx: &ConvertCtx) -> Result<()> {
    let pdf = pdfread::open(src)?;
    let total = pdf.pages().len();
    if total == 0 {
        return Err(Error::Codec("this PDF has no pages".into()));
    }
    let dpi: f32 = ctx.options.choice(Format::Pdf, &PDF_DPI).parse().unwrap_or(150.0);
    let count = if ctx.options.choice(Format::Pdf, &PDF_PAGES) == "first" { 1 } else { total };
    let images = ImageConverter::default();
    let cache = hayro::RenderCache::new();
    let one = |index: usize| -> Result<Vec<u8>> {
        let page = pdfread::render(&pdf, index, dpi, &cache)?;
        let data = ImageData { frames: vec![Frame { image: DynamicImage::ImageRgba8(page), delay: std::time::Duration::ZERO }], exif: None, icc: None, more_frames: false, pages: false, loops: 0 };
        images.encode_to_vec(data, Format::Png, to, ctx)
    };
    if count == 1 {
        write_bytes(dst, &one(0)?)?;
        if total > 1 {
            ctx.note(format!("Only the first of {total} pages was converted (change \"PDF → picture pages\" to get them all)"));
        }
        return Ok(());
    }
    let mut bundle = Bundle::create(dst, "pages", ctx.options.bundle.unwrap_or_default())?;
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "page".into());
    let width = count.to_string().len().max(2);
    let result = (|| {
        for i in 0..count {
            ctx.check_cancelled()?;
            bundle.add(&format!("{stem}-{:0width$}.{}", i + 1, to.extension()), &one(i)?)?;
            (ctx.progress)(0.05 + 0.9 * (i + 1) as f32 / count as f32);
        }
        Ok::<(), Error>(())
    })();
    match result {
        Ok(()) => {
            let (name, zip) = (bundle.name(), ctx.options.bundle == Some(BundleMode::Zip));
            ctx.note(format!("Saved {count} pages in the {} \"{name}\"", if zip { "zip" } else { "folder" }));
            ctx.set_output(bundle.finish()?);
            Ok(())
        }
        Err(e) => {
            bundle.abort();
            Err(e)
        }
    }
}

fn pdf_to_text(src: &Path, dst: &Path, ctx: &ConvertCtx) -> Result<()> {
    let pdf = pdfread::open(src)?;
    let reflow = ctx.options.choice(Format::Pdf, &PDF_LINES) == "reflow";
    let (text, empty) = pdfread::text(&pdf, reflow, |done, total| (ctx.progress)(done as f32 / total.max(1) as f32));
    ctx.check_cancelled()?;
    let total = pdf.pages().len();
    if text.trim().is_empty() {
        ctx.note("No text was found: the pages are probably scanned pictures, which need text recognition (OCR)");
    } else if empty > 0 {
        ctx.note(format!("{empty} of {total} pages have no text (pictures or scans)"));
    }
    ctx.note("Columns, tables and footnotes are read line by line, so check the order of anything laid out in more than one column");
    write_text(dst, &(text + "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotty_core::Options;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-doc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn convert(src: &Path, to: Format, options: &Options) -> (Vec<u8>, Vec<String>) {
        let from = Format::detect(src).or_else(|| (src.extension().is_some_and(|e| e == "md")).then_some(Format::Md)).expect("format");
        let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(options, &cancel, &progress);
        let dst = src.with_extension(format!("out.{}", to.extension()));
        DocConverter.convert(src, &dst, from, to, &ctx).unwrap_or_else(|e| panic!("{from} → {to}: {e}"));
        (std::fs::read(&dst).unwrap(), ctx.take_notes())
    }

    fn pdf(bytes: &[u8]) -> lopdf::Document {
        assert!(bytes.starts_with(b"%PDF-"));
        lopdf::Document::load_mem(bytes).expect("a readable PDF")
    }

    fn page_size(doc: &lopdf::Document, page: u32) -> (f32, f32) {
        let id = doc.get_pages()[&page];
        let dict = doc.get_object(id).unwrap().as_dict().unwrap();
        let mb = dict.get(b"MediaBox").unwrap().as_array().unwrap();
        let n = |i: usize| mb[i].as_float().unwrap_or_else(|_| mb[i].as_i64().unwrap() as f32);
        (n(2) - n(0), n(3) - n(1))
    }

    fn png(path: &Path, w: u32, h: u32, alpha: u8) {
        image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 90, alpha])).save(path).unwrap();
    }

    #[test]
    fn markdown_becomes_a_web_page_or_just_markup() {
        let dir = tmp("html");
        let md = dir.join("notes.md");
        std::fs::write(&md, "# Notes\n\nSome *text* & more.\n\n![pic](a.png)\n").unwrap();
        let (page, notes) = convert(&md, Format::Html, &Options::default());
        let page = String::from_utf8(page).unwrap();
        assert!(page.contains("<title>Notes</title>") && page.contains("<em>text</em>") && page.contains("<style>"));
        assert!(notes.iter().any(|n| n.contains("original paths")));
        let mut o = Options::default();
        o.set_choice(Format::Html, &HTML_WRAP, "fragment");
        let (frag, _) = convert(&md, Format::Html, &o);
        assert_eq!(String::from_utf8(frag).unwrap(), "<h1>Notes</h1>\n<p>Some <em>text</em> &amp; more.</p>\n<p><img src=\"a.png\" alt=\"pic\" /></p>\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_web_page_becomes_markdown() {
        let dir = tmp("tomd");
        let html = dir.join("page.html");
        std::fs::write(&html, "<html><head><title>T</title></head><body><h2>Hi</h2><p>A <b>bold</b> <a href=\"https://e.com\">link</a>.</p><ul><li>x</li><li>y</li></ul><script>1</script></body></html>").unwrap();
        let (md, _) = convert(&html, Format::Md, &Options::default());
        assert_eq!(String::from_utf8(md).unwrap(), "## Hi\n\nA **bold** [link](https://e.com).\n\n- x\n- y\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_markdown_file_becomes_a_pdf_with_bookmarks_pictures_and_notes() {
        let dir = tmp("pdf");
        png(&dir.join("pic.png"), 300, 200, 255);
        let md = dir.join("doc.md");
        let body: String = (0..30).map(|i| format!("## Chapter {i}\n\n{}\n\n", "words and more words to fill the page ".repeat(30))).collect();
        std::fs::write(&md, format!("# The doc\n\n![local](pic.png) ![web](https://example.com/x.png) ![gone](nope.png)\n\n{body}")).unwrap();
        let (bytes, notes) = convert(&md, Format::Pdf, &Options::default());
        let doc = pdf(&bytes);
        assert!(doc.get_pages().len() > 5);
        assert_eq!(page_size(&doc, 1), (595.28, 841.89));
        let catalog = doc.catalog().unwrap();
        assert!(catalog.get(b"Outlines").is_ok(), "bookmarks");
        assert!(notes.iter().any(|n| n.contains("from the web")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("nope.png")), "{notes:?}");
        assert!(bytes.windows(8).any(|w| w == b"/Subtype") && doc.objects.values().any(|o| o.as_stream().is_ok_and(|s| s.dict.get(b"Subtype").and_then(|v| v.as_name()).is_ok_and(|n| n == b"Image"))), "the local picture is in the file");

        // Letter paper, small text, no numbers: more room per page, so fewer pages.
        let mut o = Options::default();
        o.set_choice(Format::Pdf, &PDF_PAGE, "letter");
        o.set_choice(Format::Pdf, &PDF_TEXT, "small");
        o.set_choice(Format::Pdf, &PDF_MARGINS, "narrow");
        o.set_choice(Format::Pdf, &PDF_NUMBERS, "off");
        let (small, _) = convert(&md, Format::Pdf, &o);
        let small = pdf(&small);
        assert_eq!(page_size(&small, 1), (612.0, 792.0));
        assert!(small.get_pages().len() < doc.get_pages().len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_web_page_becomes_a_pdf() {
        let dir = tmp("htmlpdf");
        let html = dir.join("p.html");
        std::fs::write(&html, "<!doctype html><title>Page</title><h1>Hello</h1><p>Text <a href=\"https://e.com\">here</a>.</p><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table><svg></svg>").unwrap();
        let (bytes, notes) = convert(&html, Format::Pdf, &Options::default());
        assert_eq!(pdf(&bytes).get_pages().len(), 1);
        assert!(notes.iter().any(|n| n.contains("svg")), "{notes:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pictures_become_pdf_pages_jpegs_untouched() {
        let dir = tmp("pics");
        // A JPEG goes in byte for byte.
        let jpg = dir.join("photo.jpg");
        let rgb = image::RgbImage::from_fn(240, 120, |x, y| image::Rgb([x as u8, y as u8, 99]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90).encode_image(&rgb).unwrap();
        std::fs::write(&jpg, &bytes).unwrap();
        let (out, _) = convert(&jpg, Format::Pdf, &Options::default());
        let doc = pdf(&out);
        assert_eq!(doc.get_pages().len(), 1);
        let (w, h) = page_size(&doc, 1);
        assert!(w > h, "a wide picture gets a landscape page: {w}×{h}");
        assert!(out.windows(bytes.len()).any(|win| win == bytes.as_slice()), "the JPEG's bytes are in the PDF unchanged");

        // A PNG with transparency goes in losslessly, and "as big as the picture" sizes the page.
        let p = dir.join("art.png");
        png(&p, 300, 150, 128);
        let mut o = Options::default();
        o.set_choice(Format::Pdf, &PDF_PAGE, "fit");
        let (out, _) = convert(&p, Format::Pdf, &o);
        let (w, h) = page_size(&pdf(&out), 1);
        assert!((w - 225.0).abs() < 0.5 && (h - 112.5).abs() < 0.5, "{w}×{h}");

        // An icon with several sizes: a page for each.
        let ico = dir.join("multi.ico");
        let frames: Vec<_> = [16u32, 32, 48]
            .iter()
            .map(|&s| {
                let rgba = image::RgbaImage::from_pixel(s, s, image::Rgba([200, 20, 20, 255]));
                image::codecs::ico::IcoFrame::as_png(rgba.as_raw(), s, s, image::ExtendedColorType::Rgba8).unwrap()
            })
            .collect();
        image::codecs::ico::IcoEncoder::new(std::fs::File::create(&ico).unwrap()).encode_images(&frames).unwrap();
        let (out, notes) = convert(&ico, Format::Pdf, &Options::default());
        assert_eq!(pdf(&out).get_pages().len(), 3);
        assert!(notes.iter().any(|n| n.contains("3 pages")), "{notes:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_turned_jpeg_is_not_embedded_as_it_is() {
        let exif = |orientation: u8| vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, orientation, 0, 0, 0, 0, 0, 0, 0];
        let jpeg_with = |exif: Option<Vec<u8>>| {
            let rgb = image::RgbImage::from_pixel(8, 4, image::Rgb([10, 200, 30]));
            let mut out = Vec::new();
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
            if let Some(e) = exif {
                image::ImageEncoder::set_exif_metadata(&mut enc, e).unwrap();
            }
            enc.encode_image(&rgb).unwrap();
            out
        };
        assert!(jpeg_is_upright(&jpeg_with(None)));
        assert!(jpeg_is_upright(&jpeg_with(Some(exif(1)))));
        assert!(!jpeg_is_upright(&jpeg_with(Some(exif(6)))));
        assert!(!jpeg_is_upright(&jpeg_with(Some(exif(3)))), "even a half turn keeps the size but not the picture");
    }

    #[test]
    fn formats_are_recognised_and_routes_exist() {
        let routes = DocConverter.routes();
        for r in [(Format::Md, Format::Pdf), (Format::Md, Format::Html), (Format::Html, Format::Pdf), (Format::Html, Format::Md), (Format::Png, Format::Pdf), (Format::Svg, Format::Pdf), (Format::Ico, Format::Pdf)] {
            assert!(routes.contains(&r), "{r:?}");
        }
        assert!(!routes.iter().any(|r| r.0.kind() != Kind::Image && r.0.kind() != Kind::Document), "only documents and pictures");
        for r in [(Format::Pdf, Format::Png), (Format::Pdf, Format::Txt), (Format::Txt, Format::Pdf), (Format::Epub, Format::Pdf), (Format::Mobi, Format::Epub), (Format::Md, Format::Epub)] {
            assert!(routes.contains(&r), "{r:?}");
        }
        assert!(!routes.contains(&(Format::Pdf, Format::Pdf)) && !routes.contains(&(Format::Md, Format::Mobi)) && !routes.contains(&(Format::Pdf, Format::Favicon)));
    }

    /// Like `convert`, but gives the path of the result (a folder or zip when there are many files).
    fn convert_path(src: &Path, to: Format, options: &Options) -> (PathBuf, Vec<String>) {
        let from = Format::detect(src).or_else(|| (src.extension().is_some_and(|e| e == "md")).then_some(Format::Md)).expect("format");
        let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(options, &cancel, &progress);
        let dst = src.with_extension(format!("out.{}", to.extension()));
        DocConverter.convert(src, &dst, from, to, &ctx).unwrap_or_else(|e| panic!("{from} → {to}: {e}"));
        (ctx.take_output().unwrap_or(dst), ctx.take_notes())
    }

    fn sample_pdf(dir: &Path, chapters: usize) -> PathBuf {
        let md = dir.join("book.md");
        let body: String = (0..chapters).map(|i| format!("# Chapter {i}\n\n{}\n\n", "The quick brown fox jumps over the lazy dog. ".repeat(40))).collect();
        std::fs::write(&md, body).unwrap();
        let (bytes, _) = convert(&md, Format::Pdf, &Options::default());
        let pdf = dir.join("book.pdf");
        std::fs::write(&pdf, bytes).unwrap();
        pdf
    }

    #[test]
    fn the_pages_of_a_pdf_become_pictures_in_a_folder_a_zip_or_just_the_first() {
        let dir = tmp("pdfpics");
        let pdf = sample_pdf(&dir, 6);
        let pages = lopdf::Document::load_mem(&std::fs::read(&pdf).unwrap()).unwrap().get_pages().len();
        assert!(pages >= 3);
        let options = Options::default();
        assert!(DocConverter.many_files(&pdf, Format::Pdf, Format::Png, &options), "several pages: the app asks folder or zip");

        let (out, notes) = convert_path(&pdf, Format::Png, &options);
        assert!(out.is_file() && out.extension().is_some_and(|e| e == "png"), "the first file of the folder: {}", out.display());
        let folder = out.parent().unwrap();
        let files: Vec<_> = std::fs::read_dir(folder).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(files.len(), pages, "{notes:?}");
        let first = image::open(folder.join("book-01.png")).unwrap().to_rgb8();
        assert_eq!(first.dimensions(), (1240, 1753), "A4 at 150 dpi");
        assert!(first.pixels().any(|p| p.0[0] < 100), "there is dark text on it");
        assert!(first.pixels().filter(|p| p.0 == [255, 255, 255]).count() > first.pixels().len() / 2, "on a white page");

        // As a zip, at a lower detail.
        let mut o = Options { bundle: Some(BundleMode::Zip), ..Default::default() };
        o.set_choice(Format::Pdf, &PDF_DPI, "100");
        let (out, _) = convert_path(&pdf, Format::Jpeg, &o);
        assert!(out.extension().is_some_and(|e| e == "zip"));
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(zip.len(), pages);
        let mut jpg = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_name("book-01.jpg").unwrap(), &mut jpg).unwrap();
        assert_eq!(image::load_from_memory(&jpg).unwrap().width(), 826, "A4 at 100 dpi");

        // First page only: a single file, nothing to ask.
        let mut o = Options::default();
        o.set_choice(Format::Pdf, &PDF_PAGES, "first");
        assert!(!DocConverter.many_files(&pdf, Format::Pdf, Format::Png, &o));
        let (out, notes) = convert_path(&pdf, Format::Png, &o);
        assert!(out.ends_with("book.out.png") && image::open(&out).is_ok());
        assert!(notes.iter().any(|n| n.contains(&format!("first of {pages}"))), "{notes:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_one_page_pdf_is_one_picture_and_a_pdfs_text_comes_back() {
        let dir = tmp("pdftext");
        let md = dir.join("one.md");
        std::fs::write(&md, "# Title Here\n\nA paragraph with **bold** words and `code`.\n\n- first item\n- second item\n").unwrap();
        let (bytes, _) = convert(&md, Format::Pdf, &Options::default());
        let pdf = dir.join("one.pdf");
        std::fs::write(&pdf, bytes).unwrap();
        assert!(!DocConverter.many_files(&pdf, Format::Pdf, Format::Png, &Options::default()), "one page: nothing to ask");
        let (out, notes) = convert_path(&pdf, Format::Png, &Options::default());
        assert!(out.ends_with("one.out.png") && image::open(&out).is_ok(), "{notes:?}");

        let (txt, notes) = convert(&pdf, Format::Txt, &Options::default());
        let txt = String::from_utf8(txt).unwrap();
        assert_eq!(txt, "Title Here\n\nA paragraph with bold words and code.\n\n\u{2022} first item\n\u{2022} second item\n");
        assert!(notes.iter().any(|n| n.contains("column")), "{notes:?}");

        // A PDF that isn't one says so.
        let junk = dir.join("junk.pdf");
        std::fs::write(&junk, b"%PDF-1.4 this is not a pdf").unwrap();
        let (cancel, progress, options) = (AtomicBool::new(false), |_: f32| {}, Options::default());
        let ctx = ConvertCtx::new(&options, &cancel, &progress);
        let err = DocConverter.convert(&junk, &dir.join("x.txt"), Format::Pdf, Format::Txt, &ctx).unwrap_err().to_string();
        assert!(err.contains("isn't a PDF"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plain_text_goes_to_pages_and_pages_go_to_plain_text() {
        let dir = tmp("txt");
        let txt = dir.join("note.txt");
        std::fs::write(&txt, "First paragraph of the note.\n\nSecond one,\nin two lines.\n").unwrap();
        let (html, _) = convert(&txt, Format::Html, &Options::default());
        let html = String::from_utf8(html).unwrap();
        assert!(html.contains("<p>First paragraph of the note.</p>") && html.contains("in two lines.") && html.contains("<style>"), "{html}");
        assert_eq!(pdf(&convert(&txt, Format::Pdf, &Options::default()).0).get_pages().len(), 1);
        let (md, _) = convert(&txt, Format::Md, &Options::default());
        assert!(String::from_utf8(md).unwrap().starts_with("First paragraph of the note.\n\nSecond one,"));

        let md_src = dir.join("doc.md");
        std::fs::write(&md_src, "# Heading\n\nText with a [link](https://e.com).\n\n1. one\n2. two\n").unwrap();
        assert_eq!(String::from_utf8(convert(&md_src, Format::Txt, &Options::default()).0).unwrap(), "Heading\n\nText with a link (https://e.com).\n\n1. one\n2. two\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_book_is_written_and_read_back_as_epub() {
        let dir = tmp("epub");
        png(&dir.join("cover.png"), 120, 80, 255);
        let md = dir.join("novel.md");
        std::fs::write(&md, "# One\n\nThe first chapter has text.\n\n![the cover](cover.png)\n\n# Two\n\nThe second chapter has more & other text.\n\n- a list\n- of items\n").unwrap();
        let (epub, notes) = convert_path(&md, Format::Epub, &Options::default());
        assert!(notes.is_empty(), "{notes:?}");
        let bytes = std::fs::read(&epub).unwrap();
        assert_eq!(Format::sniff(&bytes[..64]), Some(Format::Epub), "recognised as an EPUB, not just a zip");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(zip.by_index(0).unwrap().name().unwrap(), "mimetype");
        assert_eq!(zip.by_index(0).unwrap().compression(), zip::CompressionMethod::Stored);
        let names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().unwrap().to_string()).collect();
        for want in ["META-INF/container.xml", "OEBPS/content.opf", "OEBPS/nav.xhtml", "OEBPS/ch1.xhtml", "OEBPS/ch2.xhtml", "OEBPS/img1.png"] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        let mut ch2 = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("OEBPS/ch2.xhtml").unwrap(), &mut ch2).unwrap();
        assert!(ch2.contains("more &amp; other text") && ch2.contains("<li>a list</li>"), "{ch2}");

        // And back out: text keeps the chapters in order, the picture is left out of text,
        // a web page carries it inside, a PDF draws it.
        let (txt, notes) = convert(&epub, Format::Txt, &Options::default());
        let txt = String::from_utf8(txt).unwrap();
        assert!(txt.find("The first chapter").unwrap() < txt.find("The second chapter").unwrap() && txt.contains("more & other text") && txt.contains("[the cover]"), "{txt}");
        assert!(notes.iter().any(|n| n.contains("1 picture")), "{notes:?}");
        let (html, notes) = convert(&epub, Format::Html, &Options::default());
        let html = String::from_utf8(html).unwrap();
        assert!(html.contains("<img src=\"data:image/png;base64,") && html.contains("<title>One</title>"), "{}", &html[..html.len().min(300)]);
        assert!(notes.iter().any(|n| n.contains("stored inside")), "{notes:?}");
        let (md2, _) = convert(&epub, Format::Md, &Options::default());
        let back = markdown::parse(&String::from_utf8(md2).unwrap());
        assert!(back.blocks.iter().filter(|b| matches!(b, model::Block::Heading(1, _))).count() == 2);
        let pdf_bytes = convert(&epub, Format::Pdf, &Options::default()).0;
        let doc = pdf(&pdf_bytes);
        assert_eq!(doc.get_pages().len(), 1);
        assert!(doc.objects.values().any(|o| o.as_stream().is_ok_and(|s| s.dict.get(b"Subtype").and_then(|v| v.as_name()).is_ok_and(|n| n == b"Image"))), "the picture inside the book is in the PDF");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_books_are_refused_with_a_message() {
        let dir = tmp("badbook");
        let (cancel, progress, options) = (AtomicBool::new(false), |_: f32| {}, Options::default());
        let ctx = ConvertCtx::new(&options, &cancel, &progress);
        let zip = dir.join("not.epub");
        std::fs::write(&zip, b"PK\x03\x04 broken").unwrap();
        let err = DocConverter.convert(&zip, &dir.join("a.txt"), Format::Epub, Format::Txt, &ctx).unwrap_err().to_string();
        assert!(err.contains("not a valid EPUB"), "{err}");
        let mobi = dir.join("not.mobi");
        std::fs::write(&mobi, vec![0u8; 200]).unwrap();
        let err = DocConverter.convert(&mobi, &dir.join("b.txt"), Format::Mobi, Format::Txt, &ctx).unwrap_err().to_string();
        assert!(err.contains("MOBI"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
