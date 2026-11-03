//! Writing the PDF: the laid-out pages through `krilla`, or pictures one to a page.

use crate::fonts::{FontId, pdf_fonts, pdf_index};
use crate::layout::{Color, Draw, Layout, Picture, PictureData};
use dotty_core::{Error, Result};
use krilla::annotation::{Annotation, LinkAnnotation, Target};
use krilla::action::{Action, LinkAction};
use krilla::color::rgb;
use krilla::destination::XyzDestination;
use krilla::geom::{PathBuilder, Point, Rect, Size, Transform};
use krilla::metadata::Metadata;
use krilla::num::NormalizedF32;
use krilla::outline::{Outline, OutlineNode};
use krilla::page::PageSettings;
use krilla::paint::{Fill, FillRule, Stroke};
use krilla::text::TextDirection;
use krilla::{Data, Document, image::Image};

fn pdf_err(what: &str, e: impl std::fmt::Debug) -> Error {
    Error::Codec(format!("PDF {what}: {e:?}"))
}

fn paint(c: Color) -> krilla::paint::Paint {
    rgb::Color::new(c[0], c[1], c[2]).into()
}

fn fill(c: Color) -> Fill {
    Fill { paint: paint(c), opacity: NormalizedF32::ONE, rule: FillRule::default() }
}

pub fn image(p: &Picture) -> Result<Image> {
    match &p.data {
        PictureData::Jpeg(bytes) => Image::from_jpeg(Data::from(bytes.clone()), true).map_err(|e| pdf_err("picture", e)),
        PictureData::Rgba(rgba) => Ok(Image::from_rgba8(rgba.clone(), p.width, p.height)),
    }
}

fn metadata(title: Option<&str>) -> Metadata {
    let m = Metadata::new().creator("Dotty".to_string());
    match title {
        Some(t) if !t.trim().is_empty() => m.title(t.trim().to_string()),
        _ => m,
    }
}

/// The laid-out document as a PDF; `numbers` adds "page / pages" at the foot of each page.
pub fn render(layout: &Layout, page: (f32, f32), margin: f32, numbers: bool, title: Option<&str>) -> Result<Vec<u8>> {
    let fonts = pdf_fonts();
    let pictures: Vec<Image> = layout.pictures.iter().map(image).collect::<Result<_>>()?;
    let mut doc = Document::new();
    let total = layout.pages.len();
    for (n, p) in layout.pages.iter().enumerate() {
        let size = Size::from_wh(page.0, page.1).ok_or_else(|| Error::Codec("bad page size".into()))?;
        let mut pg = doc.start_page_with(PageSettings::new(size));
        let mut s = pg.surface();
        let mut links: Vec<(Rect, String)> = Vec::new();
        for item in &p.items {
            match item {
                Draw::Text { x, y, font, size, text, color } => {
                    s.set_fill(Some(fill(*color)));
                    s.set_stroke(None);
                    s.draw_text(Point::from_xy(*x, *y), fonts[pdf_index(*font)].clone(), *size, text, false, TextDirection::Auto);
                }
                Draw::Rect { x, y, w, h, fill: f, stroke } => {
                    let Some(rect) = Rect::from_xywh(*x, *y, *w, *h) else { continue };
                    let mut pb = PathBuilder::new();
                    pb.push_rect(rect);
                    let Some(path) = pb.finish() else { continue };
                    s.set_fill(f.map(fill));
                    s.set_stroke(stroke.map(|(c, w)| Stroke { paint: paint(c), width: w, ..Default::default() }));
                    s.draw_path(&path);
                }
                Draw::Line { x1, y1, x2, y2, color, width } => {
                    let mut pb = PathBuilder::new();
                    pb.move_to(*x1, *y1);
                    pb.line_to(*x2, *y2);
                    let Some(path) = pb.finish() else { continue };
                    s.set_fill(None);
                    s.set_stroke(Some(Stroke { paint: paint(*color), width: *width, ..Default::default() }));
                    s.draw_path(&path);
                }
                Draw::Image { x, y, w, h, image } => {
                    let Some(size) = Size::from_wh(*w, *h) else { continue };
                    s.push_transform(&Transform::from_translate(*x, *y));
                    s.draw_image(pictures[*image].clone(), size);
                    s.pop();
                }
                Draw::Link { x, y, w, h, url } => {
                    if let Some(r) = Rect::from_xywh(*x, *y, *w, *h) {
                        links.push((r, url.clone()));
                    }
                }
            }
        }
        if numbers && total > 1 {
            let label = format!("{} / {total}", n + 1);
            let w = crate::fonts::Fonts::new().width(FontId::Sans, &label, 9.0);
            s.set_fill(Some(fill([120, 120, 120])));
            s.set_stroke(None);
            s.draw_text(Point::from_xy((page.0 - w) / 2.0, page.1 - margin / 2.0 + 3.0), fonts[pdf_index(FontId::Sans)].clone(), 9.0, &label, false, TextDirection::Auto);
        }
        s.finish();
        for (rect, url) in links {
            pg.add_annotation(Annotation::new_link(LinkAnnotation::new(rect, Target::Action(Action::Link(LinkAction::new(url)))), None));
        }
        pg.finish();
    }
    // Bookmarks: headings nested by level.
    let mut outline = Outline::new();
    let mut stack: Vec<(u8, OutlineNode)> = Vec::new();
    let close = |stack: &mut Vec<(u8, OutlineNode)>, outline: &mut Outline, down_to: u8| {
        while let Some((lvl, node)) = stack.pop_if(|(l, _)| *l >= down_to) {
            let _ = lvl;
            match stack.last_mut() {
                Some((_, parent)) => parent.push_child(node),
                None => outline.push_child(node),
            }
        }
    };
    for (level, text, page_no, y) in &layout.outline {
        close(&mut stack, &mut outline, *level);
        let dest = XyzDestination::new(*page_no, Point::from_xy(0.0, (*y - 6.0).max(0.0)));
        stack.push((*level, OutlineNode::new(text.clone(), dest)));
    }
    close(&mut stack, &mut outline, 0);
    if !layout.outline.is_empty() {
        doc.set_outline(outline);
    }
    doc.set_metadata(metadata(title));
    doc.finish().map_err(|e| pdf_err("writing", e))
}

/// A page holding one picture.
pub struct PicturePage {
    /// Page width and height in points.
    pub size: (f32, f32),
    /// Where the picture goes: x, y, width, height.
    pub at: (f32, f32, f32, f32),
    pub picture: Picture,
}

/// One picture per page.
pub fn pictures(pages: &[PicturePage]) -> Result<Vec<u8>> {
    let mut doc = Document::new();
    for PicturePage { size, at: (x, y, w, h), picture: pic } in pages {
        let img = image(pic)?;
        let size = Size::from_wh(size.0, size.1).ok_or_else(|| Error::Codec("bad page size".into()))?;
        let mut pg = doc.start_page_with(PageSettings::new(size));
        let mut s = pg.surface();
        if let Some(sz) = Size::from_wh(*w, *h) {
            s.push_transform(&Transform::from_translate(*x, *y));
            s.draw_image(img, sz);
            s.pop();
        }
        s.finish();
        pg.finish();
    }
    doc.set_metadata(metadata(None));
    doc.finish().map_err(|e| pdf_err("writing", e))
}
