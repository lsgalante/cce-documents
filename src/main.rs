//! cce-documents — the document viewer, growing into a PDF editor and a
//! word processor in one app. Pictures moved out to cce-image.
//!
//! One continuous vertically-scrolled document: PDF pages rasterized lazily
//! per page and re-rendered at higher DPI as you zoom. View state is a zoom
//! factor (screen px per point) plus a scroll offset; pages are laid out in
//! points so zoom-at-pointer is an exact rescale.
//!
//! Pages come from PDFium when its library loads (`engine`), which also
//! gives text: drag across text to select it, Ctrl+C copies, Ctrl+A selects
//! the whole document, Ctrl+F searches. Without PDFium the poppler fallback
//! only shows pages (`doc::Backend`).
//!
//! With PDFium the page can be marked up and filled in, and saved as the
//! original bytes plus an incremental update (`engine`): select text and
//! press h / u / s to highlight, underline or strike it; d draws in ink, n
//! places a note, v is back to selecting; click a form field to fill it, a
//! note to edit it, any other mark to pick it (Delete removes it); Ctrl+Z
//! takes back the last change of any kind, Ctrl+Shift+Z puts it back;
//! Ctrl+S saves, Ctrl+Shift+S saves as. The toolbar along the bottom does
//! the same.
//!
//! t opens the page sidebar: thumbnails to click through and select (Shift
//! for a range, Ctrl to add one), drag to reorder; with pages selected,
//! Delete removes them and r / l turn them in the file. Its buttons insert
//! another PDF's pages (after the selection, or at the end: a merge), save
//! the selection as a PDF of its own, turn and delete.
//!
//! A Markdown file (`.md`) opens typeset onto pages by the writing engine
//! (`writing`): read-only here — edit it in any editor, and the pages follow
//! each save. Ctrl+E exports the PDF, Ctrl+P prints (any document).
//!
//! Keys: o open · +/- zoom · 0 fit · 1 actual size · r/l rotate ·
//! arrows/PageUp/PageDown/Home/End pages · Ctrl+F find · Enter/F3 next match
//! (Shift: previous) · Esc close an editor, find, the pick, the selection or
//! the tool, in that order · q quit (twice with unsaved changes).
//! Wheel scrolls, ctrl+wheel and pinch zoom at the pointer, drag pans (or
//! selects, when the press lands on text).

mod chrome;
mod doc;
mod engine;
mod markup;
mod poppler;
mod print;
mod text;
mod trim;
mod writing;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cce_ui::engine::{Application, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::line_edit::EditOutcome;
use cce_ui::widget::scroll_motion::{Bounds, ScrollMotion};
use cce_ui::widget::{ElementState, Key, KeyEvent, LineEdit, MouseButton, MouseScrollDelta, NamedKey, Position};

use std::collections::BTreeSet;

use chrome::{Action, Button, FieldLook};
use doc::{Backend, Document, PageStore, Rendered, PAGE_BUDGET, THUMB_BUDGET};
use markup::{annot_at, move_dest, Annot, AnnotKind, Edit, FieldKind, MarkupKind, PageOp};
use text::{Mark, PageText, PtRect, Selection};

/// Vertical gap between pages, in document units (so the layout scales
/// uniformly with zoom and anchored zooming stays exact).
const GAP_UNITS: f64 = 12.0;
/// Room around a fitted page: the root plate's inset on each side.
fn fit_margin() -> f64 {
    2.0 * cce_ui::layout::root_plate_inset() as f64
}
const WHEEL_SCROLL_PX: f64 = 48.0;
const KEY_SCROLL_PX: f64 = 80.0;
const ZOOM_MIN: f64 = 0.05;
const ZOOM_MAX: f64 = 16.0;
/// DPI steps pages are rendered at; bucketing keeps small zoom jitters from
/// re-rasterizing every page.
const DPI_BUCKETS: &[u32] = &[36, 48, 72, 96, 144, 192, 288, 384, 576];

/// The find bar's size and type, in logical px.
const FIND_W: f32 = 340.0;
const FIND_H: f32 = 30.0;
const FIND_FONT: f32 = 13.0;

const SELECTION: [f32; 4] = [0.25, 0.5, 1.0, 0.35];
const MATCH: [f32; 4] = [1.0, 0.85, 0.0, 0.40];
const MATCH_CURRENT: [f32; 4] = [1.0, 0.5, 0.0, 0.60];
const PICKED: [f32; 4] = [0.25, 0.5, 1.0, 0.9];
/// Ink while it is being drawn; the engine's ink color, as RGBA.
const INK: [f32; 4] = [200.0 / 255.0, 30.0 / 255.0, 60.0 / 255.0, 1.0];
/// A pen stroke's width, in points.
const INK_PT: f64 = 2.0;
const NOTE_EDITOR_W: f32 = 280.0;
/// The page sidebar: its width, a thumbnail's width, the room between
/// thumbnails (the page number sits in it), and the button rows above.
const SIDEBAR_W: f32 = 176.0;
const THUMB_W: f64 = 120.0;
const THUMB_GAP: f64 = 26.0;
const SIDEBAR_HEADER: f32 = 76.0;
/// How far a press on a thumbnail moves before it is a drag.
const DRAG_START: f64 = 6.0;
const NOTE_EDITOR_H: f32 = 30.0;

#[derive(Debug, Clone)]
enum Message {
    /// `slot`: which `PageStore` asked (the page view or the thumbnails).
    Page { slot: u8, generation: u64, page: usize, result: Option<Rendered> },
    Text { doc: u64, page: usize, text: Arc<PageText> },
    Hits { generation: u64, page: usize, hits: Vec<Vec<PtRect>> },
    SearchDone { generation: u64 },
    Annots { doc: u64, page: usize, annots: Arc<Vec<markup::Annot>> },
    /// A typeset document's Markdown changed on disk.
    SourceChanged { doc: u64 },
    /// The print portal's answer: sent, dismissed (false), or failed.
    Printed { result: Result<bool, String> },
    /// A page's content changed. `dirty`: changes the file does not have.
    Edited { doc: u64, page: usize, ok: bool, dirty: bool },
    /// Anything may have changed: pages rearranged, or a step undone or
    /// redone. `focus`: a page for the view to go to.
    Restructured { doc: u64, sizes: Vec<doc::PageSize>, dirty: bool, focus: Option<usize>, ok: bool },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Draw,
    Note,
}

/// What an open editor writes to when committed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EditTarget {
    Field { index: usize },
    /// A note: an existing one by index, or a new one at a point.
    Note { index: Option<usize>, at: (f64, f64) },
}

/// A one-line editor over the page: a text field being filled, or a note
/// being written.
struct Editor {
    page: usize,
    target: EditTarget,
    edit: LineEdit,
    /// The text it opened with: committing it unchanged changes nothing.
    original: String,
    /// Where it sits, in display points on `page`: the field's box, or the
    /// note's anchor.
    anchor: PtRect,
}

/// The find bar and what its query has found so far.
struct Find {
    edit: LineEdit,
    /// Whether keys go to the field (it stays open, showing its matches,
    /// after a click on the page takes the keyboard back).
    focused: bool,
    /// Matches in document order: a page and the rects one match covers.
    hits: Vec<(usize, Vec<PtRect>)>,
    current: Option<usize>,
    done: bool,
}

/// A press on a thumbnail that may become a drag.
struct ThumbPress {
    page: usize,
    y: f64,
    dragging: bool,
    /// No Shift or Ctrl: released without a drag, it selects just its page.
    plain: bool,
}

struct DocumentsApp {
    backend: Backend,
    /// For threads of the app's own that report back (the source watcher,
    /// printing).
    notify: calloop::channel::Sender<Message>,
    /// Made on the first Markdown file.
    typesetter: Option<writing::Typesetter>,
    /// The document a source watcher watches; one whose document is gone
    /// stops.
    watching: Arc<AtomicU64>,
    store: PageStore,
    /// Thumbnails, in their own store and budget.
    thumbs: PageStore,
    sidebar: bool,
    /// The sidebar's own scroll, in px.
    thumb_scroll: f64,
    /// Pages selected in the sidebar, and where a Shift range starts.
    thumb_sel: BTreeSet<usize>,
    thumb_anchor: Option<usize>,
    thumb_press: Option<ThumbPress>,
    /// While dragging thumbnails: the gap they would drop into.
    drop_gap: Option<usize>,
    /// The last page operation sent, to select its result.
    last_page_op: Option<PageOp>,
    doc: Option<Document>,
    next_doc: u64,
    error: Option<String>,
    /// Page text by page, as the engine delivers it, and the pages asked for.
    texts: HashMap<usize, Arc<PageText>>,
    texts_asked: HashSet<usize>,
    selection: Option<Selection>,
    /// A drag that started on text is selecting, not panning.
    selecting: bool,
    find: Option<Find>,
    /// Bumped per query, so hits for an older one are dropped on arrival.
    search_generation: u64,
    /// For measuring the toolbar and text fields; made on first use.
    font_system: Option<cce_ui::cosmic_text::FontSystem>,
    tool: Tool,
    /// Page annotations by page, as the engine delivers them, and the
    /// pages asked for.
    annots: HashMap<usize, Arc<Vec<Annot>>>,
    annots_asked: HashSet<usize>,
    /// A picked annotation: page and index. Delete removes it.
    picked: Option<(usize, usize)>,
    /// Ink being drawn: its page and points so far, in display points.
    stroke: Option<(usize, Vec<(f64, f64)>)>,
    editor: Option<Editor>,
    /// Changes the file does not have yet.
    dirty: bool,
    /// A line under the HUD (saved, a failure, the quit warning) until the
    /// next key or press.
    status: Option<String>,
    /// `q` with unsaved changes warns once; a second `q` quits.
    quit_armed: bool,
    /// User rotation in quarter turns clockwise, whole-document.
    quarter_turns: u8,
    /// Screen px per document unit (pt).
    zoom: f64,
    /// Scroll offset in screen px; 0 when the content fits the window.
    scroll: (f64, f64),
    /// Drives `scroll` (the drawn value) from the wheel: notches glide,
    /// fingers track 1:1 and fling on the lift. Drag, keyboard and zoom
    /// write `scroll` directly; the motion adopts those through `reconcile`.
    scroll_motion: ScrollMotion,
    /// Refit on resize until the user zooms manually.
    fit: bool,
    win: (f32, f32),
    scale: f64,
    pointer: (f64, f64),
    drag: Option<(f64, f64)>,
    ctrl: bool,
    shift: bool,
    /// Whether a renderer has been handed over yet — the first one is the
    /// process's own, any later one is a replacement after a reconnect.
    /// See `renderer_init`.
    seen_renderer: bool,
}

