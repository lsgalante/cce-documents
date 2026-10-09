//! A page's text as the engine reads it, and what selection does with it.
//!
//! Pure geometry: no PDFium here. The engine hands over every character of a
//! page with its box in **display points** — the page as it is shown before
//! the user rotates it (the page's own /Rotate applied, origin top-left, y
//! down), the same space `PageSize` measures — and this module answers the
//! pointer's questions in that space.
//!
//! A selection end is a **caret position**: a boundary between characters,
//! `0..=chars.len()`, so a press in the right half of a glyph lands after it.
//! Characters PDFium generates (the spaces and line breaks it infers between
//! runs) come with empty boxes: they are copied, never hit or highlighted.

use std::cmp::Ordering;

/// A rect in display points, `x0 <= x1`, `y0 <= y1` (y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl PtRect {
    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn height(&self) -> f64 {
        self.y1 - self.y0
    }

    fn union(&self, o: &PtRect) -> PtRect {
        PtRect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

    /// Whether two boxes sit on one line: they share at least half the
    /// shorter one's height.
    fn same_line(&self, o: &PtRect) -> bool {
        let overlap = self.y1.min(o.y1) - self.y0.max(o.y0);
        overlap > 0.5 * self.height().min(o.height())
    }

    /// Distance from a point to the rect (0 inside).
    fn distance(&self, x: f64, y: f64) -> f64 {
        let dx = (self.x0 - x).max(0.0).max(x - self.x1);
        let dy = (self.y0 - y).max(0.0).max(y - self.y1);
        dx.hypot(dy)
    }
}

#[derive(Debug, Clone)]
pub struct TextChar {
    pub ch: char,
    pub rect: PtRect,
}

/// Every character of one page, in the order PDFium reads them.
#[derive(Debug, Clone, Default)]
pub struct PageText {
    pub chars: Vec<TextChar>,
}

/// How far from the nearest glyph a press still starts a selection rather
/// than a pan, in points.
const GRAB_PT: f64 = 6.0;

impl PageText {
    /// The caret position nearest (x, y), and whether the point is close
    /// enough to text to count as pressing on it. None for a page with no
    /// text at all (a scan).
    pub fn caret_at(&self, x: f64, y: f64) -> Option<(usize, bool)> {
        let hit = |i: usize, r: &PtRect| if x < (r.x0 + r.x1) / 2.0 { i } else { i + 1 };
        // On a line through y: the glyph nearest in x.
        let on_line = self
            .chars
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.rect.is_empty() && c.rect.y0 <= y && y <= c.rect.y1)
            .min_by(|(_, a), (_, b)| cmp(a.rect.distance(x, y), b.rect.distance(x, y)));
        if let Some((i, c)) = on_line {
            return Some((hit(i, &c.rect), c.rect.distance(x, y) <= GRAB_PT));
        }
        // Between lines or off the text: the nearest glyph overall.
        let (i, c) = self
            .chars
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.rect.is_empty())
            .min_by(|(_, a), (_, b)| cmp(a.rect.distance(x, y), b.rect.distance(x, y)))?;
        Some((hit(i, &c.rect), c.rect.distance(x, y) <= GRAB_PT))
    }

    /// The highlight for caret positions `a..b`: one rect per run of
    /// characters on one line.
    pub fn rects(&self, a: usize, b: usize) -> Vec<PtRect> {
        let mut out: Vec<PtRect> = Vec::new();
        for c in self.chars.get(a.min(b)..b.min(self.chars.len())).into_iter().flatten() {
            if c.rect.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.same_line(&c.rect) && c.rect.x0 >= last.x0 - 1.0 => *last = last.union(&c.rect),
                _ => out.push(c.rect),
            }
        }
        out
    }

    /// The text of caret positions `a..b`, with PDFium's CRLF line breaks
    /// as plain newlines.
    pub fn text(&self, a: usize, b: usize) -> String {
        let s: String = self.chars.get(a.min(b)..b.min(self.chars.len())).into_iter().flatten().map(|c| c.ch).collect();
        s.replace("\r\n", "\n").replace('\r', "\n")
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }
}

