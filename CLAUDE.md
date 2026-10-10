# cce-documents

The document app: a PDF viewer today, growing into a PDF editor and a word
processor in one window. Read the workspace guide
(`../cce-compositor/WORKSPACE.md`) first; this file covers only what is
particular to this crate. The plan — the two engines, the library and
format choices, six milestones — is the design doc "cce-documents: PDF
editor and word processor — proposal"
(claude.ai/code/artifact/50527ccf-8449-496e-94ba-7ed26dc5a2f6). Milestones
1–3 are here: PDFium rendering, text selection and copy, search;
highlight / underline / strike, ink, notes, form filling (text fields,
check boxes, radio buttons) and incremental saves; and the page sidebar —
thumbnails, reorder by drag, rotate, delete, insert (merge) and extract —
with undo and redo of every kind of change. Milestone 4 is here too: a
Markdown file opens typeset onto pages (read-only; it re-typesets on each
save of the file), exports as PDF and prints through the Print portal.
Split from cce-preview on 2026-10-09; pictures went to cce-image.

## Shape

| File | What it owns |
| --- | --- |
| `main.rs` | The `Application`: page layout and the viewport (scroll, zoom at the pointer, rotation), the page sidebar (thumbnails, selection, drag to reorder, its buttons), tools (select / draw / note), selection, picking marks, the field and note editors, the find bar, save, keys |
| `chrome.rs` | The markup toolbar (wrapping into rows when narrow), buttons, and the one-line text fields, drawn into the display list and measured with the renderer's shaping |
| `doc.rs` | `Backend` (PDFium or poppler), `Document`, and the GPU `PageStore` — two of them, the pages and the thumbnails, told apart by `slot` (lazy renders, DPI upgrades, `invalidate` after an edit, LRU, generations) |
| `engine.rs` | PDFium on its own thread, through its C API: open, render, text, search, annotations, edits, the edit journal, save; `Frame` maps PDF page space to display points both ways |
| `markup.rs` | Pure data: `Annot` as the app sees it, `Edit`, `PageOp`, picking under the pointer, where moved pages land |
| `trim.rs` | Cuts PDFium's incremental update to the objects that changed (lopdf) |
| `text.rs` | Pure selection geometry: a page's chars with boxes, caret hit-testing, highlight rects, copied text |
| `poppler.rs` | The fallback: `pdfinfo` sizes, `pdftoppm` renders, view-only |
| `print.rs` | The Print portal (ashpd on its own thread): `PreparePrint`, then `Print` with the PDF's fd |
| `writing/md.rs` | Markdown → flat blocks (pulldown-cmark), front matter, `\pagebreak` |
| `writing/style.rs` | Page, margins, faces, spacing; the built-in `manuscript` style, user KDL styles, front-matter overrides |
| `writing/layout.rs` | Shaping (cosmic-text) into rows, then `paginate` (keep-with-next, no orphans or widows) |
| `writing/pdf.rs` | The pages written with krilla, glyph for glyph; outline from headings, links |
| `writing/mod.rs` | `typeset`, the cache file, and `Typesetter`, the thread that owns the font system |

## PDFium

- **It is a native library outside cargo.** `scripts/fetch-pdfium` installs
  a pinned build (bblanchon/pdfium-binaries, chromium/8086) to
  `~/.local/lib/libpdfium.so`. The app looks at `$CCE_PDFIUM`, beside its
  binary, `~/.local/lib`, then the system path. Without it the app still
  opens PDFs through poppler, with no text or search (a warning is logged,
  and the find bar says so).
- **One thread owns it.** PDFium is not thread-safe, so the library, the
  document, its form-fill environment and its pages live on the `pdfium`
  thread; the app sends requests. Opening, saving, a field's value and
  `text_now` wait for their answer; renders, text, annotations, edits and
  search come back as `Message`s. Search runs one page per loop turn and
  yields to queued requests, so it never holds up a render.
