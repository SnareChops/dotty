//! Where the entries go: a folder, or a zip / tar / tar.gz / 7z file.

use crate::{Entry, read::civil};
use dotty_core::{Error, Result};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub trait Sink {
    fn add(&mut self, e: &Entry, data: &mut dyn Read) -> Result<()>;
    /// Close it; returns what to show the user when that isn't the requested file (a folder).
    fn finish(self: Box<Self>) -> Result<Option<PathBuf>>;
    /// Throw away what was written.
    fn abort(self: Box<Self>);
}

fn io(path: &Path, source: std::io::Error) -> Error {
    Error::Io { path: path.into(), source }
}

fn codec(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Codec(format!("{what}: {e}"))
}

/// Deflate level for a quality preset.
pub fn level(preset: dotty_core::Preset) -> u32 {
    match preset {
        dotty_core::Preset::Low => 1,
        dotty_core::Preset::Medium => 6,
        _ => 9,
    }
}

// ---- folder ----

pub struct FolderSink {
    root: PathBuf,
}

impl FolderSink {
    /// Make a new folder named `stem` (or `stem (1)`, …) in `parent`; creating it claims the name.
    pub fn create(parent: &Path, stem: &str) -> Result<Self> {
        for n in 0.. {
            let path = parent.join(if n == 0 { stem.to_string() } else { format!("{stem} ({n})") });
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { root: path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io(&path, e)),
            }
        }
        unreachable!("unbounded range")
    }
}

/// A path component fit for every file system we run on.
fn component(c: &str) -> String {
    let mut s: String = c.chars().map(|ch| if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { ch }).collect();
    s = s.trim_end_matches([' ', '.']).to_string();
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit())) {
        s.insert(0, '_');
    }
    if s.is_empty() { "_".into() } else { s }
}

impl Sink for FolderSink {
    fn add(&mut self, e: &Entry, data: &mut dyn Read) -> Result<()> {
        let mut path = self.root.clone();
        for part in e.path.split('/') {
            path.push(component(part));
        }
        if e.dir {
            return std::fs::create_dir_all(&path).map_err(|err| io(&path, err));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| io(parent, err))?;
        }
        let file = File::create(&path).map_err(|err| io(&path, err))?;
        let mut w = BufWriter::new(&file);
        std::io::copy(data, &mut w).map_err(|err| io(&path, err))?;
        w.flush().map_err(|err| io(&path, err))?;
        drop(w);
        if let Some(m) = e.mtime.filter(|&m| m > 0) {
            let _ = file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(m as u64));
        }
        #[cfg(unix)]
        if let Some(mode) = e.mode.filter(|m| m & 0o111 != 0) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644 | (mode & 0o111)));
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Option<PathBuf>> {
        Ok(Some(self.root))
    }

    fn abort(self: Box<Self>) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---- zip ----

pub struct ZipSink {
    w: zip::ZipWriter<BufWriter<File>>,
    level: i64,
    path: PathBuf,
}

impl ZipSink {
    pub fn create(path: &Path, preset: dotty_core::Preset) -> Result<Self> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        Ok(Self { w: zip::ZipWriter::new(BufWriter::new(file)), level: level(preset) as i64, path: path.into() })
    }
}

impl Sink for ZipSink {
    fn add(&mut self, e: &Entry, data: &mut dyn Read) -> Result<()> {
        let mut opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(self.level));
        if let Some(m) = e.mtime.filter(|&m| m >= 315_532_800) {
            let (y, mo, d, h, mi, s) = civil(m);
            if let Ok(t) = zip::DateTime::from_date_and_time(y as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8) {
                opts = opts.last_modified_time(t);
            }
        }
        if let Some(mode) = e.mode {
            opts = opts.unix_permissions(mode & 0o7777);
        }
        if e.size.is_none_or(|s| s >= 1 << 32) {
            opts = opts.large_file(true);
        }
        if e.dir {
            return self.w.add_directory(e.path.trim_end_matches('/'), opts).map_err(|err| codec("zip", err));
        }
        self.w.start_file(&e.path, opts).map_err(|err| codec("zip", err))?;
        std::io::copy(data, &mut self.w).map_err(|err| io(&self.path, err))?;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Option<PathBuf>> {
        let mut inner = self.w.finish().map_err(|e| codec("zip", e))?;
        inner.flush().map_err(|e| io(&self.path, e))?;
        Ok(None)
    }

