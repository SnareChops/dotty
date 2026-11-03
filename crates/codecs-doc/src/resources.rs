//! Where a document's pictures come from: files beside it, or entries inside an EPUB.

use crate::layout::resolve;
use std::cell::RefCell;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// The prefix of a picture source that lives inside the EPUB the document was read from.
pub const EPUB_PREFIX: &str = "epub:/";

pub struct Resources {
    dir: PathBuf,
    epub: Option<RefCell<zip::ZipArchive<BufReader<File>>>>,
}

impl Resources {
    /// Pictures next to a document in `dir`.
    pub fn files(dir: &Path) -> Self {
        Self { dir: dir.to_path_buf(), epub: None }
    }

    pub fn epub(dir: &Path, zip: zip::ZipArchive<BufReader<File>>) -> Self {
        Self { dir: dir.to_path_buf(), epub: Some(RefCell::new(zip)) }
    }

    pub fn is_remote(src: &str) -> bool {
        let l = src.to_ascii_lowercase();
        l.starts_with("http://") || l.starts_with("https://") || l.starts_with("//") || l.starts_with("data:")
    }

    /// A picture that is a plain file on disk (not inside an EPUB).
    pub fn file_path(&self, src: &str) -> Option<PathBuf> {
        (!src.starts_with(EPUB_PREFIX) && !Self::is_remote(src)).then(|| resolve(&self.dir, src))
    }

    /// The bytes of a picture, wherever it is.
    pub fn bytes(&self, src: &str) -> Option<Vec<u8>> {
        if let Some(name) = src.strip_prefix(EPUB_PREFIX) {
            let mut zip = self.epub.as_ref()?.borrow_mut();
            let mut f = zip.by_name(name).ok()?;
            let mut out = Vec::new();
            f.read_to_end(&mut out).ok()?;
            return Some(out);
        }
        std::fs::read(self.file_path(src)?).ok()
    }
}
