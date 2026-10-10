//! The PDFium backend: one thread that owns the library and the open
//! document, and does everything that touches them.
//!
//! PDFium is not thread-safe, so the library, the document, its form-fill
//! environment and its loaded pages all live on this thread, and the app
//! talks to it over a channel. Renders, text, annotations, edits and search
//! answers come back as `Message`s through calloop (they wake the app);
//! opening, saving, a field's value and a page's text for a copy are
//! answered directly, because the caller is waiting on them.
//!
//! **PDFium's C API, not pdfium-render's wrappers.** pdfium-render only
//! loads the library and supplies the function table and types. Its
//! high-level API keeps the form handle private, never registers pages with
//! the form-fill environment (`FORM_OnAfterLoadPage`), has no ink strokes,
//! and saves only whole files — and form filling, ink and incremental saves
//! are this module's job. So `Pdfium::new` is never called: the bindings
//! are used directly, `FPDF_InitLibrary` first.
//!
//! **Saving replays a journal onto a fresh copy.** PDFium's incremental
//! save writes every object it has loaded, changed or not — after a page is
//! rendered, its fonts and images too, so a save of one highlight on a
//! scanned book would add the book again. The open document is therefore
//! only ever looked at: each edit is applied to it and recorded, and a save
//! loads the file anew (never rendered), replays the record onto it, and
//! saves that. The update then holds what the edits touched, and the next
//! save appends to the file just written.
//!
//! **Undo replays too.** Taking back a step reloads the file and replays the
//! journal without it, so every kind of change — marks, field values, page
//! operations — undoes the same way; redo applies the step again.
//!
//! **Search never holds up the page.** It runs one page per turn of the
//! loop, and every turn first takes whatever requests are queued.
//!
//! **Coordinates.** PDFium answers in page space (points, origin bottom-left
//! of the media box, before /Rotate). Everything crossing this module's
//! boundary is in display points (`text` module): `Frame` maps both ways
//! from the page's bounding box and rotation.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::{c_int, c_ulong};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

use pdfium_render::prelude::{
    Pdfium, PdfiumLibraryBindings, FPDF_ANNOTATION, FPDF_DOCUMENT, FPDF_FILEWRITE, FPDF_FORMFILLINFO,
    FPDF_FORMHANDLE, FPDF_PAGE, FPDF_WCHAR, FS_POINTF, FS_QUADPOINTSF, FS_RECTF,
};

use crate::doc::{Job, PageSize};
use crate::markup::{move_dest, Annot, AnnotKind, Edit, FieldKind, MarkupKind, PageOp};
use crate::text::{PageText, PtRect, TextChar};
use crate::Message;

// fpdfview.h / fpdf_annot.h / fpdf_formfill.h / fpdf_save.h constants
// (pdfium-render re-exports the types, not these).
const FPDF_ANNOT: c_int = 0x01;
const FPDF_ERR_PASSWORD: c_ulong = 4;
const ANNOT_TEXT: c_int = 1;
const ANNOT_HIGHLIGHT: c_int = 9;
const ANNOT_UNDERLINE: c_int = 10;
const ANNOT_STRIKEOUT: c_int = 12;
const ANNOT_INK: c_int = 15;
const ANNOT_WIDGET: c_int = 20;
const FLAG_PRINT: c_int = 4;
const FLAG_NOZOOM: c_int = 8;
const FLAG_NOROTATE: c_int = 16;
const COLOR_STROKE: u32 = 0;
const FIELD_CHECKBOX: c_int = 2;
const FIELD_RADIO: c_int = 3;
const FIELD_TEXT: c_int = 6;
const FIELD_READONLY: c_int = 1;
const FPDF_INCREMENTAL: c_ulong = 1;

/// The color each kind of mark is made in (RGB).
const HIGHLIGHT_RGB: [u32; 3] = [255, 214, 0];
const UNDERLINE_RGB: [u32; 3] = [30, 110, 255];
const STRIKEOUT_RGB: [u32; 3] = [220, 40, 40];
const INK_RGB: [u32; 3] = [200, 30, 60];
const NOTE_RGB: [u32; 3] = [255, 200, 0];
/// A note's icon, square, in points.
const NOTE_PT: f32 = 20.0;

type Lib = Box<dyn PdfiumLibraryBindings>;

enum Request {
    Open { id: u64, path: PathBuf, reply: mpsc::Sender<Result<Vec<PageSize>, String>> },
    Render(Job),
    Text { doc: u64, page: usize },
    TextNow { doc: u64, page: usize, reply: mpsc::Sender<Option<Arc<PageText>>> },
    Annots { doc: u64, page: usize },
    Edit { doc: u64, edit: Edit },
    Pages { doc: u64, op: PageOp },
    Undo { doc: u64 },
    Redo { doc: u64 },
    Extract { doc: u64, pages: Vec<usize>, to: PathBuf, reply: mpsc::Sender<Result<(), String>> },
    FieldValue { doc: u64, page: usize, index: usize, reply: mpsc::Sender<Option<String>> },
    Save { doc: u64, to: PathBuf, reply: mpsc::Sender<Result<(), String>> },
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

/// Load the library and initialise it. Once per process.
fn bind() -> Result<Lib, String> {
    let mut found = None;
    for path in pdfium_paths() {
        if !path.exists() {
            continue;
        }
        match Pdfium::bind_to_library(&path) {
            Ok(b) => {
                log::info!("PDFium from {}", path.display());
                found = Some(b);
                break;
            }
            Err(e) => log::warn!("PDFium at {}: {e}", path.display()),
        }
    }
    let lib = match found {
        Some(lib) => lib,
        None => Pdfium::bind_to_system_library().map_err(|e| format!("no libpdfium found: {e}"))?,
    };
    unsafe { lib.FPDF_InitLibrary() };
    Ok(lib)
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
                Ok(lib) => {
                    let _ = ready_tx.send(Ok(()));
                    run(&lib, rx, notify);
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

    /// Ask for a page's annotations; they arrive as `Message::Annots`.
    pub fn request_annots(&self, doc: u64, page: usize) {
        let _ = self.tx.send(Request::Annots { doc, page });
    }

    /// Change the document. `Message::Edited` and a fresh `Message::Annots`
    /// for the page follow.
    pub fn edit(&self, doc: u64, edit: Edit) {
        let _ = self.tx.send(Request::Edit { doc, edit });
    }

    /// Rearrange pages. `Message::Restructured` follows.
    pub fn pages(&self, doc: u64, op: PageOp) {
        let _ = self.tx.send(Request::Pages { doc, op });
    }

    /// Take back the last change since the file was opened or saved, or
    /// apply again the last one taken back. `Message::Restructured` follows
    /// when there was one.
    pub fn undo(&self, doc: u64) {
        let _ = self.tx.send(Request::Undo { doc });
    }

    pub fn redo(&self, doc: u64) {
        let _ = self.tx.send(Request::Redo { doc });
    }

    /// Write `pages` of the document as shown (unsaved changes included) to
    /// a new PDF at `to`.
    pub fn extract(&self, doc: u64, pages: Vec<usize>, to: &Path) -> Result<(), String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Request::Extract { doc, pages, to: to.to_path_buf(), reply }).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }

    /// A form field's current value.
    pub fn field_value(&self, doc: u64, page: usize, index: usize) -> Option<String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Request::FieldValue { doc, page, index, reply }).ok()?;
        rx.recv().ok().flatten()
    }

    /// Write the document to `to` as the original bytes plus an incremental
    /// update holding every change, through a temporary file renamed over
    /// the target.
    pub fn save(&self, doc: u64, to: &Path) -> Result<(), String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Request::Save { doc, to: to.to_path_buf(), reply }).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }

    /// Search the document for `query`, replacing any search under way; an
    /// empty query just stops it. Hits arrive page by page as
    /// `Message::Hits`, then `Message::SearchDone`.
    pub fn search(&self, doc: u64, generation: u64, query: String) {
        let _ = self.tx.send(Request::Search { doc, generation, query });
    }
}

/// A loaded PDF and what PDFium needs alongside it. Raw handles: it never
/// leaves the engine thread.
struct Pdf {
    doc: FPDF_DOCUMENT,
    form: FPDF_FORMHANDLE,
    /// PDFium keeps a pointer to this for the form's whole life.
    _form_info: Box<FPDF_FORMFILLINFO>,
    /// Pages loaded so far, each registered with the form environment.
    pages: HashMap<usize, FPDF_PAGE>,
}

/// One recorded change, replayable onto a fresh copy of the file.
#[derive(Debug, Clone)]
enum Op {
    /// `name` is the /NM given to an annotation the edit creates, the same
    /// on every replay.
    Edit { edit: Edit, name: Option<String> },
    Pages(PageOp),
}

