//! Minimal ID3v2.3 writer. v2.3 + UTF-16 is the most widely understood combination.

use crate::tags::Tags;

/// Serialise `tags` as an ID3v2.3 block, or an empty vec if there is nothing to write.
pub fn id3v2(tags: &Tags) -> Vec<u8> {
    let mut frames = Vec::new();
    let mut text = |id: &[u8; 4], s: &Option<String>| {
        if let Some(s) = s {
            frame(&mut frames, id, &text_payload(s));
        }
    };
    text(b"TIT2", &tags.title);
    text(b"TPE1", &tags.artist);
    text(b"TALB", &tags.album);
    text(b"TPE2", &tags.album_artist);
    text(b"TCON", &tags.genre);
    text(b"TCOM", &tags.composer);
    // v2.3 has no full-date frame; keep the year.
    let year = tags.date.as_deref().and_then(|d| d.get(..4)).filter(|y| y.bytes().all(|b| b.is_ascii_digit()));
    text(b"TYER", &year.map(str::to_string));
    text(b"TRCK", &tags.track.map(|n| n.to_string()));
    text(b"TPOS", &tags.disc.map(|n| n.to_string()));
    if let Some(c) = &tags.comment {
        // COMM: encoding, language, short description (empty), text.
        let body = text_payload(c);
        let mut p = vec![body[0]];
        p.extend_from_slice(b"eng");
        p.extend_from_slice(if body[0] == 1 { &[0xFF, 0xFE, 0, 0] } else { &[0] });
        p.extend_from_slice(&body[1..]);
        frame(&mut frames, b"COMM", &p);
    }
    if frames.is_empty() {
        return frames;
    }

    let size = frames.len() as u32;
    let mut out = Vec::with_capacity(10 + frames.len());
    out.extend_from_slice(b"ID3\x03\x00\x00");
    out.extend_from_slice(&[(size >> 21) as u8 & 0x7F, (size >> 14) as u8 & 0x7F, (size >> 7) as u8 & 0x7F, size as u8 & 0x7F]);
    out.extend_from_slice(&frames);
    out
}

fn frame(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(payload);
}

/// Encoding byte followed by the text: Latin-1 when possible, else UTF-16LE with BOM.
fn text_payload(s: &str) -> Vec<u8> {
    if s.chars().all(|c| (c as u32) < 0x100) {
        let mut v = vec![0];
        v.extend(s.chars().map(|c| c as u8));
        v
    } else {
        let mut v = vec![1, 0xFF, 0xFE];
        v.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        v
    }
}
