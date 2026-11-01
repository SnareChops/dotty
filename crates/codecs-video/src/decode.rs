//! Video decoders behind one trait: AV1 (`rav1d`), VP9 (`filmcraft-vp9`) and Motion JPEG
//! (the `image` crate's JPEG decoder). Each turns packets into [`Picture`]s in display order.

use crate::Picture;
use dotty_core::{Error, Result};

/// Which matrix turns a picture's YUV into RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

pub trait Decoder: Send {
    /// Feed one packet (with its time in seconds); returns the pictures that came out.
    fn push(&mut self, data: &[u8], pts: f64) -> Result<Vec<Picture>>;
    /// After the last packet: anything still held back.
    fn flush(&mut self) -> Result<Vec<Picture>>;
    /// Forget everything (after a seek); the next packet must be a keyframe.
    fn reset(&mut self);
}

/// Matrix from the coded colour description, or a guess from the frame height.
pub fn matrix(coded: Option<Matrix>, height: u32) -> Matrix {
    coded.unwrap_or(if height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 })
}

// ---------------------------------------------------------------- AV1 (rav1d) ----

mod av1 {
    //! A safe wrapper over rav1d's dav1d-compatible C API. All `unsafe` in this crate is
    //! here: the API hands out raw picture buffers that must be copied and released.
    #![allow(unsafe_code)]

    use super::*;
    use crate::Planes;
    use rav1d::include::dav1d::data::Dav1dData;
    use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
    use rav1d::include::dav1d::picture::Dav1dPicture;
    use rav1d::src::lib as d;
    use std::ptr::NonNull;

    /// dav1d's "try again" (EAGAIN).
    const AGAIN: i32 = -11;

    pub struct Av1 {
        ctx: Option<Dav1dContext>,
        threads: i32,
    }

    // The context is only used from one thread at a time (`&mut self`).
    unsafe impl Send for Av1 {}

    impl Av1 {
        pub fn new(threads: usize) -> Result<Self> {
            let mut av1 = Self { ctx: None, threads: threads.clamp(1, 16) as i32 };
            av1.open()?;
            Ok(av1)
        }

        fn open(&mut self) -> Result<()> {
            // SAFETY: dav1d_default_settings fully initialises the settings it's given;
            // dav1d_open writes a context into `ctx` on success.
            unsafe {
                let mut s = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
                d::dav1d_default_settings(NonNull::new(s.as_mut_ptr()).expect("non-null"));
                let mut s = s.assume_init();
                s.n_threads = self.threads;
                s.max_frame_delay = 1;
                let r = d::dav1d_open(NonNull::new(&mut self.ctx), NonNull::new(&mut s));
                if r.0 != 0 {
                    return Err(Error::Codec(format!("av1: couldn't start the decoder ({})", r.0)));
                }
            }
            Ok(())
        }

        fn close(&mut self) {
            if self.ctx.is_some() {
                // SAFETY: the context came from dav1d_open and is closed once.
                unsafe { d::dav1d_close(NonNull::new(&mut self.ctx)) };
                self.ctx = None;
            }
        }

        fn drain(&mut self, out: &mut Vec<Picture>) -> Result<()> {
            loop {
                // SAFETY: a zeroed Dav1dPicture is the documented "empty" value; on success
                // dav1d fills it and we release it with dav1d_picture_unref after copying.
                let mut pic: Dav1dPicture = unsafe { std::mem::zeroed() };
                let r = unsafe { d::dav1d_get_picture(self.ctx, NonNull::new(&mut pic)) };
                if r.0 == AGAIN {
                    return Ok(());
                }
                if r.0 != 0 {
                    return Err(Error::Codec(format!("av1: decoding failed ({})", r.0)));
                }
                let p = unsafe { copy(&pic) };
                unsafe { d::dav1d_picture_unref(NonNull::new(&mut pic)) };
                out.push(p);
            }
        }
    }

