//! Edits made in the editor before converting (pixels for images, stages for audio). They travel with a job and the
//! converter applies them between decoding and encoding, so the source is never touched.

use std::fmt;
use std::sync::Arc;

/// The edits for one file, applied in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Edits {
    pub ops: Vec<EditOp>,
}

impl Edits {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The erase mask for the first image, if any.
    pub fn mask(&self) -> Option<&Mask> {
        self.frame_mask(0)
    }

    /// The erase mask for image `frame` (an animation frame or page), if any.
    pub fn frame_mask(&self, frame: usize) -> Option<&Mask> {
        self.ops.iter().find_map(|op| match op {
            EditOp::Erase { frame: f, mask } if *f == frame => Some(mask),
            _ => None,
        })
    }

    pub fn crop(&self) -> Option<Region> {
        self.ops.iter().find_map(|op| match op {
            EditOp::Crop(r) => Some(*r),
            _ => None,
        })
    }

    pub fn drawing(&self) -> Option<&Drawing> {
        self.ops.iter().find_map(|op| match op {
            EditOp::Draw(d) => Some(d),
            _ => None,
        })
    }

    pub fn captions(&self) -> Option<&Captions> {
        self.ops.iter().find_map(|op| match op {
            EditOp::Text(c) => Some(c),
            _ => None,
        })
    }

    /// The audio edits, if any.
    pub fn audio(&self) -> Option<&AudioEdits> {
        self.ops.iter().find_map(|op| match op {
            EditOp::Audio(a) => Some(a),
            _ => None,
        })
    }

    /// Whether the edits make parts of the image transparent.
    pub fn erases(&self) -> bool {
        self.ops.iter().any(|op| matches!(op, EditOp::Erase { .. }))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Multiply the alpha of image `frame` by `mask` (makes parts of it transparent).
    Erase { frame: usize, mask: Mask },
    /// Draw pen strokes over every image (before cropping, so they line up with the picture).
    Draw(Drawing),
    /// Write captions over every image (after drawing, before cropping).
    Text(Captions),
    /// Keep only this part of every image (animation frames are cropped alike).
    Crop(Region),
    /// Trim, fades and other changes to an audio file.
    Audio(AudioEdits),
}

/// Changes to an audio file. Unlike image edits these are a fixed set of knobs: the
/// pipeline turns them into stages in a set order (trim, then fades).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioEdits {
    /// Keep from here (seconds into the source).
    pub trim_start: f64,
    /// Keep up to here (seconds into the source); `None` = to the end.
    pub trim_end: Option<f64>,
    /// Fade-in and fade-out lengths in seconds of output.
    pub fade_in: f64,
    pub fade_out: f64,
}

impl Default for AudioEdits {
    fn default() -> Self {
        Self { trim_start: 0.0, trim_end: None, fade_in: 0.0, fade_out: 0.0 }
    }
}

impl AudioEdits {
    /// Whether these change nothing.
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }

    /// The kept range in frames of a stream at `rate` Hz that is `total` frames long
    /// (if known). Always at least one frame wide when the source has any.
    pub fn trim_frames(&self, rate: u32, total: Option<u64>) -> (u64, Option<u64>) {
        let at = |s: f64| (s.max(0.0) * rate as f64).round() as u64;
        let mut start = at(self.trim_start);
        let mut end = self.trim_end.map(at);
        if let Some(t) = total {
            start = start.min(t.saturating_sub(1));
            end = Some(end.unwrap_or(t).min(t));
        }
        if let Some(e) = &mut end {
            *e = (*e).max(start + 1);
        }
        (start, end)
    }
}

/// Captions placed on an image `of_width`×`of_height` (the editor's working size); scaled
/// to the size the image decodes at and rendered there.
#[derive(Debug, Clone, PartialEq)]
pub struct Captions {
    pub items: Vec<Caption>,
    pub of_width: u32,
    pub of_height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Caption {
    /// May hold several lines; they are centred on each other.
    pub text: String,
    /// Centre of the text, in working-size pixels.
    pub x: f32,
    pub y: f32,
    /// Font size in working-size pixels.
    pub size: f32,
    pub font: CaptionFont,
    pub uppercase: bool,
    /// sRGB colours with straight alpha.
    pub fill: [u8; 4],
    pub outline: [u8; 4],
    /// Outline thickness as a fraction of the font size (0 for none).
    pub outline_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptionFont {
    /// The bold condensed meme look (Anton, bundled so it looks the same everywhere).
    Meme,
    /// The system's bold sans-serif.
    Sans,
}

/// Pen strokes drawn on an image `of_width`×`of_height` (the editor's working size); scaled
/// to the size the image actually decodes at and rasterized there, so lines stay crisp.
#[derive(Debug, Clone, PartialEq)]
pub struct Drawing {
    pub strokes: Vec<Stroke>,
    pub of_width: u32,
    pub of_height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    /// The pen's path in working-size pixels; a single point is a dot.
    pub points: Vec<(f32, f32)>,
    /// Line width in working-size pixels.
    pub width: f32,
    /// sRGB colour with straight (not premultiplied) alpha.
    pub color: [u8; 4],
}

/// A rectangle picked on an image `of_width`×`of_height` (the editor's working size). It is
/// scaled to the size the image actually decodes at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub of_width: u32,
    pub of_height: u32,
}

impl Region {
    /// `(x, y, width, height)` on an image `w`×`h`, at least one pixel and inside it.
    pub fn scaled(&self, w: u32, h: u32) -> (u32, u32, u32, u32) {
        let scale = |v: u32, of: u32, to: u32| ((v as u64 * to as u64 + of as u64 / 2) / of.max(1) as u64) as u32;
        let x0 = scale(self.x, self.of_width, w).min(w.saturating_sub(1));
        let y0 = scale(self.y, self.of_height, h).min(h.saturating_sub(1));
        let x1 = scale(self.x + self.width, self.of_width, w).clamp(x0 + 1, w.max(1));
        let y1 = scale(self.y + self.height, self.of_height, h).clamp(y0 + 1, h.max(1));
        (x0, y0, x1 - x0, y1 - y0)
    }

    /// Whether the region is the whole image (no crop at all).
    pub fn is_full(&self) -> bool {
        self.x == 0 && self.y == 0 && self.width == self.of_width && self.height == self.of_height
    }
}

/// One byte per pixel, row-major: 255 keeps a pixel, 0 removes it, values between fade it.
/// It may be smaller than the image (the editor works on a capped size), in which case it
/// is stretched to fit.
#[derive(Clone, PartialEq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub data: Arc<[u8]>,
}

impl Mask {
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width as usize * height as usize, "mask size");
        Self { width, height, data: data.into() }
    }

    /// Whether every pixel is kept as it was.
    pub fn is_noop(&self) -> bool {
        self.data.iter().all(|&v| v == 255)
    }
}

impl fmt::Debug for Mask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mask({}x{})", self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_scales_and_stays_inside() {
        let r = Region { x: 10, y: 20, width: 30, height: 40, of_width: 100, of_height: 100 };
        assert_eq!(r.scaled(100, 100), (10, 20, 30, 40));
        assert_eq!(r.scaled(1000, 50), (100, 10, 300, 20));
        // Tiny targets still get a pixel.
        assert_eq!(r.scaled(1, 1), (0, 0, 1, 1));
        let edge = Region { x: 99, y: 99, width: 1, height: 1, of_width: 100, of_height: 100 };
        assert_eq!(edge.scaled(10, 10), (9, 9, 1, 1));
    }
}
