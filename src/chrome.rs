//! The app's own controls, drawn into its display list: the markup toolbar
//! and one-line text fields (the find bar, a form field being filled, a
//! note being written).
//!
//! Text is measured with `shaped_cluster_offsets`, the renderer's own
//! shaping, so carets and clicks land on the glyphs drawn.

use cce_ui::cosmic_text::FontSystem;
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::PaintCtx;
use cce_ui::widget::LineEdit;

pub const TOOLBAR_H: f32 = 32.0;
const TOOLBAR_FONT: f32 = 13.0;
const BUTTON_PAD: f32 = 12.0;
const SEPARATOR: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Open or close the page sidebar.
    Pages,
    Select,
    Draw,
    Note,
    Highlight,
    Underline,
    Strike,
    Undo,
    Save,
    // The page sidebar's own buttons.
    Insert,
    Extract,
    RotatePages,
    DeletePages,
}

pub struct Button {
    pub label: &'static str,
    pub action: Action,
    /// The current tool, or Save while there are unsaved changes.
    pub on: bool,
    pub enabled: bool,
    /// Starts a new group: a gap before it.
    pub group: bool,
}

pub fn width_of(fs: &mut FontSystem, text: &str, size: f32) -> f32 {
    cce_ui::engine::shaped_cluster_offsets(fs, text, size, None).last().map_or(0.0, |&(_, x)| x)
}

/// Each button's rect: a bar centred along the bottom of `area`, the root
/// plate's inset up from its edge. When the bar is wider than `area`, it
/// wraps between groups onto rows above, each centred.
pub fn toolbar_layout(fs: &mut FontSystem, buttons: &[Button], area: Rect) -> Vec<Rect> {
    let inset = cce_ui::layout::root_plate_inset();
    let widths: Vec<f32> = buttons.iter().map(|b| width_of(fs, b.label, TOOLBAR_FONT) + 2.0 * BUTTON_PAD).collect();
    // Groups: runs of buttons, each starting where `group` is set.
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, b) in buttons.iter().enumerate() {
        match groups.last_mut() {
            Some(g) if !b.group => g.end = i + 1,
            _ => groups.push(i..i + 1),
        }
    }
    let avail = (area.width - 2.0 * inset).max(0.0);
    let group_w = |g: &std::ops::Range<usize>| widths[g.clone()].iter().sum::<f32>();
    let mut rows: Vec<Vec<std::ops::Range<usize>>> = vec![Vec::new()];
    let mut row_w = 0.0;
    for g in groups {
        let w = group_w(&g);
        let row = rows.last_mut().expect("never empty");
        if !row.is_empty() && row_w + SEPARATOR + w > avail {
            rows.push(vec![g]);
            row_w = w;
        } else {
            row_w += if row.is_empty() { w } else { SEPARATOR + w };
            row.push(g);
        }
    }
    let mut rects = vec![Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 }; buttons.len()];
    let bottom = area.y + area.height - inset - TOOLBAR_H;
    let n = rows.len();
    for (r, row) in rows.iter().enumerate() {
        let total = row.iter().map(&group_w).sum::<f32>() + SEPARATOR * row.len().saturating_sub(1) as f32;
        let y = bottom - (n - 1 - r) as f32 * (TOOLBAR_H + 12.0);
        let mut x = area.x + ((area.width - total) / 2.0).max(0.0);
        for (k, g) in row.iter().enumerate() {
            if k > 0 {
                x += SEPARATOR;
            }
            for i in g.clone() {
                rects[i] = Rect { x, y, width: widths[i], height: TOOLBAR_H };
                x += widths[i];
            }
        }
    }
    rects
}

/// The bar behind each row of the toolbar, then its buttons.
pub fn paint_toolbar(pc: &mut PaintCtx, buttons: &[Button], rects: &[Rect]) {
    let mut rows: Vec<(f32, f32, f32)> = Vec::new(); // y, left, right
    for r in rects {
        match rows.iter_mut().find(|row| row.0 == r.y) {
            Some(row) => {
                row.1 = row.1.min(r.x);
                row.2 = row.2.max(r.x + r.width);
            }
            None => rows.push((r.y, r.x, r.x + r.width)),
        }
    }
    for (y, l, rr) in rows {
        let bar = Rect { x: l - 4.0, y: y - 4.0, width: rr - l + 8.0, height: TOOLBAR_H + 8.0 };
        pc.rounded_rect(bar, 8.0, (true, true, true, true), [0.0, 0.0, 0.0, 0.62]);
    }
    paint_buttons(pc, buttons, rects, TOOLBAR_FONT, false);
}