- **The C API, not pdfium-render's wrappers.** `Pdfium::new` is never
  called; the bindings are used directly (`FPDF_InitLibrary` first). The
  wrappers keep the form handle private, never call `FORM_OnAfterLoadPage`
  (so typing into fields and clicking check boxes cannot work through
  them), have no ink strokes, and save only whole files. Their
  `set_checked` also assumes a check box's on state is `/Yes`.
- **`pdfium-render` is pinned to API `pdfium_7881`**, the newest it knows;
  the fetched library is newer, which PDFium's C API allows. Bump both
  deliberately.

## Saving (read before touching `engine::Open::save`)

PDFium's incremental save (`FPDF_INCREMENTAL`) appends **every object it
holds in memory**, changed or not, and `FPDF_LoadPage` alone loads the
page's content stream and resources. Saving straight from the document on
screen added 285 KB to the 276 KB CUPS sample for one highlight. So:

1. Every change is applied to the shown document **and recorded** (`Op`: an
   `Edit` with the /NM it gave a new annotation, or a `PageOp`).
2. A save loads the file afresh (never rendered), replays the journal —
   closing and reloading each edited page right after its edit, which
   rebuilds the page's annotation list and with it the missing appearance
   streams (later page operations renumber pages, so it cannot wait) — and
   asks PDFium for an incremental save. The form-fill environment must be
   on for that rebuild; on a `/NeedAppearances` form it also gives the
   touched page's fields appearances, which the save carries (~10 KB).
3. `trim::trim` then keeps only the objects that are new or differ from
   the original (streams compared decoded), and writes them as the update
   with an explicit `/Prev` (lopdf omits it when it did not read the offset
   itself). If lopdf cannot read a file, PDFium's untrimmed update is saved.
4. Written to a sibling temporary and renamed over the target. The journal
   empties and the saved file becomes the source of the next save, which
   appends a second update.

**Undo and redo** use the same journal: undo pops the last op and rebuilds
the shown document (fresh load + replay), redo applies the popped op again.
Both, and every page operation, answer with `Message::Restructured`, after
which the app drops everything it keeps by page index (texts, annotations,
search hits, selections) and either invalidates its renders (same page
sizes: the old images stay up until the new land) or resets them. Nothing
before the last save can be undone.

**Page operations** close every loaded page first (indices shift under
them). PDFium keeps page objects when it reorders, so links and outline
entries keep leading to their pages; a deleted page's entries lead nowhere
(`FPDFDest_GetDestPageIndex` = -1), never to the wrong page. Extract copies
pages of the document as shown into a new file (a full save, not
incremental); fields there lose their form (no AcroForm is copied).

**Appearance streams**: PDFium writes an annotation's /AP only when a
render (or an annotation-list rebuild) first meets it without one; never on
create. Without /AP a mark is invisible in readers that do not draw their
own. The shown document gets them by a 1×1 render after each edit
(`generate_appearances`), the saved copy by the page reload above.

The test `engine::tests::pdfium_round_trips` covers all of it: marks, a
note, undo, a text field typed into, a check box whose on state is `/On`,
the original bytes kept, the update under 64 KB after a render, a second
save appending, `qpdf --check`, PDFium reading it back, `/AP` on every
mark, poppler drawing the highlight; and on a hand-written three-page PDF
with an outline and a link: rotate, move, delete, insert, undo, redo,
extract and save, with the link and outline still leading to the right
pages afterwards and poppler reading the new order.

## Writing (milestone 4)

**The screen shows the PDF.** A Markdown document is typeset to a PDF in
the cache (`writing::cache_path`, renamed into place) and opened through
the ordinary PDFium page view, so what is on screen is what exports and
prints, by construction. `Document::path` is the `.md`, `Document::file`
the PDF the backend reads, `Document::writing` gates every PDF-editing
path (`DocumentsApp::editable`). A thread polls the source's mtime every
400 ms; a change re-typesets and reopens, keeping page and zoom (a
source that fails to typeset keeps the old pages and says why).

