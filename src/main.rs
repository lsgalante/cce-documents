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
//! Keys: o open · +/- zoom · 0 fit · 1 actual size · r/l rotate ·
//! arrows/PageUp/PageDown/Home/End pages · Ctrl+F find · Enter/F3 next match
//! (Shift: previous) · Esc close find or clear the selection · q quit.
//! Wheel scrolls, ctrl+wheel and pinch zoom at the pointer, drag pans (or
//! selects, when the press lands on text).

mod doc;
mod engine;
mod poppler;
mod text;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cce_ui::engine::{Application, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::line_edit::EditOutcome;
use cce_ui::widget::scroll_motion::{Bounds, ScrollMotion};
use cce_ui::widget::{ElementState, Key, KeyEvent, LineEdit, MouseButton, MouseScrollDelta, NamedKey, Position};

use doc::{Backend, Document, PageStore, Rendered};
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

#[derive(Debug, Clone)]
enum Message {
    Page { generation: u64, page: usize, result: Option<Rendered> },
    Text { doc: u64, page: usize, text: Arc<PageText> },
    Hits { generation: u64, page: usize, hits: Vec<Vec<PtRect>> },
    SearchDone { generation: u64 },
    Quit,
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

struct DocumentsApp {
    backend: Backend,
    store: PageStore,
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
    /// For measuring the find field's caret; made when find first opens.
    font_system: Option<cce_ui::cosmic_text::FontSystem>,
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

    /// Top-left of the content in screen coords: centered when it fits,
    /// scrolled when it doesn't.
    fn origin(&self, content_w: f64, content_h: f64) -> (f64, f64) {
        let (w, h) = (self.win.0 as f64, self.win.1 as f64);
        let ox = ((w - content_w * self.zoom) / 2.0).max(0.0) - self.scroll.0;
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
        let (w, h) = (self.win.0 as f64, self.win.1 as f64);
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
        let (w, h) = (self.win.0 as f64, self.win.1 as f64);
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
        let (w, h) = (self.win.0 as f64, self.win.1 as f64);
        let pad_x = ((w - cw * self.zoom) / 2.0).max(0.0);
        let pad_y = ((h - ch * self.zoom) / 2.0).max(0.0);
        self.scroll.0 = pad_x - (px - dx * self.zoom);
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
        let (w, h) = ((self.win.0 as f64 - fit_margin()).max(64.0), (self.win.1 as f64 - fit_margin()).max(64.0));
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
        let (w, h) = (self.win.0, self.win.1);
        if s.y < 0.0 || s.y + s.height > h {
            self.scroll.1 += (s.y - h / 3.0) as f64;
        }
        if s.x < 0.0 || s.x + s.width > w {
            self.scroll.0 += (s.x + s.width / 2.0 - w / 2.0) as f64;
        }
        self.clamp_scroll();
    }

