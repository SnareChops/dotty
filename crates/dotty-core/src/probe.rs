//! Looking inside containers that hold either audio or video (MP4/MOV, Matroska/WebM) to
//! tell which: the first bytes only say which container it is.
//!
//! Only box/element headers and a few small fields are read; media data is skipped by
//! seeking, so this is quick even for huge files. Malformed files just give `None`.

use crate::Format;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// More boxes or elements than this means something is wrong (or hostile).
const MAX_ITEMS: usize = 4096;

/// The track kinds found in a container.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tracks {
    pub video: bool,
    pub audio: bool,
}

/// Turn a container family found by sniffing into the exact format: a file with a video
/// track is a video, one with only audio stays an audio format.
pub fn refine(path: &Path, sniffed: Format) -> Format {
    match sniffed {
        Format::Mp4 | Format::Mov | Format::M4a => match iso_tracks(path) {
            Some(t) if t.video => {
                if sniffed == Format::Mov {
                    Format::Mov
                } else {
                    Format::Mp4
                }
            }
            Some(t) if t.audio => Format::M4a,
            _ => sniffed,
        },
        Format::Mkv | Format::Webm | Format::Mka => match matroska(path) {
            Some((webm, t)) if t.video => {
                if webm {
                    Format::Webm
                } else {
                    Format::Mkv
                }
            }
            Some((_, t)) if t.audio => Format::Mka,
            _ => sniffed,
        },
        f => f,
    }
}

fn open(path: &Path) -> Option<(BufReader<File>, u64)> {
    let f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    Some((BufReader::new(f), len))
}

fn read_exact<const N: usize>(r: &mut impl Read) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b).ok()?;
    Some(b)
}

/// ISO base media (MP4, MOV, M4A): the handler type of every track (`vide`, `soun`).
pub fn iso_tracks(path: &Path) -> Option<Tracks> {
    let (mut r, len) = open(path)?;
    let moov = find_box(&mut r, 0, len, b"moov")?;
    let mut tracks = Tracks::default();
    let mut found = false;
    for_each_box(&mut r, moov.0, moov.1, |r, kind, start, end| {
        if &kind == b"trak"
            && let Some(mdia) = find_box(r, start, end, b"mdia")
            && let Some(hdlr) = find_box(r, mdia.0, mdia.1, b"hdlr")
        {
            // version/flags (4), pre_defined (4), handler_type (4)
            r.seek(SeekFrom::Start(hdlr.0 + 8)).ok()?;
            match &read_exact::<4>(r)? {
                b"vide" => tracks.video = true,
                b"soun" => tracks.audio = true,
                _ => {}
            }
            found = true;
        }
        Some(())
    })?;
    found.then_some(tracks)
}

/// The first box of `kind` among those between `start` and `end`: its payload range.
fn find_box(r: &mut BufReader<File>, start: u64, end: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    let mut hit = None;
    for_each_box(r, start, end, |_, k, s, e| {
        if hit.is_none() && &k == kind {
            hit = Some((s, e));
        }
        Some(())
    })?;
    hit
}

/// Call `f(reader, kind, payload start, payload end)` for each box between `start` and `end`.
fn for_each_box(
    r: &mut BufReader<File>,
    start: u64,
    end: u64,
    mut f: impl FnMut(&mut BufReader<File>, [u8; 4], u64, u64) -> Option<()>,
) -> Option<()> {
    let mut pos = start;
    for _ in 0..MAX_ITEMS {
        if pos + 8 > end {
            return Some(());
        }
        r.seek(SeekFrom::Start(pos)).ok()?;
        let head = read_exact::<8>(r)?;
        let kind = [head[4], head[5], head[6], head[7]];
        let (size, header) = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
            0 => (end - pos, 8), // to the end of the enclosing box/file
            1 => (u64::from_be_bytes(read_exact::<8>(r)?), 16),
            n => (n as u64, 8),
        };
        if size < header || pos + size > end {
            return None;
        }
        f(r, kind, pos + header, pos + size)?;
        pos += size;
    }
    None
}

/// Matroska/WebM: whether the DocType is "webm", and the track kinds.
pub fn matroska(path: &Path) -> Option<(bool, Tracks)> {
    const EBML: u32 = 0x1A45_DFA3;
    const DOC_TYPE: u32 = 0x4282;
    const SEGMENT: u32 = 0x1853_8067;
    const TRACKS: u32 = 0x1654_AE6B;
    const TRACK_ENTRY: u32 = 0xAE;
    const TRACK_TYPE: u32 = 0x83;
    const CLUSTER: u32 = 0x1F43_B675;

    let (mut r, len) = open(path)?;
    let (id, size) = element(&mut r)?;
    if id != EBML {
        return None;
    }
    let header_end = r.stream_position().ok()?.checked_add(size?)?;
    let mut webm = false;
    while r.stream_position().ok()? < header_end {
        let (id, size) = element(&mut r)?;
        let size = size?;
        if id == DOC_TYPE {
            let mut v = vec![0u8; size.min(32) as usize];
            r.read_exact(&mut v).ok()?;
            webm = v.starts_with(b"webm");
            r.seek(SeekFrom::Current(size as i64 - v.len() as i64)).ok()?;
        } else {
            r.seek(SeekFrom::Current(size as i64)).ok()?;
        }
    }
    r.seek(SeekFrom::Start(header_end)).ok()?;
    let (id, size) = element(&mut r)?;
    if id != SEGMENT {
        return None;
    }
    // Unknown-size segments (live recordings) run to the end of the file.
    let seg_end = size.map_or(len, |s| r.stream_position().unwrap_or(len).saturating_add(s).min(len));
    for _ in 0..MAX_ITEMS {
        if r.stream_position().ok()? >= seg_end {
            return None;
        }
        let (id, size) = element(&mut r)?;
        match id {
            TRACKS => {
                let end = r.stream_position().ok()? + size?;
                let mut tracks = Tracks::default();
                while r.stream_position().ok()? < end {
                    let (id, size) = element(&mut r)?;
                    let size = size?;
                    if id != TRACK_ENTRY {
                        r.seek(SeekFrom::Current(size as i64)).ok()?;
                        continue;
                    }
                    let entry_end = r.stream_position().ok()? + size;
                    while r.stream_position().ok()? < entry_end {
                        let (id, size) = element(&mut r)?;
                        let size = size?;
                        if id == TRACK_TYPE && (1..=8).contains(&size) {
                            let mut v = [0u8; 8];
                            r.read_exact(&mut v[8 - size as usize..]).ok()?;
                            match u64::from_be_bytes(v) {
                                1 => tracks.video = true,
                                2 => tracks.audio = true,
                                _ => {}
                            }
                        } else {
                            r.seek(SeekFrom::Current(size as i64)).ok()?;
                        }
                    }
                    r.seek(SeekFrom::Start(entry_end)).ok()?;
                }
                return Some((webm, tracks));
            }
            // Media data before any track list: give up rather than scan the whole file.
            CLUSTER => return None,
            _ => {
                r.seek(SeekFrom::Current(size? as i64)).ok()?;
            }
        }
    }
    None
}