/// Per-page layout rect in document units.
struct PageRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// A display point on a page of size (w, h) after the user's quarter
/// turns: where it lands in the page's laid-out rect.
fn turn(q: u8, w: f64, h: f64, x: f64, y: f64) -> (f64, f64) {
    match q % 4 {
        1 => (h - y, x),
        2 => (w - x, h - y),
        3 => (y, w - x),
        _ => (x, y),
    }
}

/// The inverse of `turn`.
fn unturn(q: u8, w: f64, h: f64, u: f64, v: f64) -> (f64, f64) {
    match q % 4 {
        1 => (v, h - u),
        2 => (w - u, h - v),
        3 => (w - v, u),
        _ => (u, v),
    }
}

impl DocumentsApp {
    fn rotated(&self, page: doc::PageSize) -> (f64, f64) {
        if self.quarter_turns % 2 == 1 {
            (page.h, page.w)
        } else {
            (page.w, page.h)
        }
    }

    /// Page rects stacked vertically, centered in the content width.
    fn layout(&self) -> (Vec<PageRect>, f64, f64) {
        let Some(doc) = &self.doc else { return (Vec::new(), 0.0, 0.0) };
        let content_w = doc.pages.iter().map(|p| self.rotated(*p).0).fold(0.0, f64::max);
        let mut rects = Vec::with_capacity(doc.pages.len());
        let mut y = 0.0;
        for page in &doc.pages {
            let (w, h) = self.rotated(*page);
            rects.push(PageRect { x: (content_w - w) / 2.0, y, w, h });
            y += h + GAP_UNITS;
        }
        (rects, content_w, y - GAP_UNITS)
    }

    /// Where the pages are shown: the window right of the sidebar, if it
    /// is open. Its left edge and width, in px.
    fn view_x(&self) -> f64 {
        if self.sidebar { SIDEBAR_W as f64 } else { 0.0 }
    }

    fn view_w(&self) -> f64 {
        (self.win.0 as f64 - self.view_x()).max(64.0)
    }

    fn view_rect(&self) -> Rect {
        Rect { x: self.view_x() as f32, y: 0.0, width: self.view_w() as f32, height: self.win.1 }
    }

    /// Top-left of the content in screen coords: centered when it fits,
    /// scrolled when it doesn't.
    fn origin(&self, content_w: f64, content_h: f64) -> (f64, f64) {
        let (w, h) = (self.view_w(), self.win.1 as f64);
        let ox = self.view_x() + ((w - content_w * self.zoom) / 2.0).max(0.0) - self.scroll.0;
        let oy = ((h - content_h * self.zoom) / 2.0).max(0.0) - self.scroll.1;
        (ox, oy)
    }

