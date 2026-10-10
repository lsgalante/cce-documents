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
//! **Text and source.** What the caret moves through are *units* of text:
//! a heading's, a paragraph's, a code block's, a footnote's, a caption's,
//! each cell of a table. A unit's text is its spans' text joined; glyphs
//! carry offsets into it (`Glyph::at`), its lines (`layout::Line`) say
//! where it is drawn, and its anchors map text offsets to bytes of the
//! file. The caret is a byte of the file; everything the person sees is
//! text. Markup lives between anchors, so moving and deleting step over
//! it, and typing lands inside it (typing on at the end of a bold word
//! stays bold). A footnote reference is an atom: one stop before it, one
//! after, and deleted whole.
//!
//! **Generated parts.** Footnotes are placed after the flow, at the foot of
//! the page that first refers to each (the flow keeps that room free). The
//! table of contents shows the pages headings land on, which its own
//! height can change: it is laid out again until its entries stop
//! changing (twice at most, in practice once).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use cce_ui::cosmic_text::FontSystem;

use super::layout::{self, Context, Fields, Flow, Item, Laid, Line, Look, OutlineEntry, Page, Shaped, TocEntry};
use super::md::{self, Block};
use super::style::Style;

/// What a unit of text is, as far as editing cares: Enter in a list item
/// starts another item, in code a new line, in a table cell goes down a
/// row, elsewhere starts a new paragraph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Heading(u8),
    Para { list: bool },
    Code,
    /// A table cell, by row (the header is row 0) and column.
    Cell { row: usize, col: usize },
    Note,
    Caption,
}

impl Kind {
    /// Text in the flow of the document (not a cell, a footnote or a
    /// caption).
    pub fn flows(self) -> bool {
        self.joins()
    }

    /// Text that flows: Backspace at its start may join it to the text
    /// before (not a cell, a footnote or a caption, which stand alone).
    fn joins(self) -> bool {
        matches!(self, Kind::Heading(_) | Kind::Para { .. } | Kind::Code)
    }
}

/// One block as laid out.
#[derive(Debug, Clone)]
pub struct BlockLay {
    /// Its bytes in the file.
    pub src: Range<usize>,
    pub key: u64,
    pub shaped: Arc<Shaped>,
    /// A picture's attributes; a table's column count.
    pub picture: Option<md::Picture>,
    pub columns: usize,
    pub toc: bool,
}

/// A run of text the caret moves through.
#[derive(Debug, Clone)]
pub struct Unit {
    pub block: usize,
    pub kind: Kind,
    /// Its bytes in the file (a cell's: between its pipes).
    pub src: Range<usize>,
    /// (offset in the text, byte of the file), ascending.
    pub anchors: Vec<(usize, usize)>,
    /// (text, file bytes) of each atom: text that stands for markup as a
    /// whole (a footnote reference's number).
    pub atoms: Vec<(Range<usize>, Range<usize>)>,
    /// Its text, spans joined (what `Glyph::at` indexes).
    pub text: String,
    /// Its lines: a range of its block's `shaped.lines`.
    pub lines: Range<usize>,
    /// It is on a page (an unreferenced footnote is not).
    pub live: bool,
}

impl Unit {
    /// The file byte of a text offset. At a boundary between anchors,
    /// `left` takes the end of the stretch before (the caret just after a
    /// bold word's last letter is inside the bold), else the start of the
    /// one after. An atom's end is past its markup.
    pub fn source_of(&self, t: usize, left: bool) -> usize {
        for (tr, sr) in &self.atoms {
            if t > tr.start && t <= tr.end {
                return if t == tr.end { sr.end } else { sr.start };
            }
        }
        let i = self.anchors.partition_point(|&(a, _)| a <= t);
        let i = if left && i > 1 && self.anchors[i - 1].0 == t { i - 2 } else { i.saturating_sub(1) };
        let (a, b) = self.anchors[i];
        // A stretch never runs past the atom after it.
        b + (t - a)
    }

