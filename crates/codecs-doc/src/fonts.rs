//! The built-in fonts (Liberation Sans and Mono, SIL OFL 1.1) and measuring text with them.

use crate::model::Style;
use rustybuzz::{Face, UnicodeBuffer};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontId {
    Sans,
    SansBold,
    SansItalic,
    SansBoldItalic,
    Mono,
    MonoBold,
}

impl FontId {
    pub const ALL: [FontId; 6] = [FontId::Sans, FontId::SansBold, FontId::SansItalic, FontId::SansBoldItalic, FontId::Mono, FontId::MonoBold];

    pub fn of(style: &Style, force_bold: bool) -> FontId {
        let bold = style.bold || force_bold;
        match (style.code, bold, style.italic) {
            (true, false, _) => FontId::Mono,
            (true, true, _) => FontId::MonoBold,
            (false, false, false) => FontId::Sans,
            (false, true, false) => FontId::SansBold,
            (false, false, true) => FontId::SansItalic,
            (false, true, true) => FontId::SansBoldItalic,
        }
    }

    fn data(self) -> &'static [u8] {
        match self {
            FontId::Sans => include_bytes!("../fonts/LiberationSans-Regular.ttf"),
            FontId::SansBold => include_bytes!("../fonts/LiberationSans-Bold.ttf"),
            FontId::SansItalic => include_bytes!("../fonts/LiberationSans-Italic.ttf"),
            FontId::SansBoldItalic => include_bytes!("../fonts/LiberationSans-BoldItalic.ttf"),
            FontId::Mono => include_bytes!("../fonts/LiberationMono-Regular.ttf"),
            FontId::MonoBold => include_bytes!("../fonts/LiberationMono-Bold.ttf"),
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|f| *f == self).unwrap()
    }
}

pub struct Fonts {
    faces: Vec<Face<'static>>,
    cache: HashMap<(FontId, String), (f32, u32)>,
}

impl Fonts {
    pub fn new() -> Self {
        Self { faces: FontId::ALL.iter().map(|f| Face::from_slice(f.data(), 0).expect("built-in font")).collect(), cache: HashMap::new() }
    }

    /// Ascent and descent (descent negative) as fractions of the font size.
    pub fn vertical(&self, f: FontId) -> (f32, f32) {
        let face = &self.faces[f.index()];
        let u = face.units_per_em() as f32;
        (face.ascender() as f32 / u, face.descender() as f32 / u)
    }

    /// Advance width of `text` and how many of its characters the font has no glyph for.
    fn measure(&mut self, f: FontId, text: &str) -> (f32, u32) {
        if let Some(v) = self.cache.get(&(f, text.to_string())) {
            return *v;
        }
        let face = &self.faces[f.index()];
        let mut buf = UnicodeBuffer::new();
        buf.push_str(text);
        let shaped = rustybuzz::shape(face, &[], buf);
        let units: i32 = shaped.glyph_positions().iter().map(|p| p.x_advance).sum();
        let missing = shaped.glyph_infos().iter().filter(|g| g.glyph_id == 0).count() as u32;
        let v = (units as f32 / face.units_per_em() as f32, missing);
        if self.cache.len() < 100_000 {
            self.cache.insert((f, text.to_string()), v);
        }
        v
    }

    pub fn width(&mut self, f: FontId, text: &str, size: f32) -> f32 {
        self.measure(f, text).0 * size
    }

    pub fn missing(&mut self, f: FontId, text: &str) -> u32 {
        self.measure(f, text).1
    }
}

impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

/// The fonts as PDF fonts (made once per document).
pub fn pdf_fonts() -> Vec<krilla::text::Font> {
    FontId::ALL
        .iter()
        .map(|f| {
            krilla::text::Font::new(krilla::Data::from(f.data()), 0).expect("built-in font")
        })
        .collect()
}

pub fn pdf_index(f: FontId) -> usize {
    f.index()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_measured_and_missing_glyphs_counted() {
        let mut f = Fonts::new();
        let w = f.width(FontId::Sans, "Hello", 10.0);
        assert!(w > 20.0 && w < 40.0, "{w}");
        assert!(f.width(FontId::SansBold, "Hello", 10.0) > w, "bold is wider");
        let mono = f.width(FontId::Mono, "iiii", 10.0);
        assert!((mono - f.width(FontId::Mono, "WWWW", 10.0)).abs() < 0.01, "monospace");
        assert_eq!(f.missing(FontId::Sans, "abc"), 0);
        assert_eq!(f.missing(FontId::Sans, "a\u{4e2d}\u{6587}"), 2, "Chinese isn't in the built-in font");
        let (asc, desc) = f.vertical(FontId::Sans);
        assert!(asc > 0.8 && asc < 1.0 && desc < 0.0 && desc > -0.4);
    }
}
