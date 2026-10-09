# cce-documents

The document app: a PDF viewer today, growing into a PDF editor and a word
processor in one window. Read the workspace guide
(`../cce-compositor/WORKSPACE.md`) first; this file covers only what is
particular to this crate. The plan — the two engines, the library and
format choices, six milestones — is the design doc "cce-documents: PDF
editor and word processor — proposal"
(claude.ai/code/artifact/50527ccf-8449-496e-94ba-7ed26dc5a2f6). Milestone 1
is here: PDFium rendering, text selection and copy, and search. Split from
cce-preview on 2026-10-09; pictures went to cce-image.

## Shape

| File | What it owns |
| --- | --- |
| `main.rs` | The `Application`: page layout and the viewport (scroll, zoom at the pointer, rotation), selection by drag, the find bar, keys |
| `doc.rs` | `Backend` (PDFium or poppler), `Document`, and the GPU `PageStore` (lazy renders, DPI upgrades, LRU, generations) |
| `engine.rs` | PDFium on its own thread: open, render, page text, search; `Frame` maps PDF page space to display points |
| `text.rs` | Pure selection geometry: a page's chars with boxes, caret hit-testing, highlight rects, copied text. Unit-tested |
| `poppler.rs` | The fallback: `pdfinfo` sizes, `pdftoppm` renders, view-only |

## PDFium

- **It is a native library outside cargo.** `scripts/fetch-pdfium` installs
  a pinned build (bblanchon/pdfium-binaries, chromium/8086) to
  `~/.local/lib/libpdfium.so`. The app looks at `$CCE_PDFIUM`, beside its
  binary, `~/.local/lib`, then the system path. Without it the app still
  opens PDFs through poppler, with no text or search (a warning is logged,
  and the find bar says so).
- **One thread owns it.** PDFium is not thread-safe and a `PdfDocument`
  borrows the `Pdfium` that loaded it, so both live on the `pdfium` thread;
  the app sends requests. Opening and `text_now` (a copy reaching pages
  never shown) wait for their answer; renders, text and search come back as
  `Message`s. Search runs one page per loop turn and yields to queued
  requests, so it never holds up a render.
- **`pdfium-render` is pinned to API `pdfium_7881`**, the newest it knows;
  the fetched library is newer, which PDFium's C API allows. Bump both
  deliberately.

## Coordinates (the thing to get right)

Three spaces, and each value says which it is in:

1. **PDF page space** — points, origin bottom-left of the media box, before
   the page's /Rotate. Only `engine.rs` sees it.
2. **Display points** — the page as PDFium renders it: /Rotate applied,
   origin top-left of the bounding box (crop ∩ media), y down. `PageSize`,
   `PageText` and search hits are all in this space. `Frame` converts.
3. **Screen px** — after the user's quarter turns (`turn` / `unturn` in
   `main.rs`), zoom and scroll.

`engine::tests::a_rotated_page_maps_its_text_where_it_is_drawn` checks (1)→(2)
on a page and a qpdf-rotated copy of it; run it after touching `Frame`.

## Verifying

- `cargo test -p cce-documents` — the text geometry, and the PDFium
  mapping (skips without PDFium, qpdf or the CUPS sample PDF).
- **A shadow does not find PDFium on its own**: the shadow's HOME hides
  `~/.local/lib`. Spawn with `CCE_PDFIUM=$HOME/.local/lib/libpdfium.so`
  (absolute, expanded before the spawn), or you are testing the poppler
  fallback. A test document with a rotated page:
  `qpdf --rotate=+90:1 in.pdf turned.pdf` then
  `qpdf --empty --pages in.pdf turned.pdf -- multi.pdf`.
- Ctrl+C lands in the shadow's own clipboard: `cce-shadow run wl-paste -n`.
  `ccectl key-down 29` + `keypress` delivers the Ctrl chords to this app.
