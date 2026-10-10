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

use super::md::{Block, Nest, Span};
use super::style::{Align, Face, Style};

const INK: [u8; 3] = [24, 24, 24];
const QUIET: [u8; 3] = [90, 90, 90];
const LINK: [u8; 3] = [30, 80, 200];
const CODE_GROUND: [u8; 3] = [243, 243, 243];
const QUOTE_BAR: [u8; 3] = [200, 200, 200];
const RULE: [u8; 3] = [150, 150, 150];
/// Indent per list level and per quote level, in points.
const LIST_INDENT: f32 = 20.0;
const QUOTE_INDENT: f32 = 14.0;
/// Code blocks' padding inside their ground.
const CODE_PAD: f32 = 6.0;

/// One shaped glyph: its id, where it starts (absolute x on the page), its
/// advance, its offsets (em, as cosmic-text reports them), the bytes of
/// `GlyphRun::text` it was shaped from, and where its cluster starts in its
/// block's text (`NOT_TEXT` for a list marker or a page number, which the
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
            Item::Rect { x, y, .. } | Item::Image { x, y, .. } | Item::Link { x, y, .. } => {
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

#[derive(Debug, Clone)]
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
/// left edge and the row's top.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub height: f32,
    pub items: Vec<Item>,
}

impl Row {
    /// The row's text glyphs in order, as (block-text offset of the
    /// cluster, cluster length in bytes, x, advance).
    pub fn clusters(&self) -> impl Iterator<Item = (usize, usize, f32, f32)> + '_ {
        self.items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r) } else { None }).flat_map(|r| {
            r.glyphs.iter().filter(|g| g.at != NOT_TEXT).map(|g| (g.at, g.range.len(), g.x, g.advance))
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Shaped {
    pub rows: Vec<Row>,
    pub space_before: f32,
    pub space_after: f32,
    pub keep_with_next: bool,
    pub page_break: bool,
    pub outline: Option<(u8, String)>,
}

/// The style and the text frame's width blocks are shaped for.
pub struct Look<'a> {
    pub style: &'a Style,
    pub width: f32,
}

fn attrs<'a>(face: &'a Face, span: Option<&Span>, mono: &'a Face, meta: usize) -> Attrs<'a> {
    let mono_span = span.is_some_and(|s| s.mono);
    let f = if mono_span { mono } else { face };
    let mut a = Attrs::new().family(Family::Name(&f.family)).metadata(meta);
    if f.bold || span.is_some_and(|s| s.bold) {
        a = a.weight(Weight::BOLD);
    }
    if span.is_some_and(|s| s.italic) {
        a = a.style(cosmic_text::Style::Italic);
    }
    a
}

/// Shape text into rows at `width`. Each span's look decides its face; its
/// index rides along as metadata for the decorations (links, strike-through,
/// inline code grounds).
fn shape(fs: &mut FontSystem, spans: &[Span], face: &Face, look: &Look, width: f32, leading: f32, color: [u8; 3], align: Align) -> Vec<Row> {
    let size = face.size as f32;
    let mut buffer = Buffer::new(fs, Metrics::new(size, size * leading));
    buffer.set_wrap(fs, Wrap::WordOrGlyph);
    buffer.set_size(fs, Some(width.max(1.0)), None);
    let mono = Face { size: face.size, ..look.style.mono.clone() };
    buffer.set_rich_text(
        fs,
        spans.iter().enumerate().map(|(i, s)| (s.text.as_str(), attrs(face, Some(s), &mono, i))),
        attrs(face, None, &mono, usize::MAX),
        Shaping::Advanced,
    );
    if align == Align::Justify {
        for line in buffer.lines.iter_mut() {
            line.set_align(Some(cosmic_text::Align::Justified));
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
        let mut row = Row { height: run.line_height, items: Vec::new() };
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
        }
        // Glyph runs: a new one wherever the face, size or color changes.
        let mut runs: Vec<GlyphRun> = Vec::new();
        for (i, g) in run.glyphs.iter().enumerate() {
            let span = spans.get(g.metadata);
            let c = if span.is_some_and(|s| s.link.is_some()) { LINK } else { color };
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
                Some(r) if r.font == g.font_id && r.size == g.font_size && r.color == c => r.glyphs.push(glyph),
                _ => runs.push(GlyphRun { font: g.font_id, size: g.font_size, color: c, baseline, text: Arc::clone(&text), glyphs: vec![glyph] }),
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

/// Mark every glyph of rows as not text (markers, page numbers).
fn not_text(rows: &mut [Row]) {
    for row in rows {
        for item in &mut row.items {
            if let Item::Glyphs(run) = item {
                for g in &mut run.glyphs {
                    g.at = NOT_TEXT;
                }
            }
        }
    }
}

pub fn shape_block(fs: &mut FontSystem, block: &Block, look: &Look, base: &Path) -> Shaped {
    let style = look.style;
    match block {
        Block::Heading { level, spans } => {
            let h = &style.headings[(*level as usize).clamp(1, 3) - 1];
            let rows = shape(fs, spans, &h.face, look, look.width, 1.2, INK, Align::Left);
            let title: String = spans.iter().map(|s| s.text.as_str()).collect();
            Shaped {
                rows,
                space_before: h.space_before as f32,
                space_after: h.space_after as f32,
                keep_with_next: true,
                outline: Some((*level, title)),
                ..Default::default()
            }
        }
        Block::Para { spans, nest } => {
            let indent = indent_of(nest);
            let color = if nest.quote > 0 { QUIET } else { INK };
            let mut rows = shape(fs, spans, &style.body, look, look.width - indent, style.leading as f32, color, style.align);
            for row in &mut rows {
                row.items = std::mem::take(&mut row.items).into_iter().map(|i| i.shifted(indent, 0.0)).collect();
            }
            if let (Some(marker), Some(first)) = (&nest.marker, rows.first_mut()) {
                // The marker hangs left of the text, on its first baseline.
                let mut m = shape(fs, &plain(marker), &style.body, look, LIST_INDENT * 2.0, style.leading as f32, color, Align::Left);
                not_text(&mut m);
                if let Some(Item::Glyphs(run)) = m.into_iter().next().and_then(|r| r.items.into_iter().find(|i| matches!(i, Item::Glyphs(_)))) {
                    let w: f32 = run.glyphs.iter().map(|g| g.advance).sum();
                    // First among the row's runs: drawn in reading order, so
                    // the PDF's text reads marker, then item.
                    let at = first.items.iter().position(|i| matches!(i, Item::Glyphs(_))).unwrap_or(first.items.len());
                    first.items.insert(at, Item::Glyphs(run).shifted(indent - 6.0 - w, 0.0));
                }
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
            let mut rows = shape(fs, &plain(text), face, look, look.width - indent - 2.0 * CODE_PAD, 1.3, INK, Align::Left);
            for row in &mut rows {
                row.items = std::mem::take(&mut row.items).into_iter().map(|i| i.shifted(indent + CODE_PAD, 0.0)).collect();
            }
            let pad = Row { height: CODE_PAD, items: Vec::new() };
            rows.insert(0, pad.clone());
            rows.push(pad);
            for row in &mut rows {
                row.items.insert(0, Item::Rect { x: indent, y: 0.0, w: look.width - indent, h: row.height, color: CODE_GROUND });
            }
            quote_bars(&mut rows, nest);
            Shaped { rows, space_before: style.space_after as f32, space_after: style.space_after as f32, ..Default::default() }
        }
        Block::Image { src, alt } => {
            let path = base.join(src);
            let picture = std::fs::read(&path).ok().and_then(|bytes| {
                let jpeg = matches!(path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("jpg" | "jpeg"));
                let data = krilla::Data::from(bytes.clone());
                let image = if jpeg { krilla::image::Image::from_jpeg(data, true) } else { krilla::image::Image::from_png(data, true) };
                image.ok().map(|i| (Arc::new(bytes), jpeg, i.size()))
            });
            match picture {
                Some((data, jpeg, (pw, ph))) => {
                    // 96 px to the inch, no wider than the text, no taller
                    // than the page's frame.
                    let (_, _, _, frame_h) = style.frame();
                    let mut w = (pw as f32 * 0.75).min(look.width);
                    let mut h = w * ph as f32 / pw.max(1) as f32;
                    let max_h = frame_h as f32;
                    if h > max_h {
                        w *= max_h / h;
                        h = max_h;
                    }
                    let x = (look.width - w) / 2.0;
                    let row = Row { height: h, items: vec![Item::Image { data, jpeg, x, y: 0.0, w, h }] };
                    Shaped { rows: vec![row], space_before: 4.0, space_after: style.space_after as f32, ..Default::default() }
                }
                None => {
                    log::warn!("picture {}: not found or not PNG/JPEG", path.display());
                    let note = format!("[picture not found: {src}{}]", if alt.is_empty() { String::new() } else { format!(" — {alt}") });
                    let mut rows = shape(fs, &plain(&note), &style.body, look, look.width, style.leading as f32, QUIET, Align::Left);
                    not_text(&mut rows);
                    Shaped { rows, space_after: style.space_after as f32, ..Default::default() }
                }
            }
        }
        Block::Rule => {
            let w = look.width * 0.3;
            let row = Row { height: 18.0, items: vec![Item::Rect { x: (look.width - w) / 2.0, y: 8.7, w, h: 0.6, color: RULE }] };
            Shaped { rows: vec![row], space_before: 4.0, space_after: 4.0, ..Default::default() }
        }
        Block::PageBreak => Shaped { page_break: true, ..Default::default() },
    }
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
/// frame's top, whether the page has anything on it, and the space owed
/// after the block before.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Flow {
    pub page: usize,
    pub y: f32,
    pub used: bool,
    pub after: f32,
}

/// Lay out blocks `from..` down pages of a frame `fh` tall, starting in
/// state `flow`: each block's state before it (`states[b]`) and each of its
/// rows' page and top (`places[b]`). Earlier entries are left alone, which
/// is what lets an edit re-paginate from the block it changed: placements
/// depend only on what came before.
pub fn flow(blocks: &[&Shaped], fh: f32, from: usize, mut f: Flow, places: &mut Vec<Vec<(usize, f32)>>, states: &mut Vec<Flow>) {
    places.resize(blocks.len(), Vec::new());
    states.resize(blocks.len(), Flow::default());
    for b in from..blocks.len() {
        let block = blocks[b];
        states[b] = f;
        let mut out = Vec::with_capacity(block.rows.len());
        if block.page_break {
            if f.used {
                (f.page, f.y, f.used) = (f.page + 1, 0.0, false);
            }
            f.after = 0.0;
            places[b] = out;
            continue;
        }
        if block.rows.is_empty() {
            places[b] = out;
            continue;
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
        if f.used && f.y + gap + need > fh {
            (f.page, f.y, f.used) = (f.page + 1, 0.0, false);
        }
        if f.used {
            f.y += gap;
        }
        let n = block.rows.len();
        for (i, row) in block.rows.iter().enumerate() {
            let breaks = f.y + row.height > fh;
            // Keep the last line company: if it alone would spill over,
            // take the one before it along.
            let widow = !breaks && n >= 3 && i == n - 2 && i >= 2 && f.y + row.height + block.rows[n - 1].height > fh;
            if (breaks || widow) && f.used {
                (f.page, f.y) = (f.page + 1, 0.0);
            }
            out.push((f.page, f.y));
            f.y += row.height;
            f.used = true;
        }
        f.after = block.space_after;
        places[b] = out;
    }
}

/// Where each row of each block goes, from the start.
#[cfg(test)]
pub fn paginate(blocks: &[Shaped], fh: f32) -> Vec<Vec<(usize, f32)>> {
    let refs: Vec<&Shaped> = blocks.iter().collect();
    let (mut places, mut states) = (Vec::new(), Vec::new());
    flow(&refs, fh, 0, Flow::default(), &mut places, &mut states);
    places
}

/// A page's number, centred in the foot margin: items in page coordinates.
pub fn page_number(fs: &mut FontSystem, style: &Style, page: usize) -> Vec<Item> {
    let (fx, _, fw, _) = style.frame();
    let (fx, fw) = (fx as f32, fw as f32);
    let look = Look { style, width: fw };
    let face = Face { size: (style.body.size * 0.85).max(6.0), ..style.body.clone() };
    let mut rows = shape(fs, &plain(&(page + 1).to_string()), &face, &look, fw, 1.2, QUIET, Align::Left);
    not_text(&mut rows);
    let Some(row) = rows.into_iter().next() else { return Vec::new() };
    let w: f32 = row.items.iter().map(|it| if let Item::Glyphs(r) = it { r.glyphs.iter().map(|g| g.advance).sum() } else { 0.0 }).sum();
    let foot = style.page.1 as f32 - style.margins[2] as f32 / 2.0 - face.size as f32;
    row.items.into_iter().map(|it| it.shifted(fx + (fw - w) / 2.0, foot)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows of fixed heights, as if shaped, for pagination alone.
    fn block(heights: &[f32], keep: bool) -> Shaped {
        Shaped { rows: heights.iter().map(|&h| Row { height: h, items: Vec::new() }).collect(), keep_with_next: keep, ..Default::default() }
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
    fn a_page_break_starts_a_page_unless_one_just_started() {
        let brk = Shaped { page_break: true, ..Default::default() };
        let pages = pages_of(&[block(&[10.0], false), brk.clone(), brk, block(&[10.0], false)], 100.0);
        assert_eq!(pages[3], vec![1]);
    }
}