    /// A rect in display points on `page` → screen px.
    fn screen_rect(&self, rects: &[PageRect], origin: (f64, f64), page: usize, r: &PtRect) -> Rect {
        let (Some(doc), Some(pr)) = (&self.doc, rects.get(page)) else { return Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 } };
        let size = doc.pages[page];
        let (ax, ay) = turn(self.quarter_turns, size.w, size.h, r.x0, r.y0);
        let (bx, by) = turn(self.quarter_turns, size.w, size.h, r.x1, r.y1);
        let x = origin.0 + (pr.x + ax.min(bx)) * self.zoom;
        let y = origin.1 + (pr.y + ay.min(by)) * self.zoom;
        Rect { x: x as f32, y: y as f32, width: ((ax - bx).abs() * self.zoom) as f32, height: ((ay - by).abs() * self.zoom) as f32 }
    }

    /// The page under a screen point, and the point in its display points.
    /// With `nearest`, a point between or beside pages takes the nearest
    /// page (a selection drag keeps extending across the gaps).
    fn page_at(&self, px: f64, py: f64, nearest: bool) -> Option<(usize, f64, f64)> {
        let doc = self.doc.as_ref()?;
        if px < self.view_x() {
            return None;
        }
        let (rects, cw, ch) = self.layout();
        let (ox, oy) = self.origin(cw, ch);
        let (dx, dy) = ((px - ox) / self.zoom, (py - oy) / self.zoom);
        let i = match rects.iter().position(|r| dx >= r.x && dx <= r.x + r.w && dy >= r.y && dy <= r.y + r.h) {
            Some(i) => i,
            None if nearest => rects
                .iter()
                .position(|r| dy < r.y + r.h + GAP_UNITS / 2.0)
                .unwrap_or(rects.len().checked_sub(1)?),
            None => return None,
        };
        let r = &rects[i];
        let size = doc.pages[i];
        let (u, v) = ((dx - r.x).clamp(0.0, r.w), (dy - r.y).clamp(0.0, r.h));
        let (x, y) = unturn(self.quarter_turns, size.w, size.h, u, v);
        Some((i, x, y))
    }

    fn clamp_scroll(&mut self) {
        let (_, cw, ch) = self.layout();
        let (w, h) = (self.view_w(), self.win.1 as f64);
        self.scroll.0 = self.scroll.0.clamp(0.0, (cw * self.zoom - w).max(0.0));
        self.scroll.1 = self.scroll.1.clamp(0.0, (ch * self.zoom - h).max(0.0));
    }

    fn scroll_by(&mut self, dx: f64, dy: f64) {
        self.scroll.0 += dx;
        self.scroll.1 += dy;
        self.clamp_scroll();
    }

    /// The wheel's range per axis, `0..=overflow` — what `clamp_scroll` clamps to.
    fn scroll_bounds(&self) -> (Bounds, Bounds) {
        let (_, cw, ch) = self.layout();
        let (w, h) = (self.view_w(), self.win.1 as f64);
        (Bounds::max((cw * self.zoom - w) as f32), Bounds::max((ch * self.zoom - h) as f32))
    }

    /// Copy the motion's position into `scroll` exactly (the f32 round-trips
    /// losslessly, so the next `reconcile` sees no host write).
    fn sync_scroll_from_motion(&mut self) {
        self.scroll = (self.scroll_motion.x.pos() as f64, self.scroll_motion.y.pos() as f64);
    }

    /// Per-frame wheel glide/coast; true while `scroll` is still moving, so
    /// the frame loop keeps drawing.
    fn tick_scroll(&mut self, dt: f32) -> bool {
        self.scroll_motion.reconcile(self.scroll.0 as f32, self.scroll.1 as f32);
        if !self.scroll_motion.is_animating() {
            return false;
        }
        let (bx, by) = self.scroll_bounds();
        let moved = self.scroll_motion.tick(dt, bx, by);
        self.sync_scroll_from_motion();
        moved || self.scroll_motion.is_animating()
    }

    /// Multiply zoom, keeping the document point under (px, py) fixed.
    fn zoom_at(&mut self, factor: f64, px: f64, py: f64) {
        let (_, cw, ch) = self.layout();
        let (ox, oy) = self.origin(cw, ch);
        let (dx, dy) = ((px - ox) / self.zoom, (py - oy) / self.zoom);
        self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.fit = false;
        let (w, h) = (self.view_w(), self.win.1 as f64);
        let pad_x = ((w - cw * self.zoom) / 2.0).max(0.0);
        let pad_y = ((h - ch * self.zoom) / 2.0).max(0.0);
        self.scroll.0 = self.view_x() + pad_x - (px - dx * self.zoom);
        self.scroll.1 = pad_y - (py - dy * self.zoom);
        self.clamp_scroll();
    }

    /// The page overlapping the viewport center (for HUD and refit).
    fn current_page(&self) -> usize {
        let (rects, cw, ch) = self.layout();
        let (_, oy) = self.origin(cw, ch);
        let mid = (self.win.1 as f64 / 2.0 - oy) / self.zoom;
        rects
            .iter()
            .position(|r| mid < r.y + r.h + GAP_UNITS / 2.0)
            .unwrap_or(rects.len().saturating_sub(1))
    }

    /// Fit the given page inside the window and scroll to its top.
    fn fit_page(&mut self, page: usize) {
        let (rects, _, _) = self.layout();
        let Some(r) = rects.get(page) else { return };
        let (w, h) = ((self.view_w() - fit_margin()).max(64.0), (self.win.1 as f64 - fit_margin()).max(64.0));
        self.zoom = (w / r.w).min(h / r.h).clamp(ZOOM_MIN, ZOOM_MAX);
        self.fit = true;
        self.scroll = (0.0, r.y * self.zoom);
        self.clamp_scroll();
    }

    fn go_to_page(&mut self, page: usize) {
        let (rects, _, _) = self.layout();
        if let Some(r) = rects.get(page) {
            self.scroll.1 = (r.y - GAP_UNITS / 2.0) * self.zoom;
            self.clamp_scroll();
        }
    }

    /// Scroll so a rect on a page is in view, a third of the way down.
    fn reveal(&mut self, page: usize, r: &PtRect) {
        let (rects, cw, ch) = self.layout();
        let origin = self.origin(cw, ch);
        let s = self.screen_rect(&rects, origin, page, r);
        let (x0, w, h) = (self.view_x() as f32, self.view_w() as f32, self.win.1);
        if s.y < 0.0 || s.y + s.height > h {
            self.scroll.1 += (s.y - h / 3.0) as f64;
        }
        if s.x < x0 || s.x + s.width > x0 + w {
            self.scroll.0 += (s.x + s.width / 2.0 - (x0 + w / 2.0)) as f64;
        }
        self.clamp_scroll();
    }

    fn open(&mut self, path: &Path) {
        self.store.reset();
        self.thumbs.reset();
        self.thumb_sel.clear();
        self.thumb_anchor = None;
        self.thumb_scroll = 0.0;
        self.last_page_op = None;
        self.quarter_turns = 0;
        self.error = None;
        self.texts.clear();
        self.texts_asked.clear();
        self.annots.clear();
        self.annots_asked.clear();
        self.selection = None;
        self.selecting = false;
        self.picked = None;
        self.stroke = None;
        self.editor = None;
        self.dirty = false;
        self.quit_armed = false;
        self.next_doc += 1;
        match self.load(path, self.next_doc) {
            Ok(d) => {
                if d.writing {
                    self.watch(&d.path, d.id);
                }
                self.doc = Some(d);
                self.fit_page(0);
            }
            Err(e) => {
                self.doc = None;
                self.error = Some(format!("{}: {e}", path.display()));
            }
        }
        self.restart_search();
    }

    /// Open a file as a document: a PDF as it is, Markdown typeset first.
    fn load(&mut self, path: &Path, id: u64) -> Result<Document, String> {
        if !writing::is_markdown(path) {
            return self.backend.open(path, id);
        }
        let pdf = self.typesetter.get_or_insert_with(writing::Typesetter::start).typeset(path)?;
        let mut d = self.backend.open(&pdf, id)?;
        d.path = path.to_path_buf();
        d.writing = true;
        Ok(d)
    }

    /// Watch a typeset document's source: a thread that polls its modified
    /// time and says when it changed, until another document is opened.
    fn watch(&self, path: &Path, id: u64) {
        self.watching.store(id, Ordering::Relaxed);
        let (path, watching, notify) = (path.to_path_buf(), Arc::clone(&self.watching), self.notify.clone());
        let stamp = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let _ = std::thread::Builder::new().name("watch".into()).spawn(move || {
            let mut last = stamp(&path);
            while watching.load(Ordering::Relaxed) == id {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let now = stamp(&path);
                if now.is_some() && now != last {
                    last = now;
                    if notify.send(Message::SourceChanged { doc: id }).is_err() {
                        return;
                    }
                }
            }
        });
    }

    /// Typeset the source again and show it, keeping the page and zoom. A
    /// source that does not typeset (a save half written) keeps the pages
    /// shown and says why.
    fn retypeset(&mut self) {
        let Some(path) = self.doc.as_ref().filter(|d| d.writing).map(|d| d.path.clone()) else { return };
        if let Err(e) = self.typesetter.get_or_insert_with(writing::Typesetter::start).typeset(&path) {
            self.status = Some(format!("Not typeset: {e}"));
            return;
        }
        let (page, zoom, fit, scroll) = (self.current_page(), self.zoom, self.fit, self.scroll);
        let (sidebar, thumb_scroll) = (self.sidebar, self.thumb_scroll);
        self.open(&path);
        self.sidebar = sidebar;
        self.thumb_scroll = thumb_scroll;
        let last = self.doc.as_ref().map_or(0, |d| d.pages.len().saturating_sub(1));
        if fit {
            self.fit_page(page.min(last));
        } else {
            self.zoom = zoom;
            self.fit = false;
            self.scroll = scroll;
            self.clamp_scroll();
        }
        self.clamp_thumb_scroll();
    }

    /// Whether the document can be edited as a PDF: PDFium is there, and
    /// it is not a typeset Markdown file.
    fn editable(&self) -> bool {
        self.backend.engine().is_some() && self.doc.as_ref().is_some_and(|d| !d.writing)
    }

    /// Save a typeset document's PDF where the person chooses.
    fn export(&mut self) {
        let Some(doc) = self.doc.as_ref().filter(|d| d.writing) else { return };
        let (file, stem) = (doc.file.clone(), doc.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        let target = match cce_ui::file_dialog::save_file(&format!("Export {stem} as PDF"), &[("PDF", &["pdf"])]) {
            Some(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) => p,
            Some(p) => p.with_extension("pdf"),
            None => return,
        };
        self.status = Some(match std::fs::copy(&file, &target) {
            Ok(_) => format!("Exported {}", target.file_name().and_then(|n| n.to_str()).unwrap_or("?")),
            Err(e) => format!("Not exported: {e}"),
        });
    }

    /// Print the document as a PDF: a typeset one's pages, or a PDF's file
    /// as last saved.
    fn print(&mut self) {
        let Some(doc) = &self.doc else { return };
        let title = doc.path.file_name().and_then(|n| n.to_str()).unwrap_or("Document").to_string();
        if self.dirty && !doc.writing {
            self.status = Some("Printing the file as last saved; your unsaved changes are not in it".to_string());
        }
        print::print(doc.file.clone(), title, self.notify.clone());
    }

    fn open_dialog(&mut self) {
        let filters: &[(&str, &[&str])] = &[("Documents", &["pdf", "md", "markdown"]), ("PDF", &["pdf"]), ("Markdown", &["md", "markdown"])];
        if let Some(path) = cce_ui::file_dialog::pick_file("Open", filters) {
            self.open(&path);
        }
    }

    fn rotate(&mut self, quarter_turns_cw: i8) {
        self.quarter_turns = (self.quarter_turns as i8 + quarter_turns_cw).rem_euclid(4) as u8;
        self.store.reset();
        self.clamp_scroll();
        if self.fit {
            self.fit_page(self.current_page());
        }
    }

    fn notches(delta: &MouseScrollDelta) -> (f64, f64) {
        match delta {
            MouseScrollDelta::LineDelta(x, y) => (*x as f64, *y as f64),
            MouseScrollDelta::PixelDelta(Position { x, y }) => (x / 60.0, y / 60.0),
        }
    }

    /// Ask the engine for a page's text, once.
    fn want_text(&mut self, page: usize) {
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            if !self.texts.contains_key(&page) && self.texts_asked.insert(page) {
                e.request_text(doc.id, page);
            }
        }
    }

    /// A page's text now: the cached copy, or the engine's answer, waited for.
    fn text_now(&mut self, page: usize) -> Option<Arc<PageText>> {
        if let Some(t) = self.texts.get(&page) {
            return Some(Arc::clone(t));
        }
        let t = self.backend.engine()?.text_now(self.doc.as_ref()?.id, page)?;
        self.texts.insert(page, Arc::clone(&t));
        Some(t)
    }

    /// The selection's text, page breaks as newlines.
    fn selected_text(&mut self) -> Option<String> {
        let sel = self.selection.filter(|s| !s.is_empty())?;
        let (a, b) = sel.ordered();
        let last = b.page.min(self.doc.as_ref()?.pages.len().saturating_sub(1));
        let mut out = String::new();
        for page in a.page..=last {
            let Some(text) = self.text_now(page) else { continue };
            if let Some((lo, hi)) = sel.range_on(page, text.len()) {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&text.text(lo, hi));
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn select_all(&mut self) {
        let Some(doc) = &self.doc else { return };
        let last = doc.pages.len().saturating_sub(1);
        self.selection = Some(Selection { anchor: Mark { page: 0, pos: 0 }, head: Mark { page: last, pos: usize::MAX } });
    }

    /// The caret mark under a screen point, and whether it is on text.
    fn mark_at(&mut self, px: f64, py: f64, nearest: bool) -> Option<(Mark, bool)> {
        let (page, x, y) = self.page_at(px, py, nearest)?;
        let Some(text) = self.texts.get(&page) else {
            self.want_text(page);
            return None;
        };
        let (pos, on_text) = text.caret_at(x, y)?;
        Some((Mark { page, pos }, on_text))
    }

    fn open_find(&mut self) {
        if self.font_system.is_none() {
            self.font_system = Some(cce_ui::create_font_system());
        }
        let find = self.find.get_or_insert_with(|| Find {
            edit: LineEdit::default(),
            focused: true,
            hits: Vec::new(),
            current: None,
            done: false,
        });
        find.focused = true;
        find.edit.select_all();
    }

    fn close_find(&mut self) {
        if let Some(mut f) = self.find.take() {
            f.edit.drop_composition();
        }
        self.restart_search();
    }

    /// Start the find bar's query over (or stop searching when it is closed
    /// or empty).
    fn restart_search(&mut self) {
        self.search_generation += 1;
        let query = match &mut self.find {
            Some(f) => {
                f.hits.clear();
                f.current = None;
                f.done = f.edit.text.is_empty();
                f.edit.text.clone()
            }
            None => String::new(),
        };
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            e.search(doc.id, self.search_generation, query);
        }
    }

    /// Step to the next (or previous) match and bring it into view.
    fn step_match(&mut self, forward: bool) {
        let Some(f) = &mut self.find else { return };
        if f.hits.is_empty() {
            return;
        }
        let n = f.hits.len();
        let i = match f.current {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        };
        f.current = Some(i);
        let (page, rects) = f.hits[i].clone();
        if let Some(r) = rects.first() {
            self.reveal(page, r);
        }
    }

    /// File a page's matches in document order. The first match to arrive
    /// at or after the page in view becomes the current one, and is shown.
    fn add_hits(&mut self, page: usize, hits: Vec<Vec<PtRect>>) {
        let here = self.current_page();
        let Some(f) = &mut self.find else { return };
        let at = f.hits.partition_point(|(p, _)| *p <= page);
        let n = hits.len();
        f.hits.splice(at..at, hits.into_iter().map(|r| (page, r)));
        match f.current {
            Some(c) if c >= at => f.current = Some(c + n),
            Some(_) => {}
            None if page >= here => {
                f.current = Some(at);
                let (p, rects) = f.hits[at].clone();
                if let Some(r) = rects.first() {
                    self.reveal(p, r);
                }
            }
            None => {}
        }
    }

    /// The find bar's rect: the top right corner, the root plate's inset in.
    fn find_rect(&self) -> Rect {
        let inset = cce_ui::layout::root_plate_inset();
        let w = FIND_W.min(self.win.0 - 2.0 * inset);
        Rect { x: self.win.0 - inset - w, y: inset, width: w, height: FIND_H }
    }

    /// Ask the engine for a page's annotations, once.
    fn want_annots(&mut self, page: usize) {
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            if !self.annots.contains_key(&page) && self.annots_asked.insert(page) {
                e.request_annots(doc.id, page);
            }
        }
    }

    fn edit(&self, edit: Edit) {
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            e.edit(doc.id, edit);
        }
    }

    /// Mark the selected text: one annotation per page it covers.
    fn mark_selection(&mut self, kind: MarkupKind) {
        let Some(sel) = self.selection.filter(|s| !s.is_empty()) else { return };
        let (a, b) = sel.ordered();
        let last = b.page.min(self.doc.as_ref().map_or(0, |d| d.pages.len()).saturating_sub(1));
        for page in a.page..=last {
            let Some(text) = self.text_now(page) else { continue };
            let Some((lo, hi)) = sel.range_on(page, text.len()) else { continue };
            let rects = text.rects(lo, hi);
            if !rects.is_empty() {
                self.edit(Edit::Markup { page, kind, rects });
            }
        }
        self.selection = None;
    }

    fn set_tool(&mut self, tool: Tool) {
        self.commit_editor();
        self.tool = tool;
        self.picked = None;
        if tool != Tool::Select {
            self.selection = None;
        }
    }

    fn undo(&mut self) {
        self.commit_editor();
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            e.undo(doc.id);
        }
    }

    fn redo(&mut self) {
        self.commit_editor();
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            e.redo(doc.id);
        }
    }

    /// Send a page operation; `restructured` takes in its result.
    fn page_op(&mut self, op: PageOp) {
        self.commit_editor();
        if let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) {
            e.pages(doc.id, op.clone());
            self.last_page_op = Some(op);
        }
    }

    /// Take in a document whose pages may all have changed: new sizes, and
    /// every cache kept by page index is stale.
    fn restructured(&mut self, sizes: Vec<doc::PageSize>, dirty: bool, focus: Option<usize>, ok: bool) {
        let Some(d) = &mut self.doc else { return };
        let old_len = d.pages.len();
        // The same sizes (an undone mark, pages of one size reordered): keep
        // showing the old images until the new ones land.
        let same = d.pages == sizes;
        d.pages = sizes;
        let len = d.pages.len();
        self.dirty = dirty;
        if same {
            self.store.invalidate_all();
            self.thumbs.invalidate_all();
        } else {
            self.store.reset();
            self.thumbs.reset();
        }
        self.texts.clear();
        self.texts_asked.clear();
        self.annots.clear();
        self.annots_asked.clear();
        self.selection = None;
        self.picked = None;
        self.stroke = None;
        self.cancel_editor();
        if !ok {
            self.status = Some("That change could not be made".to_string());
        }
        // Select what the operation made.
        self.thumb_sel = match self.last_page_op.take() {
            Some(PageOp::Move { pages, gap }) if ok => {
                let d = move_dest(&pages, gap);
                (d..d + pages.len()).collect()
            }
            Some(PageOp::Insert { at, .. }) if ok => (at..at + len.saturating_sub(old_len)).collect(),
            Some(PageOp::Rotate { pages, .. }) => pages.into_iter().collect(),
            _ => BTreeSet::new(),
        };
        self.thumb_sel.retain(|&p| p < len);
        self.thumb_anchor = self.thumb_sel.first().copied();
        self.restart_search();
        match focus.filter(|&f| f < len) {
            Some(f) if self.fit => self.fit_page(f),
            Some(f) => self.go_to_page(f),
            None if self.fit => self.fit_page(self.current_page()),
            None => self.clamp_scroll(),
        }
        if let Some(f) = focus {
            self.reveal_thumb(f.min(len.saturating_sub(1)));
        }
        self.clamp_thumb_scroll();
    }

    fn toggle_sidebar(&mut self) {
        if self.backend.engine().is_none() || self.doc.is_none() {
            return;
        }
        let page = self.current_page();
        self.sidebar = !self.sidebar;
        if self.fit {
            self.fit_page(page);
        } else {
            self.clamp_scroll();
        }
        if self.sidebar {
            self.reveal_thumb(page);
        }
    }

    fn insert_pdf(&mut self) {
        let Some(path) = cce_ui::file_dialog::pick_file("Insert pages from", &[("PDF", &["pdf"])]) else { return };
        let end = self.doc.as_ref().map_or(0, |d| d.pages.len());
        let at = self.thumb_sel.last().map_or(end, |&p| p + 1);
        self.page_op(PageOp::Insert { from: path, at });
    }

    fn extract_pages(&mut self) {
        let pages: Vec<usize> = self.thumb_sel.iter().copied().collect();
        if pages.is_empty() {
            return;
        }
        let target = match cce_ui::file_dialog::save_file("Save pages as", &[("PDF", &["pdf"])]) {
            Some(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) => p,
            Some(p) => p.with_extension("pdf"),
            None => return,
        };
        let (Some(e), Some(doc)) = (self.backend.engine(), &self.doc) else { return };
        let n = pages.len();
        self.status = Some(match e.extract(doc.id, pages, &target) {
            Ok(()) => {
                let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
                format!("Saved {n} page{} to {name}", if n == 1 { "" } else { "s" })
            }
            Err(err) => format!("Not saved: {err}"),
        });
    }

    fn rotate_pages(&mut self, quarter_turns: i32) {
        let pages: Vec<usize> = self.thumb_sel.iter().copied().collect();
        if !pages.is_empty() {
            self.page_op(PageOp::Rotate { pages, quarter_turns });
        }
    }

    fn delete_pages(&mut self) {
        let pages: Vec<usize> = self.thumb_sel.iter().copied().collect();
        let len = self.doc.as_ref().map_or(0, |d| d.pages.len());
        if pages.is_empty() {
            return;
        }
        if pages.len() >= len {
            self.status = Some("A document keeps at least one page".to_string());
            return;
        }
        self.page_op(PageOp::Delete { pages });
    }

    /// A thumbnail's size for a page: `THUMB_W` wide, unless the page is so
    /// tall that 1.5 × that height caps it.
    fn thumb_size(page: doc::PageSize) -> (f64, f64) {
        let s = (THUMB_W / page.w.max(1.0)).min(THUMB_W * 1.5 / page.h.max(1.0));
        (page.w * s, page.h * s)
    }

    /// Each thumbnail's rect on screen, scrolled.
    fn thumb_rects(&self) -> Vec<Rect> {
        let Some(doc) = &self.doc else { return Vec::new() };
        let mut y = SIDEBAR_HEADER as f64 + 14.0 - self.thumb_scroll;
        doc.pages
            .iter()
            .map(|p| {
                let (w, h) = Self::thumb_size(*p);
                let r = Rect { x: ((SIDEBAR_W as f64 - w) / 2.0) as f32, y: y as f32, width: w as f32, height: h as f32 };
                y += h + THUMB_GAP;
                r
            })
            .collect()
    }

    fn clamp_thumb_scroll(&mut self) {
        let content: f64 = self.doc.as_ref().map_or(0.0, |d| d.pages.iter().map(|p| Self::thumb_size(*p).1 + THUMB_GAP).sum::<f64>() + 14.0);
        let room = self.win.1 as f64 - SIDEBAR_HEADER as f64;
        self.thumb_scroll = self.thumb_scroll.clamp(0.0, (content - room).max(0.0));
    }

    /// Scroll the sidebar so a page's thumbnail is in view.
    fn reveal_thumb(&mut self, page: usize) {
        let Some(r) = self.thumb_rects().get(page).copied() else { return };
        let top = SIDEBAR_HEADER + 8.0;
        let bottom = self.win.1 - 8.0;
        if r.y < top {
            self.thumb_scroll -= (top - r.y) as f64;
        } else if r.y + r.height + THUMB_GAP as f32 > bottom {
            self.thumb_scroll += (r.y + r.height + THUMB_GAP as f32 - bottom) as f64;
        }
        self.clamp_thumb_scroll();
    }

    /// The thumbnail under a point, counting the gap below it (its number).
    fn thumb_at(&self, x: f64, y: f64) -> Option<usize> {
        let x = x as f32;
        self.thumb_rects()
            .iter()
            .position(|r| x >= r.x - 8.0 && x <= r.x + r.width + 8.0 && y as f32 >= r.y - 4.0 && (y as f32) <= r.y + r.height + THUMB_GAP as f32 - 4.0)
    }

    /// The gap a dragged thumbnail would drop into: before the first
    /// thumbnail whose middle is below the point.
    fn gap_at(&self, y: f64) -> usize {
        let rects = self.thumb_rects();
        rects.iter().position(|r| (y as f32) < r.y + r.height / 2.0).unwrap_or(rects.len())
    }

    fn sidebar_buttons(&self) -> (Vec<Button>, Vec<Rect>) {
        let len = self.doc.as_ref().map_or(0, |d| d.pages.len());
        let edit = self.editable();
        let some = edit && !self.thumb_sel.is_empty();
        let b = |label, action, enabled| Button { label, action, on: false, enabled, group: false };
        let buttons = vec![
            b("Insert…", Action::Insert, edit),
            b("Extract…", Action::Extract, some),
            b("Rotate", Action::RotatePages, some),
            b("Delete", Action::DeletePages, some && self.thumb_sel.len() < len),
        ];
        let w = (SIDEBAR_W - 24.0) / 2.0;
        let rects = (0..4)
            .map(|i| Rect { x: 8.0 + (i % 2) as f32 * (w + 8.0), y: 8.0 + (i / 2) as f32 * 34.0, width: w, height: 28.0 })
            .collect();
        (buttons, rects)
    }

    fn sidebar_press(&mut self, x: f64, y: f64) {
        self.commit_editor();
        if (y as f32) < SIDEBAR_HEADER {
            let (buttons, rects) = self.sidebar_buttons();
            if let Some(action) = chrome::button_at(&buttons, &rects, x as f32, y as f32) {
                self.run_action(action);
            }
            return;
        }
        let Some(i) = self.thumb_at(x, y) else {
            self.thumb_sel.clear();
            self.thumb_anchor = None;
            return;
        };
        let plain = !self.ctrl && !self.shift;
        if self.ctrl {
            if !self.thumb_sel.remove(&i) {
                self.thumb_sel.insert(i);
            }
            self.thumb_anchor = Some(i);
        } else if self.shift {
            let a = self.thumb_anchor.unwrap_or(i);
            self.thumb_sel = (a.min(i)..=a.max(i)).collect();
        } else if !self.thumb_sel.contains(&i) {
            // A press on a selected page keeps the selection, so it can be
            // dragged as a whole; the release narrows it if there was no drag.
            self.thumb_sel = BTreeSet::from([i]);
            self.thumb_anchor = Some(i);
        }
        if self.fit {
            self.fit_page(i);
        } else {
            self.go_to_page(i);
        }
        self.thumb_press = Some(ThumbPress { page: i, y, dragging: false, plain });
    }

    fn sidebar_release(&mut self, press: ThumbPress) {
        let gap = self.drop_gap.take();
        if !press.dragging {
            if press.plain {
                self.thumb_sel = BTreeSet::from([press.page]);
                self.thumb_anchor = Some(press.page);
            }
            return;
        }
        let Some(gap) = gap.filter(|_| self.editable()) else { return };
        let pages: Vec<usize> = self.thumb_sel.iter().copied().collect();
        let (Some(&first), Some(&last)) = (pages.first(), pages.last()) else { return };
        // Dropped back where it was: a contiguous run into a gap at or
        // inside its own edges.
        let contiguous = last - first + 1 == pages.len();
        if contiguous && gap >= first && gap <= last + 1 {
            return;
        }
        self.page_op(PageOp::Move { pages, gap });
    }

    fn paint_sidebar(&mut self, pc: &mut PaintCtx, scale: f64) {
        let h = self.win.1;
        pc.quad(Rect { x: 0.0, y: 0.0, width: SIDEBAR_W, height: h }, [0.105, 0.105, 0.115, 1.0]);
        pc.quad(Rect { x: SIDEBAR_W - 1.0, y: 0.0, width: 1.0, height: h }, [1.0, 1.0, 1.0, 0.08]);
        let rects = self.thumb_rects();
        let current = self.current_page();
        let list = Rect { x: 0.0, y: SIDEBAR_HEADER, width: SIDEBAR_W - 1.0, height: (h - SIDEBAR_HEADER).max(0.0) };
        let Some(doc) = self.doc.take() else { return };
        pc.push_clip(list);
        for (i, r) in rects.iter().enumerate() {
            if r.y > h || r.y + r.height + (THUMB_GAP as f32) < SIDEBAR_HEADER {
                continue;
            }
            if self.thumb_sel.contains(&i) {
                pc.rounded_rect(Rect { x: r.x - 6.0, y: r.y - 6.0, width: r.width + 12.0, height: r.height + 24.0 }, 6.0, (true, true, true, true), [0.25, 0.5, 1.0, 0.35]);
            }
            pc.quad(Rect { x: r.x - 1.0, y: r.y - 1.0, width: r.width + 2.0, height: r.height + 2.0 }, if i == current { [1.0, 1.0, 1.0, 0.85] } else { [0.0, 0.0, 0.0, 0.5] });
            pc.quad(*r, [0.97, 0.97, 0.97, 1.0]);
            let want = r.width as f64 * scale * 72.0 / doc.pages[i].w.max(1.0);
            let dpi = *DPI_BUCKETS.iter().find(|&&b| want <= b as f64 * 1.01).unwrap_or(DPI_BUCKETS.last().unwrap());
            if let Some(img) = self.thumbs.ensure(&self.backend, &doc, 0, i, dpi) {
                pc.image(img.image, *r, 1.0);
            }
            let label = (i + 1).to_string();
            let fs = self.font_system.get_or_insert_with(cce_ui::create_font_system);
            let lw = chrome::width_of(fs, &label, 11.0);
            pc.text(label, (SIDEBAR_W - lw) / 2.0, r.y + r.height + 5.0, 11.0, [190, 190, 190]);
        }
        if let Some(gap) = self.drop_gap {
            let y = match rects.get(gap) {
                Some(r) => r.y - (THUMB_GAP as f32) / 2.0 + 2.0,
                None => rects.last().map_or(SIDEBAR_HEADER + 10.0, |r| r.y + r.height + THUMB_GAP as f32 / 2.0 + 2.0),
            };
            pc.quad(Rect { x: 12.0, y: y - 1.5, width: SIDEBAR_W - 24.0, height: 3.0 }, PICKED);
        }
        pc.pop_clip();
        self.doc = Some(doc);
        let (buttons, brects) = self.sidebar_buttons();
        chrome::paint_buttons(pc, &buttons, &brects, 12.0, true);
    }


    /// Save over the file (or, choosing, to a new one): the original bytes
    /// plus an incremental update.
    fn save(&mut self, choose: bool) {
        self.commit_editor();
        let Some(doc) = &self.doc else { return };
        let target = if choose {
            match cce_ui::file_dialog::save_file("Save As", &[("PDF", &["pdf"])]) {
                Some(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) => p,
                Some(p) => p.with_extension("pdf"),
                None => return,
            }
        } else {
            doc.path.clone()
        };
        let id = doc.id;
        let Some(e) = self.backend.engine() else { return };
        match e.save(id, &target) {
            Ok(()) => {
                self.dirty = false;
                self.quit_armed = false;
                let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
                self.status = Some(format!("Saved {name}"));
                if let Some(d) = &mut self.doc {
                    d.path = target;
                }
            }
            Err(err) => self.status = Some(format!("Not saved: {err}")),
        }
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::Select => self.set_tool(Tool::Select),
            Action::Draw => self.set_tool(Tool::Draw),
            Action::Note => self.set_tool(Tool::Note),
            Action::Highlight => self.mark_selection(MarkupKind::Highlight),
            Action::Underline => self.mark_selection(MarkupKind::Underline),
            Action::Strike => self.mark_selection(MarkupKind::StrikeOut),
            Action::Undo => self.undo(),
            Action::Save => self.save(false),
            Action::Pages => self.toggle_sidebar(),
            Action::Print => self.print(),
            Action::Export => self.export(),
            Action::Insert => self.insert_pdf(),
            Action::Extract => self.extract_pages(),
            Action::RotatePages => self.rotate_pages(1),
            Action::DeletePages => self.delete_pages(),
        }
    }

    /// The markup toolbar's buttons and where they sit; none without PDFium
    /// or a document.
    fn toolbar(&mut self) -> Option<(Vec<Button>, Vec<Rect>)> {
        if self.backend.engine().is_none() || self.doc.is_none() {
            return None;
        }
        let has_sel = self.selection.is_some_and(|s| !s.is_empty());
        let b = |label, action, on, enabled, group| Button { label, action, on, enabled, group };
        let writing = self.doc.as_ref().is_some_and(|d| d.writing);
        let buttons = if writing {
            vec![
                b("Pages", Action::Pages, self.sidebar, true, false),
                b("Export PDF…", Action::Export, false, true, true),
                b("Print…", Action::Print, false, true, false),
            ]
        } else {
            vec![
            b("Pages", Action::Pages, self.sidebar, true, false),
            b("Select", Action::Select, self.tool == Tool::Select, true, true),
            b("Draw", Action::Draw, self.tool == Tool::Draw, true, false),
            b("Note", Action::Note, self.tool == Tool::Note, true, false),
            b("Highlight", Action::Highlight, false, has_sel, true),
            b("Underline", Action::Underline, false, has_sel, false),
            b("Strike", Action::Strike, false, has_sel, false),
            b("Undo", Action::Undo, false, true, true),
            b("Save", Action::Save, self.dirty, true, false),
            b("Print…", Action::Print, false, true, false),
            ]
        };
        let area = self.view_rect();
        let fs = self.font_system.get_or_insert_with(cce_ui::create_font_system);
        let rects = chrome::toolbar_layout(fs, &buttons, area);
        Some((buttons, rects))
    }

    fn open_editor(&mut self, page: usize, target: EditTarget, text: String, anchor: PtRect) {
        self.commit_editor();
        if self.font_system.is_none() {
            self.font_system = Some(cce_ui::create_font_system());
        }
        if let Some(f) = &mut self.find {
            f.focused = false;
        }
        let mut edit = LineEdit::with_text(text.clone());
        edit.select_all();
        self.editor = Some(Editor { page, target, edit, original: text, anchor });
    }

    /// Write an open editor's text into the document and close it.
    fn commit_editor(&mut self) {
        let Some(mut ed) = self.editor.take() else { return };
        ed.edit.drop_composition();
        let text = ed.edit.text.clone();
        if text == ed.original {
            return;
        }
        match ed.target {
            EditTarget::Field { index } => self.edit(Edit::SetText { page: ed.page, index, value: text }),
            EditTarget::Note { index: Some(index), .. } => self.edit(Edit::SetContents { page: ed.page, index, contents: text }),
            EditTarget::Note { index: None, at } => {
                if !text.trim().is_empty() {
                    self.edit(Edit::Note { page: ed.page, at, contents: text });
                }
            }
        }
    }

    fn cancel_editor(&mut self) {
        if let Some(mut ed) = self.editor.take() {
            ed.edit.drop_composition();
        }
    }

    /// The open editor's box on screen, and its type size.
    fn editor_rect(&self) -> Option<(Rect, f32)> {
        let ed = self.editor.as_ref()?;
        let (rects, cw, ch) = self.layout();
        let r = self.screen_rect(&rects, self.origin(cw, ch), ed.page, &ed.anchor);
        Some(match ed.target {
            EditTarget::Field { .. } => {
                let h = r.height.max(22.0);
                (Rect { x: r.x, y: r.y + (r.height - h) / 2.0, width: r.width.max(80.0), height: h }, (h * 0.6).clamp(11.0, 16.0))
            }
            EditTarget::Note { .. } => {
                let x = r.x.min(self.win.0 - NOTE_EDITOR_W - 8.0).max(8.0);
                let mut y = r.y + r.height + 6.0;
                if y + NOTE_EDITOR_H > self.win.1 - 8.0 {
                    y = r.y - NOTE_EDITOR_H - 6.0;
                }
                (Rect { x, y, width: NOTE_EDITOR_W, height: NOTE_EDITOR_H }, 13.0)
            }
        })
    }

    /// A press on an annotation in the Select tool: fill a field, open a
    /// note, or pick the mark.
    fn press_annot(&mut self, page: usize, a: Annot) {
        self.selection = None;
        self.picked = None;
        match a.kind {
            AnnotKind::Field(FieldKind::Text { read_only: false }) => {
                let value = match (self.backend.engine(), &self.doc) {
                    (Some(e), Some(doc)) => e.field_value(doc.id, page, a.index).unwrap_or_default(),
                    _ => return,
                };
                self.open_editor(page, EditTarget::Field { index: a.index }, value, a.rect);
            }
            AnnotKind::Field(FieldKind::CheckBox { read_only: false }) | AnnotKind::Field(FieldKind::Radio { read_only: false }) => {
                self.commit_editor();
                self.edit(Edit::Toggle { page, index: a.index });
            }
            AnnotKind::Field(_) => {}
            AnnotKind::Note => {
                let at = ((a.rect.x0 + a.rect.x1) / 2.0, (a.rect.y0 + a.rect.y1) / 2.0);
                self.open_editor(page, EditTarget::Note { index: Some(a.index), at }, a.contents.clone(), a.rect);
            }
            AnnotKind::Markup(_) | AnnotKind::Ink => self.picked = Some((page, a.index)),
            AnnotKind::Other => {}
        }
    }

    /// A screen point in display points on a given page, clamped to it (a
    /// stroke stays on the page it started on).
    fn point_on_page(&self, page: usize, px: f64, py: f64) -> Option<(f64, f64)> {
        let doc = self.doc.as_ref()?;
        let (rects, cw, ch) = self.layout();
        let (ox, oy) = self.origin(cw, ch);
        let r = rects.get(page)?;
        let size = doc.pages[page];
        let u = ((px - ox) / self.zoom - r.x).clamp(0.0, r.w);
        let v = ((py - oy) / self.zoom - r.y).clamp(0.0, r.h);
        Some(unturn(self.quarter_turns, size.w, size.h, u, v))
    }

    /// Draw what sits over the pages: the picked mark, ink being drawn, a
    /// note's text under the pointer, an open editor.
    fn paint_overlays(&mut self, pc: &mut PaintCtx, rects: &[PageRect], origin: (f64, f64)) {
        if let Some((page, index)) = self.picked {
            if let Some(a) = self.annots.get(&page).and_then(|l| l.iter().find(|a| a.index == index)) {
                let r = self.screen_rect(rects, origin, page, &a.rect);
                let (x, y, w, h, t) = (r.x - 3.0, r.y - 3.0, r.width + 6.0, r.height + 6.0, 2.0);
                pc.quad(Rect { x, y, width: w, height: t }, PICKED);
                pc.quad(Rect { x, y: y + h - t, width: w, height: t }, PICKED);
                pc.quad(Rect { x, y, width: t, height: h }, PICKED);
                pc.quad(Rect { x: x + w - t, y, width: t, height: h }, PICKED);
            }
        }
        if let Some((page, pts)) = &self.stroke {
            let thick = (INK_PT * self.zoom).max(1.0) as f32;
            let screen: Vec<(f32, f32)> = pts
                .iter()
                .map(|&(x, y)| {
                    let r = self.screen_rect(rects, origin, *page, &PtRect { x0: x, y0: y, x1: x, y1: y });
                    (r.x, r.y)
                })
                .collect();
            for w in screen.windows(2) {
                pc.vector(w[0].0, w[0].1, w[1].0, w[1].1, thick, INK, cce_ui::scene::paint::Cap::Round);
            }
        }
        // A note's text, shown while the pointer rests on it.
        if self.tool == Tool::Select && self.editor.is_none() && self.stroke.is_none() {
            let hover = self.page_at(self.pointer.0, self.pointer.1, false).and_then(|(page, x, y)| {
                let a = annot_at(self.annots.get(&page)?, x, y)?;
                (a.kind == AnnotKind::Note && !a.contents.is_empty()).then(|| a.contents.clone())
            });
            if let Some(text) = hover {
                let fs = self.font_system.get_or_insert_with(cce_ui::create_font_system);
                let w = chrome::width_of(fs, &text, 13.0).min(420.0) + 20.0;
                let (x, y) = ((self.pointer.0 as f32 + 14.0).min(self.win.0 - w - 8.0), self.pointer.1 as f32 + 18.0);
                let r = Rect { x, y, width: w, height: 28.0 };
                pc.rounded_rect(r, 6.0, (true, true, true, true), [1.0, 0.97, 0.78, 0.97]);
                pc.clip(r, |pc| pc.text(text, x + 10.0, cce_ui::layout::align_text_y(y, 28.0, 13.0, 0.0), 13.0, [40, 36, 20]));
            }
        }
        if let Some((r, size)) = self.editor_rect() {
            let (Some(ed), Some(fs)) = (&mut self.editor, &mut self.font_system) else { return };
            let note = matches!(ed.target, EditTarget::Note { .. });
            let bg = if note { [1.0, 0.97, 0.78, 1.0] } else { [1.0, 1.0, 1.0, 1.0] };
            pc.quad(Rect { x: r.x - 2.0, y: r.y - 2.0, width: r.width + 4.0, height: r.height + 4.0 }, PICKED);
            pc.quad(r, bg);
            let look = FieldLook { size, text: [20, 20, 20], placeholder: [140, 140, 140], caret: [0.0, 0.0, 0.0, 0.9], selection: SELECTION };
            let field = Rect { x: r.x + 6.0, y: r.y, width: r.width - 12.0, height: r.height };
            chrome::paint_field(pc, fs, &mut ed.edit, field, &look, if note { "Write a note, Enter to keep it" } else { "" }, true);
        }
    }

    fn paint_find(&mut self, pc: &mut PaintCtx) {
        let r = self.find_rect();
        let has_engine = self.backend.engine().is_some();
        let (Some(f), Some(fs)) = (&mut self.find, &mut self.font_system) else { return };
        let pad = cce_ui::layout::CONTROL_TEXT_INSET;
        pc.quad(r, [0.0, 0.0, 0.0, 0.6]);
        let count = if !has_engine {
            String::new()
        } else if f.edit.text.is_empty() {
            String::new()
        } else if f.hits.is_empty() {
            if f.done { "no matches".to_string() } else { "searching…".to_string() }
        } else {
            let of = if f.done { f.hits.len().to_string() } else { format!("{}+", f.hits.len()) };
            format!("{}/{of}", f.current.map_or(0, |c| c + 1))
        };
        let count_w = if count.is_empty() { 0.0 } else { chrome::width_of(fs, &count, FIND_FONT) };
        let ty = cce_ui::layout::align_text_y(r.y, r.height, FIND_FONT, 0.0);
        if !count.is_empty() {
            pc.text(count, r.x + r.width - pad - count_w, ty, FIND_FONT, [170, 170, 170]);
        }
        let field = Rect { x: r.x + pad, y: r.y, width: (r.width - 3.0 * pad - count_w).max(0.0), height: r.height };
        let look = FieldLook { size: FIND_FONT, text: [235, 235, 235], placeholder: [130, 130, 130], caret: [1.0, 1.0, 1.0, 0.9], selection: SELECTION };
        let placeholder = if has_engine { "Find in document" } else { "Search needs PDFium: run scripts/fetch-pdfium" };
        let focused = f.focused;
        chrome::paint_field(pc, fs, &mut f.edit, field, &look, placeholder, focused);
    }
}

