# cce-documents

The document app: a PDF viewer today, growing into a PDF editor and a word
processor in one window. Read the workspace guide
(`../cce-compositor/WORKSPACE.md`) first; this file covers only what is
particular to this crate. The plan — the two engines, the library and
format choices, six milestones — is the design doc "cce-documents: PDF
editor and word processor — proposal"
(claude.ai/code/artifact/50527ccf-8449-496e-94ba-7ed26dc5a2f6). Milestones
1 and 2 are here: PDFium rendering, text selection and copy, search; and
highlight / underline / strike, ink, notes, form filling (text fields,
check boxes, radio buttons), undo of added marks, and incremental saves.
Split from cce-preview on 2026-10-09; pictures went to cce-image.

## Shape

| File | What it owns |
| --- | --- |
| `main.rs` | The `Application`: page layout and the viewport (scroll, zoom at the pointer, rotation), tools (select / draw / note), selection, picking marks, the field and note editors, the find bar, save, keys |
| `chrome.rs` | The markup toolbar and the one-line text fields, drawn into the display list and measured with the renderer's shaping |
| `doc.rs` | `Backend` (PDFium or poppler), `Document`, and the GPU `PageStore` (lazy renders, DPI upgrades, `invalidate` after an edit, LRU, generations) |
| `engine.rs` | PDFium on its own thread, through its C API: open, render, text, search, annotations, edits, the edit journal, save; `Frame` maps PDF page space to display points both ways |
| `markup.rs` | Pure data: `Annot` as the app sees it, `Edit`, picking under the pointer |
| `trim.rs` | Cuts PDFium's incremental update to the objects that changed (lopdf) |
| `text.rs` | Pure selection geometry: a page's chars with boxes, caret hit-testing, highlight rects, copied text |
| `poppler.rs` | The fallback: `pdfinfo` sizes, `pdftoppm` renders, view-only |

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

1. Every edit is applied to the shown document **and recorded** (`Op`, with
   the /NM it gave a new annotation, so undo-by-name replays too).
2. A save loads the file afresh (never rendered), replays the journal,
   closes and reloads each touched page — which rebuilds the page's
   annotation list and with it the missing appearance streams, without a
   render — and asks PDFium for an incremental save.
3. `trim::trim` then keeps only the objects that are new or differ from
   the original (streams compared decoded), and writes them as the update
   with an explicit `/Prev` (lopdf omits it when it did not read the offset
   itself). If lopdf cannot read a file, PDFium's untrimmed update is saved.
4. Written to a sibling temporary and renamed over the target. The journal
   empties and the saved file becomes the source of the next save, which
   appends a second update.

**Appearance streams**: PDFium writes an annotation's /AP only when a
render (or an annotation-list rebuild) first meets it without one; never on
create. Without /AP a mark is invisible in readers that do not draw their
own. The shown document gets them by a 1×1 render after each edit
(`generate_appearances`), the saved copy by the page reload above.

The test `engine::tests::pdfium_round_trips` covers all of it: marks, a
note, undo, a text field typed into, a check box whose on state is `/On`,
the original bytes kept, the update under 64 KB after a render, a second
save appending, `qpdf --check`, PDFium reading it back, `/AP` on every
mark, and poppler drawing the highlight.

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
