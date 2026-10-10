//! The open document, the backend that reads it, and the GPU page cache.
//!
//! Two backends answer the same questions — page sizes, and a page
//! rasterized at a DPI. **PDFium** (`engine`) runs in-process on one thread
//! that owns the document, and also reads text and searches. **poppler**
//! (`poppler`) is the fallback when PDFium cannot be loaded: it spawns
//! `pdfinfo` / `pdftoppm` per request and can only show pages. Either way a
//! rendered page is uploaded via `cce_ui::vk::upload_rgba` (the upload queue
//! is thread-safe) and announced over the calloop channel so the engine
//! wakes and repaints.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{engine, poppler, Message};

/// GPU images kept resident across both stores (`PAGE_BUDGET` pages, plus
/// thumbnails) must stay well under the cce-ui image registry's hard cap of
/// 256 images.
pub const PAGE_BUDGET: usize = 24;
pub const THUMB_BUDGET: usize = 64;
/// Largest bitmap edge we'll upload; bigger pages are rendered at a capped
/// DPI.
const MAX_DIM: u32 = 8192;

/// Page size in display points: the page as shown before the user rotates
/// it, with its own /Rotate applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    pub w: f64,
    pub h: f64,
}

pub struct Document {
    /// Distinguishes this opening from every other, so late answers about
    /// a previous document are recognised and dropped.
    pub id: u64,
    pub path: PathBuf,
    pub pages: Vec<PageSize>,
}

/// A rendered page as delivered by a worker.
#[derive(Debug, Clone, Copy)]
pub struct Rendered {
    pub image: u32,
    pub dpi: u32,
}

pub struct Job {
    /// Which `PageStore` asked: the page view or the thumbnails.
    pub slot: u8,
    pub generation: u64,
    pub doc: u64,
    pub page: usize,
    pub dpi: u32,
    pub path: PathBuf,
    pub size: PageSize,
    /// Extra user rotation in quarter turns cw, applied to the pixels.
    pub quarter_turns: u8,
}

impl Job {
    /// The DPI capped so the bitmap's longer edge stays under MAX_DIM
    /// (page size is in points, 72/inch).
    pub fn capped_dpi(&self) -> u32 {
        let max_pts = self.size.w.max(self.size.h).max(1.0);
        (self.dpi as f64).min(MAX_DIM as f64 * 72.0 / max_pts).max(18.0) as u32
    }

    /// Rotate the page's pixels by the user's quarter turns and upload them.
    pub fn finish(&self, mut rgba: image::RgbaImage) -> Rendered {
        match self.quarter_turns % 4 {
            1 => rgba = image::imageops::rotate90(&rgba),
            2 => rgba = image::imageops::rotate180(&rgba),
            3 => rgba = image::imageops::rotate270(&rgba),
            _ => {}
        }
        let (w, h) = rgba.dimensions();
        let image = cce_ui::vk::upload_rgba(rgba.into_raw(), w, h);
        Rendered { image, dpi: self.dpi }
    }
}

pub enum Backend {
    Pdfium(engine::Engine),
    Poppler(poppler::Renderer),
}

impl Backend {
    /// PDFium when the library loads, poppler otherwise.
    pub fn start(notify: calloop::channel::Sender<Message>) -> Self {
        match engine::Engine::start(notify.clone()) {
            Ok(e) => Backend::Pdfium(e),
            Err(e) => {
                log::warn!("PDFium unavailable ({e}); viewing through poppler, without text or search");
                Backend::Poppler(poppler::Renderer::new(notify))
            }
        }
    }

    pub fn open(&self, path: &Path, id: u64) -> Result<Document, String> {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        if ext.as_deref() != Some("pdf") {
            return Err("unsupported file type".to_string());
        }
        let pages = match self {
            Backend::Pdfium(e) => e.open(path, id)?,
            Backend::Poppler(_) => poppler::page_sizes(path)?,
        };
        if pages.is_empty() {
            return Err("empty PDF".to_string());
        }
        Ok(Document { id, path: path.to_path_buf(), pages })
    }

    pub fn engine(&self) -> Option<&engine::Engine> {
        match self {
            Backend::Pdfium(e) => Some(e),
            Backend::Poppler(_) => None,
        }
    }

    fn render(&self, job: Job) {
        match self {
            Backend::Pdfium(e) => e.render(job),
            Backend::Poppler(p) => p.render(job),
        }
    }
}

