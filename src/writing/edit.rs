//! The document being edited: an incremental layout, and the geometry the
//! caret and selection need.
//!
//! **Incremental.** Each edit hands `Layouter::relayout` the whole source.
//! Parsing it again is cheap; shaping is not, so shaped blocks are cached
//! by what shapes them (their text and looks, never their position in the
//! file), and only a block whose content changed is shaped again.
//! Pagination resumes at the first block whose shape changed (or the
//! heading before it, which keeps with it): placements depend only on what
//! came before. And only pages whose rows moved or changed are reported,
//! so only those are drawn again.
//!
//! **Text and source.** A block's text is its spans' text joined; glyphs
//! carry offsets into it (`Glyph::at`), and the block's anchors map those
//! to bytes of the file. The caret is a byte of the file; everything the
//! person sees is text. Markup lives between anchors, so moving and
//! deleting step over it, and typing lands inside it (typing on at the end
//! of a bold word stays bold).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use cce_ui::cosmic_text::FontSystem;

use super::layout::{self, Flow, Item, Laid, Look, OutlineEntry, Page, Shaped};
use super::md::{self, Block};
use super::style::Style;

/// What a block is, as far as editing cares: Enter in a list item starts
/// another item, in code a new line, elsewhere a new paragraph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Heading(u8),
    Para { list: bool },
    Code,
    Other,
}

/// One block as laid out.
#[derive(Debug, Clone)]
pub struct BlockLay {
    pub kind: Kind,
    /// Its bytes in the file.
    pub src: Range<usize>,
    /// (offset in the block's text, byte of the file), ascending. Empty for
    /// a block without text (a picture, a rule, a page break).
    pub anchors: Vec<(usize, usize)>,
    /// The block's text, spans joined (what `Glyph::at` indexes).
    pub text: String,
    pub key: u64,
    pub shaped: Arc<Shaped>,
}

impl BlockLay {
    pub fn has_text(&self) -> bool {
        !self.anchors.is_empty()
    }

    /// The file byte of a text offset. At a boundary between anchors,
    /// `left` takes the end of the stretch before (the caret just after a
    /// bold word's last letter is inside the bold), else the start of the
    /// one after.
    pub fn source_of(&self, t: usize, left: bool) -> usize {
        let i = self.anchors.partition_point(|&(a, _)| a <= t);
        let i = if left && i > 1 && self.anchors[i - 1].0 == t { i - 2 } else { i.saturating_sub(1) };
        let (a, b) = self.anchors[i];
        b + (t - a)
    }

    /// The text offset of a file byte: within a stretch, counted; in the
    /// markup between two, the start of the next.
    pub fn text_of(&self, src: usize) -> usize {
        let i = self.anchors.partition_point(|&(_, b)| b <= src).saturating_sub(1);
        let (a, b) = self.anchors[i];
        let next = self.anchors.get(i + 1).map_or(self.text.len(), |&(n, _)| n);
        (a + src.saturating_sub(b)).min(next).min(self.text.len())
    }
}

/// A typeset document, ready to draw and to edit.
#[derive(Debug, Clone)]
pub struct Doc {
    pub style: Arc<Style>,
    /// Text frame: left, top, width, height, in points.
    pub frame: (f32, f32, f32, f32),
    pub blocks: Vec<BlockLay>,
    /// Each block's rows: (page, top below the frame's top).
    pub places: Vec<Vec<(usize, f32)>>,
    /// The flow before each block, for resuming.
    pub states: Vec<Flow>,
    pub page_count: usize,
    /// What each page shows: (block, row) in drawing order.
    pub page_rows: Vec<Vec<(usize, usize)>>,
    pub outline: Vec<OutlineEntry>,
    pub words: usize,
}

/// A caret's place on a page: its page, x, top and height, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretBox {
    pub page: usize,
    pub x: f32,
    pub y: f32,
    pub h: f32,
}

impl Doc {
    pub fn size(&self) -> (f32, f32) {
        (self.style.page.0 as f32, self.style.page.1 as f32)
    }