    /// Copy a picture's visible planes out of dav1d's buffers.
    ///
    /// SAFETY: `pic` must be a picture returned by dav1d_get_picture and not yet released.
    unsafe fn copy(pic: &Dav1dPicture) -> Picture {
        let (w, h, bits) = (pic.p.w as usize, pic.p.h as usize, pic.p.bpc as u8);
        // 0 = 4:0:0, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4
        let layout = pic.p.layout;
        let (cw, ch) = match layout {
            1 => (w.div_ceil(2), h.div_ceil(2)),
            2 => (w.div_ceil(2), h),
            3 => (w, h),
            _ => (0, 0),
        };
        let plane = |p: usize, pw: usize, ph: usize| -> Vec<u16> {
            let Some(base) = pic.data[p] else { return vec![0; pw * ph] };
            let stride = pic.stride[(p > 0) as usize];
            let mut v = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                // SAFETY: dav1d guarantees `ph` rows of at least `pw` samples at `stride`.
                let row = unsafe { (base.as_ptr() as *const u8).offset(stride * y as isize) };
                if bits > 8 {
                    let row = unsafe { std::slice::from_raw_parts(row as *const u16, pw) };
                    v.extend_from_slice(row);
                } else {
                    let row = unsafe { std::slice::from_raw_parts(row, pw) };
                    v.extend(row.iter().map(|&b| b as u16));
                }
            }
            v
        };
        let y = plane(0, w, h);
        let (u, v) = if cw > 0 {
            (plane(1, cw, ch), plane(2, cw, ch))
        } else {
            // Monochrome: neutral chroma at full size so conversion needs no special case.
            let mid = 1u16 << (bits - 1);
            (vec![mid; w * h], vec![mid; w * h])
        };
        let (cw, ch) = if cw > 0 { (cw, ch) } else { (w, h) };
        // SAFETY: the sequence header pointer is valid while the picture is.
        let seq = pic.seq_hdr.map(|s| unsafe { s.as_ref() });
        let coded = seq.and_then(|s| match s.mtrx {
            1 => Some(Matrix::Bt709),
            5 | 6 => Some(Matrix::Bt601),
            9 | 10 => Some(Matrix::Bt2020),
            _ => None,
        });
        let full_range = seq.is_some_and(|s| s.color_range != 0);
        let pts = pic.m.timestamp as f64 / 1e6;
        Picture {
            width: w as u32,
            height: h as u32,
            pts,
            planes: Planes::Yuv { bits, cw: cw as u32, ch: ch as u32, data: [y, u, v], matrix: matrix(coded, h as u32), full_range },
        }
    }

    impl Decoder for Av1 {
        fn push(&mut self, data: &[u8], pts: f64) -> Result<Vec<Picture>> {
            let mut out = Vec::new();
            // SAFETY: dav1d_data_create allocates `data.len()` bytes we fill before sending;
            // dav1d takes ownership and empties `dd` (sz = 0) once it's consumed.
            unsafe {
                let mut dd: Dav1dData = std::mem::zeroed();
                let buf = d::dav1d_data_create(NonNull::new(&mut dd), data.len());
                if buf.is_null() {
                    return Err(Error::Codec("av1: out of memory".into()));
                }
                std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
                dd.m.timestamp = (pts * 1e6).round() as i64;
                loop {
                    let r = d::dav1d_send_data(self.ctx, NonNull::new(&mut dd));
                    if r.0 != 0 && r.0 != AGAIN {
                        d::dav1d_data_unref(NonNull::new(&mut dd));
                        return Err(Error::Codec(format!("av1: the data is damaged ({})", r.0)));
                    }
                    self.drain(&mut out)?;
                    if dd.sz == 0 {
                        break;
                    }
                }
            }
            Ok(out)
        }
        fn flush(&mut self) -> Result<Vec<Picture>> {
            let mut out = Vec::new();
            self.drain(&mut out)?;
            Ok(out)
        }
        fn reset(&mut self) {
            // SAFETY: flushing a live context is always allowed.
            if let Some(ctx) = self.ctx {
                unsafe { d::dav1d_flush(ctx) };
            }
        }
    }

    impl Drop for Av1 {
        fn drop(&mut self) {
            self.close();
        }
    }
}

