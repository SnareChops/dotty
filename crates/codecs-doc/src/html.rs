//! HTML in, to the document model: the structure (headings, paragraphs, lists, tables, code,
//! links, pictures) and the basics of emphasis. Style sheets, scripts and layout are ignored.

use crate::model::*;
use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, QualName, local_name, ns, parse_document, parse_fragment as parse_frag};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

pub fn parse_document_str(text: &str) -> Doc {
    let dom = parse_document(RcDom::default(), ParseOpts::default()).from_utf8().one(text.trim_start_matches('\u{feff}').as_bytes());
    build(&dom.document)
}

pub fn parse_fragment(text: &str) -> Doc {
    let dom = parse_frag(RcDom::default(), ParseOpts::default(), QualName::new(None, ns!(html), local_name!("body")), vec![], false).from_utf8().one(text.as_bytes());
    build(&dom.document)
}

fn build(root: &Handle) -> Doc {
    let mut doc = Doc::default();
    let mut cur = Vec::new();
    let mut ctx = Ctx { dropped: &mut doc.dropped, title: None };
    let mut blocks = Vec::new();
    children(root, &Style::default(), &mut blocks, &mut cur, &mut ctx);
    flush(&mut cur, &mut blocks);
    doc.title = ctx.title.take().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    doc.blocks = blocks;
    doc
}

struct Ctx<'a> {
    dropped: &'a mut Dropped,
    title: Option<String>,
}

fn flush(cur: &mut Vec<Inline>, out: &mut Vec<Block>) {
    let inl = tidy(std::mem::take(cur));
    if !inl.is_empty() {
        out.push(Block::Para(inl));
    }
}

fn attr(h: &Handle, name: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &h.data {
        return attrs.borrow().iter().find(|a| &*a.name.local == name).map(|a| a.value.to_string());
    }
    None
}

/// The few CSS properties that change what is shown: bold, italic, underline, strike, hidden.
fn apply_style(h: &Handle, mut s: Style) -> (Style, bool) {
    let mut hidden = attr(h, "hidden").is_some();
    if let Some(css) = attr(h, "style") {
        for decl in css.split(';') {
            let Some((k, v)) = decl.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
            match k.as_str() {
                "display" if v == "none" => hidden = true,
                "visibility" if v == "hidden" => hidden = true,
                "font-weight" => s.bold = v == "bold" || v == "bolder" || v.parse::<u32>().is_ok_and(|w| w >= 600),
                "font-style" => s.italic = v == "italic" || v == "oblique",
                "text-decoration" | "text-decoration-line" => {
                    s.underline |= v.contains("underline");
                    s.strike |= v.contains("line-through");
                }
                _ => {}
            }
        }
    }
    (s, hidden)
}

fn text_of(h: &Handle, out: &mut String) {
    match &h.data {
        NodeData::Text { contents } => out.push_str(&contents.borrow()),
        NodeData::Element { name, .. } if &*name.local == "br" => out.push('\n'),
        _ => {
            for c in h.children.borrow().iter() {
                text_of(c, out);
            }
        }
    }
}

fn children(h: &Handle, style: &Style, out: &mut Vec<Block>, cur: &mut Vec<Inline>, ctx: &mut Ctx) {
    for c in h.children.borrow().iter() {
        node(c, style, out, cur, ctx);
    }
}

/// Everything inside `h` as its own list of blocks.
fn sub(h: &Handle, style: &Style, ctx: &mut Ctx) -> Vec<Block> {
    let (mut out, mut cur) = (Vec::new(), Vec::new());
    children(h, style, &mut out, &mut cur, ctx);
    flush(&mut cur, &mut out);
    out
}

/// A table cell or heading: its content as one run of inlines (blocks are joined by breaks).
fn inline_content(h: &Handle, style: &Style, ctx: &mut Ctx) -> Vec<Inline> {
    let blocks = sub(h, style, ctx);
    let mut out = Vec::new();
    for b in blocks {
        if !out.is_empty() {
            out.push(Inline::Break);
        }
        match b {
            Block::Para(i) | Block::Heading(_, i) => out.extend(i),
            Block::Code(c) => out.push(Inline::Text(c, Style { code: true, ..style.clone() })),
            Block::Quote(_) | Block::List { .. } | Block::Table(_) | Block::Rule => {}
        }
    }
    tidy(out)
}

fn node(h: &Handle, style: &Style, out: &mut Vec<Block>, cur: &mut Vec<Inline>, ctx: &mut Ctx) {
    match &h.data {
        NodeData::Text { contents } => {
            let raw = contents.borrow();
            let mut text = String::with_capacity(raw.len());
            let mut space = false;
            for c in raw.chars() {
                if c.is_ascii_whitespace() {
                    if !space {
                        text.push(' ');
                    }
                    space = true;
                } else {
                    text.push(c);
                    space = false;
                }
            }
            if !text.is_empty() {
                cur.push(Inline::Text(text, style.clone()));
            }
        }
        NodeData::Document => children(h, style, out, cur, ctx),
        NodeData::Element { name, .. } => element(&name.local, h, style, out, cur, ctx),
        _ => {}
    }
}

