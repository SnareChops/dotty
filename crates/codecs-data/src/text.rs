//! Reading text files whatever their encoding: UTF-8 (with or without a byte-order mark),
//! UTF-16 with one, else Windows-1252, which old subtitle and CSV files very often are.

use dotty_core::{ConvertCtx, Error, Result};
use std::path::Path;

pub fn read(path: &Path, ctx: &ConvertCtx) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io { path: path.into(), source })?;
    Ok(decode(&bytes, ctx))
}

pub fn decode(bytes: &[u8], ctx: &ConvertCtx) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    for (bom, enc) in [([0xFF, 0xFE], encoding_rs::UTF_16LE), ([0xFE, 0xFF], encoding_rs::UTF_16BE)] {
        if let Some(rest) = bytes.strip_prefix(&bom) {
            return enc.decode_without_bom_handling(rest).0.into_owned();
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            ctx.note("The file isn't UTF-8; it was read as Windows-1252 (Western European)");
            encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned()
        }
    }
}

pub fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|source| Error::Io { path: path.into(), source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn encodings_are_recognised() {
        let (opts, cancel, progress) = (dotty_core::Options::default(), AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        assert_eq!(decode("\u{feff}héllo".as_bytes(), &ctx), "héllo");
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("héllo".encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(decode(&utf16, &ctx), "héllo");
        assert!(ctx.take_notes().is_empty());
        assert_eq!(decode(b"caf\xe9", &ctx), "café");
        assert_eq!(ctx.take_notes().len(), 1);
    }
}