    /// A row's top on its page, in page points.
    fn row_top(&self, b: usize, r: usize) -> (usize, f32) {
        let (page, y) = self.places[b][r];
        (page, self.frame.1 + y)
    }

    /// The text block holding a file byte. A byte between blocks (a blank
    /// line, a heading's `## `, a space the paragraph trims) belongs to the
    /// block after it — the caret there is at that block's start — and
    /// past the last block, to the last.
    pub fn block_at(&self, src: usize) -> Option<usize> {
        let texts = || self.blocks.iter().enumerate().filter(|(_, b)| b.has_text());
        match texts().filter(|(_, b)| b.src.start <= src).last() {
            Some((i, b)) if src > b.src.end => texts().find(|(_, n)| n.src.start > src).map_or(Some(i), |(n, _)| Some(n)),
            Some((i, _)) => Some(i),
            None => texts().next().map(|(i, _)| i),
        }
    }

    /// The row of block `b` holding text offset `t`: the last row starting
    /// at or before it (the caret at a wrap belongs to the next line).
    fn row_of(&self, b: usize, t: usize) -> usize {
        let rows = &self.blocks[b].shaped.rows;
        let mut best = 0;
        for (i, row) in rows.iter().enumerate() {
            if let Some((at, ..)) = row.clusters().next() {
                if at <= t {
                    best = i;
                }
            }
        }
        best
    }

    /// The x of text offset `t` in a row: the start of the cluster there,
    /// or the end of the last one before it.
    fn x_in_row(&self, b: usize, r: usize, t: usize) -> f32 {
        let row = &self.blocks[b].shaped.rows[r];
        let mut x = None;
        for (at, len, gx, adv) in row.clusters() {
            if at == t {
                return gx;
            }
            if at < t {
                x = Some(if t >= at + len { gx + adv } else { gx });
            }
        }
        x.unwrap_or_else(|| row.clusters().next().map_or(0.0, |c| c.2))
    }

    pub fn caret(&self, src: usize) -> Option<CaretBox> {
        let b = self.block_at(src)?;
        let t = self.blocks[b].text_of(src);
        let r = self.row_of(b, t);
        let (page, y) = self.row_top(b, r);
        let h = self.blocks[b].shaped.rows[r].height;
        Some(CaretBox { page, x: self.frame.0 + self.x_in_row(b, r, t), y, h })
    }

    /// The text offset nearest page x in a row.
    fn t_at_x(&self, b: usize, r: usize, x: f32) -> usize {
        // Rows are laid out from the text frame's left edge.
        let x = x - self.frame.0;
        let row = &self.blocks[b].shaped.rows[r];
        let mut best = (f32::MAX, 0);
        for (at, len, gx, adv) in row.clusters() {
            for (edge, t) in [(gx, at), (gx + adv, at + len)] {
                let d = (edge - x).abs();
                if d < best.0 {
                    best = (d, t);
                }
            }
        }
        best.1
    }

    /// The file byte under a point on a page (page points): the nearest
    /// text row, then the nearest boundary in it.
    pub fn hit(&self, page: usize, x: f32, y: f32) -> Option<usize> {
        let rows = self.page_rows.get(page)?;
        let mut best: Option<(f32, usize, usize)> = None;
        for &(b, r) in rows {
            if !self.blocks[b].has_text() || self.blocks[b].shaped.rows[r].clusters().next().is_none() {
                continue;
            }
            let (_, top) = self.row_top(b, r);
            let h = self.blocks[b].shaped.rows[r].height;
            let d = if y < top { top - y } else if y > top + h { y - top - h } else { 0.0 };
            if best.is_none_or(|(bd, ..)| d < bd) {
                best = Some((d, b, r));
            }
        }
        let (_, b, r) = best?;
        let t = self.t_at_x(b, r, x);
        Some(self.blocks[b].source_of(t, true))
    }

    /// The text offsets where the caret may stand in block `b`: every
    /// character boundary. (Not just glyph starts: the space where a line
    /// wraps is drawn with no glyph, yet is a character to step over and
    /// delete on its own.)
    fn stops(&self, b: usize) -> Vec<usize> {
        let text = &self.blocks[b].text;
        text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect()
    }

