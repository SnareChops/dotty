//! Rendering editor captions (meme text): each caption is laid out as a tiny SVG and
//! rendered with resvg, which handles fonts, outlines and anti-aliasing. The meme font
//! (Anton, SIL Open Font License, see `fonts/Anton-OFL.txt`) is bundled so captions look the
//! same on every machine; system fonts fill in characters it lacks.

use resvg::{tiny_skia, usvg};
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};
use dotty_core::{Caption, CaptionFont, Captions};

const ANTON: &[u8] = include_bytes!("../fonts/Anton-Regular.ttf");
/// Line spacing as a multiple of the font size.
const LINE_HEIGHT: f32 = 1.15;

/// A rendered caption: premultiplied RGBA, centred on the caption's position.
pub struct CaptionImage {
    pub width: u32,
    pub height: u32,
    pub premultiplied: Vec<u8>,
}

/// The bundled meme font plus system fonts, loaded once (scanning system fonts takes a
/// moment, so the editor calls this while it decodes the image).
pub fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            db.load_font_data(ANTON.to_vec());
            Arc::new(db)
        })
        .clone()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn hex([r, g, b, _]: [u8; 4]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The caption as SVG, its text centred on (0, 0) horizontally.
fn svg(c: &Caption) -> String {
    let family = match c.font {
        CaptionFont::Meme => "Anton, Impact, sans-serif",
        CaptionFont::Sans => "sans-serif",
    };
    let weight = if c.font == CaptionFont::Sans { "bold" } else { "normal" };
    let text = if c.uppercase { c.text.to_uppercase() } else { c.text.clone() };
    let mut body = String::new();
    for (i, line) in text.lines().enumerate() {
        // An empty tspan would collapse; a no-break space keeps the blank line.
        let line = if line.trim().is_empty() { "\u{a0}".to_string() } else { escape(line) };
        let dy = if i == 0 { 0.0 } else { c.size * LINE_HEIGHT };
        let _ = write!(body, r#"<tspan x="0" dy="{dy}">{line}</tspan>"#);
    }
    // The stroke is centred on the glyph edge and painted under the fill, so only its
    // outer half shows: double it to get the asked-for thickness.
    let stroke = c.outline_width.max(0.0) * c.size * 2.0;
    let outline = if stroke > 0.0 {
        format!(
            r#" stroke="{}" stroke-opacity="{}" stroke-width="{stroke}" stroke-linejoin="round" paint-order="stroke""#,
            hex(c.outline),
            c.outline[3] as f32 / 255.0
        )
    } else {
        String::new()
    };
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><text xml:space="preserve" font-family="{family}" font-weight="{weight}" font-size="{}" text-anchor="middle" fill="{}" fill-opacity="{}"{outline}>{body}</text></svg>"#,
        c.size,
        hex(c.fill),
        c.fill[3] as f32 / 255.0,
    )
}

/// Render one caption at `scale` (output pixels per working pixel). `None` when it shows
/// nothing (empty text, or no font could draw it).
pub fn render(c: &Caption, scale: f32) -> Option<CaptionImage> {
    if c.text.trim().is_empty() || c.size <= 0.0 {
        return None;
    }
    let options = usvg::Options { fontdb: fonts(), ..Default::default() };
    let tree = usvg::Tree::from_str(&svg(c), &options).ok()?;
    if !tree.root().has_children() {
        return None;
    }
    let bounds = tree.root().abs_stroke_bounding_box();
    let (width, height) = ((bounds.width() * scale).ceil() as u32 + 2, (bounds.height() * scale).ceil() as u32 + 2);
    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, 1.0 - bounds.left() * scale, 1.0 - bounds.top() * scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(CaptionImage { width, height, premultiplied: pixmap.take() })
}

/// Every caption as one premultiplied RGBA layer at `w`×`h`.
pub fn layer(captions: &Captions, w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; w as usize * h as usize * 4];
    let (sx, sy) = (w as f32 / captions.of_width.max(1) as f32, h as f32 / captions.of_height.max(1) as f32);
    for c in &captions.items {
        let Some(img) = render(c, (sx + sy) / 2.0) else { continue };
        let left = (c.x * sx - img.width as f32 / 2.0).round() as i64;
        let top = (c.y * sy - img.height as f32 / 2.0).round() as i64;
        for row in 0..img.height as i64 {
            let y = top + row;
            if y < 0 || y >= h as i64 {
                continue;
            }
            for col in 0..img.width as i64 {
                let x = left + col;
                if x < 0 || x >= w as i64 {
                    continue;
                }
                let src = &img.premultiplied[((row * img.width as i64 + col) * 4) as usize..][..4];
                let dst = &mut out[((y * w as i64 + x) * 4) as usize..][..4];
                // Premultiplied source-over: later captions sit on top of earlier ones.
                let keep = 255 - src[3] as u32;
                for i in 0..4 {
                    dst[i] = (src[i] as u32 + (dst[i] as u32 * keep + 127) / 255) as u8;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caption(text: &str) -> Caption {
        Caption {
            text: text.into(),
            x: 50.0,
            y: 50.0,
            size: 20.0,
            font: CaptionFont::Meme,
            uppercase: true,
            fill: [255, 255, 255, 255],
            outline: [0, 0, 0, 255],
            outline_width: 0.08,
        }
    }

    #[test]
    fn bundled_font_is_available() {
        assert!(fonts().faces().any(|f| f.families.iter().any(|(name, _)| name == "Anton")));
    }

    #[test]
    fn renders_with_the_bundled_font_and_an_outline() {
        let img = render(&caption("Hi"), 1.0).expect("text renders");
        // Anton capitals are roughly 0.7 em tall; with the outline and padding, ~20 px.
        assert!((14..40).contains(&img.height), "{}", img.height);
        let px: Vec<&[u8]> = img.premultiplied.chunks(4).collect();
        assert!(px.iter().any(|p| p == &[255, 255, 255, 255]), "white fill");
        assert!(px.iter().any(|p| p == &[0, 0, 0, 255]), "black outline");

        // Twice the scale, about twice the size; more lines, taller.
        let big = render(&caption("Hi"), 2.0).unwrap();
        assert!(big.height.abs_diff(img.height * 2) <= 3);
        assert!(render(&caption("Hi\nthere"), 1.0).unwrap().height > img.height * 2 - 4);
        assert!(render(&caption("  "), 1.0).is_none());
        assert!(render(&Caption { font: CaptionFont::Sans, ..caption("a<b & \"c\"") }, 1.0).is_some(), "markup is escaped");
    }

    #[test]
    fn layer_places_captions_at_their_scaled_centre() {
        let captions = Captions { items: vec![caption("X")], of_width: 100, of_height: 100 };
        let layer = layer(&captions, 200, 200);
        let alpha = |x: usize, y: usize| layer[(y * 200 + x) * 4 + 3];
        assert!(alpha(100, 100) > 0, "centred on (50, 50) × 2");
        assert_eq!(alpha(10, 10), 0);
        assert_eq!(alpha(190, 190), 0);
    }
}
