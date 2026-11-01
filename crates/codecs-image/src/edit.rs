//! Editor support: applying [`Edits`] in the pipeline (erase, draw, text, then crop), and the
//! selection tools the editor runs on an 8-bit preview of the image.
//!
//! Selections and masks are one byte per pixel (0 = none, 255 = full), so soft edges
//! are just values in between.

use crate::ImageData;
use image::DynamicImage::{self, *};
use image::imageops::FilterType;
use image::{GrayImage, RgbaImage};
use std::borrow::Cow;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use resvg::tiny_skia;
use dotty_core::{Captions, ConvertCtx, Drawing, EditOp, Edits, Error, Format, Mask, Options, Region, Result};

pub use image::RgbaImage as Rgba8Image;

/// Apply every edit to `img`.
pub fn apply(img: &mut ImageData, edits: &Edits) {
    for op in &edits.ops {
        match op {
            EditOp::Erase { frame, mask } => {
                if let Some(f) = img.frames.get_mut(*frame) {
                    erase(&mut f.image, mask);
                }
            }
            EditOp::Draw(drawing) => {
                // Frames of one size (animations) share one rasterized layer.
                let mut ink: Option<((u32, u32), Vec<u8>)> = None;
                for f in &mut img.frames {
                    let size = (f.image.width(), f.image.height());
                    if ink.as_ref().is_none_or(|(s, _)| *s != size) {
                        ink = Some((size, rasterize(drawing, size.0, size.1)));
                    }
                    if let Some((_, layer)) = &ink {
                        draw(&mut f.image, layer);
                    }
                }
            }
            EditOp::Text(captions) => {
                let mut ink: Option<((u32, u32), Vec<u8>)> = None;
                for f in &mut img.frames {
                    let size = (f.image.width(), f.image.height());
                    if ink.as_ref().is_none_or(|(s, _)| *s != size) {
                        ink = Some((size, crate::caption::layer(captions, size.0, size.1)));
                    }
                    if let Some((_, layer)) = &ink {
                        draw(&mut f.image, layer);
                    }
                }
            }
            EditOp::Crop(region) => {
                for f in &mut img.frames {
                    let (x, y, w, h) = region.scaled(f.image.width(), f.image.height());
                    f.image = f.image.crop_imm(x, y, w, h);
                }
            }
            EditOp::Audio(_) | EditOp::Frames(_) => {}
        }
    }
}

/// Multiply the image's alpha by `mask`, adding an alpha channel if it has none.
fn erase(image: &mut DynamicImage, mask: &Mask) {
    let m = fit(mask, image.width(), image.height());
    let with_alpha = match std::mem::take(image) {
        img @ (ImageLumaA8(_) | ImageRgba8(_) | ImageLumaA16(_) | ImageRgba16(_) | ImageRgba32F(_)) => img,
        ImageLuma8(b) => ImageLumaA8(DynamicImage::ImageLuma8(b).to_luma_alpha8()),
        ImageLuma16(b) => ImageLumaA16(DynamicImage::ImageLuma16(b).to_luma_alpha16()),
        ImageRgb16(b) => ImageRgba16(DynamicImage::ImageRgb16(b).to_rgba16()),
        ImageRgb32F(b) => ImageRgba32F(DynamicImage::ImageRgb32F(b).to_rgba32f()),
        other => ImageRgba8(other.to_rgba8()),
    };
    *image = with_alpha;
    match image {
        ImageLumaA8(b) => scale_alpha(b, 2, &m, |a, k| ((a as u32 * k as u32 + 127) / 255) as u8),
        ImageRgba8(b) => scale_alpha(b, 4, &m, |a, k| ((a as u32 * k as u32 + 127) / 255) as u8),
        ImageLumaA16(b) => scale_alpha(b, 2, &m, |a, k| ((a as u32 * k as u32 + 127) / 255) as u16),
        ImageRgba16(b) => scale_alpha(b, 4, &m, |a, k| ((a as u32 * k as u32 + 127) / 255) as u16),
        ImageRgba32F(b) => scale_alpha(b, 4, &m, |a, k| a * k as f32 / 255.0),
        _ => unreachable!("converted to a type with alpha above"),
    }
}