    /// The caret one visible character on (or back), crossing into the
    /// next (previous) text block at an end.
    pub fn step(&self, src: usize, forward: bool) -> usize {
        let Some(b) = self.block_at(src) else { return src };
        let t = self.blocks[b].text_of(src);
        let stops = self.stops(b);
        let i = stops.partition_point(|&s| s < t);
        if forward {
            match stops.get(i + usize::from(stops.get(i) == Some(&t))) {
                Some(&n) => self.blocks[b].source_of(n, true),
                None => self.next_text(b).map_or(src, |nb| self.blocks[nb].source_of(0, false)),
            }
        } else if i > 0 {
            self.blocks[b].source_of(stops[i - 1], false)
        } else {
            self.prev_text(b).map_or(src, |pb| self.blocks[pb].source_of(self.blocks[pb].text.len(), true))
        }
    }

    pub fn next_text(&self, b: usize) -> Option<usize> {
        (b + 1..self.blocks.len()).find(|&i| self.blocks[i].has_text())
    }

    pub fn prev_text(&self, b: usize) -> Option<usize> {
        (0..b).rev().find(|&i| self.blocks[i].has_text())
    }

    /// The caret on the row above (below), at `goal_x`; None at the
    /// document's first (last) row.
    pub fn vertical(&self, src: usize, goal_x: f32, down: bool) -> Option<usize> {
        let b = self.block_at(src)?;
        let r = self.row_of(b, self.blocks[b].text_of(src));
        let (nb, nr) = if down {
            if r + 1 < self.blocks[b].shaped.rows.len() {
                (b, r + 1)
            } else {
                let nb = self.next_text(b)?;
                (nb, 0)
            }
        } else if r > 0 {
            (b, r - 1)
        } else {
            let pb = self.prev_text(b)?;
            (pb, self.blocks[pb].shaped.rows.len().checked_sub(1)?)
        };
        Some(self.blocks[nb].source_of(self.t_at_x(nb, nr, goal_x), true))
    }

    /// The start or end of the caret's row.
    pub fn row_edge(&self, src: usize, end: bool) -> usize {
        let Some(b) = self.block_at(src) else { return src };
        let r = self.row_of(b, self.blocks[b].text_of(src));
        let row = &self.blocks[b].shaped.rows[r];
        let t = if end {
            row.clusters().last().map_or(0, |(at, len, ..)| at + len)
        } else {
            row.clusters().next().map_or(0, |c| c.0)
        };
        self.blocks[b].source_of(t, end)
    }

    /// The bytes Backspace (Delete) removes: the visible character before
    /// (after) the caret, or at a block's edge, the break between it and
    /// the previous (next) block's text — joining the two. `file` is the
    /// source, to tell a break's markup (`## `, `- `) from plain spaces
    /// after it, which stay: a paragraph split before a space and joined
    /// again reads as it did.
    pub fn deletion(&self, src: usize, forward: bool, file: &str) -> Option<Range<usize>> {
        let join = |a: usize, z: usize| -> Range<usize> {
            let gap = file.get(a..z).unwrap_or("");
            let tail = gap.rsplit('\n').next().unwrap_or("");
            if gap.contains('\n') && !tail.is_empty() && tail.chars().all(|c| c == ' ' || c == '\t') {
                a..z - tail.len()
            } else {
                a..z
            }
        };
        let b = self.block_at(src)?;
        let block = &self.blocks[b];
        let t = block.text_of(src);
        let stops = self.stops(b);
        if forward {
            if t < block.text.len() {
                let next = stops.into_iter().find(|&s| s > t).unwrap_or(block.text.len());
                let start = block.source_of(t, false);
                return Some(start..start + (next - t));
            }
            let nb = self.next_text(b)?;
            Some(join(block.source_of(t, true), self.blocks[nb].source_of(0, false)))
        } else {
            if t > 0 {
                let prev = stops.into_iter().filter(|&s| s < t).last().unwrap_or(0);
                let start = block.source_of(prev, false);
                return Some(start..start + (t - prev));
            }
            let pb = self.prev_text(b)?;
            Some(join(self.blocks[pb].source_of(self.blocks[pb].text.len(), true), block.source_of(0, false)))
        }
    }

