use crate::{Format, Options};
use std::path::{Path, PathBuf};

/// Pick the output path: sibling of the source (or `dest_dir`), same stem,
/// target extension, never overwriting an existing file (`name (1).ext`).
pub fn output_path(src: &Path, to: Format, opts: &Options) -> PathBuf {
    let dir = match &opts.dest_dir {
        Some(d) => d.clone(),
        None => src.parent().map(Path::to_path_buf).unwrap_or_default(),
    };
    // file_stem never contains a separator, so this cannot escape `dir`.
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let ext = to.extension();

    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() && first != src {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}).{ext}")))
        .find(|p| !p.exists())
        .expect("unbounded range")
}

/// A fresh folder next to `dst` to hold one file per animation frame (`name-frames`).
pub fn frames_dir(dst: &Path) -> PathBuf {
    let dir = dst.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = dst.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let first = dir.join(format!("{stem}-frames"));
    if !first.exists() {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("{stem}-frames ({n})")))
        .find(|p| !p.exists())
        .expect("unbounded range")
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

        assert_eq!(output_path(&src, Format::Jpeg, &opts), dir.join("pic.jpg"));
        std::fs::write(dir.join("pic.jpg"), b"x").unwrap();
        assert_eq!(output_path(&src, Format::Jpeg, &opts), dir.join("pic (1).jpg"));
        // Same format must not clobber the source.
        assert_eq!(output_path(&src, Format::Png, &opts), dir.join("pic (1).png"));

        assert_eq!(frames_dir(&dir.join("pic.jpg")), dir.join("pic-frames"));
        std::fs::create_dir(dir.join("pic-frames")).unwrap();
        assert_eq!(frames_dir(&dir.join("pic.jpg")), dir.join("pic-frames (1)"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
