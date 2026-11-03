//! Markdown in (to the document model, or straight to HTML) and out (from the model).

use crate::model::*;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::iter::Peekable;

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

pub fn parse(text: &str) -> Doc {
    let text = text.trim_start_matches('\u{feff}');
    let mut doc = Doc::default();
    let mut it = Parser::new_ext(text, options()).peekable();
    doc.blocks = blocks(&mut it, &mut doc.dropped);
    doc
}

type Events<'a> = Peekable<Parser<'a>>;

fn is_inline(e: &Event) -> bool {
    matches!(
        e,
        Event::Text(_) | Event::Code(_) | Event::SoftBreak | Event::HardBreak | Event::InlineHtml(_) | Event::TaskListMarker(_) | Event::FootnoteReference(_)
            | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. })
    )
}

fn blocks(it: &mut Events, dropped: &mut Dropped) -> Vec<Block> {
    let mut out = Vec::new();
    while let Some(e) = it.peek() {
        if matches!(e, Event::End(_)) {
            break;
        }
        if is_inline(e) {
            let inl = tidy(inlines(it, &Style::default()));
            if !inl.is_empty() {
                out.push(Block::Para(inl));
            }
            continue;
        }
        match it.next().unwrap() {
            Event::Start(Tag::Paragraph) => {
                let inl = tidy(inlines(it, &Style::default()));
                end(it);
                if !inl.is_empty() {
                    out.push(Block::Para(inl));
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let inl = tidy(inlines(it, &Style::default()));
                end(it);
                out.push(Block::Heading(heading(level), inl));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let inner = blocks(it, dropped);
                end(it);
                out.push(Block::Quote(inner));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let _ = matches!(kind, CodeBlockKind::Fenced(_));
                let mut code = String::new();
                for e in it.by_ref() {
                    match e {
                        Event::Text(t) => code.push_str(&t),
                        Event::End(_) => break,
                        _ => {}
                    }
                }
                out.push(Block::Code(code.trim_end_matches('\n').to_string()));
            }
            Event::Start(Tag::List(start)) => {
                let mut items = Vec::new();
                while let Some(Event::Start(Tag::Item)) = it.peek() {
                    it.next();
                    let task = if let Some(Event::TaskListMarker(done)) = it.peek() {
                        let d = *done;
                        it.next();
                        Some(d)
                    } else {
                        None
                    };
                    let inner = blocks(it, dropped);
                    end(it);
                    items.push(Item { task, blocks: inner });
                }
                end(it);
                out.push(Block::List { start, items });
            }
            Event::Start(Tag::Table(aligns)) => out.push(Block::Table(table(it, &aligns))),
            Event::Start(Tag::HtmlBlock) => {
                let mut html = String::new();
                for e in it.by_ref() {
                    match e {
                        Event::Html(t) | Event::Text(t) => html.push_str(&t),
                        Event::End(_) => break,
                        _ => {}
                    }
                }
                let sub = crate::html::parse_fragment(&html);
                dropped.remote_images += sub.dropped.remote_images;
                dropped.other.extend(sub.dropped.other);
                out.extend(sub.blocks);
            }
            Event::Rule => out.push(Block::Rule),
            Event::Start(_) => {
                // Anything we don't model (footnote definitions, metadata, …): read through it.
                let inner = blocks(it, dropped);
                end(it);
                out.extend(inner);
            }
            Event::Html(t) => {
                let sub = crate::html::parse_fragment(&t);
                out.extend(sub.blocks);
            }
            _ => {}
        }
    }
    out
}

fn end(it: &mut Events) {
    if matches!(it.peek(), Some(Event::End(_))) {
        it.next();
    }
}

fn heading(l: HeadingLevel) -> u8 {
    l as u8
}

/// Consume inline events (stopping before a block-level one or an `End`).
fn inlines(it: &mut Events, style: &Style) -> Vec<Inline> {
    let mut out = Vec::new();
    while let Some(e) = it.peek() {
        if !is_inline(e) {
            break;
        }
        match it.next().unwrap() {
            Event::Text(t) => out.push(Inline::Text(t.to_string(), style.clone())),
            Event::Code(t) => out.push(Inline::Text(t.to_string(), Style { code: true, ..style.clone() })),
            Event::SoftBreak => out.push(Inline::Text(" ".into(), style.clone())),
            Event::HardBreak => out.push(Inline::Break),
            Event::InlineHtml(h) => {
                let tag = h.trim().trim_matches(['<', '>', '/']).trim().to_ascii_lowercase();
                if tag == "br" {
                    out.push(Inline::Break);
                }
            }
            Event::FootnoteReference(l) => out.push(Inline::Text(format!("[{l}]"), style.clone())),
            Event::Start(Tag::Emphasis) => {
                out.extend(inlines(it, &Style { italic: true, ..style.clone() }));
                end(it);
            }
            Event::Start(Tag::Strong) => {
                out.extend(inlines(it, &Style { bold: true, ..style.clone() }));
                end(it);
            }
            Event::Start(Tag::Strikethrough) => {
                out.extend(inlines(it, &Style { strike: true, ..style.clone() }));
                end(it);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                out.extend(inlines(it, &Style { link: Some(dest_url.to_string()), ..style.clone() }));
                end(it);
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let alt = crate::model::plain(&inlines(it, style));
                end(it);
                out.push(Inline::Image { src: dest_url.to_string(), alt });
            }
            _ => {}
        }
    }
    out
}

fn table(it: &mut Events, aligns: &[Alignment]) -> Table {
    let mut t = Table { aligns: aligns.iter().map(|a| match a { Alignment::Center => Align::Center, Alignment::Right => Align::Right, _ => Align::Left }).collect(), ..Default::default() };
    let mut row: Vec<Vec<Inline>> = Vec::new();
    let mut in_head = false;
    while let Some(e) = it.next() {
        match e {
            Event::Start(Tag::TableHead) => in_head = true,
            Event::End(TagEnd::TableHead) => {
                in_head = false;
                t.head = std::mem::take(&mut row);
            }
            Event::Start(Tag::TableRow) => row.clear(),
            Event::End(TagEnd::TableRow) => {
                if !in_head {
                    t.rows.push(std::mem::take(&mut row));
                }
            }
            Event::Start(Tag::TableCell) => {
                let cell = tidy(inlines(it, &Style::default()));
                end(it);
                row.push(cell);
            }
            Event::End(TagEnd::Table) => break,
            _ => {}
        }
    }
    t
}

// ---- HTML ----

/// Markdown to a web page (or just the body markup).
pub fn to_html(text: &str, wrap: bool, title: Option<&str>) -> String {
    let text = text.trim_start_matches('\u{feff}');
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, Parser::new_ext(text, options()));
    if !wrap {
        return body;
    }
    let title = title.map(|t| t.to_string()).or_else(|| parse(text).display_title()).unwrap_or_default();
    let title = title.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{title}</title>\n<style>\n{CSS}</style>\n</head>\n<body>\n<main>\n{body}</main>\n</body>\n</html>\n")
}