    /// The text offset of a file byte: within a stretch, counted; in the
    /// markup between two, the start of the next; inside an atom, its end.
    pub fn text_of(&self, src: usize) -> usize {
        for (tr, sr) in &self.atoms {
            if src > sr.start && src <= sr.end {
                return tr.end;
            }
        }
        let i = self.anchors.partition_point(|&(_, b)| b <= src).saturating_sub(1);
        let (a, b) = self.anchors[i];
        let next = self.anchors.get(i + 1).map_or(self.text.len(), |&(n, _)| n);
        let next = self.atoms.iter().map(|(tr, _)| tr.start).filter(|&s| s >= a).min().map_or(next, |s| next.min(s));
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
    pub units: Vec<Unit>,
    /// Each block's rows: (page, top below the frame's top).
    pub places: Vec<Vec<(usize, f32)>>,
    /// The flow before each block, for resuming.
    pub states: Vec<Flow>,
    pub page_count: usize,
    /// What each page shows: (block, row) in drawing order.
    pub page_rows: Vec<Vec<(usize, usize)>>,
    pub outline: Vec<OutlineEntry>,
    pub words: usize,
    /// Pages with footnotes, and where above them the separator goes.
    pub seps: Vec<(usize, f32)>,
    /// Each footnote's height, by number (for resuming).
    note_h: Vec<f32>,
    /// The contents' entries, as last laid out.
    pub toc: Vec<TocEntry>,
    /// What the header and footer fields read, page and section aside.
    pub fields: Fields,
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

    fn line(&self, u: usize, li: usize) -> &Line {
        &self.blocks[self.units[u].block].shaped.lines[li]
    }

    fn lines(&self, u: usize) -> &[Line] {
        let unit = &self.units[u];
        &self.blocks[unit.block].shaped.lines[unit.lines.clone()]
    }

    /// A line's top on its page, in page points.
    fn line_top(&self, u: usize, li: usize) -> (usize, f32) {
        let l = self.line(u, li);
        let (page, y) = self.places[self.units[u].block][l.row];
        (page, self.frame.1 + y + l.dy)
    }

    fn live(&self) -> impl Iterator<Item = (usize, &Unit)> {
        self.units.iter().enumerate().filter(|(_, u)| u.live)
    }

    /// The unit holding a file byte. A byte between units (a blank line, a
    /// heading's `## `, a space the paragraph trims) belongs to the unit
    /// after it — the caret there is at that unit's start — and past the
    /// last unit, to the last.
    pub fn unit_at(&self, src: usize) -> Option<usize> {
        match self.live().filter(|(_, u)| u.src.start <= src).last() {
            Some((i, u)) if src > u.src.end => self.live().find(|(_, n)| n.src.start > src).map_or(Some(i), |(n, _)| Some(n)),
            Some((i, _)) => Some(i),
            None => self.live().next().map(|(i, _)| i),
        }
    }

    /// The line of unit `u` holding text offset `t` (an index into its
    /// block's lines): the last starting at or before it (the caret at a
    /// wrap belongs to the next line).
    fn line_of(&self, u: usize, t: usize) -> usize {
        let range = self.units[u].lines.clone();
        let lines = self.lines(u);
        range.start + lines.iter().rposition(|l| l.start <= t).unwrap_or(0)
    }

    /// The x of text offset `t` on a line: the start of the cluster there,
    /// or the end of the last one before it.
    fn x_in_line(&self, u: usize, li: usize, t: usize) -> f32 {
        let line = self.line(u, li);
        let mut x = None;
        for &(at, len, gx, adv) in &line.clusters {
            if at == t {
                return gx;
            }
            if at < t {
                x = Some(if t >= at + len { gx + adv } else { gx });
            }
        }
        x.unwrap_or_else(|| line.clusters.first().map_or(line.x, |c| c.2))
    }

    pub fn caret(&self, src: usize) -> Option<CaretBox> {
        let u = self.unit_at(src)?;
        let t = self.units[u].text_of(src);
        let li = self.line_of(u, t);
        let (page, y) = self.line_top(u, li);
        let h = self.line(u, li).h;
        Some(CaretBox { page, x: self.frame.0 + self.x_in_line(u, li, t), y, h })
    }

    /// The text offset nearest page x on a line.
    fn t_at_x(&self, u: usize, li: usize, x: f32) -> usize {
        // Lines are laid out from the text frame's left edge.
        let x = x - self.frame.0;
        let line = self.line(u, li);
        let mut best = (f32::MAX, line.start);
        for &(at, len, gx, adv) in &line.clusters {
            for (edge, t) in [(gx, at), (gx + adv, at + len)] {
                let d = (edge - x).abs();
                if d < best.0 {
                    best = (d, t);
                }
            }
        }
        self.snap(u, best.1)
    }

    /// A text offset off any atom's inside.
    fn snap(&self, u: usize, t: usize) -> usize {
        self.units[u].atoms.iter().find(|(tr, _)| t > tr.start && t < tr.end).map_or(t, |(tr, _)| tr.end)
    }

    /// The lines of live units on a page: (unit, line index).
    fn lines_on(&self, page: usize) -> Vec<(usize, usize)> {
        let Some(rows) = self.page_rows.get(page) else { return Vec::new() };
        let mut out = Vec::new();
        for (u, unit) in self.live() {
            for li in unit.lines.clone() {
                let row = self.blocks[unit.block].shaped.lines[li].row;
                if rows.contains(&(unit.block, row)) {
                    out.push((u, li));
                }
            }
        }
        out
    }

    /// The file byte under a point on a page (page points): the nearest
    /// line (down, then across: a table's cells sit side by side), then
    /// the nearest boundary on it.
    pub fn hit(&self, page: usize, x: f32, y: f32) -> Option<usize> {
        let fx = x - self.frame.0;
        let mut best: Option<(f32, usize, usize)> = None;
        for (u, li) in self.lines_on(page) {
            let (_, top) = self.line_top(u, li);
            let l = self.line(u, li);
            let dy = if y < top { top - y } else if y > top + l.h { y - top - l.h } else { 0.0 };
            let dx = if fx < l.x0 { l.x0 - fx } else if fx > l.x1 { fx - l.x1 } else { 0.0 };
            let d = dy + dx;
            if best.is_none_or(|(bd, ..)| d < bd) {
                best = Some((d, u, li));
            }
        }
        let (_, u, li) = best?;
        let t = self.t_at_x(u, li, x);
        Some(self.units[u].source_of(t, true))
    }

    /// The text offsets where the caret may stand in unit `u`: every
    /// character boundary (not just glyph starts: the space where a line
    /// wraps is drawn with no glyph, yet is a character to step over and
    /// delete on its own), but none inside an atom.
    fn stops(&self, u: usize) -> Vec<usize> {
        let unit = &self.units[u];
        let inside = |i: usize| unit.atoms.iter().any(|(tr, _)| i > tr.start && i < tr.end);
        unit.text.char_indices().map(|(i, _)| i).chain(std::iter::once(unit.text.len())).filter(|&i| !inside(i)).collect()
    }

    /// The caret one visible character on (or back), crossing into the
    /// next (previous) unit at an end.
    pub fn step(&self, src: usize, forward: bool) -> usize {
        let Some(u) = self.unit_at(src) else { return src };
        let unit = &self.units[u];
        let t = unit.text_of(src);
        let stops = self.stops(u);
        let i = stops.partition_point(|&s| s < t);
        if forward {
            match stops.get(i + usize::from(stops.get(i) == Some(&t))) {
                Some(&n) => unit.source_of(n, true),
                None => self.next_text(u).map_or(src, |n| self.units[n].source_of(0, false)),
            }
        } else if i > 0 {
            unit.source_of(stops[i - 1], false)
        } else {
            self.prev_text(u).map_or(src, |p| self.units[p].source_of(self.units[p].text.len(), true))
        }
    }

    pub fn next_text(&self, u: usize) -> Option<usize> {
        (u + 1..self.units.len()).find(|&i| self.units[i].live)
    }

    pub fn prev_text(&self, u: usize) -> Option<usize> {
        (0..u).rev().find(|&i| self.units[i].live)
    }

    /// The caret on the line above (below), at `goal_x`: within the block
    /// first (in a table, the cell above or below), else the nearest line
    /// of the block before (after). None at the document's first (last).
    pub fn vertical(&self, src: usize, goal_x: f32, down: bool) -> Option<usize> {
        let u = self.unit_at(src)?;
        let li = self.line_of(u, self.units[u].text_of(src));
        let b = self.units[u].block;
        let cur = self.line(u, li);
        let key = |l: &Line| (l.row, l.dy);
        let gx = goal_x - self.frame.0;
        let below = |l: &Line| {
            let (a, c) = (key(l), key(cur));
            if down {
                a.0 > c.0 || (a.0 == c.0 && a.1 > c.1 + 0.01)
            } else {
                a.0 < c.0 || (a.0 == c.0 && a.1 < c.1 - 0.01)
            }
        };
        // Lines of the live units of a block.
        let block_lines = |b: usize| -> Vec<(usize, usize)> {
            self.live().filter(|(_, n)| n.block == b).flat_map(|(n, unit)| unit.lines.clone().map(move |li| (n, li))).collect()
        };
        let pick = |cands: Vec<(usize, usize)>, nearest_first: bool| -> Option<(usize, usize)> {
            if cands.is_empty() {
                return None;
            }
            let lk = |&(n, li): &(usize, usize)| key(self.line(n, li));
            let within: Vec<(usize, usize)> = cands.iter().copied().filter(|&(n, li)| {
                let l = self.line(n, li);
                gx >= l.x0 && gx <= l.x1
            }).collect();
            let pool = if within.is_empty() { cands } else { within };
            let best = |a: &(usize, usize), b: &(usize, usize)| {
                let (ka, kb) = (lk(a), lk(b));
                (ka.0, ka.1).partial_cmp(&(kb.0, kb.1)).unwrap_or(std::cmp::Ordering::Equal)
            };
            if nearest_first == down {
                pool.into_iter().min_by(best)
            } else {
                pool.into_iter().max_by(best)
            }
        };
        let here: Vec<(usize, usize)> = block_lines(b).into_iter().filter(|&(n, l)| below(self.line(n, l))).collect();
        let (nu, nli) = match pick(here, true) {
            Some(t) => t,
            None => {
                let other = if down { self.next_text(self.units.iter().rposition(|x| x.block == b)?)? } else { self.prev_text(self.units.iter().position(|x| x.block == b)?)? };
                pick(block_lines(self.units[other].block), true)?
            }
        };
        Some(self.units[nu].source_of(self.t_at_x(nu, nli, goal_x), true))
    }

    /// The start or end of the caret's line.
    pub fn row_edge(&self, src: usize, end: bool) -> usize {
        let Some(u) = self.unit_at(src) else { return src };
        let li = self.line_of(u, self.units[u].text_of(src));
        let line = self.line(u, li);
        let t = if end { line.clusters.last().map_or(line.start, |&(at, len, ..)| at + len) } else { line.clusters.first().map_or(line.start, |c| c.0) };
        self.units[u].source_of(t, end)
    }

    /// The bytes Backspace (Delete) removes: the visible character before
    /// (after) the caret — a footnote reference whole — or at a unit's
    /// edge, what lies between it and the previous (next) unit: a picture,
    /// rule, page break or the contents there, else the break between the
    /// two, joining them. `file` is the source, to tell a break's markup
    /// (`## `, `- `) from plain spaces after it, which stay: a paragraph
    /// split before a space and joined again reads as it did. Table cells,
    /// footnotes and captions are never joined.
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
        let u = self.unit_at(src)?;
        let unit = &self.units[u];
        let t = unit.text_of(src);
        let stops = self.stops(u);
        if forward {
            if t < unit.text.len() {
                let next = stops.into_iter().find(|&s| s > t).unwrap_or(unit.text.len());
                return Some(unit.source_of(t, false)..unit.source_of(next, true));
            }
            if let Some(r) = self.between(u, true).filter(|_| unit.kind.joins()) {
                return Some(r);
            }
            let n = self.next_text(u)?;
            if !unit.kind.joins() || !self.units[n].kind.joins() {
                return None;
            }
            Some(join(unit.source_of(t, true), self.units[n].source_of(0, false)))
        } else {
            if t > 0 {
                let prev = stops.into_iter().filter(|&s| s < t).last().unwrap_or(0);
                return Some(unit.source_of(prev, false)..unit.source_of(t, true));
            }
            if let Some(r) = self.between(u, false).filter(|_| unit.kind.joins()) {
                return Some(r);
            }
            let p = self.prev_text(u)?;
            if !unit.kind.joins() || !self.units[p].kind.joins() {
                return None;
            }
            Some(join(self.units[p].source_of(self.units[p].text.len(), true), unit.source_of(0, false)))
        }
    }

