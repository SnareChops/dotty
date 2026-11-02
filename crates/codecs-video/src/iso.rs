//! What symphonia doesn't tell us about an MP4/MOV video track: the sample entry's codec
//! (it doesn't know `av01`, `vp09` or JPEG), the AV1 configuration record, and the
//! rotation in the track header's matrix.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_ITEMS: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IsoVideo {
    /// The sample entry type: `av01`, `vp09`, `jpeg`, `mjpa`, `avc1`, `hvc1`…
    pub fourcc: [u8; 4],
    /// The `av1C` / `vpcC` record, if any.
    pub config: Vec<u8>,
    /// Clockwise rotation to apply for display: 0, 90, 180 or 270.
    pub rotation: u16,
    /// The track header's size (before rotation).
    pub width: u32,
    pub height: u32,
    /// The track's ID, to match symphonia's track.
    pub track_id: u32,
    /// File offset of the `av1C`/`vpcC` payload (to patch it), when there is one.
    pub config_at: u64,
}

fn read<const N: usize>(r: &mut impl Read) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b).ok()?;
    Some(b)
}

/// Walk the boxes between `start` and `end`, calling `f(kind, payload start, payload end)`.
fn boxes(r: &mut BufReader<File>, start: u64, end: u64, mut f: impl FnMut(&mut BufReader<File>, [u8; 4], u64, u64) -> Option<()>) -> Option<()> {
    let mut pos = start;
    for _ in 0..MAX_ITEMS {
        if pos + 8 > end {
            return Some(());
        }
        r.seek(SeekFrom::Start(pos)).ok()?;
        let h = read::<8>(r)?;
        let kind = [h[4], h[5], h[6], h[7]];
        let (size, hl) = match u32::from_be_bytes([h[0], h[1], h[2], h[3]]) {
            0 => (end - pos, 8),
            1 => (u64::from_be_bytes(read::<8>(r)?), 16),
            n => (n as u64, 8),
        };
        if size < hl || pos + size > end {
            return None;
        }
        f(r, kind, pos + hl, pos + size)?;
        pos += size;
    }
    None
}

fn find(r: &mut BufReader<File>, start: u64, end: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    let mut hit = None;
    boxes(r, start, end, |_, k, s, e| {
        if hit.is_none() && &k == kind {
            hit = Some((s, e));
        }
        Some(())
    })?;
    hit
}

/// Rotation from a `tkhd` matrix `[a b u; c d v; x y w]` (16.16 fixed point).
fn rotation(a: i32, b: i32, c: i32, d: i32) -> u16 {
    let one = 0x10000;
    match (a, b, c, d) {
        (0, x, y, 0) if x == one && y == -one => 90,
        (x, 0, 0, y) if x == -one && y == -one => 180,
        (0, x, y, 0) if x == -one && y == one => 270,
        _ => 0,
    }
}

/// The first video track of an MP4/MOV file.
pub fn video_track(path: &Path) -> Option<IsoVideo> {
    let f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut r = BufReader::new(f);
    let moov = find(&mut r, 0, len, b"moov")?;
    let mut found = None;
    boxes(&mut r, moov.0, moov.1, |r, kind, s, e| {
        if found.is_some() || &kind != b"trak" {
            return Some(());
        }
        let Some(mdia) = find(r, s, e, b"mdia") else { return Some(()) };
        let Some(hdlr) = find(r, mdia.0, mdia.1, b"hdlr") else { return Some(()) };
        r.seek(SeekFrom::Start(hdlr.0 + 8)).ok()?;
        if &read::<4>(r)? != b"vide" {
            return Some(());
        }
        let mut v = IsoVideo::default();
        if let Some(tkhd) = find(r, s, e, b"tkhd") {
            r.seek(SeekFrom::Start(tkhd.0)).ok()?;
            let version = read::<1>(r)?[0];
            // version/flags, times, track ID…: the ID is at 12 (v0) or 20 (v1); the matrix
            // at 40 (v0) or 52 (v1).
            let (id_at, matrix_at) = if version == 1 { (20, 52) } else { (12, 40) };
            r.seek(SeekFrom::Start(tkhd.0 + id_at)).ok()?;
            v.track_id = u32::from_be_bytes(read::<4>(r)?);
            r.seek(SeekFrom::Start(tkhd.0 + matrix_at)).ok()?;
            // The matrix (9 × 4 bytes), then width and height (16.16 fixed point).
            let m = read::<44>(r)?;
            let at = |i: usize| i32::from_be_bytes([m[i * 4], m[i * 4 + 1], m[i * 4 + 2], m[i * 4 + 3]]);
            v.rotation = rotation(at(0), at(1), at(3), at(4));
            (v.width, v.height) = ((at(9) as u32) >> 16, (at(10) as u32) >> 16);
        }
        let minf = find(r, mdia.0, mdia.1, b"minf")?;
        let stbl = find(r, minf.0, minf.1, b"stbl")?;
        let stsd = find(r, stbl.0, stbl.1, b"stsd")?;
        // stsd: version/flags (4), entry count (4), then the first sample entry box.
        r.seek(SeekFrom::Start(stsd.0 + 8)).ok()?;
        let h = read::<8>(r)?;
        let entry_len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        v.fourcc = [h[4], h[5], h[6], h[7]];
        // A visual sample entry has 78 bytes of fields before its child boxes.
        let (es, ee) = (stsd.0 + 8 + 8 + 78, (stsd.0 + 8 + entry_len).min(stsd.1));
        for cfg in [b"av1C", b"vpcC"] {
            if let Some((cs, ce)) = find(r, es, ee, cfg)
                && ce - cs < 1 << 16
            {
                r.seek(SeekFrom::Start(cs)).ok()?;
                let mut buf = vec![0u8; (ce - cs) as usize];
                r.read_exact(&mut buf).ok()?;
                v.config = buf;
                v.config_at = cs;
            }
        }
        found = Some(v);
        Some(())
    })?;
    found
}

/// Whether the matrix tagged for a picture `height` lines tall is BT.709 (else BT.601): what
/// our encoder converts with, and what players assume when nothing is tagged.
pub fn uses_709(height: u32) -> bool {
    height >= 720
}

/// `muxide` tags VP9 in MP4 with a transfer of 0 ("reserved") and an identity matrix, which
/// players read as RGB-in-YUV. Write the colour description we actually encode with.
pub fn fix_vp9_colour(path: &Path) -> std::io::Result<()> {
    let Some(v) = video_track(path).filter(|v| &v.fourcc == b"vp09" && v.config_at > 0) else { return Ok(()) };
    // vpcC (a full box): version/flags (4), profile, level, bit depth/chroma/range, then
    // primaries, transfer, matrix.
    let (p, t, m) = if uses_709(v.height) { (1, 1, 1) } else { (6, 6, 6) };
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(v.config_at + 7))?;
    std::io::Write::write_all(&mut f, &[p, t, m])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_matrices() {
        let one = 0x10000;
        assert_eq!(rotation(one, 0, 0, one), 0);
        assert_eq!(rotation(0, one, -one, 0), 90);
        assert_eq!(rotation(-one, 0, 0, -one), 180);
        assert_eq!(rotation(0, -one, one, 0), 270);
    }
}
