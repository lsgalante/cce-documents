//! A page of the layout drawn into pixels, for the page view while editing.
//!
//! The same items the PDF is written from, at the same positions: glyphs
//! rasterized by swash (cosmic-text's) at the view's DPI, rectangles,
//! pictures. Only the edited page is drawn again on a keystroke
//! (`edit::Changes`), on the `Painter` thread, so typing never waits on it.

use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};

use cce_ui::cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use image::RgbaImage;

use super::edit::Doc;
use super::layout::Item;
use crate::doc::Job;
use crate::Message;

/// Glyph images and decoded pictures, kept between pages.
#[derive(Default)]
pub struct Raster {
    swash: Option<SwashCache>,
    pictures: HashMap<usize, Arc<RgbaImage>>,
}

/// Glyph coverage, corrected: blending raw coverage in sRGB draws dark text
/// on white thin and grey; raising it to 1/1.4 gives strokes the weight
/// PDFium and the rest of the desktop draw them with.
fn coverage_lut() -> &'static [u8; 256] {
    static LUT: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|i| ((i as f32 / 255.0).powf(1.0 / 1.4) * 255.0).round() as u8))
}

fn blend(img: &mut RgbaImage, x: i32, y: i32, color: [u8; 3], alpha: u8) {
    if x < 0 || y < 0 || x as u32 >= img.width() || y as u32 >= img.height() || alpha == 0 {
        return;
    }
    let p = img.get_pixel_mut(x as u32, y as u32);
    let a = alpha as u32;
    for c in 0..3 {
        p[c] = ((color[c] as u32 * a + p[c] as u32 * (255 - a)) / 255) as u8;
    }
}

impl Raster {
    /// Page `page` of `doc` at `dpi`.
    pub fn page(&mut self, fs: &mut FontSystem, doc: &Doc, page: usize, dpi: u32) -> RgbaImage {
        let s = dpi as f32 / 72.0;
        let (pw, ph) = doc.size();
        let (w, h) = (((pw * s).round() as u32).max(1), ((ph * s).round() as u32).max(1));
        let mut img = RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]));
        let (fx, fy, ..) = doc.frame;
        let mut items = doc.page_items(page, fx, fy);
        items.extend(doc.extras(fs, page));
        let swash = self.swash.get_or_insert_with(SwashCache::new);
        for item in &items {
            match item {
                Item::Rect { x, y, w: rw, h: rh, color } => {
                    let (x0, y0) = ((x * s).round() as i32, (y * s).round() as i32);
                    let (x1, y1) = (((x + rw) * s).round().max(x0 as f32 + 1.0) as i32, ((y + rh) * s).round().max(y0 as f32 + 1.0) as i32);
                    for yy in y0..y1 {
                        for xx in x0..x1 {
                            blend(&mut img, xx, yy, *color, 255);
                        }
                    }
                }
                Item::Glyphs(run) => {
                    let size = run.size * s;
                    for g in &run.glyphs {
                        let gx = (g.x + g.x_offset * run.size) * s;
                        // Baselines on whole pixels: a line set between two
                        // rows of pixels blurs. (Screen only; the PDF keeps
                        // the exact positions.)
                        let gy = ((run.baseline - g.y_offset * run.size) * s).round();
                        let (key, ix, iy) = CacheKey::new(run.font, g.id, size, (gx, gy), g.flags);
                        let Some(image) = swash.get_image(fs, key) else { continue };
                        let pl = image.placement;
                        let (ox, oy) = (ix + pl.left, iy - pl.top);
                        match image.content {
                            SwashContent::Mask => {
                                for yy in 0..pl.height as i32 {
                                    for xx in 0..pl.width as i32 {
                                        let a = image.data[(yy as u32 * pl.width + xx as u32) as usize];
                                        blend(&mut img, ox + xx, oy + yy, run.color, coverage_lut()[a as usize]);
                                    }
                                }
                            }
                            SwashContent::Color | SwashContent::SubpixelMask => {
                                for yy in 0..pl.height as i32 {
                                    for xx in 0..pl.width as i32 {
                                        let i = ((yy as u32 * pl.width + xx as u32) * 4) as usize;
                                        let px = &image.data[i..i + 4];
                                        let c = if image.content == SwashContent::Color { [px[0], px[1], px[2]] } else { run.color };
                                        blend(&mut img, ox + xx, oy + yy, c, px[3]);
                                    }
                                }
                            }
                        }
                    }
                }
                Item::Image { data, x, y, w: iw, h: ih, .. } => {
                    let key = Arc::as_ptr(data) as usize;
                    let picture = match self.pictures.get(&key) {
                        Some(p) => Arc::clone(p),
                        None => match image::load_from_memory(data) {
                            Ok(i) => {
                                let p = Arc::new(i.to_rgba8());
                                self.pictures.insert(key, Arc::clone(&p));
                                p
                            }
                            Err(_) => continue,
                        },
                    };
                    let (tw, th) = (((iw * s).round() as u32).max(1), ((ih * s).round() as u32).max(1));
                    let scaled = image::imageops::resize(picture.as_ref(), tw, th, image::imageops::FilterType::Triangle);
                    let (x0, y0) = ((x * s).round() as i32, (y * s).round() as i32);
                    for (xx, yy, px) in scaled.enumerate_pixels() {
                        blend(&mut img, x0 + xx as i32, y0 + yy as i32, [px[0], px[1], px[2]], px[3]);
                    }
                }
                Item::Link { .. } | Item::GoTo { .. } => {}
            }
        }
        img
    }
}