    fn abort(self: Box<Self>) {
        drop(self.w);
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---- tar ----

/// Close the stream under a tar (gzip needs its trailer written).
pub trait Close {
    fn close(self) -> std::io::Result<()>;
}

impl Close for BufWriter<File> {
    fn close(mut self) -> std::io::Result<()> {
        self.flush()
    }
}

impl Close for flate2::write::GzEncoder<BufWriter<File>> {
    fn close(self) -> std::io::Result<()> {
        self.finish()?.flush()
    }
}

pub struct TarSink<W: Write + Close> {
    b: tar::Builder<W>,
    path: PathBuf,
}

impl TarSink<BufWriter<File>> {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        Ok(Self { b: tar::Builder::new(BufWriter::new(file)), path: path.into() })
    }
}

impl TarSink<flate2::write::GzEncoder<BufWriter<File>>> {
    pub fn create_gz(path: &Path, preset: dotty_core::Preset) -> Result<Self> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        let enc = flate2::write::GzEncoder::new(BufWriter::new(file), flate2::Compression::new(level(preset)));
        Ok(Self { b: tar::Builder::new(enc), path: path.into() })
    }
}

impl<W: Write + Close> Sink for TarSink<W> {
    fn add(&mut self, e: &Entry, data: &mut dyn Read) -> Result<()> {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(e.mtime.unwrap_or(0).max(0) as u64);
        if e.dir {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(e.mode.unwrap_or(0o755) & 0o7777);
            h.set_size(0);
            let path = format!("{}/", e.path.trim_end_matches('/'));
            return self.b.append_data(&mut h, path, std::io::empty()).map_err(|err| io(&self.path, err));
        }
        // A tar header carries the size first, so the entry must have one.
        let size = e.size.ok_or_else(|| Error::Codec("a tar file needs to know each file's size up front".into()))?;
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(e.mode.unwrap_or(0o644) & 0o7777);
        h.set_size(size);
        self.b.append_data(&mut h, &e.path, data.take(size)).map_err(|err| io(&self.path, err))
    }

    fn finish(self: Box<Self>) -> Result<Option<PathBuf>> {
        let inner = self.b.into_inner().map_err(|e| io(&self.path, e))?;
        inner.close().map_err(|e| io(&self.path, e))?;
        Ok(None)
    }

    fn abort(self: Box<Self>) {
        drop(self.b);
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---- 7z ----

pub struct SevenSink {
    w: sevenz_rust2::ArchiveWriter<BufWriter<File>>,
    path: PathBuf,
}

impl SevenSink {
    pub fn create(path: &Path) -> Result<Self> {
        let file = File::create(path).map_err(|e| io(path, e))?;
        let w = sevenz_rust2::ArchiveWriter::new(BufWriter::new(file)).map_err(|e| codec("7z", e))?;
        Ok(Self { w, path: path.into() })
    }
}

impl Sink for SevenSink {
    fn add(&mut self, e: &Entry, data: &mut dyn Read) -> Result<()> {
        if e.dir {
            self.w.push_archive_entry::<&[u8]>(sevenz_rust2::ArchiveEntry::new_directory(e.path.trim_end_matches('/')), None).map_err(|err| codec("7z", err))?;
        } else {
            self.w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(&e.path), Some(data)).map_err(|err| codec("7z", err))?;
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Option<PathBuf>> {
        let mut inner = self.w.finish().map_err(|e| io(&self.path, e))?;
        inner.flush().map_err(|e| io(&self.path, e))?;
        Ok(None)
    }

    fn abort(self: Box<Self>) {
        drop(self.w);
        let _ = std::fs::remove_file(&self.path);
    }
}
