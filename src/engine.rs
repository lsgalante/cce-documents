//! The PDFium backend: one thread that owns the library and the open
//! document, and does everything that touches them.
//!
//! PDFium is not thread-safe, and a `PdfDocument` borrows the `Pdfium` that
//! loaded it, so both live on this thread for its whole life and the app
//! talks to it over a channel. Renders, text and search answers come back as
//! `Message`s through calloop (they wake the app); opening a file and
//! reading a page's text for a copy are answered directly, because the
//! caller is waiting on them.
//!
//! **Search never holds up the page.** It runs one page per turn of the
//! loop, and every turn first takes whatever requests are queued — so a
//! render asked for mid-search lands before the next page is searched, and
//! a new query replaces the old one at once.
//!
//! **Coordinates.** PDFium answers in page space (points, origin bottom-left
//! of the media box, before /Rotate). Everything leaving this module is in
//! display points instead (`text` module): `Frame` maps one to the other
//! from the page's bounding box and rotation.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use pdfium_render::prelude::*;

use crate::doc::{Job, PageSize};
use crate::text::{PageText, PtRect, TextChar};
use crate::Message;

enum Request {
    Open { id: u64, path: PathBuf, reply: mpsc::Sender<Result<Vec<PageSize>, String>> },
    Render(Job),
    Text { doc: u64, page: usize },
    TextNow { doc: u64, page: usize, reply: mpsc::Sender<Option<Arc<PageText>>> },
    Search { doc: u64, generation: u64, query: String },
}

pub struct Engine {
    tx: mpsc::Sender<Request>,
}

/// Where to look for `libpdfium.so`, in order: an explicit override, beside
/// the binary, then `~/.local/lib` (where `scripts/fetch-pdfium` puts it).
/// The system library path is tried after these.
fn pdfium_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("CCE_PDFIUM") {
        out.push(PathBuf::from(p));
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        out.push(Pdfium::pdfium_platform_library_name_at_path(&dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(Pdfium::pdfium_platform_library_name_at_path(&PathBuf::from(home).join(".local/lib")));
    }
    out
}

fn bind() -> Result<Pdfium, String> {
    for path in pdfium_paths() {
        if !path.exists() {
            continue;
        }
        match Pdfium::bind_to_library(&path) {
            Ok(b) => {
                log::info!("PDFium from {}", path.display());
                return Ok(Pdfium::new(b));
            }
            Err(e) => log::warn!("PDFium at {}: {e}", path.display()),
        }
    }
    Pdfium::bind_to_system_library().map(Pdfium::new).map_err(|e| format!("no libpdfium found: {e}"))
}

impl Engine {
    /// Start the thread and load the library on it. Fails (and the thread
    /// ends) when no PDFium can be loaded.
    pub fn start(notify: calloop::channel::Sender<Message>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("pdfium".into())
            .spawn(move || match bind() {
                Ok(pdfium) => {
                    let _ = ready_tx.send(Ok(()));
                    run(&pdfium, rx, notify);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { tx })
    }

    /// Load `path` as document `id`, replacing the open one; its page sizes.
    pub fn open(&self, path: &Path, id: u64) -> Result<Vec<PageSize>, String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Request::Open { id, path: path.to_path_buf(), reply }).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }

    pub fn render(&self, job: Job) {
        let _ = self.tx.send(Request::Render(job));
    }

    /// Ask for a page's text; it arrives as `Message::Text`.
    pub fn request_text(&self, doc: u64, page: usize) {
        let _ = self.tx.send(Request::Text { doc, page });
    }

    /// A page's text, waiting for it (for a copy that needs a page the
    /// screen never showed).
    pub fn text_now(&self, doc: u64, page: usize) -> Option<Arc<PageText>> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Request::TextNow { doc, page, reply }).ok()?;
        rx.recv().ok().flatten()
    }

    /// Search the document for `query`, replacing any search under way; an
    /// empty query just stops it. Hits arrive page by page as
    /// `Message::Hits`, then `Message::SearchDone`.
    pub fn search(&self, doc: u64, generation: u64, query: String) {
        let _ = self.tx.send(Request::Search { doc, generation, query });
    }
}

struct Search {
    doc: u64,
    generation: u64,
    query: String,
    next_page: usize,
}

