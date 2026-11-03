//! EPUB and MOBI books: reading them into the document model, and writing an EPUB.

use crate::layout::percent_decode;
use crate::model::*;
use crate::resources::{EPUB_PREFIX, Resources};
use crate::{html, xhtml};
use dotty_core::{Error, Result};
use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

fn bad(why: impl std::fmt::Display) -> Error {
    Error::Codec(format!("not a valid EPUB: {why}"))
}

/// Every element of an XML file as (name, attributes, text), in document order. (HTML parsing
/// is good enough for these small files and forgives sloppy ones.)
fn elements(xml: &str) -> Vec<(String, HashMap<String, String>, String)> {
    fn text(h: &Handle, out: &mut String) {
        match &h.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            _ => h.children.borrow().iter().for_each(|c| text(c, out)),
        }
    }
    fn walk(h: &Handle, out: &mut Vec<(String, HashMap<String, String>, String)>) {
        if let NodeData::Element { name, attrs, .. } = &h.data {
            let attrs = attrs.borrow().iter().map(|a| (a.name.local.to_string(), a.value.to_string())).collect();
            let mut t = String::new();
            text(h, &mut t);
            out.push((name.local.to_string(), attrs, t));
        }
        for c in h.children.borrow().iter() {
            walk(c, out);
        }
    }
    let dom = parse_document(RcDom::default(), ParseOpts::default()).from_utf8().one(xml.as_bytes());
    let mut out = Vec::new();
    walk(&dom.document, &mut out);
    out
}

