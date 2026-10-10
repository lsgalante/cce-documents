//! Blocks onto pages.
//!
//! Two passes. **Shaping** turns each block into rows — a line of shaped
//! glyphs, a picture, a rule — each with its own height and items placed
//! relative to the text frame's left edge and the row's top. Decorations
//! that run alongside text (a quote's bar, a code block's ground) are
//! items of every row they cover, so a block split across pages carries
//! them onto both. **Pagination** then stacks rows down the pages:
//! headings stay with the first two lines of what follows, a paragraph
//! never leaves one line alone at the foot of a page (an orphan) or the
//! head of the next (a widow).
//!
//! Positions are points from the page's top-left, y down — what krilla
//! draws in, and what PDFium reports back (`text` module's display
//! points), so the layout can be checked against the PDF it becomes.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use cce_ui::cosmic_text::{self, fontdb, Attrs, Buffer, CacheKeyFlags, Family, FontSystem, Metrics, Shaping, Weight, Wrap};

use super::md::{Block, ColAlign, Nest, Picture, Span, Width};
use super::style::{Align, Face, Style};

const INK: [u8; 3] = [24, 24, 24];
const QUIET: [u8; 3] = [90, 90, 90];
const LINK: [u8; 3] = [30, 80, 200];
const CODE_GROUND: [u8; 3] = [243, 243, 243];
const QUOTE_BAR: [u8; 3] = [200, 200, 200];
const RULE: [u8; 3] = [150, 150, 150];
const TABLE_RULE: [u8; 3] = [60, 60, 60];
const TABLE_LINE: [u8; 3] = [210, 210, 210];
/// Indent per list level and per quote level, in points.
const LIST_INDENT: f32 = 20.0;
const QUOTE_INDENT: f32 = 14.0;
/// Code blocks' padding inside their ground.
const CODE_PAD: f32 = 6.0;
/// A table cell's padding, across and down.
const CELL_PAD_X: f32 = 6.0;
const CELL_PAD_Y: f32 = 3.5;
/// A footnote's text is this much of the body's size; its number hangs in
/// an indent this wide.
const NOTE_SIZE: f64 = 0.82;
const NOTE_INDENT: f32 = 14.0;
/// Between the text and the first footnote on a page (the short rule sits
/// in it), and between footnotes.
pub const NOTE_SEP: f32 = 14.0;
pub const NOTE_GAP: f32 = 2.0;
/// A footnote reference's size and rise, as shares of the text's size.
const SUP_SIZE: f32 = 0.62;
const SUP_RISE: f32 = 0.38;

/// One shaped glyph: its id, where it starts (absolute x on the page), its
/// advance, its offsets (em, as cosmic-text reports them), the bytes of
/// `GlyphRun::text` it was shaped from, and where its cluster starts in its
/// unit's text (`NOT_TEXT` for a list marker or a page number, which the
/// caret never visits).
#[derive(Debug, Clone)]
pub struct Glyph {
    pub id: u16,
    pub x: f32,
    pub advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
    pub range: Range<usize>,
    pub at: usize,
    pub flags: CacheKeyFlags,
}

pub const NOT_TEXT: usize = usize::MAX;

/// Glyphs of one face, size and color on one baseline.
#[derive(Debug, Clone)]
pub struct GlyphRun {
    pub font: fontdb::ID,
    pub size: f32,
    pub color: [u8; 3],
    pub baseline: f32,
    /// The text the glyphs' ranges index (the whole shaped line).
    pub text: Arc<str>,
    pub glyphs: Vec<Glyph>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Glyphs(GlyphRun),
    Rect { x: f32, y: f32, w: f32, h: f32, color: [u8; 3] },
    /// A picture: its file's bytes (PNG or JPEG) and where it goes.
    Image { data: Arc<Vec<u8>>, jpeg: bool, x: f32, y: f32, w: f32, h: f32 },
    /// A clickable area leading to `url`.
    Link { x: f32, y: f32, w: f32, h: f32, url: String },
    /// A clickable area leading to a place in the document: a page, and
    /// a y on it (page points). A contents entry.
    GoTo { x: f32, y: f32, w: f32, h: f32, page: usize, top: f32 },
}

impl Item {
    pub fn shifted(mut self, dx: f32, dy: f32) -> Item {
        match &mut self {
            Item::Glyphs(run) => {
                run.baseline += dy;
                for g in &mut run.glyphs {
                    g.x += dx;
                }
            }
            Item::Rect { x, y, .. } | Item::Image { x, y, .. } | Item::Link { x, y, .. } | Item::GoTo { x, y, .. } => {
                *x += dx;
                *y += dy;
            }
        }
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineEntry {
    pub level: u8,
    pub title: String,
    pub page: usize,
    pub y: f32,
}

/// A typeset document.
#[derive(Debug, Clone)]
pub struct Laid {
    /// Page width and height in points.
    pub size: (f32, f32),
    pub pages: Vec<Page>,
    pub outline: Vec<OutlineEntry>,
}

/// One row of a block: its height, and its items relative to the frame's
/// left edge and the row's top. A row is what pagination places; a page
/// break never splits one.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub height: f32,
    pub items: Vec<Item>,
    /// A line of text: where it starts in the block's text, and the x the
    /// caret stands at when it has no glyphs. None for anything else (a
    /// code block's padding, a picture, a table's rows — which make their
    /// lines themselves).
    pub start: Option<usize>,
    pub x: f32,
    /// Footnotes referred to on this row.
    pub notes: Vec<u32>,
}

impl Row {
    /// The row's text glyphs in order, as (text offset of the cluster,
    /// cluster length in bytes, x, advance).
    pub fn clusters(&self) -> impl Iterator<Item = (usize, usize, f32, f32)> + '_ {
        self.items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r) } else { None }).flat_map(|r| {
            r.glyphs.iter().filter(|g| g.at != NOT_TEXT).map(|g| (g.at, g.range.len(), g.x, g.advance))
        })
    }

    pub fn shift(&mut self, dx: f32) {
        self.items = std::mem::take(&mut self.items).into_iter().map(|i| i.shifted(dx, 0.0)).collect();
        self.x += dx;
    }
}

