//! Laying a [`Doc`] out on pages: line breaking, pagination, lists, quotes, code, tables and
//! pictures. The result is a list of drawing items per page, in points from the top left;
//! `pdf` turns them into a file. Left-aligned text, one column, no hyphenation.

use crate::fonts::{FontId, Fonts};
use crate::model::*;
use std::path::{Path, PathBuf};

pub type Color = [u8; 3];

const TEXT: Color = [24, 24, 24];
const LINK: Color = [11, 87, 208];
const QUOTE: Color = [85, 85, 85];
const CODE_BG: Color = [243, 243, 243];
const RULE: Color = [190, 190, 190];
const LINE_HEIGHT: f32 = 1.35;

#[derive(Clone, Debug)]
pub enum Draw {
    Text { x: f32, y: f32, font: FontId, size: f32, text: String, color: Color },
    Rect { x: f32, y: f32, w: f32, h: f32, fill: Option<Color>, stroke: Option<(Color, f32)> },
    Line { x1: f32, y1: f32, x2: f32, y2: f32, color: Color, width: f32 },
    Image { x: f32, y: f32, w: f32, h: f32, image: usize },
    Link { x: f32, y: f32, w: f32, h: f32, url: String },
}

#[derive(Default, Debug)]
pub struct Page {
    pub items: Vec<Draw>,
}

/// A picture ready to embed.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub data: PictureData,
}

pub enum PictureData {
    /// A JPEG file, embedded as it is.
    Jpeg(Vec<u8>),
    Rgba(Vec<u8>),
}

#[derive(Clone)]
pub struct Settings {
    pub page: (f32, f32),
    pub margin: f32,
    pub font_size: f32,
}

impl Settings {
    pub const A4: (f32, f32) = (595.28, 841.89);
    pub const LETTER: (f32, f32) = (612.0, 792.0);
}

#[derive(Default)]
pub struct Layout {
    pub pages: Vec<Page>,
    pub pictures: Vec<Picture>,
    /// `(level, title, page, y)` of every heading.
    pub outline: Vec<(u8, String, usize, f32)>,
    pub missing_glyphs: u32,
    pub remote_images: usize,
    pub failed_images: Vec<String>,
}

#[derive(Clone)]
struct Word {
    text: String,
    font: FontId,
    size: f32,
    w: f32,
    /// Space before it (0 at the start of a line or after a break).
    gap: f32,
    code: bool,
    underline: bool,
    strike: bool,
    link: Option<String>,
}

#[derive(Clone)]
enum Tok {
    Word(Word),
    Break,
}

struct Line {
    words: Vec<(f32, Word)>,
    width: f32,
    size: f32,
}

#[derive(Clone, Copy)]
struct Flow {
    x: f32,
    width: f32,
    color: Color,
}

struct Layouter<F> {
    s: Settings,
    fonts: Fonts,
    out: Layout,
    load: F,
    page: usize,
    y: f32,
    /// Set while the first line of a list item is waited for (to put its marker beside it).
    first_line: Option<Option<(usize, f32, f32)>>,
}

/// Lay `doc` out; `load` fetches a picture from its source as written in the document.
pub fn layout(doc: &Doc, settings: Settings, load: impl FnMut(&str) -> Option<Picture>) -> Layout {
    let mut l = Layouter { y: settings.margin, s: settings, fonts: Fonts::new(), out: Layout::default(), load, page: 0, first_line: None };
    l.out.pages.push(Page::default());
    let flow = Flow { x: l.s.margin, width: l.s.page.0 - 2.0 * l.s.margin, color: TEXT };
    l.blocks(&doc.blocks, flow, 0);
    l.out
}

impl<F: FnMut(&str) -> Option<Picture>> Layouter<F> {
    fn fs(&self) -> f32 {
        self.s.font_size
    }

    fn top(&self) -> f32 {
        self.s.margin
    }

    fn bottom(&self) -> f32 {
        self.s.page.1 - self.s.margin
    }

    fn at_top(&self) -> bool {
        self.y <= self.top() + 0.5
    }

    fn new_page(&mut self) {
        self.out.pages.push(Page::default());
        self.page = self.out.pages.len() - 1;
        self.y = self.top();
    }

