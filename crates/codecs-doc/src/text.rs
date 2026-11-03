//! Plain text in and out of the document model.

use crate::model::*;

fn plain_style() -> Style {
    Style::default()
}

/// Does a run of lines look like a paragraph that was wrapped at a fixed width?
fn wrapped(lines: &[&str]) -> bool {
    if lines.len() < 3 {
        return false;
    }
    let lens: Vec<usize> = lines.iter().map(|l| l.trim_end().chars().count()).collect();
    let max = *lens.iter().max().unwrap_or(&0);
    // All but the last line come close to the same width.
    max >= 40 && lens[..lens.len() - 1].iter().filter(|&&l| l * 100 >= max * 85).count() * 10 >= (lens.len() - 1) * 8
}

/// Paragraphs are separated by blank lines. A paragraph whose lines were wrapped at a fixed
/// width is joined up; one with short lines (an address, a poem) keeps its line breaks.
pub fn parse(text: &str) -> Doc {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let mut doc = Doc::default();
    let mut group: Vec<&str> = Vec::new();
    let flush = |group: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if group.is_empty() {
            return;
        }
        let mut inl = Vec::new();
        if wrapped(group) {
            inl.push(Inline::Text(group.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "), plain_style()));
        } else {
            for (i, l) in group.iter().enumerate() {
                if i > 0 {
                    inl.push(Inline::Break);
                }
                inl.push(Inline::Text(l.trim_end().to_string(), plain_style()));
            }
        }
        blocks.push(Block::Para(tidy(inl)));
        group.clear();
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut group, &mut doc.blocks);
        } else {
            group.push(line);
        }
    }
    flush(&mut group, &mut doc.blocks);
    doc
}

fn inline_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for i in inlines {
        match i {
            Inline::Break => out.push('\n'),
            Inline::Image { alt, .. } => {
                if !alt.is_empty() {
                    out.push_str(&format!("[{alt}]"));
                }
            }
            Inline::Text(t, s) => {
                out.push_str(t);
                if let Some(url) = &s.link
                    && url != t.trim()
                    && !url.starts_with("epub:")
                {
                    out.push_str(&format!(" ({url})"));
                }
            }
        }
    }
    out
}

fn indent(text: &str, first: &str, rest: &str) -> String {
    text.lines().enumerate().map(|(i, l)| format!("{}{l}", if i == 0 { first } else { rest }).trim_end().to_string()).collect::<Vec<_>>().join("\n")
}

fn blocks_text(blocks: &[Block]) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            Block::Heading(_, i) | Block::Para(i) => inline_text(i),
            Block::Rule => "-----".to_string(),
            Block::Code(c) => indent(c, "    ", "    "),
            Block::Quote(inner) => indent(&blocks_text(inner), "> ", "> "),
            Block::List { start, items } => items
                .iter()
                .enumerate()
                .map(|(k, item)| {
                    let marker = match start {
                        Some(s) => format!("{}. ", s + k as u64),
                        None => "- ".to_string(),
                    };
                    let task = match item.task {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    let body = blocks_text(&item.blocks).replace("\n\n", "\n");
                    indent(&body, &format!("{marker}{task}"), &" ".repeat(marker.len()))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Table(t) => std::iter::once(&t.head)
                .filter(|h| !h.is_empty())
                .chain(t.rows.iter())
                .map(|row| row.iter().map(|c| inline_text(c).replace('\n', " ")).collect::<Vec<_>>().join(" | "))
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect();
    parts.join("\n\n")
}

pub fn write(doc: &Doc) -> String {
    let mut s = blocks_text(&doc.blocks);
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_wrapped_text_and_short_lines() {
        let wrapped_par = "This is a long paragraph that was hard wrapped at about sixty\ncolumns by some editor a long time ago and which should now\nflow freely again when it is laid out on a page of any width.";
        let doc = parse(&format!("{wrapped_par}\n\nDear Sir,\nWith regards,\nMe\n\n\n\nLast one"));
        assert_eq!(doc.blocks.len(), 3);
        let Block::Para(p) = &doc.blocks[0] else { panic!() };
        assert!(!p.contains(&Inline::Break) && plain(p).starts_with("This is a long paragraph that was hard wrapped at about sixty columns"));
        let Block::Para(p) = &doc.blocks[1] else { panic!() };
        assert_eq!(p.iter().filter(|i| **i == Inline::Break).count(), 2, "short lines stay lines");
        assert_eq!(parse("a\r\nb\r\n\r\nc").blocks.len(), 2);
    }

    #[test]
    fn the_model_is_written_as_plain_text() {
        let md = crate::markdown::parse("# Title\n\nSome **bold** and a [link](https://e.com) and `code`.\n\n- one\n- two\n  - nested\n\n1. a\n2. b\n\n> quoted\n\n```\nlet x = 1;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n![alt](p.png)\n");
        let t = write(&md);
        assert_eq!(t, "Title\n\nSome bold and a link (https://e.com) and code.\n\n- one\n- two\n  - nested\n\n1. a\n2. b\n\n> quoted\n\n    let x = 1;\n\na | b\n1 | 2\n\n-----\n\n[alt]\n");
    }
}