/// The strokes as premultiplied RGBA at `w`×`h`, anti-aliased with round ends and joins.
fn rasterize(drawing: &Drawing, w: u32, h: u32) -> Vec<u8> {
    let Some(mut pixmap) = tiny_skia::Pixmap::new(w, h) else { return Vec::new() };
    let (sx, sy) = (w as f32 / drawing.of_width.max(1) as f32, h as f32 / drawing.of_height.max(1) as f32);
    let transform = tiny_skia::Transform::from_scale(sx, sy);
    for stroke in &drawing.strokes {
        let mut paint = tiny_skia::Paint::default();
        let [r, g, b, a] = stroke.color;
        paint.set_color_rgba8(r, g, b, a);
        paint.anti_alias = true;
        let Some(&(x0, y0)) = stroke.points.first() else { continue };
        if stroke.points.len() == 1 {
            if let Some(dot) = tiny_skia::PathBuilder::from_circle(x0, y0, stroke.width / 2.0) {
                pixmap.fill_path(&dot, &paint, tiny_skia::FillRule::Winding, transform, None);
            }
            continue;
        }
        let mut path = tiny_skia::PathBuilder::new();
        path.move_to(x0, y0);
        for &(x, y) in &stroke.points[1..] {
            path.line_to(x, y);
        }
        let Some(path) = path.finish() else { continue };
        let pen = tiny_skia::Stroke {
            width: stroke.width,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint, &pen, transform, None);
    }
    pixmap.take()
}

/// Paint a premultiplied RGBA layer over the image. Opaque images stay opaque (so formats
/// without transparency don't report any); grey images become colour.
fn draw(image: &mut DynamicImage, layer: &[u8]) {
    if layer.len() != image.width() as usize * image.height() as usize * 4 {
        return;
    }
    let colour = match std::mem::take(image) {
        img @ (ImageRgb8(_) | ImageRgba8(_) | ImageRgb16(_) | ImageRgba16(_) | ImageRgb32F(_) | ImageRgba32F(_)) => img,
        ImageLuma8(b) => ImageRgb8(DynamicImage::ImageLuma8(b).to_rgb8()),
        ImageLuma16(b) => ImageRgb16(DynamicImage::ImageLuma16(b).to_rgb16()),
        ImageLumaA16(b) => ImageRgba16(DynamicImage::ImageLumaA16(b).to_rgba16()),
        other => ImageRgba8(other.to_rgba8()),
    };
    *image = colour;
    let (u8_in, u8_out) = (|v: u8| v as f32 / 255.0, |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    let (u16_in, u16_out) = (|v: u16| v as f32 / 65535.0, |v: f32| (v.clamp(0.0, 1.0) * 65535.0).round() as u16);
    match image {
        ImageRgb8(b) => blend(b, 3, layer, false, u8_in, u8_out),
        ImageRgba8(b) => blend(b, 4, layer, false, u8_in, u8_out),
        ImageRgb16(b) => blend(b, 3, layer, false, u16_in, u16_out),
        ImageRgba16(b) => blend(b, 4, layer, false, u16_in, u16_out),
        // Float pixels are linear light, so the sRGB ink is converted first.
        ImageRgb32F(b) => blend(b, 3, layer, true, |v| v, |v| v),
        ImageRgba32F(b) => blend(b, 4, layer, true, |v| v, |v| v),
        _ => unreachable!("converted to a colour type above"),
    }
}

/// Source-over compositing of premultiplied `layer` onto straight-alpha pixels.
fn blend<T: Copy>(pixels: &mut [T], channels: usize, layer: &[u8], linear: bool, get: impl Fn(T) -> f32, put: impl Fn(f32) -> T) {
    for (px, ink) in pixels.chunks_exact_mut(channels).zip(layer.as_chunks::<4>().0) {
        if ink[3] == 0 {
            continue;
        }
        let sa = ink[3] as f32 / 255.0;
        let da = if channels == 4 { get(px[3]) } else { 1.0 };
        let out_a = sa + da * (1.0 - sa);
        for c in 0..3 {
            let mut sc = ink[c] as f32 / 255.0 / sa;
            if linear {
                sc = crate::color::srgb_to_linear(sc);
            }
            px[c] = put((sc * sa + get(px[c]) * da * (1.0 - sa)) / out_a);
        }
        if channels == 4 {
            px[3] = put(out_a);
        }
    }
}

/// Draw the strokes onto an 8-bit image (the editor's preview).
pub fn draw_rgba(img: &mut RgbaImage, drawing: &Drawing) {
    if drawing.strokes.is_empty() {
        return;
    }
    let layer = rasterize(drawing, img.width(), img.height());
    blend_rgba(img, &layer);
}

/// Write the captions onto an 8-bit image (the editor's thumbnail).
pub fn captions_rgba(img: &mut RgbaImage, captions: &Captions) {
    let layer = crate::caption::layer(captions, img.width(), img.height());
    blend_rgba(img, &layer);
}

fn blend_rgba(img: &mut RgbaImage, layer: &[u8]) {
    blend(img, 4, layer, false, |v: u8| v as f32 / 255.0, |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
}

fn scale_alpha<T: Copy>(pixels: &mut [T], channels: usize, mask: &[u8], f: impl Fn(T, u8) -> T) {
    for (px, &k) in pixels.chunks_exact_mut(channels).zip(mask) {
        px[channels - 1] = f(px[channels - 1], k);
    }
}

/// The mask at `w`×`h`, stretched smoothly if it was made at another size.
fn fit(mask: &Mask, w: u32, h: u32) -> Cow<'_, [u8]> {
    if (mask.width, mask.height) == (w, h) {
        return Cow::Borrowed(&mask.data);
    }
    let gray = GrayImage::from_raw(mask.width, mask.height, mask.data.to_vec()).expect("mask size checked on creation");
    Cow::Owned(image::imageops::resize(&gray, w, h, FilterType::Triangle).into_raw())
}

/// How [`magic_wand`] decides which pixels belong together.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WandSettings {
    /// 0–100: how far a colour may be from the clicked one and still be selected.
    pub tolerance: f32,
    /// Only pixels connected to the clicked one, or every similar pixel in the image.
    pub contiguous: bool,
    /// Fade the selection over colours just beyond the tolerance instead of a hard cut.
    pub smooth: bool,
    /// Blur radius in pixels applied to the result.
    pub feather: u32,
}