    /// A block without text (picture, rule, page break, contents) right
    /// after (before) unit `u`'s block: its bytes up to the next block.
    fn between(&self, u: usize, after: bool) -> Option<Range<usize>> {
        let b = self.units[u].block;
        let nb = if after { b + 1 } else { b.checked_sub(1)? };
        let block = self.blocks.get(nb)?;
        // A picture with a caption has text: Backspace from the text after
        // it still takes the picture (the caption goes with it).
        let textless = !self.units.iter().any(|x| x.block == nb && x.kind != Kind::Caption) && block.shaped.note.is_none();
        if !textless {
            return None;
        }
        let end = self.blocks.get(nb + 1).map_or(block.src.end, |n| n.src.start);
        Some(block.src.start..end.max(block.src.end))
    }

    /// Selection highlight between two file bytes: (page, x, y, w, h) per
    /// line it covers, in page points.
    pub fn selection(&self, a: usize, z: usize) -> Vec<(usize, f32, f32, f32, f32)> {
        let (a, z) = (a.min(z), a.max(z));
        let mut out = Vec::new();
        let (Some(ua), Some(uz)) = (self.unit_at(a), self.unit_at(z)) else { return out };
        for u in ua..=uz {
            let unit = &self.units[u];
            if !unit.live {
                continue;
            }
            let ta = if u == ua { unit.text_of(a) } else { 0 };
            let tz = if u == uz { unit.text_of(z) } else { unit.text.len() };
            for li in unit.lines.clone() {
                let line = self.line(u, li);
                let r0 = line.clusters.first().map_or(line.start, |c| c.0);
                let r1 = line.clusters.last().map_or(line.start, |c| c.0 + c.1);
                let (s, e) = (ta.max(r0), tz.min(r1));
                if s >= e && !(s == e && u < uz && r1 == unit.text.len() && ta <= r1) {
                    continue;
                }
                let x0 = self.x_in_line(u, li, s);
                let mut x1 = self.x_in_line(u, li, e);
                if e == r1 && (u < uz || tz > r1) {
                    // Running on past the line: a little past its end.
                    x1 += 4.0;
                }
                let (page, y) = self.line_top(u, li);
                out.push((page, self.frame.0 + x0, y, (x1 - x0).max(1.0), line.h));
            }
        }
        out
    }

