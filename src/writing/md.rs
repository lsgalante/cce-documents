//! A Markdown file as the layout reads it: front matter, then a flat list
//! of blocks.
//!
//! Nesting is flattened: a paragraph inside a list item inside a quote is
//! one `Block::Para` that carries its quote depth, list depth and, on an
//! item's first paragraph, the item's marker. That is all the layout needs
//! to indent it, draw the quote bar and hang the marker.
//!
//! Page breaks are a paragraph holding only `\pagebreak` (or `\newpage`):
//! not CommonMark, but it reads as what it is in any other editor. A
//! paragraph holding only `[TOC]` (or `\toc`) is the table of contents.
//!
//! Tables are GitHub's; footnotes are `[^label]` with a `[^label]: text`
//! definition anywhere, numbered in the order they are first referred to.
//! A picture takes pandoc's attributes after it, `{width=50% align=left}`
//! (or Obsidian's `![alt|300](…)` for a width in pixels), and its alt text
//! is its caption.
//!
//! **Where text came from.** The editor works on the file's bytes but shows
//! only the text, markup hidden, so every span records anchors: points in
//! its text and the byte of the file they were read from. Between anchors
//! text and source run in step, so any offset in a block's text maps back
//! to the file (`Span::source_of`) — and markup, never in the text, is what
//! the anchors step over. A footnote reference is an *atom*: its number
//! stands for the whole `[^label]`, and is stepped over and deleted whole.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// A run of inline text with one look.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    pub strike: bool,
    pub link: Option<String>,
    /// A footnote reference: its number, drawn raised and small.
    pub note: Option<u32>,
    /// (offset in `text`, byte of the file) pairs, ascending: where each
    /// stretch of the text was read from.
    pub src: Vec<(usize, usize)>,
    /// The bytes of the file the whole text stands for, when it is not
    /// read from them character by character (a footnote reference).
    pub atom: Option<Range<usize>>,
}

impl Span {
    /// Whether two spans look the same (and may share a run of text).
    fn same_look(&self, o: &Span) -> bool {
        self.atom.is_none() && o.atom.is_none() && (self.bold, self.italic, self.mono, self.strike, &self.link, self.note) == (o.bold, o.italic, o.mono, o.strike, &o.link, o.note)
    }

    /// The file byte of an offset in this span's text: from the anchor at
    /// or before it, counting on.
    #[cfg(test)]
    pub fn source_of(&self, off: usize) -> Option<usize> {
        let i = self.src.partition_point(|&(t, _)| t <= off).checked_sub(1)?;
        let (t, b) = self.src[i];
        Some(b + (off - t))
    }
}

/// Where a paragraph sits in lists and quotes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Nest {
    pub quote: u8,
    /// List nesting: 0 outside any list.
    pub list: u8,
    /// The item's marker ("•", "3."), on the first paragraph of an item.
    pub marker: Option<String>,
}

/// A table column's alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColAlign {
    Left,
    Center,
    Right,
}

/// A table cell: its text, and the bytes between its pipes.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub spans: Vec<Span>,
    pub src: Range<usize>,
}

/// How wide a picture is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Width {
    /// A share of the text's width, 0–1.
    Share(f32),
    Points(f32),
}

/// A picture's attributes: `{width=50% align=left}` after it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Picture {
    pub width: Option<Width>,
    pub align: Option<ColAlign>,
    /// Where the `{…}` sits in the file, when there is one.
    pub attrs: Option<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading { level: u8, spans: Vec<Span> },
    Para { spans: Vec<Span>, nest: Nest },
    Code { text: String, nest: Nest },
    /// A picture; its alt text, the caption, may be empty.
    Image { src: String, alt: Vec<Span>, picture: Picture },
    /// Rows of cells; the first `head` rows are the header.
    Table { aligns: Vec<ColAlign>, rows: Vec<Vec<Cell>>, head: usize },
    /// A footnote's text, and its number (None: nothing refers to it).
    Note { label: String, number: Option<u32>, spans: Vec<Span> },
    /// The generated table of contents.
    Toc,
    Rule,
    PageBreak,
}