impl Default for WandSettings {
    fn default() -> Self {
        Self { tolerance: 20.0, contiguous: true, smooth: true, feather: 0 }
    }
}

/// Largest difference between two pixels over the colour and alpha channels.
fn distance(a: [u8; 4], b: [u8; 4]) -> u8 {
    (0..4).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0)
}

/// Selection strength for a pixel `d` away from the reference colour.
fn strength(d: u8, s: &WandSettings) -> u8 {
    let limit = s.tolerance.clamp(0.0, 100.0) * 2.55;
    let d = d as f32;
    if d <= limit {
        return 255;
    }
    // Anti-aliased edges sit between the two colours, so fade over a band past the limit.
    let band = if s.smooth { (limit * 0.5).max(12.0) } else { 0.0 };
    if d >= limit + band { 0 } else { (255.0 * (1.0 - (d - limit) / band)).round() as u8 }
}

/// Select the area around `(x, y)` that is close in colour to it.
pub fn magic_wand(img: &RgbaImage, x: u32, y: u32, s: &WandSettings) -> Vec<u8> {
    let reference = img.get_pixel(x, y).0;
    select(img, reference, &[(x, y)], s)
}

/// Select the background: the most common colour around the edges, flooding inwards
/// from every edge pixel of that colour.
pub fn select_background(img: &RgbaImage, s: &WandSettings) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let border: Vec<(u32, u32)> = (0..w)
        .flat_map(|x| [(x, 0), (x, h - 1)])
        .chain((0..h).flat_map(|y| [(0, y), (w - 1, y)]))
        .collect();
    // Bucket colours coarsely so noise and gradients still vote together.
    let mut counts = std::collections::HashMap::new();
    for &(x, y) in &border {
        let p = img.get_pixel(x, y).0;
        *counts.entry(p.map(|c| c >> 4)).or_insert(0u32) += 1;
    }
    let Some((bucket, _)) = counts.into_iter().max_by_key(|&(k, n)| (n, k)) else { return vec![0; (w * h) as usize] };
    let members: Vec<[u8; 4]> =
        border.iter().map(|&(x, y)| img.get_pixel(x, y).0).filter(|p| p.map(|c| c >> 4) == bucket).collect();
    // The reference is the average of the winning bucket, a stable "background colour".
    let mut sum = [0u32; 4];
    for p in &members {
        for i in 0..4 {
            sum[i] += p[i] as u32;
        }
    }
    let reference = sum.map(|v| (v / members.len() as u32) as u8);
    select(img, reference, &border, s)
}

