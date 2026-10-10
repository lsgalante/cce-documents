//! The writing engine: a Markdown file typeset onto pages, as PDF.
//!
//! `md` reads the file (front matter, then blocks), `style` decides the
//! look, `layout` shapes and paginates with cosmic-text, and `pdf` writes
//! the result with krilla. The app shows that PDF through the same PDFium
//! page view as any other — so the page on screen *is* the page that
//! prints, glyph for glyph — and exports it unchanged.
//!
//! Typesetting needs a `FontSystem` with the system's fonts, which is slow
//! to build, so it lives on a thread of its own (`Typesetter`), made once.

pub mod layout;
pub mod md;
pub mod pdf;
pub mod style;

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use cce_ui::cosmic_text::FontSystem;

pub use layout::Laid;

/// Whether a file is one this engine reads.
pub fn is_markdown(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("md" | "markdown"))
}

/// Typeset a Markdown file: the layout and its PDF.
pub fn typeset(fs: &mut FontSystem, path: &Path) -> Result<(Laid, Vec<u8>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (front, body) = md::split_front_matter(&text);
    let style = style::Style::for_document(&front);
    let blocks = md::parse(body);
    let base = path.parent().unwrap_or(Path::new("."));
    let laid = layout::typeset(fs, &blocks, &style, base);
    let title = front
        .iter()
        .find(|(k, _)| k == "title")
        .map(|(_, v)| v.clone())
        .or_else(|| laid.outline.first().map(|e| e.title.clone()))
        .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let bytes = pdf::write(fs, &laid, &title)?;
    Ok((laid, bytes))
}

/// The cache file a document's typeset PDF is kept in: one per source
/// path, replaced on each typesetting.
pub fn cache_path(source: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    base.join("cce-documents").join(format!("{stem}-{:016x}.pdf", h.finish()))
}

type Job = (PathBuf, mpsc::Sender<Result<PathBuf, String>>);

/// The thread that typesets, and owns the font system for it.
pub struct Typesetter {
    tx: mpsc::Sender<Job>,
}

impl Typesetter {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("typesetter".into())
            .spawn(move || {
                let mut fs: Option<FontSystem> = None;
                for (source, reply) in rx {
                    let fs = fs.get_or_insert_with(cce_ui::create_font_system_with_system_fonts);
                    let result = typeset(fs, &source).and_then(|(_, bytes)| {
                        let out = cache_path(&source);
                        if let Some(dir) = out.parent() {
                            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                        }
                        // Renamed into place: the engine may still be reading
                        // the previous one, which keeps its bytes.
                        let tmp = out.with_extension(format!("tmp-{}", std::process::id()));
                        std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &out)).map_err(|e| e.to_string())?;
                        Ok(out)
                    });
                    let _ = reply.send(result);
                }
            })
            .expect("spawn the typesetter");
        Self { tx }
    }

    /// Typeset `source` into its cache PDF, waiting for it.
    pub fn typeset(&self, source: &Path) -> Result<PathBuf, String> {
        let (reply, rx) = mpsc::channel();
        self.tx.send((source.to_path_buf(), reply)).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }
}
