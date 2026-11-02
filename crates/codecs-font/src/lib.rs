//! Fonts: TTF / OTF ↔ WOFF / WOFF2 (web font packaging). The outlines are never touched: a
//! web font is the same tables, compressed. WOFF is written here (zlib per table); WOFF2 by
//! `ttf2woff2`; both are read by `wuff`.

use dotty_core::{ConvertCtx, Converter, Error, Format, Registry, Result, output};
use flate2::{Compression, write::ZlibEncoder};
use std::io::Write;
use std::path::Path;

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(FontConverter));
}

pub struct FontConverter;

const SOURCES: [Format; 4] = [Format::Ttf, Format::Otf, Format::Woff, Format::Woff2];

fn is_web(f: Format) -> bool {
    matches!(f, Format::Woff | Format::Woff2)
}

impl Converter for FontConverter {
    /// Plain fonts → web fonts, web fonts → plain fonts or the other web format.
    /// (TTF ↔ OTF would mean redrawing the outlines: not offered.)
    fn routes(&self) -> Vec<(Format, Format)> {
        let mut r = Vec::new();
        for from in SOURCES {
            for to in SOURCES {
                if from != to && (is_web(from) || is_web(to)) {
                    r.push((from, to));
                }
            }
        }
        r
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        let bytes = std::fs::read(src).map_err(|source| Error::Io { path: src.into(), source })?;
        (ctx.progress)(0.2);
        let sfnt = to_sfnt(&bytes, from)?;
        ctx.check_cancelled()?;
        (ctx.progress)(0.5);
        let (out, path) = match to {
            Format::Woff => (woff1(&sfnt)?, dst.to_path_buf()),
            Format::Woff2 => (woff2(&sfnt)?, dst.to_path_buf()),
            _ => {
                // The outlines decide the file type, whatever the target says.
                let real = if sfnt.starts_with(b"OTTO") { Format::Otf } else { Format::Ttf };
                if real == to {
                    (sfnt, dst.to_path_buf())
                } else {
                    let options = dotty_core::Options { dest_dir: dst.parent().map(Path::to_path_buf), ..ctx.options.clone() };
                    let other = output::output_path(src, real, &options).map_err(|source| Error::Io { path: dst.into(), source })?;
                    ctx.note(format!(
                        "This font has {} outlines, so it was saved as {} instead of {}",
                        if real == Format::Otf { "CFF (PostScript)" } else { "TrueType" },
                        real,
                        to
                    ));
                    ctx.set_output(other.clone());
                    (sfnt, other)
                }
            }
        };
        if let Err(source) = std::fs::write(&path, out) {
            return Err(Error::Io { path, source });
        }
        (ctx.progress)(1.0);
        Ok(())
    }
}

/// The plain OpenType/TrueType file inside any of the four.
fn to_sfnt(bytes: &[u8], from: Format) -> Result<Vec<u8>> {
    let sfnt = match from {
        Format::Woff => wuff::decompress_woff1(bytes).map_err(|e| Error::Codec(format!("not a valid WOFF font: {e}")))?,
        Format::Woff2 => wuff::decompress_woff2(bytes).map_err(|e| Error::Codec(format!("not a valid WOFF2 font: {e}")))?,
        _ => bytes.to_vec(),
    };
    match sfnt.get(..4) {
        Some(b"ttcf") => Err(Error::Codec("font collections (.ttc) aren't supported".into())),
        Some(b"OTTO" | b"true" | [0, 1, 0, 0]) => Ok(sfnt),
        _ => Err(Error::Codec("this isn't a TrueType or OpenType font".into())),
    }
}