    /// Selection highlight between two file bytes: (page, x, y, w, h) per
    /// row it covers, in page points.
    pub fn selection(&self, a: usize, z: usize) -> Vec<(usize, f32, f32, f32, f32)> {
        let (a, z) = (a.min(z), a.max(z));
        let mut out = Vec::new();
        let (Some(ba), Some(bz)) = (self.block_at(a), self.block_at(z)) else { return out };
        for b in ba..=bz {
            let block = &self.blocks[b];
            if !block.has_text() {
                continue;
            }
            let ta = if b == ba { block.text_of(a) } else { 0 };
            let tz = if b == bz { block.text_of(z) } else { block.text.len() };
            for (r, row) in block.shaped.rows.iter().enumerate() {
                let (Some(first), Some(last)) = (row.clusters().next(), row.clusters().last()) else { continue };
                let (r0, r1) = (first.0, last.0 + last.1);
                let (s, e) = (ta.max(r0), tz.min(r1));
                if s >= e && !(s == e && b < bz && r1 == block.text.len() && ta <= r1) {
                    continue;
                }
                let x0 = self.x_in_row(b, r, s);
                let mut x1 = self.x_in_row(b, r, e);
                if e == r1 && (b < bz || tz > r1) {
                    // Running on past the row: a little past its end.
                    x1 += 4.0;
                }
                let (page, y) = self.row_top(b, r);
                out.push((page, self.frame.0 + x0, y, (x1 - x0).max(1.0), row.height));
            }
        }
        out
    }

    /// The visible text between two file bytes, blocks on lines of their
    /// own (for copying).
    pub fn text_between(&self, a: usize, z: usize) -> String {
        let (a, z) = (a.min(z), a.max(z));
        let (Some(ba), Some(bz)) = (self.block_at(a), self.block_at(z)) else { return String::new() };
        let mut parts = Vec::new();
        for b in ba..=bz {
            let block = &self.blocks[b];
            if !block.has_text() {
                continue;
            }
            let ta = if b == ba { block.text_of(a) } else { 0 };
            let tz = if b == bz { block.text_of(z) } else { block.text.len() };
            parts.push(block.text[ta.min(tz)..tz].to_string());
        }
        parts.join("\n\n")
    }

    /// Every page's items, for the PDF.
    pub fn laid(&self, fs: &mut FontSystem) -> Laid {
        let (fx, fy, ..) = self.frame;
        let mut pages = vec![Page::default(); self.page_count];
        for (p, page) in pages.iter_mut().enumerate() {
            page.items = self.page_items(p, fx, fy);
            if self.style.page_numbers && self.page_count > 1 {
                page.items.extend(layout::page_number(fs, &self.style, p));
            }
        }
        Laid { size: self.size(), pages, outline: self.outline.clone() }
    }

    /// One page's items in page coordinates (without its number).
    pub fn page_items(&self, page: usize, fx: f32, fy: f32) -> Vec<Item> {
        let mut items = Vec::new();
        for &(b, r) in self.page_rows.get(page).map(|v| v.as_slice()).unwrap_or(&[]) {
            let (_, y) = self.places[b][r];
            items.extend(self.blocks[b].shaped.rows[r].items.iter().cloned().map(|i| i.shifted(fx, fy + y)));
        }
        items
    }
}

/// What shapes a block: its kind, text and looks, its nesting — never its
/// position in the file.
fn shape_key(block: &Block, style_key: u64) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    style_key.hash(&mut h);
    let spans = |h: &mut std::collections::hash_map::DefaultHasher, spans: &[md::Span]| {
        for s in spans {
            (s.text.as_str(), s.bold, s.italic, s.mono, s.strike, s.link.is_some()).hash(h);
        }
    };
    let nest = |h: &mut std::collections::hash_map::DefaultHasher, n: &md::Nest| (n.quote, n.list, &n.marker).hash(h);
    match block {
        Block::Heading { level, spans: s } => {
            (0u8, level).hash(&mut h);
            spans(&mut h, s);
        }
        Block::Para { spans: s, nest: n } => {
            1u8.hash(&mut h);
            spans(&mut h, s);
            nest(&mut h, n);
        }
        Block::Code { text, nest: n } => {
            (2u8, text).hash(&mut h);
            nest(&mut h, n);
        }
        Block::Image { src, alt } => (3u8, src, alt).hash(&mut h),
        Block::Rule => 4u8.hash(&mut h),
        Block::PageBreak => 5u8.hash(&mut h),
    }
    h.finish()
}

