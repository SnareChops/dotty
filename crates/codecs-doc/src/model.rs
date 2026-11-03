//! The document both Markdown and HTML are read into, and the PDF is laid out from.

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub underline: bool,
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text(String, Style),
    /// A forced line break.
    Break,
    Image { src: String, alt: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// `Some(done)` for a task-list item.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Table {
    pub aligns: Vec<Align>,
    pub head: Vec<Vec<Inline>>,
    pub rows: Vec<Vec<Vec<Inline>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Para(Vec<Inline>),
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<Item> },
    Code(String),
    Rule,
    Table(Table),
}

#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub title: Option<String>,
    pub blocks: Vec<Block>,
    /// Things that were left out, for notes to the user.
    pub dropped: Dropped,
}

#[derive(Clone, Debug, Default)]
pub struct Dropped {
    /// Pictures that are web addresses (not downloaded).
    pub remote_images: usize,
    /// HTML elements or styling that isn't supported.
    pub other: Vec<String>,
}

impl Doc {
    /// The text of the first heading, else the explicit title.
    pub fn display_title(&self) -> Option<String> {
        self.blocks.iter().find_map(|b| if let Block::Heading(_, i) = b { Some(plain(i)).filter(|t| !t.trim().is_empty()) } else { None }).or_else(|| self.title.clone())
    }
}

pub fn plain(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for i in inlines {
        match i {
            Inline::Text(t, _) => s.push_str(t),
            Inline::Break => s.push(' '),
            Inline::Image { alt, .. } => s.push_str(alt),
        }
    }
    s
}

/// Merge neighbouring text runs of one style, and trim spaces at the ends and around breaks.
pub fn tidy(inlines: Vec<Inline>) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    for i in inlines {
        match (out.last_mut(), i) {
            (Some(Inline::Text(a, sa)), Inline::Text(b, sb)) if *sa == sb => a.push_str(&b),
            (_, Inline::Text(t, s)) if t.is_empty() => drop(s),
            (_, i) => out.push(i),
        }
    }
    // Spaces next to a break or at the ends carry no meaning.
    let n = out.len();
    for k in 0..n {
        let next_is_break = matches!(out.get(k + 1), Some(Inline::Break));
        let prev_is_break = k > 0 && matches!(out[k - 1], Inline::Break);
        if let Inline::Text(t, _) = &mut out[k] {
            if k == 0 || prev_is_break {
                *t = t.trim_start().to_string();
            }
            if k + 1 == n || next_is_break {
                *t = t.trim_end().to_string();
            }
        }
    }
    out.retain(|i| !matches!(i, Inline::Text(t, _) if t.is_empty()));
    while matches!(out.last(), Some(Inline::Break)) {
        out.pop();
    }
    out
}

/// Call `f` on every inline in `blocks`.
pub fn visit_inlines(blocks: &mut [Block], f: &mut dyn FnMut(&mut Inline)) {
    fn run(i: &mut [Inline], f: &mut dyn FnMut(&mut Inline)) {
        for x in i {
            f(x);
        }
    }
    for b in blocks {
        match b {
            Block::Heading(_, i) | Block::Para(i) => run(i, f),
            Block::Quote(inner) => visit_inlines(inner, f),
            Block::List { items, .. } => {
                for it in items {
                    visit_inlines(&mut it.blocks, f);
                }
            }
            Block::Table(t) => {
                for cell in t.head.iter_mut().chain(t.rows.iter_mut().flatten()) {
                    run(cell, f);
                }
            }
            Block::Code(_) | Block::Rule => {}
        }
    }
}

/// Call `f` on the source of every picture in `blocks` and replace it with the result.
pub fn map_images(blocks: &mut [Block], f: &mut dyn FnMut(&str) -> String) {
    visit_inlines(blocks, &mut |i| {
        if let Inline::Image { src, .. } = i {
            *src = f(src);
        }
    });
}

/// The source of every picture in `blocks`, in order.
pub fn images(blocks: &[Block]) -> Vec<String> {
    let mut found = Vec::new();
    let mut copy = blocks.to_vec();
    map_images(&mut copy, &mut |s| {
        found.push(s.to_string());
        s.to_string()
    });
    found
}
