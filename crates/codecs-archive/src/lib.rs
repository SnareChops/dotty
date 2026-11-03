//! Archives: ZIP, comic-book ZIP (CBZ), TAR, TAR.GZ, GZ and 7z, read and rewritten as any
//! of them or extracted into a folder. Nothing is trusted: entry names are cleaned so they
//! can't leave the destination, links are skipped, and sizes are limited against zip bombs.

mod read;
mod write;

use dotty_core::{ConvertCtx, Converter, Error, Format, Registry, Result};
use std::cell::Cell;
use std::io::Read;
use std::path::{Path, PathBuf};
use write::Sink;

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(ArchiveConverter));
}

/// One thing in an archive.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Cleaned: `/`-separated, relative, no `..`.
    pub path: String,
    pub dir: bool,
    /// Uncompressed size when the format says (a lone `.gz` doesn't).
    pub size: Option<u64>,
    /// Seconds since 1970.
    pub mtime: Option<i64>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub entries: usize,
    /// Most bytes all files together may add up to.
    pub bytes: u64,
    /// Above this many bytes, an archive that expands more than `ratio` times is refused.
    pub ratio_from: u64,
    pub ratio: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits { entries: 200_000, bytes: 16 << 30, ratio_from: 1 << 30, ratio: 1000 };
}

const SOURCES: [Format; 6] = [Format::Zip, Format::Cbz, Format::Tar, Format::TarGz, Format::Gz, Format::SevenZ];
const TARGETS: [Format; 6] = [Format::Zip, Format::Cbz, Format::Tar, Format::TarGz, Format::SevenZ, Format::Folder];

pub struct ArchiveConverter;

impl Converter for ArchiveConverter {
    /// Every archive to every other kind (a lone `.gz` has no size up front, so not to TAR)
    /// and to a folder; never to itself.
    fn routes(&self) -> Vec<(Format, Format)> {
        let mut r = Vec::new();
        for from in SOURCES {
            for to in TARGETS {
                if from != to && !(from == Format::Gz && matches!(to, Format::Tar | Format::TarGz | Format::Cbz)) {
                    r.push((from, to));
                }
            }
        }
        r
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        run(src, dst, from, to, ctx, Limits::DEFAULT)
    }
}

/// An entry name made safe, or `None` when it can't be (it climbs out, or is empty).
fn safe_path(raw: &str) -> Option<String> {
    let raw = raw.replace('\\', "/");
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            p if p.len() == 2 && p.ends_with(':') && p.as_bytes()[0].is_ascii_alphabetic() => return None, // C:
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Counts what flows through, stops when the user cancels or the limits are crossed.
struct Guard<'a, R: Read + ?Sized> {
    inner: &'a mut R,
    ctx: &'a ConvertCtx<'a>,
    total: &'a Cell<u64>,
    limits: Limits,
    archive_len: u64,
}

impl<R: Read + ?Sized> Read for Guard<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.ctx.check_cancelled().is_err() {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = self.inner.read(buf)?;
        let total = self.total.get() + n as u64;
        self.total.set(total);
        let bomb = total > self.limits.bytes || (total > self.limits.ratio_from && total / self.archive_len.max(1) > self.limits.ratio);
        if bomb {
            return Err(std::io::Error::other("archive bomb"));
        }
        Ok(n)
    }
}

/// Stem to name an extracted folder: the archive's name without `.zip`, `.tar.gz` and so on.
fn folder_stem(src: &Path) -> String {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = stem.strip_suffix(".tar").or_else(|| stem.strip_suffix(".TAR")).map(str::to_string).unwrap_or(stem);
    if stem.is_empty() { "extracted".into() } else { stem }
}