/// The document the app has open: what it shows, and what it would save.
struct Open {
    id: u64,
    view: Pdf,
    /// The file a save starts from: the one opened, then the last saved.
    source: PathBuf,
    /// Changes since the last save, in order.
    journal: Vec<Op>,
    /// Changes taken back, newest last, for redo. Any new change clears it.
    redone: Vec<Op>,
}

impl Open {
    fn load(lib: &Lib, id: u64, path: &Path) -> Result<Self, String> {
        Ok(Open { id, view: Pdf::load(lib, path)?, source: path.to_path_buf(), journal: Vec::new(), redone: Vec::new() })
    }

    fn close(self, lib: &Lib) {
        self.view.close(lib);
    }

    fn dirty(&self) -> bool {
        !self.journal.is_empty()
    }

    /// Apply an edit to the document shown, and record it.
    fn edit(&mut self, lib: &Lib, edit: Edit) -> Result<(), String> {
        let creates = matches!(edit, Edit::Markup { .. } | Edit::Ink { .. } | Edit::Note { .. });
        let page = edit.page();
        let op = Op::Edit { edit, name: creates.then(unique_name) };
        apply(lib, &mut self.view, &op)?;
        generate_appearances(lib, self.view.page(lib, page).ok_or("no such page")?);
        self.journal.push(op);
        self.redone.clear();
        Ok(())
    }

    /// Rearrange the pages of the document shown, and record it.
    fn pages(&mut self, lib: &Lib, op: PageOp) -> Result<(), String> {
        let op = Op::Pages(op);
        apply(lib, &mut self.view, &op)?;
        self.journal.push(op);
        self.redone.clear();
        Ok(())
    }

    /// Take back the last change: reload and replay the journal without it.
    /// False when there is nothing to take back.
    fn undo(&mut self, lib: &Lib) -> Result<bool, String> {
        let Some(op) = self.journal.pop() else { return Ok(false) };
        self.redone.push(op);
        self.rebuild(lib)?;
        Ok(true)
    }

    /// Apply the last change taken back again.
    fn redo(&mut self, lib: &Lib) -> Result<bool, String> {
        let Some(op) = self.redone.pop() else { return Ok(false) };
        if let Op::Edit { edit, .. } = &op {
            let page = edit.page();
            apply(lib, &mut self.view, &op)?;
            generate_appearances(lib, self.view.page(lib, page).ok_or("no such page")?);
        } else {
            apply(lib, &mut self.view, &op)?;
        }
        self.journal.push(op);
        Ok(true)
    }

    /// The document shown, made again from the source and the journal.
    fn rebuild(&mut self, lib: &Lib) -> Result<(), String> {
        let mut fresh = Pdf::load(lib, &self.source)?;
        if let Err(e) = replay(lib, &mut fresh, &self.journal) {
            fresh.close(lib);
            return Err(e);
        }
        std::mem::replace(&mut self.view, fresh).close(lib);
        Ok(())
    }

    /// `pages` of the document shown, as a new PDF at `to`.
    fn extract(&mut self, lib: &Lib, pages: &[usize], to: &Path) -> Result<(), String> {
        if pages.is_empty() {
            return Err("no pages chosen".to_string());
        }
        let indices: Vec<c_int> = pages.iter().map(|&p| p as c_int).collect();
        unsafe {
            let new = lib.FPDF_CreateNewDocument();
            if new.is_null() {
                return Err("PDFium could not make a document".to_string());
            }
            let copied = lib.is_true(lib.FPDF_ImportPagesByIndex(new, self.view.doc, indices.as_ptr(), indices.len() as c_ulong, 0));
            let mut sink = Sink { fw: FPDF_FILEWRITE { version: 1, WriteBlock: Some(write_block) }, out: Vec::new() };
            let saved = copied && lib.is_true(lib.FPDF_SaveAsCopy(new, &mut sink.fw, 0));
            lib.FPDF_CloseDocument(new);
            if !saved || sink.out.is_empty() {
                return Err("PDFium could not copy the pages".to_string());
            }
            write_atomically(to, &sink.out)
        }
    }

    /// Save to `to`: a fresh copy of `source` with the journal replayed,
    /// written as its bytes plus one incremental update.
    fn save(&mut self, lib: &Lib, to: &Path) -> Result<(), String> {
        let bytes = if self.journal.is_empty() {
            std::fs::read(&self.source).map_err(|e| format!("{}: {e}", self.source.display()))?
        } else {
            let mut fresh = Pdf::load(lib, &self.source)?;
            let result = replay(lib, &mut fresh, &self.journal).and_then(|()| fresh.incremental(lib));
            fresh.close(lib);
            let updated = result?;
            // PDFium's update is valid but carries every loaded object; cut
            // it to what changed. Should lopdf fail to read either file,
            // the untrimmed one is still a correct save.
            let original = std::fs::read(&self.source).map_err(|e| format!("{}: {e}", self.source.display()))?;
            match crate::trim::trim(&original, &updated) {
                Ok(trimmed) => trimmed,
                Err(e) => {
                    log::warn!("saving PDFium's whole update: {e}");
                    updated
                }
            }
        };
        write_atomically(to, &bytes)?;
        self.journal.clear();
        self.redone.clear();
        self.source = to.to_path_buf();
        Ok(())
    }
}

impl Pdf {
    /// Load a PDF with its form-fill environment. Always on, even for a
    /// copy made only to save: it is what rebuilds a reloaded page's
    /// annotation list, and with it the appearance streams of new marks. On
    /// a form marked /NeedAppearances that also draws the touched page's
    /// fields their appearances, and the save carries them (about 10 KB per
    /// page of the CUPS sample) — what that flag asks a viewer to do anyway.
    fn load(lib: &Lib, path: &Path) -> Result<Self, String> {
        let doc = unsafe { lib.FPDF_LoadDocument(&path.to_string_lossy(), None) };
        if doc.is_null() {
            return Err(match unsafe { lib.FPDF_GetLastError() } {
                FPDF_ERR_PASSWORD => "password-protected PDFs are not supported yet".to_string(),
                code => format!("PDFium could not open it (error {code})"),
            });
        }
        // All callbacks unset: nothing interactive is asked of the host.
        let mut info: Box<FPDF_FORMFILLINFO> = Box::new(unsafe { std::mem::zeroed() });
        info.version = 2;
        let form = unsafe { lib.FPDFDOC_InitFormFillEnvironment(doc, &mut *info) };
        if !form.is_null() {
            // Fillable fields show a light blue wash, as other readers do.
            // The color is a Windows COLORREF, 0x00BBGGRR, whatever
            // fpdf_formfill.h says.
            unsafe {
                lib.FPDF_SetFormFieldHighlightColor(form, 0, 0x00FF_E8DD);
                lib.FPDF_SetFormFieldHighlightAlpha(form, 90);
            }
        }
        Ok(Pdf { doc, form, _form_info: info, pages: HashMap::new() })
    }

    fn page_count(&self, lib: &Lib) -> usize {
        unsafe { lib.FPDF_GetPageCount(self.doc) }.max(0) as usize
    }

    /// Every page's size in display points.
    fn sizes(&mut self, lib: &Lib) -> Vec<PageSize> {
        (0..self.page_count(lib))
            .map(|i| self.page(lib, i).map(|p| Frame::of(lib, p).size()).unwrap_or(PageSize { w: 612.0, h: 792.0 }))
            .collect()
    }

    /// Close every loaded page: page operations shift the indices they are
    /// kept under.
    fn close_pages(&mut self, lib: &Lib) {
        for (_, p) in self.pages.drain() {
            unsafe {
                if !self.form.is_null() {
                    lib.FORM_OnBeforeClosePage(p, self.form);
                }
                lib.FPDF_ClosePage(p);
            }
        }
    }

    fn page(&mut self, lib: &Lib, index: usize) -> Option<FPDF_PAGE> {
        if let Some(p) = self.pages.get(&index) {
            return Some(*p);
        }
        let p = unsafe { lib.FPDF_LoadPage(self.doc, index as c_int) };
        if p.is_null() {
            return None;
        }
        if !self.form.is_null() {
            unsafe { lib.FORM_OnAfterLoadPage(p, self.form) };
        }
        self.pages.insert(index, p);
        Some(p)
    }

    fn close(self, lib: &Lib) {
        unsafe {
            for (_, p) in self.pages {
                if !self.form.is_null() {
                    lib.FORM_OnBeforeClosePage(p, self.form);
                }
                lib.FPDF_ClosePage(p);
            }
            if !self.form.is_null() {
                lib.FPDFDOC_ExitFormFillEnvironment(self.form);
            }
            lib.FPDF_CloseDocument(self.doc);
        }
    }
}

struct Search {
    doc: u64,
    generation: u64,
    query: String,
    next_page: usize,
}