    /// The visible text between two file bytes: units on lines of their
    /// own, a blank line between paragraphs; a table's cells by tabs and
    /// lines (for copying).
    pub fn text_between(&self, a: usize, z: usize) -> String {
        let (a, z) = (a.min(z), a.max(z));
        let (Some(ua), Some(uz)) = (self.unit_at(a), self.unit_at(z)) else { return String::new() };
        let mut out = String::new();
        let mut last: Option<&Unit> = None;
        for u in ua..=uz {
            let unit = &self.units[u];
            if !unit.live {
                continue;
            }
            if let Some(prev) = last {
                out.push_str(match (prev.kind, unit.kind) {
                    (Kind::Cell { row: r0, .. }, Kind::Cell { row: r1, .. }) if prev.block == unit.block => {
                        if r0 == r1 {
                            "\t"
                        } else {
                            "\n"
                        }
                    }
                    _ => "\n\n",
                });
            }
            let ta = if u == ua { unit.text_of(a) } else { 0 };
            let tz = if u == uz { unit.text_of(z) } else { unit.text.len() };
            out.push_str(&unit.text[ta.min(tz)..tz]);
            last = Some(unit);
        }
        out
    }

    /// Every match of `query` (ignoring case), as file byte ranges.
    pub fn find(&self, query: &str) -> Vec<(usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
        let q = fold(query);
        let mut out = Vec::new();
        for (_, unit) in self.live() {
            // Case folding can change byte lengths; match on folded text
            // through a map back to the original's offsets.
            let mut folded = String::new();
            let mut back = Vec::new();
            for (i, c) in unit.text.char_indices() {
                for f in c.to_lowercase() {
                    for _ in 0..f.len_utf8() {
                        back.push(i);
                    }
                    folded.push(f);
                }
            }
            back.push(unit.text.len());
            let mut from = 0;
            while let Some(i) = folded[from..].find(&q) {
                let (s, e) = (back[from + i], back[from + i + q.len()]);
                let e = if e <= s { unit.text[s..].chars().next().map_or(s, |c| s + c.len_utf8()) } else { e };
                out.push((unit.source_of(s, false), unit.source_of(e, true)));
                from += i + q.len().max(1);
            }
        }
        out
    }

    /// Footnote numbers, header and footer: what a page has besides its
    /// rows, in page points.
    pub fn extras(&self, fs: &mut FontSystem, page: usize) -> Vec<Item> {
        let mut items = Vec::new();
        if let Some(&(_, y)) = self.seps.iter().find(|(p, _)| *p == page) {
            items.push(layout::note_rule(self.frame, y));
        }
        items.extend(layout::furniture(fs, &self.style, page, &self.fields_for(page)));
        items
    }

    /// The fields on page `page`: its number, and the section it is in
    /// (the last top-level heading on or before it).
    pub fn fields_for(&self, page: usize) -> Fields {
        let top = self.outline.iter().map(|e| e.level).min().unwrap_or(1);
        let section = self.outline.iter().filter(|e| e.level == top && e.page <= page).last().map(|e| e.title.clone()).unwrap_or_default();
        Fields { section, page: page + 1, pages: self.page_count, ..self.fields.clone() }
    }

    /// Every page's items, for the PDF.
    pub fn laid(&self, fs: &mut FontSystem) -> Laid {
        let (fx, fy, ..) = self.frame;
        let mut pages = vec![Page::default(); self.page_count];
        for (p, page) in pages.iter_mut().enumerate() {
            page.items = self.page_items(p, fx, fy);
            page.items.extend(self.extras(fs, p));
        }
        Laid { size: self.size(), pages, outline: self.outline.clone() }
    }

    /// One page's items in page coordinates (without its extras).
    pub fn page_items(&self, page: usize, fx: f32, fy: f32) -> Vec<Item> {
        let mut items = Vec::new();
        for &(b, r) in self.page_rows.get(page).map(|v| v.as_slice()).unwrap_or(&[]) {
            let (_, y) = self.places[b][r];
            items.extend(self.blocks[b].shaped.rows[r].items.iter().cloned().map(|i| i.shifted(fx, fy + y)));
        }
        items
    }

    /// A picture on a page: its block and its rect (page points).
    pub fn pictures_on(&self, page: usize) -> Vec<(usize, (f32, f32, f32, f32))> {
        let (fx, fy, ..) = self.frame;
        let mut out = Vec::new();
        for &(b, r) in self.page_rows.get(page).map(|v| v.as_slice()).unwrap_or(&[]) {
            if self.blocks[b].picture.is_none() {
                continue;
            }
            let (_, y) = self.places[b][r];
            for item in &self.blocks[b].shaped.rows[r].items {
                if let Item::Image { x, y: iy, w, h, .. } = item {
                    out.push((b, (fx + x, fy + y + iy, *w, *h)));
                }
            }
        }
        out
    }

    /// A contents entry under a point: the page and y it leads to.
    pub fn goto_at(&self, page: usize, x: f32, y: f32) -> Option<(usize, f32)> {
        self.page_items(page, self.frame.0, self.frame.1).into_iter().find_map(|i| match i {
            Item::GoTo { x: gx, y: gy, w, h, page, top } if x >= gx && x <= gx + w && y >= gy && y <= gy + h => Some((page, top)),
            _ => None,
        })
    }