fn run(pdfium: &Pdfium, rx: mpsc::Receiver<Request>, notify: calloop::channel::Sender<Message>) {
    let mut open: Option<(u64, PdfDocument)> = None;
    let mut search: Option<Search> = None;
    loop {
        let req = if search.is_some() {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => return,
            }
        };
        let doc = |id: u64, open: &Option<(u64, PdfDocument<'_>)>| open.as_ref().filter(|(d, _)| *d == id).is_some();
        match req {
            Some(Request::Open { id, path, reply }) => {
                open = None;
                search = None;
                let result = pdfium
                    .load_pdf_from_file(&path, None)
                    .map_err(|e| format!("PDFium: {e}"))
                    .map(|d| {
                        let sizes = d.pages().iter().map(|p| Frame::of(&p).size()).collect();
                        open = Some((id, d));
                        sizes
                    });
                let _ = reply.send(result);
            }
            Some(Request::Render(job)) => {
                let result = match &open {
                    Some((id, d)) if *id == job.doc => render(d, &job)
                        .map(|rgba| job.finish(rgba))
                        .map_err(|e| log::warn!("{}: page {}: {e}", job.path.display(), job.page + 1))
                        .ok(),
                    _ => None,
                };
                let msg = Message::Page { generation: job.generation, page: job.page, result };
                if notify.send(msg).is_err() {
                    return;
                }
            }
            Some(Request::Text { doc: id, page }) => {
                if let (true, Some((_, d))) = (doc(id, &open), &open) {
                    let text = Arc::new(page_text(d, page));
                    if notify.send(Message::Text { doc: id, page, text }).is_err() {
                        return;
                    }
                }
            }
            Some(Request::TextNow { doc: id, page, reply }) => {
                let text = match &open {
                    Some((d_id, d)) if *d_id == id => Some(Arc::new(page_text(d, page))),
                    _ => None,
                };
                let _ = reply.send(text);
            }
            Some(Request::Search { doc: id, generation, query }) => {
                search = (!query.is_empty() && doc(id, &open)).then_some(Search { doc: id, generation, query, next_page: 0 });
            }
            None => {
                // A turn with nothing queued: search one more page.
                let Some(s) = search.as_mut() else { continue };
                let Some((_, d)) = open.as_ref().filter(|(id, _)| *id == s.doc) else {
                    search = None;
                    continue;
                };
                let count = d.pages().len() as usize;
                if s.next_page >= count {
                    let _ = notify.send(Message::SearchDone { generation: s.generation });
                    search = None;
                    continue;
                }
                let page = s.next_page;
                s.next_page += 1;
                let hits = search_page(d, page, &s.query);
                if !hits.is_empty() && notify.send(Message::Hits { generation: s.generation, page, hits }).is_err() {
                    return;
                }
            }
        }
    }
}

/// How page space maps onto display points for one page.
struct Frame {
    left: f64,
    top: f64,
    w: f64,
    h: f64,
    /// The page's own /Rotate in quarter turns clockwise.
    rot: u8,
}

impl Frame {
    fn of(page: &PdfPage) -> Self {
        let rot = match page.rotation() {
            Ok(PdfPageRenderRotation::Degrees90) => 1,
            Ok(PdfPageRenderRotation::Degrees180) => 2,
            Ok(PdfPageRenderRotation::Degrees270) => 3,
            _ => 0,
        };
        // The bounding box is the crop box clipped to the media box: the
        // area PDFium renders.
        match page.boundaries().bounding() {
            Ok(b) => {
                let r = b.bounds;
                Frame {
                    left: r.left().value as f64,
                    top: r.top().value as f64,
                    w: (r.right().value - r.left().value) as f64,
                    h: (r.top().value - r.bottom().value) as f64,
                    rot,
                }
            }
            Err(_) => {
                let (pw, ph) = (page.width().value as f64, page.height().value as f64);
                let (w, h) = if rot % 2 == 1 { (ph, pw) } else { (pw, ph) };
                Frame { left: 0.0, top: h, w, h, rot }
            }
        }
    }

    fn size(&self) -> PageSize {
        if self.rot % 2 == 1 {
            PageSize { w: self.h, h: self.w }
        } else {
            PageSize { w: self.w, h: self.h }
        }
    }

    fn point(&self, x: f64, y: f64) -> (f64, f64) {
        let (dx, dy) = (x - self.left, self.top - y);
        match self.rot {
            1 => (self.h - dy, dx),
            2 => (self.w - dx, self.h - dy),
            3 => (dy, self.w - dx),
            _ => (dx, dy),
        }
    }

    fn rect(&self, r: &PdfRect) -> PtRect {
        let (ax, ay) = self.point(r.left().value as f64, r.top().value as f64);
        let (bx, by) = self.point(r.right().value as f64, r.bottom().value as f64);
        PtRect { x0: ax.min(bx), y0: ay.min(by), x1: ax.max(bx), y1: ay.max(by) }
    }
}