fn select(img: &RgbaImage, reference: [u8; 4], seeds: &[(u32, u32)], s: &WandSettings) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let px = img.as_raw();
    let at = |i: usize| [px[i * 4], px[i * 4 + 1], px[i * 4 + 2], px[i * 4 + 3]];
    let mut sel = vec![0u8; w as usize * h as usize];
    if !s.contiguous {
        for (i, v) in sel.iter_mut().enumerate() {
            *v = strength(distance(at(i), reference), s);
        }
    } else {
        // Flood through fully matching pixels; partially matching ones get their soft
        // value but stop the fill, so the selection doesn't leak along gradients.
        let mut stack: Vec<usize> = Vec::new();
        for &(x, y) in seeds {
            let i = (y * w + x) as usize;
            if sel[i] == 0 && strength(distance(at(i), reference), s) == 255 {
                sel[i] = 255;
                stack.push(i);
            }
        }
        while let Some(i) = stack.pop() {
            let (x, y) = ((i as u32) % w, (i as u32) / w);
            let neighbours = [
                (x > 0).then(|| i - 1),
                (x + 1 < w).then(|| i + 1),
                (y > 0).then(|| i - w as usize),
                (y + 1 < h).then(|| i + w as usize),
            ];
            for n in neighbours.into_iter().flatten() {
                if sel[n] == 255 {
                    continue;
                }
                let v = strength(distance(at(n), reference), s);
                if v == 255 {
                    stack.push(n);
                }
                sel[n] = sel[n].max(v);
            }
        }
    }
    feather(&mut sel, w, h, s.feather);
    sel
}

/// Box-blur a mask twice in each direction (close to a Gaussian) with the given radius.
pub fn feather(mask: &mut [u8], w: u32, h: u32, radius: u32) {
    if radius == 0 {
        return;
    }
    let (w, h, r) = (w as usize, h as usize, radius as usize);
    let mut tmp = vec![0u8; mask.len()];
    for _ in 0..2 {
        for y in 0..h {
            blur_line(&mask[y * w..(y + 1) * w], 1, &mut tmp[y * w..], 1, w, r);
        }
        for x in 0..w {
            blur_line(&tmp[x..], w, &mut mask[x..], w, h, r);
        }
    }
}

/// Moving average of `len` values spaced `stride` apart, with edges clamped.
fn blur_line(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, len: usize, r: usize) {
    let get = |i: isize| src[i.clamp(0, len as isize - 1) as usize * src_stride] as u32;
    let width = (2 * r + 1) as u32;
    let mut sum: u32 = (-(r as isize)..=r as isize).map(get).sum();
    for i in 0..len {
        dst[i * dst_stride] = ((sum + width / 2) / width) as u8;
        sum += get(i as isize + r as isize + 1);
        sum -= get(i as isize - r as isize);
    }
}

/// Every image in `path` (animation frames, pages) as 8-bit sRGB for the editor, along with
/// the first image's full size. All are scaled to one working size: at most `max_side` wide
/// and high, and small enough that all of them together hold at most `max_pixels` pixels.
/// Float (HDR) sources are tone-mapped the same way as when converted to an 8-bit format.
pub fn decode_for_edit(
    path: &Path,
    from: Format,
    options: &Options,
    max_side: u32,
    max_pixels: u64,
) -> Result<(Vec<RgbaImage>, (u32, u32))> {
    let converter = crate::ImageConverter::default();
    let decoder = converter.decoders.iter().find(|d| d.format() == from).ok_or(Error::NoRoute { from, to: from })?;
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let ctx = ConvertCtx::new(options, &cancel, &progress);
    let img = decoder.decode(path, usize::MAX, &ctx)?;
    Ok(fit_for_edit(img, from, options, max_side, max_pixels))
}