/// The thread that draws pages: each job with the layout current when it
/// was asked, sharing the editor's font system (glyph ids are its own).
pub struct Painter {
    tx: mpsc::Sender<(Arc<Doc>, Job)>,
    /// The layout jobs are drawn from: the editor replaces it on each edit.
    pub doc: Arc<Mutex<Arc<Doc>>>,
}

impl Painter {
    pub fn start(fs: Arc<Mutex<FontSystem>>, doc: Arc<Doc>, notify: calloop::channel::Sender<Message>) -> Self {
        let (tx, rx) = mpsc::channel::<(Arc<Doc>, Job)>();
        std::thread::Builder::new()
            .name("painter".into())
            .spawn(move || {
                let mut raster = Raster::default();
                for (doc, job) in rx {
                    let rgba = {
                        let mut fs = fs.lock().unwrap_or_else(|e| e.into_inner());
                        raster.page(&mut fs, &doc, job.page, job.capped_dpi())
                    };
                    let result = (job.page < doc.page_count).then(|| job.finish(rgba));
                    if notify.send(Message::Page { slot: job.slot, generation: job.generation, page: job.page, result }).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn the painter");
        Self { tx, doc: Arc::new(Mutex::new(doc)) }
    }
}

impl crate::doc::Render for Painter {
    fn render(&self, job: Job) {
        let doc = Arc::clone(&self.doc.lock().unwrap_or_else(|e| e.into_inner()));
        let _ = self.tx.send((doc, job));
    }
}

#[cfg(test)]
mod tests {
    /// Render a Markdown file's pages to PNGs, to look at:
    /// `CCE_RENDER=<file.md> CCE_RENDER_OUT=<dir> cargo test -p cce-documents render_pages -- --ignored`.
    #[test]
    #[ignore]
    fn render_pages() {
        let (Some(src), Some(out)) = (std::env::var_os("CCE_RENDER"), std::env::var_os("CCE_RENDER_OUT")) else { return };
        let src = std::path::PathBuf::from(src);
        let text = std::fs::read_to_string(&src).unwrap();
        let mut fs = cce_ui::create_font_system_with_system_fonts();
        let (doc, _) = super::super::edit::Layouter::default().relayout(&mut fs, &text, src.parent().unwrap());
        let mut r = super::Raster::default();
        for p in 0..doc.page_count {
            r.page(&mut fs, &doc, p, 110).save(std::path::Path::new(&out).join(format!("page-{p}.png"))).unwrap();
        }
        let bytes = crate::writing::typeset_text(&mut fs, &text, &src).unwrap().1;
        std::fs::write(std::path::Path::new(&out).join("out.pdf"), bytes).unwrap();
        eprintln!("{} pages", doc.page_count);
    }
}