impl Application for DocumentsApp {
    type Message = Message;

    fn create(sender: cce_ui::engine::AppSender<Self::Message>) -> Self {
        // The app keeps calloop's sender; `AppSender` converts into it.
        let sender: calloop::channel::Sender<Self::Message> = sender.into();
        let mut app = Self {
            notify: sender.clone(),
            typesetter: None,
            watching: Arc::new(AtomicU64::new(0)),
            backend: Backend::start(sender),
            store: PageStore::new(0, PAGE_BUDGET),
            thumbs: PageStore::new(1, THUMB_BUDGET),
            sidebar: false,
            thumb_scroll: 0.0,
            thumb_sel: BTreeSet::new(),
            thumb_anchor: None,
            thumb_press: None,
            drop_gap: None,
            last_page_op: None,
            doc: None,
            next_doc: 0,
            error: None,
            texts: HashMap::new(),
            texts_asked: HashSet::new(),
            selection: None,
            selecting: false,
            find: None,
            search_generation: 0,
            font_system: None,
            tool: Tool::Select,
            annots: HashMap::new(),
            annots_asked: HashSet::new(),
            picked: None,
            stroke: None,
            editor: None,
            dirty: false,
            status: None,
            quit_armed: false,
            quarter_turns: 0,
            zoom: 1.0,
            scroll: (0.0, 0.0),
            scroll_motion: ScrollMotion::new(),
            fit: true,
            win: (900.0, 700.0),
            scale: 1.0,
            pointer: (0.0, 0.0),
            drag: None,
            ctrl: false,
            shift: false,
            seen_renderer: false,
        };
        if let Some(path) = std::env::args_os().nth(1) {
            app.open(&PathBuf::from(path));
        }
        app
    }

