//! Reading PDFs (through `hayro`): page pictures, and the text on each page.

use dotty_core::{Error, Result};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::font::GlyphRun;
use hayro::hayro_interpret::{BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings, SoftMask, TransformExt, interpret_page};
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use image::RgbaImage;
use std::path::Path;
use std::sync::Arc;

pub fn open(path: &Path) -> Result<Pdf> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io { path: path.into(), source })?;
    Pdf::new(Arc::new(bytes)).map_err(|e| match e {
        hayro::hayro_interpret::hayro_syntax::LoadPdfError::Decryption(_) => Error::Codec("this PDF is password protected, which isn't supported".into()),
        _ => Error::Codec("this isn't a PDF Dotty can read".into()),
    })
}

pub fn page_count(path: &Path) -> Result<usize> {
    Ok(open(path)?.pages().len())
}

/// Most pixels a rendered page may have; bigger pages are rendered smaller.
const MAX_PIXELS: f64 = 120e6;

/// Render page `index` at `dpi` on white.
pub fn render<'a>(pdf: &'a Pdf, index: usize, dpi: f32, cache: &RenderCache<'a>) -> Result<RgbaImage> {
    let page = pdf.pages().get(index).ok_or_else(|| Error::Codec(format!("the PDF has no page {}", index + 1)))?;
    let (w, h) = page.render_dimensions();
    let mut scale = dpi / 72.0;
    let pixels = (w * scale) as f64 * (h * scale) as f64;
    if pixels > MAX_PIXELS {
        scale *= (MAX_PIXELS / pixels).sqrt() as f32;
    }
    scale = scale.min(65_000.0 / w.max(h).max(1.0));
    let pixmap = hayro::render(
        page,
        cache,
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: hayro::vello_cpu::color::palette::css::WHITE },
    );
    let (pw, ph) = (pixmap.width() as u32, pixmap.height() as u32);
    let data: Vec<u8> = pixmap.data_as_u8_slice().to_vec();
    RgbaImage::from_raw(pw, ph, data).ok_or_else(|| Error::Codec("couldn't read the rendered page".into()))
}

// ---- text ----

#[derive(Debug, Clone)]
struct Glyph {
    x: f64,
    y: f64,
    /// Length of one glyph unit across and up the page, in page points.
    ux: f64,
    /// How far the font says the pen moves after this glyph, in page points.
    adv: Option<f64>,
    text: String,
}

#[derive(Default)]
struct TextDevice {
    glyphs: Vec<Glyph>,
}