pub use av1::Av1;

// ---------------------------------------------------------------- VP9 ----

pub struct Vp9 {
    dec: filmcraft_vp9::Decoder,
    threads: usize,
}

impl Vp9 {
    pub fn new(threads: usize) -> Self {
        Self { dec: filmcraft_vp9::Decoder::with_threads(threads.max(1)), threads: threads.max(1) }
    }
}

fn vp9_picture(p: filmcraft_vp9::Picture) -> Picture {
    let (w, h, cw, ch) = (p.width as usize, p.height as usize, p.chroma_width as usize, p.chroma_height as usize);
    let plane = |pl: &filmcraft_vp9::Plane, stride: usize, pw: usize, ph: usize| -> Vec<u16> {
        let mut v = Vec::with_capacity(pw * ph);
        for y in 0..ph {
            if let Some(row) = pl.as_u8() {
                v.extend(row[y * stride..y * stride + pw].iter().map(|&b| b as u16));
            } else if let Some(row) = pl.as_u16() {
                v.extend_from_slice(&row[y * stride..y * stride + pw]);
            }
        }
        v
    };
    // VP9 colour space: 1 = BT.601, 2 = BT.709, 5 = BT.2020 (0 = unknown).
    let coded = match p.color.color_space {
        1 | 3 => Some(Matrix::Bt601),
        2 => Some(Matrix::Bt709),
        5 => Some(Matrix::Bt2020),
        _ => None,
    };
    Picture {
        width: w as u32,
        height: h as u32,
        pts: p.pts as f64 / 1e6,
        planes: crate::Planes::Yuv {
            bits: p.bit_depth as u8,
            cw: cw as u32,
            ch: ch as u32,
            data: [plane(&p.y, p.y_stride, w, h), plane(&p.u, p.uv_stride, cw, ch), plane(&p.v, p.uv_stride, cw, ch)],
            matrix: matrix(coded, h as u32),
            full_range: p.color.full_range,
        },
    }
}

impl Decoder for Vp9 {
    fn push(&mut self, data: &[u8], pts: f64) -> Result<Vec<Picture>> {
        let pics = self.dec.decode(data, (pts * 1e6).round() as i64).map_err(|e| Error::Codec(format!("vp9: {e}")))?;
        Ok(pics.into_iter().map(vp9_picture).collect())
    }
    fn flush(&mut self) -> Result<Vec<Picture>> {
        Ok(self.dec.flush().into_iter().map(vp9_picture).collect())
    }
    fn reset(&mut self) {
        self.dec = filmcraft_vp9::Decoder::with_threads(self.threads);
    }
}

// ---------------------------------------------------------------- Motion JPEG ----

pub struct Mjpeg;

/// The standard Huffman tables (JPEG Annex K.3) as one DHT segment. Motion JPEG from many
/// cameras leaves them out and relies on the decoder knowing them.
const STD_DHT: &[u8] = include_bytes!("jpeg_std_dht.bin");

/// Insert the standard tables before the first SOF/SOS marker if the frame has none.
fn with_tables(jpeg: &[u8]) -> Option<Vec<u8>> {
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let m = jpeg[i + 1];
        if m == 0xC4 {
            return None; // has its own tables
        }
        if matches!(m, 0xC0..=0xC3 | 0xDA) {
            let mut v = Vec::with_capacity(jpeg.len() + STD_DHT.len());
            v.extend_from_slice(&jpeg[..i]);
            v.extend_from_slice(STD_DHT);
            v.extend_from_slice(&jpeg[i..]);
            return Some(v);
        }
        i += 2 + u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
    }
    None
}