    /// The unit of a table cell.
    pub fn cell(&self, block: usize, row: usize, col: usize) -> Option<usize> {
        self.units.iter().position(|u| u.block == block && u.kind == Kind::Cell { row, col })
    }
}

/// What shapes a block: its kind, text and looks, its nesting and context
/// — never its position in the file.
fn shape_key(block: &Block, style_key: u64, cx: &Context) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    style_key.hash(&mut h);
    let spans = |h: &mut std::collections::hash_map::DefaultHasher, spans: &[md::Span]| {
        for s in spans {
            (s.text.as_str(), s.bold, s.italic, s.mono, s.strike, s.link.is_some(), s.note).hash(h);
        }
    };
    let nest = |h: &mut std::collections::hash_map::DefaultHasher, n: &md::Nest| (n.quote, n.list, &n.marker).hash(h);
    match block {
        Block::Heading { level, spans: s } => {
            (0u8, level).hash(&mut h);
            spans(&mut h, s);
        }
        Block::Para { spans: s, nest: n } => {
            (1u8, cx.indent).hash(&mut h);
            spans(&mut h, s);
            nest(&mut h, n);
        }
        Block::Code { text, nest: n } => {
            (2u8, text).hash(&mut h);
            nest(&mut h, n);
        }
        Block::Image { src, alt, picture } => {
            (3u8, src, format!("{:?}{:?}", picture.width, picture.align)).hash(&mut h);
            spans(&mut h, alt);
        }
        Block::Table { aligns, rows, head } => {
            (6u8, aligns, head).hash(&mut h);
            for row in rows {
                row.len().hash(&mut h);
                for c in row {
                    spans(&mut h, &c.spans);
                }
            }
        }
        Block::Note { number, spans: s, .. } => {
            (7u8, number).hash(&mut h);
            spans(&mut h, s);
        }
        Block::Toc => (8u8, cx.toc).hash(&mut h),
        Block::Rule => 4u8.hash(&mut h),
        Block::PageBreak => 5u8.hash(&mut h),
    }
    h.finish()
}

/// Text and anchors (and atoms) of spans.
fn spans_text(spans: &[md::Span], fallback: usize) -> (String, Vec<(usize, usize)>, Vec<(Range<usize>, Range<usize>)>) {
    let mut text = String::new();
    let mut anchors = Vec::new();
    let mut atoms = Vec::new();
    for s in spans {
        for &(t, b) in &s.src {
            anchors.push((text.len() + t, b));
        }
        if let Some(a) = &s.atom {
            atoms.push((text.len()..text.len() + s.text.len(), a.clone()));
        }
        text.push_str(&s.text);
    }
    if anchors.is_empty() && !text.is_empty() {
        anchors.push((0, fallback));
    }
    (text, anchors, atoms)
}

/// A block's units: (kind, source, text, anchors, atoms), in order.
type UnitParts = (Kind, Range<usize>, String, Vec<(usize, usize)>, Vec<(Range<usize>, Range<usize>)>);

