//! Moving between linear-light float pixels (HDR / EXR) and sRGB-encoded integers.

use crate::ImageData;
use image::{DynamicImage, ImageBuffer, Rgb, Rgba};
use transformer_core::{Choice, Format, Options};

pub const HIGHLIGHTS: Choice = Choice {
    key: "highlights",
    label: "Bright areas",
    help: "HDR images can be brighter than a normal image can show. Compress keeps detail in bright areas; Clip keeps everything else exactly as bright as it was.",
    values: &[("compress", "Compress"), ("clip", "Clip")],
    multi_image_only: false,
};

fn is_float(img: &DynamicImage) -> bool {
    matches!(img, DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_))
}

/// Convert every frame to what the encoder takes: linear float, or sRGB integers.
pub fn for_encoder(mut img: ImageData, float: bool, from: Format, options: &Options) -> ImageData {
    let compress = options.choice(from, &HIGHLIGHTS) == "compress";
    for frame in &mut img.frames {
        if float && !is_float(&frame.image) {
            frame.image = to_linear(&frame.image);
        } else if !float && is_float(&frame.image) {
            frame.image = to_srgb16(&frame.image, compress);
        }
    }
    img
}

pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

pub fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

fn to_linear(img: &DynamicImage) -> DynamicImage {
    if img.color().has_alpha() {
        let mut f = img.to_rgba32f();
        for p in f.pixels_mut() {
            for c in &mut p.0[..3] {
                *c = srgb_to_linear(*c);
            }
        }
        DynamicImage::ImageRgba32F(f)
    } else {
        let mut f = img.to_rgb32f();
        for p in f.pixels_mut() {
            for c in &mut p.0 {
                *c = srgb_to_linear(*c);
            }
        }
        DynamicImage::ImageRgb32F(f)
    }
}

/// Extended Reinhard on luminance, white point at the brightest pixel: content that
/// already fits in [0, 1] passes through almost unchanged.
fn tone_mapper(img: &DynamicImage) -> impl Fn([f32; 3]) -> [f32; 3] {
    let lum = |p: [f32; 3]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let rgba = img.to_rgba32f();
    let white = rgba.pixels().map(|p| lum([p[0], p[1], p[2]])).fold(1.0f32, f32::max);
    move |p| {
        let l = lum(p);
        if l <= 0.0 {
            return p;
        }
        let mapped = l * (1.0 + l / (white * white)) / (1.0 + l);
        let scale = mapped / l;
        [p[0] * scale, p[1] * scale, p[2] * scale]
    }
}

fn to_srgb16(img: &DynamicImage, compress: bool) -> DynamicImage {
    let map = tone_mapper(img);
    let q = |v: f32| (linear_to_srgb(v) * 65535.0 + 0.5) as u16;
    let rgba = img.to_rgba32f();
    let (w, h) = rgba.dimensions();
    if img.color().has_alpha() {
        DynamicImage::ImageRgba16(ImageBuffer::from_fn(w, h, |x, y| {
            let p = rgba.get_pixel(x, y).0;
            let c = if compress { map([p[0], p[1], p[2]]) } else { [p[0], p[1], p[2]] };
            Rgba([q(c[0]), q(c[1]), q(c[2]), (p[3].clamp(0.0, 1.0) * 65535.0 + 0.5) as u16])
        }))
    } else {
        DynamicImage::ImageRgb16(ImageBuffer::from_fn(w, h, |x, y| {
            let p = rgba.get_pixel(x, y).0;
            let c = if compress { map([p[0], p[1], p[2]]) } else { [p[0], p[1], p[2]] };
            Rgb([q(c[0]), q(c[1]), q(c[2])])
        }))
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn srgb_round_trips_and_in_range_content_survives_tone_mapping() {
        for i in 0..=255 {
            let v = i as f32 / 255.0;
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-5);
        }
        let img = DynamicImage::ImageRgb32F(ImageBuffer::from_pixel(2, 2, Rgb([0.2f32, 0.5, 0.8])));
        let out = to_srgb16(&img, true).to_rgb32f();
        let back = srgb_to_linear(out.get_pixel(0, 0)[1]);
        assert!((back - 0.5).abs() < 0.01, "{back}");
    }
}
