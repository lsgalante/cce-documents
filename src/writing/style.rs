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
footer center="{page}"
"#;

/// A book: A5, justified, paragraphs marked by a first-line indent rather
/// than space, each chapter on a new page, its title in the page head.
pub const BOOK: &str = r#"// The built-in book style.
page size="A5" margin-top=20 margin-bottom=22 margin-left=18 margin-right=18
body font="Noto Serif" size=10.5 leading=1.4 space-after=0 indent=14
heading level=1 font="Noto Serif" size=20 bold=#false space-before=60 space-after=28 page-break=#true
heading level=2 font="Noto Serif" size=13 bold=#true space-before=14 space-after=6
heading level=3 font="Noto Serif" size=11 bold=#true space-before=10 space-after=4
mono font="Noto Sans Mono" size=9
align "justify"
header center="{section}" first=#false
footer center="{page}"
"#;

/// The styles that come with the app: (name, KDL).
pub const BUILT_IN: &[(&str, &str)] = &[("manuscript", MANUSCRIPT), ("book", BOOK)];

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
    /// Starts a new page.
    pub break_before: bool,
}

/// A page's header or footer: text on the left, centre and right, with
/// fields (`layout::Fields`) filled in; and whether the first page has it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Furniture {
    pub left: String,
    pub center: String,
    pub right: String,
    pub first: bool,
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
    /// A paragraph that follows another starts this far in, in points.
    pub indent: f64,
    pub headings: [HeadingStyle; 3],
    pub mono: Face,
    pub align: Align,
    pub header: Furniture,
    pub footer: Furniture,
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
            indent: 0.0,
            headings: std::array::from_fn(|_| HeadingStyle {
                face: Face { family: "serif".into(), size: 14.0, bold: true },
                space_before: 12.0,
                space_after: 6.0,
                break_before: false,
            }),
            mono: Face { family: "monospace".into(), size: 9.5, bold: false },
            align: Align::Left,
            header: Furniture { first: true, ..Default::default() },
            footer: Furniture { first: true, ..Default::default() },
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
                    if let Some(i) = num(node, "indent") {
                        self.indent = i;
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
                    if let Some(b) = get(node, "page-break").and_then(|v| v.as_bool()) {
                        h.break_before = b;
                    }
                }
                "header" | "footer" => {
                    let f = if node.name().value() == "header" { &mut self.header } else { &mut self.footer };
                    *f = Furniture { first: true, ..Default::default() };
                    for (key, slot) in [("left", &mut f.left), ("center", &mut f.center), ("right", &mut f.right)] {
                        if let Some(t) = get(node, key).and_then(|v| v.as_string()) {
                            *slot = t.to_string();
                        }
                    }
                    if let Some(b) = get(node, "first").and_then(|v| v.as_bool()) {
                        f.first = b;
                    }
                }
                "mono" => apply_face(&mut self.mono, node),
                "align" => {
                    self.align = match node.entry(0).and_then(|e| e.value().as_string()) {
                        Some("justify") => Align::Justify,
                        _ => Align::Left,
                    }
                }
                "page-numbers" => self.page_numbers(node.entry(0).and_then(|e| e.value().as_bool()).unwrap_or(true)),
                other => log::warn!("style: unknown node `{other}`"),
            }
        }
        Ok(())
    }

    /// `page-numbers`, as the styles before headers and footers had it: a
    /// number centred in the footer, or none.
    fn page_numbers(&mut self, on: bool) {
        if on && self.footer.center.is_empty() {
            self.footer.center = "{page}".into();
        } else if !on {
            for t in [&mut self.footer.left, &mut self.footer.center, &mut self.footer.right] {
                if t.contains("{page}") {
                    t.clear();
                }
            }
        }
    }

    /// The style named `name`: the user's file of that name, else the
    /// built-in one, over the manuscript (which fills in what it leaves
    /// out).
    pub fn named(name: &str) -> Style {
        let mut style = Style::manuscript();
        match std::fs::read_to_string(style_path(name)) {
            Ok(text) => {
                if let Err(e) = style.apply_kdl(&text) {
                    log::warn!("style {name}: {e}");
                }
            }
            Err(e) => match BUILT_IN.iter().find(|(n, _)| *n == name) {
                Some((_, kdl)) => style.apply_kdl(kdl).expect("the built-in styles parse"),
                None => log::warn!("style {name}: {e}"),
            },
        }
        style
    }

    /// The style a document asks for: its `style:` (a user style file, or
    /// a built-in), then its other front-matter keys on top.
    pub fn for_document(front: &[(String, String)]) -> Style {
        let mut style = match front.iter().find(|(k, _)| k == "style") {
            Some((_, name)) => Style::named(name),
            None => Style::manuscript(),
        };
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
                "indent" => {
                    if let Some(i) = length(v, 1.0) {
                        style.indent = i;
                    }
                }
                "page-numbers" => style.page_numbers(v != "false" && v != "no"),
                "header" => style.header.center = v.clone(),
                "header-left" => style.header.left = v.clone(),
                "header-right" => style.header.right = v.clone(),
                "footer" => style.footer.center = v.clone(),
                "footer-left" => style.footer.left = v.clone(),
                "footer-right" => style.footer.right = v.clone(),
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

/// The styles there are: the built-in ones, then the user's (any
/// `.kdl` in the styles folder), by name.
pub fn available() -> Vec<String> {
    let mut names: Vec<String> = BUILT_IN.iter().map(|(n, _)| n.to_string()).collect();
    if let Ok(dir) = std::fs::read_dir(styles_dir()) {
        let mut user: Vec<String> = dir
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().is_some_and(|x| x == "kdl")).then(|| p.file_stem()?.to_str().map(str::to_string)).flatten()
            })
            .filter(|n| !names.contains(n))
            .collect();
        user.sort();
        names.extend(user);
    }
    names
}

/// A new document in style `name`: the user's `<name>.md` template beside
/// the style when there is one, else front matter naming the style and an
/// empty first heading.
pub fn template(name: &str) -> String {
    if let Ok(t) = std::fs::read_to_string(styles_dir().join(format!("{name}.md"))) {
        return t;
    }
    format!("---\nstyle: {name}\n---\n# Untitled\n\n")
}

fn styles_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cce/documents/styles")
}

pub fn style_path(name: &str) -> PathBuf {
    styles_dir().join(format!("{name}.kdl"))
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
        assert_eq!(s.footer.center, "{page}");
        let b = Style::named("book");
        assert!(b.page.0 < s.page.0 && b.indent > 0.0 && b.headings[0].break_before);
        assert_eq!((b.header.center.as_str(), b.header.first), ("{section}", false));
    }

    #[test]
    fn page_numbers_off_clears_the_footer_number() {
        let s = Style::for_document(&[("page-numbers".into(), "false".into())]);
        assert_eq!(s.footer.center, "");
        let s = Style::for_document(&[("header-right".into(), "{title}".into())]);
        assert_eq!(s.header.right, "{title}");
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
