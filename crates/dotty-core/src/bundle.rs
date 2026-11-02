//! Results made of many files (a page per image, an extracted archive, a favicon pack) go
//! into a folder or a zip next to the source. Converters add files by name; the bundle
//! claims a fresh name for itself, so jobs running together never share one.

use crate::{Error, Result};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BundleMode {
    #[default]
    Folder,
    Zip,
}

impl BundleMode {
    pub fn label(self) -> &'static str {
        match self {
            BundleMode::Folder => "Folder",
            BundleMode::Zip => "Zip file",
        }
    }
}

enum Sink {
    Folder(PathBuf),
    Zip(Box<zip::ZipWriter<File>>),
}

pub struct Bundle {
    /// The folder or the zip.
    path: PathBuf,
    sink: Sink,
    first: Option<PathBuf>,
    count: usize,
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io { path: path.to_path_buf(), source }
}

fn zip_err(path: &Path, e: zip::result::ZipError) -> Error {
    Error::Codec(format!("{}: {e}", path.display()))
}

/// A name that can't leave the bundle: no folders, no `..`, no empty or odd names.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let base = base.trim().trim_matches('.');
    if base.is_empty() { "file".to_string() } else { base.chars().map(|c| if c.is_control() || c == ':' { '_' } else { c }).collect() }
}

fn numbered(stem: &str, n: u32, suffix: &str) -> String {
    if n == 0 { format!("{stem}{suffix}") } else { format!("{stem} ({n}){suffix}") }
}

impl Bundle {
    /// Start a bundle beside `dst`: a folder named `<stem>-<what>` or a zip `<stem>-<what>.zip`
    /// (`what` like "frames" or "pages"). Creating it is what claims the name.
    pub fn create(dst: &Path, what: &str, mode: BundleMode) -> Result<Self> {
        let dir = dst.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = dst.file_stem().map_or_else(|| "output".to_string(), |s| s.to_string_lossy().into_owned());
        let base = format!("{stem}-{what}");
        for n in 0.. {
            match mode {
                BundleMode::Folder => {
                    let path = dir.join(numbered(&base, n, ""));
                    match std::fs::create_dir(&path) {
                        Ok(()) => return Ok(Self { sink: Sink::Folder(path.clone()), path, first: None, count: 0 }),
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                        Err(e) => return Err(io_err(&path, e)),
                    }
                }
                BundleMode::Zip => {
                    let path = dir.join(numbered(&base, n, ".zip"));
                    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                        Ok(file) => return Ok(Self { sink: Sink::Zip(Box::new(zip::ZipWriter::new(file))), path, first: None, count: 0 }),
                        Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                        Err(e) => return Err(io_err(&path, e)),
                    }
                }
            }
        }
        unreachable!("unbounded range")
    }

    /// Add a file. Names are reduced to a plain file name; use [`Bundle::add_in`] for folders.
    pub fn add(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.add_path(&[], name, bytes)
    }

    /// Add a file inside sub-folders (`parts`), each made safe the same way.
    pub fn add_in(&mut self, parts: &[&str], name: &str, bytes: &[u8]) -> Result<()> {
        self.add_path(parts, name, bytes)
    }

    fn add_path(&mut self, parts: &[&str], name: &str, bytes: &[u8]) -> Result<()> {
        let mut rel: Vec<String> = parts.iter().map(|p| safe_name(p)).collect();
        rel.push(safe_name(name));
        match &mut self.sink {
            Sink::Folder(dir) => {
                let mut path = dir.clone();
                for part in &rel[..rel.len() - 1] {
                    path.push(part);
                }
                std::fs::create_dir_all(&path).map_err(|e| io_err(&path, e))?;
                path.push(&rel[rel.len() - 1]);
                std::fs::write(&path, bytes).map_err(|e| io_err(&path, e))?;
                self.first.get_or_insert(path);
            }
            Sink::Zip(zip) => {
                let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
                zip.start_file(rel.join("/"), options).map_err(|e| zip_err(&self.path, e))?;
                zip.write_all(bytes).map_err(|e| io_err(&self.path, e))?;
            }
        }
        self.count += 1;
        Ok(())
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// What it's called, for notes: the folder or zip's file name.
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Close it; returns what to show the user: the zip, or the first file in the folder.
    pub fn finish(self) -> Result<PathBuf> {
        match self.sink {
            Sink::Folder(dir) => Ok(self.first.unwrap_or(dir)),
            Sink::Zip(zip) => {
                zip.finish().map_err(|e| zip_err(&self.path, e))?;
                Ok(self.path)
            }
        }
    }

    /// Throw away what was written (a failed or cancelled job leaves nothing behind).
    pub fn abort(self) {
        match self.sink {
            Sink::Folder(dir) => {
                let _ = std::fs::remove_dir_all(dir);
            }
            Sink::Zip(zip) => {
                drop(zip);
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-bundle-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_folder_bundle_claims_names_and_keeps_files_inside() {
        let d = dir("folder");
        let dst = d.join("doc.png");
        let mut a = Bundle::create(&dst, "pages", BundleMode::Folder).unwrap();
        let b = Bundle::create(&dst, "pages", BundleMode::Folder).unwrap();
        assert_eq!((a.name(), b.name()), ("doc-pages".into(), "doc-pages (1)".into()));
        a.add("page-1.png", b"1").unwrap();
        a.add("../../escape.png", b"x").unwrap();
        a.add_in(&["sub", ".."], "deep.txt", b"y").unwrap();
        assert_eq!(a.count(), 3);
        assert_eq!(std::fs::read(d.join("doc-pages/escape.png")).unwrap(), b"x", "a path in a name is reduced to its last part");
        assert!(d.join("doc-pages/sub/file/deep.txt").exists(), "a .. part becomes a plain name");
        assert!(!d.parent().unwrap().join("escape.png").exists());
        assert_eq!(a.finish().unwrap(), d.join("doc-pages/page-1.png"));
        b.abort();
        assert!(!d.join("doc-pages (1)").exists(), "aborting removes it");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_zip_bundle_holds_the_files_and_aborting_removes_it() {
        let d = dir("zip");
        let dst = d.join("doc.png");
        let mut z = Bundle::create(&dst, "pages", BundleMode::Zip).unwrap();
        z.add("a.txt", b"hello").unwrap();
        z.add_in(&["inner"], "b.txt", b"world").unwrap();
        let path = z.finish().unwrap();
        assert_eq!(path, d.join("doc-pages.zip"));
        let mut archive = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut text = String::new();
        archive.by_name("a.txt").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello");
        assert!(archive.by_name("inner/b.txt").is_ok());

        let again = Bundle::create(&dst, "pages", BundleMode::Zip).unwrap();
        assert_eq!(again.name(), "doc-pages (1).zip");
        again.abort();
        assert!(!d.join("doc-pages (1).zip").exists());
        std::fs::remove_dir_all(&d).ok();
    }
}
