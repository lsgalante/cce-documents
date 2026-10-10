//! A Markdown file as the layout reads it: front matter, then a flat list
//! of blocks.
//!
//! Nesting is flattened: a paragraph inside a list item inside a quote is
//! one `Block::Para` that carries its quote depth, list depth and, on an
//! item's first paragraph, the item's marker. That is all the layout needs
//! to indent it, draw the quote bar and hang the marker.
//!
//! Page breaks are a paragraph holding only `\pagebreak` (or `\newpage`):
//! not CommonMark, but it reads as what it is in any other editor.
//!
//! **Where text came from.** The editor works on the file's bytes but shows
//! only the text, markup hidden, so every span records anchors: points in
//! its text and the byte of the file they were read from. Between anchors
//! text and source run in step, so any offset in a block's text maps back
//! to the file (`Span::source_of`) — and markup, never in the text, is what
//! the anchors step over.

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
    /// (offset in `text`, byte of the file) pairs, ascending: where each
    /// stretch of the text was read from.
    pub src: Vec<(usize, usize)>,
}

impl Span {
    /// Whether two spans look the same (and may share a run of text).
    fn same_look(&self, o: &Span) -> bool {
        (self.bold, self.italic, self.mono, self.strike, &self.link) == (o.bold, o.italic, o.mono, o.strike, &o.link)
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

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading { level: u8, spans: Vec<Span> },
    Para { spans: Vec<Span>, nest: Nest },
    Code { text: String, nest: Nest },
    Image { src: String, alt: String },
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
    image: Option<(String, String)>,
}

impl Reader {
    fn nest(&mut self) -> Nest {
        Nest { quote: self.quote, list: self.lists.len() as u8, marker: self.marker.take() }
    }

    /// Add text read from the file at byte `src` (None: not anchored).
    fn text(&mut self, t: &str, src: Option<usize>) {
        if let Some((_, alt)) = &mut self.image {
            alt.push_str(t);
            return;
        }
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
    };
    for (event, at) in Parser::new_ext(body, Options::ENABLE_STRIKETHROUGH).into_offset_iter() {
        let range = base + at.start..base + at.end;
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
            Event::Start(Tag::Paragraph) => {
                r.flush();
                r.range = range;
            }
            Event::End(TagEnd::Paragraph) => r.flush(),
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
            Event::Start(Tag::Image { dest_url, .. }) => {
                // A picture stands on its own: what came before it in the
                // paragraph is a paragraph of its own.
                r.flush();
                r.range = range;
                r.image = Some((dest_url.to_string(), String::new()));
            }
            Event::End(TagEnd::Image) => {
                if let Some((src, alt)) = r.image.take() {
                    r.blocks.push((Block::Image { src, alt }, r.range.clone()));
                }
            }
            Event::Start(Tag::Emphasis) => r.look.italic = true,
            Event::End(TagEnd::Emphasis) => r.look.italic = false,
            Event::Start(Tag::Strong) => r.look.bold = true,
            Event::End(TagEnd::Strong) => r.look.bold = false,
            Event::Start(Tag::Strikethrough) => r.look.strike = true,
            Event::End(TagEnd::Strikethrough) => r.look.strike = false,
            Event::Start(Tag::Link { dest_url, .. }) => r.look.link = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => r.look.link = None,
            Event::Text(t) => {
                // Escapes and entities make the text differ from its source;
                // then only its start is anchored.
                let src = &body[at.clone()];
                r.text(&t, Some(range.start + if src == &*t { 0 } else { find_in(src, &t) }));
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
        assert_eq!(blocks[1], Block::Image { src: "cat.png".into(), alt: "a cat".into() });
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
