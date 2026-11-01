use crate::{Format, Options};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// `stem`, then ` (n)` for `n > 0`, then `suffix`.
fn numbered(stem: &std::ffi::OsStr, n: u32, suffix: &str) -> OsString {
    let mut name = stem.to_os_string();
    if n > 0 {
        name.push(format!(" ({n})"));
    }
    name.push(suffix);
    name
}

/// Pick and claim the output path: sibling of the source (or `dest_dir`), same stem, target
/// extension, never overwriting anything (`name (1).ext`). The name is claimed by creating
/// an empty file, so jobs running at the same time can't pick the same one; the converter
/// then writes over it.
pub fn output_path(src: &Path, to: Format, opts: &Options) -> std::io::Result<PathBuf> {
    let dir = match &opts.dest_dir {
        Some(d) => d.clone(),
        None => src.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    // file_stem never contains a separator, so this cannot escape `dir`.
    let stem = src.file_stem().unwrap_or("output".as_ref());
    let suffix = format!(".{}", to.extension());
    for n in 0.. {
        let path = dir.join(numbered(stem, n, &suffix));
        if path == src {
            continue;
        }
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("unbounded range")
}

/// Create a fresh folder next to `dst` to hold one file per animation frame
/// (`name-frames`, `name-frames (1)`, …). Creating it is what claims the name.
pub fn frames_dir(dst: &Path) -> std::io::Result<PathBuf> {
    let dir = dst.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = dst.file_stem().unwrap_or("output".as_ref());
    let mut base = stem.to_os_string();
    base.push("-frames");
    for n in 0.. {
        let path = dir.join(numbered(&base, n, ""));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("unbounded range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_with_new_extension_and_no_overwrite() {
        let dir = std::env::temp_dir().join(format!("tf-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("pic.png");
        std::fs::write(&src, b"x").unwrap();
        let opts = Options::default();

        assert_eq!(output_path(&src, Format::Jpeg, &opts).unwrap(), dir.join("pic.jpg"));
        assert!(dir.join("pic.jpg").exists(), "the name is claimed straight away");
        // A second job asking at the same time gets the next name.
        assert_eq!(output_path(&src, Format::Jpeg, &opts).unwrap(), dir.join("pic (1).jpg"));
        // Same format must not clobber the source.
        assert_eq!(output_path(&src, Format::Png, &opts).unwrap(), dir.join("pic (1).png"));
        assert_eq!(std::fs::read(&src).unwrap(), b"x");

        assert_eq!(frames_dir(&dir.join("pic.jpg")).unwrap(), dir.join("pic-frames"));
        assert_eq!(frames_dir(&dir.join("pic.jpg")).unwrap(), dir.join("pic-frames (1)"));
        assert!(dir.join("pic-frames (1)").is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn keeps_names_that_are_not_valid_unicode() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("tf-out-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9.png"));
        std::fs::write(&src, b"x").unwrap();
        let out = output_path(&src, Format::Jpeg, &Options::default()).unwrap();
        assert_eq!(out, dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9.jpg")));
        std::fs::remove_dir_all(&dir).ok();
    }
}
