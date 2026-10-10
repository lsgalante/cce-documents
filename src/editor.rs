//! Editing a Markdown document on its pages.
//!
//! The text is the file's Markdown, held in cce-ui's `DocEditor` buffer
//! (lines, caret, selection, undo grouped by typing runs). The person sees
//! pages with the markup hidden; `writing::edit::Doc` maps between the two,
//! so every operation here is phrased in file bytes and visible characters:
//! a step moves one visible character, Backspace removes one (or joins two
//! blocks), Enter starts a new paragraph — or another list item, or a new
//! line in code. After each change the layout is redone incrementally
//! (`Layouter`) and the pages it reports are drawn again by the painter.
//!
//! The file is saved shortly after typing stops (`AUTOSAVE`), on Ctrl+S,
//! and before quitting; the source watcher's echo of our own save is
//! recognised by comparing the file with what was last written.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cce_ui::cosmic_text::FontSystem;
use cce_ui::widget::doc_editor::{Buffer, EditKind, Pos};

use crate::writing::edit::{Changes, Doc, Kind, Layouter};
use crate::writing::raster::Painter;
use crate::Message;

/// How long after the last change the file is saved.
pub const AUTOSAVE: Duration = Duration::from_millis(1500);

pub struct Editor {
    pub path: PathBuf,
    pub buffer: Buffer,
    layouter: Layouter,
    pub doc: Arc<Doc>,
    pub fs: Arc<Mutex<FontSystem>>,
    pub painter: Painter,
    /// The x a run of Up/Down keeps to.
    goal_x: Option<f32>,
    pub dirty: bool,
    pub save_due: Option<Instant>,
    /// The file as last read or written.
    pub disk: String,
    /// How long the last relayout took.
    pub last_layout: Duration,
}

/// The prefix of a Markdown line that makes it a list item (with any quote
/// markers before it), with an ordered item's number one on.
fn next_item_prefix(line: &str) -> Option<String> {
    let quote_end = line.len() - line.trim_start_matches(|c: char| c == '>' || c == ' ').len();
    let (quotes, rest) = line.split_at(quote_end);
    let indent = rest.len() - rest.trim_start().len();
    let body = &rest[indent..];
    let marker = if let Some(c) = body.chars().next().filter(|c| matches!(c, '-' | '*' | '+')) {
        body[1..].starts_with(' ').then(|| c.to_string())?
    } else {
        let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let after = &body[digits..];
        if digits == 0 || !(after.starts_with(". ") || after.starts_with(") ")) {
            return None;
        }
        let n: u64 = body[..digits].parse().ok()?;
        format!("{}{}", n + 1, &after[..1])
    };
    Some(format!("{quotes}{}{marker} ", &rest[..indent]))
}

