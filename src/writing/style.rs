//! How a document looks: page, margins, faces and spacing.
//!
//! A style is a KDL file named after it in `~/.config/cce/documents/styles/`
//! (`manuscript.kdl` for `style: manuscript`); the built-in `manuscript`
//! below is the fallback and the example. A document picks its style and
//! may override single keys in its front matter.
//!
//! Lengths on the page (size, margins) are millimetres unless they carry a
//! unit (`mm`, `cm`, `in`, `pt`); type sizes and spacing are points, as
//! type is measured. Everything is held in points.

use std::path::PathBuf;

use kdl::{KdlDocument, KdlNode, KdlValue};

pub const MANUSCRIPT: &str = r#"// The built-in style. Copy it to ~/.config/cce/documents/styles/<name>.kdl
// and change what you like; a document picks it with `style: <name>`.
page size="A4" margin=25
body font="Noto Serif" size=11 leading=1.45 space-after=7
heading level=1 font="Noto Serif" size=22 bold=#true space-before=20 space-after=10
heading level=2 font="Noto Serif" size=16 bold=#true space-before=16 space-after=8
heading level=3 font="Noto Serif" size=12.5 bold=#true space-before=12 space-after=6
mono font="Noto Sans Mono" size=9.5
align "left"
page-numbers #true
"#;

const MM: f64 = 72.0 / 25.4;

#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub family: String,
    pub size: f64,
    pub bold: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeadingStyle {
    pub face: Face,
    pub space_before: f64,
    pub space_after: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Justify,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// Page width and height, in points.
    pub page: (f64, f64),
    /// Top, right, bottom, left, in points.
    pub margins: [f64; 4],
    pub body: Face,
    /// Line height as a multiple of the type size.
    pub leading: f64,
    /// Space after a paragraph, in points.
    pub space_after: f64,
    pub headings: [HeadingStyle; 3],
    pub mono: Face,
    pub align: Align,
    pub page_numbers: bool,
}

/// A named paper size in points.
fn paper(name: &str) -> Option<(f64, f64)> {
    let mm = |w: f64, h: f64| Some((w * MM, h * MM));
    match name.to_ascii_lowercase().as_str() {
        "a4" => mm(210.0, 297.0),
        "a5" => mm(148.0, 210.0),
        "a3" => mm(297.0, 420.0),
        "b5" => mm(176.0, 250.0),
        "letter" => Some((612.0, 792.0)),
        "legal" => Some((612.0, 1008.0)),
        _ => None,
    }
}

/// A length in points: a number in `default_unit_pt` units, or a string
/// with a unit.
pub fn length(s: &str, default_unit_pt: f64) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.trim().parse().ok()?;
    let k = match unit.trim() {
        "" => default_unit_pt,
        "mm" => MM,
        "cm" => 10.0 * MM,
        "in" => 72.0,
        "pt" => 1.0,
        _ => return None,
    };
    Some(n * k)
}

fn value_len(v: &KdlValue, default_unit_pt: f64) -> Option<f64> {
    match v {
        KdlValue::Integer(i) => Some(*i as f64 * default_unit_pt),
        KdlValue::Float(f) => Some(f * default_unit_pt),
        KdlValue::String(s) => length(s, default_unit_pt),
        _ => None,
    }
}

fn get<'a>(node: &'a KdlNode, key: &str) -> Option<&'a KdlValue> {
    node.entry(key).map(|e| e.value())
}

fn num(node: &KdlNode, key: &str) -> Option<f64> {
    get(node, key).and_then(|v| value_len(v, 1.0))
}

impl Style {
    pub fn manuscript() -> Style {
        let mut s = Style {
            page: paper("a4").unwrap(),
            margins: [25.0 * MM; 4],
            body: Face { family: "serif".into(), size: 11.0, bold: false },
            leading: 1.4,
            space_after: 6.0,
            headings: std::array::from_fn(|_| HeadingStyle { face: Face { family: "serif".into(), size: 14.0, bold: true }, space_before: 12.0, space_after: 6.0 }),
            mono: Face { family: "monospace".into(), size: 9.5, bold: false },
            align: Align::Left,
            page_numbers: true,
        };
        s.apply_kdl(MANUSCRIPT).expect("the built-in style parses");
        s
    }