impl<'a> Device<'a> for TextDevice {
    fn draw_path(&mut self, _: &hayro::kurbo::BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for g in run.glyphs() {
            let t = props.transform * g.transform();
            let o = t * hayro::kurbo::Point::new(0.0, 0.0);
            let u = t * hayro::kurbo::Point::new(1.0, 0.0);
            let text = match g.as_unicode() {
                Some(BfString::Char(c)) => c.to_string(),
                Some(BfString::String(s)) => s,
                None => continue,
            };
            let ux = ((u.x - o.x).powi(2) + (u.y - o.y).powi(2)).sqrt();
            let adv = match &**g {
                hayro::hayro_interpret::font::Glyph::Outline(og) => og.advance_width().map(|w| w as f64 * ux),
                _ => None,
            };
            self.glyphs.push(Glyph { x: o.x, y: o.y, ux, adv, text });
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn page_glyphs(page: &Page<'_>, cache: &RenderCache<'_>) -> Vec<Glyph> {
    let _ = cache;
    let (w, h) = page.render_dimensions();
    let interp_cache = hayro::hayro_interpret::InterpreterCache::new();
    let mut ctx = Context::new(page.initial_transform(true).to_kurbo(), hayro::kurbo::Rect::new(0.0, 0.0, w as f64, h as f64), &interp_cache, page.xref(), InterpreterSettings::default());
    let mut dev = TextDevice::default();
    interpret_page(page, &mut ctx, &mut dev);
    dev.glyphs
}

/// Plain text of every page; `reflow` joins the lines of a paragraph, else the PDF's own
/// line breaks stay. Pages without any text are counted in the second value.
pub fn text(pdf: &Pdf, reflow: bool, mut progress: impl FnMut(usize, usize)) -> (String, usize) {
    let cache = RenderCache::new();
    let pages = pdf.pages();
    let mut out = String::new();
    let mut empty = 0;
    for (i, page) in pages.iter().enumerate() {
        let glyphs = page_glyphs(page, &cache);
        if glyphs.is_empty() {
            empty += 1;
        }
        let t = layout_text(&glyphs, reflow);
        if !t.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&t);
        }
        progress(i + 1, pages.len());
    }
    (out, empty)
}

/// Lines from glyph positions: top to bottom, left to right, with spaces where the gaps say so.
fn layout_text(glyphs: &[Glyph], reflow: bool) -> String {
    use crate::fonts::{FontId, Fonts};
    let mut fonts = Fonts::new();
    let mut gs: Vec<&Glyph> = glyphs.iter().filter(|g| !g.text.is_empty()).collect();
    gs.sort_by(|a, b| a.y.total_cmp(&b.y));
    // Rows: glyphs whose baselines are within half a line of each other.
    let mut rows: Vec<Vec<&Glyph>> = Vec::new();
    for g in gs {
        match rows.last_mut() {
            Some(row) if (g.y - row[0].y).abs() <= 0.5 * (row[0].ux * 1000.0).max(g.ux * 1000.0).max(1.0) => row.push(g),
            _ => rows.push(vec![g]),
        }
    }
    struct Line {
        text: String,
        y: f64,
        size: f64,
    }
    let mut lines: Vec<Line> = Vec::new();
    for mut row in rows {
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        let size = row.iter().map(|g| g.ux * 1000.0).fold(0.0, f64::max).max(1.0);
        let adv: Vec<f64> = row.iter().map(|g| g.text.chars().next().map_or(0.0, |c| fonts.width(FontId::Sans, &c.to_string(), 1.0) as f64) * g.ux * 1000.0).collect();
        // Monospace text: every distance between neighbours is a whole number of one advance
        // (letters one, spaces two). Proportional text has no such unit.
        let em: Vec<f64> = (1..row.len()).map(|i| (row[i].x - row[i - 1].x) / (row[i - 1].ux * 1000.0).max(0.001)).collect();
        let mono = {
            let mut buckets: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
            for e in &em {
                *buckets.entry((e * 100.0).round() as i64).or_default() += 1;
            }
            let frequent = (em.len() / 5).max(2);
            let unit = buckets.iter().find(|(e, n)| **e > 25 && **n >= frequent).map(|(e, _)| *e as f64 / 100.0);
            unit.filter(|u| em.len() >= 4 && em.iter().filter(|e| (**e / u - (**e / u).round()).abs() < 0.08 && **e / u > 0.9).count() * 10 >= em.len() * 9)
        };
        // Proportional text: how much wider or narrower than our font this font's letters are.
        let mut ratios: Vec<f64> = (1..row.len())
            .filter(|&i| !row[i - 1].text.trim().is_empty() && adv[i - 1] > 0.0)
            .map(|i| (row[i].x - row[i - 1].x) / adv[i - 1])
            .filter(|r| (0.5..2.0).contains(r))
            .collect();
        ratios.sort_by(f64::total_cmp);
        let k = ratios.get(ratios.len() / 2).copied().unwrap_or(1.0).clamp(0.7, 1.3);
        let mut text = String::new();
        for (i, g) in row.iter().enumerate() {
            if i > 0 {
                let prev = row[i - 1];
                let d = g.x - prev.x;
                let psize = prev.ux * 1000.0;
                let ends_space = text.ends_with(' ');
                let space = match (prev.adv, mono) {
                    (Some(a), _) => d - a > 0.15 * psize,
                    (None, Some(m)) => d > 1.5 * m * psize,
                    (None, None) => d - adv[i - 1] * k > 0.17 * psize,
                };
                if d > 3.0 * size.max(psize) + if mono.is_some() { 2.0 * psize } else { 0.0 } {
                    if !ends_space {
                        text.push_str("    ");
                    }
                } else if space && !ends_space && g.text != " " {
                    text.push(' ');
                }
            }
            if g.text == " " && text.ends_with(' ') {
                continue;
            }
            text.push_str(&g.text);
        }
        let text = text.trim_end().to_string();
        if !text.is_empty() {
            lines.push(Line { text, y: row[0].y, size });
        }
    }
    // The usual distance between lines tells where paragraphs break.
    let mut dys: Vec<f64> = lines.windows(2).map(|w| w[1].y - w[0].y).filter(|d| *d > 0.0).collect();
    dys.sort_by(f64::total_cmp);
    let line_step = dys.get(dys.len() / 2).copied().unwrap_or(0.0);
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(&l.text);
            continue;
        }
        let prev = &lines[i - 1];
        let dy = l.y - prev.y;
        let new_paragraph = !reflow || (line_step > 0.0 && dy > 1.45 * line_step) || (l.size - prev.size).abs() > 0.15 * prev.size.max(l.size);
        // List items and rows of columns each stay on a line of their own.
        let own_line = starts_item(&l.text) || l.text.contains("    ") || prev.text.contains("    ");
        if reflow && own_line && !new_paragraph {
            // A list gets a blank line before it; its items follow one another.
            let list_starts = starts_item(&l.text) && !starts_item(&prev.text);
            out.push_str(if list_starts { "\n\n" } else { "\n" });
            out.push_str(&l.text);
        } else if new_paragraph {
            out.push_str(if reflow { "\n\n" } else { "\n" });
            if !reflow && line_step > 0.0 && dy > 1.45 * line_step {
                out.push('\n');
            }
            out.push_str(&l.text);
        } else if out.ends_with('-') && out[..out.len() - 1].chars().last().is_some_and(char::is_alphabetic) && l.text.chars().next().is_some_and(char::is_lowercase) {
            out.pop();
            out.push_str(&l.text);
        } else {
            out.push(' ');
            out.push_str(&l.text);
        }
    }
    out
}

