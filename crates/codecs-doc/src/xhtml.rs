//! The document model as XHTML (for EPUB chapters and self-contained web pages).

use crate::model::*;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// Maps a picture's source to what the page should say, or `None` to leave the picture out.
pub type ImageMap<'a> = dyn FnMut(&str) -> Option<String> + 'a;

fn inline(i: &[Inline], img: &mut ImageMap) -> String {
    let mut out = String::new();
    for x in i {
        match x {
            Inline::Break => out.push_str("<br />"),
            Inline::Image { src, alt } => match img(src) {
                Some(s) => out.push_str(&format!("<img src=\"{}\" alt=\"{}\" />", escape(&s), escape(alt))),
                None if !alt.is_empty() => out.push_str(&format!("<em>[{}]</em>", escape(alt))),
                None => {}
            },
            Inline::Text(t, s) => {
                let mut h = escape(t);
                if s.code {
                    h = format!("<code>{h}</code>");
                }
                if s.bold {
                    h = format!("<strong>{h}</strong>");
                }
                if s.italic {
                    h = format!("<em>{h}</em>");
                }
                if s.strike {
                    h = format!("<del>{h}</del>");
                }
                if s.underline && s.link.is_none() {
                    h = format!("<u>{h}</u>");
                }
                if let Some(url) = s.link.as_ref().filter(|u| !u.starts_with("epub:")) {
                    h = format!("<a href=\"{}\">{h}</a>", escape(url));
                }
                out.push_str(&h);
            }
        }
    }
    out
}

pub fn blocks(list: &[Block], img: &mut ImageMap) -> String {
    let mut out = String::new();
    for b in list {
        match b {
            Block::Heading(l, i) => out.push_str(&format!("<h{l}>{}</h{l}>\n", inline(i, img), l = (*l).clamp(1, 6))),
            Block::Para(i) => out.push_str(&format!("<p>{}</p>\n", inline(i, img))),
            Block::Rule => out.push_str("<hr />\n"),
            Block::Code(c) => out.push_str(&format!("<pre><code>{}</code></pre>\n", escape(c))),
            Block::Quote(inner) => out.push_str(&format!("<blockquote>\n{}</blockquote>\n", blocks(inner, img))),
            Block::List { start, items } => {
                let tag = if start.is_some() { "ol" } else { "ul" };
                let attr = match start {
                    Some(s) if *s != 1 => format!(" start=\"{s}\""),
                    _ => String::new(),
                };
                out.push_str(&format!("<{tag}{attr}>\n"));
                for item in items {
                    let task = match item.task {
                        Some(true) => "[x] ",
                        Some(false) => "[ ] ",
                        None => "",
                    };
                    // A one-paragraph item is just its text.
                    let body = match item.blocks.as_slice() {
                        [Block::Para(i)] => inline(i, img),
                        other => blocks(other, img).trim_end().to_string(),
                    };
                    out.push_str(&format!("<li>{task}{body}</li>\n"));
                }
                out.push_str(&format!("</{tag}>\n"));
            }
            Block::Table(t) => {
                let align = |c: usize| match t.aligns.get(c) {
                    Some(Align::Center) => " style=\"text-align:center\"",
                    Some(Align::Right) => " style=\"text-align:right\"",
                    _ => "",
                };
                out.push_str("<table>\n");
                if !t.head.is_empty() {
                    out.push_str("<thead><tr>");
                    for (c, cell) in t.head.iter().enumerate() {
                        out.push_str(&format!("<th{}>{}</th>", align(c), inline(cell, img)));
                    }
                    out.push_str("</tr></thead>\n");
                }
                out.push_str("<tbody>\n");
                for row in &t.rows {
                    out.push_str("<tr>");
                    for (c, cell) in row.iter().enumerate() {
                        out.push_str(&format!("<td{}>{}</td>", align(c), inline(cell, img)));
                    }
                    out.push_str("</tr>\n");
                }
                out.push_str("</tbody>\n</table>\n");
            }
        }
    }
    out
}

/// A complete web page around already-converted body markup.
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>\n{}</style>\n</head>\n<body>\n<main>\n{body}</main>\n</body>\n</html>\n",
        escape(title),
        crate::markdown::CSS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_becomes_well_formed_markup() {
        let doc = crate::markdown::parse("# T & U\n\n**b** *i* `c` ~~s~~ [l](https://e.com/?a=1&b=2) ![alt](a.png) ![gone](b.png)\n\n- [x] a\n- b\n\n3. x\n\n| h | r |\n|:-:|--:|\n| 1 | 2 |\n\n```\n<tag>\n```\n");
        let h = blocks(&doc.blocks, &mut |s| (s == "a.png").then(|| "img/a.png".to_string()));
        assert!(h.starts_with("<h1>T &amp; U</h1>\n<p><strong>b</strong> <em>i</em> <code>c</code> <del>s</del> <a href=\"https://e.com/?a=1&amp;b=2\">l</a> <img src=\"img/a.png\" alt=\"alt\" /> <em>[gone]</em></p>"), "{h}");
        assert!(h.contains("<li>[x] a</li>") && h.contains("<ol start=\"3\">") && h.contains("<th style=\"text-align:center\">h</th>") && h.contains("<td style=\"text-align:right\">2</td>"));
        assert!(h.contains("<pre><code>&lt;tag&gt;</code></pre>"));
    }
}
