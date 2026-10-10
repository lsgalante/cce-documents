//! A typeset document written as PDF with krilla.
//!
//! Every glyph goes down where the layout put it: each run starts at its
//! first glyph's x, and each glyph advances to the next one's start, so
//! justified spacing survives exactly. Fonts are the very faces cosmic-text
//! shaped with (by fontdb id, fallbacks included), embedded as subsets by
//! krilla. Each glyph carries the bytes of text it was shaped from, so the
//! PDF's text can be searched, selected and copied.

use std::collections::HashMap;

use cce_ui::cosmic_text::{fontdb, FontSystem};
use krilla::action::{Action, LinkAction};
use krilla::annotation::{Annotation, LinkAnnotation, Target};
use krilla::color::rgb;
use krilla::destination::XyzDestination;
use krilla::geom::{PathBuilder, Point, Rect, Size, Transform};
use krilla::image::Image;
use krilla::metadata::Metadata;
use krilla::outline::{Outline, OutlineNode};
use krilla::page::PageSettings;
use krilla::paint::Fill;
use krilla::text::{Font, GlyphId, KrillaGlyph};
use krilla::{Data, Document};

use super::layout::{Item, Laid, OutlineEntry};

fn fill(color: [u8; 3]) -> Fill {
    Fill { paint: rgb::Color::new(color[0], color[1], color[2]).into(), ..Default::default() }
}

/// The krilla font for a face, read once from fontdb.
fn font(fs: &FontSystem, cache: &mut HashMap<fontdb::ID, Option<Font>>, id: fontdb::ID) -> Option<Font> {
    cache
        .entry(id)
        .or_insert_with(|| fs.db().with_face_data(id, |data, index| Font::new(Data::from(data.to_vec()), index)).flatten())
        .clone()
}

/// Nest flat outline entries by level: each entry under the nearest
/// earlier one of a lower level.
fn outline(entries: &[OutlineEntry]) -> Outline {
    fn take(entries: &[OutlineEntry], i: &mut usize, level: u8) -> Vec<OutlineNode> {
        let mut nodes = Vec::new();
        while let Some(e) = entries.get(*i) {
            if e.level < level {
                break;
            }
            *i += 1;
            let mut node = OutlineNode::new(e.title.clone(), XyzDestination::new(e.page, Point::from_xy(0.0, e.y)));
            for child in take(entries, i, e.level + 1) {
                node.push_child(child);
            }
            nodes.push(node);
        }
        nodes
    }
    let mut out = Outline::new();
    let mut i = 0;
    let top = entries.iter().map(|e| e.level).min().unwrap_or(1);
    for node in take(entries, &mut i, top) {
        out.push_child(node);
    }
    out
}

pub fn write(fs: &FontSystem, laid: &Laid, title: &str, author: &str) -> Result<Vec<u8>, String> {
    let mut doc = Document::new();
    let mut fonts = HashMap::new();
    let size = Size::from_wh(laid.size.0, laid.size.1).ok_or("bad page size")?;
    for page in &laid.pages {
        let mut p = doc.start_page_with(PageSettings::new(size));
        let mut links = Vec::new();
        {
            let mut surface = p.surface();
            for item in &page.items {
                match item {
                    Item::Rect { x, y, w, h, color } => {
                        let mut pb = PathBuilder::new();
                        if let Some(r) = Rect::from_xywh(*x, *y, *w, *h) {
                            pb.push_rect(r);
                        }
                        if let Some(path) = pb.finish() {
                            surface.set_fill(Some(fill(*color)));
                            surface.draw_path(&path);
                        }
                    }
                    Item::Glyphs(run) => {
                        let Some(f) = font(fs, &mut fonts, run.font) else {
                            log::warn!("no font data for {:?}", run.font);
                            continue;
                        };
                        let Some(first) = run.glyphs.first() else { continue };
                        let em = run.size.max(0.01);
                        let glyphs: Vec<KrillaGlyph> = run
                            .glyphs
                            .iter()
                            .map(|g| KrillaGlyph::new(GlyphId::new(g.id as u32), g.advance / em, g.x_offset, g.y_offset, 0.0, g.range.clone(), None))
                            .collect();
                        surface.set_fill(Some(fill(run.color)));
                        surface.draw_glyphs(Point::from_xy(first.x, run.baseline), &glyphs, f, &run.text, run.size, false);
                    }
                    Item::Image { data, jpeg, x, y, w, h } => {
                        let d = Data::from(data.as_ref().clone());
                        let image = if *jpeg { Image::from_jpeg(d, true) } else { Image::from_png(d, true) };
                        if let (Ok(image), Some(s)) = (image, Size::from_wh(*w, *h)) {
                            surface.push_transform(&Transform::from_translate(*x, *y));
                            surface.draw_image(image, s);
                            surface.pop();
                        }
                    }
                    Item::Link { x, y, w, h, url } => links.push((*x, *y, *w, *h, Ok(url.clone()))),
                    Item::GoTo { x, y, w, h, page, top } => links.push((*x, *y, *w, *h, Err((*page, *top)))),
                }
            }
            surface.finish();
        }
        for (x, y, w, h, to) in links {
            let Some(r) = Rect::from_xywh(x, y, w, h) else { continue };
            match to {
                Ok(url) => {
                    let target = Target::Action(Action::Link(LinkAction::new(url.clone())));
                    p.add_annotation(Annotation::new_link(LinkAnnotation::new(r, target), Some(url)));
                }
                Err((page, top)) => {
                    let target = Target::Destination(XyzDestination::new(page, Point::from_xy(0.0, top)).into());
                    p.add_annotation(Annotation::new_link(LinkAnnotation::new(r, target), None));
                }
            }
        }
        p.finish();
    }
    if !laid.outline.is_empty() {
        doc.set_outline(outline(&laid.outline));
    }
    let mut meta = Metadata::new().title(title.to_string());
    if !author.is_empty() {
        meta = meta.authors(vec![author.to_string()]);
    }
    doc.set_metadata(meta);
    doc.finish().map_err(|e| format!("PDF: {e:?}"))
}