    fn settings(&self) -> WindowSettings {
        let title = match &self.doc {
            Some(d) => {
                let name = d.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let name = if self.dirty { format!("• {name}") } else { name.to_string() };
                if d.pages.len() > 1 {
                    format!("{name} (page {}/{}) — Documents", self.current_page() + 1, d.pages.len())
                } else {
                    format!("{name} — Documents")
                }
            }
            None => "Documents".to_string(),
        };
        WindowSettings {
            title,
            app_id: "cce-documents".to_string(),
            width: 900,
            height: 700,
            fullscreen: false,
            min_size: Some((320, 240)),
        }
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, exit: &mut bool) {
        match msg {
            Message::Page { slot, generation, page, result } => {
                if slot == self.thumbs.slot() {
                    self.thumbs.complete(generation, page, result);
                } else {
                    self.store.complete(generation, page, result);
                }
                *needs_rebuild = true;
            }
            Message::Text { doc, page, text } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) {
                    self.texts.insert(page, text);
                    *needs_rebuild = true;
                }
            }
            Message::Hits { generation, page, hits } => {
                if generation == self.search_generation {
                    self.add_hits(page, hits);
                    *needs_rebuild = true;
                }
            }
            Message::SearchDone { generation } => {
                if let (true, Some(f)) = (generation == self.search_generation, &mut self.find) {
                    f.done = true;
                    *needs_rebuild = true;
                }
            }
            Message::Annots { doc, page, annots } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) {
                    if self.picked.is_some_and(|(p, i)| p == page && i >= annots.len()) {
                        self.picked = None;
                    }
                    self.annots.insert(page, annots);
                    *needs_rebuild = true;
                }
            }
            Message::Edited { doc, page, ok, dirty } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) {
                    self.dirty = dirty;
                    if ok {
                        self.store.invalidate(page);
                        self.thumbs.invalidate(page);
                    } else {
                        self.status = Some("That change could not be made".to_string());
                    }
                    *needs_rebuild = true;
                }
            }
            Message::Restructured { doc, sizes, dirty, focus, ok } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) {
                    self.restructured(sizes, dirty, focus, ok);
                    *needs_rebuild = true;
                }
            }
            Message::SourceChanged { doc } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc && d.writing) {
                    self.retypeset();
                    *needs_rebuild = true;
                }
            }
            Message::Printed { result } => {
                match result {
                    Ok(true) => self.status = Some("Sent to the printer".to_string()),
                    Ok(false) => {}
                    Err(e) => self.status = Some(format!("Not printed: {e}")),
                }
                *needs_rebuild = true;
            }
            Message::Quit => *exit = true,
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.tick_scroll(dt) {
            *needs_rebuild = true;
        }
    }

    /// Throw the resident pages away when the renderer is replaced.
    ///
    /// `PageStore` holds **renderer** image ids, and a renderer does not
    /// outlive its session: `cce-ui`'s `window_runner` repairs a lost Wayland
    /// transport by opening a new session around the same `Application`, which
    /// rebuilds the renderer and with it the image table. The cached ids then
    /// name images that no longer exist, and a draw for an unknown id is
    /// skipped rather than reported — so a reconnected viewer came back with
    /// its chrome and a blank document, and stayed that way, because a
    /// resident page is never re-rendered.
    ///
    /// `reset` is exactly the right hammer: it frees every page (a free for an
    /// id the new renderer never had is a no-op) and bumps the generation, so
    /// a render still in flight for the old session is dropped on arrival
    /// instead of landing as a page nobody asked for. The next `display_list`
    /// finds nothing resident and queues the visible pages again.
    ///
    /// Not on the first renderer: the pages queued from `new()` are waiting
    /// for precisely that one.
    fn renderer_init(&mut self, _renderer: &mut cce_ui::vk::VkRenderer) {
        if std::mem::replace(&mut self.seen_renderer, true) {
            log::info!("[documents] renderer replaced; re-rendering the resident pages");
            self.store.reset();
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, scale: f64) {
        self.win = (width, height);
        self.scale = scale;
        if self.fit {
            self.fit_page(self.current_page());
        } else {
            self.clamp_scroll();
        }
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (px, py) = (pos.x as f64, pos.y as f64);
        if let Some(press) = &mut self.thumb_press {
            if !press.dragging && (py - press.y).abs() > DRAG_START {
                press.dragging = true;
            }
            if press.dragging {
                // Near the sidebar's top or bottom edge, it scrolls along.
                if py < SIDEBAR_HEADER as f64 + 24.0 {
                    self.thumb_scroll -= 12.0;
                } else if py > self.win.1 as f64 - 24.0 {
                    self.thumb_scroll += 12.0;
                }
                self.clamp_thumb_scroll();
                self.drop_gap = Some(self.gap_at(py));
                *needs_rebuild = true;
            }
            self.pointer = (px, py);
            return;
        }
        if let Some((page, _)) = &self.stroke {
            let page = *page;
            if let (Some(p), Some((_, pts))) = (self.point_on_page(page, px, py), self.stroke.as_mut()) {
                // Skip points closer than a quarter point: a slow drag
                // would otherwise pile up hundreds.
                if pts.last().is_none_or(|l| (l.0 - p.0).hypot(l.1 - p.1) > 0.25) {
                    pts.push(p);
                    *needs_rebuild = true;
                }
            }
            self.pointer = (px, py);
            return;
        }
        if self.editor.as_ref().is_some_and(|e| e.edit.dragging()) {
            if let (Some((r, size)), Some(ed), Some(fs)) = (self.editor_rect(), self.editor.as_mut(), self.font_system.as_mut()) {
                let at = chrome::offset_at(fs, &ed.edit, size, pos.x - r.x - 6.0);
                if ed.edit.drag_to(at) {
                    *needs_rebuild = true;
                }
            }
            self.pointer = (px, py);
            return;
        }
        if self.tool == Tool::Select && !self.selecting && self.drag.is_none() {
            // The note under the pointer shows its text.
            *needs_rebuild = true;
        }
        if self.selecting {
            if let (Some((head, _)), Some(sel)) = (self.mark_at(px, py, true), self.selection.as_mut()) {
                if sel.head != head {
                    sel.head = head;
                    *needs_rebuild = true;
                }
            }
        } else if let Some((lx, ly)) = self.drag {
            self.scroll_by(lx - px, ly - py);
            self.drag = Some((px, py));
            *needs_rebuild = true;
        }
        self.pointer = (px, py);
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        if button != MouseButton::Left {
            return None;
        }
        let (px, py) = (pos.x as f64, pos.y as f64);
        match state {
            ElementState::Pressed => {
                self.status = None;
                self.quit_armed = false;
                *needs_rebuild = true;
                let inside = |r: Rect| pos.x >= r.x && pos.x <= r.x + r.width && pos.y >= r.y && pos.y <= r.y + r.height;
                if let Some((buttons, rects)) = self.toolbar() {
                    if rects.iter().any(|r| inside(*r)) {
                        if let Some(action) = chrome::button_at(&buttons, &rects, pos.x, pos.y) {
                            self.run_action(action);
                        }
                        return None;
                    }
                }
                if self.sidebar && pos.x < SIDEBAR_W && !(self.find.is_some() && inside(self.find_rect())) {
                    self.sidebar_press(px, py);
                    return None;
                }
                // The find bar takes the keyboard when clicked; the page
                // takes it back.
                let in_find = self.find.is_some() && inside(self.find_rect());
                if let Some(f) = &mut self.find {
                    f.focused = in_find;
                }
                if in_find {
                    self.commit_editor();
                    return None;
                }
                // An open editor: a press in it places the caret, a press
                // anywhere else commits it (and does nothing more).
                if let Some((r, size)) = self.editor_rect() {
                    if inside(r) {
                        let shift = self.shift;
                        if let (Some(ed), Some(fs)) = (self.editor.as_mut(), self.font_system.as_mut()) {
                            let at = chrome::offset_at(fs, &ed.edit, size, pos.x - r.x - 6.0);
                            ed.edit.press(at, shift);
                        }
                    } else {
                        self.commit_editor();
                    }
                    return None;
                }
                if let Some((page, x, y)) = self.page_at(px, py, false).filter(|_| self.editable()) {
                    match self.tool {
                        Tool::Draw => {
                            self.stroke = Some((page, vec![(x, y)]));
                            return None;
                        }
                        Tool::Note => {
                            let anchor = PtRect { x0: x - 1.0, y0: y - 1.0, x1: x + 1.0, y1: y + 1.0 };
                            self.open_editor(page, EditTarget::Note { index: None, at: (x, y) }, String::new(), anchor);
                            return None;
                        }
                        Tool::Select => {
                            if let Some(a) = self.annots.get(&page).and_then(|l| annot_at(l, x, y)).cloned() {
                                self.press_annot(page, a);
                                return None;
                            }
                        }
                    }
                }
                self.picked = None;
                match self.mark_at(px, py, false) {
                    Some((mark, true)) if self.tool == Tool::Select => {
                        self.selection = Some(match self.selection {
                            Some(s) if self.shift => Selection { anchor: s.anchor, head: mark },
                            _ => Selection { anchor: mark, head: mark },
                        });
                        self.selecting = true;
                    }
                    _ => {
                        if self.selection.take().is_some() {
                            *needs_rebuild = true;
                        }
                        self.drag = Some((px, py));
                    }
                }
                *needs_rebuild = true;
            }
            ElementState::Released => {
                if let Some(press) = self.thumb_press.take() {
                    self.sidebar_release(press);
                    *needs_rebuild = true;
                    return None;
                }
                if let Some((page, points)) = self.stroke.take() {
                    if points.len() >= 2 {
                        self.edit(Edit::Ink { page, points, width: INK_PT });
                    }
                    *needs_rebuild = true;
                }
                if let Some(ed) = &mut self.editor {
                    ed.edit.release();
                }
                self.drag = None;
                self.selecting = false;
                if self.selection.is_some_and(|s| s.is_empty()) {
                    self.selection = None;
                    *needs_rebuild = true;
                }
            }
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        if self.sidebar && pos.x < SIDEBAR_W {
            let line = WHEEL_SCROLL_PX as f32;
            let (_, dy) = ScrollMotion::delta_px(delta, (line, line));
            self.thumb_scroll += dy as f64;
            self.clamp_thumb_scroll();
            *needs_rebuild = true;
            return;
        }
        if self.ctrl {
            // Zoom stays instant: a notch (or 60px of finger) is one 1.1 step.
            let (_, ny) = Self::notches(delta);
            if ny != 0.0 {
                self.zoom_at(1.1f64.powf(ny), pos.x as f64, pos.y as f64);
                *needs_rebuild = true;
            }
            return;
        }
        // Plain wheel: a 2-D scroll through the motion — a notch is
        // WHEEL_SCROLL_PX, pixel deltas are 1:1; shift turns the vertical
        // motion horizontal. `tick_scroll` carries `scroll` after it.
        let line = WHEEL_SCROLL_PX as f32;
        let (mut dx, mut dy) = ScrollMotion::delta_px(delta, (line, line));
        if self.shift {
            dx = dy;
            dy = 0.0;
        }
        let discrete = matches!(delta, MouseScrollDelta::LineDelta(..));
        let (bx, by) = self.scroll_bounds();
        self.scroll_motion.reconcile(self.scroll.0 as f32, self.scroll.1 as f32);
        if self.scroll_motion.apply_px(dx, dy, discrete, bx, by) {
            self.sync_scroll_from_motion();
            *needs_rebuild = true;
        }
    }

    fn handle_pinch(&mut self, factor: f32, pos: LogicalPosition, needs_rebuild: &mut bool) -> bool {
        if factor > 0.0 && factor != 1.0 {
            self.zoom_at(factor as f64, pos.x as f64, pos.y as f64);
            *needs_rebuild = true;
        }
        true
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        // Wheel events carry no modifiers, so track ctrl/shift from the key
        // stream for ctrl+wheel zoom / shift+wheel horizontal scroll.
        match &event.logical_key {
            Key::Named(NamedKey::Control) => self.ctrl = event.state == ElementState::Pressed,
            Key::Named(NamedKey::Shift) => self.shift = event.state == ElementState::Pressed,
            _ => {
                self.ctrl = event.ctrl;
                self.shift = event.shift;
            }
        }
        if event.state != ElementState::Pressed {
            return None;
        }
        log::debug!("key: {:?} text={:?} ctrl={} shift={}", event.logical_key, event.text, event.ctrl, event.shift);
        let chord = |c: &str| event.ctrl && matches!(&event.logical_key, Key::Character(k) if k.eq_ignore_ascii_case(c));
        let modifier = matches!(&event.logical_key, Key::Named(NamedKey::Control | NamedKey::Shift | NamedKey::Alt | NamedKey::Super));
        if !modifier {
            self.status = None;
            *needs_rebuild = true;
        }
        let is_q = !event.ctrl && matches!(&event.logical_key, Key::Character(c) if c == "q");
        if !modifier && !is_q {
            self.quit_armed = false;
        }

        // An editor over the page has the keyboard: Enter keeps the text,
        // Escape drops it; chords it does not own (Ctrl+S) fall through.
        if self.editor.is_some() && !chord("s") {
            let outcome = self.editor.as_mut().map(|e| e.edit.handle_key(event));
            match outcome {
                Some(EditOutcome::Edited) => return None,
                Some(EditOutcome::Submit) => {
                    self.commit_editor();
                    return None;
                }
                Some(EditOutcome::Cancel) => {
                    self.cancel_editor();
                    return None;
                }
                _ => {}
            }
        }

        // The find field has the keyboard: it edits, Enter steps through
        // matches, Escape closes it; chords it does not own fall through.
        if self.find.as_ref().is_some_and(|f| f.focused) && !chord("f") {
            let shift = event.shift;
            let outcome = self.find.as_mut().map(|f| f.edit.handle_key(event));
            match outcome {
                Some(EditOutcome::Edited) => {
                    self.restart_search();
                    *needs_rebuild = true;
                    return None;
                }
                Some(EditOutcome::Submit) => {
                    self.step_match(!shift);
                    *needs_rebuild = true;
                    return None;
                }
                Some(EditOutcome::Cancel) => {
                    self.close_find();
                    *needs_rebuild = true;
                    return None;
                }
                _ => {}
            }
        }

        if event.ctrl {
            let mut handled = true;
            if chord("f") {
                self.open_find();
                self.restart_search();
            } else if chord("c") {
                if let Some(text) = self.selected_text() {
                    cce_ui::widget::clipboard::copy_to_clipboard(&text);
                }
            } else if chord("a") {
                self.select_all();
            } else if chord("g") {
                self.step_match(!event.shift);
            } else if chord("p") {
                self.print();
            } else if chord("e") {
                self.export();
            } else if !self.editable() && (chord("s") || chord("z") || chord("y")) {
                if self.doc.as_ref().is_some_and(|d| d.writing) {
                    self.status = Some("Edit the Markdown in your editor; the pages follow each save. Ctrl+E exports the PDF".to_string());
                }
            } else if chord("s") {
                self.save(event.shift);
            } else if chord("z") && event.shift || chord("y") {
                self.redo();
            } else if chord("z") {
                self.undo();
            } else {
                handled = false;
            }
            if handled {
                *needs_rebuild = true;
            }
            return None;
        }

        let (cx, cy) = (self.view_x() + self.view_w() / 2.0, self.win.1 as f64 / 2.0);
        let pages = self.doc.as_ref().map_or(0, |d| d.pages.len());
        let mut handled = true;
        match &event.logical_key {
            Key::Character(c) if c == "+" || c == "=" => self.zoom_at(1.25, cx, cy),
            Key::Character(c) if c == "-" => self.zoom_at(0.8, cx, cy),
            Key::Character(c) if c == "0" => self.fit_page(self.current_page()),
            Key::Character(c) if c == "1" => {
                let f = 1.0 / self.zoom;
                self.zoom_at(f, cx, cy);
            }
            Key::Character(c) if (c == "r" || c == "R") && self.sidebar && !self.thumb_sel.is_empty() && self.editable() => self.rotate_pages(1),
            Key::Character(c) if (c == "l" || c == "L") && self.sidebar && !self.thumb_sel.is_empty() && self.editable() => self.rotate_pages(-1),
            Key::Character(c) if c == "r" || c == "R" => self.rotate(1),
            Key::Character(c) if c == "l" || c == "L" => self.rotate(-1),
            Key::Character(c) if c == "t" => self.toggle_sidebar(),
            Key::Character(c) if c == "o" => self.open_dialog(),
            Key::Character(c) if c == "q" => {
                if self.dirty && !self.quit_armed {
                    self.quit_armed = true;
                    self.status = Some("Unsaved changes: Ctrl+S saves them, q again quits without".to_string());
                } else {
                    return Some(Message::Quit);
                }
            }
            Key::Character(c) if c == "h" && self.editable() => self.mark_selection(MarkupKind::Highlight),
            Key::Character(c) if c == "u" && self.editable() => self.mark_selection(MarkupKind::Underline),
            Key::Character(c) if c == "s" && self.editable() => self.mark_selection(MarkupKind::StrikeOut),
            Key::Character(c) if c == "d" && self.editable() => self.set_tool(Tool::Draw),
            Key::Character(c) if c == "n" && self.editable() => self.set_tool(Tool::Note),
            Key::Character(c) if c == "v" => self.set_tool(Tool::Select),
            Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) if self.picked.is_some() => {
                if let Some((page, index)) = self.picked.take() {
                    self.edit(Edit::Delete { page, index });
                }
            }
            Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) if self.sidebar && !self.thumb_sel.is_empty() && self.editable() => self.delete_pages(),
            Key::Character(c) if c == "/" => {
                self.open_find();
                self.restart_search();
            }
            Key::Named(NamedKey::F3) => self.step_match(!event.shift),
            Key::Named(NamedKey::Escape) if self.find.is_some() => self.close_find(),
            Key::Named(NamedKey::Escape) if self.picked.is_some() => self.picked = None,
            Key::Named(NamedKey::Escape) if self.selection.is_some() => self.selection = None,
            Key::Named(NamedKey::Escape) if !self.thumb_sel.is_empty() => self.thumb_sel.clear(),
            Key::Named(NamedKey::Escape) => self.set_tool(Tool::Select),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(0.0, -KEY_SCROLL_PX),
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(0.0, KEY_SCROLL_PX),
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::PageUp) => {
                let p = self.current_page();
                self.go_to_page(p.saturating_sub(1));
            }
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::PageDown) => {
                let p = self.current_page();
                self.go_to_page((p + 1).min(pages.saturating_sub(1)));
            }
            Key::Named(NamedKey::Space) => self.scroll_by(0.0, self.win.1 as f64 * 0.9),
            Key::Named(NamedKey::Home) => self.go_to_page(0),
            Key::Named(NamedKey::End) => self.go_to_page(pages.saturating_sub(1)),
            _ => handled = false,
        }
        if handled {
            *needs_rebuild = true;
        }
        None
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        self.win = (size.width, size.height);
        self.scale = scale;
        self.store.begin_frame();
        self.thumbs.begin_frame();
        let mut pc = PaintCtx::new();
        // The standard root plate (cce-ui PlateSpec::window); the document is
        // full-bleed content drawn on it.
        pc.root_plate(size.width, size.height);

        if self.doc.is_none() {
            let msg = self.error.as_deref().unwrap_or("Press 'o' to open a file");
            pc.text(msg, cce_ui::layout::root_plate_inset(), size.height / 2.0 - 8.0, 14.0, [180, 180, 180]);
            if self.find.is_some() {
                self.paint_find(&mut pc);
            }
            return Some(pc.finish());
        }

        let (rects, cw, ch) = self.layout();
        let (ox, oy) = self.origin(cw, ch);
        let want_dpi = {
            let want = 72.0 * self.zoom * scale;
            *DPI_BUCKETS
                .iter()
                .find(|&&b| want <= b as f64 * 1.01)
                .unwrap_or(DPI_BUCKETS.last().unwrap())
        };

        let mut visible = Vec::new();
        for (i, r) in rects.iter().enumerate() {
            let rect = Rect {
                x: (ox + r.x * self.zoom) as f32,
                y: (oy + r.y * self.zoom) as f32,
                width: (r.w * self.zoom) as f32,
                height: (r.h * self.zoom) as f32,
            };
            if rect.y > size.height || rect.y + rect.height < 0.0 {
                continue;
            }
            visible.push((i, rect));
        }
        // Pages, and everything drawn over them, stay right of the sidebar.
        pc.push_clip(self.view_rect());
        let doc = self.doc.take().unwrap();
        for (i, rect) in &visible {
            // White page ground: placeholder while rendering.
            pc.quad(
                Rect { x: rect.x - 1.0, y: rect.y - 1.0, width: rect.width + 2.0, height: rect.height + 2.0 },
                [0.0, 0.0, 0.0, 0.35],
            );
            pc.quad(*rect, [0.97, 0.97, 0.97, 1.0]);
            if let Some(r) = self.store.ensure(&self.backend, &doc, self.quarter_turns, *i, want_dpi) {
                pc.image(r.image, *rect, 1.0);
            }
        }
        self.doc = Some(doc);

        // Text for every page on screen, so a press can select without
        // waiting; then the matches and the selection over the pages.
        for (i, _) in &visible {
            self.want_text(*i);
            self.want_annots(*i);
        }
        if let Some(f) = &self.find {
            for (n, (page, hit)) in f.hits.iter().enumerate() {
                if visible.iter().any(|(i, _)| i == page) {
                    let color = if f.current == Some(n) { MATCH_CURRENT } else { MATCH };
                    for r in hit {
                        pc.quad(self.screen_rect(&rects, (ox, oy), *page, r), color);
                    }
                }
            }
        }
        if let Some(sel) = self.selection {
            for (i, _) in &visible {
                let Some(text) = self.texts.get(i) else { continue };
                let Some((a, b)) = sel.range_on(*i, text.len()) else { continue };
                for r in text.rects(a, b) {
                    pc.quad(self.screen_rect(&rects, (ox, oy), *i, &r), SELECTION);
                }
            }
        }

        self.paint_overlays(&mut pc, &rects, (ox, oy));
        pc.pop_clip();
        if self.sidebar {
            self.paint_sidebar(&mut pc, scale);
        }

        // HUD: file name, page, zoom (top-left chip).
        let doc = self.doc.as_ref().unwrap();
        let name = doc.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let mut hud = name.to_string();
        if doc.pages.len() > 1 {
            hud.push_str(&format!("   ·   page {}/{}", self.current_page() + 1, doc.pages.len()));
        }
        hud.push_str(&format!("   ·   {:.0}%", self.zoom * 100.0));
        // The HUD stands the root plate's inset off the window corner, its
        // text the control text inset inside the box.
        let inset = cce_ui::layout::root_plate_inset();
        let text_in = cce_ui::layout::CONTROL_TEXT_INSET;
        let x0 = self.view_x() as f32 + inset;
        let fs = self.font_system.get_or_insert_with(cce_ui::create_font_system);
        let w = 2.0 * text_in + chrome::width_of(fs, &hud, 12.0);
        pc.quad(Rect { x: x0, y: inset, width: w, height: 24.0 }, [0.0, 0.0, 0.0, 0.45]);
        pc.text(hud, x0 + text_in, inset + 5.0, 12.0, [230, 230, 230]);
        if let Some(status) = self.status.clone() {
            let fs = self.font_system.get_or_insert_with(cce_ui::create_font_system);
            let w = 2.0 * text_in + chrome::width_of(fs, &status, 12.0);
            pc.quad(Rect { x: x0, y: inset + 30.0, width: w, height: 24.0 }, [0.0, 0.0, 0.0, 0.6]);
            pc.text(status, x0 + text_in, inset + 35.0, 12.0, [255, 220, 150]);
        }
        if let Some((buttons, rects)) = self.toolbar() {
            chrome::paint_toolbar(&mut pc, &buttons, &rects);
        }

        if self.find.is_some() {
            self.paint_find(&mut pc);
        }

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn clear_color(&self) -> [f32; 4] {
        [0.13, 0.13, 0.14, 1.0]
    }
}

fn main() {
    env_logger::init();
    cce_ui::engine::run::<DocumentsApp>();
}