fn run(lib: &Lib, rx: mpsc::Receiver<Request>, notify: calloop::channel::Sender<Message>) {
    let mut open: Option<Open> = None;
    let mut search: Option<Search> = None;
    loop {
        let req = if search.is_some() {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => break,
            }
        };
        // The open document, if it is the one a request names.
        fn current(open: &mut Option<Open>, id: u64) -> Option<&mut Open> {
            open.as_mut().filter(|o| o.id == id)
        }
        let sent = match req {
            Some(Request::Open { id, path, reply }) => {
                if let Some(o) = open.take() {
                    o.close(lib);
                }
                search = None;
                let result = Open::load(lib, id, &path).map(|mut o| {
                    let sizes = o.view.sizes(lib);
                    open = Some(o);
                    sizes
                });
                let _ = reply.send(result);
                true
            }
            Some(Request::Render(job)) => {
                let result = current(&mut open, job.doc)
                    .and_then(|o| {
                        render(lib, &mut o.view, &job)
                            .map_err(|e| log::warn!("{}: page {}: {e}", job.path.display(), job.page + 1))
                            .ok()
                    })
                    .map(|rgba| job.finish(rgba));
                notify.send(Message::Page { slot: job.slot, generation: job.generation, page: job.page, result }).is_ok()
            }
            Some(Request::Text { doc, page }) => match current(&mut open, doc) {
                Some(o) => {
                    let text = Arc::new(page_text(lib, &mut o.view, page));
                    notify.send(Message::Text { doc, page, text }).is_ok()
                }
                None => true,
            },
            Some(Request::TextNow { doc, page, reply }) => {
                let _ = reply.send(current(&mut open, doc).map(|o| Arc::new(page_text(lib, &mut o.view, page))));
                true
            }
            Some(Request::Annots { doc, page }) => match current(&mut open, doc) {
                Some(o) => notify.send(Message::Annots { doc, page, annots: Arc::new(annots(lib, &mut o.view, page)) }).is_ok(),
                None => true,
            },
            Some(Request::Edit { doc, edit }) => match current(&mut open, doc) {
                Some(o) => {
                    let page = edit.page();
                    let ok = o.edit(lib, edit).map_err(|e| log::warn!("edit on page {}: {e}", page + 1)).is_ok();
                    notify.send(Message::Edited { doc, page, ok, dirty: o.dirty() }).is_ok()
                        && notify.send(Message::Annots { doc, page, annots: Arc::new(annots(lib, &mut o.view, page)) }).is_ok()
                }
                None => true,
            },
            Some(Request::Pages { doc, op }) => match current(&mut open, doc) {
                Some(o) => {
                    let focus = op.lands_at();
                    let ok = o.pages(lib, op).map_err(|e| log::warn!("page operation: {e}")).is_ok();
                    let sizes = o.view.sizes(lib);
                    notify.send(Message::Restructured { doc, sizes, dirty: o.dirty(), focus, ok }).is_ok()
                }
                None => true,
            },
            Some(Request::Undo { doc }) | Some(Request::Redo { doc }) if current(&mut open, doc).is_none() => true,
            Some(Request::Undo { doc }) => {
                let o = current(&mut open, doc).expect("checked above");
                match o.undo(lib) {
                    Ok(false) => true,
                    result => {
                        let ok = result.map_err(|e| log::warn!("undo: {e}")).is_ok();
                        let sizes = o.view.sizes(lib);
                        notify.send(Message::Restructured { doc, sizes, dirty: o.dirty(), focus: None, ok }).is_ok()
                    }
                }
            }
            Some(Request::Redo { doc }) => {
                let o = current(&mut open, doc).expect("checked above");
                match o.redo(lib) {
                    Ok(false) => true,
                    result => {
                        let ok = result.map_err(|e| log::warn!("redo: {e}")).is_ok();
                        let sizes = o.view.sizes(lib);
                        notify.send(Message::Restructured { doc, sizes, dirty: o.dirty(), focus: None, ok }).is_ok()
                    }
                }
            }
            Some(Request::Extract { doc, pages, to, reply }) => {
                let result = match current(&mut open, doc) {
                    Some(o) => o.extract(lib, &pages, &to),
                    None => Err("no document open".to_string()),
                };
                let _ = reply.send(result);
                true
            }
            Some(Request::FieldValue { doc, page, index, reply }) => {
                let _ = reply.send(current(&mut open, doc).and_then(|o| field_value(lib, &mut o.view, page, index)));
                true
            }
            Some(Request::Save { doc, to, reply }) => {
                let result = match current(&mut open, doc) {
                    Some(o) => o.save(lib, &to),
                    None => Err("no document open".to_string()),
                };
                let _ = reply.send(result);
                true
            }
            Some(Request::Search { doc, generation, query }) => {
                search = (!query.is_empty() && current(&mut open, doc).is_some()).then_some(Search {
                    doc,
                    generation,
                    query,
                    next_page: 0,
                });
                true
            }
            None => {
                // A turn with nothing queued: search one more page.
                let Some(s) = search.as_mut() else { continue };
                let Some(o) = current(&mut open, s.doc) else {
                    search = None;
                    continue;
                };
                if s.next_page >= o.view.page_count(lib) {
                    let generation = s.generation;
                    search = None;
                    notify.send(Message::SearchDone { generation }).is_ok()
                } else {
                    let page = s.next_page;
                    s.next_page += 1;
                    let generation = s.generation;
                    let hits = search_page(lib, &mut o.view, page, &s.query);
                    hits.is_empty() || notify.send(Message::Hits { generation, page, hits }).is_ok()
                }
            }
        };
        if !sent {
            break;
        }
    }
    if let Some(o) = open.take() {
        o.close(lib);
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
    fn of(lib: &Lib, page: FPDF_PAGE) -> Self {
        let rot = (unsafe { lib.FPDFPage_GetRotation(page) }.rem_euclid(4)) as u8;
        // The bounding box is the crop box clipped to the media box: the
        // area PDFium renders.
        let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
        if lib.is_true(unsafe { lib.FPDF_GetPageBoundingBox(page, &mut r) }) && r.right > r.left && r.top > r.bottom {
            Frame {
                left: r.left as f64,
                top: r.top as f64,
                w: (r.right - r.left) as f64,
                h: (r.top - r.bottom) as f64,
                rot,
            }
        } else {
            let (pw, ph) = unsafe { (lib.FPDF_GetPageWidthF(page) as f64, lib.FPDF_GetPageHeightF(page) as f64) };
            let (w, h) = if rot % 2 == 1 { (ph, pw) } else { (pw, ph) };
            Frame { left: 0.0, top: h, w, h, rot }
        }
    }

    fn size(&self) -> PageSize {
        if self.rot % 2 == 1 {
            PageSize { w: self.h, h: self.w }
        } else {
            PageSize { w: self.w, h: self.h }
        }
    }

    /// Page space → display points.
    fn point(&self, x: f64, y: f64) -> (f64, f64) {
        let (dx, dy) = (x - self.left, self.top - y);
        match self.rot {
            1 => (self.h - dy, dx),
            2 => (self.w - dx, self.h - dy),
            3 => (dy, self.w - dx),
            _ => (dx, dy),
        }
    }

    /// Display points → page space (the inverse of `point`).
    fn page_point(&self, u: f64, v: f64) -> (f64, f64) {
        let (dx, dy) = match self.rot {
            1 => (v, self.h - u),
            2 => (self.w - u, self.h - v),
            3 => (self.w - v, u),
            _ => (u, v),
        };
        (self.left + dx, self.top - dy)
    }

    /// A page-space rect given by two opposite corners → display points.
    fn rect(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> PtRect {
        let (ax, ay) = self.point(x0, y0);
        let (bx, by) = self.point(x1, y1);
        PtRect { x0: ax.min(bx), y0: ay.min(by), x1: ax.max(bx), y1: ay.max(by) }
    }

    fn fs_rect(&self, r: &FS_RECTF) -> PtRect {
        self.rect(r.left as f64, r.top as f64, r.right as f64, r.bottom as f64)
    }

    /// A display rect → page space as (left, bottom, right, top).
    fn page_rect(&self, r: &PtRect) -> (f32, f32, f32, f32) {
        let (ax, ay) = self.page_point(r.x0, r.y0);
        let (bx, by) = self.page_point(r.x1, r.y1);
        (ax.min(bx) as f32, ay.min(by) as f32, ax.max(bx) as f32, ay.max(by) as f32)
    }
}

fn render(lib: &Lib, o: &mut Pdf, job: &Job) -> Result<image::RgbaImage, String> {
    let page = o.page(lib, job.page).ok_or("no such page")?;
    let scale = job.capped_dpi() as f64 / 72.0;
    let w = ((job.size.w * scale).round() as c_int).max(1);
    let h = ((job.size.h * scale).round() as c_int).max(1);
    unsafe {
        let bitmap = lib.FPDFBitmap_Create(w, h, 0);
        if bitmap.is_null() {
            return Err("PDFium: no bitmap".to_string());
        }
        lib.FPDFBitmap_FillRect(bitmap, 0, 0, w, h, 0xFFFF_FFFF);
        lib.FPDF_RenderPageBitmap(bitmap, page, 0, 0, w, h, 0, FPDF_ANNOT);
        if !o.form.is_null() {
            lib.FPDF_FFLDraw(o.form, bitmap, page, 0, 0, w, h, 0, FPDF_ANNOT);
        }
        let stride = lib.FPDFBitmap_GetStride(bitmap) as usize;
        let buf = lib.FPDFBitmap_GetBuffer(bitmap) as *const u8;
        let (wu, hu) = (w as usize, h as usize);
        let mut rgba = vec![0u8; wu * hu * 4];
        for y in 0..hu {
            let row = std::slice::from_raw_parts(buf.add(y * stride), wu * 4);
            for (dst, src) in rgba[y * wu * 4..(y + 1) * wu * 4].chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
            }
        }
        lib.FPDFBitmap_Destroy(bitmap);
        image::RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or_else(|| "bitmap size mismatch".to_string())
    }
}