/// A line of text the caret can stand on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Line {
    /// Which of the block's texts it belongs to: 0, or a table cell's
    /// number in reading order.
    pub unit: usize,
    /// The row it is drawn in, and how far below the row's top.
    pub row: usize,
    pub dy: f32,
    pub h: f32,
    /// The column it belongs to, from the frame's left: the text's whole
    /// width, or a table cell's.
    pub x0: f32,
    pub x1: f32,
    /// Where its text starts in its unit's text, and the x the caret
    /// stands at when it has no glyphs.
    pub start: usize,
    pub x: f32,
    /// (text offset, length, x, advance) of each cluster, in order.
    pub clusters: Vec<(usize, usize, f32, f32)>,
}

/// The lines of rows that are lines of text (unit 0).
fn lines_of(rows: &[Row], width: f32) -> Vec<Line> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let start = row.start?;
            Some(Line { unit: 0, row: i, dy: 0.0, h: row.height, x0: 0.0, x1: width, start, x: row.x, clusters: row.clusters().collect() })
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Shaped {
    pub rows: Vec<Row>,
    /// The lines of text in the rows, by unit then in reading order.
    pub lines: Vec<Line>,
    pub space_before: f32,
    pub space_after: f32,
    pub keep_with_next: bool,
    pub page_break: bool,
    /// Starts a page of its own (a chapter heading, by the style).
    pub break_before: bool,
    pub outline: Option<(u8, String)>,
    /// A footnote: placed at the foot of the page that first refers to
    /// it, not in the flow.
    pub note: Option<u32>,
}

/// The style and the text frame's width blocks are shaped for.
pub struct Look<'a> {
    pub style: &'a Style,
    pub width: f32,
}

/// An entry of the table of contents, as shaped.
#[derive(Debug, Clone, PartialEq, Hash)]
pub struct TocEntry {
    pub level: u8,
    pub title: String,
    pub page: usize,
    /// Where the heading is, page points.
    pub top: u32,
}

/// What else shaping a block needs to know about where it is.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    /// The first line is indented (a paragraph following a paragraph,
    /// when the style indents).
    pub indent: bool,
    /// The headings, for a table of contents.
    pub toc: &'a [TocEntry],
}

fn attrs<'a>(face: &'a Face, span: Option<&Span>, mono: &'a Face, meta: usize, leading: f32) -> Attrs<'a> {
    let mono_span = span.is_some_and(|s| s.mono);
    let f = if mono_span { mono } else { face };
    let mut a = Attrs::new().family(Family::Name(&f.family)).metadata(meta);
    if f.bold || span.is_some_and(|s| s.bold) {
        a = a.weight(Weight::BOLD);
    }
    if span.is_some_and(|s| s.italic) {
        a = a.style(cosmic_text::Style::Italic);
    }
    if span.is_some_and(|s| s.note.is_some()) {
        // Small, on a line as tall as the text's.
        let size = face.size as f32;
        a = a.metrics(Metrics::new(size * SUP_SIZE, size * leading));
    }
    a
}

/// How a line of text sits across its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Set {
    Left,
    Justify,
    Center,
    Right,
}

impl From<Align> for Set {
    fn from(a: Align) -> Set {
        match a {
            Align::Left => Set::Left,
            Align::Justify => Set::Justify,
        }
    }
}

impl From<ColAlign> for Set {
    fn from(a: ColAlign) -> Set {
        match a {
            ColAlign::Left => Set::Left,
            ColAlign::Center => Set::Center,
            ColAlign::Right => Set::Right,
        }
    }
}

/// Shape text into rows at `width`, the first line `indent` narrower and
/// set in by as much. Each span's look decides its face; its index rides
/// along as metadata for the decorations (links, strike-through, inline
/// code grounds, raised footnote numbers).
#[allow(clippy::too_many_arguments)]
fn shape(fs: &mut FontSystem, spans: &[Span], face: &Face, look: &Look, width: f32, leading: f32, color: [u8; 3], set: Set, indent: f32) -> Vec<Row> {
    if indent <= 0.0 {
        return shape_rows(fs, spans, face, look, width, leading, color, set);
    }
    // The first line at its own width; the rest, from where it ended, at
    // the full width.
    let mut first = shape_rows(fs, spans, face, look, width - indent, leading, color, set);
    let Some(cut) = first.get(1).and_then(|r| r.start) else {
        for row in &mut first {
            row.shift(indent);
        }
        return first;
    };
    let mut rows = vec![first.swap_remove(0)];
    rows[0].shift(indent);
    let tail = split_spans(spans, cut);
    for mut row in shape_rows(fs, &tail, face, look, width, leading, color, set) {
        row.start = row.start.map(|s| s + cut);
        for item in &mut row.items {
            if let Item::Glyphs(run) = item {
                for g in &mut run.glyphs {
                    if g.at != NOT_TEXT {
                        g.at += cut;
                    }
                }
            }
        }
        rows.push(row);
    }
    rows
}