/// Split a file into its front matter (`key: value` lines between `---`
/// fences at the very top) and the body.
pub fn split_front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let rest = match text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) {
        Some(rest) => rest,
        None => return (Vec::new(), text),
    };
    let mut fields = Vec::new();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let l = line.trim_end_matches(['\r', '\n']);
        if l == "---" || l == "..." {
            return (fields, &rest[offset..]);
        }
        if let Some((k, v)) = l.split_once(':') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            fields.push((k.trim().to_string(), v.to_string()));
        }
    }
    // No closing fence: it was not front matter.
    (Vec::new(), text)
}

/// One open list: its next number, or None for bullets.
struct List {
    next: Option<u64>,
}

struct Reader {
    blocks: Vec<(Block, Range<usize>)>,
    /// The source range of the block being read.
    range: Range<usize>,
    spans: Vec<Span>,
    /// The look of the text being read: nested emphasis and links.
    look: Span,
    lists: Vec<List>,
    quote: u8,
    marker: Option<String>,
    heading: Option<u8>,
    code: Option<String>,
    /// A picture being read: its source and where it started; its alt text
    /// collects in `spans`.
    image: Option<(String, usize)>,
    /// Text after a picture may open with its `{…}` attributes.
    after_image: bool,
    table: Option<Table>,
    /// A footnote definition being read: its label and where its text
    /// would start.
    note: Option<(String, usize)>,
    /// Footnote numbers by label, in order of first reference.
    numbers: Vec<String>,
}

/// A table being read.
struct Table {
    aligns: Vec<ColAlign>,
    rows: Vec<Vec<Cell>>,
    head: usize,
    cell: Range<usize>,
}

impl Reader {
    fn nest(&mut self) -> Nest {
        Nest { quote: self.quote, list: self.lists.len() as u8, marker: self.marker.take() }
    }

    /// Add text read from the file at byte `src` (None: not anchored).
    fn text(&mut self, t: &str, src: Option<usize>) {
        if let Some(code) = &mut self.code {
            code.push_str(t);
            return;
        }
        if !matches!(self.spans.last(), Some(last) if last.same_look(&self.look)) {
            self.spans.push(Span { text: String::new(), src: Vec::new(), ..self.look.clone() });
        }
        let span = self.spans.last_mut().expect("just ensured");
        if let Some(src) = src {
            span.src.push((span.text.len(), src));
        }
        span.text.push_str(t);
    }

    /// A footnote reference: its number, standing for `src`.
    fn note_ref(&mut self, label: &str, src: Range<usize>) {
        let label = label.to_lowercase();
        let n = match self.numbers.iter().position(|l| *l == label) {
            Some(i) => i + 1,
            None => {
                self.numbers.push(label);
                self.numbers.len()
            }
        } as u32;
        self.spans.push(Span { text: n.to_string(), note: Some(n), src: vec![(0, src.start)], atom: Some(src), ..self.look.clone() });
    }

    /// End a paragraph (or a tight list item's text): emit it.
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let all: String = spans.iter().map(|s| s.text.as_str()).collect();
        if matches!(all.trim(), "\\pagebreak" | "\\newpage") {
            self.blocks.push((Block::PageBreak, self.range.clone()));
            return;
        }
        if matches!(all.trim(), "[TOC]" | "\\toc") && self.lists.is_empty() && self.quote == 0 {
            self.blocks.push((Block::Toc, self.range.clone()));
            return;
        }
        if self.note.is_some() {
            // A footnote's later paragraphs run on in its one block.
            self.spans = spans;
            return;
        }
        let nest = self.nest();
        self.blocks.push((Block::Para { spans, nest }, self.range.clone()));
    }
}

#[cfg(test)]
pub fn parse(body: &str) -> Vec<Block> {
    parse_located(body, 0).into_iter().map(|(b, _)| b).collect()
}