/// Decoded images as the editor's 8-bit working copies (capped like [`decode_for_edit`]),
/// with the first image's real size. Videos' picked frames come in this way too.
pub fn fit_for_edit(img: ImageData, from: Format, options: &Options, max_side: u32, max_pixels: u64) -> (Vec<RgbaImage>, (u32, u32)) {
    // Load caption fonts now, in the background, rather than on the first caption.
    crate::caption::fonts();
    let img = crate::color::for_encoder(img, false, from, options);
    let (w, h) = (img.first().width(), img.first().height());
    let n = img.frames.len() as f64;
    let scale = (max_side as f64 / w.max(h) as f64)
        .min((max_pixels as f64 / (n * w as f64 * h as f64)).sqrt())
        .min(1.0);
    let (nw, nh) = (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1));
    // Pages of other sizes (TIFF pages, icon sizes) are stretched to the same working size;
    // their edits are stretched back when converting.
    let frames = img
        .frames
        .iter()
        .map(|f| {
            let rgba = f.image.to_rgba8();
            if rgba.dimensions() == (nw, nh) { rgba } else { image::imageops::resize(&rgba, nw, nh, FilterType::Triangle) }
        })
        .collect();
    (frames, (w, h))
}

/// `mask` stretched to `w`×`h`.
pub fn resize_mask(mask: &Mask, w: u32, h: u32) -> Vec<u8> {
    fit(mask, w, h).into_owned()
}

/// `img` with its alpha multiplied by `keep`.
pub fn composite(img: &RgbaImage, keep: &[u8]) -> RgbaImage {
    let mut out = img.clone();
    scale_alpha(&mut out, 4, keep, |a, k| ((a as u32 * k as u32 + 127) / 255) as u8);
    out
}