/// Does the line start like a list item (a bullet, a dash, "3." or "[x]")?
fn starts_item(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    if line.len() == first.len() {
        return false;
    }
    matches!(first, "\u{2022}" | "\u{2013}" | "\u{2014}" | "-" | "*" | "\u{25aa}" | "\u{25e6}" | "[x]" | "[ ]")
        || (first.len() <= 4 && first.len() > 1 && (first.ends_with('.') || first.ends_with(')')) && first[..first.len() - 1].chars().all(|c| c.is_ascii_digit()))
}

/// Debug: the glyph positions of page `index`.
pub fn debug_glyphs(pdf: &Pdf, index: usize) -> Vec<(f64, f64, f64, String)> {
    let cache = RenderCache::new();
    page_glyphs(&pdf.pages()[index], &cache).into_iter().map(|g| (g.x, g.y, g.ux, g.text)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Glyphs for `text` starting at (x, y): proportional (our font's widths, so `scale` 1.0 is
    /// "the same as ours") or monospace (`mono`: advance in em). Spaces leave a gap, no glyph.
    fn line(text: &str, x: f64, y: f64, size: f64, scale: f64, mono: Option<f64>) -> Vec<Glyph> {
        line_with(text, x, y, size, scale, mono, false)
    }

    /// As [`line`]; `known` also gives each glyph its true advance, like a real font does.
    fn line_with(text: &str, x: f64, y: f64, size: f64, scale: f64, mono: Option<f64>, known: bool) -> Vec<Glyph> {
        let mut fonts = crate::fonts::Fonts::new();
        let (mut out, mut cx) = (Vec::new(), x);
        for c in text.chars() {
            let adv = mono.unwrap_or_else(|| fonts.width(crate::fonts::FontId::Sans, &c.to_string(), 1.0) as f64 * scale);
            if c != ' ' {
                out.push(Glyph { x: cx, y, ux: size / 1000.0, adv: known.then_some(adv * size), text: c.to_string() });
            }
            cx += adv * size;
        }
        out
    }

    #[test]
    fn words_lines_paragraphs_and_hyphens_come_back() {
        let mut g = Vec::new();
        // Spaces are only gaps (as in TeX output), proportionally spaced.
        g.extend(line("Hello brave world", 50.0, 100.0, 12.0, 1.0, None));
        g.extend(line("and an inter-", 50.0, 114.0, 12.0, 1.0, None));
        g.extend(line("esting text", 50.0, 128.0, 12.0, 1.0, None));
        g.extend(line("Second paragraph", 50.0, 160.0, 12.0, 1.0, None));
        let reflowed = layout_text(&g, true);
        assert_eq!(reflowed, "Hello brave world and an interesting text\n\nSecond paragraph");
        let kept = layout_text(&g, false);
        assert_eq!(kept, "Hello brave world\nand an inter-\nesting text\n\nSecond paragraph");
    }

    #[test]
    fn exact_advances_find_every_space_whatever_the_font() {
        let mut g = line_with("inline code, and wide mmm words", 40.0, 90.0, 11.0, 1.0, None, true);
        g.extend(line_with("x", 400.0, 90.0, 11.0, 1.0, Some(0.6), true));
        assert_eq!(layout_text(&g, true), "inline code, and wide mmm words    x");
        // Letters in a monospace font are wide: exactly the case guessing gets wrong.
        let m = line_with("fill il lit", 40.0, 120.0, 11.0, 1.0, Some(0.6), true);
        assert_eq!(layout_text(&m, true), "fill il lit");
    }

    #[test]
    fn a_font_narrower_than_ours_still_gets_its_spaces() {
        let g = line("the quick brown fox jumps over", 40.0, 90.0, 11.0, 0.85, None);
        assert_eq!(layout_text(&g, true), "the quick brown fox jumps over");
    }

    #[test]
    fn monospace_columns_and_headings_are_told_apart() {
        let mut g = Vec::new();
        g.extend(line("Title here", 50.0, 60.0, 24.0, 1.0, None));
        g.extend(line("let x = 1;", 50.0, 100.0, 10.0, 1.0, Some(0.6)));
        // Two columns on one row are kept apart, not run together.
        g.extend(line("left", 50.0, 160.0, 10.0, 1.0, None));
        g.extend(line("right", 300.0, 160.0, 10.0, 1.0, None));
        let t = layout_text(&g, true);
        assert_eq!(t, "Title here\n\nlet x = 1;\nleft    right", "a heading by its size; two columns on one row stay apart on their own line");
    }
}