/// The spans' text from offset `cut` on.
fn split_spans(spans: &[Span], cut: usize) -> Vec<Span> {
    let mut out = Vec::new();
    let mut off = 0;
    for s in spans {
        let end = off + s.text.len();
        if end > cut {
            let from = cut.saturating_sub(off);
            out.push(Span { text: s.text[from..].to_string(), src: Vec::new(), ..s.clone() });
        }
        off = end;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn shape_rows(fs: &mut FontSystem, spans: &[Span], face: &Face, look: &Look, width: f32, leading: f32, color: [u8; 3], set: Set) -> Vec<Row> {
    let size = face.size as f32;
    let mut buffer = Buffer::new(fs, Metrics::new(size, size * leading));
    buffer.set_wrap(fs, Wrap::WordOrGlyph);
    buffer.set_size(fs, Some(width.max(1.0)), None);
    let mono = Face { size: face.size, ..look.style.mono.clone() };
    buffer.set_rich_text(
        fs,
        spans.iter().enumerate().map(|(i, s)| (s.text.as_str(), attrs(face, Some(s), &mono, i, leading))),
        attrs(face, None, &mono, usize::MAX, leading),
        Shaping::Advanced,
    );
    let align = match set {
        Set::Left => None,
        Set::Justify => Some(cosmic_text::Align::Justified),
        Set::Center => Some(cosmic_text::Align::Center),
        Set::Right => Some(cosmic_text::Align::Right),
    };
    if align.is_some() {
        for line in buffer.lines.iter_mut() {
            line.set_align(align);
        }
    }
    buffer.shape_until_scroll(fs, false);

    // Where each of cosmic-text's lines (the text split at hard breaks)
    // starts in the block's text.
    let mut line_starts = vec![0usize];
    let mut off = 0;
    for s in spans {
        for (i, c) in s.text.char_indices() {
            if c == '\n' {
                line_starts.push(off + i + 1);
            }
        }
        off += s.text.len();
    }
    let mut rows = Vec::new();
    for run in buffer.layout_runs() {
        let line_start = line_starts.get(run.line_i).copied().unwrap_or(0);
        let start = run.glyphs.first().map_or(line_start, |g| line_start + g.start);
        let empty_x = match set {
            Set::Center => width / 2.0,
            Set::Right => width,
            _ => 0.0,
        };
        let mut row = Row { height: run.line_height, items: Vec::new(), start: Some(start), x: empty_x, notes: Vec::new() };
        let baseline = run.line_y - run.line_top;
        let text: Arc<str> = Arc::from(run.text);
        // Grounds and lines under or through the text, per span.
        let mut extents: Vec<(usize, f32, f32)> = Vec::new();
        for g in run.glyphs {
            match extents.last_mut() {
                Some(e) if e.0 == g.metadata => e.2 = g.x + g.w,
                _ => extents.push((g.metadata, g.x, g.x + g.w)),
            }
        }
        for &(meta, x0, x1) in &extents {
            let Some(span) = spans.get(meta) else { continue };
            if span.mono {
                row.items.push(Item::Rect { x: x0 - 1.5, y: baseline - size * 0.85, w: x1 - x0 + 3.0, h: size * 1.15, color: CODE_GROUND });
            }
            if let Some(n) = span.note {
                if !row.notes.contains(&n) {
                    row.notes.push(n);
                }
            }
        }
        // Glyph runs: a new one wherever the face, size, color or baseline
        // changes.
        let mut runs: Vec<GlyphRun> = Vec::new();
        for (i, g) in run.glyphs.iter().enumerate() {
            let span = spans.get(g.metadata);
            let c = if span.is_some_and(|s| s.link.is_some()) { LINK } else { color };
            let base = if span.is_some_and(|s| s.note.is_some()) { baseline - size * SUP_RISE } else { baseline };
            let next_x = run.glyphs.get(i + 1).map(|n| n.x);
            let glyph = Glyph {
                id: g.glyph_id,
                x: g.x,
                // To the next glyph's start where there is one: justified
                // lines widen their spaces by moving what follows.
                advance: next_x.map_or(g.w, |nx| nx - g.x),
                x_offset: g.x_offset,
                y_offset: g.y_offset,
                range: g.start..g.end,
                at: line_start + g.start,
                flags: g.cache_key_flags,
            };
            match runs.last_mut() {
                Some(r) if r.font == g.font_id && r.size == g.font_size && r.color == c && r.baseline == base => r.glyphs.push(glyph),
                _ => runs.push(GlyphRun { font: g.font_id, size: g.font_size, color: c, baseline: base, text: Arc::clone(&text), glyphs: vec![glyph] }),
            }
        }
        // The last glyph of a run that is followed by another run advances
        // only by its own width; fix it up to the next run's start.
        for i in 0..runs.len().saturating_sub(1) {
            let next = runs[i + 1].glyphs[0].x;
            if let Some(last) = runs[i].glyphs.last_mut() {
                last.advance = next - last.x;
            }
        }
        row.items.extend(runs.into_iter().map(Item::Glyphs));
        for &(meta, x0, x1) in &extents {
            let Some(span) = spans.get(meta) else { continue };
            if span.strike {
                row.items.push(Item::Rect { x: x0, y: baseline - size * 0.3, w: x1 - x0, h: (size * 0.055).max(0.5), color });
            }
            if let Some(url) = &span.link {
                row.items.push(Item::Rect { x: x0, y: baseline + size * 0.12, w: x1 - x0, h: (size * 0.05).max(0.5), color: LINK });
                row.items.push(Item::Link { x: x0, y: 0.0, w: x1 - x0, h: run.line_height, url: url.clone() });
            }
        }
        rows.push(row);
    }
    rows
}

fn plain(text: &str) -> Vec<Span> {
    vec![Span { text: text.to_string(), ..Default::default() }]
}

/// Mark rows as not text (markers, page numbers, the contents): no glyph
/// is a caret stop and no row a line.
fn not_text(rows: &mut [Row]) {
    for row in rows {
        row.start = None;
        for item in &mut row.items {
            if let Item::Glyphs(run) = item {
                for g in &mut run.glyphs {
                    g.at = NOT_TEXT;
                }
            }
        }
    }
}

/// The width of a row's glyphs.
fn row_width(row: &Row) -> f32 {
    row.items.iter().map(|it| if let Item::Glyphs(r) = it { r.glyphs.iter().map(|g| g.advance).sum() } else { 0.0 }).sum()
}

/// A marker hung left of a row's text, on its baseline, `gap` before
/// `x`: the first of the row's glyph runs, so the PDF's text reads
/// marker, then text.
fn hang(fs: &mut FontSystem, row: &mut Row, marker: &str, face: &Face, look: &Look, leading: f32, color: [u8; 3], x: f32, gap: f32) {
    let mut m = shape_rows(fs, &plain(marker), face, look, 200.0, leading, color, Set::Left);
    not_text(&mut m);
    let Some(mrow) = m.into_iter().next() else { return };
    let w = row_width(&mrow);
    let Some(Item::Glyphs(mut run)) = mrow.items.into_iter().find(|i| matches!(i, Item::Glyphs(_))) else { return };
    // On the text's baseline, whatever the marker's own line height.
    if let Some(Item::Glyphs(text)) = row.items.iter().find(|i| matches!(i, Item::Glyphs(_))) {
        run.baseline = text.baseline;
    }
    let at = row.items.iter().position(|i| matches!(i, Item::Glyphs(_))).unwrap_or(row.items.len());
    row.items.insert(at, Item::Glyphs(run).shifted(x - gap - w, 0.0));
}

pub fn shape_block(fs: &mut FontSystem, block: &Block, look: &Look, base: &Path, cx: Context) -> Shaped {
    let style = look.style;
    let body_lead = style.leading as f32;
    let mut shaped = match block {
        Block::Heading { level, spans } => {
            let h = &style.headings[(*level as usize).clamp(1, 3) - 1];
            let rows = shape(fs, spans, &h.face, look, look.width, 1.2, INK, Set::Left, 0.0);
            let title: String = spans.iter().map(|s| s.text.as_str()).collect();
            Shaped {
                rows,
                space_before: h.space_before as f32,
                space_after: h.space_after as f32,
                keep_with_next: true,
                break_before: h.break_before,
                outline: Some((*level, title)),
                ..Default::default()
            }
        }
        Block::Para { spans, nest } => {
            let indent = indent_of(nest);
            let color = if nest.quote > 0 { QUIET } else { INK };
            let first = if cx.indent { style.indent as f32 } else { 0.0 };
            let mut rows = shape(fs, spans, &style.body, look, look.width - indent, body_lead, color, style.align.into(), first);
            for row in &mut rows {
                row.shift(indent);
            }
            if let (Some(marker), Some(first)) = (&nest.marker, rows.first_mut()) {
                hang(fs, first, marker, &style.body, look, body_lead, color, indent, 6.0);
            }
            quote_bars(&mut rows, nest);
            // List items sit close together; anything else keeps a
            // paragraph's distance, from a list above it too.
            let space = style.space_after as f32 * if nest.list > 0 { 0.4 } else { 1.0 };
            Shaped { rows, space_before: space, space_after: space, ..Default::default() }
        }
        Block::Code { text, nest } => {
            let indent = indent_of(nest);
            let face = &style.mono;
            let mut rows = shape(fs, &plain(text), face, look, look.width - indent - 2.0 * CODE_PAD, 1.3, INK, Set::Left, 0.0);
            for row in &mut rows {
                row.shift(indent + CODE_PAD);
            }
            let pad = Row { height: CODE_PAD, ..Default::default() };
            rows.insert(0, pad.clone());
            rows.push(pad);
            for row in &mut rows {
                row.items.insert(0, Item::Rect { x: indent, y: 0.0, w: look.width - indent, h: row.height, color: CODE_GROUND });
            }
            quote_bars(&mut rows, nest);
            Shaped { rows, space_before: style.space_after as f32, space_after: style.space_after as f32, ..Default::default() }
        }
        Block::Image { src, alt, picture } => shape_picture(fs, look, base, src, alt, picture),
        Block::Table { aligns, rows, head } => return shape_table(fs, look, aligns, rows, *head),
        Block::Note { number, spans, .. } => {
            let face = Face { size: style.body.size * NOTE_SIZE, ..style.body.clone() };
            let mut rows = shape(fs, spans, &face, look, look.width - NOTE_INDENT, 1.3, INK, Set::Left, 0.0);
            for row in &mut rows {
                row.shift(NOTE_INDENT);
            }
            if let (Some(n), Some(first)) = (number, rows.first_mut()) {
                hang(fs, first, &n.to_string(), &face, look, 1.3, INK, NOTE_INDENT, 4.0);
            }
            Shaped { rows, note: Some(number.unwrap_or(0)), ..Default::default() }
        }
        Block::Toc => shape_toc(fs, look, cx.toc),
        Block::Rule => {
            let w = look.width * 0.3;
            let row = Row { height: 18.0, items: vec![Item::Rect { x: (look.width - w) / 2.0, y: 8.7, w, h: 0.6, color: RULE }], ..Default::default() };
            Shaped { rows: vec![row], space_before: 4.0, space_after: 4.0, ..Default::default() }
        }
        Block::PageBreak => Shaped { page_break: true, ..Default::default() },
    };
    shaped.lines = lines_of(&shaped.rows, look.width);
    shaped
}

/// A picture at its size and place, its alt text as a caption beneath.
fn shape_picture(fs: &mut FontSystem, look: &Look, base: &Path, src: &str, alt: &[Span], picture: &Picture) -> Shaped {
    let style = look.style;
    let path = base.join(src);
    let data = std::fs::read(&path).ok().and_then(|bytes| {
        let jpeg = matches!(path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("jpg" | "jpeg"));
        let data = krilla::Data::from(bytes.clone());
        let image = if jpeg { krilla::image::Image::from_jpeg(data, true) } else { krilla::image::Image::from_png(data, true) };
        image.ok().map(|i| (Arc::new(bytes), jpeg, i.size()))
    });
    let set: Set = picture.align.map_or(Set::Center, Set::from);
    let mut rows = Vec::new();
    match data {
        Some((data, jpeg, (pw, ph))) => {
            // Its width as asked, else 96 px to the inch; no wider than the
            // text, no taller than the page's frame.
            let (_, _, _, frame_h) = style.frame();
            let mut w = match picture.width {
                Some(Width::Share(f)) => look.width * f,
                Some(Width::Points(p)) => p,
                None => pw as f32 * 0.75,
            }
            .min(look.width);
            let mut h = w * ph as f32 / pw.max(1) as f32;
            let max_h = frame_h as f32 * 0.9;
            if h > max_h {
                w *= max_h / h;
                h = max_h;
            }
            let x = match set {
                Set::Left | Set::Justify => 0.0,
                Set::Center => (look.width - w) / 2.0,
                Set::Right => look.width - w,
            };
            rows.push(Row { height: h, items: vec![Item::Image { data, jpeg, x, y: 0.0, w, h }], ..Default::default() });
        }
        None => {
            log::warn!("picture {}: not found or not PNG/JPEG", path.display());
            let mut note = shape(fs, &plain(&format!("[picture not found: {src}]")), &style.body, look, look.width, style.leading as f32, QUIET, set, 0.0);
            not_text(&mut note);
            rows.extend(note);
        }
    }
    if !alt.is_empty() {
        let face = Face { size: style.body.size * 0.9, ..style.body.clone() };
        let caption: Vec<Span> = alt.iter().map(|s| Span { italic: !s.italic, ..s.clone() }).collect();
        rows.push(Row { height: 4.0, ..Default::default() });
        rows.extend(shape(fs, &caption, &face, look, look.width, 1.3, QUIET, set, 0.0));
    }
    Shaped { rows, space_before: 8.0, space_after: style.space_after.max(6.0) as f32 + 4.0, ..Default::default() }
}

/// Columns' widths: each its natural width when they fit together, else
/// the narrow ones keep theirs and the wide ones share what is left.
fn column_widths(natural: &[f32], width: f32) -> Vec<f32> {
    if natural.iter().sum::<f32>() <= width {
        return natural.to_vec();
    }
    let mut out = vec![0.0; natural.len()];
    let mut open: Vec<usize> = (0..natural.len()).collect();
    let mut left = width;
    loop {
        let share = left / open.len().max(1) as f32;
        let (fits, wide): (Vec<usize>, Vec<usize>) = open.iter().partition(|&&c| natural[c] <= share);
        if fits.is_empty() {
            for c in wide {
                out[c] = share;
            }
            return out;
        }
        for c in fits {
            out[c] = natural[c];
            left -= natural[c];
        }
        open = wide;
        if open.is_empty() {
            return out;
        }
    }
}

/// A table: one row per table row (a page never breaks inside one), its
/// cells' lines numbered as units in reading order. Rules above, under the
/// header and below; hairlines between body rows.
fn shape_table(fs: &mut FontSystem, look: &Look, aligns: &[ColAlign], cells: &[Vec<super::md::Cell>], head: usize) -> Shaped {
    let style = look.style;
    let cols = cells.iter().map(|r| r.len()).max().unwrap_or(0).max(aligns.len()).max(1);
    let lead = 1.3;
    let face = |r: usize| Face { bold: style.body.bold || r < head, ..style.body.clone() };
    // Natural widths: each cell unwrapped.
    let mut natural = vec![2.0 * CELL_PAD_X + 12.0; cols];
    for (r, row) in cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let rows = shape_rows(fs, &cell.spans, &face(r), look, 100_000.0, lead, INK, Set::Left);
            let w = rows.iter().map(row_width).fold(0.0, f32::max);
            natural[c] = natural[c].max(w + 2.0 * CELL_PAD_X + 1.0);
        }
    }
    let widths = column_widths(&natural, look.width);
    let xs: Vec<f32> = widths.iter().scan(0.0, |x, w| {
        let at = *x;
        *x += w;
        Some(at)
    }).collect();
    let total: f32 = widths.iter().sum();
    let space = style.space_after.max(6.0) as f32;
    let mut out = Shaped { space_before: space + 2.0, space_after: space + 4.0, ..Default::default() };
    let mut unit = 0;
    for (r, row) in cells.iter().enumerate() {
        let mut items = Vec::new();
        let mut lines = Vec::new();
        let mut notes = Vec::new();
        let mut height: f32 = 0.0;
        for c in 0..cols {
            let set: Set = aligns.get(c).copied().unwrap_or(ColAlign::Left).into();
            let inner = widths[c] - 2.0 * CELL_PAD_X;
            let empty = Vec::new();
            let spans = row.get(c).map_or(&empty, |cell| &cell.spans);
            let shaped = shape(fs, spans, &face(r), look, inner, lead, INK, set, 0.0);
            let mut dy = CELL_PAD_Y;
            for line in shaped {
                let x = xs[c] + CELL_PAD_X;
                if let Some(start) = line.start.filter(|_| row.get(c).is_some()) {
                    let mut moved = line.clone();
                    moved.shift(x);
                    lines.push(Line { unit, row: r, dy, h: line.height, x0: xs[c], x1: xs[c] + widths[c], start, x: moved.x, clusters: moved.clusters().collect() });
                }
                for n in &line.notes {
                    if !notes.contains(n) {
                        notes.push(*n);
                    }
                }
                items.extend(line.items.into_iter().map(|i| i.shifted(x, dy)));
                dy += line.height;
            }
            height = height.max(dy + CELL_PAD_Y);
            if row.get(c).is_some() {
                unit += 1;
            }
        }
        // Rules: above the table, under the header, below the table; a
        // hairline between body rows.
        let rule = |y: f32, h: f32, color| Item::Rect { x: 0.0, y, w: total, h, color };
        if r == 0 {
            items.insert(0, rule(0.0, 0.8, TABLE_RULE));
        }
        if r + 1 == head {
            items.push(rule(height - 0.5, 0.5, TABLE_RULE));
        } else if r + 1 == cells.len() {
            items.push(rule(height - 0.8, 0.8, TABLE_RULE));
        } else if r >= head {
            items.push(rule(height - 0.3, 0.3, TABLE_LINE));
        }
        out.rows.push(Row { height, items, start: None, x: 0.0, notes });
        out.lines.extend(lines);
    }
    // Lines by unit, then down the cell.
    out.lines.sort_by(|a, b| a.unit.cmp(&b.unit).then(a.dy.total_cmp(&b.dy)));
    out
}