    /// Change what a KDL style names; the rest stays.
    pub fn apply_kdl(&mut self, text: &str) -> Result<(), String> {
        let doc: KdlDocument = text.parse().map_err(|e: kdl::KdlError| e.to_string())?;
        for node in doc.nodes() {
            match node.name().value() {
                "page" => {
                    if let Some(size) = get(node, "size").and_then(|v| v.as_string()).and_then(paper) {
                        self.page = size;
                    }
                    if let Some(w) = get(node, "width").and_then(|v| value_len(v, MM)) {
                        self.page.0 = w;
                    }
                    if let Some(h) = get(node, "height").and_then(|v| value_len(v, MM)) {
                        self.page.1 = h;
                    }
                    if get(node, "orientation").and_then(|v| v.as_string()) == Some("landscape") && self.page.0 < self.page.1 {
                        self.page = (self.page.1, self.page.0);
                    }
                    if let Some(m) = get(node, "margin").and_then(|v| value_len(v, MM)) {
                        self.margins = [m; 4];
                    }
                    for (i, key) in ["margin-top", "margin-right", "margin-bottom", "margin-left"].iter().enumerate() {
                        if let Some(m) = get(node, key).and_then(|v| value_len(v, MM)) {
                            self.margins[i] = m;
                        }
                    }
                }
                "body" => {
                    apply_face(&mut self.body, node);
                    if let Some(l) = num(node, "leading") {
                        self.leading = l;
                    }
                    if let Some(a) = num(node, "space-after") {
                        self.space_after = a;
                    }
                }
                "heading" => {
                    let level = get(node, "level").and_then(|v| v.as_integer()).unwrap_or(1).clamp(1, 3) as usize;
                    let h = &mut self.headings[level - 1];
                    apply_face(&mut h.face, node);
                    if let Some(b) = num(node, "space-before") {
                        h.space_before = b;
                    }
                    if let Some(a) = num(node, "space-after") {
                        h.space_after = a;
                    }
                }
                "mono" => apply_face(&mut self.mono, node),
                "align" => {
                    self.align = match node.entry(0).and_then(|e| e.value().as_string()) {
                        Some("justify") => Align::Justify,
                        _ => Align::Left,
                    }
                }
                "page-numbers" => self.page_numbers = node.entry(0).and_then(|e| e.value().as_bool()).unwrap_or(true),
                other => log::warn!("style: unknown node `{other}`"),
            }
        }
        Ok(())
    }

    /// The style a document asks for: its `style:` (a user style file, or
    /// the built-in), then its other front-matter keys on top.
    pub fn for_document(front: &[(String, String)]) -> Style {
        let mut style = Style::manuscript();
        if let Some((_, name)) = front.iter().find(|(k, _)| k == "style") {
            match std::fs::read_to_string(style_path(name)) {
                Ok(text) => {
                    if let Err(e) = style.apply_kdl(&text) {
                        log::warn!("style {name}: {e}");
                    }
                }
                Err(_) if name == "manuscript" => {}
                Err(e) => log::warn!("style {name}: {e}"),
            }
        }
        for (k, v) in front {
            match k.as_str() {
                "page" => {
                    if let Some(p) = paper(v) {
                        self::orient(&mut style, p);
                    }
                }
                "margins" | "margin" => {
                    if let Some(m) = length(v, MM) {
                        style.margins = [m; 4];
                    }
                }
                "font" => {
                    style.body.family = v.clone();
                    for h in &mut style.headings {
                        h.face.family = v.clone();
                    }
                }
                "size" => {
                    if let Some(s) = length(v, 1.0) {
                        style.body.size = s;
                    }
                }
                "align" => style.align = if v == "justify" { Align::Justify } else { Align::Left },
                "page-numbers" => style.page_numbers = v != "false" && v != "no",
                _ => {}
            }
        }
        style
    }

    /// The text block: left, top, width, height, in points.
    pub fn frame(&self) -> (f64, f64, f64, f64) {
        let [t, r, b, l] = self.margins;
        (l, t, (self.page.0 - l - r).max(36.0), (self.page.1 - t - b).max(36.0))
    }
}

fn orient(style: &mut Style, p: (f64, f64)) {
    // Keep the style's orientation.
    style.page = if style.page.0 > style.page.1 { (p.1, p.0) } else { p };
}

fn apply_face(face: &mut Face, node: &KdlNode) {
    if let Some(f) = get(node, "font").and_then(|v| v.as_string()) {
        face.family = f.to_string();
    }
    if let Some(s) = num(node, "size") {
        face.size = s;
    }
    if let Some(b) = get(node, "bold").and_then(|v| v.as_bool()) {
        face.bold = b;
    }
}

pub fn style_path(name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cce/documents/styles").join(format!("{name}.kdl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_take_units() {
        assert_eq!(length("72pt", MM), Some(72.0));
        assert_eq!(length("1in", MM), Some(72.0));
        assert!((length("25.4", MM).unwrap() - 72.0).abs() < 1e-9);
        assert!((length("2.54cm", 1.0).unwrap() - 72.0).abs() < 1e-9);
        assert_eq!(length("3 parsecs", 1.0), None);
    }

    #[test]
    fn the_built_in_style_reads() {
        let s = Style::manuscript();
        assert!((s.page.0 - 595.3).abs() < 0.1 && (s.page.1 - 841.9).abs() < 0.1);
        assert_eq!(s.body.family, "Noto Serif");
        assert_eq!(s.headings[0].face.size, 22.0);
        assert!(s.headings[1].face.bold);
        assert!(s.page_numbers);
    }

    #[test]
    fn front_matter_overrides() {
        let front = vec![("page".to_string(), "Letter".to_string()), ("margins".to_string(), "1in".to_string()), ("align".to_string(), "justify".to_string())];
        let s = Style::for_document(&front);
        assert_eq!(s.page, (612.0, 792.0));
        assert_eq!(s.margins, [72.0; 4]);
        assert_eq!(s.align, Align::Justify);
    }

    #[test]
    fn a_style_file_changes_only_what_it_names() {
        let mut s = Style::manuscript();
        s.apply_kdl("page size=\"A5\" orientation=\"landscape\"\nbody size=12\nheading level=2 size=18\n").unwrap();
        assert!(s.page.0 > s.page.1);
        assert_eq!(s.body.size, 12.0);
        assert_eq!(s.body.family, "Noto Serif");
        assert_eq!(s.headings[1].face.size, 18.0);
        assert_eq!(s.headings[0].face.size, 22.0);
    }
}