/// Where `needle` sits in `hay` (a source slice), for text whose event
/// range also covers markup (inline code's backticks).
fn find_in(hay: &str, needle: &str) -> usize {
    if needle.is_empty() {
        0
    } else {
        hay.find(needle).unwrap_or(0)
    }
}

/// `{width=50% align=left}` at the start of `t`: the picture's attributes
/// and the bytes they take.
fn picture_attrs(t: &str) -> Option<(Option<Width>, Option<ColAlign>, usize)> {
    let inner = t.strip_prefix('{')?;
    let end = inner.find('}')?;
    let (mut width, mut align) = (None, None);
    for word in inner[..end].split_whitespace() {
        let Some((k, v)) = word.split_once('=') else { continue };
        let v = v.trim_matches('"');
        match k {
            "width" => {
                width = if let Some(p) = v.strip_suffix('%') {
                    p.parse::<f32>().ok().map(|p| Width::Share((p / 100.0).clamp(0.05, 1.0)))
                } else if let Some(px) = v.strip_suffix("px") {
                    px.parse::<f32>().ok().map(|px| Width::Points(px * 0.75))
                } else {
                    super::style::length(v, 72.0 / 25.4).map(|pt| Width::Points(pt as f32))
                }
            }
            "align" => {
                align = match v {
                    "left" => Some(ColAlign::Left),
                    "center" | "centre" => Some(ColAlign::Center),
                    "right" => Some(ColAlign::Right),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    Some((width, align, end + 2))
}

/// Blocks with their source ranges, every offset counted from `base` (the
/// body's offset in the file, past any front matter).
pub fn parse_located(body: &str, base: usize) -> Vec<(Block, Range<usize>)> {
    let mut r = Reader {
        blocks: Vec::new(),
        range: 0..0,
        spans: Vec::new(),
        look: Span::default(),
        lists: Vec::new(),
        quote: 0,
        marker: None,
        heading: None,
        code: None,
        image: None,
        after_image: false,
        table: None,
        note: None,
        numbers: Vec::new(),
    };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_FOOTNOTES;
    for (event, at) in Parser::new_ext(body, options).into_offset_iter() {
        let range = base + at.start..base + at.end;
        let after_image = std::mem::take(&mut r.after_image);
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                r.flush();
                r.range = range;
                r.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                let spans = std::mem::take(&mut r.spans);
                let level = r.heading.take().unwrap_or(1);
                r.blocks.push((Block::Heading { level, spans }, r.range.clone()));
            }
            Event::Start(Tag::Paragraph) if r.note.is_some() => {
                // A footnote's next paragraph: a line break in its text.
                if !r.spans.is_empty() {
                    r.text("\n", Some(range.start));
                }
            }
            Event::Start(Tag::Paragraph) => {
                r.flush();
                r.range = range;
            }
            Event::End(TagEnd::Paragraph) => {
                // (After a picture the paragraph's rest starts past it.)
                r.range.end = r.range.end.max(range.end);
                r.flush();
            }
            Event::Start(Tag::List(start)) => {
                r.flush();
                r.lists.push(List { next: start });
            }
            Event::End(TagEnd::List(_)) => {
                r.flush();
                r.lists.pop();
            }
            Event::Start(Tag::Item) => {
                r.flush();
                // A tight item's text has no paragraph of its own.
                r.range = range;
                r.marker = Some(match r.lists.last_mut().and_then(|l| l.next.as_mut()) {
                    Some(n) => {
                        let m = format!("{n}.");
                        *n += 1;
                        m
                    }
                    None => "•".to_string(),
                });
            }
            Event::End(TagEnd::Item) => r.flush(),
            Event::Start(Tag::BlockQuote(_)) => {
                r.flush();
                r.quote += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                r.flush();
                r.quote = r.quote.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(_kind @ (CodeBlockKind::Fenced(_) | CodeBlockKind::Indented))) => {
                r.flush();
                r.range = range;
                r.code = Some(String::new());
            }
            Event::End(TagEnd::CodeBlock) => {
                let text = r.code.take().unwrap_or_default();
                let nest = r.nest();
                r.blocks.push((Block::Code { text: text.trim_end_matches('\n').to_string(), nest }, r.range.clone()));
            }
            // In a table cell a picture is its alt text.
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) if r.table.is_some() => {}
            Event::Start(Tag::Image { dest_url, .. }) => {
                // A picture stands on its own: what came before it in the
                // paragraph is a paragraph of its own.
                r.flush();
                r.image = Some((dest_url.to_string(), range.start));
                r.range = range;
            }
            Event::End(TagEnd::Image) => {
                if let Some((src, _)) = r.image.take() {
                    let mut alt = std::mem::take(&mut r.spans);
                    let mut picture = Picture::default();
                    // Obsidian's width: `![alt|300](…)`, in pixels.
                    if let Some(last) = alt.last_mut() {
                        if let Some((rest, px)) = last.text.rsplit_once('|') {
                            if let Ok(px) = px.trim().parse::<f32>() {
                                picture.width = Some(Width::Points(px * 0.75));
                                last.text = rest.trim_end().to_string();
                            }
                        }
                    }
                    alt.retain(|s| !s.text.is_empty());
                    r.blocks.push((Block::Image { src, alt, picture }, r.range.clone()));
                    r.range = range.end..range.end;
                    r.after_image = true;
                }
            }
            Event::Start(Tag::Table(aligns)) => {
                r.flush();
                r.range = range;
                let aligns = aligns
                    .iter()
                    .map(|a| match a {
                        pulldown_cmark::Alignment::Center => ColAlign::Center,
                        pulldown_cmark::Alignment::Right => ColAlign::Right,
                        _ => ColAlign::Left,
                    })
                    .collect();
                r.table = Some(Table { aligns, rows: Vec::new(), head: 0, cell: 0..0 });
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                if let Some(t) = &mut r.table {
                    t.rows.push(Vec::new());
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = &mut r.table {
                    t.head = t.rows.len();
                }
            }
            Event::Start(Tag::TableCell) => {
                r.spans.clear();
                if let Some(t) = &mut r.table {
                    t.cell = range;
                }
            }
            Event::End(TagEnd::TableCell) => {
                let mut spans = std::mem::take(&mut r.spans);
                if let Some(t) = &mut r.table {
                    if spans.is_empty() {
                        // An empty cell still takes the caret: between its
                        // pipes, in the middle of its spaces.
                        let at = t.cell.start + (t.cell.len() + 1) / 2;
                        spans.push(Span { src: vec![(0, at)], ..Default::default() });
                    }
                    let cell = Cell { spans, src: t.cell.clone() };
                    if let Some(row) = t.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(t) = r.table.take() {
                    r.blocks.push((Block::Table { aligns: t.aligns, rows: t.rows, head: t.head }, r.range.clone()));
                }
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                r.flush();
                let src = &body[at.clone()];
                let text_at = src.find("]:").map_or(0, |i| i + 2);
                let text_at = text_at + usize::from(src[text_at..].starts_with([' ', '\t']));
                r.note = Some((label.to_lowercase(), range.start + text_at));
                r.range = range;
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if let Some((label, text_at)) = r.note.take() {
                    let mut spans = std::mem::take(&mut r.spans);
                    if spans.is_empty() {
                        spans.push(Span { src: vec![(0, text_at)], ..Default::default() });
                    }
                    r.blocks.push((Block::Note { label, number: None, spans }, r.range.clone()));
                }
            }
            Event::FootnoteReference(label) => r.note_ref(&label, range),
            Event::Start(Tag::Emphasis) => r.look.italic = true,
            Event::End(TagEnd::Emphasis) => r.look.italic = false,
            Event::Start(Tag::Strong) => r.look.bold = true,
            Event::End(TagEnd::Strong) => r.look.bold = false,
            Event::Start(Tag::Strikethrough) => r.look.strike = true,
            Event::End(TagEnd::Strikethrough) => r.look.strike = false,
            Event::Start(Tag::Link { dest_url, .. }) => r.look.link = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => r.look.link = None,
            Event::Text(t) => {
                let mut t: &str = &t;
                let mut start = range.start;
                // A picture's `{…}` attributes, right after it.
                if after_image {
                    if let Some((width, align, len)) = picture_attrs(t) {
                        if let Some((Block::Image { picture, .. }, block_range)) = r.blocks.last_mut() {
                            picture.width = width.or(picture.width);
                            picture.align = align;
                            picture.attrs = Some(start..start + len);
                            block_range.end = start + len;
                        }
                        t = &t[len..];
                        start += len;
                        r.range = start..range.end;
                        if t.trim().is_empty() {
                            continue;
                        }
                    }
                }
                // Escapes and entities make the text differ from its source;
                // then only its start is anchored.
                let src = &body[start - base..range.end - base];
                let at = if src == t { 0 } else { find_in(src, t) };
                r.text(t, Some(start + at));
            }
            Event::Code(t) => {
                let was = std::mem::replace(&mut r.look.mono, true);
                r.text(&t, Some(range.start + find_in(&body[at.clone()], &t)));
                r.look.mono = was;
            }
            Event::SoftBreak => r.text(" ", Some(range.start)),
            Event::HardBreak => r.text("\n", Some(range.start)),
            Event::Rule => {
                r.flush();
                r.blocks.push((Block::Rule, range));
            }
            _ => {}
        }
    }
    r.flush();
    // Footnotes take the numbers their first references were given.
    let numbers = std::mem::take(&mut r.numbers);
    for (block, _) in &mut r.blocks {
        if let Block::Note { label, number, .. } = block {
            *number = numbers.iter().position(|l| l == label).map(|i| i as u32 + 1);
        }
    }
    r.blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn front_matter_is_split_off() {
        let (f, body) = split_front_matter("---\nstyle: manuscript\npage: \"A5\"\n---\n# Title\n");
        assert_eq!(f, vec![("style".into(), "manuscript".into()), ("page".into(), "A5".into())]);
        assert_eq!(body, "# Title\n");
        // An unclosed fence is not front matter.
        let (f, body) = split_front_matter("---\nnot: closed\n");
        assert!(f.is_empty());
        assert_eq!(body, "---\nnot: closed\n");
    }

    #[test]
    fn blocks_carry_their_nesting_and_looks() {
        let blocks = parse("# One\n\nSome *soft* and **strong** `code`.\n\n> - first\n> - second\n\n1. a\n2. b\n\n\\pagebreak\n\n---\n");
        let Block::Heading { level: 1, spans } = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(text(spans), "One");
        let Block::Para { spans, .. } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(text(spans), "Some soft and strong code.");
        assert!(spans.iter().any(|s| s.italic && s.text == "soft"));
        assert!(spans.iter().any(|s| s.bold && s.text == "strong"));
        assert!(spans.iter().any(|s| s.mono && s.text == "code"));
        let Block::Para { nest, .. } = &blocks[2] else { panic!() };
        assert_eq!(nest, &Nest { quote: 1, list: 1, marker: Some("•".into()) });
        let Block::Para { nest, .. } = &blocks[5] else { panic!() };
        assert_eq!(nest.marker.as_deref(), Some("2."));
        assert_eq!(blocks[6], Block::PageBreak);
        assert_eq!(blocks[7], Block::Rule);
    }

    #[test]
    fn a_picture_stands_alone() {
        let blocks = parse("Look: ![a cat](cat.png) there.\n");
        assert_eq!(blocks.len(), 3);
        let Block::Image { src, alt, picture } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!((src.as_str(), text(alt).as_str(), picture), ("cat.png", "a cat", &Picture::default()));
    }

    #[test]
    fn pictures_take_attributes() {
        let body = "![A cat](cat.png){width=50% align=right} and on.\n\n![Dog|300](dog.jpg)\n";
        let blocks = parse_located(body, 0);
        let (Block::Image { picture, .. }, range) = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(picture.width, Some(Width::Share(0.5)));
        assert_eq!(picture.align, Some(ColAlign::Right));
        assert_eq!(&body[picture.attrs.clone().unwrap()], "{width=50% align=right}");
        assert_eq!(&body[range.clone()], "![A cat](cat.png){width=50% align=right}");
        let (Block::Para { spans, .. }, range) = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(text(spans), " and on.");
        assert!(range.start >= body.find('}').unwrap());
        let (Block::Image { alt, picture, .. }, _) = &blocks[2] else { panic!("{:?}", blocks[2]) };
        assert_eq!(text(alt), "Dog");
        assert_eq!(picture.width, Some(Width::Points(225.0)));
    }

    #[test]
    fn tables_read_cell_by_cell() {
        let body = "| Name | Qty |\n|:-----|----:|\n| apple |  3 |\n|  | x \\| y |\n";
        let blocks = parse_located(body, 0);
        let (Block::Table { aligns, rows, head }, _) = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(aligns, &vec![ColAlign::Left, ColAlign::Right]);
        assert_eq!((rows.len(), *head), (3, 1));
        assert_eq!(text(&rows[1][0].spans), "apple");
        assert_eq!(text(&rows[2][1].spans), "x | y");
        // An empty cell is anchored between its spaces.
        assert_eq!(text(&rows[2][0].spans), "");
        assert_eq!(rows[2][0].spans[0].src, vec![(0, body.find("|  |").unwrap() + 2)]);
    }

    #[test]
    fn footnotes_number_by_first_reference() {
        let body = "[^b]: Bee.\n\nOne[^a] two[^b] three[^a].\n\n[^a]: The *a* note.\n\n[^c]: Unused.\n";
        let blocks = parse(body);
        let Block::Para { spans, .. } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(text(spans), "One1 two2 three1.");
        let atom = spans.iter().find(|s| s.note == Some(2)).unwrap();
        assert_eq!(&body[atom.atom.clone().unwrap()], "[^b]");
        let numbers: Vec<_> = blocks.iter().filter_map(|b| if let Block::Note { label, number, .. } = b { Some((label.as_str(), *number)) } else { None }).collect();
        assert_eq!(numbers, vec![("b", Some(2)), ("a", Some(1)), ("c", None)]);
        let Block::Note { spans, .. } = &blocks[2] else { panic!() };
        assert_eq!(text(spans), "The a note.");
        // An empty definition still has a place for the caret.
        let blocks = parse_located("x[^1]\n\n[^1]: \n", 0);
        let (Block::Note { spans, .. }, _) = &blocks[1] else { panic!("{:?}", blocks) };
        assert_eq!(spans[0].src, vec![(0, 13)]);
    }

    #[test]
    fn contents_is_a_block() {
        assert_eq!(parse("[TOC]\n\n# One\n")[0], Block::Toc);
        assert_eq!(parse("\\toc\n")[0], Block::Toc);
    }

    #[test]
    fn text_maps_back_to_the_file() {
        let body = "Some **bold** and `code`,\nwrapped.\n";
        let blocks = parse_located(body, 100);
        let (Block::Para { spans, .. }, range) = &blocks[0] else { panic!() };
        assert_eq!(range.start, 100);
        // Every character of every span sits in the file where it was read.
        for span in spans {
            for (i, c) in span.text.char_indices() {
                if c == ' ' && span.text[..i].ends_with(',') {
                    continue; // the soft break's space stands for "\n"
                }
                let at = span.source_of(i).unwrap() - 100;
                assert_eq!(body[at..].chars().next(), Some(c), "{:?} at {i}", span.text);
            }
        }
    }
}