pub fn run(src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx, limits: Limits) -> Result<()> {
    let preset = ctx.options.preset;
    let mut sink: Box<dyn Sink> = match to {
        Format::Folder => {
            let parent = dst.parent().map(Path::to_path_buf).unwrap_or_default();
            Box::new(write::FolderSink::create(&parent, &folder_stem(src))?)
        }
        Format::Zip | Format::Cbz => Box::new(write::ZipSink::create(dst, preset)?),
        Format::Tar => Box::new(write::TarSink::create(dst)?),
        Format::TarGz => Box::new(write::TarSink::create_gz(dst, preset)?),
        Format::SevenZ => Box::new(write::SevenSink::create(dst)?),
        _ => return Err(Error::NoRoute { from, to }),
    };
    let archive_len = std::fs::metadata(src).map_or(1, |m| m.len());
    let total = Cell::new(0u64);
    let (mut files, mut dirs, mut unsafe_names, mut links) = (0usize, 0usize, 0usize, 0usize);
    (ctx.progress)(0.05);
    let result = read::walk(src, from, &mut links, &mut |entry, data| {
        ctx.check_cancelled()?;
        if files + dirs >= limits.entries {
            return Err(Error::Codec(format!("this archive holds more than {} files, which is refused as a precaution", limits.entries)));
        }
        let Some(path) = safe_path(&entry.path) else {
            unsafe_names += 1;
            return Ok(());
        };
        let entry = Entry { path, ..entry.clone() };
        let mut guard = Guard { inner: data, ctx, total: &total, limits, archive_len };
        sink.add(&entry, &mut guard).map_err(|e| match e {
            Error::Io { source, .. } if source.kind() == std::io::ErrorKind::Interrupted => Error::Cancelled,
            Error::Io { source, .. } if source.to_string() == "archive bomb" => bomb(limits),
            other => other,
        })?;
        if entry.dir { dirs += 1 } else { files += 1 }
        Ok(())
    });
    // Guard errors surface inside the readers of some formats; give them their message too.
    let result = result.map_err(|e| match &e {
        Error::Codec(m) if m.contains("archive bomb") => bomb(limits),
        _ => e,
    });
    match result {
        Ok(()) => {
            if unsafe_names > 0 {
                ctx.note(format!("{unsafe_names} entr{} with an unsafe name (leaving the folder) {} skipped", if unsafe_names == 1 { "y" } else { "ies" }, if unsafe_names == 1 { "was" } else { "were" }));
            }
            if links > 0 {
                ctx.note(format!("{links} link{} {} skipped (links in archives aren't followed)", if links == 1 { "" } else { "s" }, if links == 1 { "was" } else { "were" }));
            }
            if files + dirs == 0 {
                ctx.note("The archive is empty");
            }
            let shown: Option<PathBuf> = sink.finish()?;
            if let Some(folder) = shown {
                let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ctx.note(format!("Extracted {files} file{} to the folder \"{name}\"", if files == 1 { "" } else { "s" }));
                ctx.set_output(folder);
            }
            (ctx.progress)(1.0);
            Ok(())
        }
        Err(e) => {
            sink.abort();
            Err(e)
        }
    }
}