/// The table of contents: a title, then each heading with its page
/// number on the right, dotted leaders between. Not text: the caret passes
/// it by, and each entry leads to its heading.
fn shape_toc(fs: &mut FontSystem, look: &Look, entries: &[TocEntry]) -> Shaped {
    let style = look.style;
    let h2 = &style.headings[1];
    let mut rows = shape(fs, &plain("Contents"), &h2.face, look, look.width, 1.2, INK, Set::Left, 0.0);
    if let Some(last) = rows.last_mut() {
        last.height += h2.space_after as f32;
    }
    let num_w = style.body.size as f32 * 2.6;
    for e in entries {
        let indent = (e.level.saturating_sub(1)) as f32 * 14.0;
        let face = Face { bold: e.level == 1, ..style.body.clone() };
        let mut title = shape(fs, &plain(&e.title), &face, look, look.width - indent - num_w - 8.0, 1.35, INK, Set::Left, 0.0);
        let mut num = shape(fs, &plain(&(e.page + 1).to_string()), &style.body, look, num_w, 1.35, INK, Set::Right, 0.0);
        let (false, Some(n)) = (title.is_empty(), num.pop()) else { continue };
        let li = title.len() - 1;
        let end = indent + row_width(&title[li]);
        // (The rows are set in by the indent below; the number stays put.)
        let nx = look.width - num_w - indent;
        let n_start = nx + num_w - row_width(&n);
        title[li].items.extend(n.items.into_iter().map(|i| i.shifted(nx, 0.0)));
        // Leaders: small dots on the baseline, on a grid so they line up
        // from entry to entry.
        let baseline = title[li].items.iter().find_map(|i| if let Item::Glyphs(r) = i { Some(r.baseline) } else { None }).unwrap_or(10.0);
        let step = 4.5;
        let mut x = ((end + 6.0) / step).ceil() * step;
        while x + 1.0 < n_start + indent - 4.0 {
            title[li].items.push(Item::Rect { x: x - indent, y: baseline - 0.8, w: 0.8, h: 0.8, color: QUIET });
            x += step;
        }
        let mut height = 0.0;
        for row in &mut title {
            row.shift(indent);
            height += row.height;
        }
        not_text(&mut title);
        // One area leading to the heading, over the whole entry.
        if let Some(first) = title.first_mut() {
            first.items.push(Item::GoTo { x: indent, y: 0.0, w: look.width - indent, h: height, page: e.page, top: e.top as f32 });
        }
        rows.extend(title);
    }
    if entries.is_empty() {
        let mut none = shape(fs, &plain("(no headings yet)"), &style.body, look, look.width, 1.35, QUIET, Set::Left, 0.0);
        not_text(&mut none);
        rows.extend(none);
    }
    not_text(&mut rows);
    Shaped { rows, space_before: 4.0, space_after: style.space_after as f32 * 2.0, ..Default::default() }
}