    fn open(&mut self, path: &Path) {
        self.store.reset();
        self.quarter_turns = 0;
        self.error = None;
        self.texts.clear();
        self.texts_asked.clear();
        self.selection = None;
        self.selecting = false;
        self.next_doc += 1;
        match self.backend.open(path, self.next_doc) {
            Ok(d) => {
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

    fn open_dialog(&mut self) {
        let filters: &[(&str, &[&str])] = &[("PDF", &["pdf"])];
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
        let count_w = if count.is_empty() { 0.0 } else { cce_ui::engine::shaped_cluster_offsets(fs, &count, FIND_FONT, None).last().map_or(0.0, |&(_, x)| x) };
        let ty = cce_ui::layout::align_text_y(r.y, r.height, FIND_FONT, 0.0);
        if !count.is_empty() {
            pc.text(count, r.x + r.width - pad - count_w, ty, FIND_FONT, [170, 170, 170]);
        }
        let field = Rect { x: r.x + pad, y: r.y, width: (r.width - 3.0 * pad - count_w).max(0.0), height: r.height };
        if f.focused {
            f.edit.sync_ime();
        }
        let shown = f.edit.display();
        let x_of = |fs: &mut cce_ui::cosmic_text::FontSystem, byte: usize| {
            cce_ui::engine::shaped_cluster_offsets(fs, &shown, FIND_FONT, None)
                .iter()
                .rev()
                .find(|&&(b, _)| b <= byte)
                .map_or(0.0, |&(_, x)| x)
        };
        let caret = x_of(fs, f.edit.display_index(f.edit.cursor));
        let sel = f.edit.selection.filter(|&(a, b)| a < b).map(|(a, b)| (x_of(fs, f.edit.display_index(a)), x_of(fs, f.edit.display_index(b))));
        let focused = f.focused;
        let placeholder = if has_engine { "Find in document" } else { "Search needs PDFium: run scripts/fetch-pdfium" };
        pc.clip(field, |pc| {
            if let Some((a, b)) = sel {
                pc.quad(Rect { x: field.x + a, y: field.y + 6.0, width: b - a, height: field.height - 12.0 }, SELECTION);
            }
            if shown.is_empty() {
                pc.text(placeholder.to_string(), field.x, ty, FIND_FONT, [130, 130, 130]);
            } else {
                pc.text(shown.clone(), field.x, ty, FIND_FONT, [235, 235, 235]);
            }
            if focused {
                pc.quad(Rect { x: field.x + caret, y: field.y + 6.0, width: 1.0, height: field.height - 12.0 }, [1.0, 1.0, 1.0, 0.9]);
            }
        });
        if focused {
            cce_ui::ime::report_caret(field.x + caret, field.y + 6.0, 1.0, field.height - 12.0);
        }
    }
}

impl Application for DocumentsApp {
    type Message = Message;

    fn create(sender: cce_ui::engine::AppSender<Self::Message>) -> Self {
        // The app keeps calloop's sender; `AppSender` converts into it.
        let sender: calloop::channel::Sender<Self::Message> = sender.into();
        let mut app = Self {
            backend: Backend::start(sender),
            store: PageStore::new(),
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
            Message::Page { generation, page, result } => {
                self.store.complete(generation, page, result);
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
                // The find bar takes the keyboard when clicked; the page
                // takes it back.
                let in_find = self.find.is_some() && {
                    let r = self.find_rect();
                    pos.x >= r.x && pos.x <= r.x + r.width && pos.y >= r.y && pos.y <= r.y + r.height
                };
                if let Some(f) = &mut self.find {
                    f.focused = in_find;
                    *needs_rebuild = true;
                }
                if in_find {
                    return None;
                }
                match self.mark_at(px, py, false) {
                    Some((mark, true)) => {
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
            } else {
                handled = false;
            }
            if handled {
                *needs_rebuild = true;
            }
            return None;
        }

        let (cx, cy) = (self.win.0 as f64 / 2.0, self.win.1 as f64 / 2.0);
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
            Key::Character(c) if c == "r" || c == "R" => self.rotate(1),
            Key::Character(c) if c == "l" || c == "L" => self.rotate(-1),
            Key::Character(c) if c == "o" => self.open_dialog(),
            Key::Character(c) if c == "q" => return Some(Message::Quit),
            Key::Character(c) if c == "/" => {
                self.open_find();
                self.restart_search();
            }
            Key::Named(NamedKey::F3) => self.step_match(!event.shift),
            Key::Named(NamedKey::Escape) if self.find.is_some() => self.close_find(),
            Key::Named(NamedKey::Escape) => self.selection = None,
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
        let w = 2.0 * text_in + hud.chars().count() as f32 * 6.6;
        pc.quad(Rect { x: inset, y: inset, width: w, height: 24.0 }, [0.0, 0.0, 0.0, 0.45]);
        pc.text(hud, inset + text_in, inset + 5.0, 12.0, [230, 230, 230]);

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
