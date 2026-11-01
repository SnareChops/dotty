//! Reads the flattened ("maximize compatibility") image Photoshop stores after the
//! layers in PSD and PSB files. Layers themselves aren't composited.

use image::{DynamicImage, ImageBuffer, Luma, LumaA, Rgb, Rgba};
use transformer_core::{Error, Result};

fn err(msg: &str) -> Error {
    Error::Codec(format!("psd: {msg}"))
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos.checked_add(n).ok_or_else(|| err("truncated"))?).ok_or_else(|| err("file is truncated"))?;
        self.pos += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn skip_block32(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// PackBits-decode one row into `out` (exactly `out.len()` bytes).
fn unpack_bits(mut src: &[u8], out: &mut [u8]) -> Result<()> {
    let mut o = 0;
    while o < out.len() {
        let (&n, rest) = src.split_first().ok_or_else(|| err("compressed row is truncated"))?;
        src = rest;
        let n = n as i8;
        if n >= 0 {
            let len = n as usize + 1;
            let lit = src.get(..len).ok_or_else(|| err("compressed row is truncated"))?;
            let end = (o + len).min(out.len());
            out[o..end].copy_from_slice(&lit[..end - o]);
            src = &src[len..];
            o = end;
        } else if n != -128 {
            let len = (1 - n as isize) as usize;
            let (&v, rest) = src.split_first().ok_or_else(|| err("compressed row is truncated"))?;
            src = rest;
            let end = (o + len).min(out.len());
            out[o..end].fill(v);
            o = end;
        }
    }
    Ok(())
}

pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let mut r = Reader { buf: bytes, pos: 0 };
    if r.take(4)? != b"8BPS" {
        return Err(err("not a Photoshop file"));
    }
    let big = match r.u16()? {
        1 => false,
        2 => true,
        _ => return Err(err("unknown version")),
    };
    r.take(6)?;
    let channels = r.u16()? as usize;
    let height = r.u32()? as usize;
    let width = r.u32()? as usize;
    let depth = r.u16()?;
    let mode = r.u16()?;
    if width == 0 || height == 0 || (width as u64) * (height as u64) * channels as u64 > 1 << 31 {
        return Err(err("image size is out of range"));
    }
    let palette = r.skip_block32()?;
    r.skip_block32()?; // image resources
    let layers = if big { r.u64()? as usize } else { r.u32()? as usize };
    r.take(layers)?;

    let bytes_per = match depth {
        1 | 8 => 1,
        16 => 2,
        _ => return Err(err(&format!("{depth}-bit images aren't supported yet"))),
    };
    let row_bytes = if depth == 1 { width.div_ceil(8) } else { width * bytes_per };
    let plane_len = row_bytes * height;
    let mut planes = vec![vec![0u8; plane_len]; channels];
    match r.u16()? {
        0 => {
            for p in &mut planes {
                p.copy_from_slice(r.take(plane_len)?);
            }
        }
        1 => {
            let mut counts = Vec::with_capacity(channels * height);
            for _ in 0..channels * height {
                counts.push(if big { r.u32()? as usize } else { r.u16()? as usize });
            }
            let mut counts = counts.into_iter();
            for p in &mut planes {
                for row in p.chunks_exact_mut(row_bytes) {
                    unpack_bits(r.take(counts.next().unwrap())?, row)?;
                }
            }
        }
        _ => return Err(err("ZIP-compressed images aren't supported yet")),
    }

    // Sample at (plane, pixel) scaled to 16 bits.
    let sample = |c: usize, i: usize| -> u16 {
        if bytes_per == 2 {
            u16::from_be_bytes([planes[c][i * 2], planes[c][i * 2 + 1]])
        } else {
            planes[c][i] as u16 * 257
        }
    };
    let (w, h) = (width as u32, height as u32);
    let idx = |x: u32, y: u32| y as usize * width + x as usize;
    let color_channels = match mode {
        0 | 1 | 2 | 8 => 1,
        3 => 3,
        4 => 4,
        _ => return Err(err("this colour mode (Lab, multichannel) isn't supported yet")),
    };
    if channels < color_channels {
        return Err(err("missing colour channels"));
    }
    let alpha = channels > color_channels;
    // Photoshop blends the flattened image over white where it's transparent; undo that.
    let unmatte = |v: u16, a: u16| -> u16 {
        if a == 0 || a == 65535 {
            return v;
        }
        let a = a as f32 / 65535.0;
        ((v as f32 - 65535.0 * (1.0 - a)) / a).round().clamp(0.0, 65535.0) as u16
    };
    let deep = bytes_per == 2;

    let img = match mode {
        // Bitmap: 1 bit per pixel, 1 = black.
        0 => DynamicImage::ImageLuma8(ImageBuffer::from_fn(w, h, |x, y| {
            let bit = planes[0][y as usize * row_bytes + x as usize / 8] >> (7 - x % 8) & 1;
            Luma([if bit == 1 { 0 } else { 255 }])
        })),
        // Indexed: 768-byte palette, all reds then greens then blues.
        2 => {
            if palette.len() < 768 {
                return Err(err("indexed image without a palette"));
            }
            DynamicImage::ImageRgb8(ImageBuffer::from_fn(w, h, |x, y| {
                let i = planes[0][idx(x, y)] as usize;
                Rgb([palette[i], palette[256 + i], palette[512 + i]])
            }))
        }
        1 | 8 => {
            let px = |x, y| {
                let i = idx(x, y);
                let a = if alpha { sample(1, i) } else { 65535 };
                (unmatte(sample(0, i), a), a)
            };
            match (alpha, deep) {
                (false, false) => DynamicImage::ImageLuma8(ImageBuffer::from_fn(w, h, |x, y| Luma([(px(x, y).0 >> 8) as u8]))),
                (false, true) => DynamicImage::ImageLuma16(ImageBuffer::from_fn(w, h, |x, y| Luma([px(x, y).0]))),
                (true, false) => DynamicImage::ImageLumaA8(ImageBuffer::from_fn(w, h, |x, y| {
                    let (v, a) = px(x, y);
                    LumaA([(v >> 8) as u8, (a >> 8) as u8])
                })),
                (true, true) => DynamicImage::ImageLumaA16(ImageBuffer::from_fn(w, h, |x, y| {
                    let (v, a) = px(x, y);
                    LumaA([v, a])
                })),
            }
        }
        _ => {
            // RGB, or CMYK stored inverted (65535 = no ink): R = C' · K' with no colour profile.
            let px = |x, y| -> ([u16; 3], u16) {
                let i = idx(x, y);
                let a = if alpha { sample(color_channels, i) } else { 65535 };
                let rgb = if mode == 3 {
                    [sample(0, i), sample(1, i), sample(2, i)]
                } else {
                    let k = sample(3, i) as u32;
                    let ch = |c: usize| (sample(c, i) as u32 * k / 65535) as u16;
                    [ch(0), ch(1), ch(2)]
                };
                (rgb.map(|v| unmatte(v, a)), a)
            };
            let to8 = |v: u16| (v >> 8) as u8;
            match (alpha, deep) {
                (false, false) => DynamicImage::ImageRgb8(ImageBuffer::from_fn(w, h, |x, y| Rgb(px(x, y).0.map(to8)))),
                (false, true) => DynamicImage::ImageRgb16(ImageBuffer::from_fn(w, h, |x, y| Rgb(px(x, y).0))),
                (true, false) => DynamicImage::ImageRgba8(ImageBuffer::from_fn(w, h, |x, y| {
                    let ([r, g, b], a) = px(x, y);
                    Rgba([to8(r), to8(g), to8(b), to8(a)])
                })),
                (true, true) => DynamicImage::ImageRgba16(ImageBuffer::from_fn(w, h, |x, y| {
                    let ([r, g, b], a) = px(x, y);
                    Rgba([r, g, b, a])
                })),
            }
        }
    };
    Ok(img)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn packbits_literals_runs_and_noops() {
        let mut out = [0u8; 7];
        unpack_bits(&[2, 1, 2, 3, 0xFE, 9, 0x80, 0, 4], &mut out).unwrap();
        assert_eq!(out, [1, 2, 3, 9, 9, 9, 4]);
        assert!(unpack_bits(&[5, 1], &mut out).is_err());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(b"8BPS\0\x01").is_err());
        assert!(decode(b"nope").is_err());
    }
}