enum PageState {
    Pending,
    /// `stale`: the document changed under this render; it stays on screen
    /// until the new one lands.
    Ready { r: Rendered, refreshing: bool, stale: bool, last_used: u64 },
    Failed,
}

/// Per-document GPU page cache: lazy render requests, DPI upgrades, LRU
/// eviction. `reset()` bumps the generation so late results from a previous
/// document/rotation are freed on arrival instead of displayed.
pub struct PageStore {
    /// Carried by its jobs, so each answer finds the store that asked.
    slot: u8,
    /// Pages kept on the GPU.
    budget: usize,
    states: HashMap<usize, PageState>,
    generation: u64,
    frame: u64,
}

impl PageStore {
    pub fn new(slot: u8, budget: usize) -> Self {
        Self { slot, budget, states: HashMap::new(), generation: 0, frame: 0 }
    }

    pub fn slot(&self) -> u8 {
        self.slot
    }

    /// The document changed under every page (an undo): render them all
    /// again, showing the old images meanwhile.
    pub fn invalidate_all(&mut self) {
        let pages: Vec<usize> = self.states.keys().copied().collect();
        for page in pages {
            self.invalidate(page);
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    pub fn reset(&mut self) {
        for (_, state) in self.states.drain() {
            if let PageState::Ready { r, .. } = state {
                cce_ui::vk::free_image(r.image);
            }
        }
        self.generation += 1;
    }

    /// The page's GPU image if resident (marks it used, queues a DPI upgrade
    /// when the resident render is stale); otherwise queues a render (once)
    /// and returns None.
    pub fn ensure(
        &mut self,
        backend: &Backend,
        doc: &Document,
        quarter_turns: u8,
        page: usize,
        want_dpi: u32,
    ) -> Option<Rendered> {
        let job = |dpi| Job {
            slot: self.slot,
            generation: self.generation,
            doc: doc.id,
            page,
            dpi,
            path: doc.path.clone(),
            size: doc.pages[page],
            quarter_turns,
        };
        match self.states.get_mut(&page) {
            Some(PageState::Ready { r, refreshing, stale, last_used }) => {
                *last_used = self.frame;
                if (r.dpi != want_dpi || *stale) && !*refreshing {
                    *stale = false;
                    *refreshing = true;
                    backend.render(job(want_dpi));
                }
                Some(*r)
            }
            Some(_) => None,
            None => {
                self.states.insert(page, PageState::Pending);
                backend.render(job(want_dpi));
                None
            }
        }
    }

    pub fn complete(&mut self, generation: u64, page: usize, result: Option<Rendered>) {
        if generation != self.generation {
            if let Some(r) = result {
                cce_ui::vk::free_image(r.image);
            }
            return;
        }
        let state = match result {
            Some(r) => PageState::Ready { r, refreshing: false, stale: false, last_used: self.frame },
            None => PageState::Failed,
        };
        if let Some(PageState::Ready { r, .. }) = self.states.insert(page, state) {
            cce_ui::vk::free_image(r.image);
        }
        self.evict();
    }

    /// The page changed (an edit): render it again, showing the old image
    /// meanwhile. A render already in flight shows the page from before the
    /// edit, so a fresh one is asked for regardless.
    pub fn invalidate(&mut self, page: usize) {
        match self.states.get_mut(&page) {
            Some(PageState::Ready { refreshing, stale, .. }) => {
                *refreshing = false;
                *stale = true;
            }
            // A first render still pending may predate the edit too: ask
            // again (the later answer lands last and wins).
            Some(PageState::Failed) | Some(PageState::Pending) => {
                self.states.remove(&page);
            }
            None => {}
        }
    }

    /// Free the least-recently-used pages once over budget; pages touched
    /// this frame are never evicted.
    fn evict(&mut self) {
        let resident = self.states.values().filter(|s| matches!(s, PageState::Ready { .. })).count();
        if resident <= self.budget {
            return;
        }
        let mut ready: Vec<(usize, u64)> = self
            .states
            .iter()
            .filter_map(|(p, s)| match s {
                PageState::Ready { last_used, .. } if *last_used < self.frame => Some((*p, *last_used)),
                _ => None,
            })
            .collect();
        ready.sort_by_key(|&(_, used)| used);
        for (page, _) in ready.into_iter().take(resident - self.budget) {
            if let Some(PageState::Ready { r, .. }) = self.states.remove(&page) {
                cce_ui::vk::free_image(r.image);
            }
        }
    }
}