/// A block's text and anchors.
fn text_and_anchors(block: &Block, src: &Range<usize>, file: &str) -> (String, Vec<(usize, usize)>) {
    match block {
        Block::Heading { spans, .. } | Block::Para { spans, .. } => {
            let mut text = String::new();
            let mut anchors = Vec::new();
            for s in spans {
                for &(t, b) in &s.src {
                    anchors.push((text.len() + t, b));
                }
                text.push_str(&s.text);
            }
            if anchors.is_empty() && !text.is_empty() {
                anchors.push((0, src.start));
            }
            (text, anchors)
        }
        Block::Code { text, .. } => {
            // The code's lines sit in the block's source after the opening
            // fence (or as-is, indented code aside).
            let slice = file.get(src.clone()).unwrap_or("");
            let first = text.lines().next().unwrap_or("");
            let at = if first.is_empty() { slice.find('\n').map_or(0, |i| i + 1) } else { slice.find(first).unwrap_or(0) };
            (text.clone(), vec![(0, src.start + at)])
        }
        _ => (String::new(), Vec::new()),
    }
}

/// The cache and the last layout, kept between edits.
#[derive(Default)]
pub struct Layouter {
    cache: HashMap<u64, Arc<Shaped>>,
    prev: Option<Arc<Doc>>,
    /// Blocks shaped by the last `relayout` (for tests and timing).
    pub shaped_last: usize,
}

/// What `relayout` changed: the pages to draw again.
#[derive(Debug, Clone)]
pub struct Changes {
    pub pages: Vec<usize>,
    /// The page count changed (or the page size): everything moves.
    pub all: bool,
}

impl Layouter {
    pub fn relayout(&mut self, fs: &mut FontSystem, file: &str, base: &Path) -> (Arc<Doc>, Changes) {
        let (front, body) = md::split_front_matter(file);
        let style = Style::for_document(&front);
        let style_key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            format!("{style:?}").hash(&mut h);
            h.finish()
        };
        let located = md::parse_located(body, file.len() - body.len());
        let (fx, fy, fw, fh) = style.frame();
        let (fx, fy, fw, fh) = (fx as f32, fy as f32, fw as f32, fh as f32);
        let look = Look { style: &style, width: fw };
        self.shaped_last = 0;
        let mut blocks = Vec::with_capacity(located.len());
        let mut used_keys = std::collections::HashSet::new();
        for (block, src) in &located {
            let key = shape_key(block, style_key);
            used_keys.insert(key);
            let shaped = match self.cache.get(&key) {
                Some(s) => Arc::clone(s),
                None => {
                    self.shaped_last += 1;
                    let s = Arc::new(layout::shape_block(fs, block, &look, base));
                    self.cache.insert(key, Arc::clone(&s));
                    s
                }
            };
            let (text, anchors) = text_and_anchors(block, src, file);
            let kind = match block {
                Block::Heading { level, .. } => Kind::Heading(*level),
                Block::Para { nest, .. } => Kind::Para { list: nest.list > 0 },
                Block::Code { .. } => Kind::Code,
                _ => Kind::Other,
            };
            blocks.push(BlockLay { kind, src: src.clone(), anchors, text, key, shaped });
        }
        // Forget shapes nothing uses any more.
        self.cache.retain(|k, _| used_keys.contains(k));