fn be16(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2).map(|s| u16::from_be_bytes([s[0], s[1]])).ok_or_else(|| Error::Codec("the font is cut short".into()))
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| Error::Codec("the font is cut short".into()))
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// WOFF 1.0: the sfnt's tables, each zlib-compressed when that makes it smaller.
fn woff1(sfnt: &[u8]) -> Result<Vec<u8>> {
    struct Table {
        tag: [u8; 4],
        checksum: u32,
        data: Vec<u8>,
        packed: Vec<u8>,
    }
    let count = be16(sfnt, 4)? as usize;
    let mut tables = Vec::with_capacity(count);
    for i in 0..count {
        let rec = 12 + i * 16;
        let (offset, length) = (be32(sfnt, rec + 8)? as usize, be32(sfnt, rec + 12)? as usize);
        let data = sfnt.get(offset..offset + length).ok_or_else(|| Error::Codec("a font table lies outside the file".into()))?.to_vec();
        let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
        z.write_all(&data).and_then(|_| z.try_finish()).map_err(|e| Error::Codec(format!("compression failed: {e}")))?;
        let packed = z.finish().map_err(|e| Error::Codec(format!("compression failed: {e}")))?;
        let packed = if packed.len() < data.len() { packed } else { data.clone() };
        tables.push(Table { tag: [sfnt[rec], sfnt[rec + 1], sfnt[rec + 2], sfnt[rec + 3]], checksum: be32(sfnt, rec + 4)?, data, packed });
    }
    tables.sort_by_key(|t| t.tag);
    let dir_end = 44 + 20 * count;
    let total_sfnt = 12 + 16 * count + tables.iter().map(|t| pad4(t.data.len())).sum::<usize>();
    let mut out = Vec::new();
    out.extend_from_slice(b"wOFF");
    out.extend_from_slice(&sfnt[0..4]); // flavor
    out.extend_from_slice(&[0; 4]); // length, patched below
    out.extend_from_slice(&(count as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(total_sfnt as u32).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // version
    out.extend_from_slice(&[0; 20]); // no metadata or private data
    let mut offset = dir_end;
    for t in &tables {
        out.extend_from_slice(&t.tag);
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(t.packed.len() as u32).to_be_bytes());
        out.extend_from_slice(&(t.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&t.checksum.to_be_bytes());
        offset += pad4(t.packed.len());
    }
    for t in &tables {
        out.extend_from_slice(&t.packed);
        out.resize(pad4(out.len()), 0);
    }
    let len = out.len() as u32;
    out[8..12].copy_from_slice(&len.to_be_bytes());
    Ok(out)
}

fn woff2(sfnt: &[u8]) -> Result<Vec<u8>> {
    ttf2woff2::encode(sfnt, ttf2woff2::BrotliQuality::default()).map_err(|e| Error::Codec(format!("WOFF2 compression failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/egui-icons.ttf")).unwrap()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tf-font-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(src: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> std::path::PathBuf {
        let dst = output::output_path(src, to, ctx.options).unwrap();
        FontConverter.convert(src, &dst, from, to, ctx).unwrap();
        ctx.take_output().unwrap_or(dst)
    }

    /// Table tags and contents, whatever the order or container. `head`'s checksum field is
    /// blanked: WOFF2 recomputes it.
    fn tables(sfnt: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let n = be16(sfnt, 4).unwrap() as usize;
        let mut v: Vec<_> = (0..n)
            .map(|i| {
                let r = 12 + i * 16;
                let (o, l) = (be32(sfnt, r + 8).unwrap() as usize, be32(sfnt, r + 12).unwrap() as usize);
                let tag = [sfnt[r], sfnt[r + 1], sfnt[r + 2], sfnt[r + 3]];
                let mut data = sfnt[o..o + l].to_vec();
                if &tag == b"head" {
                    data[8..12].fill(0);
                }
                (tag, data)
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_font_goes_to_woff_and_woff2_and_back_with_every_table_intact() {
        let dir = tmp("round");
        let ttf = dir.join("icons.ttf");
        std::fs::write(&ttf, fixture()).unwrap();
        let (opts, cancel, progress) = (dotty_core::Options::default(), AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let want = tables(&fixture());

        for web in [Format::Woff, Format::Woff2] {
            let packed = run(&ttf, Format::Ttf, web, &ctx);
            assert_eq!(packed.extension().unwrap(), web.extension());
            let bytes = std::fs::read(&packed).unwrap();
            assert!(bytes.starts_with(if web == Format::Woff { b"wOFF" } else { b"wOF2" }));
            assert_eq!(Format::sniff(&bytes), Some(web));
            let back = run(&packed, web, Format::Ttf, &ctx);
            assert!(tables(&std::fs::read(&back).unwrap()) == want, "{web}: tables changed");
        }
        // One web format to the other.
        let w1 = run(&ttf, Format::Ttf, Format::Woff, &ctx);
        let w2 = run(&w1, Format::Woff, Format::Woff2, &ctx);
        assert!(tables(&wuff::decompress_woff2(&std::fs::read(w2).unwrap()).unwrap()) == want, "WOFF to WOFF2 changed tables");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_outlines_decide_whether_a_web_font_unpacks_to_ttf_or_otf() {
        let dir = tmp("flavor");
        // A font claiming CFF outlines (only the header tag matters to the packager).
        let mut cff = fixture();
        cff[..4].copy_from_slice(b"OTTO");
        let woff = dir.join("cff.woff");
        std::fs::write(&woff, woff1(&cff).unwrap()).unwrap();
        let (opts, cancel, progress) = (dotty_core::Options::default(), AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let out = run(&woff, Format::Woff, Format::Ttf, &ctx);
        assert_eq!(out.extension().unwrap(), "otf", "saved with the right extension");
        assert!(std::fs::read(&out).unwrap().starts_with(b"OTTO"));
        assert!(ctx.take_notes().iter().any(|n| n.contains("CFF")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_input_is_refused_with_a_message() {
        assert!(to_sfnt(b"wOFFnonsense", Format::Woff).is_err());
        assert!(to_sfnt(b"not a font at all", Format::Ttf).unwrap_err().to_string().contains("isn't a TrueType"));
        let mut ttc = fixture();
        ttc[..4].copy_from_slice(b"ttcf");
        assert!(to_sfnt(&ttc, Format::Ttf).unwrap_err().to_string().contains("collections"));
    }
}