/// The part of `img` inside `region`.
pub fn crop(img: &RgbaImage, region: &Region) -> RgbaImage {
    let (x, y, w, h) = region.scaled(img.width(), img.height());
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

/// `img` scaled down to fit in `max_side`×`max_side`.
pub fn thumbnail(img: &RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let scale = (max_side as f32 / w.max(h) as f32).min(1.0);
    let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
    image::imageops::thumbnail(img, nw, nh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// A white square in the middle of a blue image, with a 1-pixel half-blend ring.
    fn square() -> RgbaImage {
        RgbaImage::from_fn(20, 20, |x, y| {
            let inside = (5..15).contains(&x) && (5..15).contains(&y);
            let ring = (4..16).contains(&x) && (4..16).contains(&y);
            if inside {
                Rgba([255, 255, 255, 255])
            } else if ring {
                Rgba([128, 128, 255, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        })
    }

    #[test]
    fn wand_selects_connected_similar_pixels_with_soft_edge() {
        let img = square();
        let hard = WandSettings { smooth: false, ..Default::default() };
        let sel = magic_wand(&img, 0, 0, &hard);
        assert_eq!(sel[0], 255);
        assert_eq!(sel[4 * 20 + 4], 0, "ring is too different");
        assert_eq!(sel[10 * 20 + 10], 0);

        let smooth = WandSettings { tolerance: 40.0, ..Default::default() };
        let sel = magic_wand(&img, 0, 0, &smooth);
        assert!((1..255).contains(&sel[4 * 20 + 4]), "ring is partly selected: {}", sel[4 * 20 + 4]);
        assert_eq!(sel[10 * 20 + 10], 0, "the fill stops at the ring");
    }

    #[test]
    fn global_mode_ignores_connectivity() {
        // Two white blocks separated by black: contiguous picks one, global both.
        let img = RgbaImage::from_fn(9, 1, |x, _| if x == 4 { Rgba([0, 0, 0, 255]) } else { Rgba([255; 4]) });
        let s = WandSettings { smooth: false, ..Default::default() };
        assert_eq!(magic_wand(&img, 0, 0, &s)[8], 0);
        assert_eq!(magic_wand(&img, 0, 0, &WandSettings { contiguous: false, ..s })[8], 255);
    }

    #[test]
    fn background_is_the_edge_colour() {
        let sel = select_background(&square(), &WandSettings { smooth: false, ..Default::default() });
        assert_eq!(sel[0], 255);
        assert_eq!(sel[19 * 20 + 19], 255);
        assert_eq!(sel[10 * 20 + 10], 0);
    }

    #[test]
    fn feather_softens_a_hard_edge() {
        let mut m: Vec<u8> = (0..10).map(|x| if x < 5 { 255 } else { 0 }).collect();
        feather(&mut m, 10, 1, 1);
        assert_eq!(m[0], 255);
        assert_eq!(m[9], 0);
        assert!((1..255).contains(&m[4]) && (1..255).contains(&m[5]));
    }

    #[test]
    fn erase_adds_alpha_and_stretches_the_mask() {
        let rgb = image::RgbImage::from_pixel(4, 4, image::Rgb([10, 20, 30]));
        let mut img = ImageData::still(ImageRgb8(rgb));
        // 2x2 mask removing the left half, applied to a 4x4 image.
        let mask = Mask::new(2, 2, vec![0, 255, 0, 255]);
        apply(&mut img, &Edits { ops: vec![EditOp::Erase { frame: 0, mask }] });
        let out = img.first().as_rgba8().expect("alpha added").clone();
        assert_eq!(out.get_pixel(0, 0).0, [10, 20, 30, 0]);
        assert_eq!(out.get_pixel(3, 3).0[3], 255);

        let mut deep = ImageData::still(ImageRgb16(image::ImageBuffer::from_pixel(2, 1, image::Rgb([1000u16; 3]))));
        apply(&mut deep, &Edits { ops: vec![EditOp::Erase { frame: 0, mask: Mask::new(2, 1, vec![0, 128]) }] });
        let out = deep.first().as_rgba16().expect("16-bit kept");
        assert_eq!(out.get_pixel(0, 0).0[3], 0);
        assert_eq!(out.get_pixel(1, 0).0[3], 32896);
    }

    fn line(color: [u8; 4]) -> Drawing {
        // A 2-px horizontal line across the middle of a 10x10 working copy.
        let stroke = dotty_core::Stroke { points: vec![(0.0, 5.0), (10.0, 5.0)], width: 2.0, color };
        Drawing { strokes: vec![stroke], of_width: 10, of_height: 10 }
    }

    #[test]
    fn strokes_are_drawn_scaled_and_keep_opaque_images_opaque() {
        let mut img = ImageData::still(ImageRgb8(image::RgbImage::from_pixel(20, 20, image::Rgb([255, 255, 255]))));
        apply(&mut img, &Edits { ops: vec![EditOp::Draw(line([255, 0, 0, 255]))] });
        let out = img.first().as_rgb8().expect("still RGB, no alpha added");
        assert_eq!(out.get_pixel(10, 9).0, [255, 0, 0], "line scaled to 4 px around y=10");
        assert_eq!(out.get_pixel(10, 2).0, [255, 255, 255]);

        // Grey becomes colour; a dot is a filled circle.
        let mut grey = ImageData::still(ImageLuma8(image::GrayImage::from_pixel(10, 10, image::Luma([0]))));
        let dot = dotty_core::Stroke { points: vec![(5.0, 5.0)], width: 4.0, color: [0, 255, 0, 255] };
        apply(&mut grey, &Edits { ops: vec![EditOp::Draw(Drawing { strokes: vec![dot], of_width: 10, of_height: 10 })] });
        let out = grey.first().as_rgb8().expect("colour");
        assert_eq!(out.get_pixel(5, 5).0, [0, 255, 0]);
        assert_eq!(out.get_pixel(0, 0).0, [0, 0, 0]);
    }

    #[test]
    fn ink_covers_erased_pixels_and_float_ink_is_linear() {
        let mut img = ImageData::still(ImageRgba8(RgbaImage::from_pixel(10, 10, Rgba([0, 0, 255, 255]))));
        let erase = EditOp::Erase { frame: 0, mask: Mask::new(10, 10, vec![0; 100]) };
        apply(&mut img, &Edits { ops: vec![erase, EditOp::Draw(line([255, 255, 0, 255]))] });
        let out = img.first().as_rgba8().unwrap();
        assert_eq!(out.get_pixel(5, 5).0, [255, 255, 0, 255], "ink is opaque over transparency");
        assert_eq!(out.get_pixel(5, 0).0[3], 0);

        let mut hdr = ImageData::still(ImageRgb32F(image::Rgb32FImage::from_pixel(10, 10, image::Rgb([4.0, 4.0, 4.0]))));
        apply(&mut hdr, &Edits { ops: vec![EditOp::Draw(line([128, 0, 0, 255]))] });
        let p = hdr.first().as_rgb32f().unwrap().get_pixel(5, 5).0;
        assert!((p[0] - crate::color::srgb_to_linear(128.0 / 255.0)).abs() < 1e-3 && p[1] == 0.0, "{p:?}");
        assert_eq!(hdr.first().as_rgb32f().unwrap().get_pixel(5, 0).0, [4.0; 3], "HDR values untouched elsewhere");
    }

    #[test]
    fn captions_are_written_on_every_frame_and_cropped_after() {
        let mut img = ImageData::still(ImageRgb8(image::RgbImage::from_pixel(40, 40, image::Rgb([30, 60, 90]))));
        img.frames.push(crate::Frame { image: ImageRgb8(image::RgbImage::from_pixel(40, 40, image::Rgb([30, 60, 90]))), delay: Default::default() });
        let caption = dotty_core::Caption {
            text: "I".into(),
            x: 10.0,
            y: 10.0,
            size: 12.0,
            font: dotty_core::CaptionFont::Meme,
            uppercase: true,
            fill: [255, 255, 0, 255],
            outline: [0, 0, 0, 255],
            outline_width: 0.0,
        };
        let captions = Captions { items: vec![caption], of_width: 20, of_height: 20 };
        let crop = Region { x: 0, y: 0, width: 10, height: 20, of_width: 20, of_height: 20 };
        apply(&mut img, &Edits { ops: vec![EditOp::Text(captions), EditOp::Crop(crop)] });
        for f in &img.frames {
            let out = f.image.as_rgb8().expect("opaque stays RGB");
            assert_eq!(out.dimensions(), (20, 40));
            // The caption sat at x = 20 of 40; after cropping to the left half it's at the edge.
            assert!((0..40).any(|y| out.get_pixel(19, y).0 == [255, 255, 0]), "yellow text on each frame");
        }
    }

    #[test]
    fn crop_scales_to_each_frame_after_erasing() {
        let mut img = ImageData::still(ImageRgba8(RgbaImage::from_fn(8, 8, |x, y| Rgba([x as u8, y as u8, 0, 255]))));
        img.frames.push(crate::Frame { image: ImageRgba8(RgbaImage::new(8, 8)), delay: Default::default() });
        // Erase pixel (4, 4), then keep the bottom-right quarter, picked on a 4x4 working copy.
        let mut keep = vec![255; 64];
        keep[4 * 8 + 4] = 0;
        let region = Region { x: 2, y: 2, width: 2, height: 2, of_width: 4, of_height: 4 };
        apply(&mut img, &Edits { ops: vec![EditOp::Erase { frame: 0, mask: Mask::new(8, 8, keep) }, EditOp::Crop(region)] });
        let first = img.first().to_rgba8();
        assert_eq!(first.dimensions(), (4, 4));
        assert_eq!(first.get_pixel(0, 0).0, [4, 4, 0, 0]);
        assert_eq!(first.get_pixel(3, 3).0, [7, 7, 0, 255]);
        assert_eq!((img.frames[1].image.width(), img.frames[1].image.height()), (4, 4), "every frame is cropped");
    }
}
