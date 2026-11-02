//! Applying [`VideoEdits`] to pictures: the geometry (rotate, flip, crop, resize), fades and
//! text. The conversion and the editor's preview both use this, so what's previewed is
//! what's written.

use codecs_image::caption;
use dotty_core::{Captions, VideoEdits};
use image::RgbImage;
use image::imageops::{self, FilterType};

/// The picture after rotation, flips, crop and resize, for a source of `src` pixels with
/// the file's own `rotation`. Even sides, at least 2.
pub fn output_size(src: (u32, u32), rotation: u16, e: &VideoEdits) -> (u32, u32) {
    let turned = if (rotation + e.rotate) % 180 == 90 { (src.1, src.0) } else { src };
    let (mut w, mut h) = turned;
    if let Some(c) = &e.crop {
        let (_, _, cw, ch) = c.scaled(w, h);
        (w, h) = (cw, ch);
    }
    if let Some(side) = e.max_side.filter(|&s| s < w.max(h)) {
        let k = side as f64 / w.max(h) as f64;
        (w, h) = (((w as f64 * k).round() as u32).max(2), ((h as f64 * k).round() as u32).max(2));
    }
    ((w & !1).max(2), (h & !1).max(2))
}

/// Rotate (file rotation plus the edit's), flip, crop, resize to [`output_size`].
pub fn geometry(img: RgbImage, rotation: u16, e: &VideoEdits) -> RgbImage {
    let size_in = (img.width(), img.height());
    let target = output_size(size_in, rotation, e);
    let mut img = crate::rotate(img, (rotation + e.rotate) % 360);
    if e.flip_h {
        img = imageops::flip_horizontal(&img);
    }
    if e.flip_v {
        img = imageops::flip_vertical(&img);
    }
    if let Some(c) = &e.crop {
        let (x, y, w, h) = c.scaled(img.width(), img.height());
        img = imageops::crop_imm(&img, x, y, w, h).to_image();
    }
    if (img.width(), img.height()) != target {
        img = imageops::resize(&img, target.0, target.1, FilterType::Triangle);
    }
    img
}

/// Make each text's reference frame the shape of the result `size`, keeping the text at the
/// same fractional place, so it scales evenly when a crop or turn changes the picture's shape.
/// Returns whether anything changed.
pub fn fit_texts(e: &mut VideoEdits, size: (u32, u32)) -> bool {
    let mut changed = false;
    for t in &mut e.texts {
        let want = ((t.of_width as f64 * size.1 as f64 / size.0.max(1) as f64).round() as u32).max(1);
        if want.abs_diff(t.of_height) > 1 {
            t.caption.y *= want as f32 / t.of_height.max(1) as f32;
            t.of_height = want;
            changed = true;
        }
    }
    changed
}

/// 0 (black) to 1 (untouched) at `t` seconds of a result `duration` seconds long.
pub fn fade_level(t: f64, duration: f64, e: &VideoEdits) -> f32 {
    let mut level = 1.0f64;
    if e.fade_in > 0.0 {
        level = level.min(t / e.fade_in);
    }
    if e.fade_out > 0.0 {
        level = level.min((duration - t) / e.fade_out);
    }
    level.clamp(0.0, 1.0) as f32
}

/// Text overlays rendered once per size and reused for every frame they show on.
#[derive(Default)]
pub struct Overlays {
    size: (u32, u32),
    /// Per text (same order as `VideoEdits::texts`): its rendered layer (premultiplied RGBA).
    layers: Vec<Option<Vec<u8>>>,
}

impl Overlays {
    fn layer(&mut self, e: &VideoEdits, i: usize, size: (u32, u32)) -> &[u8] {
        if self.size != size || self.layers.len() != e.texts.len() {
            *self = Overlays { size, layers: vec![None; e.texts.len()] };
        }
        self.layers[i].get_or_insert_with(|| {
            let t = &e.texts[i];
            let captions = Captions { items: vec![t.caption.clone()], of_width: t.of_width, of_height: t.of_height };
            caption::layer(&captions, size.0, size.1)
        })
    }

