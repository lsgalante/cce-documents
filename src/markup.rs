//! What the reader changes in a PDF: the annotations on a page as the app
//! sees them, the edits it asks the engine to make to a page, and the page
//! operations that rearrange the document.
//!
//! Pure data: rects are in display points (see `text`), the engine converts
//! them to PDF page space. An annotation is addressed by its index in the
//! page's /Annots array, which shifts when one is removed, so the app takes
//! a fresh list after every edit (the engine sends one).

use crate::text::PtRect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupKind {
    Highlight,
    Underline,
    StrikeOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text { read_only: bool },
    CheckBox { read_only: bool },
    Radio { read_only: bool },
    /// A combo box, list box, push button or signature: shown, not filled
    /// here yet.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotKind {
    Note,
    Markup(MarkupKind),
    Ink,
    Field(FieldKind),
    /// Links, popups and the rest: drawn by PDFium, never picked.
    Other,
}

#[derive(Debug, Clone)]
pub struct Annot {
    pub index: usize,
    pub kind: AnnotKind,
    pub rect: PtRect,
    /// A note's text.
    pub contents: String,
}

impl Annot {
    /// Whether a press here picks this annotation (fields and markup the
    /// reader made; not links or popups).
    pub fn pickable(&self) -> bool {
        !matches!(self.kind, AnnotKind::Other)
    }
}

/// The topmost pickable annotation under a display point. Later entries
/// draw over earlier ones, so the search runs backwards.
pub fn annot_at(annots: &[Annot], x: f64, y: f64) -> Option<&Annot> {
    // A thin underline or ink stroke is hard to hit exactly: give every
    // box a couple of points of slack.
    const SLACK: f64 = 2.0;
    annots.iter().rev().find(|a| {
        a.pickable() && x >= a.rect.x0 - SLACK && x <= a.rect.x1 + SLACK && y >= a.rect.y0 - SLACK && y <= a.rect.y1 + SLACK
    })
}

#[derive(Debug, Clone)]
pub enum Edit {
    /// Mark text: one rect per line run (`PageText::rects`).
    Markup { page: usize, kind: MarkupKind, rects: Vec<PtRect> },
    /// One freehand stroke, in display points.
    Ink { page: usize, points: Vec<(f64, f64)>, width: f64 },
    /// A sticky note at a point.
    Note { page: usize, at: (f64, f64), contents: String },
    /// A note's text.
    SetContents { page: usize, index: usize, contents: String },
    Delete { page: usize, index: usize },
    /// A text field's value, typed in as a reader would (so PDFium redraws
    /// its appearance).
    SetText { page: usize, index: usize, value: String },
    /// Click a check box or radio button.
    Toggle { page: usize, index: usize },
}

impl Edit {
    pub fn page(&self) -> usize {
        match self {
            Edit::Markup { page, .. }
            | Edit::Ink { page, .. }
            | Edit::Note { page, .. }
            | Edit::SetContents { page, .. }
            | Edit::Delete { page, .. }
            | Edit::SetText { page, .. }
            | Edit::Toggle { page, .. } => *page,
        }
    }
}

/// A change to the document's pages. Page indices are those before the
/// operation.
#[derive(Debug, Clone, PartialEq)]
pub enum PageOp {
    Delete { pages: Vec<usize> },
    /// Turn pages by quarter turns clockwise (negative: counter-clockwise),
    /// in the file: their /Rotate.
    Rotate { pages: Vec<usize>, quarter_turns: i32 },
    /// Move pages, in their order, into the gap before page `gap` (0..=page
    /// count; the count is the end).
    Move { pages: Vec<usize>, gap: usize },
    /// Insert every page of another PDF before page `at`.
    Insert { from: std::path::PathBuf, at: usize },
}

impl PageOp {
    /// Where the first moved or inserted page ends up, for the view to
    /// follow; None when the pages are gone.
    pub fn lands_at(&self) -> Option<usize> {
        match self {
            PageOp::Delete { .. } => None,
            PageOp::Rotate { pages, .. } => pages.iter().min().copied(),
            PageOp::Move { pages, gap } => Some(move_dest(pages, *gap)),
            PageOp::Insert { at, .. } => Some(*at),
        }
    }
}

/// The index the first of `pages` has after moving them into `gap`: the gap
/// counted without the pages being moved.
pub fn move_dest(pages: &[usize], gap: usize) -> usize {
    gap - pages.iter().filter(|&&p| p < gap).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annot(index: usize, kind: AnnotKind, x0: f64, y0: f64, x1: f64, y1: f64) -> Annot {
        Annot { index, kind, rect: PtRect { x0, y0, x1, y1 }, contents: String::new() }
    }

    #[test]
    fn the_topmost_pickable_annotation_wins() {
        let annots = [
            annot(0, AnnotKind::Markup(MarkupKind::Highlight), 0.0, 0.0, 100.0, 20.0),
            annot(1, AnnotKind::Note, 10.0, 0.0, 30.0, 20.0),
            annot(2, AnnotKind::Other, 0.0, 0.0, 200.0, 200.0),
        ];
        assert_eq!(annot_at(&annots, 15.0, 10.0).map(|a| a.index), Some(1));
        assert_eq!(annot_at(&annots, 50.0, 10.0).map(|a| a.index), Some(0));
        // A link covers everything, but is never picked.
        assert_eq!(annot_at(&annots, 150.0, 150.0).map(|a| a.index), None);
    }

    #[test]
    fn a_thin_mark_has_slack() {
        let annots = [annot(0, AnnotKind::Markup(MarkupKind::Underline), 0.0, 10.0, 100.0, 11.0)];
        assert!(annot_at(&annots, 50.0, 12.5).is_some());
        assert!(annot_at(&annots, 50.0, 14.0).is_none());
    }

    #[test]
    fn moving_pages_counts_the_gap_without_them() {
        // Pages 1 and 2 of 0..5 dropped before page 4 land at index 2.
        assert_eq!(move_dest(&[1, 2], 4), 2);
        // Dropped at the front.
        assert_eq!(move_dest(&[3], 0), 0);
        // Dropped at the end of five pages.
        assert_eq!(move_dest(&[0], 5), 4);
    }
}