/// Buttons alone, without the bar (or each on a faint plate of its own,
/// `framed`): labels at the padding, the `on` one lit, disabled ones
/// dimmed.
pub fn paint_buttons(pc: &mut PaintCtx, buttons: &[Button], rects: &[Rect], size: f32, framed: bool) {
    for (b, r) in buttons.iter().zip(rects) {
        if framed {
            pc.rounded_rect(*r, 6.0, (true, true, true, true), [1.0, 1.0, 1.0, if b.enabled { 0.10 } else { 0.04 }]);
        }
        if b.on {
            pc.rounded_rect(*r, 6.0, (true, true, true, true), [1.0, 1.0, 1.0, 0.18]);
        }
        let ty = cce_ui::layout::align_text_y(r.y, r.height, size, 0.0);
        let color = if b.enabled { [235, 235, 235] } else { [120, 120, 120] };
        pc.text(b.label.to_string(), r.x + BUTTON_PAD, ty, size, color);
    }
}

/// The button under a point.
pub fn button_at(buttons: &[Button], rects: &[Rect], x: f32, y: f32) -> Option<Action> {
    buttons
        .iter()
        .zip(rects)
        .find(|(_, r)| x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height)
        .and_then(|(b, _)| b.enabled.then_some(b.action))
}

/// The x of a byte boundary in `text`, as drawn at `size`.
fn x_of(fs: &mut FontSystem, text: &str, size: f32, byte: usize) -> f32 {
    cce_ui::engine::shaped_cluster_offsets(fs, text, size, None)
        .iter()
        .rev()
        .find(|&&(b, _)| b <= byte)
        .map_or(0.0, |&(_, x)| x)
}

/// The byte boundary in what `edit` shows nearest to `x` (from the text's
/// left edge), as a `LineEdit` index.
pub fn offset_at(fs: &mut FontSystem, edit: &LineEdit, size: f32, x: f32) -> usize {
    let shown = edit.display();
    let at = cce_ui::engine::shaped_cluster_offsets(fs, &shown, size, None)
        .into_iter()
        .min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
        .map_or(0, |(b, _)| b);
    edit.text_index(at)
}

pub struct FieldLook {
    pub size: f32,
    pub text: [u8; 3],
    pub placeholder: [u8; 3],
    pub caret: [f32; 4],
    pub selection: [f32; 4],
}

/// Draw a field's text with its selection and caret inside `field`. When
/// focused, reports the caret to the input method (which is also what tells
/// the shell text input is wanted).
pub fn paint_field(pc: &mut PaintCtx, fs: &mut FontSystem, edit: &mut LineEdit, field: Rect, look: &FieldLook, placeholder: &str, focused: bool) {
    if focused {
        edit.sync_ime();
    }
    let shown = edit.display();
    let caret = x_of(fs, &shown, look.size, edit.display_index(edit.cursor));
    let sel = edit
        .selection
        .filter(|&(a, b)| a < b)
        .map(|(a, b)| (x_of(fs, &shown, look.size, edit.display_index(a)), x_of(fs, &shown, look.size, edit.display_index(b))));
    let ty = cce_ui::layout::align_text_y(field.y, field.height, look.size, 0.0);
    let inset = (field.height * 0.18).min(6.0);
    pc.clip(field, |pc| {
        if let Some((a, b)) = sel {
            pc.quad(Rect { x: field.x + a, y: field.y + inset, width: b - a, height: field.height - 2.0 * inset }, look.selection);
        }
        if shown.is_empty() {
            pc.text(placeholder.to_string(), field.x, ty, look.size, look.placeholder);
        } else {
            pc.text(shown.clone(), field.x, ty, look.size, look.text);
        }
        if focused {
            pc.quad(Rect { x: field.x + caret, y: field.y + inset, width: 1.0, height: field.height - 2.0 * inset }, look.caret);
        }
    });
    if focused {
        cce_ui::ime::report_caret(field.x + caret, field.y + inset, 1.0, field.height - 2.0 * inset);
    }
}
