//! The fallback backend: poppler's command-line tools, view-only.
//!
//! Used only when PDFium cannot be loaded (see `doc::Backend::start`), so
//! the app always opens a PDF. `pdfinfo` reports the page sizes, and each
//! page is a `pdftoppm` run writing a PNG to stdout, on two worker threads.

use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};

use crate::doc::{Job, PageSize};
use crate::Message;

const RENDER_THREADS: usize = 2;

/// Page count from `pdfinfo`, then per-page sizes from a second ranged
/// call. `pdfinfo` reports MediaBox dimensions with a separate `rot`
/// field, while `pdftoppm` bakes /Rotate into its output — so swap
/// width/height here for 90°/270° pages to keep layout and pixels agreed.
pub fn page_sizes(path: &Path) -> Result<Vec<PageSize>, String> {
    let count_out = pdfinfo(path, &[])?;
    let count: usize = count_out
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or("pdfinfo: no page count")?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let sizes_out = pdfinfo(path, &["-f", "1", "-l", &count.to_string()])?;
    let mut sizes: Vec<PageSize> = Vec::with_capacity(count);
    let mut rots: Vec<i32> = Vec::with_capacity(count);
    for line in sizes_out.lines() {
        let Some(rest) = line.strip_prefix("Page ") else { continue };
        let Some((_, field)) = rest.trim_start().split_once(' ') else { continue };
        if let Some(v) = field.trim_start().strip_prefix("size:") {
            // "595.276 x 841.89 pts (A4)"
            let mut it = v.trim().split_whitespace();
            let w: f64 = it.next().and_then(|s| s.parse().ok()).ok_or("pdfinfo: bad size")?;
            let h: f64 = it.nth(1).and_then(|s| s.parse().ok()).ok_or("pdfinfo: bad size")?;
            sizes.push(PageSize { w, h });
        } else if let Some(v) = field.trim_start().strip_prefix("rot:") {
            rots.push(v.trim().parse().unwrap_or(0));
        }
    }
    if sizes.len() != count {
        return Err(format!("pdfinfo: {} sizes for {count} pages", sizes.len()));
    }
    for (s, rot) in sizes.iter_mut().zip(rots) {
        if rot == 90 || rot == 270 {
            std::mem::swap(&mut s.w, &mut s.h);
        }
    }
    Ok(sizes)
}

fn pdfinfo(path: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("pdfinfo")
        .args(args)
        .arg(path)
        .output()
        .map_err(|e| format!("pdfinfo: {e}"))?;
    if !out.status.success() {
        return Err(format!("pdfinfo: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub struct Renderer {
    queue: mpsc::Sender<Job>,
}

impl Renderer {
    pub fn new(notify: calloop::channel::Sender<Message>) -> Self {
        let (queue, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..RENDER_THREADS {
            let rx = Arc::clone(&rx);
            let notify = notify.clone();
            std::thread::spawn(move || worker(rx, notify));
        }
        Self { queue }
    }

    pub fn render(&self, job: Job) {
        let _ = self.queue.send(job);
    }
}

fn worker(rx: Arc<Mutex<mpsc::Receiver<Job>>>, notify: calloop::channel::Sender<Message>) {
    loop {
        let job = match rx.lock().unwrap().recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        let result = render(&job)
            .map(|rgba| job.finish(rgba))
            .map_err(|e| log::warn!("{}: page {}: {e}", job.path.display(), job.page + 1))
            .ok();
        let msg = Message::Page { generation: job.generation, page: job.page, result };
        if notify.send(msg).is_err() {
            return;
        }
    }
}

fn render(job: &Job) -> Result<image::RgbaImage, String> {
    let page = (job.page + 1).to_string();
    // No output root: poppler's pdftoppm writes the PNG to stdout.
    let out = Command::new("pdftoppm")
        .args(["-png", "-r", &job.capped_dpi().to_string(), "-f", &page, "-l", &page])
        .arg(&job.path)
        .output()
        .map_err(|e| format!("pdftoppm: {e}"))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!("pdftoppm: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(image::load_from_memory_with_format(&out.stdout, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?
        .to_rgba8())
}