impl Decoder for Mjpeg {
    fn push(&mut self, data: &[u8], pts: f64) -> Result<Vec<Picture>> {
        let decode = |b: &[u8]| image::load_from_memory_with_format(b, image::ImageFormat::Jpeg);
        let img = match decode(data) {
            Ok(i) => i,
            Err(e) => match with_tables(data) {
                Some(fixed) => decode(&fixed).map_err(|e| Error::Codec(format!("motion jpeg: {e}")))?,
                None => return Err(Error::Codec(format!("motion jpeg: {e}"))),
            },
        };
        let rgb = img.to_rgb8();
        Ok(vec![Picture { width: rgb.width(), height: rgb.height(), pts, planes: crate::Planes::Rgb(rgb) }])
    }
    fn flush(&mut self) -> Result<Vec<Picture>> {
        Ok(Vec::new())
    }
    fn reset(&mut self) {}
}

// ---------------------------------------------------------------- keyframes ----

/// Whether a packet starts a picture that can be decoded on its own.
pub fn is_keyframe(codec: crate::Codec, data: &[u8]) -> bool {
    match codec {
        crate::Codec::Mjpeg => true,
        crate::Codec::Vp9 => filmcraft_vp9::is_keyframe(data),
        crate::Codec::Av1 => av1_keyframe(data),
    }
}

/// AV1: the temporal unit's first frame header says KEY_FRAME (and isn't just re-showing
/// an earlier frame). Reads only OBU headers and the first bits of the frame header.
fn av1_keyframe(mut data: &[u8]) -> bool {
    while let Some(&header) = data.first() {
        let kind = (header >> 3) & 0xF;
        let has_ext = header & 4 != 0;
        let has_size = header & 2 != 0;
        let mut at = 1 + has_ext as usize;
        let size = if has_size {
            // LEB128
            let mut v = 0usize;
            for i in 0..8 {
                let Some(&b) = data.get(at) else { return false };
                at += 1;
                v |= ((b & 0x7F) as usize) << (7 * i);
                if b & 0x80 == 0 {
                    break;
                }
            }
            v
        } else {
            data.len().saturating_sub(at)
        };
        let Some(payload) = data.get(at..at + size) else { return false };
        // OBU_FRAME_HEADER = 3, OBU_FRAME = 6
        if kind == 3 || kind == 6 {
            let Some(&b) = payload.first() else { return false };
            let show_existing = b & 0x80 != 0;
            let frame_type = (b >> 5) & 3;
            return !show_existing && frame_type == 0;
        }
        data = &data[at + size..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_tables_go_in_front_of_the_frame_header() {
        // SOI, APP0 (len 4), SOF0 (len 4)…
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xC0, 0, 4, 9, 9];
        let fixed = with_tables(&jpeg).unwrap();
        assert_eq!(&fixed[..8], &jpeg[..8]);
        assert_eq!(&fixed[8..8 + STD_DHT.len()], STD_DHT);
        assert_eq!(&fixed[8 + STD_DHT.len()..], &jpeg[8..]);
        assert_eq!(STD_DHT.len(), 420);
        assert_eq!(&STD_DHT[..4], &[0xFF, 0xC4, 0x01, 0xA2]);
        // Already has tables: nothing to do.
        assert!(with_tables(&[0xFF, 0xD8, 0xFF, 0xC4, 0, 2, 0xFF, 0xC0, 0, 2]).is_none());
    }

    #[test]
    fn av1_keyframes_from_the_frame_header() {
        // Temporal delimiter (size 0), then an OBU_FRAME whose header starts 0b0_00_…: KEY.
        let key = [0x12, 0x00, 0x32, 0x02, 0b0001_0000, 0];
        assert!(av1_keyframe(&key));
        // frame_type 1 (inter).
        let inter = [0x12, 0x00, 0x32, 0x02, 0b0011_0000, 0];
        assert!(!av1_keyframe(&inter));
        // show_existing_frame.
        let existing = [0x12, 0x00, 0x1A, 0x01, 0b1000_0000];
        assert!(!av1_keyframe(&existing));
    }
}