fn indent_of(nest: &Nest) -> f32 {
    nest.quote as f32 * QUOTE_INDENT + nest.list as f32 * LIST_INDENT
}

/// A quoted block's bar, beside every row.
fn quote_bars(rows: &mut [Row], nest: &Nest) {
    if nest.quote == 0 {
        return;
    }
    let x = nest.quote as f32 * QUOTE_INDENT - QUOTE_INDENT + 2.0;
    for row in rows {
        row.items.insert(0, Item::Rect { x, y: 0.0, w: 2.0, h: row.height, color: QUOTE_BAR });
    }
}

/// Where the flow of blocks stands before a block: the page, y below the
/// frame's top, whether the page has anything on it, the space owed after
/// the block before, the height the page's footnotes take at its foot, and
/// the last footnote placed (footnotes are numbered in the order they are
/// first referred to, so a higher number is a first reference).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Flow {
    pub page: usize,
    pub y: f32,
    pub used: bool,
    pub after: f32,
    pub foot: f32,
    pub seen: u32,
}

impl Flow {
    fn new_page(&mut self) {
        (self.page, self.y, self.used, self.foot) = (self.page + 1, 0.0, false, 0.0);
    }
}

/// The room footnotes first referred to on a row take: their heights, and
/// the separator if the page has none yet.
fn notes_need(row: &Row, f: &Flow, foot: f32, note_h: &[f32]) -> f32 {
    let h: f32 = row.notes.iter().filter(|&&n| n > f.seen).filter_map(|&n| note_h.get(n as usize - 1).filter(|h| **h > 0.0).map(|h| h + NOTE_GAP)).sum();
    if h > 0.0 && foot == 0.0 {
        h + NOTE_SEP
    } else {
        h
    }
}