fn element(name: &str, h: &Handle, style: &Style, out: &mut Vec<Block>, cur: &mut Vec<Inline>, ctx: &mut Ctx) {
    let (style, hidden) = apply_style(h, style.clone());
    if hidden {
        return;
    }
    match name {
        "title" => {
            let mut t = String::new();
            text_of(h, &mut t);
            ctx.title.get_or_insert(t);
        }
        "head" => {
            // Only the title matters.
            for c in h.children.borrow().iter() {
                if matches!(&c.data, NodeData::Element { name, .. } if &*name.local == "title") {
                    node(c, &style, out, cur, ctx);
                }
            }
        }
        "script" | "style" | "noscript" | "template" | "svg" | "iframe" | "object" | "embed" | "canvas" | "select" | "button" | "input" | "textarea" | "meta" | "link" | "audio" | "video" => {
            if matches!(name, "svg" | "iframe" | "object" | "embed" | "canvas" | "audio" | "video") {
                let msg = format!("<{name}> elements aren't supported and were left out");
                if !ctx.dropped.other.contains(&msg) {
                    ctx.dropped.other.push(msg);
                }
            }
        }
        "html" | "body" | "div" | "section" | "article" | "main" | "header" | "footer" | "nav" | "aside" | "figure" | "center" | "address" | "details" | "summary" | "form" | "fieldset" | "hgroup" | "p" | "dl" | "dd" | "figcaption" | "li" | "caption" => {
            flush(cur, out);
            children(h, &style, out, cur, ctx);
            flush(cur, out);
        }
        "dt" => {
            flush(cur, out);
            children(h, &Style { bold: true, ..style }, out, cur, ctx);
            flush(cur, out);
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            flush(cur, out);
            let level = name.as_bytes()[1] - b'0';
            let inl = inline_content(h, &style, ctx);
            if !inl.is_empty() {
                out.push(Block::Heading(level, inl));
            }
        }
        "blockquote" => {
            flush(cur, out);
            let inner = sub(h, &style, ctx);
            out.push(Block::Quote(inner));
        }
        "ul" | "ol" => {
            flush(cur, out);
            let start = (name == "ol").then(|| attr(h, "start").and_then(|s| s.trim().parse().ok()).unwrap_or(1));
            let mut items = Vec::new();
            for c in h.children.borrow().iter() {
                if matches!(&c.data, NodeData::Element { name, .. } if &*name.local == "li") {
                    let (ls, hidden) = apply_style(c, style.clone());
                    if !hidden {
                        items.push(Item { task: None, blocks: sub(c, &ls, ctx) });
                    }
                }
            }
            out.push(Block::List { start, items });
        }
        "pre" => {
            flush(cur, out);
            let mut code = String::new();
            text_of(h, &mut code);
            out.push(Block::Code(code.trim_matches('\n').trim_end().to_string()));
        }
        "hr" => {
            flush(cur, out);
            out.push(Block::Rule);
        }
        "br" => cur.push(Inline::Break),
        "img" => {
            if let Some(src) = attr(h, "src") {
                cur.push(Inline::Image { src, alt: attr(h, "alt").unwrap_or_default() });
            }
        }
        "table" => {
            flush(cur, out);
            out.push(Block::Table(table(h, &style, ctx)));
        }
        "a" => {
            let href = attr(h, "href").filter(|u| !u.starts_with('#') && !u.to_ascii_lowercase().starts_with("javascript:"));
            children(h, &Style { link: href.or(style.link.clone()), ..style }, out, cur, ctx);
        }
        "strong" | "b" => children(h, &Style { bold: true, ..style }, out, cur, ctx),
        "em" | "i" | "cite" | "dfn" | "var" => children(h, &Style { italic: true, ..style }, out, cur, ctx),
        "code" | "kbd" | "samp" | "tt" => children(h, &Style { code: true, ..style }, out, cur, ctx),
        "del" | "s" | "strike" => children(h, &Style { strike: true, ..style }, out, cur, ctx),
        "u" | "ins" => children(h, &Style { underline: true, ..style }, out, cur, ctx),
        _ => children(h, &style, out, cur, ctx),
    }
}

/// A table row as read: whether it is a header, its cells, and their alignments.
type Row = (bool, Vec<Vec<Inline>>, Vec<Align>);

