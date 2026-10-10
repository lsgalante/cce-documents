//! The writing engine: a Markdown file typeset onto pages, as PDF.
//!
//! `md` reads the file (front matter, then blocks, with where each piece of
//! text came from), `style` decides the look, `layout` shapes and paginates
//! with cosmic-text, `edit` keeps that layout up to date as the text
//! changes and answers the caret's questions, `raster` draws its pages for
//! the screen and `pdf` writes them with krilla. Screen and PDF are drawn
//! from the same layout, glyph for glyph (the engine test checks the PDF
//! against it).

pub mod edit;
pub mod layout;
pub mod md;
pub mod pdf;
pub mod raster;
pub mod style;

use std::path::{Path, PathBuf};

use cce_ui::cosmic_text::FontSystem;

pub use layout::Laid;

/// Whether a file is one this engine reads.
pub fn is_markdown(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(), Some("md" | "markdown"))
}

/// Typeset a Markdown file: the layout and its PDF.
#[cfg(test)]
pub fn typeset(fs: &mut FontSystem, path: &Path) -> Result<(Laid, Vec<u8>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    typeset_text(fs, &text, path)
}

/// Typeset Markdown text as if it were the file at `path` (its name titles
/// the PDF, its folder finds pictures): the layout and its PDF. The same
/// layout the editor draws.
pub fn typeset_text(fs: &mut FontSystem, text: &str, path: &Path) -> Result<(Laid, Vec<u8>), String> {
    let (front, _) = md::split_front_matter(text);
    let base = path.parent().unwrap_or(Path::new("."));
    let (doc, _) = edit::Layouter::default().relayout(fs, text, base);
    let laid = doc.laid(fs);
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