    /// Start a new page unless `h` fits below.
    fn need(&mut self, h: f32) {
        if self.y + h > self.bottom() + 0.01 && !self.at_top() {
            self.new_page();
        }
    }

    fn push(&mut self, item: Draw) {
        self.out.pages[self.page].items.push(item);
    }

    fn gap(&mut self, h: f32) {
        if !self.at_top() {
            self.y += h;
        }
    }

    // ---- inline text ----

    fn tokens(&mut self, inlines: &[Inline], size: f32, bold: bool) -> Vec<Tok> {
        let mut toks: Vec<Tok> = Vec::new();
        let mut pending_space = false;
        for i in inlines {
            match i {
                Inline::Break => {
                    toks.push(Tok::Break);
                    pending_space = false;
                }
                Inline::Image { alt, .. } => {
                    // Pictures inside a run of text (a table cell, say) show as their description.
                    let style = Style { italic: true, ..Default::default() };
                    let text = format!("[{}]", if alt.is_empty() { "image" } else { alt });
                    self.push_words(&mut toks, &text, &style, size, bold, &mut pending_space);
                }
                Inline::Text(t, style) => self.push_words(&mut toks, t, style, size, bold, &mut pending_space),
            }
        }
        toks
    }

    fn push_words(&mut self, toks: &mut Vec<Tok>, text: &str, style: &Style, size: f32, bold: bool, pending_space: &mut bool) {
        let font = FontId::of(style, bold);
        let size = if style.code { size * 0.9 } else { size };
        if text.starts_with(char::is_whitespace) {
            *pending_space = true;
        }
        for (n, word) in text.split_whitespace().enumerate() {
            if n > 0 {
                *pending_space = true;
            }
            self.out.missing_glyphs += self.fonts.missing(font, word);
            let w = self.fonts.width(font, word, size);
            let after_break = matches!(toks.last(), Some(Tok::Break) | None);
            let gap = if *pending_space && !after_break { self.fonts.width(font, " ", size) } else { 0.0 };
            *pending_space = false;
            toks.push(Tok::Word(Word { text: word.to_string(), font, size, w, gap, code: style.code, underline: style.underline || style.link.is_some(), strike: style.strike, link: style.link.clone() }));
        }
        if text.ends_with(char::is_whitespace) && !text.trim().is_empty() {
            *pending_space = true;
        }
    }