/// Lay out blocks `from..` down pages of a frame `fh` tall, starting in
/// state `flow`: each block's state before it (`states[b]`) and each of its
/// rows' page and top (`places[b]`). Earlier entries are left alone, which
/// is what lets an edit re-paginate from the block it changed: placements
/// depend only on what came before. Footnotes (`Shaped::note`) are not
/// placed here, but the room each takes (`note_h`, by number) is kept free
/// at the foot of the page that first refers to it; `place_notes` puts
/// them there.
pub fn flow(blocks: &[&Shaped], fh: f32, from: usize, mut f: Flow, places: &mut Vec<Vec<(usize, f32)>>, states: &mut Vec<Flow>, note_h: &[f32]) {
    places.resize(blocks.len(), Vec::new());
    states.resize(blocks.len(), Flow::default());
    for b in from..blocks.len() {
        let block = blocks[b];
        states[b] = f;
        let mut out = Vec::with_capacity(block.rows.len());
        if block.page_break {
            if f.used {
                f.new_page();
            }
            f.after = 0.0;
            places[b] = out;
            continue;
        }
        if block.rows.is_empty() || block.note.is_some() {
            places[b] = out;
            continue;
        }
        if block.break_before && f.used {
            f.new_page();
        }
        let gap = if f.used { f.after.max(block.space_before) } else { 0.0 };
        // What must fit with the block's first row: its second (no orphan),
        // and for a heading, the first two rows of what follows.
        let mut need: f32 = block.rows.iter().take(if block.keep_with_next { usize::MAX } else { 2 }).map(|r| r.height).sum();
        if block.keep_with_next {
            if let Some(next) = blocks.get(b + 1).filter(|n| !n.page_break) {
                need += block.space_after.max(next.space_before) + next.rows.iter().take(2).map(|r| r.height).sum::<f32>();
            }
        }
        if f.used && f.y + gap + need > fh - f.foot {
            f.new_page();
        }
        if f.used {
            f.y += gap;
        }
        let n = block.rows.len();
        for (i, row) in block.rows.iter().enumerate() {
            let notes = notes_need(row, &f, f.foot, note_h);
            let breaks = f.y + row.height > fh - f.foot - notes;
            // Keep the last line company: if it alone would spill over,
            // take the one before it along.
            let widow = !breaks && n >= 3 && i == n - 2 && i >= 2 && f.y + row.height + block.rows[n - 1].height > fh - f.foot - notes;
            if (breaks || widow) && f.used {
                f.new_page();
            }
            out.push((f.page, f.y));
            f.y += row.height;
            f.used = true;
            f.foot += notes_need(row, &f, f.foot, note_h);
            f.seen = row.notes.iter().copied().fold(f.seen, u32::max);
        }
        f.after = block.space_after;
        places[b] = out;
    }
}

