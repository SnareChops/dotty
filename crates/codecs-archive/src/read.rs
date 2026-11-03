//! Walking the entries of an archive, whatever its format, streaming each one.

use crate::Entry;
use dotty_core::{Error, Format, Result};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub type Visit<'a> = dyn FnMut(&Entry, &mut dyn Read) -> Result<()> + 'a;

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
    let msg = e.to_string();
    if msg.to_ascii_lowercase().contains("password") || msg.to_ascii_lowercase().contains("encrypt") {
        Error::Codec("this archive is password protected, which isn't supported".into())
    } else {
        Error::Codec(format!("not a valid {what} archive: {msg}"))
    }
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|source| Error::Io { path: path.into(), source })
}

/// Seconds since 1970 for a calendar date and time (UTC), as zip stores them.
pub fn unix_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    // Days from civil (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60 + s
}

/// Calendar date and time (UTC) for seconds since 1970.
pub fn civil(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Call `visit` for each entry of the archive `src` of format `from`. Links are counted in
/// `skipped_links` and not visited.
pub fn walk(src: &Path, from: Format, skipped_links: &mut usize, visit: &mut Visit) -> Result<()> {
    match from {
        Format::Zip | Format::Cbz => zip(src, skipped_links, visit),
        Format::Tar => tar(tar::Archive::new(BufReader::new(open(src)?)), skipped_links, visit),
        Format::TarGz => tar(tar::Archive::new(flate2::read::MultiGzDecoder::new(BufReader::new(open(src)?))), skipped_links, visit),
        Format::Gz => gz(src, visit),
        Format::SevenZ => seven(src, visit),
        _ => Err(Error::UnknownFormat),
    }
}

fn zip(src: &Path, skipped_links: &mut usize, visit: &mut Visit) -> Result<()> {
    let mut archive = zip::ZipArchive::new(BufReader::new(open(src)?)).map_err(|e| bad("zip", e))?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| bad("zip", e))?;
        if f.is_symlink() {
            *skipped_links += 1;
            continue;
        }
        let mtime = f.last_modified().filter(|d| d.is_valid()).map(|d| unix_secs(d.year() as i64, d.month() as i64, d.day() as i64, d.hour() as i64, d.minute() as i64, d.second() as i64));
        let name = f.name().map_err(|e| bad("zip", e))?.into_owned();
        let entry = Entry { path: name, dir: f.is_dir(), size: Some(f.size()), mtime, mode: f.unix_mode() };
        visit(&entry, &mut f)?;
    }
    Ok(())
}

fn tar<R: Read>(mut archive: tar::Archive<R>, skipped_links: &mut usize, visit: &mut Visit) -> Result<()> {
    for entry in archive.entries().map_err(|e| bad("tar", e))? {
        let mut entry = entry.map_err(|e| bad("tar", e))?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            *skipped_links += 1;
            continue;
        }
        if !(kind.is_file() || kind.is_dir() || kind.is_contiguous()) {
            continue; // pax headers, devices, fifos
        }
        let path = entry.path().map_err(|e| bad("tar", e))?.to_string_lossy().into_owned();
        let e = Entry { path, dir: kind.is_dir(), size: entry.header().size().ok(), mtime: entry.header().mtime().ok().map(|m| m as i64), mode: entry.header().mode().ok() };
        visit(&e, &mut entry)?;
    }
    Ok(())
}

fn gz(src: &Path, visit: &mut Visit) -> Result<()> {
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let inner = name.strip_suffix(".gz").or_else(|| name.strip_suffix(".GZ")).filter(|n| !n.is_empty()).unwrap_or("file").to_string();
    let mut dec = flate2::read::MultiGzDecoder::new(BufReader::new(open(src)?));
    let entry = Entry { path: inner, dir: false, size: None, mtime: None, mode: None };
    visit(&entry, &mut dec)
}

fn seven(src: &Path, visit: &mut Visit) -> Result<()> {
    let mut reader = sevenz_rust2::ArchiveReader::open(src, sevenz_rust2::Password::empty()).map_err(|e| bad("7z", e))?;
    let mut failure: Option<Error> = None;
    let result = reader.for_each_entries(|entry, data| {
        let e = Entry { path: entry.name().to_string(), dir: entry.is_directory(), size: Some(entry.size()), mtime: None, mode: None };
        match visit(&e, data) {
            Ok(()) => Ok(true),
            Err(err) => {
                failure = Some(err);
                Ok(false)
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| bad("7z", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_through_unix_seconds() {
        for secs in [0, 86_399, 951_782_400, 1_709_251_199, 1_762_041_600, 4_102_444_800] {
            let (y, mo, d, h, mi, s) = civil(secs);
            assert_eq!(unix_secs(y, mo, d, h, mi, s), secs);
        }
        assert_eq!(civil(1_709_251_199), (2024, 2, 29, 23, 59, 59));
    }
}
