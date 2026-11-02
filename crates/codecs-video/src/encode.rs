//! Video encoders behind one trait: VP9 (`rusty_vp9`, the default) and AV1 (`rav1e`), both
//! taking 8-bit 4:2:0 frames.

use crate::{Picture, Planes};
use dotty_core::{Error, Preset, Result};
use image::RgbImage;
use std::collections::VecDeque;

/// An 8-bit 4:2:0 frame with even dimensions (what both encoders and most players expect).
#[derive(Clone)]
pub struct Yuv420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// A compressed frame, with its presentation time.
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: f64,
    pub key: bool,
}

impl Yuv420 {
    /// From RGB (cropping a last odd row/column), with the matrix players assume for this
    /// size when a file doesn't say: BT.709 from 720 lines up, else BT.601. Limited range.
    pub fn from_rgb(img: &RgbImage) -> Self {
        let (w, h) = (img.width() & !1, img.height() & !1);
        let (w, h) = (w.max(2), h.max(2));
        let (kr, kb) = if h >= 720 { (0.2126f32, 0.0722f32) } else { (0.299, 0.114) };
        let kg = 1.0 - kr - kb;
        let px = |x: u32, y: u32| {
            let p = img.get_pixel(x.min(img.width() - 1), y.min(img.height() - 1)).0;
            (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0)
        };
        let mut y_plane = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = px(x, y);
                y_plane.push((16.0 + 219.0 * (kr * r + kg * g + kb * b)).round() as u8);
            }
        }
        let (cw, ch) = (w / 2, h / 2);
        let (mut u, mut v) = (Vec::with_capacity((cw * ch) as usize), Vec::with_capacity((cw * ch) as usize));
        for cy in 0..ch {
            for cx in 0..cw {
                // Average the 2×2 block.
                let mut s = (0.0, 0.0, 0.0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (r, g, b) = px(cx * 2 + dx, cy * 2 + dy);
                    s = (s.0 + r / 4.0, s.1 + g / 4.0, s.2 + b / 4.0);
                }
                let yy = kr * s.0 + kg * s.1 + kb * s.2;
                let cb = (s.2 - yy) / (2.0 * (1.0 - kb));
                let cr = (s.0 - yy) / (2.0 * (1.0 - kr));
                u.push((128.0 + 224.0 * cb).round().clamp(0.0, 255.0) as u8);
                v.push((128.0 + 224.0 * cr).round().clamp(0.0, 255.0) as u8);
            }
        }
        Self { width: w, height: h, y: y_plane, u, v }
    }

    /// From a decoded picture: 8-bit 4:2:0 pictures pass straight through (no colour
    /// round trip); anything else goes through RGB. `rotation` is applied.
    pub fn from_picture(p: &Picture, rotation: u16) -> Self {
        if rotation == 0
            && let Planes::Yuv { bits: 8, cw, ch, data, .. } = &p.planes
            && *cw == p.width.div_ceil(2)
            && *ch == p.height.div_ceil(2)
        {
            let (w, h) = ((p.width & !1).max(2), (p.height & !1).max(2));
            let take = |plane: &[u16], stride: u32, pw: u32, ph: u32| -> Vec<u8> {
                let mut out = Vec::with_capacity((pw * ph) as usize);
                for y in 0..ph {
                    let row = (y.min(p.height.max(1) - 1) * stride) as usize;
                    out.extend(plane[row..row + pw as usize].iter().map(|&s| s as u8));
                }
                out
            };
            return Self { width: w, height: h, y: take(&data[0], p.width, w, h), u: take(&data[1], *cw, w / 2, h / 2), v: take(&data[2], *cw, w / 2, h / 2) };
        }
        Self::from_rgb(&crate::rotate(p.to_rgb8(), rotation))
    }
}

pub trait VideoEncoder {
    /// Feed one frame shown at `pts` seconds; returns any packets that came out.
    fn encode(&mut self, frame: &Yuv420, pts: f64) -> Result<Vec<Packet>>;
    /// After the last frame: the rest.
    fn finish(&mut self) -> Result<Vec<Packet>>;
    /// What a container stores to describe the stream (AV1's `av1C`), if anything.
    fn config(&mut self) -> Option<Vec<u8>> {
        None
    }
}

/// VP9's quality (0..63, lower is better) per preset.
fn vp9_crf(p: Preset) -> u32 {
    match p {
        Preset::Low => 44,
        Preset::Medium => 38,
        Preset::High => 32,
        Preset::Lossless => 20,
    }
}

/// AV1's quantizer (0..255, lower is better) per preset.
fn av1_quantizer(p: Preset) -> usize {
    match p {
        Preset::Low => 160,
        Preset::Medium => 130,
        Preset::High => 100,
        Preset::Lossless => 50,
    }
}