fn cmp(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// One end of a selection: a page and a caret position on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mark {
    pub page: usize,
    pub pos: usize,
}

/// A selection made by dragging: where the press landed and where the
/// pointer is now, in either order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selection {
    pub anchor: Mark,
    pub head: Mark,
}

impl Selection {
    /// The ends in reading order.
    pub fn ordered(&self) -> (Mark, Mark) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The caret range this selection covers on `page` (whose text has
    /// `len` characters), or None when the page is outside it.
    pub fn range_on(&self, page: usize, len: usize) -> Option<(usize, usize)> {
        let (a, b) = self.ordered();
        if page < a.page || page > b.page {
            return None;
        }
        let lo = if page == a.page { a.pos } else { 0 };
        let hi = if page == b.page { b.pos.min(len) } else { len };
        (lo < hi).then_some((lo, hi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: f64, y0: f64, x1: f64, y1: f64) -> PtRect {
        PtRect { x0, y0, x1, y1 }
    }

    /// "ab" on one line, a generated CRLF, then "c" on the next line.
    fn page() -> PageText {
        let none = r(0.0, 0.0, 0.0, 0.0);
        PageText {
            chars: vec![
                TextChar { ch: 'a', rect: r(10.0, 10.0, 20.0, 22.0) },
                TextChar { ch: 'b', rect: r(20.0, 10.0, 30.0, 22.0) },
                TextChar { ch: '\r', rect: none },
                TextChar { ch: '\n', rect: none },
                TextChar { ch: 'c', rect: r(10.0, 30.0, 20.0, 42.0) },
            ],
        }
    }

    #[test]
    fn a_press_lands_before_or_after_a_glyph_by_its_half() {
        assert_eq!(page().caret_at(12.0, 15.0), Some((0, true)));
        assert_eq!(page().caret_at(18.0, 15.0), Some((1, true)));
        assert_eq!(page().caret_at(29.0, 15.0), Some((2, true)));
    }

    #[test]
    fn a_press_beside_a_line_takes_its_nearest_glyph_but_is_not_on_text() {
        // Far right of the first line: after 'b', too far to start a selection.
        assert_eq!(page().caret_at(80.0, 15.0), Some((2, false)));
        // Just past the end, within the grab distance.
        assert_eq!(page().caret_at(33.0, 15.0), Some((2, true)));
    }

    #[test]
    fn a_press_between_lines_takes_the_nearest_glyph() {
        assert_eq!(page().caret_at(12.0, 27.0).map(|(p, _)| p), Some(4));
    }

    #[test]
    fn a_page_without_text_has_no_caret() {
        assert_eq!(PageText::default().caret_at(1.0, 1.0), None);
    }

    #[test]
    fn highlights_merge_a_line_and_skip_generated_breaks() {
        assert_eq!(page().rects(0, 5), vec![r(10.0, 10.0, 30.0, 22.0), r(10.0, 30.0, 20.0, 42.0)]);
        assert_eq!(page().rects(1, 2), vec![r(20.0, 10.0, 30.0, 22.0)]);
    }

    #[test]
    fn copied_text_turns_crlf_into_newlines() {
        assert_eq!(page().text(0, 5), "ab\nc");
        assert_eq!(page().text(1, 2), "b");
    }

    #[test]
    fn a_selection_spans_pages_in_reading_order() {
        let s = Selection { anchor: Mark { page: 2, pos: 3 }, head: Mark { page: 0, pos: 5 } };
        assert_eq!(s.range_on(0, 10), Some((5, 10)));
        assert_eq!(s.range_on(1, 7), Some((0, 7)));
        assert_eq!(s.range_on(2, 10), Some((0, 3)));
        assert_eq!(s.range_on(3, 10), None);
    }
}