fn bomb(limits: Limits) -> Error {
    Error::Codec(format!(
        "this archive expands to an unreasonable size (over {} GB, or more than {}× its own size): refused as a precaution",
        limits.bytes >> 30,
        limits.ratio
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-archive-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn convert(src: &Path, from: Format, to: Format, limits: Limits) -> (Result<()>, PathBuf, Vec<String>) {
        let (opts, cancel, progress) = (dotty_core::Options::default(), AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let dst = dotty_core::output::output_path(src, to, &opts).unwrap();
        let r = run(src, &dst, from, to, &ctx, limits);
        let out = ctx.take_output().unwrap_or_else(|| dst.clone());
        if out != dst {
            let _ = std::fs::remove_file(&dst);
        }
        (r, out, ctx.take_notes())
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            if name.ends_with('/') {
                w.add_directory(name.trim_end_matches('/'), opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap();
    }

    /// Every file under `root` as (relative path, contents), sorted.
    fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn go(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    go(&p, root, out);
                } else {
                    out.push((p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read(&p).unwrap()));
                }
            }
        }
        let mut v = Vec::new();
        go(root, root, &mut v);
        v.sort();
        v
    }

    const FILES: [(&str, &[u8]); 4] = [("readme.txt", b"hello"), ("docs/", b""), ("docs/a.txt", b"alpha alpha alpha"), ("docs/deep/b.bin", &[0, 1, 2, 3, 255])];

    #[test]
    fn contents_survive_every_format_and_a_folder() {
        let dir = tmp("round");
        let zip = dir.join("start.zip");
        make_zip(&zip, &FILES);
        let want = vec![("docs/a.txt".to_string(), b"alpha alpha alpha".to_vec()), ("docs/deep/b.bin".to_string(), vec![0, 1, 2, 3, 255]), ("readme.txt".to_string(), b"hello".to_vec())];

        // zip → tar → tar.gz → 7z → cbz → zip, then out to a folder.
        let mut current = (zip.clone(), Format::Zip);
        for to in [Format::Tar, Format::TarGz, Format::SevenZ, Format::Cbz, Format::Zip] {
            let (r, out, _) = convert(&current.0, current.1, to, Limits::DEFAULT);
            r.unwrap_or_else(|e| panic!("{:?} → {to}: {e}", current.1));
            assert_eq!(Format::detect(&out), Some(to), "{to} recognised ({})", out.display());
            current = (out, to);
        }
        let (r, folder, notes) = convert(&current.0, Format::Zip, Format::Folder, Limits::DEFAULT);
        r.unwrap();
        assert!(folder.is_dir());
        assert_eq!(tree(&folder), want);
        assert!(notes.iter().any(|n| n.contains("Extracted 3 files")), "{notes:?}");
        // A second extraction doesn't touch the first.
        let (_, again, _) = convert(&current.0, Format::Zip, Format::Folder, Limits::DEFAULT);
        assert_ne!(again, folder);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsafe_names_never_leave_the_folder() {
        let dir = tmp("evil");
        let zip = dir.join("evil.zip");
        make_zip(&zip, &[("../escape.txt", b"x"), ("/abs.txt", b"x"), ("..\\win.txt", b"x"), ("C:/drive.txt", b"x"), ("ok/./fine.txt", b"fine"), ("nul.txt", b"n")]);
        let (r, folder, notes) = convert(&zip, Format::Zip, Format::Folder, Limits::DEFAULT);
        r.unwrap();
        assert!(!dir.join("escape.txt").exists() && !dir.parent().unwrap().join("escape.txt").exists());
        let names: Vec<String> = tree(&folder).into_iter().map(|t| t.0).collect();
        assert!(names.contains(&"ok/fine.txt".to_string()), "{names:?}");
        assert!(names.contains(&"abs.txt".to_string()), "a leading slash is just dropped: {names:?}");
        assert!(notes.iter().any(|n| n.contains("unsafe name")), "{notes:?}");
        assert_eq!(names, ["_nul.txt", "abs.txt", "ok/fine.txt"], "reserved Windows names are prefixed, escaping ones are gone");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn links_are_skipped_in_tars() {
        let dir = tmp("links");
        let tar_path = dir.join("l.tar");
        {
            let mut b = tar::Builder::new(std::fs::File::create(&tar_path).unwrap());
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o644);
            b.append_data(&mut h, "real.txt", &b"hi"[..]).unwrap();
            let mut l = tar::Header::new_gnu();
            l.set_entry_type(tar::EntryType::Symlink);
            l.set_size(0);
            b.append_link(&mut l, "link", "/etc/passwd").unwrap();
            b.finish().unwrap();
        }
        let (r, folder, notes) = convert(&tar_path, Format::Tar, Format::Folder, Limits::DEFAULT);
        r.unwrap();
        assert_eq!(tree(&folder).len(), 1);
        assert!(notes.iter().any(|n| n.contains("link")), "{notes:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bombs_and_huge_counts_are_refused_and_leave_nothing() {
        let dir = tmp("bomb");
        let zip = dir.join("big.zip");
        let zeros = vec![0u8; 5_000_000];
        make_zip(&zip, &[("zeros.bin", &zeros)]);
        let tiny = Limits { entries: 100, bytes: 1_000_000, ratio_from: 100, ratio: 10 };
        let (r, folder, _) = convert(&zip, Format::Zip, Format::Folder, tiny);
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("unreasonable size"), "{msg}");
        let _ = folder;
        assert!(!dir.join("big").exists(), "the partly extracted folder is removed");
        let ratio_only = Limits { bytes: u64::MAX, ratio_from: 1000, ratio: 50, ..Limits::DEFAULT };
        assert!(convert(&zip, Format::Zip, Format::Tar, ratio_only).0.unwrap_err().to_string().contains("unreasonable"));
        let (r, out, _) = convert(&zip, Format::Zip, Format::Tar, Limits { entries: 0, ..Limits::DEFAULT });
        assert!(r.unwrap_err().to_string().contains("more than 0 files"));
        assert!(!out.exists(), "a failed archive isn't left behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_lone_gz_unpacks_and_broken_archives_say_so() {
        let dir = tmp("gz");
        let gz = dir.join("notes.txt.gz");
        let mut e = flate2::write::GzEncoder::new(std::fs::File::create(&gz).unwrap(), flate2::Compression::default());
        e.write_all(b"plain text").unwrap();
        e.finish().unwrap();
        let (r, folder, _) = convert(&gz, Format::Gz, Format::Folder, Limits::DEFAULT);
        r.unwrap();
        assert_eq!(tree(&folder), vec![("notes.txt".to_string(), b"plain text".to_vec())]);
        let (r, out, _) = convert(&gz, Format::Gz, Format::Zip, Limits::DEFAULT);
        r.unwrap();
        assert_eq!(Format::detect(&out), Some(Format::Zip));

        let junk = dir.join("junk.zip");
        std::fs::write(&junk, b"PK\x03\x04 not really").unwrap();
        let msg = convert(&junk, Format::Zip, Format::Folder, Limits::DEFAULT).0.unwrap_err().to_string();
        assert!(msg.contains("not a valid zip"), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names_are_made_safe_for_every_file_system() {
        assert_eq!(safe_path("a//b/./c"), Some("a/b/c".into()));
        assert_eq!(safe_path("a/../b"), None);
        assert_eq!(safe_path("D:/x"), None);
        assert_eq!(safe_path("///"), None);
        assert_eq!(folder_stem(Path::new("/a/b.tar.gz")), "b");
        assert_eq!(folder_stem(Path::new("/a/data.zip")), "data");
    }
}
