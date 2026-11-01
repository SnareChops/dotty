//! Wrap a simple lossy WebP in the extended (VP8X) container so it can carry ICC / EXIF.

fn chunk(out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0); // chunks are padded to even length
    }
}

/// Split a RIFF/WEBP file into its chunks. `None` if it isn't a well-formed one.
fn chunks(webp: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    if webp.len() < 12 || &webp[..4] != b"RIFF" || &webp[8..12] != b"WEBP" {
        return None;
    }
    let mut v = Vec::new();
    let mut p = 12;
    while p + 8 <= webp.len() {
        let id: [u8; 4] = webp[p..p + 4].try_into().ok()?;
        let len = u32::from_le_bytes(webp[p + 4..p + 8].try_into().ok()?) as usize;
        let payload = webp.get(p + 8..p + 8 + len)?;
        v.push((id, payload));
        p += 8 + len + (len & 1);
    }
    Some(v)
}

/// Returns `webp` with the given metadata embedded, or `webp` unchanged if there is
/// nothing to add or the input isn't a file we understand.
pub fn embed(webp: Vec<u8>, width: u32, height: u32, icc: Option<&[u8]>, exif: Option<&[u8]>) -> Vec<u8> {
    let icc = icc.filter(|d| !d.is_empty());
    let exif = exif.filter(|d| !d.is_empty());
    if icc.is_none() && exif.is_none() {
        return webp;
    }
    let Some(parts) = chunks(&webp) else { return webp };

    let mut body = Vec::with_capacity(webp.len() + 64);
    body.extend_from_slice(b"WEBP");
    let existing_vp8x = parts.iter().find(|(id, _)| id == b"VP8X").map(|(_, p)| *p);
    let mut flags = existing_vp8x.and_then(|p| p.first().copied()).unwrap_or(0);
    if icc.is_some() {
        flags |= 0x20;
    }
    if exif.is_some() {
        flags |= 0x08;
    }
    let (cw, ch) = (width - 1, height - 1);
    let mut vp8x = vec![flags, 0, 0, 0];
    vp8x.extend_from_slice(&cw.to_le_bytes()[..3]);
    vp8x.extend_from_slice(&ch.to_le_bytes()[..3]);
    chunk(&mut body, b"VP8X", &vp8x);
    if let Some(icc) = icc {
        chunk(&mut body, b"ICCP", icc);
    }
    for (id, payload) in parts.iter().filter(|(id, _)| id != b"VP8X" && id != b"ICCP" && id != b"EXIF") {
        chunk(&mut body, id, payload);
    }
    if let Some(exif) = exif {
        chunk(&mut body, b"EXIF", exif);
    }

    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// One frame of an animation: a complete single-image WebP file and how long it shows.
pub struct AnimFrame {
    pub webp: Vec<u8>,
    pub duration_ms: u32,
}

/// Build an animated WebP (VP8X + ANIM + one ANMF per frame) from single-image WebP files
/// that each cover the whole `width`×`height` canvas. `loops` = 0 plays forever.
pub fn animate(
    frames: &[AnimFrame],
    width: u32,
    height: u32,
    loops: u16,
    alpha: bool,
    icc: Option<&[u8]>,
    exif: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let icc = icc.filter(|d| !d.is_empty());
    let exif = exif.filter(|d| !d.is_empty());
    let mut body = Vec::new();
    body.extend_from_slice(b"WEBP");
    let mut flags = 0x02; // animation
    if alpha {
        flags |= 0x10;
    }
    if icc.is_some() {
        flags |= 0x20;
    }
    if exif.is_some() {
        flags |= 0x08;
    }
    let mut vp8x = vec![flags, 0, 0, 0];
    vp8x.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    vp8x.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    chunk(&mut body, b"VP8X", &vp8x);
    if let Some(icc) = icc {
        chunk(&mut body, b"ICCP", icc);
    }
    let mut anim = vec![0, 0, 0, 0]; // background colour: transparent
    anim.extend_from_slice(&loops.to_le_bytes());
    chunk(&mut body, b"ANIM", &anim);

    for f in frames {
        let mut anmf = vec![0, 0, 0, 0, 0, 0]; // frame at (0, 0)
        anmf.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        anmf.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        anmf.extend_from_slice(&f.duration_ms.min(0xFF_FFFF).to_le_bytes()[..3]);
        anmf.push(0x02); // don't blend: every frame is a full canvas
        for (id, payload) in chunks(&f.webp)?.into_iter().filter(|(id, _)| matches!(id, b"ALPH" | b"VP8 " | b"VP8L")) {
            chunk(&mut anmf, &id, payload);
        }
        chunk(&mut body, b"ANMF", &anmf);
    }
    if let Some(exif) = exif {
        chunk(&mut body, b"EXIF", exif);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Some(out)
}