**The layout is the source of truth for glyph positions**, and the PDF
must agree with it — milestone 5 will draw the same layout live. The
test `typesetting_puts_every_glyph_where_the_pdf_draws_it` typesets a
multi-page sample, reads it back through PDFium and requires the same
pages, identical text in the same order, and every single-glyph character
within 0.75 pt of its laid-out x (measured: 1335 glyphs within 0.121 pt).
Things that matter for it: each krilla glyph advances to the next glyph's
start (justification moves glyphs, it does not widen advances), runs end
on the next run's start, and items are drawn in reading order (a list
marker before its text — PDFium orders text by position).

**Styles** live in `~/.config/cce/documents/styles/<name>.kdl` (the
built-in `writing::style::MANUSCRIPT` is the example); page lengths in mm,
type in pt. Front matter may override `page`, `margins`, `font`, `size`,
`align`, `page-numbers`, and set `title` (the PDF's). Fonts are what
cosmic-text resolves from the system (`create_font_system_with_system_fonts`
— the bundled set alone lacks Noto Serif), embedded as subsets.

Not yet: first-line indents (cosmic-text has none), tables, footnotes
(milestone 6), and `.md` is not claimed in the desktop entry (that would
take Markdown files from the other editors).

## Coordinates (the thing to get right)

Three spaces, and each value says which it is in:

1. **PDF page space** — points, origin bottom-left of the media box, before
   the page's /Rotate. Only `engine.rs` sees it.
2. **Display points** — the page as PDFium renders it: /Rotate applied,
   origin top-left of the bounding box (crop ∩ media), y down. `PageSize`,
   `PageText` and search hits are all in this space. `Frame` converts.
3. **Screen px** — after the user's quarter turns (`turn` / `unturn` in
   `main.rs`), zoom and scroll.

`a_rotated_page_maps_its_text_where_it_is_drawn` (inside
`engine::tests::pdfium_round_trips`) checks (1)→(2) on a page and a
qpdf-rotated copy of it, and that `page_point` undoes `point`. Markup quads
are written in page space with upper-left, upper-right, lower-left,
lower-right corners, so "upper" follows the text whatever the /Rotate.

Two quirks met on the way: `FPDF_SetFormFieldHighlightColor` takes a
Windows COLORREF (0x00BBGGRR), not the 0xRRGGBB its header promises; and
qpdf exits 3 (not 0) when it repaired a file, which the tests accept.

## Verifying

- `cargo test -p cce-documents` — the text geometry, picking, and the
  PDFium round trip above (skips parts without PDFium, qpdf, poppler or the
  CUPS sample PDF; `-- --nocapture` shows what it skipped).
- **A shadow does not find PDFium on its own**: the shadow's HOME hides
  `~/.local/lib`. Spawn with `CCE_PDFIUM=$HOME/.local/lib/libpdfium.so`
  (absolute, expanded before the spawn), or you are testing the poppler
  fallback. A test document with a rotated page:
  `qpdf --rotate=+90:1 in.pdf turned.pdf` then
  `qpdf --empty --pages in.pdf turned.pdf -- multi.pdf`.
- Ctrl+C lands in the shadow's own clipboard: `cce-shadow run wl-paste -n`.
  `ccectl key-down 29` + `keypress` delivers the Ctrl chords to this app.
- `/usr/share/cups/data/form_english.pdf` is a good fill-in subject: text,
  17 text fields (`/NeedAppearances true`). Copy it before editing. After a
  save, check what reached the file: `cmp -n <orig size>` against the
  original, `qpdf --check`, `qpdf --qdf --object-streams=disable f.pdf -`
  for the dictionaries, and `pdftoppm` for poppler's drawing.
- A window close quits at once (cce-ui has no close-request hook), so
  unsaved changes are only guarded on `q`.
- Do not click Print… in a shadow either: the portal's dialog opens on the
  live display. Printing has not been driven end to end by a test.
- Do not click Insert… or Extract… in a shadow: they open the portal file
  chooser, which appears on the LIVE display (the shadow shares the
  session bus). The engine test covers what they do.