pub struct Vp9Encoder {
    enc: rusty_vp9::Vp9Encoder,
    /// Presentation times of frames sent but not yet out (one packet per shown frame, in
    /// order).
    pts: VecDeque<f64>,
}

impl Vp9Encoder {
    pub fn new(preset: Preset, fps: f64) -> Result<Self> {
        let mut enc = rusty_vp9::Vp9Encoder::default();
        // Speed 4 is ~2× faster than 2 for ~3% larger files (measured in the V0 audit).
        let cfg = rusty_vp9::Vp9EncoderConfig { crf: Some(vp9_crf(preset)), speed: Some(4), fps: Some(fps), ..Default::default() };
        enc.configure(&cfg).map_err(|e| Error::Codec(format!("vp9: {e:?}")))?;
        Ok(Self { enc, pts: VecDeque::new() })
    }

    fn take(&mut self) -> Vec<Packet> {
        let mut out = Vec::new();
        while let Ok(p) = self.enc.next_packet() {
            let pts = self.pts.pop_front().unwrap_or(0.0);
            out.push(Packet { data: p.data, pts, key: p.keyframe });
        }
        out
    }
}

impl VideoEncoder for Vp9Encoder {
    fn encode(&mut self, f: &Yuv420, pts: f64) -> Result<Vec<Packet>> {
        let cw = (f.width / 2) as usize;
        self.enc
            .push_frame([&f.y, &f.u, &f.v], [f.width as usize, cw, cw], f.width, f.height)
            .map_err(|e| Error::Codec(format!("vp9: {e:?}")))?;
        self.pts.push_back(pts);
        Ok(self.take())
    }
    fn finish(&mut self) -> Result<Vec<Packet>> {
        self.enc.flush();
        Ok(self.take())
    }
}

pub struct Av1Encoder {
    ctx: rav1e::Context<u8>,
    /// Presentation time per input frame number.
    pts: Vec<f64>,
}

impl Av1Encoder {
    pub fn new(preset: Preset, fps: f64, width: u32, height: u32, threads: usize) -> Result<Self> {
        use rav1e::prelude::{ColorDescription, ColorPrimaries, Config, EncoderConfig, MatrixCoefficients, Rational, SpeedSettings, TransferCharacteristics};
        let den = (fps * 1000.0).round().max(1.0) as u64;
        let enc = EncoderConfig {
            width: width as usize,
            height: height as usize,
            time_base: Rational::new(1000, den),
            quantizer: av1_quantizer(preset),
            // Say which matrix the pictures were converted with (see `Yuv420::from_rgb`).
            color_description: Some(if height >= 720 {
                ColorDescription { color_primaries: ColorPrimaries::BT709, transfer_characteristics: TransferCharacteristics::BT709, matrix_coefficients: MatrixCoefficients::BT709 }
            } else {
                ColorDescription { color_primaries: ColorPrimaries::BT601, transfer_characteristics: TransferCharacteristics::BT601, matrix_coefficients: MatrixCoefficients::BT601 }
            }),
            // Speed 8: close to speed 6's quality per byte (V0 audit), several times faster.
            speed_settings: SpeedSettings::from_preset(8),
            ..Default::default()
        };
        let ctx = Config::new().with_encoder_config(enc).with_threads(threads).new_context().map_err(|e| Error::Codec(format!("av1: {e}")))?;
        Ok(Self { ctx, pts: Vec::new() })
    }

    fn take(&mut self) -> Result<Vec<Packet>> {
        use rav1e::prelude::{EncoderStatus, FrameType};
        let mut out = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(p) => {
                    let pts = self.pts.get(p.input_frameno as usize).copied().unwrap_or(0.0);
                    out.push(Packet { data: p.data, pts, key: p.frame_type == FrameType::KEY });
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(out),
                Err(e) => return Err(Error::Codec(format!("av1: {e}"))),
            }
        }
    }
}

impl VideoEncoder for Av1Encoder {
    fn encode(&mut self, f: &Yuv420, pts: f64) -> Result<Vec<Packet>> {
        let mut frame = self.ctx.new_frame();
        let cw = (f.width / 2) as usize;
        frame.planes[0].copy_from_raw_u8(&f.y, f.width as usize, 1);
        frame.planes[1].copy_from_raw_u8(&f.u, cw, 1);
        frame.planes[2].copy_from_raw_u8(&f.v, cw, 1);
        self.pts.push(pts);
        self.ctx.send_frame(frame).map_err(|e| Error::Codec(format!("av1: {e}")))?;
        self.take()
    }
    fn finish(&mut self) -> Result<Vec<Packet>> {
        self.ctx.flush();
        self.take()
    }
    fn config(&mut self) -> Option<Vec<u8>> {
        Some(self.ctx.container_sequence_header())
    }
}