fn render(doc: &PdfDocument, job: &Job) -> Result<image::RgbaImage, String> {
    let page = doc.pages().get(job.page as PdfPageIndex).map_err(|e| e.to_string())?;
    let scale = job.capped_dpi() as f64 / 72.0;
    let w = ((job.size.w * scale).round() as i32).max(1);
    let h = ((job.size.h * scale).round() as i32).max(1);
    let bitmap = page.render(w, h, None).map_err(|e| e.to_string())?;
    image::RgbaImage::from_raw(bitmap.width() as u32, bitmap.height() as u32, bitmap.as_rgba_bytes())
        .ok_or_else(|| "PDFium: bitmap size mismatch".to_string())
}

fn page_text(doc: &PdfDocument, index: usize) -> PageText {
    let Ok(page) = doc.pages().get(index as PdfPageIndex) else { return PageText::default() };
    let Ok(text) = page.text() else { return PageText::default() };
    let frame = Frame::of(&page);
    let chars = text
        .chars()
        .iter()
        .filter_map(|c| {
            let ch = c.unicode_char()?;
            let rect = c.loose_bounds().map(|r| frame.rect(&r)).unwrap_or(PtRect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 });
            // A generated char (an inferred space or line break) has no
            // glyph; keep it for copying, without a box to hit.
            let rect = if c.is_generated().unwrap_or(false) { PtRect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 } } else { rect };
            Some(TextChar { ch, rect })
        })
        .collect();
    PageText { chars }
}

/// Every match of `query` on one page, each as the rects it covers (a match
/// can wrap onto a second line).
fn search_page(doc: &PdfDocument, index: usize, query: &str) -> Vec<Vec<PtRect>> {
    let Ok(page) = doc.pages().get(index as PdfPageIndex) else { return Vec::new() };
    let Ok(text) = page.text() else { return Vec::new() };
    let Ok(search) = text.search(query, &PdfSearchOptions::new()) else { return Vec::new() };
    let frame = Frame::of(&page);
    let mut hits = Vec::new();
    while let Some(segments) = search.find_next() {
        let rects: Vec<PtRect> = segments.iter().map(|s| frame.rect(&s.bounds())).collect();
        if !rects.is_empty() {
            hits.push(rects);
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A match found on a page and on a copy of it turned 90° by its /Rotate
    /// must land in the same place once the turn is undone: the check that
    /// `Frame` maps page space onto display points as PDFium renders it.
    /// Needs PDFium and qpdf, and a PDF with text; skips without them.
    #[test]
    fn a_rotated_page_maps_its_text_where_it_is_drawn() {
        let src = Path::new("/usr/share/cups/data/form_english.pdf");
        let Ok(pdfium) = bind() else { return eprintln!("skipped: no PDFium") };
        if !src.exists() {
            return eprintln!("skipped: no {}", src.display());
        }
        let dir = std::env::temp_dir().join(format!("cce-documents-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let turned = dir.join("turned.pdf");
        let ok = std::process::Command::new("qpdf")
            .args(["--rotate=+90:1"])
            .arg(src)
            .arg(&turned)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return eprintln!("skipped: qpdf failed");
        }
        let plain = pdfium.load_pdf_from_file(src, None).unwrap();
        let rotated = pdfium.load_pdf_from_file(&turned, None).unwrap();
        let size = Frame::of(&plain.pages().get(0).unwrap()).size();
        let turned_size = Frame::of(&rotated.pages().get(0).unwrap()).size();
        assert!((turned_size.w - size.h).abs() < 0.5 && (turned_size.h - size.w).abs() < 0.5, "{size:?} vs {turned_size:?}");
        // And the size agrees with what PDFium itself reports for the page.
        let p = rotated.pages().get(0).unwrap();
        assert!((turned_size.w - p.width().value as f64).abs() < 0.5);

        let a = search_page(&plain, 0, "Printer");
        let b = search_page(&rotated, 0, "Printer");
        assert!(!a.is_empty(), "no match on the plain page");
        assert_eq!(a.len(), b.len());
        for (ra, rb) in a.iter().flatten().zip(b.iter().flatten()) {
            // Turning (x, y) a quarter clockwise on a page of height h gives
            // (h - y, x).
            let want = PtRect { x0: size.h - ra.y1, y0: ra.x0, x1: size.h - ra.y0, y1: ra.x1 };
            for (w, g) in [(want.x0, rb.x0), (want.y0, rb.y0), (want.x1, rb.x1), (want.y1, rb.y1)] {
                assert!((w - g).abs() < 0.5, "want {want:?}, got {rb:?}");
            }
        }
        // The text the matches cover reads as the query.
        let text = page_text(&plain, 0);
        assert!(text.text(0, text.len()).contains("Printer name"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