    /// Break `toks` into lines no wider than `width`. An empty line (two breaks in a row)
    /// is `empty_size` tall.
    fn wrap(&mut self, toks: &[Tok], width: f32, empty_size: f32) -> Vec<Line> {
        let fresh = || Line { words: Vec::new(), width: 0.0, size: 0.0 };
        let finish = |mut l: Line| {
            if l.words.is_empty() {
                l.size = empty_size;
            }
            l
        };
        let mut lines = Vec::new();
        let mut cur = fresh();
        for t in toks {
            match t {
                Tok::Break => lines.push(finish(std::mem::replace(&mut cur, fresh()))),
                Tok::Word(w) => {
                    let mut w = w.clone();
                    loop {
                        let gap = if cur.words.is_empty() { 0.0 } else { w.gap };
                        if !cur.words.is_empty() && cur.width + gap + w.w > width + 0.01 {
                            lines.push(finish(std::mem::replace(&mut cur, fresh())));
                            continue;
                        }
                        if cur.words.is_empty() && w.w > width {
                            // A word wider than the line: cut it where it fits.
                            let (head, tail) = self.split_word(&w, width);
                            cur.size = cur.size.max(head.size);
                            cur.width = head.w;
                            cur.words.push((0.0, head));
                            lines.push(finish(std::mem::replace(&mut cur, fresh())));
                            match tail {
                                Some(t) => {
                                    w = t;
                                    continue;
                                }
                                None => break,
                            }
                        }
                        cur.size = cur.size.max(w.size);
                        cur.words.push((cur.width + gap, w.clone()));
                        cur.width += gap + w.w;
                        break;
                    }
                }
            }
        }
        if !cur.words.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// The longest start of `w` that fits `width` (at least one character), and the rest.
    fn split_word(&mut self, w: &Word, width: f32) -> (Word, Option<Word>) {
        let chars: Vec<char> = w.text.chars().collect();
        let (mut used, mut n) = (0.0, 0);
        for c in &chars {
            let cw = self.fonts.width(w.font, &c.to_string(), w.size);
            if used + cw > width && n > 0 {
                break;
            }
            used += cw;
            n += 1;
        }
        let head: String = chars[..n].iter().collect();
        let tail: String = chars[n..].iter().collect();
        let mut head_w = w.clone();
        head_w.text = head;
        head_w.w = used;
        if tail.is_empty() {
            return (head_w, None);
        }
        let mut tail_w = w.clone();
        tail_w.w = self.fonts.width(w.font, &tail, w.size);
        tail_w.text = tail;
        tail_w.gap = 0.0;
        (head_w, Some(tail_w))
    }

    fn line_height(&self, size: f32) -> f32 {
        size * LINE_HEIGHT
    }

    /// Draw one line with its top at `top`; `offset` shifts it (alignment).
    fn place_line(&mut self, line: &Line, x0: f32, top: f32, lh: f32, color: Color) {
        let (asc, desc) = self.fonts.vertical(FontId::Sans);
        let baseline = top + (lh - line.size * (asc - desc)) / 2.0 + asc * line.size;
        if let Some(slot) = self.first_line.as_mut()
            && slot.is_none()
        {
            *slot = Some((self.page, baseline, line.size));
        }
        // Backgrounds (inline code), then decorations, then the text.
        for (k, (x, w)) in line.words.iter().enumerate() {
            if w.code {
                let joined = k > 0 && line.words[k - 1].1.code && w.gap > 0.0;
                let (x1, x2) = (if joined { x0 + x - w.gap } else { x0 + x }, x0 + x + w.w);
                self.push(Draw::Rect { x: x1 - 1.0, y: baseline - w.size * 0.95, w: x2 - x1 + 2.0, h: w.size * 1.25, fill: Some(CODE_BG), stroke: None });
            }
        }
        let mut k = 0;
        while k < line.words.len() {
            let (x, w) = &line.words[k];
            let col = if w.link.is_some() { LINK } else { color };
            // Merge neighbours that share font, size and colour into one run.
            let mut text = w.text.clone();
            let mut end = k + 1;
            while end < line.words.len() {
                let (_, n) = &line.words[end];
                let ncol = if n.link.is_some() { LINK } else { color };
                if n.font == w.font && n.size == w.size && ncol == col {
                    if n.gap > 0.0 {
                        text.push(' ');
                    }
                    text.push_str(&n.text);
                    end += 1;
                } else {
                    break;
                }
            }
            self.push(Draw::Text { x: x0 + x, y: baseline, font: w.font, size: w.size, text, color: col });
            k = end;
        }
        // Underline, strike-through and links, one span per run of like words.
        let mut k = 0;
        while k < line.words.len() {
            let (x, w) = &line.words[k];
            let key = (w.underline, w.strike, w.link.clone());
            if !(key.0 || key.1 || key.2.is_some()) {
                k += 1;
                continue;
            }
            let mut end = k + 1;
            while end < line.words.len() && (line.words[end].1.underline, line.words[end].1.strike, line.words[end].1.link.clone()) == key {
                end += 1;
            }
            let (xa, xb) = (x0 + x, x0 + line.words[end - 1].0 + line.words[end - 1].1.w);
            let col = if key.2.is_some() { LINK } else { color };
            if key.0 {
                self.push(Draw::Line { x1: xa, y1: baseline + w.size * 0.12, x2: xb, y2: baseline + w.size * 0.12, color: col, width: (w.size * 0.06).max(0.5) });
            }
            if key.1 {
                self.push(Draw::Line { x1: xa, y1: baseline - w.size * 0.3, x2: xb, y2: baseline - w.size * 0.3, color: col, width: (w.size * 0.06).max(0.5) });
            }
            if let Some(url) = key.2 {
                self.push(Draw::Link { x: xa, y: top, w: xb - xa, h: lh, url });
            }
            k = end;
        }
    }

    // ---- blocks ----

    fn blocks(&mut self, blocks: &[Block], flow: Flow, depth: usize) {
        for b in blocks {
            self.block(b, flow, depth);
        }
    }

    fn block(&mut self, b: &Block, flow: Flow, depth: usize) {
        let fs = self.fs();
        match b {
            Block::Para(inl) => {
                self.para(inl, flow, fs, false);
                self.y += fs * 0.5;
            }
            Block::Heading(level, inl) => {
                let size = fs * [2.0, 1.6, 1.35, 1.15, 1.0, 0.9][(*level as usize).clamp(1, 6) - 1];
                self.gap(size * 0.7);
                let toks = self.tokens(inl, size, true);
                let lines = self.wrap(&toks, flow.width, size);
                let lh = self.line_height(size);
                // Keep a heading with what follows: room for it and two more lines.
                self.need(lines.len() as f32 * lh + 2.0 * self.line_height(fs));
                self.out.outline.push((*level, plain(inl), self.page, self.y));
                for line in &lines {
                    self.need(lh);
                    self.place_line(line, flow.x, self.y, lh, [0, 0, 0]);
                    self.y += lh;
                }
                if *level <= 2 {
                    self.push(Draw::Line { x1: flow.x, y1: self.y + 1.0, x2: flow.x + flow.width, y2: self.y + 1.0, color: RULE, width: 0.6 });
                    self.y += 3.0;
                }
                self.y += size * 0.3;
            }
            Block::Rule => {
                self.need(fs);
                self.y += fs * 0.5;
                self.push(Draw::Line { x1: flow.x, y1: self.y, x2: flow.x + flow.width, y2: self.y, color: RULE, width: 0.8 });
                self.y += fs * 0.9;
            }
            Block::Quote(inner) => {
                let indent = 14.0;
                let start = (self.page, self.y);
                let inner_flow = Flow { x: flow.x + indent, width: (flow.width - indent).max(40.0), color: QUOTE };
                self.blocks(inner, inner_flow, depth + 1);
                let end = (self.page, (self.y - fs * 0.5).max(start.1));
                self.bar(start, end, flow.x + 2.0);
            }
            Block::List { start, items } => self.list(*start, items, flow, depth),
            Block::Code(code) => self.code(code, flow),
            Block::Table(t) => self.table(t, flow),
        }
    }

    /// A vertical bar from `start` to `end`, over however many pages that spans.
    fn bar(&mut self, start: (usize, f32), end: (usize, f32), x: f32) {
        for page in start.0..=end.0 {
            let y1 = if page == start.0 { start.1 } else { self.top() };
            let y2 = if page == end.0 { end.1 } else { self.bottom() };
            if y2 > y1 {
                self.out.pages[page].items.push(Draw::Rect { x, y: y1, w: 3.0, h: y2 - y1, fill: Some([200, 200, 200]), stroke: None });
            }
        }
    }

    /// A paragraph; pictures in it become blocks of their own.
    fn para(&mut self, inl: &[Inline], flow: Flow, size: f32, bold: bool) {
        let mut run: Vec<Inline> = Vec::new();
        for i in inl {
            if let Inline::Image { src, alt } = i {
                if !run.is_empty() {
                    self.text_lines(&std::mem::take(&mut run), flow, size, bold);
                }
                self.picture(src, alt, flow);
            } else {
                run.push(i.clone());
            }
        }
        if !run.is_empty() {
            self.text_lines(&run, flow, size, bold);
        }
    }

    fn text_lines(&mut self, inl: &[Inline], flow: Flow, size: f32, bold: bool) {
        let inl = tidy(inl.to_vec());
        if inl.is_empty() {
            return;
        }
        let toks = self.tokens(&inl, size, bold);
        let lines = self.wrap(&toks, flow.width, size);
        let lh = self.line_height(size);
        let n = lines.len();
        let mut i = 0;
        while i < n {
            let room = ((self.bottom() - self.y) / lh + 0.001).floor() as usize;
            let left = n - i;
            if (room == 0 || (i == 0 && left >= 4 && room == 1)) && !self.at_top() {
                self.new_page();
                continue;
            }
            let mut take = left.min(room.max(1));
            // Never leave a lone last line on the next page.
            if take < left && left - take == 1 && take > 2 {
                take -= 1;
            }
            for line in &lines[i..i + take] {
                self.place_line(line, flow.x, self.y, lh, flow.color);
                self.y += lh;
            }
            i += take;
            if i < n {
                self.new_page();
            }
        }
    }

    fn list(&mut self, start: Option<u64>, items: &[crate::model::Item], flow: Flow, depth: usize) {
        let fs = self.fs();
        let numbered = start.is_some();
        let last_number = start.unwrap_or(1) + items.len() as u64;
        let gutter = if numbered { (self.fonts.width(FontId::Sans, &format!("{last_number}."), fs) + 8.0).max(20.0) } else { 16.0 };
        for (k, item) in items.iter().enumerate() {
            let marker = match (item.task, start) {
                (Some(true), _) => "[x]".to_string(),
                (Some(false), _) => "[ ]".to_string(),
                (None, Some(s)) => format!("{}.", s + k as u64),
                (None, None) => if depth.is_multiple_of(2) { "\u{2022}".into() } else { "\u{2013}".into() },
            };
            let g = if item.task.is_some() { gutter.max(self.fonts.width(FontId::Sans, &marker, fs) + 6.0) } else { gutter };
            let (page0, y0) = (self.page, self.y);
            self.need(self.line_height(fs));
            self.first_line = Some(None);
            let inner = Flow { x: flow.x + g, width: (flow.width - g).max(40.0), ..flow };
            self.blocks(&item.blocks, inner, depth + 1);
            // Blocks add paragraph spacing; inside a list keep items closer.
            self.y -= fs * 0.25;
            let first = self.first_line.take().flatten();
            let (page, baseline, size) = first.unwrap_or((page0, y0 + fs, fs));
            let w = self.fonts.width(FontId::Sans, &marker, size);
            let x = if numbered { flow.x + g - 6.0 - w } else { flow.x + (g - w) / 2.0 - 2.0 };
            self.out.pages[page].items.push(Draw::Text { x: x.max(flow.x), y: baseline, font: FontId::Sans, size, text: marker, color: flow.color });
        }
        self.y += fs * 0.25;
    }

    fn code(&mut self, code: &str, flow: Flow) {
        let fs = self.fs() * 0.85;
        let lh = fs * 1.3;
        let pad = 6.0;
        let cw = self.fonts.width(FontId::Mono, "0", fs);
        let max_chars = (((flow.width - 2.0 * pad) / cw).floor() as usize).max(8);
        let mut rows: Vec<String> = Vec::new();
        for raw in code.lines() {
            let line = raw.replace('\t', "    ");
            if line.chars().count() <= max_chars {
                rows.push(line);
            } else {
                let chars: Vec<char> = line.chars().collect();
                for chunk in chars.chunks(max_chars) {
                    rows.push(chunk.iter().collect());
                }
            }
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        self.need(lh * rows.len().min(3) as f32 + 2.0 * pad);
        let (asc, desc) = self.fonts.vertical(FontId::Mono);
        self.push(Draw::Rect { x: flow.x, y: self.y, w: flow.width, h: pad, fill: Some(CODE_BG), stroke: None });
        self.y += pad;
        for row in rows {
            if self.y + lh > self.bottom() {
                self.push(Draw::Rect { x: flow.x, y: self.y, w: flow.width, h: pad.min(self.bottom() - self.y), fill: Some(CODE_BG), stroke: None });
                self.new_page();
                self.push(Draw::Rect { x: flow.x, y: self.y, w: flow.width, h: pad, fill: Some(CODE_BG), stroke: None });
                self.y += pad;
            }
            self.out.missing_glyphs += self.fonts.missing(FontId::Mono, &row);
            self.push(Draw::Rect { x: flow.x, y: self.y, w: flow.width, h: lh, fill: Some(CODE_BG), stroke: None });
            if !row.trim().is_empty() {
                let baseline = self.y + (lh - fs * (asc - desc)) / 2.0 + asc * fs;
                self.push(Draw::Text { x: flow.x + pad, y: baseline, font: FontId::Mono, size: fs, text: row, color: [40, 40, 40] });
            }
            self.y += lh;
        }
        self.push(Draw::Rect { x: flow.x, y: self.y, w: flow.width, h: pad, fill: Some(CODE_BG), stroke: None });
        self.y += pad + self.fs() * 0.5;
    }

    fn table(&mut self, t: &Table, flow: Flow) {
        let fs = self.fs() * 0.95;
        let cols = t.head.len().max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if cols == 0 {
            return;
        }
        let (pad_x, pad_y) = (5.0, 3.0);
        let toks = |s: &mut Self, cell: Option<&Vec<Inline>>, bold: bool| -> Vec<Tok> { s.tokens(cell.map(Vec::as_slice).unwrap_or(&[]), fs, bold) };
        // Column widths from the narrowest and the widest each cell can be.
        let (mut min_w, mut max_w) = (vec![24.0f32; cols], vec![24.0f32; cols]);
        let mut rows: Vec<(bool, Vec<Vec<Tok>>)> = Vec::new();
        if !t.head.is_empty() {
            rows.push((true, (0..cols).map(|c| toks(self, t.head.get(c), true)).collect()));
        }
        for r in &t.rows {
            rows.push((false, (0..cols).map(|c| toks(self, r.get(c), false)).collect()));
        }
        for (_, row) in &rows {
            for (c, cell) in row.iter().enumerate() {
                let words: Vec<&Word> = cell.iter().filter_map(|t| if let Tok::Word(w) = t { Some(w) } else { None }).collect();
                let widest = words.iter().map(|w| w.w).fold(0.0, f32::max);
                let total: f32 = words.iter().enumerate().map(|(i, w)| w.w + if i > 0 { w.gap } else { 0.0 }).sum();
                min_w[c] = min_w[c].max(widest + 2.0 * pad_x);
                max_w[c] = max_w[c].max(total + 2.0 * pad_x);
            }
        }
        let avail = flow.width;
        let (sum_min, sum_max): (f32, f32) = (min_w.iter().sum(), max_w.iter().sum());
        let widths: Vec<f32> = if sum_max <= avail {
            max_w.iter().map(|w| w * avail / sum_max).collect()
        } else if sum_min >= avail {
            min_w.iter().map(|w| w * avail / sum_min).collect()
        } else {
            let spare = avail - sum_min;
            let want: f32 = max_w.iter().zip(&min_w).map(|(a, b)| a - b).sum();
            min_w.iter().zip(&max_w).map(|(lo, hi)| lo + spare * (hi - lo) / want).collect()
        };
        let lh = self.line_height(fs);
        let mut first_row = true;
        let header = rows.first().filter(|r| r.0).cloned();
        for (is_head, cells) in &rows {
            let wrapped: Vec<Vec<Line>> = cells.iter().enumerate().map(|(c, toks)| self.wrap(toks, widths[c] - 2.0 * pad_x, fs)).collect();
            let lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let h = lines as f32 * lh + 2.0 * pad_y;
            let mut fresh_page = false;
            if self.y + h > self.bottom() + 0.01 && !self.at_top() {
                self.new_page();
                fresh_page = true;
            }
            // A table that runs over a page repeats its header row.
            if fresh_page && !*is_head && !first_row
                && let Some((_, hcells)) = &header
            {
                {
                    let hw: Vec<Vec<Line>> = hcells.iter().enumerate().map(|(c, toks)| self.wrap(toks, widths[c] - 2.0 * pad_x, fs)).collect();
                    let hh = hw.iter().map(Vec::len).max().unwrap_or(1).max(1) as f32 * lh + 2.0 * pad_y;
                    self.table_row(&hw, &widths, flow, hh, true, t, (pad_x, pad_y), lh);
                    self.y += hh;
                }
            }
            self.table_row(&wrapped, &widths, flow, h, *is_head, t, (pad_x, pad_y), lh);
            self.y += h;
            first_row = false;
        }
        self.y += self.fs() * 0.6;
    }

    #[allow(clippy::too_many_arguments)]
    fn table_row(&mut self, cells: &[Vec<Line>], widths: &[f32], flow: Flow, h: f32, head: bool, t: &Table, (pad_x, pad_y): (f32, f32), lh: f32) {
        let mut x = flow.x;
        for (c, lines) in cells.iter().enumerate() {
            self.push(Draw::Rect { x, y: self.y, w: widths[c], h, fill: head.then_some([238, 238, 238]), stroke: Some(([180, 180, 180], 0.5)) });
            for (n, line) in lines.iter().enumerate() {
                let shift = match t.aligns.get(c).copied().unwrap_or_default() {
                    Align::Left => 0.0,
                    Align::Center => (widths[c] - 2.0 * pad_x - line.width) / 2.0,
                    Align::Right => widths[c] - 2.0 * pad_x - line.width,
                };
                self.place_line(line, x + pad_x + shift.max(0.0), self.y + pad_y + n as f32 * lh, lh, flow.color);
            }
            x += widths[c];
        }
    }

    // ---- pictures ----

    fn picture(&mut self, src: &str, alt: &str, flow: Flow) {
        let lower = src.to_ascii_lowercase();
        let fs = self.fs();
        let remote = lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("//");
        let loaded = if remote || lower.starts_with("data:") { None } else { (self.load)(src) };
        let Some(pic) = loaded else {
            if remote {
                self.out.remote_images += 1;
            } else {
                self.out.failed_images.push(src.to_string());
            }
            let text = if alt.is_empty() { "[picture]".to_string() } else { format!("[{alt}]") };
            self.text_lines(&[Inline::Text(text, Style { italic: true, ..Default::default() })], Flow { color: QUOTE, ..flow }, fs, false);
            self.y += fs * 0.4;
            return;
        };
        let (mut w, mut h) = (pic.width as f32 * 0.75, pic.height as f32 * 0.75);
        let max_h = (self.bottom() - self.top()) * 0.85;
        let k = (flow.width / w).min(max_h / h).min(1.0);
        (w, h) = (w * k, h * k);
        self.need(h);
        self.out.pictures.push(pic);
        let image = self.out.pictures.len() - 1;
        self.push(Draw::Image { x: flow.x, y: self.y, w, h, image });
        self.y += h + fs * 0.6;
    }
}

/// A picture path as written in a document, made absolute: percent-escapes undone, `file://`
/// dropped, relative to `base`.
pub fn resolve(base: &Path, src: &str) -> PathBuf {
    let src = src.strip_prefix("file://").unwrap_or(src);
    let decoded = percent_decode(src.split(['?', '#']).next().unwrap_or(src));
    let p = Path::new(&decoded);
    if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
}

/// `%20`-style escapes undone.
pub fn percent_decode(src: &str) -> String {
    let b = src.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown;

    fn settings() -> Settings {
        Settings { page: Settings::A4, margin: 54.0, font_size: 11.5 }
    }

    fn lay(md: &str, s: Settings) -> Layout {
        layout(&markdown::parse(md), s, |_| None)
    }

    fn words(n: usize) -> String {
        let w = ["lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "tempor"];
        (0..n).map(|i| w[i * 7 % w.len()]).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn text_wraps_inside_the_margins_on_every_page() {
        let md = (0..40).map(|i| format!("## Part {i}\n\n{}\n\n- {}\n- {}\n", words(120), words(30), words(40))).collect::<String>();
        let s = settings();
        let l = lay(&md, s.clone());
        assert!(l.pages.len() > 5, "{} pages", l.pages.len());
        let mut fonts = Fonts::new();
        for (n, p) in l.pages.iter().enumerate() {
            for item in &p.items {
                if let Draw::Text { x, y, font, size, text, .. } = item {
                    let w = fonts.width(*font, text, *size);
                    assert!(*x >= s.margin - 0.5 && x + w <= s.page.0 - s.margin + 0.5, "page {n}: {text:?} at {x}+{w}");
                    assert!(*y > s.margin && *y <= s.page.1 - s.margin + 1.0, "page {n}: baseline {y}");
                }
            }
        }
    }

    #[test]
    fn headings_are_never_left_alone_at_the_foot_of_a_page() {
        let md = (0..60).map(|i| format!("## Heading {i}\n\n{}\n\n", words(40 + i % 25))).collect::<String>();
        let s = settings();
        let l = lay(&md, s.clone());
        let lh = s.font_size * LINE_HEIGHT;
        assert_eq!(l.outline.len(), 60);
        for (_, title, page, y) in &l.outline {
            assert!(*y + 3.0 * lh <= s.page.1 - s.margin + 1.0 || *y <= s.margin + 1.0, "{title} on page {page} at {y} has too little room after it");
        }
        // And the bookmarks point at the right pages: later headings never come earlier.
        assert!(l.outline.windows(2).all(|w| (w[0].2, w[0].3) < (w[1].2, w[1].3)));
    }

    #[test]
    fn tables_fit_the_width_and_repeat_their_header_across_pages() {
        let rows: String = (0..90).map(|i| format!("| r{i} | {} | {i} |\n", words(8))).collect();
        let l = lay(&format!("| Key | Description | N |\n|---|---|--:|\n{rows}"), settings());
        assert!(l.pages.len() >= 2);
        let s = settings();
        let header_on = |p: &Page| p.items.iter().any(|i| matches!(i, Draw::Text { text, font: FontId::SansBold, .. } if text == "Description"));
        assert!(l.pages.iter().all(header_on), "every page of the table starts with the header");
        for p in &l.pages {
            for item in &p.items {
                if let Draw::Rect { x, w, .. } = item {
                    assert!(*x >= s.margin - 0.01 && x + w <= s.page.0 - s.margin + 0.01);
                }
            }
        }
    }

    #[test]
    fn long_words_and_code_lines_are_cut_to_fit() {
        let l = lay(&format!("{}\n\n```\n{}\n```\n", "x".repeat(400), "y".repeat(500)), settings());
        let mut fonts = Fonts::new();
        let s = settings();
        let mut pieces = 0;
        for item in l.pages.iter().flat_map(|p| &p.items) {
            if let Draw::Text { x, font, size, text, .. } = item {
                assert!(x + fonts.width(*font, text, *size) <= s.page.0 - s.margin + 0.5, "{} chars overflow", text.len());
                pieces += 1;
            }
        }
        assert!(pieces >= 4, "the word and the code line were each split");
    }

    #[test]
    fn links_are_underlined_and_clickable_and_pictures_are_scaled() {
        let big = Picture { width: 3000, height: 2000, data: PictureData::Rgba(Vec::new()) };
        let md = "See [the site](https://example.com/a) for details.\n\n![pic](p.png)\n\n![web](https://x.test/a.png) and ![gone](missing.png)\n";
        let mut asked = Vec::new();
        let mut big = Some(big);
        let l = layout(&markdown::parse(md), settings(), |src| {
            asked.push(src.to_string());
            if src == "p.png" { big.take() } else { None }
        });
        assert!(l.pages[0].items.iter().any(|i| matches!(i, Draw::Link { url, w, .. } if url == "https://example.com/a" && *w > 30.0)));
        assert!(l.pages[0].items.iter().any(|i| matches!(i, Draw::Line { color, .. } if *color == LINK)), "underlined");
        let Some(Draw::Image { w, h, .. }) = l.pages[0].items.iter().find(|i| matches!(i, Draw::Image { .. })) else { panic!("picture placed") };
        let s = settings();
        assert!(*w <= s.page.0 - 2.0 * s.margin + 0.01 && *h <= (s.page.1 - 2.0 * s.margin) * 0.85 + 0.01);
        assert!((w / h - 1.5).abs() < 0.01, "aspect ratio kept");
        assert_eq!((l.remote_images, l.failed_images.clone()), (1, vec!["missing.png".to_string()]));
        assert_eq!(asked.len(), 2, "web pictures are never fetched: {asked:?}");
    }

    #[test]
    fn characters_the_font_lacks_are_counted() {
        assert_eq!(lay("plain text", settings()).missing_glyphs, 0);
        assert!(lay("\u{4e2d}\u{6587} text", settings()).missing_glyphs >= 2);
    }

    #[test]
    fn quotes_get_a_bar_that_follows_them_over_a_page_break() {
        let l = lay(&format!("> {}\n", words(900)), settings());
        assert!(l.pages.len() >= 2);
        let bars = l.pages.iter().filter(|p| p.items.iter().any(|i| matches!(i, Draw::Rect { w, fill: Some([200, 200, 200]), .. } if *w == 3.0))).count();
        assert_eq!(bars, l.pages.len());
    }

    #[test]
    fn picture_paths_are_resolved_like_a_browser_would() {
        let base = Path::new("/docs");
        assert_eq!(resolve(base, "img/a.png"), Path::new("/docs/img/a.png"));
        assert_eq!(resolve(base, "my%20pic.png?x=1#frag"), Path::new("/docs/my pic.png"));
        assert_eq!(resolve(base, "file:///tmp/a.png"), Path::new("/tmp/a.png"));
        assert_eq!(resolve(base, "/abs/a.png"), Path::new("/abs/a.png"));
        assert_eq!(resolve(base, "100%.png"), Path::new("/docs/100%.png"));
    }
}