fn units_of(block: &Block, src: &Range<usize>, file: &str) -> Vec<UnitParts> {
    let one = |kind, spans: &[md::Span]| {
        let (text, anchors, atoms) = spans_text(spans, src.start);
        if anchors.is_empty() {
            Vec::new()
        } else {
            vec![(kind, src.clone(), text, anchors, atoms)]
        }
    };
    match block {
        Block::Heading { level, spans } => one(Kind::Heading(*level), spans),
        Block::Para { spans, nest } => one(Kind::Para { list: nest.list > 0 }, spans),
        Block::Note { spans, .. } => one(Kind::Note, spans),
        Block::Image { alt, .. } => one(Kind::Caption, alt),
        Block::Code { text, .. } => {
            // The code's lines sit in the block's source after the opening
            // fence (or as-is, indented code aside).
            let slice = file.get(src.clone()).unwrap_or("");
            let first = text.lines().next().unwrap_or("");
            let at = if first.is_empty() { slice.find('\n').map_or(0, |i| i + 1) } else { slice.find(first).unwrap_or(0) };
            vec![(Kind::Code, src.clone(), text.clone(), vec![(0, src.start + at)], Vec::new())]
        }
        Block::Table { rows, .. } => rows
            .iter()
            .enumerate()
            .flat_map(|(r, row)| {
                row.iter().enumerate().map(move |(c, cell)| {
                    let (text, anchors, atoms) = spans_text(&cell.spans, cell.src.start);
                    (Kind::Cell { row: r, col: c }, cell.src.clone(), text, anchors, atoms)
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A plain paragraph: one the style's first-line indent may apply to.
fn plain_para(block: &Block) -> bool {
    matches!(block, Block::Para { nest, .. } if nest.quote == 0 && nest.list == 0 && nest.marker.is_none())
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

/// The contents' entries a layout's outline makes.
fn toc_entries(outline: &[OutlineEntry]) -> Vec<TocEntry> {
    outline.iter().map(|e| TocEntry { level: e.level, title: e.title.clone(), page: e.page, top: e.y.round() as u32 }).collect()
}

impl Layouter {
    fn shaped(&mut self, fs: &mut FontSystem, block: &Block, key: u64, look: &Look, base: &Path, cx: Context) -> Arc<Shaped> {
        match self.cache.get(&key) {
            Some(s) => Arc::clone(s),
            None => {
                self.shaped_last += 1;
                let s = Arc::new(layout::shape_block(fs, block, look, base, cx));
                self.cache.insert(key, Arc::clone(&s));
                s
            }
        }
    }

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
        let prev = self.prev.clone().filter(|p| *p.style == style);
        let mut toc = prev.as_ref().map(|p| p.toc.clone()).unwrap_or_default();

        // Which paragraphs are indented: a plain one right after another
        // (footnotes and page furniture between them aside).
        let mut indent = vec![false; located.len()];
        if style.indent > 0.0 {
            let mut last_plain = false;
            for (i, (block, _)) in located.iter().enumerate() {
                if matches!(block, Block::Note { .. }) {
                    continue;
                }
                indent[i] = last_plain && plain_para(block);
                last_plain = plain_para(block);
            }
        }

        let mut blocks = Vec::with_capacity(located.len());
        let mut units = Vec::new();
        let mut used_keys = std::collections::HashSet::new();
        for (i, (block, src)) in located.iter().enumerate() {
            let cx = Context { indent: indent[i], toc: &toc };
            let key = shape_key(block, style_key, &cx);
            used_keys.insert(key);
            let shaped = self.shaped(fs, block, key, &look, base, cx);
            for (index, (kind, usrc, text, anchors, atoms)) in units_of(block, src, file).into_iter().enumerate() {
                let lines = {
                    let from = shaped.lines.iter().position(|l| l.unit == index).unwrap_or(shaped.lines.len());
                    let to = from + shaped.lines[from..].iter().take_while(|l| l.unit == index).count();
                    from..to
                };
                units.push(Unit { block: i, kind, src: usrc, anchors, atoms, text, live: !lines.is_empty(), lines });
            }
            let (picture, columns) = match block {
                Block::Image { picture, .. } => (Some(picture.clone()), 0),
                Block::Table { rows, aligns, .. } => (None, rows.iter().map(|r| r.len()).max().unwrap_or(0).max(aligns.len())),
                _ => (None, 0),
            };
            blocks.push(BlockLay { src: src.clone(), key, shaped, picture, columns, toc: matches!(block, Block::Toc) });
        }

        // Footnotes by number: their blocks and heights.
        let count = blocks.iter().filter_map(|b| b.shaped.note).max().unwrap_or(0) as usize;
        let mut notes: Vec<Option<usize>> = vec![None; count];
        let mut note_h = vec![0.0f32; count];
        for (i, b) in blocks.iter().enumerate() {
            if let Some(n) = b.shaped.note.filter(|&n| n > 0) {
                if notes[n as usize - 1].is_none() {
                    notes[n as usize - 1] = Some(i);
                    note_h[n as usize - 1] = b.shaped.rows.iter().map(|r| r.height).sum();
                }
            }
        }

        // Resume pagination where the shapes first differ — or at the
        // heading just before, which kept with the old shape; earlier if a
        // footnote changed height, from where it is first referred to.
        let mut from = match &prev {
            Some(p) => blocks.iter().zip(&p.blocks).position(|(a, b)| a.key != b.key).unwrap_or(blocks.len().min(p.blocks.len())),
            None => 0,
        };
        if let Some(p) = &prev {
            if p.note_h != note_h {
                let changed = (0..note_h.len().max(p.note_h.len())).find(|&i| note_h.get(i) != p.note_h.get(i)).map_or(0, |i| i as u32 + 1);
                if let Some(first) = blocks.iter().position(|b| b.shaped.rows.iter().any(|r| r.notes.iter().any(|&n| n >= changed))) {
                    from = from.min(first);
                }
            }
        }
        let back = |mut from: usize, blocks: &[BlockLay]| {
            while from > 0 && blocks[from - 1].shaped.keep_with_next {
                from -= 1;
            }
            from
        };
        from = back(from, &blocks);
        let (mut places, mut states) = match &prev {
            Some(p) => (p.places[..from.min(p.places.len())].to_vec(), p.states[..from.min(p.states.len())].to_vec()),
            None => (Vec::new(), Vec::new()),
        };
        let start = match &prev {
            Some(p) if from < p.states.len() => p.states[from],
            Some(p) if from > 0 => resume_after(p, from),
            _ => Flow::default(),
        };
        let mut start = start;
        let mut seps;
        let mut outline;
        // The contents shows where headings land, and its height moves
        // them: lay out until its entries hold still.
        let mut rounds = 0;
        loop {
            {
                let refs: Vec<&Shaped> = blocks.iter().map(|b| b.shaped.as_ref()).collect();
                layout::flow(&refs, fh, from, start, &mut places, &mut states, &note_h);
                seps = layout::place_notes(&refs, fh, &mut places, &notes);
            }
            outline = blocks
                .iter()
                .zip(&places)
                .filter_map(|(b, pl)| {
                    let (level, title) = b.shaped.outline.clone()?;
                    let &(page, y) = pl.first()?;
                    Some(OutlineEntry { level, title, page, y: fy + y })
                })
                .collect::<Vec<_>>();
            let entries = toc_entries(&outline);
            rounds += 1;
            let Some(t) = blocks.iter().position(|b| b.toc) else { break };
            if entries == toc || rounds > 3 {
                break;
            }
            toc = entries;
            for (i, (block, _)) in located.iter().enumerate().filter(|(_, (b, _))| matches!(b, Block::Toc)) {
                let cx = Context { indent: false, toc: &toc };
                let key = shape_key(block, style_key, &cx);
                used_keys.insert(key);
                blocks[i].shaped = self.shaped(fs, block, key, &look, base, cx);
                blocks[i].key = key;
            }
            from = back(t, &blocks);
            start = states[from];
        }
        // Forget shapes nothing uses any more.
        self.cache.retain(|k, _| used_keys.contains(k));

        let page_count = places.iter().flatten().map(|&(p, _)| p + 1).max().unwrap_or(1);
        let mut page_rows = vec![Vec::new(); page_count];
        for (b, rows) in places.iter().enumerate() {
            for (r, &(p, _)) in rows.iter().enumerate() {
                page_rows[p].push((b, r));
            }
        }
        for u in &mut units {
            u.live = u.live && !places[u.block].is_empty();
        }
        let words = units.iter().filter(|u| u.live).map(|u| u.text.split_whitespace().count()).sum();
        let meta = |k: &str| front.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let title = Some(meta("title")).filter(|t| !t.is_empty()).or_else(|| outline.iter().find(|e| e.level == 1).map(|e| e.title.clone())).unwrap_or_default();
        let fields = Fields { title, author: meta("author"), date: meta("date"), ..Default::default() };
        let doc = Arc::new(Doc {
            style: Arc::new(style),
            frame: (fx, fy, fw, fh),
            blocks,
            units,
            places,
            states,
            page_count,
            page_rows,
            outline,
            words,
            seps,
            note_h,
            toc,
            fields,
        });

        let changes = match &self.prev {
            Some(p) if p.page_count == doc.page_count && p.size() == doc.size() => {
                let sig = |d: &Doc, page: usize| -> (Vec<(u64, usize, u32)>, u64) {
                    let rows = d.page_rows[page].iter().map(|&(b, r)| (d.blocks[b].key, r, d.places[b][r].1.to_bits())).collect();
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    d.fields_for(page).hash(&mut h);
                    d.seps.iter().find(|(p, _)| *p == page).map(|(_, y)| y.to_bits()).hash(&mut h);
                    (rows, h.finish())
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
    // The last block before `from` that the flow placed.
    let Some(last) = (0..from).rev().find(|&b| p.blocks[b].shaped.note.is_none() && !p.places[b].is_empty()) else {
        return p.states.get(from - 1).copied().unwrap_or_default();
    };
    let mut f = p.states[last];
    let block = &p.blocks[last].shaped;
    if let Some(&(page, y)) = p.places[last].last() {
        f.page = page;
        f.y = y + block.rows.last().map_or(0.0, |r| r.height);
        f.used = true;
        f.after = block.space_after;
    }
    // The page's footnotes and the last number seen, as the flow left them.
    f.foot = 0.0;
    for r in &block.rows {
        f.seen = r.notes.iter().copied().fold(f.seen, u32::max);
    }
    let page = f.page;
    let mut seen = std::collections::HashSet::new();
    for b in 0..from {
        for (r, row) in p.blocks[b].shaped.rows.iter().enumerate() {
            if p.blocks[b].shaped.note.is_some() || p.places[b].get(r).is_none_or(|&(pg, _)| pg != page) {
                for &n in &row.notes {
                    seen.insert(n);
                }
                continue;
            }
            for &n in &row.notes {
                if seen.insert(n) {
                    f.foot += p.note_h.get(n as usize - 1).copied().unwrap_or(0.0) + layout::NOTE_GAP;
                }
            }
        }
    }
    if f.foot > 0.0 {
        f.foot += layout::NOTE_SEP;
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
        let block = &doc.units[1];
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
        assert_eq!(doc.unit_at(gap), Some(1));
        assert_eq!(doc.units[1].text_of(gap), 0);
        // At the start of a wrapped line, Backspace removes the space the
        // line wrapped at, and nothing more.
        let b = &doc.units[1];
        let row1 = doc.blocks[1].shaped.lines[1].clusters[0].0;
        let src = b.source_of(row1, false);
        let r = doc.deletion(src, false, &file).unwrap();
        assert_eq!(&file[r], " ");
    }

    #[test]
    fn joining_keeps_the_space_a_split_left() {
        let file = "One two\n\n three.\n";
        let (_, doc, _) = layouter_doc(file);
        let start = doc.units[1].source_of(0, false);
        let r = doc.deletion(start, false, file).unwrap();
        assert_eq!(&file[r], "\n\n", "the space before \"three\" stays");
        // A heading's markup goes with the break.
        let file = "One two\n\n## Three\n";
        let (_, doc, _) = layouter_doc(file);
        let r = doc.deletion(doc.units[1].source_of(0, false), false, file).unwrap();
        assert_eq!(&file[r], "\n\n## ");
    }

    #[test]
    fn table_cells_take_the_caret() {
        let file = "Before.\n\n| Name | Qty |\n|:--|--:|\n| apple | 3 |\n|  | 12 |\n\nAfter.\n";
        let (_, doc, _) = layouter_doc(file);
        let cells: Vec<(Kind, &str)> = doc.units.iter().filter(|u| matches!(u.kind, Kind::Cell { .. })).map(|u| (u.kind, u.text.as_str())).collect();
        assert_eq!(cells.len(), 6);
        assert_eq!(cells[2], (Kind::Cell { row: 1, col: 0 }, "apple"));
        // The empty cell: the caret between its pipes, where typing goes.
        let empty = doc.units.iter().position(|u| u.kind == Kind::Cell { row: 2, col: 0 }).unwrap();
        let at = doc.units[empty].source_of(0, false);
        assert_eq!(&file[at - 2..at + 2], "|  |");
        let c = doc.caret(at).unwrap();
        // A press there finds that cell, not its neighbour on the row.
        assert_eq!(doc.hit(c.page, c.x + 1.0, c.y + c.h / 2.0), Some(at));
        // Down from "Qty" is "3"; right from "Name" crosses to "Qty".
        let qty = doc.units.iter().find(|u| u.text == "Qty").unwrap();
        let from = qty.source_of(1, false);
        let x = doc.caret(from).unwrap().x;
        let down = doc.vertical(from, x, true).unwrap();
        assert_eq!(doc.units[doc.unit_at(down).unwrap()].text, "3");
        let name_end = doc.units.iter().find(|u| u.text == "Name").unwrap().source_of(4, true);
        assert_eq!(doc.units[doc.unit_at(doc.step(name_end, true)).unwrap()].text, "Qty");
        // Backspace at a cell's start never joins it to the cell before.
        assert_eq!(doc.deletion(qty.source_of(0, false), false, file), None);
        // Down from the last row leaves the table for the paragraph.
        let twelve = doc.units.iter().find(|u| u.text == "12").unwrap().source_of(0, false);
        let after = doc.vertical(twelve, doc.frame.0, true).unwrap();
        assert_eq!(doc.units[doc.unit_at(after).unwrap()].text, "After.");
    }

    #[test]
    fn a_footnote_sits_at_the_foot_of_its_page_and_its_reference_is_one_stop() {
        let file = "# Title\n\nA claim[^src] worth checking.\n\n## Next\n\nMore text.\n\n[^src]: Where it comes from.\n";
        let (_, doc, _) = layouter_doc(file);
        let para = doc.units.iter().position(|u| u.text.starts_with("A claim")).unwrap();
        let unit = &doc.units[para];
        assert_eq!(unit.text, "A claim1 worth checking.");
        // The number is one stop: from before it, one step lands after the
        // whole `[^src]`, and Backspace there removes all of it.
        let before = unit.source_of(7, false);
        assert_eq!(before, file.find("[^src]").unwrap());
        let after = doc.step(before, true);
        assert_eq!(after, before + "[^src]".len());
        assert_eq!(doc.deletion(after, false, file), Some(before..after));
        // The footnote is on page 0, below the text, inside the frame.
        let note = doc.units.iter().find(|u| u.kind == Kind::Note).unwrap();
        assert_eq!(note.text, "Where it comes from.");
        let c = doc.caret(note.source_of(0, false)).unwrap();
        assert_eq!(c.page, 0);
        let last_text = doc.caret(doc.units.iter().find(|u| u.text == "More text.").unwrap().source_of(0, false)).unwrap();
        assert!(c.y > last_text.y + 100.0, "note at {}, text at {}", c.y, last_text.y);
        assert!(c.y + c.h <= doc.frame.1 + doc.frame.3 + 0.5);
        assert_eq!(doc.seps.len(), 1);
        // Stepping back from the note's start does not reach into it from
        // the text by a join.
        assert_eq!(doc.deletion(note.source_of(0, false), false, file), None);
    }

    #[test]
    fn the_contents_lists_the_pages_headings_land_on() {
        let mut file = String::from("# Book\n\n[TOC]\n\n");
        for ch in 1..=4 {
            file.push_str(&format!("# Chapter {ch}\n\n"));
            for i in 0..30 {
                file.push_str(&format!("Paragraph {i} of chapter {ch}, long enough to wrap across the width of the page at least once or twice.\n\n"));
            }
        }
        let (mut l, doc, mut fs) = layouter_doc(&file);
        assert_eq!(doc.toc, toc_entries(&doc.outline));
        assert!(doc.outline.iter().map(|e| e.page).collect::<std::collections::BTreeSet<_>>().len() >= 3);
        // Each entry leads to its heading.
        let toc_block = doc.blocks.iter().position(|b| b.toc).unwrap();
        let &(page, _) = doc.places[toc_block].first().unwrap();
        let gotos: Vec<(usize, f32)> = doc.page_items(page, doc.frame.0, doc.frame.1).into_iter().filter_map(|i| if let Item::GoTo { page, top, .. } = i { Some((page, top)) } else { None }).collect();
        assert_eq!(gotos.len(), doc.outline.len());
        assert_eq!(gotos[2].0, doc.outline[2].page);
        // Lengthen chapter 1 by a page: the contents follows.
        let at = file.find("# Chapter 2").unwrap();
        file.insert_str(at, &"Another paragraph that pushes the later chapters on, long enough to wrap twice over.\n\n".repeat(25));
        let (doc2, _) = l.relayout(&mut fs, &file, Path::new("."));
        let (_, fresh, _) = layouter_doc(&file);
        assert_eq!(fresh.places, doc2.places, "the same as laying out afresh");
        assert_eq!(doc2.toc, toc_entries(&doc2.outline));
        assert!(doc2.outline.last().unwrap().page > doc.outline.last().unwrap().page);
    }

    #[test]
    fn the_book_style_indents_following_paragraphs_and_heads_its_pages() {
        let file = "---\nstyle: book\ntitle: A Book\n---\n# One\n\nFirst paragraph, set full out.\n\nSecond paragraph, indented.\n\n- a list item, not indented\n\n# Two\n\nAnother.\n";
        let (_, doc, mut fs) = layouter_doc(file);
        let x_of = |text: &str| {
            let u = doc.units.iter().find(|u| u.text.starts_with(text)).unwrap();
            doc.caret(u.source_of(0, false)).unwrap().x - doc.frame.0
        };
        assert!(x_of("First").abs() < 0.5);
        assert!((x_of("Second") - doc.style.indent as f32).abs() < 0.5, "{}", x_of("Second"));
        // Chapter two starts a page, and its page is headed with its title.
        let two = doc.outline.iter().find(|e| e.title == "Two").unwrap();
        assert_eq!(two.page, 1);
        assert_eq!(doc.fields_for(1).section, "Two");
        assert_eq!(doc.fields.title, "A Book");
        // The first page has no header (first=false) but a footer number.
        let glyph_count = |items: &[Item]| items.iter().filter(|i| matches!(i, Item::Glyphs(_))).count();
        let first = doc.extras(&mut fs, 0);
        let second = doc.extras(&mut fs, 1);
        assert!(glyph_count(&first) >= 1 && glyph_count(&second) > glyph_count(&first));
    }

    #[test]
    fn a_picture_takes_its_width_and_place_and_its_caption_is_text() {
        let dir = std::env::temp_dir().join(format!("cce-documents-pic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbaImage::from_pixel(400, 200, image::Rgba([200, 30, 30, 255])).save(dir.join("red.png")).unwrap();
        let file = "Text.\n\n![A red box](red.png){width=50% align=right}\n\nMore.\n";
        let mut fs = cce_ui::create_font_system_with_system_fonts();
        let (doc, _) = Layouter::default().relayout(&mut fs, file, &dir);
        let b = doc.blocks.iter().position(|b| b.picture.is_some()).unwrap();
        let (_, (x, _, w, h)) = doc.pictures_on(0).into_iter().find(|(pb, _)| *pb == b).unwrap();
        assert!((w - doc.frame.2 * 0.5).abs() < 0.5 && (h - w / 2.0).abs() < 0.5);
        assert!((x + w - (doc.frame.0 + doc.frame.2)).abs() < 0.5, "flush right");
        let caption = doc.units.iter().find(|u| u.kind == Kind::Caption).unwrap();
        assert_eq!(caption.text, "A red box");
        assert!(doc.caret(caption.source_of(0, false)).is_some());
        // Backspace at the start of "More." takes the picture, caption and
        // all, and nothing of the text.
        let more = doc.units.iter().find(|u| u.text == "More.").unwrap().source_of(0, false);
        let r = doc.deletion(more, false, file).unwrap();
        assert_eq!(&file[r], "![A red box](red.png){width=50% align=right}\n\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_matches_ignore_case_and_map_to_the_file() {
        let file = "# Über\n\nThe **cat** sat; the Cat ran.\n";
        let (_, doc, _) = layouter_doc(file);
        let hits = doc.find("cat");
        assert_eq!(hits.len(), 2);
        assert_eq!(&file[hits[0].0..hits[0].1], "cat");
        assert_eq!(&file[hits[1].0..hits[1].1], "Cat");
        assert_eq!(doc.find("über").len(), 1);
    }
}