/// Put footnotes at the foot of the page that first refers to each, in
/// the order they are referred to, the last one ending at the frame's
/// foot: their rows' places. `notes[n - 1]` is footnote n's block. Returns
/// the pages that have footnotes, with where their separator goes.
pub fn place_notes(blocks: &[&Shaped], fh: f32, places: &mut [Vec<(usize, f32)>], notes: &[Option<usize>]) -> Vec<(usize, f32)> {
    // Each footnote's page: where its first reference landed.
    let mut on_page: Vec<(usize, u32)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (b, block) in blocks.iter().enumerate() {
        for (r, row) in block.rows.iter().enumerate() {
            let Some(&(page, _)) = places[b].get(r) else { continue };
            for &n in &row.notes {
                if seen.insert(n) {
                    on_page.push((page, n));
                }
            }
        }
    }
    for (b, block) in blocks.iter().enumerate() {
        if block.note.is_some() {
            places[b].clear();
        }
    }
    let mut seps = Vec::new();
    let mut i = 0;
    while i < on_page.len() {
        let page = on_page[i].0;
        let here: Vec<usize> = on_page[i..].iter().take_while(|(p, _)| *p == page).filter_map(|&(_, n)| notes.get(n as usize - 1).copied().flatten()).collect();
        i += on_page[i..].iter().take_while(|(p, _)| *p == page).count();
        let total: f32 = here.iter().map(|&nb| blocks[nb].rows.iter().map(|r| r.height).sum::<f32>() + NOTE_GAP).sum();
        if here.is_empty() {
            continue;
        }
        let mut y = fh - total;
        seps.push((page, y - NOTE_SEP));
        for nb in here {
            let mut out = Vec::new();
            for row in &blocks[nb].rows {
                out.push((page, y));
                y += row.height;
            }
            y += NOTE_GAP;
            places[nb] = out;
        }
    }
    seps
}

/// Where a footnote area starts: the short rule above it, page items.
pub fn note_rule(frame: (f32, f32, f32, f32), y: f32) -> Item {
    Item::Rect { x: frame.0, y: frame.1 + y + NOTE_SEP / 2.0, w: (frame.2 * 0.3).min(120.0), h: 0.5, color: RULE }
}

/// Where each row of each block goes, from the start.
#[cfg(test)]
pub fn paginate(blocks: &[Shaped], fh: f32) -> Vec<Vec<(usize, f32)>> {
    let refs: Vec<&Shaped> = blocks.iter().collect();
    let (mut places, mut states) = (Vec::new(), Vec::new());
    flow(&refs, fh, 0, Flow::default(), &mut places, &mut states, &[]);
    places
}

