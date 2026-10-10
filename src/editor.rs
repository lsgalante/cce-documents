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
//! Tables, footnotes, pictures and the contents are edited as what they
//! look like, too: Tab walks a table's cells and rows and columns come and
//! go whole (`add_row`, `remove_column`, …); a new footnote puts its
//! reference at the caret and its text at the end of the file, and the
//! caret into that text — at the foot of the page; a picked picture is
//! resized, aligned or removed by rewriting its `{…}` attributes.
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
use crate::writing::md::{ColAlign, Width};
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

/// The cells of a table's line: the bytes between its pipes (escaped
/// pipes, `\\|`, are text), without the leading and trailing pipe.
fn cells_of(line: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = line.as_bytes();
    let pipes: Vec<usize> = (0..bytes.len()).filter(|&i| bytes[i] == b'|' && (i == 0 || bytes[i - 1] != b'\\')).collect();
    let lead = line.trim_start().starts_with('|');
    let trail = line.trim_end().ends_with('|') && pipes.len() > usize::from(lead);
    let mut bounds = Vec::new();
    if !lead {
        bounds.push(0);
    }
    for &p in &pipes {
        bounds.push(p + 1);
    }
    let mut cells = Vec::new();
    for (i, &start) in bounds.iter().enumerate() {
        let end = match bounds.get(i + 1) {
            Some(&next) => next - 1,
            None if trail => break,
            None => line.len(),
        };
        cells.push(start..end);
    }
    cells
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
        let first = doc.units.iter().find(|u| u.live).map_or(0, |u| u.source_of(0, false));
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
        // A table cell is one line between pipes; a footnote, one paragraph.
        let text = match self.kind() {
            Some(Kind::Cell { .. }) => text.replace('|', "\\|").replace(['\n', '\r'], " "),
            Some(Kind::Note) | Some(Kind::Caption) => text.replace(['\n', '\r'], " "),
            _ => text.to_string(),
        };
        let text = text.as_str();
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

    /// The kind of text the caret is in.
    pub fn kind(&self) -> Option<Kind> {
        self.doc.unit_at(self.caret()).map(|u| self.doc.units[u].kind)
    }

    /// Enter: a new paragraph, list item, or (in code) line; in a table,
    /// the cell below (a new row from the last). Nothing in a footnote or
    /// a caption, which are one paragraph each.
    pub fn enter(&mut self) -> Option<Changes> {
        let c = self.selection().map_or(self.caret(), |(a, _)| a);
        let u = self.doc.unit_at(c)?;
        let (kind, start) = (self.doc.units[u].kind, self.doc.units[u].src.start);
        let insert = match kind {
            Kind::Code => "\n".to_string(),
            Kind::Para { list: true } => {
                let line = self.buffer.line(self.pos_of(start).line).to_string();
                next_item_prefix(&line).map_or("\n\n".to_string(), |p| format!("\n{p}"))
            }
            Kind::Cell { row, col } => {
                let b = self.doc.units[u].block;
                return match self.doc.cell(b, row + 1, col) {
                    Some(n) => {
                        let unit = &self.doc.units[n];
                        let to = unit.source_of(unit.text.len(), true);
                        self.set_caret(to, false);
                        None
                    }
                    None => self.add_row(),
                };
            }
            Kind::Note | Kind::Caption => return None,
            _ => "\n\n".to_string(),
        };
        Some(self.type_text(&insert))
    }

    /// Replace several ranges of the file as one undoable edit: the span
    /// covering them all is rewritten (the buffer undoes one replace at a
    /// time).
    fn replace_many(&mut self, mut edits: Vec<(std::ops::Range<usize>, String)>) -> Changes {
        edits.sort_by_key(|(r, _)| r.start);
        let text = self.buffer.text();
        let (a, z) = (edits.first().map_or(0, |(r, _)| r.start), edits.iter().map(|(r, _)| r.end).max().unwrap_or(0));
        let mut out = String::new();
        let mut at = a;
        for (r, t) in &edits {
            out.push_str(&text[at..r.start]);
            out.push_str(t);
            at = r.end;
        }
        out.push_str(&text[at..z]);
        self.replace(a, z, &out, EditKind::Other)
    }

    /// A new footnote: its reference at the caret, its (empty) text at the
    /// end of the file, and the caret in that text — drawn at the foot of
    /// the page. Labels are numbers, the lowest one free.
    pub fn footnote(&mut self) -> Changes {
        let text = self.buffer.text();
        let label = (1..).find(|n| !text.contains(&format!("[^{n}]"))).unwrap_or(1);
        let at = self.selection().map_or(self.caret(), |(_, b)| b);
        let end = text.len();
        let tail = if text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let def = format!("{tail}[^{label}]: ");
        let changes = self.replace_many(vec![(at..at, format!("[^{label}]")), (end..end, format!("{def}\n"))]);
        let caret = self.buffer.text().len() - 1;
        self.set_caret(caret, false);
        changes
    }

    /// From a footnote's text back to just after its reference.
    pub fn leave_note(&mut self) -> bool {
        let Some(u) = self.doc.unit_at(self.caret()) else { return false };
        if self.doc.units[u].kind != Kind::Note {
            return false;
        }
        let Some(n) = self.doc.blocks[self.doc.units[u].block].shaped.note else { return false };
        let n = n.to_string();
        let to = self.doc.units.iter().find_map(|unit| unit.atoms.iter().find(|(tr, _)| unit.text.get(tr.clone()) == Some(n.as_str())).map(|(_, sr)| sr.end));
        match to {
            Some(to) => {
                self.set_caret(to, false);
                true
            }
            None => false,
        }
    }

    /// Put a block (a table, the contents, a page break, a picture) at the
    /// caret: before its paragraph when the caret is at the start, after
    /// it at the end, else splitting it. The caret goes where typing
    /// continues: into `focus` bytes into the inserted text, or after it.
    pub fn insert_block(&mut self, md: &str, focus: Option<usize>) -> Changes {
        let c = self.selection().map_or(self.caret(), |(a, _)| a);
        let unit = self.doc.unit_at(c).map(|u| self.doc.units[u].clone());
        let (at, before, after) = match unit {
            Some(u) if u.kind.flows() && u.text_of(c) == 0 => (u.src.start, "", "\n\n"),
            Some(u) if u.text_of(c) >= u.text.len() => (c, "\n\n", "\n\n"),
            Some(_) => (c, "\n\n", "\n\n"),
            None => (self.buffer.text().len(), "\n\n", "\n"),
        };
        // A newline already there makes half the blank line after.
        let after = if after == "\n\n" && self.buffer.text()[at..].starts_with('\n') { "\n" } else { after };
        let out = self.replace(at, at, &format!("{before}{md}{after}"), EditKind::Other);
        let to = at + before.len() + focus.unwrap_or(md.len() + after.len());
        self.set_caret(to, false);
        out
    }

    /// A new table, two columns by two rows, the caret in its first cell.
    pub fn table(&mut self) -> Option<Changes> {
        if matches!(self.kind(), Some(Kind::Cell { .. }) | Some(Kind::Note) | Some(Kind::Caption)) {
            return None;
        }
        Some(self.insert_block("|  |  |\n| --- | --- |\n|  |  |\n|  |  |", Some(2)))
    }

    /// The table the caret is in: its block, the caret's row and column,
    /// and the table's source split into lines (with their offsets).
    fn table_at(&self) -> Option<(usize, usize, usize, usize, Vec<(usize, String)>)> {
        let u = self.doc.unit_at(self.caret())?;
        let Kind::Cell { row, col } = self.doc.units[u].kind else { return None };
        let b = self.doc.units[u].block;
        let src = self.doc.blocks[b].src.clone();
        let text = self.buffer.text();
        let mut lines = Vec::new();
        let mut at = src.start;
        for l in text[src.clone()].split_inclusive('\n') {
            lines.push((at, l.trim_end_matches(['\n', '\r']).to_string()));
            at += l.len();
        }
        Some((b, row, col, self.doc.blocks[b].columns, lines))
    }

    /// Put the caret at the end of a cell of block `b`.
    fn to_cell(&mut self, b: usize, row: usize, col: usize) {
        if let Some(u) = self.doc.cell(b, row, col) {
            let unit = &self.doc.units[u];
            let to = unit.source_of(unit.text.len(), true);
            self.set_caret(to, false);
        }
    }

    /// Tab (Shift+Tab): the next (previous) cell; from the last, a new row.
    pub fn cell_step(&mut self, forward: bool) -> Option<Changes> {
        let (b, row, col, cols, lines) = self.table_at()?;
        let rows = lines.len().saturating_sub(1);
        let (r, c) = if forward {
            if col + 1 < cols {
                (row, col + 1)
            } else {
                (row + 1, 0)
            }
        } else if col > 0 {
            (row, col - 1)
        } else if row > 0 {
            (row - 1, cols - 1)
        } else {
            return None;
        };
        if r >= rows {
            let out = self.add_row();
            self.to_cell(b, r, 0);
            return out;
        }
        self.to_cell(b, r, c);
        None
    }

    /// A row of empty cells below the caret's (above the first body row,
    /// from the header).
    pub fn add_row(&mut self) -> Option<Changes> {
        let (b, row, col, cols, lines) = self.table_at()?;
        // Row r is line r, past the header's delimiter line.
        let after = if row == 0 { 1 } else { row + 1 };
        let (at, line) = lines.get(after)?;
        let new = format!("\n|{}", "  |".repeat(cols.max(1)));
        let end = at + line.len();
        let out = self.replace(end, end, &new, EditKind::Other);
        self.to_cell(b, row + 1, col);
        Some(out)
    }

    /// Remove the caret's row (not the header).
    pub fn remove_row(&mut self) -> Option<Changes> {
        let (b, row, col, _, lines) = self.table_at()?;
        if row == 0 || lines.len() <= 3 {
            return None;
        }
        let (at, line) = &lines[row + 1];
        let out = self.replace(at - 1, at + line.len(), "", EditKind::Other);
        self.to_cell(b, row.min(lines.len() - 3), col);
        Some(out)
    }

    /// An empty column after the caret's.
    pub fn add_column(&mut self) -> Option<Changes> {
        let (b, row, col, _, lines) = self.table_at()?;
        let mut edits = Vec::new();
        for (i, (at, line)) in lines.iter().enumerate() {
            let cells = cells_of(line);
            let pos = cells.get(col).map_or(line.len(), |c| c.end);
            let new = if i == 1 { "| --- " } else { "|  " };
            edits.push((at + pos..at + pos, new.to_string()));
        }
        let out = self.replace_many(edits);
        self.to_cell(b, row, col + 1);
        Some(out)
    }

    /// Remove the caret's column (not the last one).
    pub fn remove_column(&mut self) -> Option<Changes> {
        let (b, row, col, cols, lines) = self.table_at()?;
        if cols <= 1 {
            return None;
        }
        let mut edits = Vec::new();
        for (at, line) in &lines {
            let cells = cells_of(line);
            let Some(c) = cells.get(col) else { continue };
            // The cell and the pipe before it; the first cell of a row
            // without a leading pipe, with the pipe after it.
            let r = if c.start == 0 { 0..(c.end + 1).min(line.len()) } else { c.start - 1..c.end };
            edits.push((at + r.start..at + r.end, String::new()));
        }
        let out = self.replace_many(edits);
        self.to_cell(b, row, col.min(cols - 2));
        Some(out)
    }

    /// Set a picture's attributes (block `b`), keeping what is not given.
    pub fn set_picture(&mut self, b: usize, width: Option<Width>, align: Option<ColAlign>) -> Option<Changes> {
        let block = self.doc.blocks.get(b)?;
        let p = block.picture.clone()?;
        let width = width.or(p.width);
        let align = align.or(p.align);
        let mut words = Vec::new();
        match width {
            Some(Width::Share(f)) => words.push(format!("width={}%", (f * 100.0).round())),
            Some(Width::Points(pt)) => words.push(format!("width={}px", (pt / 0.75).round())),
            None => {}
        }
        if let Some(a) = align {
            words.push(format!("align={}", match a {
                ColAlign::Left => "left",
                ColAlign::Center => "center",
                ColAlign::Right => "right",
            }));
        }
        let attrs = if words.is_empty() { String::new() } else { format!("{{{}}}", words.join(" ")) };
        let range = p.attrs.clone().unwrap_or(block.src.end..block.src.end);
        Some(self.replace(range.start, range.end, &attrs, EditKind::Other))
    }

    /// Remove a picture (block `b`) and the blank line after it.
    pub fn remove_picture(&mut self, b: usize) -> Option<Changes> {
        let block = self.doc.blocks.get(b)?;
        let end = self.doc.blocks.get(b + 1).map_or(block.src.end, |n| n.src.start).max(block.src.end);
        let (a, z) = (block.src.start, end);
        let out = self.replace(a, z, "", EditKind::Other);
        let c = self.caret();
        self.set_caret(c, false);
        Some(out)
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
        let u = self.doc.unit_at(self.caret())?;
        if !matches!(self.doc.units[u].kind, Kind::Heading(_) | Kind::Para { list: false }) {
            return None;
        }
        let start = self.doc.units[u].src.start;
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
        self.doc.units.iter().find(|u| u.live).map_or(0, |u| u.source_of(0, false))
    }

    pub fn doc_end(&self) -> usize {
        self.doc.units.iter().rev().find(|u| u.live).map_or(0, |u| u.source_of(u.text.len(), true))
    }

    pub fn word(&mut self, forward: bool, select: bool) {
        let p = self.buffer.caret;
        let to = if forward { self.buffer.word_right(p) } else { self.buffer.word_left(p) };
        // Land on a visible character, not inside markup.
        let src = self.src_of(to);
        let src = self.doc.unit_at(src).map_or(src, |u| {
            let unit = &self.doc.units[u];
            unit.source_of(unit.text_of(src), !forward)
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

    #[test]
    fn table_lines_split_into_cells() {
        let l = "| a | b \\| c |  |";
        let cells: Vec<&str> = cells_of(l).into_iter().map(|r| &l[r]).collect();
        assert_eq!(cells, vec![" a ", " b \\| c ", "  "]);
        let l = "a | b";
        let cells: Vec<&str> = cells_of(l).into_iter().map(|r| &l[r]).collect();
        assert_eq!(cells, vec!["a ", " b"]);
    }

    /// An editor on a file in a fresh folder, with its own font system.
    fn editor_on(text: &str) -> (Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cce-documents-ed-{}-{}", std::process::id(), text.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.md");
        std::fs::write(&path, text).unwrap();
        let fs = Arc::new(Mutex::new(cce_ui::create_font_system_with_system_fonts()));
        let (tx, _rx) = calloop::channel::channel();
        (Editor::open(&path, fs, tx).unwrap(), dir)
    }

    #[test]
    fn tables_grow_and_shrink_and_tab_walks_them() {
        let (mut ed, dir) = editor_on("Intro.\n");
        let end = ed.doc_end();
        ed.set_caret(end, false);
        ed.table().unwrap();
        assert_eq!(ed.buffer.text(), "Intro.\n\n|  |  |\n| --- | --- |\n|  |  |\n|  |  |\n\n");
        // The caret is in the first header cell: typing fills it.
        assert_eq!(ed.kind(), Some(Kind::Cell { row: 0, col: 0 }));
        ed.type_text("Name");
        ed.cell_step(true);
        ed.type_text("Qty|ish");
        assert!(ed.buffer.text().contains("| Name | Qty\\|ish |"), "{}", ed.buffer.text());
        // Tab on to the last cell, then once more: a new row.
        for _ in 0..4 {
            ed.cell_step(true);
        }
        assert_eq!(ed.kind(), Some(Kind::Cell { row: 2, col: 1 }));
        ed.cell_step(true);
        assert_eq!(ed.kind(), Some(Kind::Cell { row: 3, col: 0 }));
        assert_eq!(ed.buffer.text().matches("|  |  |").count(), 3);
        // A column after the first, then it goes again.
        ed.add_column().unwrap();
        assert_eq!(ed.kind(), Some(Kind::Cell { row: 3, col: 1 }));
        assert!(ed.buffer.text().contains("| Name |  | Qty\\|ish |"), "{}", ed.buffer.text());
        assert!(ed.buffer.text().contains("| --- | --- | --- |"), "{}", ed.buffer.text());
        ed.remove_column().unwrap();
        assert!(ed.buffer.text().contains("| Name | Qty\\|ish |"), "{}", ed.buffer.text());
        // Remove the row the caret is in; the header row cannot go.
        ed.remove_row().unwrap();
        assert_eq!(ed.buffer.text().matches("|  |  |").count(), 2);
        let head = ed.doc.units.iter().find(|u| u.text == "Name").unwrap().source_of(0, false);
        ed.set_caret(head, false);
        assert!(ed.remove_row().is_none());
        // One undo takes back one table edit.
        ed.undo(false);
        assert_eq!(ed.buffer.text().matches("|  |  |").count(), 3);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_footnote_goes_in_and_the_caret_into_its_text_and_back() {
        let (mut ed, dir) = editor_on("# T\n\nA claim here.\n");
        let at = ed.doc.units[1].source_of("A claim".len(), true);
        ed.set_caret(at, false);
        ed.footnote();
        assert_eq!(ed.buffer.text(), "# T\n\nA claim[^1] here.\n\n[^1]: \n");
        assert_eq!(ed.kind(), Some(Kind::Note));
        ed.type_text("Source.");
        assert_eq!(ed.buffer.text(), "# T\n\nA claim[^1] here.\n\n[^1]: Source.\n");
        // Esc's way back: just after the reference.
        assert!(ed.leave_note());
        assert_eq!(ed.caret(), "# T\n\nA claim[^1]".len());
        // A second footnote takes the next free label.
        ed.footnote();
        assert!(ed.buffer.text().contains("A claim[^1][^2] here.") && ed.buffer.text().ends_with("[^2]: \n"), "{}", ed.buffer.text());
        // And one undo removes it, reference and text together.
        ed.undo(false);
        assert_eq!(ed.buffer.text(), "# T\n\nA claim[^1] here.\n\n[^1]: Source.\n");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_picked_picture_is_resized_aligned_and_removed() {
        let (mut ed, dir) = editor_on("Text.\n\n![Box](box.png)\n\nMore.\n");
        image::RgbaImage::from_pixel(80, 40, image::Rgba([0, 0, 0, 255])).save(dir.join("box.png")).unwrap();
        ed.reload(ed.buffer.text());
        let b = ed.doc.blocks.iter().position(|b| b.picture.is_some()).unwrap();
        ed.set_picture(b, Some(Width::Share(0.4)), None).unwrap();
        assert_eq!(ed.buffer.text(), "Text.\n\n![Box](box.png){width=40%}\n\nMore.\n");
        ed.set_picture(b, None, Some(ColAlign::Left)).unwrap();
        assert_eq!(ed.buffer.text(), "Text.\n\n![Box](box.png){width=40% align=left}\n\nMore.\n");
        ed.remove_picture(b).unwrap();
        assert_eq!(ed.buffer.text(), "Text.\n\nMore.\n");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn blocks_go_in_before_after_or_between() {
        let (mut ed, dir) = editor_on("## Head\n\nOne two.\n");
        // At a heading's start: before it, markup and all.
        let head = ed.doc.units[0].source_of(0, false);
        ed.set_caret(head, false);
        ed.insert_block("\\pagebreak", None);
        assert_eq!(ed.buffer.text(), "\\pagebreak\n\n## Head\n\nOne two.\n");
        // Mid-paragraph: split around it.
        let mid = ed.doc.units.iter().find(|u| u.text == "One two.").unwrap().source_of(3, false);
        ed.set_caret(mid, false);
        ed.insert_block("[TOC]", None);
        assert_eq!(ed.buffer.text(), "\\pagebreak\n\n## Head\n\nOne\n\n[TOC]\n\n two.\n");
        std::fs::remove_dir_all(dir).ok();
    }
}