        // Resume pagination where the shapes first differ — or at the
        // heading just before, which kept with the old shape.
        let prev = self.prev.as_ref().filter(|p| *p.style == style);
        let mut from = match prev {
            Some(p) => blocks.iter().zip(&p.blocks).position(|(a, b)| a.key != b.key).unwrap_or(blocks.len().min(p.blocks.len())),
            None => 0,
        };
        while from > 0 && blocks[from - 1].shaped.keep_with_next {
            from -= 1;
        }
        let (mut places, mut states) = match prev {
            Some(p) => (p.places[..from].to_vec(), p.states[..from].to_vec()),
            None => (Vec::new(), Vec::new()),
        };
        let start = match prev {
            Some(p) if from < p.states.len() => p.states[from],
            Some(p) if from > 0 => resume_after(p, from),
            _ => Flow::default(),
        };
        let refs: Vec<&Shaped> = blocks.iter().map(|b| b.shaped.as_ref()).collect();
        layout::flow(&refs, fh, from, start, &mut places, &mut states);

        let page_count = places.iter().flatten().map(|&(p, _)| p + 1).max().unwrap_or(1);
        let mut page_rows = vec![Vec::new(); page_count];
        for (b, rows) in places.iter().enumerate() {
            for (r, &(p, _)) in rows.iter().enumerate() {
                page_rows[p].push((b, r));
            }
        }
        let outline = blocks
            .iter()
            .zip(&places)
            .filter_map(|(b, pl)| {
                let (level, title) = b.shaped.outline.clone()?;
                let &(page, y) = pl.first()?;
                Some(OutlineEntry { level, title, page, y: fy + y })
            })
            .collect();
        let words = blocks.iter().map(|b| b.text.split_whitespace().count()).sum();
        let doc = Arc::new(Doc {
            style: Arc::new(style),
            frame: (fx, fy, fw, fh),
            blocks,
            places,
            states,
            page_count,
            page_rows,
            outline,
            words,
        });

        let changes = match &self.prev {
            Some(p) if p.page_count == doc.page_count && p.size() == doc.size() => {
                let sig = |d: &Doc, page: usize| -> Vec<(u64, usize, u32)> {
                    d.page_rows[page].iter().map(|&(b, r)| (d.blocks[b].key, r, d.places[b][r].1.to_bits())).collect()
                };
                Changes { pages: (0..doc.page_count).filter(|&pg| sig(p, pg) != sig(&doc, pg)).collect(), all: false }
            }
            _ => Changes { pages: (0..doc.page_count).collect(), all: true },
        };
        self.prev = Some(Arc::clone(&doc));
        (doc, changes)
    }
}