/// What a page's header and footer fields read: `{page}`, `{pages}`,
/// `{title}`, `{author}`, `{date}`, `{section}`.
#[derive(Debug, Clone, Default, Hash, PartialEq)]
pub struct Fields {
    pub title: String,
    pub author: String,
    pub date: String,
    pub section: String,
    pub page: usize,
    pub pages: usize,
}

impl Fields {
    pub fn fill(&self, template: &str) -> String {
        template
            .replace("{page}", &self.page.to_string())
            .replace("{pages}", &self.pages.to_string())
            .replace("{title}", &self.title)
            .replace("{author}", &self.author)
            .replace("{date}", &self.date)
            .replace("{section}", &self.section)
    }
}

/// A page's header and footer: page items, in the margins above and below
/// the text frame. `page` counts from 0.
pub fn furniture(fs: &mut FontSystem, style: &Style, page: usize, fields: &Fields) -> Vec<Item> {
    let (fx, fy, fw, fh) = style.frame();
    let (fx, fy, fw, fh) = (fx as f32, fy as f32, fw as f32, fh as f32);
    let look = Look { style, width: fw };
    let face = Face { size: (style.body.size * 0.85).max(6.0), bold: false, ..style.body.clone() };
    let size = face.size as f32;
    let mut items = Vec::new();
    for (f, top) in [(&style.header, ((fy - size * 1.2) / 2.0).max(0.0)), (&style.footer, fy + fh + (style.margins[2] as f32 / 2.0 - size).max(size * 0.6))] {
        if page == 0 && !f.first {
            continue;
        }
        for (template, set) in [(&f.left, Set::Left), (&f.center, Set::Center), (&f.right, Set::Right)] {
            let text = fields.fill(template);
            if text.trim().is_empty() {
                continue;
            }
            let mut rows = shape_rows(fs, &plain(&text), &face, &look, fw, 1.2, QUIET, set);
            not_text(&mut rows);
            if let Some(row) = rows.into_iter().next() {
                items.extend(row.items.into_iter().map(|it| it.shifted(fx, top)));
            }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows of fixed heights, as if shaped, for pagination alone.
    fn block(heights: &[f32], keep: bool) -> Shaped {
        Shaped { rows: heights.iter().map(|&h| Row { height: h, ..Default::default() }).collect(), keep_with_next: keep, ..Default::default() }
    }

    /// The page each row lands on.
    fn pages_of(blocks: &[Shaped], fh: f32) -> Vec<Vec<usize>> {
        paginate(blocks, fh).into_iter().map(|p| p.into_iter().map(|(page, _)| page).collect()).collect()
    }

    #[test]
    fn a_heading_never_ends_a_page() {
        // Eight lines fill a ten-line page; the heading and two lines of the
        // next paragraph would not fit, so the heading moves over.
        let pages = pages_of(&[block(&[10.0; 8], false), block(&[10.0], true), block(&[10.0; 4], false)], 100.0);
        assert_eq!(pages[1], vec![1]);
    }

    #[test]
    fn no_line_is_left_alone() {
        // Nine lines used: a new paragraph's first line would be an orphan.
        let pages = pages_of(&[block(&[10.0; 9], false), block(&[10.0; 3], false)], 100.0);
        assert_eq!(pages[1], vec![1, 1, 1]);
        // Six used, a five-line paragraph: four fit, which would leave its
        // last line a widow — so three stay and two go over.
        let pages = pages_of(&[block(&[10.0; 6], false), block(&[10.0; 5], false)], 100.0);
        assert_eq!(pages[1], vec![0, 0, 0, 1, 1]);
    }

    #[test]
    fn columns_keep_their_width_when_they_fit_else_share() {
        assert_eq!(column_widths(&[50.0, 80.0], 300.0), vec![50.0, 80.0]);
        // The narrow column keeps its 40; the two wide share what is left.
        assert_eq!(column_widths(&[40.0, 500.0, 900.0], 300.0), vec![40.0, 130.0, 130.0]);
    }

    #[test]
    fn footnotes_take_room_at_the_foot_of_the_page() {
        // Ten lines would fill the page; the fifth refers to a 30-high
        // footnote, so only six fit (100 - 30 - separator).
        let mut text = block(&[10.0; 10], false);
        text.rows[4].notes = vec![1];
        let note = Shaped { rows: vec![Row { height: 30.0, ..Default::default() }], note: Some(1), ..Default::default() };
        let refs = vec![&text, &note];
        let (mut places, mut states) = (Vec::new(), Vec::new());
        flow(&refs, 100.0, 0, Flow::default(), &mut places, &mut states, &[30.0]);
        let pages: Vec<usize> = places[0].iter().map(|p| p.0).collect();
        let room = 100.0 - 30.0 - NOTE_GAP - NOTE_SEP;
        let fit = (room / 10.0).floor() as usize;
        assert_eq!(pages.iter().filter(|&&p| p == 0).count(), fit);
        let seps = place_notes(&refs, 100.0, &mut places, &[Some(1)]);
        assert_eq!(places[1], vec![(0, 100.0 - 30.0 - NOTE_GAP)]);
        assert_eq!(seps, vec![(0, 100.0 - 30.0 - NOTE_GAP - NOTE_SEP)]);
    }

    #[test]
    fn a_page_break_starts_a_page_unless_one_just_started() {
        let brk = Shaped { page_break: true, ..Default::default() };
        let pages = pages_of(&[block(&[10.0], false), brk.clone(), brk, block(&[10.0], false)], 100.0);
        assert_eq!(pages[3], vec![1]);
    }
}