fn table(h: &Handle, style: &Style, ctx: &mut Ctx) -> Table {
    let mut rows: Vec<Row> = Vec::new();
    fn collect(h: &Handle, style: &Style, ctx: &mut Ctx, rows: &mut Vec<Row>, in_head: bool) {
        for c in h.children.borrow().iter() {
            let NodeData::Element { name, .. } = &c.data else { continue };
            match &*name.local {
                "thead" => collect(c, style, ctx, rows, true),
                "tbody" | "tfoot" => collect(c, style, ctx, rows, false),
                "tr" => {
                    let (mut cells, mut aligns, mut all_th) = (Vec::new(), Vec::new(), true);
                    for cell in c.children.borrow().iter() {
                        let NodeData::Element { name, .. } = &cell.data else { continue };
                        let tag = &*name.local;
                        if tag != "td" && tag != "th" {
                            continue;
                        }
                        all_th &= tag == "th";
                        let (cs, hidden) = apply_style(cell, style.clone());
                        if hidden {
                            continue;
                        }
                        let align = match attr(cell, "align").unwrap_or_default().to_ascii_lowercase().as_str() {
                            "center" => Align::Center,
                            "right" => Align::Right,
                            _ => Align::Left,
                        };
                        cells.push(inline_content(cell, &cs, ctx));
                        aligns.push(align);
                        for _ in 1..attr(cell, "colspan").and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1).min(50) {
                            cells.push(Vec::new());
                            aligns.push(align);
                        }
                    }
                    if !cells.is_empty() {
                        rows.push((in_head || all_th, cells, aligns));
                    }
                }
                _ => {}
            }
        }
    }
    collect(h, style, ctx, &mut rows, false);
    let mut t = Table::default();
    if rows.first().is_some_and(|r| r.0) {
        let (_, head, aligns) = rows.remove(0);
        t.head = head;
        t.aligns = aligns;
    } else if let Some(first) = rows.first() {
        t.aligns = first.2.clone();
    }
    t.rows = rows.into_iter().map(|r| r.1).collect();
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html><html><head><title> My page </title><style>p{color:red}</style><script>alert(1)</script></head>
<body><h1>Hello <em>world</em></h1>
<p>Some <b>bold</b>, <i>italic</i>, <code>code</code>, <del>old</del> and a <a href="https://example.com">link</a>.<br>Next   line
with  wrapping.</p>
<div style="display:none">hidden</div>
<p style="font-weight: bold">Styled</p>
<ul><li>one</li><li>two<ul><li>inner</li></ul></li></ul>
<ol start="3"><li>x</li></ol>
<blockquote><p>quoted</p></blockquote>
<pre><code>line 1
  line 2</code></pre>
<hr>
<table><thead><tr><th>A</th><th align="right">B</th></tr></thead><tbody><tr><td>1</td><td colspan="2">2</td></tr></tbody></table>
<img src="pic.png" alt="a picture">
<svg><circle/></svg>
</body></html>"#;

    #[test]
    fn a_web_page_is_read_into_blocks() {
        let d = parse_document_str(PAGE);
        assert_eq!(d.title.as_deref(), Some("My page"));
        assert_eq!(d.display_title().as_deref(), Some("Hello world"));
        let kinds: Vec<&str> = d
            .blocks
            .iter()
            .map(|b| match b {
                Block::Heading(..) => "h",
                Block::Para(_) => "p",
                Block::Quote(_) => "q",
                Block::List { .. } => "l",
                Block::Code(_) => "c",
                Block::Rule => "r",
                Block::Table(_) => "t",
            })
            .collect();
        // The picture sits in its own paragraph; script, style, hidden and svg are gone.
        assert_eq!(kinds, ["h", "p", "p", "l", "l", "q", "c", "r", "t", "p"], "{:?}", d.blocks);
        let Block::Para(p) = &d.blocks[1] else { panic!() };
        assert_eq!(plain(p), "Some bold, italic, code, old and a link. Next line with wrapping.");
        assert!(p.iter().any(|i| matches!(i, Inline::Text(t, s) if t == "bold" && s.bold)));
        assert!(p.iter().any(|i| matches!(i, Inline::Text(t, s) if t == "old" && s.strike)));
        assert!(p.iter().any(|i| matches!(i, Inline::Text(t, s) if t == "link" && s.link.as_deref() == Some("https://example.com"))));
        assert!(p.contains(&Inline::Break));
        assert!(matches!(&d.blocks[2], Block::Para(i) if matches!(&i[0], Inline::Text(_, s) if s.bold)), "inline style");
        let Block::List { items, .. } = &d.blocks[3] else { panic!() };
        assert!(items[1].blocks.iter().any(|b| matches!(b, Block::List { .. })));
        assert!(matches!(&d.blocks[4], Block::List { start: Some(3), .. }));
        assert_eq!(d.blocks[6], Block::Code("line 1\n  line 2".into()));
        let Block::Table(t) = &d.blocks[8] else { panic!() };
        assert_eq!((t.head.len(), t.rows.len(), t.rows[0].len(), t.aligns[1]), (2, 1, 3, Align::Right), "colspan adds a cell");
        assert!(matches!(&d.blocks[9], Block::Para(i) if matches!(&i[0], Inline::Image { src, alt } if src == "pic.png" && alt == "a picture")));
        assert!(d.dropped.other.iter().any(|m| m.contains("svg")));
    }

    #[test]
    fn a_fragment_has_no_page_around_it() {
        let d = parse_fragment("<p>one</p><p>two <b>x</b></p>");
        assert_eq!(d.blocks.len(), 2);
    }
}