/// An EBML element header: its ID (marker bits kept) and size (`None` = unknown).
fn element(r: &mut impl Read) -> Option<(u32, Option<u64>)> {
    let first = read_exact::<1>(r)?[0];
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut id = first as u32;
    for _ in 1..len {
        id = id << 8 | read_exact::<1>(r)?[0] as u32;
    }
    let first = read_exact::<1>(r)?[0];
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut size = (first as u64) & (0xFF >> len);
    let mut all_ones = size == (0xFF >> len) as u64;
    for _ in 1..len {
        let b = read_exact::<1>(r)?[0];
        all_ones &= b == 0xFF;
        size = size << 8 | b as u64;
    }
    Some((id, (!all_ones).then_some(size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tf-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// A box with a 32-bit size.
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn trak(handler: &[u8; 4]) -> Vec<u8> {
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0u8; 13]);
        bx(b"trak", &bx(b"mdia", &[bx(b"mdhd", &[0; 24]), bx(b"hdlr", &hdlr)].concat()))
    }

    fn mp4(brand: &[u8; 4], tracks: &[&[u8; 4]]) -> Vec<u8> {
        let ftyp = bx(b"ftyp", &[brand.as_slice(), &[0, 0, 0, 0]].concat());
        let traks: Vec<u8> = tracks.iter().flat_map(|h| trak(h)).collect();
        // Media data first and a 64-bit "free" box, as some muxers write.
        let mut big = vec![0, 0, 0, 1];
        big.extend_from_slice(b"free");
        big.extend_from_slice(&24u64.to_be_bytes());
        big.extend_from_slice(&[0; 8]);
        [ftyp, bx(b"mdat", &[7; 100]), big, bx(b"moov", &[bx(b"mvhd", &[0; 100]), traks].concat())].concat()
    }

    /// An EBML element with a one-byte size (payloads under 127 bytes).
    fn el(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect();
        v.push(0x80 | payload.len() as u8);
        v.extend_from_slice(payload);
        v
    }

    fn mkv(doc: &str, types: &[u8]) -> Vec<u8> {
        let header = el(0x1A45_DFA3, &el(0x4282, doc.as_bytes()));
        let entries: Vec<u8> = types.iter().flat_map(|&t| el(0xAE, &[el(0xD7, &[1]), el(0x83, &[t])].concat())).collect();
        // Unknown-size segment, as live recordings have.
        let mut seg = vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        seg.extend(el(0x1549_A966, &[0; 10]));
        seg.extend(el(0x1654_AE6B, &entries));
        [header, seg].concat()
    }

    #[test]
    fn mp4_files_are_video_or_audio_by_their_tracks() {
        let v = write("v.mp4", &mp4(b"isom", &[b"vide", b"soun"]));
        assert_eq!(refine(&v, Format::Mp4), Format::Mp4);
        let a = write("a.mp4", &mp4(b"isom", &[b"soun"]));
        assert_eq!(refine(&a, Format::Mp4), Format::M4a);
        let m = write("m.m4a", &mp4(b"M4A ", &[b"vide", b"soun"]));
        assert_eq!(refine(&m, Format::M4a), Format::Mp4, "a video track wins over the brand");
        let q = write("q.mov", &mp4(b"qt  ", &[b"vide"]));
        assert_eq!(refine(&q, Format::Mov), Format::Mov);
        let junk = write("j.mp4", b"\0\0\0\x10ftypisom\0\0\0\0\xff\xff\xff\xffmoov");
        assert_eq!(refine(&junk, Format::Mp4), Format::Mp4, "unreadable: keep the sniffed guess");
    }

    #[test]
    fn matroska_files_are_video_or_audio_by_their_tracks() {
        assert_eq!(refine(&write("v.webm", &mkv("webm", &[1, 2])), Format::Mkv), Format::Webm);
        assert_eq!(refine(&write("v.mkv", &mkv("matroska", &[2, 1])), Format::Mkv), Format::Mkv);
        assert_eq!(refine(&write("a.webm", &mkv("webm", &[2])), Format::Mkv), Format::Mka);
        assert_eq!(matroska(&write("t.mkv", &mkv("matroska", &[1, 2, 2]))), Some((false, Tracks { video: true, audio: true })));
    }
}