pub const CSS: &str = "body { margin: 0; background: #fff; color: #1b1b1b; font: 16px/1.6 system-ui, -apple-system, \"Segoe UI\", Roboto, sans-serif; }
main { max-width: 46rem; margin: 2rem auto; padding: 0 1rem; }
h1, h2, h3, h4, h5, h6 { line-height: 1.25; margin: 1.6em 0 .5em; }
h1 { font-size: 2rem; } h2 { font-size: 1.5rem; border-bottom: 1px solid #ddd; padding-bottom: .2em; }
a { color: #0b57d0; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .9em; }
code { background: #f1f1f1; padding: .1em .3em; border-radius: 3px; }
pre { background: #f6f6f6; padding: .8rem 1rem; overflow-x: auto; border-radius: 4px; }
pre code { background: none; padding: 0; }
blockquote { margin: 1em 0; padding: 0 1em; color: #555; border-left: 4px solid #ccc; }
table { border-collapse: collapse; } th, td { border: 1px solid #ccc; padding: .35em .7em; } th { background: #f1f1f1; }
img { max-width: 100%; } hr { border: 0; border-top: 1px solid #ddd; margin: 2em 0; }
@media (prefers-color-scheme: dark) { body { background: #181818; color: #e6e6e6; } a { color: #8ab4f8; } code, pre { background: #262626; } th { background: #262626; } th, td { border-color: #444; } blockquote { color: #aaa; border-color: #555; } h2, hr { border-color: #333; } }
";

// ---- writing Markdown ----

pub fn write(doc: &Doc) -> String {
    let mut out = String::new();
    write_blocks(&doc.blocks, &mut out, "");
    out.trim_end().to_string() + "\n"
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn inline_md(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for i in inlines {
        match i {
            Inline::Break => out.push_str("  \n"),
            Inline::Image { src, alt } => out.push_str(&format!("![{}]({src})", escape(alt))),
            Inline::Text(t, s) => {
                if t.trim().is_empty() {
                    out.push_str(t);
                    continue;
                }
                let lead = &t[..t.len() - t.trim_start().len()];
                let trail = &t[t.trim_end().len()..];
                let core = t.trim();
                let mut text = if s.code { format!("`{core}`") } else { escape(core) };
                if s.bold {
                    text = format!("**{text}**");
                }
                if s.italic {
                    text = format!("*{text}*");
                }
                if s.strike {
                    text = format!("~~{text}~~");
                }
                if let Some(l) = &s.link {
                    text = format!("[{text}]({l})");
                }
                out.push_str(&format!("{lead}{text}{trail}"));
            }
        }
    }
    out
}

fn prefixed(text: &str, first: &str, rest: &str) -> String {
    text.lines().enumerate().map(|(i, l)| format!("{}{l}", if i == 0 { first } else { rest }).trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

fn write_blocks(blocks: &[Block], out: &mut String, _indent: &str) {
    for (n, b) in blocks.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        match b {
            Block::Heading(l, i) => out.push_str(&format!("{} {}\n", "#".repeat(*l as usize), inline_md(i).replace("  \n", " "))),
            Block::Para(i) => out.push_str(&format!("{}\n", inline_md(i))),
            Block::Rule => out.push_str("---\n"),
            Block::Code(c) => {
                let fence = if c.contains("```") { "~~~~" } else { "```" };
                out.push_str(&format!("{fence}\n{c}\n{fence}\n"));
            }
            Block::Quote(inner) => {
                let mut s = String::new();
                write_blocks(inner, &mut s, "");
                for l in s.trim_end().lines() {
                    out.push_str(&format!("{}\n", format!("> {l}").trim_end()));
                }
            }
            Block::List { start, items } => {
                for (k, item) in items.iter().enumerate() {
                    let mut s = String::new();
                    write_blocks(&item.blocks, &mut s, "");
                    let marker = match start {
                        Some(st) => format!("{}. ", st + k as u64),
                        None => "- ".to_string(),
                    };
                    let task = match item.task {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    let pad = " ".repeat(marker.len());
                    out.push_str(&prefixed(s.trim_end(), &format!("{marker}{task}"), &pad));
                    out.push('\n');
                }
            }
            Block::Table(t) => {
                let cols = t.head.len().max(t.rows.iter().map(Vec::len).max().unwrap_or(0)).max(1);
                let cell = |c: Option<&Vec<Inline>>| c.map(|c| inline_md(c).replace('\n', " ")).unwrap_or_default();
                let row = |r: &[Vec<Inline>]| format!("| {} |\n", (0..cols).map(|i| cell(r.get(i))).collect::<Vec<_>>().join(" | "));
                out.push_str(&row(&t.head));
                let sep = |i: usize| match t.aligns.get(i) {
                    Some(Align::Center) => ":---:",
                    Some(Align::Right) => "---:",
                    _ => "---",
                };
                out.push_str(&format!("| {} |\n", (0..cols).map(sep).collect::<Vec<_>>().join(" | ")));
                for r in &t.rows {
                    out.push_str(&row(r));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD: &str = "# Title\n\nSome *emphasis*, **strong**, ~~gone~~, `code` and a [link](https://example.com).  \nNext line.\n\n> quoted\n> text\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n- [x] done\n- [ ] todo\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|:--|--:|\n| 1 | 2 |\n\n---\n\n![alt text](pic.png)\n";

    #[test]
    fn markdown_is_read_into_blocks() {
        let d = parse(MD);
        assert_eq!(d.display_title().as_deref(), Some("Title"));
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
        assert_eq!(kinds, ["h", "p", "q", "l", "l", "l", "c", "t", "r", "p"]);
        let Block::Para(p) = &d.blocks[1] else { panic!() };
        assert!(p.iter().any(|i| matches!(i, Inline::Text(t, s) if t == "emphasis" && s.italic)));
        assert!(p.iter().any(|i| matches!(i, Inline::Text(t, s) if t == "gone" && s.strike)));
        assert!(p.iter().any(|i| matches!(i, Inline::Text(t, s) if t == "link" && s.link.as_deref() == Some("https://example.com"))));
        assert!(p.contains(&Inline::Break));
        let Block::List { items, .. } = &d.blocks[3] else { panic!() };
        assert_eq!(items.len(), 2);
        assert!(items[1].blocks.iter().any(|b| matches!(b, Block::List { .. })), "nested list");
        let Block::List { items, .. } = &d.blocks[5] else { panic!() };
        assert_eq!((items[0].task, items[1].task), (Some(true), Some(false)));
        let Block::Table(t) = &d.blocks[7] else { panic!() };
        assert_eq!((t.head.len(), t.rows.len(), t.aligns.clone()), (2, 1, vec![Align::Left, Align::Right]));
    }

    #[test]
    fn markdown_goes_to_html_with_or_without_the_page() {
        let page = to_html("# Hi & bye\n\ntext", true, None);
        assert!(page.starts_with("<!DOCTYPE html>") && page.contains("<title>Hi &amp; bye</title>") && page.contains("<h1>Hi &amp; bye</h1>"));
        assert!(page.contains("prefers-color-scheme: dark"));
        assert_eq!(to_html("# Hi", false, None), "<h1>Hi</h1>\n");
    }

    #[test]
    fn markdown_written_from_the_model_reads_back_the_same() {
        let d = parse(MD);
        let out = write(&d);
        let again = parse(&out);
        for (i, (a, b)) in again.blocks.iter().zip(&d.blocks).enumerate() {
            assert_eq!(a, b, "block {i} differs; markdown was:\n{out}");
        }
        assert_eq!(again.blocks.len(), d.blocks.len(), "{out}");
    }
}