/// A file's text: UTF-8, or UTF-16 when a byte-order mark (or a `<` followed by a zero byte)
/// says so.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) }).collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [b'<', 0, ..] => utf16(bytes, true),
        [0, b'<', ..] => utf16(bytes, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// `a/b/../c.png` → `a/c.png`.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// `rel` as seen from the file `from` (both inside the zip), fragment and query dropped.
fn join(from: &str, rel: &str) -> String {
    let rel = percent_decode(rel.split(['#', '?']).next().unwrap_or(rel));
    if rel.starts_with('/') {
        return normalize(&rel);
    }
    let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
    normalize(&format!("{dir}/{rel}"))
}

fn read_entry(zip: &mut zip::ZipArchive<BufReader<File>>, name: &str) -> Result<Vec<u8>> {
    let mut f = zip.by_name(name).map_err(|_| bad(format!("{name} is missing")))?;
    let mut out = Vec::new();
    f.read_to_end(&mut out).map_err(bad)?;
    Ok(out)
}

pub fn read(path: &Path) -> Result<(Doc, Resources)> {
    let file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(bad)?;
    let container = decode(&read_entry(&mut zip, "META-INF/container.xml")?);
    let opf_path = elements(&container).into_iter().find(|(n, ..)| n == "rootfile").and_then(|(_, a, _)| a.get("full-path").cloned()).ok_or_else(|| bad("no package file"))?;
    let opf_path = normalize(&opf_path);
    let opf = decode(&read_entry(&mut zip, &opf_path)?);
    let items = elements(&opf);
    let manifest: HashMap<String, String> = items.iter().filter(|(n, ..)| n == "item").filter_map(|(_, a, _)| Some((a.get("id")?.clone(), a.get("href")?.clone()))).collect();
    let title = items.iter().find(|(n, ..)| n == "dc:title").map(|(_, _, t)| t.trim().to_string()).filter(|t| !t.is_empty());
    let spine: Vec<String> = items.iter().filter(|(n, ..)| n == "itemref").filter_map(|(_, a, _)| manifest.get(a.get("idref")?)).map(|href| join(&opf_path, href)).collect();
    if spine.is_empty() {
        return Err(bad("the book has no chapters"));
    }
    let mut doc = Doc { title, ..Default::default() };
    for chapter in &spine {
        let Ok(bytes) = read_entry(&mut zip, chapter) else { continue };
        let mut part = html::parse_document_str(&decode(&bytes));
        visit_inlines(&mut part.blocks, &mut |i| match i {
            Inline::Image { src, .. } if !Resources::is_remote(src) => *src = format!("{EPUB_PREFIX}{}", join(chapter, src)),
            // Links between chapters mean nothing once the book is one document.
            Inline::Text(_, s) => s.link = s.link.take().filter(|u| u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:")),
            _ => {}
        });
        doc.dropped.other.extend(part.dropped.other);
        doc.blocks.extend(part.blocks);
    }
    doc.dropped.other.dedup();
    let dir = path.parent().unwrap_or(Path::new(""));
    let file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let res = Resources::epub(dir, zip::ZipArchive::new(BufReader::new(file)).map_err(bad)?);
    Ok((doc, res))
}

/// A MOBI book's text (its pictures aren't read).
pub fn read_mobi(path: &Path) -> Result<Doc> {
    let book = mobi::Mobi::from_path(path).map_err(|e| Error::Codec(format!("not a valid MOBI book: {e}")))?;
    let content = book.content_as_string_lossy();
    let mut doc = html::parse_document_str(&content);
    let title = book.title();
    doc.title = Some(title.trim().to_string()).filter(|t| !t.is_empty());
    visit_inlines(&mut doc.blocks, &mut |i| {
        if let Inline::Text(_, s) = i {
            s.link = s.link.take().filter(|u| u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:"));
        }
    });
    if doc.blocks.is_empty() {
        return Err(Error::Codec("this MOBI book has no readable text (it may be protected)".into()));
    }
    Ok(doc)
}

// ---- writing ----

struct Chapter {
    title: String,
    blocks: Vec<Block>,
}

/// One chapter per top-level heading when there are several, else the whole document.
fn chapters(doc: &Doc, title: &str) -> Vec<Chapter> {
    let h1 = doc.blocks.iter().filter(|b| matches!(b, Block::Heading(1, _))).count();
    if h1 < 2 {
        return vec![Chapter { title: title.to_string(), blocks: doc.blocks.clone() }];
    }
    let mut out: Vec<Chapter> = Vec::new();
    for b in &doc.blocks {
        match b {
            Block::Heading(1, i) => out.push(Chapter { title: plain(i), blocks: vec![b.clone()] }),
            other => match out.last_mut() {
                Some(c) => c.blocks.push(other.clone()),
                None => out.push(Chapter { title: title.to_string(), blocks: vec![other.clone()] }),
            },
        }
    }
    out
}

fn media_type(name: &str, bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG") {
        Some(("png", "image/png"))
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        Some(("jpg", "image/jpeg"))
    } else if bytes.starts_with(b"GIF8") {
        Some(("gif", "image/gif"))
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else if name.to_ascii_lowercase().ends_with(".svg") || bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        Some(("svg", "image/svg+xml"))
    } else {
        None
    }
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// `2025-11-02T10:30:00Z` for now.
fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

const CSS: &str = "body { font-family: serif; line-height: 1.4; margin: 1em; }\nh1, h2, h3 { font-family: sans-serif; line-height: 1.2; }\npre { white-space: pre-wrap; background: #f4f4f4; padding: .5em; }\ncode { font-family: monospace; }\nblockquote { margin-left: 1em; padding-left: 1em; border-left: 3px solid #ccc; color: #444; }\ntable { border-collapse: collapse; } td, th { border: 1px solid #999; padding: .2em .5em; }\nimg { max-width: 100%; }\n";

/// Write `doc` as an EPUB 3 file; pictures come from `res`. Returns how many couldn't be added.
pub fn write(doc: &Doc, res: &Resources, dst: &Path, fallback_title: &str) -> Result<usize> {
    let title = doc.display_title().unwrap_or_else(|| fallback_title.to_string());
    let chapters = chapters(doc, &title);
    // Pictures: copied into the book under new names.
    let mut files: Vec<(String, &'static str, Vec<u8>)> = Vec::new();
    let mut names: HashMap<String, Option<String>> = HashMap::new();
    let mut missing = 0;
    let mut image_for = |src: &str| -> Option<String> {
        if let Some(known) = names.get(src) {
            return known.clone();
        }
        let found = res.bytes(src).and_then(|b| media_type(src, &b).map(|(ext, mt)| (ext, mt, b)));
        let out = found.map(|(ext, mt, b)| {
            let name = format!("img{}.{ext}", files.len() + 1);
            files.push((name.clone(), mt, b));
            name
        });
        if out.is_none() && !Resources::is_remote(src) {
            missing += 1;
        }
        names.insert(src.to_string(), out.clone());
        out
    };
    let bodies: Vec<String> = chapters.iter().map(|c| xhtml::blocks(&c.blocks, &mut image_for)).collect();

    let id = format!("urn:uuid:{:016x}-{:04x}", fnv(title.as_bytes()) ^ fnv(bodies.concat().as_bytes()), bodies.len());
    let head = |t: &str| format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" lang=\"en\">\n<head>\n<meta charset=\"utf-8\" />\n<title>{}</title>\n<link rel=\"stylesheet\" type=\"text/css\" href=\"style.css\" />\n</head>\n<body>\n", xhtml::escape(t));
    let io = |e: std::io::Error| Error::Io { path: dst.into(), source: e };
    let zerr = |e: zip::result::ZipError| Error::Codec(format!("EPUB: {e}"));
    let file = File::create(dst).map_err(io)?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    // The mimetype comes first and uncompressed: readers look for it at a fixed place.
    zip.start_file("mimetype", stored).map_err(zerr)?;
    zip.write_all(b"application/epub+zip").map_err(io)?;
    let mut add = |name: &str, data: &[u8]| -> Result<()> {
        zip.start_file(name, deflated).map_err(zerr)?;
        zip.write_all(data).map_err(io)
    };
    add("META-INF/container.xml", b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n<rootfiles><rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\" /></rootfiles>\n</container>\n")?;
    add("OEBPS/style.css", CSS.as_bytes())?;
    let mut manifest = String::from("<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\" />\n<item id=\"css\" href=\"style.css\" media-type=\"text/css\" />\n");
    let (mut spine, mut toc) = (String::new(), String::new());
    for (n, (c, body)) in chapters.iter().zip(&bodies).enumerate() {
        let name = format!("ch{}.xhtml", n + 1);
        add(&format!("OEBPS/{name}"), format!("{}{body}</body>\n</html>\n", head(&c.title)).as_bytes())?;
        manifest.push_str(&format!("<item id=\"ch{0}\" href=\"{name}\" media-type=\"application/xhtml+xml\" />\n", n + 1));
        spine.push_str(&format!("<itemref idref=\"ch{}\" />\n", n + 1));
        toc.push_str(&format!("<li><a href=\"{name}\">{}</a></li>\n", xhtml::escape(&c.title)));
    }
    for (n, (name, mt, bytes)) in files.iter().enumerate() {
        add(&format!("OEBPS/{name}"), bytes)?;
        manifest.push_str(&format!("<item id=\"img{}\" href=\"{name}\" media-type=\"{mt}\" />\n", n + 1));
    }
    add("OEBPS/nav.xhtml", format!("{}<nav epub:type=\"toc\" id=\"toc\">\n<h1>{}</h1>\n<ol>\n{toc}</ol>\n</nav>\n</body>\n</html>\n", head(&title), xhtml::escape(&title)).as_bytes())?;
    add(
        "OEBPS/content.opf",
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"bookid\">\n<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n<dc:identifier id=\"bookid\">{id}</dc:identifier>\n<dc:title>{}</dc:title>\n<dc:language>en</dc:language>\n<dc:creator>Unknown</dc:creator>\n<meta property=\"dcterms:modified\">{}</meta>\n</metadata>\n<manifest>\n{manifest}</manifest>\n<spine>\n{spine}</spine>\n</package>\n",
            xhtml::escape(&title),
            now()
        )
        .as_bytes(),
    )?;
    zip.finish().map_err(zerr)?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_inside_a_book_are_joined_like_a_browser_would() {
        assert_eq!(join("OEBPS/text/ch1.xhtml", "../images/a%20b.png#x"), "OEBPS/images/a b.png");
        assert_eq!(join("content.opf", "ch1.xhtml"), "ch1.xhtml");
        assert_eq!(join("a/b.opf", "/abs/c.png"), "abs/c.png");
        assert_eq!(normalize("a/./b//c/../d"), "a/b/d");
    }

    #[test]
    fn utf16_package_files_are_read() {
        let text = "<?xml version=\"1.0\"?><package><spine><itemref idref=\"a\"/></spine></package>";
        let le: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let be: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let bom = [vec![0xFF, 0xFE], le.clone()].concat();
        assert_eq!((decode(&le), decode(&be), decode(&bom)), (text.to_string(), text.to_string(), text.to_string()));
        assert_eq!(decode("\u{feff}caf\u{e9}".as_bytes()), "caf\u{e9}");
    }

    #[test]
    fn now_is_a_valid_timestamp() {
        let t = now();
        assert!(t.len() == 20 && t.ends_with('Z') && t.starts_with("20"), "{t}");
    }
}