fn page_text(lib: &Lib, o: &mut Pdf, index: usize) -> PageText {
    let Some(page) = o.page(lib, index) else { return PageText::default() };
    let frame = Frame::of(lib, page);
    unsafe {
        let text = lib.FPDFText_LoadPage(page);
        if text.is_null() {
            return PageText::default();
        }
        let n = lib.FPDFText_CountChars(text).max(0);
        let none = PtRect { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0 };
        let chars = (0..n)
            .filter_map(|i| {
                let ch = char::from_u32(lib.FPDFText_GetUnicode(text, i))?;
                // A generated char (an inferred space or line break) has no
                // glyph; keep it for copying, without a box to hit.
                let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
                let rect = if lib.FPDFText_IsGenerated(text, i) == 1 || !lib.is_true(lib.FPDFText_GetLooseCharBox(text, i, &mut r)) {
                    none
                } else {
                    frame.fs_rect(&r)
                };
                Some(TextChar { ch, rect })
            })
            .collect();
        lib.FPDFText_ClosePage(text);
        PageText { chars }
    }
}

fn wide(s: &str) -> Vec<FPDF_WCHAR> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read one of PDFium's UTF-16 string answers: it is asked twice, once for
/// the length in bytes (terminator included).
fn read_wide(get: impl Fn(*mut FPDF_WCHAR, c_ulong) -> c_ulong) -> String {
    let bytes = get(std::ptr::null_mut(), 0) as usize;
    if bytes < 2 {
        return String::new();
    }
    let mut buf = vec![0u16; bytes / 2];
    get(buf.as_mut_ptr(), bytes as c_ulong);
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Every match of `query` on one page, each as the rects it covers (a match
/// can wrap onto a second line).
fn search_page(lib: &Lib, o: &mut Pdf, index: usize, query: &str) -> Vec<Vec<PtRect>> {
    let Some(page) = o.page(lib, index) else { return Vec::new() };
    let frame = Frame::of(lib, page);
    let what = wide(query);
    let mut hits = Vec::new();
    unsafe {
        let text = lib.FPDFText_LoadPage(page);
        if text.is_null() {
            return hits;
        }
        let find = lib.FPDFText_FindStart(text, what.as_ptr(), 0, 0);
        if !find.is_null() {
            while lib.is_true(lib.FPDFText_FindNext(find)) {
                let (start, count) = (lib.FPDFText_GetSchResultIndex(find), lib.FPDFText_GetSchCount(find));
                let n = lib.FPDFText_CountRects(text, start, count);
                let rects: Vec<PtRect> = (0..n.max(0))
                    .filter_map(|i| {
                        let (mut l, mut t, mut r, mut b) = (0.0, 0.0, 0.0, 0.0);
                        lib.is_true(lib.FPDFText_GetRect(text, i, &mut l, &mut t, &mut r, &mut b)).then(|| frame.rect(l, t, r, b))
                    })
                    .collect();
                if !rects.is_empty() {
                    hits.push(rects);
                }
            }
            lib.FPDFText_FindClose(find);
        }
        lib.FPDFText_ClosePage(text);
    }
    hits
}

/// A page's annotations as the app sees them.
fn annots(lib: &Lib, o: &mut Pdf, index: usize) -> Vec<Annot> {
    let Some(page) = o.page(lib, index) else { return Vec::new() };
    let frame = Frame::of(lib, page);
    let n = unsafe { lib.FPDFPage_GetAnnotCount(page) }.max(0);
    (0..n)
        .filter_map(|i| unsafe {
            let a = lib.FPDFPage_GetAnnot(page, i);
            if a.is_null() {
                return None;
            }
            let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
            lib.FPDFAnnot_GetRect(a, &mut r);
            let kind = match lib.FPDFAnnot_GetSubtype(a) {
                ANNOT_TEXT => AnnotKind::Note,
                ANNOT_HIGHLIGHT => AnnotKind::Markup(MarkupKind::Highlight),
                ANNOT_UNDERLINE => AnnotKind::Markup(MarkupKind::Underline),
                ANNOT_STRIKEOUT => AnnotKind::Markup(MarkupKind::StrikeOut),
                ANNOT_INK => AnnotKind::Ink,
                ANNOT_WIDGET if !o.form.is_null() => {
                    let read_only = lib.FPDFAnnot_GetFormFieldFlags(o.form, a) & FIELD_READONLY != 0;
                    AnnotKind::Field(match lib.FPDFAnnot_GetFormFieldType(o.form, a) {
                        FIELD_TEXT => FieldKind::Text { read_only },
                        FIELD_CHECKBOX => FieldKind::CheckBox { read_only },
                        FIELD_RADIO => FieldKind::Radio { read_only },
                        _ => FieldKind::Other,
                    })
                }
                _ => AnnotKind::Other,
            };
            let contents = if kind == AnnotKind::Note {
                read_wide(|buf, len| lib.FPDFAnnot_GetStringValue(a, "Contents", buf, len))
            } else {
                String::new()
            };
            lib.FPDFPage_CloseAnnot(a);
            Some(Annot { index: i as usize, kind, rect: frame.fs_rect(&r), contents })
        })
        .collect()
}

fn field_value(lib: &Lib, o: &mut Pdf, page: usize, index: usize) -> Option<String> {
    if o.form.is_null() {
        return None;
    }
    let p = o.page(lib, page)?;
    unsafe {
        let a = lib.FPDFPage_GetAnnot(p, index as c_int);
        if a.is_null() {
            return None;
        }
        let v = read_wide(|buf, len| lib.FPDFAnnot_GetFormFieldValue(o.form, a, buf, len));
        lib.FPDFPage_CloseAnnot(a);
        Some(v)
    }
}

/// A unique /NM for an annotation this session makes.
fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("cce-{nanos:x}-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// Create an annotation with the parts every new one shares: color, the
/// print flag, its unique name (for undo).
unsafe fn create(lib: &Lib, page: FPDF_PAGE, subtype: c_int, rgb: [u32; 3], flags: c_int, name: Option<&str>) -> Result<FPDF_ANNOTATION, String> {
    let a = lib.FPDFPage_CreateAnnot(page, subtype);
    if a.is_null() {
        return Err("PDFium refused the annotation".to_string());
    }
    lib.FPDFAnnot_SetColor(a, COLOR_STROKE, rgb[0], rgb[1], rgb[2], 255);
    lib.FPDFAnnot_SetFlags(a, flags);
    if let Some(name) = name {
        lib.FPDFAnnot_SetStringValue_str(a, "NM", name);
    }
    Ok(a)
}

fn set_rect(lib: &Lib, a: FPDF_ANNOTATION, (l, b, r, t): (f32, f32, f32, f32)) {
    let rect = FS_RECTF { left: l, top: t, right: r, bottom: b };
    unsafe { lib.FPDFAnnot_SetRect(a, &rect) };
}

fn apply(lib: &Lib, o: &mut Pdf, op: &Op) -> Result<(), String> {
    let (edit, name) = match op {
        Op::Edit { edit, name } => (edit.clone(), name.as_deref()),
        Op::Pages(op) => return apply_pages(lib, o, op),
    };
    let page = o.page(lib, edit.page()).ok_or("no such page")?;
    let frame = Frame::of(lib, page);
    unsafe {
        match edit {
            Edit::Markup { kind, rects, .. } => {
                let (subtype, rgb) = match kind {
                    MarkupKind::Highlight => (ANNOT_HIGHLIGHT, HIGHLIGHT_RGB),
                    MarkupKind::Underline => (ANNOT_UNDERLINE, UNDERLINE_RGB),
                    MarkupKind::StrikeOut => (ANNOT_STRIKEOUT, STRIKEOUT_RGB),
                };
                let a = create(lib, page, subtype, rgb, FLAG_PRINT, name)?;
                let mut bounds: Option<(f32, f32, f32, f32)> = None;
                for r in &rects {
                    // Quad corners in the order readers expect: upper left,
                    // upper right, lower left, lower right (page space, so
                    // "upper" follows the text, whatever the page's /Rotate).
                    let (l, b, rr, t) = frame.page_rect(r);
                    let q = FS_QUADPOINTSF { x1: l, y1: t, x2: rr, y2: t, x3: l, y3: b, x4: rr, y4: b };
                    lib.FPDFAnnot_AppendAttachmentPoints(a, &q);
                    bounds = Some(match bounds {
                        Some((bl, bb, br, bt)) => (bl.min(l), bb.min(b), br.max(rr), bt.max(t)),
                        None => (l, b, rr, t),
                    });
                }
                if let Some(bounds) = bounds {
                    set_rect(lib, a, bounds);
                }
                lib.FPDFPage_CloseAnnot(a);
            }
            Edit::Ink { points, width, .. } => {
                if points.len() < 2 {
                    return Err("a stroke needs two points".to_string());
                }
                let a = create(lib, page, ANNOT_INK, INK_RGB, FLAG_PRINT, name)?;
                let pts: Vec<FS_POINTF> = points
                    .iter()
                    .map(|&(x, y)| {
                        let (px, py) = frame.page_point(x, y);
                        FS_POINTF { x: px as f32, y: py as f32 }
                    })
                    .collect();
                lib.FPDFAnnot_AddInkStroke(a, pts.as_ptr(), pts.len());
                lib.FPDFAnnot_SetBorder(a, 0.0, 0.0, width as f32);
                let pad = width as f32;
                let l = pts.iter().map(|p| p.x).fold(f32::MAX, f32::min) - pad;
                let r = pts.iter().map(|p| p.x).fold(f32::MIN, f32::max) + pad;
                let b = pts.iter().map(|p| p.y).fold(f32::MAX, f32::min) - pad;
                let t = pts.iter().map(|p| p.y).fold(f32::MIN, f32::max) + pad;
                set_rect(lib, a, (l, b, r, t));
                lib.FPDFPage_CloseAnnot(a);
            }
            Edit::Note { at, contents, .. } => {
                let a = create(lib, page, ANNOT_TEXT, NOTE_RGB, FLAG_PRINT | FLAG_NOZOOM | FLAG_NOROTATE, name)?;
                let (px, py) = frame.page_point(at.0, at.1);
                let half = NOTE_PT / 2.0;
                set_rect(lib, a, (px as f32 - half, py as f32 - half, px as f32 + half, py as f32 + half));
                lib.FPDFAnnot_SetStringValue_str(a, "Contents", &contents);
                lib.FPDFPage_CloseAnnot(a);
            }
            Edit::SetContents { index, contents, .. } => {
                let a = lib.FPDFPage_GetAnnot(page, index as c_int);
                if a.is_null() {
                    return Err("no such annotation".to_string());
                }
                lib.FPDFAnnot_SetStringValue_str(a, "Contents", &contents);
                lib.FPDFPage_CloseAnnot(a);
            }
            Edit::Delete { index, .. } => {
                if !lib.is_true(lib.FPDFPage_RemoveAnnot(page, index as c_int)) {
                    return Err("PDFium could not remove it".to_string());
                }
            }
            Edit::SetText { index, value, .. } => {
                if o.form.is_null() {
                    return Err("no form".to_string());
                }
                let a = lib.FPDFPage_GetAnnot(page, index as c_int);
                if a.is_null() {
                    return Err("no such field".to_string());
                }
                // Type it in: focus, select all, replace, leave. PDFium then
                // stores the value and redraws the field's appearance, which
                // other readers show.
                let text = wide(&value);
                let focused = lib.is_true(lib.FORM_SetFocusedAnnot(o.form, a));
                if focused {
                    lib.FORM_SelectAllText(o.form, page);
                    lib.FORM_ReplaceSelection(o.form, page, text.as_ptr());
                    lib.FORM_ForceToKillFocus(o.form);
                }
                lib.FPDFPage_CloseAnnot(a);
                if !focused {
                    return Err("the field would not take focus".to_string());
                }
            }
            Edit::Toggle { index, .. } => {
                if o.form.is_null() {
                    return Err("no form".to_string());
                }
                let a = lib.FPDFPage_GetAnnot(page, index as c_int);
                if a.is_null() {
                    return Err("no such field".to_string());
                }
                let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
                lib.FPDFAnnot_GetRect(a, &mut r);
                lib.FPDFPage_CloseAnnot(a);
                let (x, y) = (((r.left + r.right) / 2.0) as f64, ((r.top + r.bottom) / 2.0) as f64);
                lib.FORM_OnLButtonDown(o.form, page, 0, x, y);
                lib.FORM_OnLButtonUp(o.form, page, 0, x, y);
                lib.FORM_ForceToKillFocus(o.form);
            }
        }
    }
    Ok(())
}

/// Apply a journal to a freshly loaded copy, then make the appearance
/// streams of what it added — by closing and reloading each page it
/// touched, which rebuilds the page's annotation list (and with it the
/// missing streams) without parsing the page's content. A render would do
/// it too, but would load the page's fonts and images, and a save writes
/// whatever is loaded.
fn replay(lib: &Lib, o: &mut Pdf, journal: &[Op]) -> Result<(), String> {
    for op in journal {
        apply(lib, o, op)?;
        // Right away: a later page operation renumbers the pages.
        if let Op::Edit { edit, .. } = op {
            o.reload_page(lib, edit.page());
        }
    }
    Ok(())
}

/// Rearrange the pages of a document.
fn apply_pages(lib: &Lib, o: &mut Pdf, op: &PageOp) -> Result<(), String> {
    let count = o.page_count(lib);
    let valid = |pages: &[usize]| !pages.is_empty() && pages.iter().all(|&p| p < count);
    match op {
        PageOp::Rotate { pages, quarter_turns } => {
            if !valid(pages) {
                return Err("no such page".to_string());
            }
            for &i in pages {
                let p = o.page(lib, i).ok_or("no such page")?;
                unsafe {
                    let turns = (lib.FPDFPage_GetRotation(p) + quarter_turns).rem_euclid(4);
                    lib.FPDFPage_SetRotation(p, turns);
                }
                // The page's size and text boxes are read again on reload.
                o.reload_page(lib, i);
            }
        }
        PageOp::Delete { pages } => {
            if !valid(pages) || pages.len() >= count {
                return Err("a document keeps at least one page".to_string());
            }
            o.close_pages(lib);
            let mut sorted = pages.clone();
            sorted.sort_unstable();
            sorted.dedup();
            for &i in sorted.iter().rev() {
                unsafe { lib.FPDFPage_Delete(o.doc, i as c_int) };
            }
        }
        PageOp::Move { pages, gap } => {
            if !valid(pages) || *gap > count {
                return Err("no such page".to_string());
            }
            o.close_pages(lib);
            let indices: Vec<c_int> = pages.iter().map(|&p| p as c_int).collect();
            let dest = move_dest(pages, *gap) as c_int;
            if !lib.is_true(unsafe { lib.FPDF_MovePages(o.doc, indices.as_ptr(), indices.len() as c_ulong, dest) }) {
                return Err("PDFium could not move the pages".to_string());
            }
        }
        PageOp::Insert { from, at } => {
            if *at > count {
                return Err("no such page".to_string());
            }
            let src = unsafe { lib.FPDF_LoadDocument(&from.to_string_lossy(), None) };
            if src.is_null() {
                return Err(format!("could not open {}", from.display()));
            }
            o.close_pages(lib);
            let n = unsafe { lib.FPDF_GetPageCount(src) }.max(0);
            let all: Vec<c_int> = (0..n).collect();
            let ok = unsafe { lib.FPDF_ImportPagesByIndex(o.doc, src, all.as_ptr(), all.len() as c_ulong, *at as c_int) };
            unsafe { lib.FPDF_CloseDocument(src) };
            if !lib.is_true(ok) {
                return Err("PDFium could not insert the pages".to_string());
            }
        }
    }
    Ok(())
}

/// Make the appearance streams of the page's new annotations.
///
/// PDFium writes an annotation's /AP only when a render first meets it
/// without one — creating it, setting its quads or ink, never does — and a
/// saved annotation without /AP is invisible in readers that do not draw
/// their own. So every edit that adds a mark renders the page once into a
/// 1×1 bitmap, which builds and stores them.
fn generate_appearances(lib: &Lib, page: FPDF_PAGE) {
    unsafe {
        let bitmap = lib.FPDFBitmap_Create(1, 1, 0);
        if !bitmap.is_null() {
            lib.FPDF_RenderPageBitmap(bitmap, page, 0, 0, 1, 1, 0, FPDF_ANNOT);
            lib.FPDFBitmap_Destroy(bitmap);
        }
    }
}

/// Collects what PDFium writes. `fw` must stay the first field: PDFium
/// hands its pointer back, and the callback casts it to the whole sink.
#[repr(C)]
struct Sink {
    fw: FPDF_FILEWRITE,
    out: Vec<u8>,
}

unsafe extern "C" fn write_block(this: *mut FPDF_FILEWRITE, data: *const c_void, size: c_ulong) -> c_int {
    let sink = &mut *(this as *mut Sink);
    sink.out.extend_from_slice(std::slice::from_raw_parts(data as *const u8, size as usize));
    1
}

impl Pdf {
    /// Close a loaded page and load it again (see `replay`).
    fn reload_page(&mut self, lib: &Lib, index: usize) {
        if let Some(p) = self.pages.remove(&index) {
            unsafe {
                if !self.form.is_null() {
                    lib.FORM_OnBeforeClosePage(p, self.form);
                }
                lib.FPDF_ClosePage(p);
            }
        }
        self.page(lib, index);
    }

    /// The file's original bytes plus an incremental update of everything
    /// loaded.
    fn incremental(&mut self, lib: &Lib) -> Result<Vec<u8>, String> {
        let mut sink = Sink { fw: FPDF_FILEWRITE { version: 1, WriteBlock: Some(write_block) }, out: Vec::new() };
        let ok = unsafe { lib.FPDF_SaveAsCopy(self.doc, &mut sink.fw, FPDF_INCREMENTAL) };
        if !lib.is_true(ok) || sink.out.is_empty() {
            return Err("PDFium could not write the document".to_string());
        }
        Ok(sink.out)
    }
}

/// Write `bytes` to `to` through a sibling temporary renamed over it: a
/// failed write never leaves half a file, and an open handle on the old
/// file (PDFium reads pages lazily) keeps the old bytes.
fn write_atomically(to: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = to.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = to.file_name().ok_or("no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.cce-save-{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(to) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, to)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", to.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test owns the library: PDFium initialises once per process.
    /// Needs PDFium, qpdf and poppler, and the CUPS sample PDF; skips
    /// without them.
    #[test]
    fn pdfium_round_trips() {
        let src = Path::new("/usr/share/cups/data/form_english.pdf");
        let Ok(lib) = bind() else { return eprintln!("skipped: no PDFium") };
        if !src.exists() {
            return eprintln!("skipped: no {}", src.display());
        }
        let dir = std::env::temp_dir().join(format!("cce-documents-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        a_rotated_page_maps_its_text_where_it_is_drawn(&lib, src, &dir);
        edits_survive_an_incremental_save(&lib, src, &dir);
        a_check_box_toggles_to_its_own_on_state(&lib, &dir);
        pages_rearrange_and_keep_links_and_outlines(&lib, src, &dir);
        typesetting_puts_every_glyph_where_the_pdf_draws_it(&lib, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_ok(cmd: &mut std::process::Command) -> bool {
        cmd.status().is_ok_and(|s| s.success())
    }

    /// A match found on a page and on a copy of it turned 90° by its /Rotate
    /// must land in the same place once the turn is undone: the check that
    /// `Frame` maps page space onto display points as PDFium renders it.
    fn a_rotated_page_maps_its_text_where_it_is_drawn(lib: &Lib, src: &Path, dir: &Path) {
        let turned = dir.join("turned.pdf");
        if !run_ok(std::process::Command::new("qpdf").arg("--rotate=+90:1").arg(src).arg(&turned)) {
            return eprintln!("skipped rotation: qpdf failed");
        }
        let mut plain = Pdf::load(lib, src).unwrap();
        let mut rotated = Pdf::load(lib, &turned).unwrap();
        let p = plain.page(lib, 0).unwrap();
        let q = rotated.page(lib, 0).unwrap();
        let size = Frame::of(lib, p).size();
        let turned_size = Frame::of(lib, q).size();
        assert!((turned_size.w - size.h).abs() < 0.5 && (turned_size.h - size.w).abs() < 0.5, "{size:?} vs {turned_size:?}");
        // And the size agrees with what PDFium itself reports for the page.
        assert!((turned_size.w - unsafe { lib.FPDF_GetPageWidthF(q) } as f64).abs() < 0.5);

        let a = search_page(lib, &mut plain, 0, "Printer");
        let b = search_page(lib, &mut rotated, 0, "Printer");
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
        // page_point undoes point, on every rotation.
        for f in [Frame::of(lib, p), Frame::of(lib, q)] {
            let (u, v) = f.point(100.0, 200.0);
            let (x, y) = f.page_point(u, v);
            assert!((x - 100.0).abs() < 1e-9 && (y - 200.0).abs() < 1e-9);
        }
        let text = page_text(lib, &mut plain, 0);
        assert!(text.text(0, text.len()).contains("Printer name"));
        plain.close(lib);
        rotated.close(lib);
    }

    /// Mark up a page, fill a field, save incrementally, and read it all back:
    /// in PDFium (what Chrome uses), through qpdf's structural check, and
    /// rendered by poppler.
    fn edits_survive_an_incremental_save(lib: &Lib, src: &Path, dir: &Path) {
        let path = dir.join("marked.pdf");
        std::fs::copy(src, &path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut o = Open::load(lib, 3, &path).unwrap();
        // Shown first, as in the app: rendering loads the page's fonts,
        // which a save must not drag along.
        let size = Frame::of(lib, o.view.page(lib, 0).unwrap()).size();
        let job = Job { slot: 0, generation: 0, doc: 3, page: 0, dpi: 96, path: path.clone(), size, quarter_turns: 0 };
        render(lib, &mut o.view, &job).unwrap();

        let hit = search_page(lib, &mut o.view, 0, "Printer name").remove(0);
        o.edit(lib, Edit::Markup { page: 0, kind: MarkupKind::Highlight, rects: hit.clone() }).unwrap();
        let under = search_page(lib, &mut o.view, 0, "Driver version").remove(0);
        o.edit(lib, Edit::Markup { page: 0, kind: MarkupKind::Underline, rects: under }).unwrap();
        let strike = search_page(lib, &mut o.view, 0, "Source host").remove(0);
        o.edit(lib, Edit::Markup { page: 0, kind: MarkupKind::StrikeOut, rects: strike }).unwrap();
        o.edit(lib, Edit::Ink { page: 0, points: vec![(400.0, 700.0), (450.0, 720.0), (500.0, 700.0)], width: 2.0 }).unwrap();
        o.edit(lib, Edit::Note { page: 0, at: (520.0, 100.0), contents: "check this".into() }).unwrap();
        // An undone stroke is gone.
        o.edit(lib, Edit::Ink { page: 0, points: vec![(100.0, 750.0), (200.0, 760.0)], width: 2.0 }).unwrap();
        assert!(o.undo(lib).unwrap());

        let list = annots(lib, &mut o.view, 0);
        let kinds: Vec<AnnotKind> = list.iter().map(|a| a.kind).filter(|k| !matches!(k, AnnotKind::Field(_))).collect();
        assert_eq!(
            kinds,
            vec![
                AnnotKind::Markup(MarkupKind::Highlight),
                AnnotKind::Markup(MarkupKind::Underline),
                AnnotKind::Markup(MarkupKind::StrikeOut),
                AnnotKind::Ink,
                AnnotKind::Note
            ]
        );
        assert_eq!(list.iter().find(|a| a.kind == AnnotKind::Note).unwrap().contents, "check this");
        // The highlight covers the text it was made from.
        let h = list.iter().find(|a| a.kind == AnnotKind::Markup(MarkupKind::Highlight)).unwrap();
        assert!((h.rect.x0 - hit[0].x0).abs() < 1.0 && (h.rect.y1 - hit[0].y1).abs() < 1.0, "{:?} vs {:?}", h.rect, hit[0]);

        // Fill the first text field.
        let field = list.iter().find(|a| matches!(a.kind, AnnotKind::Field(FieldKind::Text { .. }))).expect("a text field").index;
        o.edit(lib, Edit::SetText { page: 0, index: field, value: "Laser 9000".into() }).unwrap();
        assert_eq!(field_value(lib, &mut o.view, 0, field).as_deref(), Some("Laser 9000"));

        o.save(lib, &path).unwrap();
        let saved = std::fs::read(&path).unwrap();
        assert!(saved.len() > original.len());
        assert_eq!(&saved[..original.len()], &original[..], "the original bytes are kept");
        // The update holds the edits, not the page's 239 KB font the render
        // loaded.
        let update = saved.len() - original.len();
        assert!(update < 64 * 1024, "the update is {update} bytes");
        assert!(run_ok(std::process::Command::new("qpdf").arg("--check").arg(&path).stdout(std::process::Stdio::null())), "qpdf --check");

        // A second save appends to the first.
        o.edit(lib, Edit::Note { page: 0, at: (520.0, 300.0), contents: "and this".into() }).unwrap();
        o.save(lib, &path).unwrap();
        let again = std::fs::read(&path).unwrap();
        assert_eq!(&again[..saved.len()], &saved[..], "the first save's bytes are kept");
        assert!(again.len() - saved.len() < 64 * 1024);
        o.close(lib);

        // PDFium reads it all back.
        let mut back = Pdf::load(lib, &path).unwrap();
        let back_list = annots(lib, &mut back, 0);
        assert_eq!(back_list.iter().filter(|a| !matches!(a.kind, AnnotKind::Field(_))).count(), 6);
        assert_eq!(field_value(lib, &mut back, 0, field).as_deref(), Some("Laser 9000"));
        back.close(lib);

        // Every mark carries an appearance stream, so readers that do not
        // generate their own still draw it.
        let dump = std::process::Command::new("qpdf").args(["--qdf", "--object-streams=disable"]).arg(&path).arg("-").output().unwrap();
        let dump = String::from_utf8_lossy(&dump.stdout);
        for subtype in ["/Highlight", "/Underline", "/StrikeOut", "/Ink", "/Text"] {
            let at = dump.find(&format!("/Subtype {subtype}")).unwrap_or_else(|| panic!("{subtype} missing"));
            // The annotation's dictionary runs to the next "endobj".
            let dict = &dump[dump[..at].rfind("obj").unwrap()..at + dump[at..].find("endobj").unwrap()];
            assert!(dict.contains("/AP"), "{subtype} has no appearance stream");
        }

        // poppler draws the highlight: the middle of its first rect is
        // yellow, not white.
        let png = dir.join("poppler");
        if run_ok(std::process::Command::new("pdftoppm").args(["-png", "-r", "72", "-f", "1", "-l", "1", "-singlefile"]).arg(&path).arg(&png)) {
            let img = image::open(png.with_extension("png")).unwrap().to_rgba8();
            let (x, y) = (((hit[0].x0 + hit[0].x1) / 2.0) as u32, ((hit[0].y0 + hit[0].y1) / 2.0) as u32);
            // Sample a few pixels in the run: some fall on glyphs.
            let yellow = (0..8).any(|dx| {
                let p = img.get_pixel(x + dx, y);
                p[0] > 200 && p[1] > 170 && p[2] < 120
            });
            assert!(yellow, "poppler drew no highlight at ({x}, {y})");
        } else {
            eprintln!("skipped poppler check: no pdftoppm");
        }
    }

    /// A check box whose on state is named /On (not the /Yes many tools
    /// assume) toggles to /On, keeps it through a save, and redraws.
    fn a_check_box_toggles_to_its_own_on_state(lib: &Lib, dir: &Path) {
        // Hand-written, then rebuilt by qpdf so the xref is right.
        let raw = b"%PDF-1.7\n\
1 0 obj << /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [4 0 R] >> >> endobj\n\
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n\
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [4 0 R] >> endobj\n\
4 0 obj << /Type /Annot /Subtype /Widget /FT /Btn /T (agree) /Rect [50 50 70 70] /V /Off /AS /Off \
/AP << /N << /On 5 0 R /Off 6 0 R >> >> /P 3 0 R /F 4 >> endobj\n\
5 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 20 20] /Length 23 >> stream\n0 0 0 rg 4 4 12 12 re f\nendstream endobj\n\
6 0 obj << /Type /XObject /Subtype /Form /BBox [0 0 20 20] /Length 0 >> stream\n\nendstream endobj\n\
trailer << /Root 1 0 R >>\n%%EOF\n";
        let rough = dir.join("rough.pdf");
        let path = dir.join("checkbox.pdf");
        std::fs::write(&rough, raw).unwrap();
        // qpdf exits 3 when it repaired something, which is the point here.
        let status = std::process::Command::new("qpdf").arg(&rough).arg(&path).stderr(std::process::Stdio::null()).status();
        if !status.is_ok_and(|s| matches!(s.code(), Some(0) | Some(3))) {
            return eprintln!("skipped check box: qpdf failed");
        }
        let checked = |o: &mut Pdf, index: usize| -> bool {
            let p = o.page(lib, 0).unwrap();
            unsafe {
                let a = lib.FPDFPage_GetAnnot(p, index as c_int);
                let c = lib.is_true(lib.FPDFAnnot_IsChecked(o.form, a));
                lib.FPDFPage_CloseAnnot(a);
                c
            }
        };
        let mut o = Open::load(lib, 5, &path).unwrap();
        let list = annots(lib, &mut o.view, 0);
        let index = list.iter().find(|a| matches!(a.kind, AnnotKind::Field(FieldKind::CheckBox { .. }))).expect("a check box").index;
        assert!(!checked(&mut o.view, index));
        o.edit(lib, Edit::Toggle { page: 0, index }).unwrap();
        assert!(checked(&mut o.view, index), "the click did not check it");
        o.save(lib, &path).unwrap();
        o.close(lib);

        let mut back = Pdf::load(lib, &path).unwrap();
        assert!(checked(&mut back, index), "not checked after a save");
        back.close(lib);
        let dump = std::process::Command::new("qpdf").args(["--qdf", "--object-streams=disable"]).arg(&path).arg("-").output().unwrap();
        let dump = String::from_utf8_lossy(&dump.stdout);
        assert!(dump.contains("/AS /On") && dump.contains("/V /On"), "the on state is not /On:\n{dump}");
    }

    /// A small PDF: three pages reading "Page one/two/three", an outline
    /// entry for each, and a link on page one to page three. Written by
    /// hand, then rebuilt by qpdf so the xref is right.
    fn three_pages(dir: &Path) -> Option<PathBuf> {
        let mut pdf = String::from("%PDF-1.7\n");
        let mut obj = |n: u32, body: &str| pdf.push_str(&format!("{n} 0 obj\n{body}\nendobj\n"));
        obj(1, "<< /Type /Catalog /Pages 2 0 R /Outlines 10 0 R >>");
        obj(2, "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>");
        let page = |contents: u32, annots: &str| {
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents {contents} 0 R /Resources << /Font << /F1 9 0 R >> >> {annots}>>")
        };
        obj(3, &page(6, "/Annots [14 0 R] "));
        obj(4, &page(7, ""));
        obj(5, &page(8, ""));
        for (n, word) in [(6, "one"), (7, "two"), (8, "three")] {
            let text = format!("BT /F1 24 Tf 50 150 Td (Page {word}) Tj ET");
            obj(n, &format!("<< /Length {} >>\nstream\n{text}\nendstream", text.len()));
        }
        obj(9, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
        obj(10, "<< /Type /Outlines /First 11 0 R /Last 13 0 R /Count 3 >>");
        obj(11, "<< /Title (One) /Parent 10 0 R /Next 12 0 R /Dest [3 0 R /Fit] >>");
        obj(12, "<< /Title (Two) /Parent 10 0 R /Prev 11 0 R /Next 13 0 R /Dest [4 0 R /Fit] >>");
        obj(13, "<< /Title (Three) /Parent 10 0 R /Prev 12 0 R /Dest [5 0 R /Fit] >>");
        obj(14, "<< /Type /Annot /Subtype /Link /Rect [40 140 200 180] /Border [0 0 0] /Dest [5 0 R /Fit] >>");
        pdf.push_str("trailer << /Root 1 0 R >>\n%%EOF\n");
        let rough = dir.join("pages-rough.pdf");
        let path = dir.join("pages.pdf");
        std::fs::write(&rough, pdf).ok()?;
        let status = std::process::Command::new("qpdf").arg(&rough).arg(&path).stderr(std::process::Stdio::null()).status();
        status.is_ok_and(|s| matches!(s.code(), Some(0) | Some(3))).then_some(path)
    }

    /// Where each link on a page leads, as page indices.
    fn link_targets(lib: &Lib, o: &mut Pdf, page: usize) -> Vec<c_int> {
        let p = o.page(lib, page).unwrap();
        let mut out = Vec::new();
        let mut pos: c_int = 0;
        let mut link = std::ptr::null_mut();
        unsafe {
            while lib.is_true(lib.FPDFLink_Enumerate(p, &mut pos, &mut link)) {
                let mut dest = lib.FPDFLink_GetDest(o.doc, link);
                if dest.is_null() {
                    let action = lib.FPDFLink_GetAction(link);
                    if !action.is_null() {
                        dest = lib.FPDFAction_GetDest(o.doc, action);
                    }
                }
                out.push(if dest.is_null() { -1 } else { lib.FPDFDest_GetDestPageIndex(o.doc, dest) });
            }
        }
        out
    }

    /// The top-level outline: each entry's title and target page index.
    fn outline(lib: &Lib, o: &Pdf) -> Vec<(String, c_int)> {
        let mut out = Vec::new();
        unsafe {
            let mut b = lib.FPDFBookmark_GetFirstChild(o.doc, std::ptr::null_mut());
            while !b.is_null() {
                let title = read_wide(|buf, len| lib.FPDFBookmark_GetTitle(b, buf as *mut c_void, len));
                let dest = lib.FPDFBookmark_GetDest(o.doc, b);
                out.push((title, if dest.is_null() { -1 } else { lib.FPDFDest_GetDestPageIndex(o.doc, dest) }));
                b = lib.FPDFBookmark_GetNextSibling(o.doc, b);
            }
        }
        out
    }

    fn page_says(lib: &Lib, o: &mut Pdf, page: usize, words: &str) -> bool {
        let t = page_text(lib, o, page);
        t.text(0, t.len()).contains(words)
    }

    /// Rotate, move, delete and insert pages; undo and redo; save and read
    /// back: the pages are in the new order, and the link and the outline
    /// still lead to the pages they named.
    fn pages_rearrange_and_keep_links_and_outlines(lib: &Lib, src: &Path, dir: &Path) {
        let Some(path) = three_pages(dir) else { return eprintln!("skipped pages: qpdf failed") };
        let mut o = Open::load(lib, 7, &path).unwrap();
        o.pages(lib, PageOp::Rotate { pages: vec![0], quarter_turns: 1 }).unwrap();
        o.pages(lib, PageOp::Move { pages: vec![2], gap: 0 }).unwrap(); // three, one, two
        o.pages(lib, PageOp::Delete { pages: vec![2] }).unwrap(); // three, one
        assert!(o.pages(lib, PageOp::Delete { pages: vec![0, 1] }).is_err(), "deleted every page");
        o.pages(lib, PageOp::Insert { from: src.to_path_buf(), at: 2 }).unwrap(); // three, one, form

        let check = |lib: &Lib, v: &mut Pdf| {
            assert_eq!(v.page_count(lib), 3);
            assert!(page_says(lib, v, 0, "Page three"));
            assert!(page_says(lib, v, 1, "Page one"));
            assert!(page_says(lib, v, 2, "Printer name"));
            let one = v.page(lib, 1).unwrap();
            assert_eq!(unsafe { lib.FPDFPage_GetRotation(one) }, 1, "page one keeps its turn");
            // Page one's link still leads to page three, now first.
            assert_eq!(link_targets(lib, v, 1), vec![0]);
            // The outline follows the pages; the deleted one leads nowhere.
            assert_eq!(outline(lib, v), vec![("One".to_string(), 1), ("Two".to_string(), -1), ("Three".to_string(), 0)]);
        };
        check(lib, &mut o.view);

        // Undo takes back the insert, redo puts it back.
        assert!(o.undo(lib).unwrap());
        assert_eq!(o.view.page_count(lib), 2);
        assert!(o.redo(lib).unwrap());
        check(lib, &mut o.view);

        // Extract pages one and the form into a file of their own.
        let out = dir.join("extracted.pdf");
        o.extract(lib, &[1, 2], &out).unwrap();
        let mut x = Pdf::load(lib, &out).unwrap();
        assert_eq!(x.page_count(lib), 2);
        assert!(page_says(lib, &mut x, 0, "Page one"));
        assert!(page_says(lib, &mut x, 1, "Printer name"));
        x.close(lib);

        o.save(lib, &path).unwrap();
        o.close(lib);
        assert!(run_ok(std::process::Command::new("qpdf").arg("--check").arg(&path).stdout(std::process::Stdio::null())), "qpdf --check");
        let mut back = Pdf::load(lib, &path).unwrap();
        check(lib, &mut back);
        back.close(lib);
        // poppler agrees on the order.
        let order = std::process::Command::new("pdftotext").arg(&path).arg("-").output();
        if let Ok(order) = order {
            let text = String::from_utf8_lossy(&order.stdout);
            let (a, b) = (text.find("Page three"), text.find("Page one"));
            assert!(a.is_some() && b.is_some() && a < b, "poppler reads: {text}");
        }
    }

    /// The writing engine's promise: the PDF draws every glyph where the
    /// layout put it. Typeset a document with most of what Markdown offers,
    /// read the PDF back through PDFium, and compare page by page: the same
    /// pages, the same text in the same order, each character within a
    /// fraction of a point of its glyph, and an outline of the headings.
    fn typesetting_puts_every_glyph_where_the_pdf_draws_it(lib: &Lib, dir: &Path) {
        use crate::writing::layout::Item;
        let md = dir.join("essay.md");
        let mut text = String::from("---\npage: A5\nalign: justify\n---\n# On Typesetting\n\n");
        for i in 0..6 {
            text.push_str(&format!(
                "Paragraph {i} has *emphasis*, **strength**, `code`, a [link](https://example.com) and ~~a strike~~. \
                 It runs long enough to wrap over several lines, so that justification has spaces to widen and the \
                 page has to break somewhere inside the document rather than only at its end.\n\n"
            ));
            if i == 2 {
                text.push_str("## A list and a quote\n\n- first point\n- second point, longer than the first\n  1. nested one\n  2. nested two\n\n> A quoted line, set in grey with a bar beside it.\n\n```\nfn main() {\n    println!(\"code\");\n}\n```\n\n---\n\n");
            }
        }
        text.push_str("\\pagebreak\n\n### After a page break\n\nThe end.\n");
        std::fs::write(&md, &text).unwrap();

        let mut fs = cce_ui::create_font_system_with_system_fonts();
        let (laid, bytes) = crate::writing::typeset(&mut fs, &md).unwrap();
        assert!(laid.pages.len() >= 3, "expected several pages, got {}", laid.pages.len());
        let pdf = dir.join("essay.pdf");
        std::fs::write(&pdf, &bytes).unwrap();
        assert!(run_ok(std::process::Command::new("qpdf").arg("--check").arg(&pdf).stdout(std::process::Stdio::null())), "qpdf --check");

        let mut o = Pdf::load(lib, &pdf).unwrap();
        assert_eq!(o.page_count(lib), laid.pages.len());
        let (mut compared, mut worst) = (0usize, 0.0f64);
        for (p, page) in laid.pages.iter().enumerate() {
            let size = Frame::of(lib, o.page(lib, p).unwrap()).size();
            assert!((size.w - laid.size.0 as f64).abs() < 0.5 && (size.h - laid.size.1 as f64).abs() < 0.5);
            // The layout's characters in drawing order, each with where its
            // glyph starts and its baseline (a cluster's later characters
            // share its glyph: no position of their own).
            let mut want: Vec<(char, Option<(f64, f64)>)> = Vec::new();
            for item in &page.items {
                let Item::Glyphs(run) = item else { continue };
                for g in &run.glyphs {
                    let cluster = &run.text[g.range.clone()];
                    let x = (g.x + g.x_offset * run.size) as f64;
                    for (k, c) in cluster.chars().filter(|c| !c.is_whitespace()).enumerate() {
                        want.push((c, (k == 0 && cluster.chars().filter(|c| !c.is_whitespace()).count() == 1).then_some((x, run.baseline as f64))));
                    }
                }
            }
            let got = page_text(lib, &mut o, p);
            let got: Vec<&TextChar> = got.chars.iter().filter(|c| !c.ch.is_whitespace() && !c.rect.is_empty()).collect();
            let want_text: String = want.iter().map(|w| w.0).collect();
            let got_text: String = got.iter().map(|c| c.ch).collect();
            assert_eq!(got_text, want_text, "page {}: the PDF's text differs", p + 1);
            for ((c, at), g) in want.iter().zip(&got) {
                let Some((x, baseline)) = at else { continue };
                assert!((g.rect.x0 - x).abs() < 0.75, "page {}: '{c}' at x {:.2}, laid out at {x:.2}", p + 1, g.rect.x0);
                compared += 1;
                worst = worst.max((g.rect.x0 - x).abs());
                assert!(g.rect.y0 - 0.5 < *baseline && *baseline < g.rect.y1 + 0.5, "page {}: '{c}' box {:?} misses baseline {baseline:.2}", p + 1, g.rect);
            }
        }
        eprintln!("typeset: {} pages, {compared} glyphs placed within {worst:.3} pt of the layout", laid.pages.len());
        let titles: Vec<String> = outline(lib, &o).into_iter().map(|(t, _)| t).collect();
        assert_eq!(titles, vec!["On Typesetting".to_string()], "top level of the outline");
        assert!(link_targets(lib, &mut o, 0).len() >= 1, "the link survives as an annotation");
        o.close(lib);
    }
}