    /// Draw the texts showing at `t`, then apply the fade.
    pub fn apply(&mut self, img: &mut RgbImage, e: &VideoEdits, t: f64, duration: f64) {
        let size = (img.width(), img.height());
        for i in 0..e.texts.len() {
            let text = &e.texts[i];
            if t < text.start || text.end.is_some_and(|end| t >= end) {
                continue;
            }
            let layer = self.layer(e, i, size);
            for (px, ink) in img.pixels_mut().zip(layer.as_chunks::<4>().0) {
                let a = ink[3] as u32;
                if a == 0 {
                    continue;
                }
                // Premultiplied source-over.
                for (dst, &src) in px.0.iter_mut().zip(ink) {
                    *dst = (src as u32 + (*dst as u32 * (255 - a) + 127) / 255).min(255) as u8;
                }
            }
        }
        let level = fade_level(t, duration, e);
        if level < 1.0 {
            for b in img.as_mut() {
                *b = (*b as f32 * level).round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotty_core::{Caption, CaptionFont, Region, VideoText};

    fn edits() -> VideoEdits {
        VideoEdits::default()
    }

    /// Left half red, right half blue.
    fn halves(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgb([255, 0, 0]) } else { image::Rgb([0, 0, 255]) })
    }

    #[test]
    fn rotation_flip_crop_and_resize_give_the_predicted_size_and_pixels() {
        let img = halves(40, 20);
        // The file's own 90° plus an extra 90°: upside down, same size.
        let both = VideoEdits { rotate: 90, ..edits() };
        assert_eq!(output_size((40, 20), 90, &both), (40, 20));
        let out = geometry(img.clone(), 90, &both);
        assert_eq!(out.dimensions(), (40, 20));
        assert_eq!(out.get_pixel(1, 10).0, [0, 0, 255], "red was on the left; 180° puts it right");
        let flipped = geometry(img.clone(), 0, &VideoEdits { flip_h: true, ..edits() });
        assert_eq!(flipped.get_pixel(1, 10).0, [0, 0, 255]);
        // Crop to the right half (coordinates on a 40×20 frame), then limit the side.
        let crop = Region { x: 20, y: 0, width: 20, height: 20, of_width: 40, of_height: 20 };
        let e = VideoEdits { crop: Some(crop), max_side: Some(10), ..edits() };
        assert_eq!(output_size((40, 20), 0, &e), (10, 10));
        let out = geometry(img.clone(), 0, &e);
        assert_eq!(out.dimensions(), (10, 10));
        assert!(out.pixels().all(|p| p.0 == [0, 0, 255]));
        // A crop made on a smaller preview scales to the real frame.
        let preview = Region { x: 10, y: 0, width: 10, height: 10, of_width: 20, of_height: 10 };
        let e = VideoEdits { crop: Some(preview), ..edits() };
        assert_eq!(output_size((40, 20), 0, &e), (20, 20));
        // Odd results are made even.
        assert_eq!(output_size((41, 21), 0, &edits()), (40, 20));
    }

    /// A picture where every pixel is different (and so tells where it came from).
    fn marked(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, ((x + y) % 251) as u8]))
    }

    #[test]
    fn turning_and_flipping_act_on_what_is_on_screen_whatever_flips_are_on() {
        let src = marked(30, 20);
        for (fh, fv, rotate) in [(false, false, 0), (true, false, 0), (false, true, 90), (true, true, 180), (true, false, 270)] {
            let base = VideoEdits { flip_h: fh, flip_v: fv, rotate, ..edits() };
            let seen = geometry(src.clone(), 0, &base);
            let mut cw = base.clone();
            cw.turn_view(true);
            assert_eq!(geometry(src.clone(), 0, &cw), imageops::rotate90(&seen), "turn right, flips ({fh},{fv}) rotation {rotate}");
            let mut ccw = base.clone();
            ccw.turn_view(false);
            assert_eq!(geometry(src.clone(), 0, &ccw), imageops::rotate270(&seen), "turn left");
            let mut h = base.clone();
            h.flip_view(true);
            assert_eq!(geometry(src.clone(), 0, &h), imageops::flip_horizontal(&seen), "mirror left-right");
            let mut v = base.clone();
            v.flip_view(false);
            assert_eq!(geometry(src.clone(), 0, &v), imageops::flip_vertical(&seen), "mirror top-bottom");
        }
    }

    #[test]
    fn a_crop_box_stays_around_the_same_pixels_through_turns_and_flips() {
        let src = marked(40, 30);
        let mut e = VideoEdits { flip_h: true, rotate: 90, ..edits() };
        let (w, h) = (geometry(src.clone(), 0, &e).width(), geometry(src.clone(), 0, &e).height());
        e.crop = Some(Region { x: 3, y: 5, width: 12, height: 7, of_width: w, of_height: h });
        let cut = |e: &VideoEdits| geometry(src.clone(), 0, e);
        let before = cut(&e);
        let mut turned = e.clone();
        turned.turn_view(true);
        assert_eq!(cut(&turned), imageops::rotate90(&before), "the same pixels, turned");
        let mut mirrored = e.clone();
        mirrored.flip_view(false);
        assert_eq!(cut(&mirrored), imageops::flip_vertical(&before));
        let mut round = e.clone();
        for _ in 0..4 {
            round.turn_view(true);
        }
        assert_eq!(round, e, "four turns are no change");
    }

    #[test]
    fn texts_keep_their_place_when_the_pictures_shape_changes() {
        let caption = Caption {
            text: "A".into(),
            x: 250.0,
            y: 400.0,
            size: 50.0,
            font: CaptionFont::Meme,
            uppercase: true,
            fill: [255; 4],
            outline: [0, 0, 0, 255],
            outline_width: 0.0,
        };
        let mut e = VideoEdits { texts: vec![VideoText { caption, of_width: 1000, of_height: 500, start: 0.0, end: None }], ..edits() };
        assert!(!fit_texts(&mut e, (640, 320)), "already the right shape");
        // The picture becomes square: the same fraction down (80%), over a 1000-high frame.
        assert!(fit_texts(&mut e, (500, 500)));
        let t = &e.texts[0];
        assert_eq!((t.of_height, t.caption.y, t.caption.x), (1000, 800.0, 250.0));
        assert!(!fit_texts(&mut e, (500, 500)), "settled");
    }

    #[test]
    fn fades_and_text_show_at_the_right_times() {
        let e = VideoEdits { fade_in: 1.0, fade_out: 2.0, ..edits() };
        assert_eq!(fade_level(0.0, 10.0, &e), 0.0);
        assert_eq!(fade_level(0.5, 10.0, &e), 0.5);
        assert_eq!(fade_level(5.0, 10.0, &e), 1.0);
        assert_eq!(fade_level(9.0, 10.0, &e), 0.5);
        assert_eq!(fade_level(10.0, 10.0, &e), 0.0);

        let caption = Caption {
            text: "HI".into(),
            x: 50.0,
            y: 50.0,
            size: 40.0,
            font: CaptionFont::Meme,
            uppercase: true,
            fill: [255, 255, 255, 255],
            outline: [0, 0, 0, 255],
            outline_width: 0.08,
        };
        let e = VideoEdits { texts: vec![VideoText { caption, of_width: 100, of_height: 100, start: 1.0, end: Some(2.0) }], ..edits() };
        let mut overlays = Overlays::default();
        let shown = |t: f64, overlays: &mut Overlays| {
            let mut img = RgbImage::from_pixel(100, 100, image::Rgb([0, 128, 0]));
            overlays.apply(&mut img, &e, t, 10.0);
            img.pixels().filter(|p| p.0 != [0, 128, 0]).count()
        };
        assert_eq!(shown(0.5, &mut overlays), 0, "before it starts");
        assert!(shown(1.5, &mut overlays) > 100, "while it shows");
        assert_eq!(shown(2.0, &mut overlays), 0, "the end is exclusive");
        // Rendered once, then reused.
        assert_eq!(overlays.layers.iter().filter(|l| l.is_some()).count(), 1);
    }
}