impl Editor {
    pub fn open(path: &Path, fs: Arc<Mutex<FontSystem>>, notify: calloop::channel::Sender<Message>) -> Result<Editor, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut layouter = Layouter::default();
        let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let t0 = Instant::now();
        let (doc, _) = layouter.relayout(&mut fs.lock().unwrap_or_else(|e| e.into_inner()), &text, &base);
        let painter = Painter::start(Arc::clone(&fs), Arc::clone(&doc), notify);
        let first = doc.blocks.iter().find(|b| b.has_text()).map_or(0, |b| b.source_of(0, false));
        let mut ed = Editor {
            path: path.to_path_buf(),
            buffer: Buffer::new(&text),
            layouter,
            doc,
            fs,
            painter,
            goal_x: None,
            dirty: false,
            save_due: None,
            disk: text,
            last_layout: t0.elapsed(),
        };
        ed.set_caret(first, false);
        Ok(ed)
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(self.buffer.lines().len());
        let mut at = 0;
        for l in self.buffer.lines() {
            v.push(at);
            at += l.len() + 1;
        }
        v
    }

    pub fn pos_of(&self, src: usize) -> Pos {
        let starts = self.line_starts();
        let line = starts.partition_point(|&s| s <= src).saturating_sub(1);
        self.buffer.clamp(Pos::new(line, src - starts[line]))
    }

    pub fn src_of(&self, p: Pos) -> usize {
        self.line_starts().get(p.line).copied().unwrap_or(0) + p.col
    }

    pub fn caret(&self) -> usize {
        self.src_of(self.buffer.caret)
    }

    /// The selection as file bytes, ordered.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.buffer.selection().map(|(a, b)| (self.src_of(a), self.src_of(b)))
    }

    pub fn set_caret(&mut self, src: usize, select: bool) {
        let p = self.pos_of(src);
        self.buffer.set_caret(p, select);
    }

    /// Lay the document out again after a change, and hand the painter the
    /// new pages.
    fn relayout(&mut self) -> Changes {
        let text = self.buffer.text();
        let base = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let t0 = Instant::now();
        let (doc, changes) = self.layouter.relayout(&mut self.fs.lock().unwrap_or_else(|e| e.into_inner()), &text, &base);
        self.last_layout = t0.elapsed();
        *self.painter.doc.lock().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&doc);
        self.doc = doc;
        changes
    }

    fn changed(&mut self) -> Changes {
        self.dirty = true;
        self.save_due = Some(Instant::now() + AUTOSAVE);
        self.goal_x = None;
        self.relayout()
    }

    /// Replace file bytes `a..b` with `text`.
    fn replace(&mut self, a: usize, b: usize, text: &str, kind: EditKind) -> Changes {
        let (pa, pb) = (self.pos_of(a.min(b)), self.pos_of(a.max(b)));
        self.buffer.replace(pa, pb, text, kind);
        self.changed()
    }

    pub fn type_text(&mut self, text: &str) -> Changes {
        match self.selection() {
            Some((a, b)) => self.replace(a, b, text, EditKind::Other),
            None => {
                let c = self.caret();
                self.replace(c, c, text, if text.len() == 1 { EditKind::Typing } else { EditKind::Other })
            }
        }
    }

    pub fn delete(&mut self, forward: bool) -> Option<Changes> {
        if let Some((a, b)) = self.selection() {
            return Some(self.replace(a, b, "", EditKind::Other));
        }
        let range = self.doc.deletion(self.caret(), forward, &self.buffer.text())?;
        Some(self.replace(range.start, range.end, "", EditKind::Deleting))
    }

    /// Enter: a new paragraph, list item, or (in code) line.
    pub fn enter(&mut self) -> Changes {
        let c = self.selection().map_or(self.caret(), |(a, _)| a);
        let kind = self.doc.block_at(c).map(|b| (self.doc.blocks[b].kind, self.doc.blocks[b].src.start));
        let insert = match kind {
            Some((Kind::Code, _)) => "\n".to_string(),
            Some((Kind::Para { list: true }, start)) => {
                let line = self.buffer.line(self.pos_of(start).line).to_string();
                next_item_prefix(&line).map_or("\n\n".to_string(), |p| format!("\n{p}"))
            }
            _ => "\n\n".to_string(),
        };
        self.type_text(&insert)
    }

    /// Wrap the selection in a marker (`**`, `*`, `` ` ``, `~~`), or put a
    /// pair at the caret to type between.
    pub fn wrap(&mut self, marker: &str) -> Changes {
        match self.selection() {
            Some((a, b)) => {
                let inner = self.buffer.text_range(self.pos_of(a), self.pos_of(b));
                let out = self.replace(a, b, &format!("{marker}{inner}{marker}"), EditKind::Other);
                let start = a + marker.len();
                self.set_caret(start, false);
                self.set_caret(start + inner.len(), true);
                out
            }
            None => {
                let c = self.caret();
                let out = self.replace(c, c, &format!("{marker}{marker}"), EditKind::Other);
                self.set_caret(c + marker.len(), false);
                out
            }
        }
    }

    /// Make the caret's block a heading of `level` (0: a paragraph again).
    pub fn heading(&mut self, level: u8) -> Option<Changes> {
        let b = self.doc.block_at(self.caret())?;
        if !matches!(self.doc.blocks[b].kind, Kind::Heading(_) | Kind::Para { list: false }) {
            return None;
        }
        let start = self.doc.blocks[b].src.start;
        let line = self.buffer.line(self.pos_of(start).line).to_string();
        let hashes = line.len() - line.trim_start_matches('#').len();
        let old = if hashes > 0 && line[hashes..].starts_with(' ') { hashes + 1 } else { 0 };
        let new = if level == 0 { String::new() } else { format!("{} ", "#".repeat(level as usize)) };
        let caret = self.caret();
        let out = self.replace(start, start + old, &new, EditKind::Other);
        self.set_caret((caret + new.len()).saturating_sub(old).max(start), false);
        Some(out)
    }

    pub fn step(&mut self, forward: bool, select: bool) {
        let c = self.caret();
        let to = match self.selection() {
            Some((a, b)) if !select => {
                if forward {
                    b
                } else {
                    a
                }
            }
            _ => self.doc.step(c, forward),
        };
        self.set_caret(to, select);
        self.goal_x = None;
    }

    pub fn vertical(&mut self, down: bool, select: bool) {
        let c = self.caret();
        let goal = self.goal_x.or_else(|| self.doc.caret(c).map(|b| b.x)).unwrap_or(0.0);
        let to = self.doc.vertical(c, goal, down).unwrap_or_else(|| if down { self.doc_end() } else { self.doc_start() });
        self.set_caret(to, select);
        self.goal_x = Some(goal);
    }

    pub fn row_edge(&mut self, end: bool, select: bool) {
        let to = self.doc.row_edge(self.caret(), end);
        self.set_caret(to, select);
        self.goal_x = None;
    }

    pub fn doc_start(&self) -> usize {
        self.doc.blocks.iter().find(|b| b.has_text()).map_or(0, |b| b.source_of(0, false))
    }

    pub fn doc_end(&self) -> usize {
        self.doc.blocks.iter().rev().find(|b| b.has_text()).map_or(0, |b| b.source_of(b.text.len(), true))
    }

    pub fn word(&mut self, forward: bool, select: bool) {
        let p = self.buffer.caret;
        let to = if forward { self.buffer.word_right(p) } else { self.buffer.word_left(p) };
        // Land on a visible character, not inside markup.
        let src = self.src_of(to);
        let src = self.doc.block_at(src).map_or(src, |b| {
            let blk = &self.doc.blocks[b];
            blk.source_of(blk.text_of(src), !forward)
        });
        self.set_caret(src, select);
        self.goal_x = None;
    }

    pub fn select_all(&mut self) {
        let (a, z) = (self.doc_start(), self.doc_end());
        self.set_caret(a, false);
        self.set_caret(z, true);
    }

    pub fn copy(&self) -> Option<String> {
        self.selection().map(|(a, b)| self.doc.text_between(a, b))
    }

    pub fn undo(&mut self, redo: bool) -> Option<Changes> {
        let done = if redo { self.buffer.redo() } else { self.buffer.undo() };
        done.then(|| self.changed())
    }

    /// Write the file (through a temporary renamed over it).
    pub fn save(&mut self) -> Result<(), String> {
        let text = self.buffer.text();
        let tmp = self.path.with_extension(format!("md.cce-save-{}", std::process::id()));
        std::fs::write(&tmp, &text)
            .and_then(|()| {
                if let Ok(meta) = std::fs::metadata(&self.path) {
                    let _ = std::fs::set_permissions(&tmp, meta.permissions());
                }
                std::fs::rename(&tmp, &self.path)
            })
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("{}: {e}", self.path.display())
            })?;
        self.disk = text;
        self.dirty = false;
        self.save_due = None;
        Ok(())
    }

    /// Take the file as it now is on disk (changed by another program),
    /// keeping the caret near where it was.
    pub fn reload(&mut self, text: String) -> Changes {
        let c = self.caret();
        self.buffer.set_text(&text);
        self.disk = text;
        self.dirty = false;
        self.save_due = None;
        let changes = self.relayout();
        self.set_caret(c.min(self.doc_end()), false);
        changes
    }

    /// The document as PDF, typeset from the text as it is now.
    pub fn pdf(&self) -> Result<Vec<u8>, String> {
        let mut fs = self.fs.lock().unwrap_or_else(|e| e.into_inner());
        crate::writing::typeset_text(&mut fs, &self.buffer.text(), &self.path).map(|(_, bytes)| bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_items_continue() {
        assert_eq!(next_item_prefix("- one").as_deref(), Some("- "));
        assert_eq!(next_item_prefix("  3. three").as_deref(), Some("  4. "));
        assert_eq!(next_item_prefix("> * quoted").as_deref(), Some("> * "));
        assert_eq!(next_item_prefix("Just text"), None);
        assert_eq!(next_item_prefix("-not a list"), None);
    }
}