/// The flow just after the last block of a previous layout, for blocks
/// appended at the end.
fn resume_after(p: &Doc, from: usize) -> Flow {
    let last = from - 1;
    let mut f = p.states[last];
    let block = &p.blocks[last].shaped;
    if let Some(&(page, y)) = p.places[last].last() {
        f.page = page;
        f.y = y + block.rows.last().map_or(0.0, |r| r.height);
        f.used = true;
        f.after = block.space_after;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layouter_doc(text: &str) -> (Layouter, Arc<Doc>, FontSystem) {
        let mut fs = cce_ui::create_font_system_with_system_fonts();
        let mut l = Layouter::default();
        let (doc, _) = l.relayout(&mut fs, text, Path::new("."));
        (l, doc, fs)
    }

    #[test]
    fn caret_and_hit_agree_and_step_over_markup() {
        let file = "# Title\n\nSome **bold** text, `code` and *more*.\n";
        let (_, doc, _) = layouter_doc(file);
        let b = 1;
        let block = &doc.blocks[b];
        assert_eq!(block.text, "Some bold text, code and more.");
        // The caret before the first letter stands where the PDF draws it:
        // the frame's left edge (the left margin), not the page's.
        let first = doc.caret(block.source_of(0, false)).unwrap();
        assert!((first.x - doc.frame.0).abs() < 0.5, "caret at {}, frame at {}", first.x, doc.frame.0);
        // Every visible character's place hits back to it.
        for t in 0..block.text.len() {
            if !block.text.is_char_boundary(t) {
                continue;
            }
            let src = block.source_of(t, false);
            let c = doc.caret(src).unwrap();
            let back = doc.hit(c.page, c.x + 0.1, c.y + c.h / 2.0).unwrap();
            assert_eq!(block.text_of(back), t, "offset {t}");
        }
        // Stepping onto the start of "bold" stops before the "**" (typing
        // there is plain text, as the character to its left is); one more
        // step crosses the markup and lands after the "b".
        let before_bold = file.find("**bold").unwrap();
        let at_start = doc.step(before_bold - 1, true);
        assert_eq!(at_start, before_bold);
        assert_eq!(doc.step(at_start, true), file.find("bold").unwrap() + 1);
        // Backspace after the "b" of bold removes just the "b".
        let after_b = file.find("bold").unwrap() + 1;
        assert_eq!(doc.deletion(after_b, false, file), Some(after_b - 1..after_b));
        // Backspace at the paragraph's start joins it to the heading,
        // taking the blank line with it.
        let start = block.source_of(0, false);
        let r = doc.deletion(start, false, file).unwrap();
        assert_eq!(&file[r], "\n\n");
    }

    #[test]
    fn typing_in_a_long_document_reshapes_one_block_and_redraws_one_page() {
        let mut file = String::from("# A long document\n\n");
        for i in 0..600 {
            file.push_str(&format!("Paragraph {i}: a line of ordinary prose that wraps over a couple of lines on an A4 page, so the document runs to many pages.\n\n"));
            if i % 50 == 0 {
                file.push_str(&format!("## Section {i}\n\n"));
            }
        }
        let mut fs = cce_ui::create_font_system_with_system_fonts();
        let mut l = Layouter::default();
        let (first, _) = l.relayout(&mut fs, &file, Path::new("."));
        assert!(first.page_count >= 25, "{} pages", first.page_count);
        // Type a letter in the middle.
        let at = file.find("Paragraph 200:").unwrap() + 5;
        file.insert(at, 'x');
        let t0 = std::time::Instant::now();
        let (doc, changes) = l.relayout(&mut fs, &file, Path::new("."));
        let took = t0.elapsed();
        assert_eq!(l.shaped_last, 1, "only the edited paragraph is shaped again");
        assert!(!changes.all && changes.pages.len() == 1, "pages redrawn: {:?}", changes.pages);
        assert_eq!(doc.caret(at + 1).map(|c| c.page), changes.pages.first().copied());
        eprintln!("relayout after one keystroke in {} pages: {:?}", doc.page_count, took);
        assert!(took < std::time::Duration::from_millis(60), "relayout took {took:?}");
        // The same layout as typesetting from scratch.
        let (_, fresh, _) = layouter_doc(&file);
        assert_eq!(fresh.places, doc.places);
    }

    #[test]
    fn the_gap_between_blocks_belongs_to_the_next_and_a_wrap_space_deletes_alone() {
        let mut long = String::new();
        for _ in 0..12 {
            long.push_str("several words that will wrap ");
        }
        let file = format!("First paragraph.\n\n {long}end.\n");
        let (_, doc, _) = layouter_doc(&file);
        // The space the second paragraph trims is at its start.
        let gap = file.find("\n\n").unwrap() + 2;
        assert_eq!(doc.block_at(gap), Some(1));
        assert_eq!(doc.blocks[1].text_of(gap), 0);
        // At the start of a wrapped line, Backspace removes the space the
        // line wrapped at, and nothing more.
        let b = &doc.blocks[1];
        let row1 = b.shaped.rows[1].clusters().next().unwrap().0;
        let src = b.source_of(row1, false);
        let r = doc.deletion(src, false, &file).unwrap();
        assert_eq!(&file[r], " ");
    }

    #[test]
    fn joining_keeps_the_space_a_split_left() {
        let file = "One two\n\n three.\n";
        let (_, doc, _) = layouter_doc(file);
        let start = doc.blocks[1].source_of(0, false);
        let r = doc.deletion(start, false, file).unwrap();
        assert_eq!(&file[r], "\n\n", "the space before \"three\" stays");
        // A heading's markup goes with the break.
        let file = "One two\n\n## Three\n";
        let (_, doc, _) = layouter_doc(file);
        let r = doc.deletion(doc.blocks[1].source_of(0, false), false, file).unwrap();
        assert_eq!(&file[r], "\n\n## ");
    }
}
