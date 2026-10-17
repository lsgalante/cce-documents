//! Where the window was, so it comes back there: the open document, the page
//! on screen, the zoom (or fitting the page), and in a Markdown document the
//! caret. The document is saved whenever one is opened (or a PDF saved as),
//! the rest when the window exits (`on_exit`, which `ccectl restart-app` runs
//! as any close does); a launch reopens it — after a restart, at login, or
//! when reopened.
//!
//! **A restart replays the same command line**, so a window opened as
//! `cce-documents a.pdf` would always come back on a.pdf, and one opened as
//! `cce-documents --new <style> b.md` would try to make b.md again. The file
//! therefore also records what the command line asked for (`launched`): the
//! same command line again, or one asking for nothing, is this window
//! reopening, and the saved document wins — opened, never created; a command
//! line asking for something else is a new request, and it wins. A saved
//! document that has gone is not reopened, and the page, zoom and caret only
//! go back into the document they came from.
//!
//! `cce_state_dir()/documents/state.json`, written whole to a temporary file
//! and renamed over, so a crash mid-write leaves the last one.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// What a command line asked for.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Launched {
    /// The document it named, absolute.
    pub file: Option<PathBuf>,
    /// `--new <style>`: make the document in this style.
    pub style: Option<String>,
}

/// Where in a document the window was.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub page: usize,
    /// Screen px per point; `None` fits the page to the window.
    pub zoom: Option<f64>,
    /// A Markdown document's caret, in file bytes.
    pub caret: Option<usize>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    /// The open document, absolute.
    file: Option<PathBuf>,
    /// What the command line asked for when this was saved.
    #[serde(default)]
    launched: Launched,
    #[serde(default)]
    view: View,
}

/// How a launch starts.
#[derive(Debug, PartialEq)]
pub enum Start {
    /// Open this document (rather than what the command line asks), there.
    Reopen(PathBuf, View),
    /// Do what the command line asks; then, if it opened the document the
    /// view was saved in, go back there.
    Launch(Option<View>),
}

/// What this run's command line asked for.
static LAUNCHED: OnceLock<Launched> = OnceLock::new();

fn path() -> PathBuf {
    cce_ui::config::cce_state_dir().join("documents").join("state.json")
}

pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// How to start, given what the command line asked for: back where the
/// window was when this command line is the one it was saved under (or asks
/// for nothing), else as the command line asks.
pub fn initial(launched: Launched) -> Start {
    let _ = LAUNCHED.set(launched.clone());
    let saved = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str::<Saved>(&s).ok());
    choose(&launched, saved.as_ref(), Path::exists)
}

fn choose(launched: &Launched, saved: Option<&Saved>, exists: impl Fn(&Path) -> bool) -> Start {
    let asks_nothing = *launched == Launched::default();
    let reopening = saved.filter(|s| asks_nothing || s.launched == *launched);
    if let Some((file, s)) = reopening.and_then(|s| Some((s.file.as_ref().filter(|f| exists(f))?, s))) {
        return Start::Reopen(file.clone(), s.view);
    }
    let same_file = saved.filter(|s| launched.file.is_some() && s.file == launched.file);
    Start::Launch(same_file.map(|s| s.view))
}

/// The open document changed, or the window is closing with the view where
/// it is.
pub fn save(file: Option<&Path>, view: View) {
    let saved = Saved { file: file.map(absolute), launched: LAUNCHED.get().cloned().unwrap_or_default(), view };
    let path = path();
    let Some(dir) = path.parent() else { return };
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec(&saved).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        log::warn!("[documents] cannot save the open document to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(file: Option<&str>, style: Option<&str>) -> Launched {
        Launched { file: file.map(PathBuf::from), style: style.map(String::from) }
    }

    const VIEW: View = View { page: 7, zoom: Some(1.5), caret: None };

    fn saved(file: &str, launched: Launched) -> Saved {
        Saved { file: Some(file.into()), launched, view: VIEW }
    }

    const ALL: fn(&Path) -> bool = |_| true;

    #[test]
    fn the_same_command_line_reopens_where_it_was() {
        // Launched on a.pdf, opened b.pdf, restarted with the same argv.
        let argv = launch(Some("/a.pdf"), None);
        assert_eq!(choose(&argv, Some(&saved("/b.pdf", argv.clone())), ALL), Start::Reopen("/b.pdf".into(), VIEW));
        // A plain launch reopens whatever was open, however that was launched.
        let s = saved("/b.pdf", argv);
        assert_eq!(choose(&Launched::default(), Some(&s), ALL), Start::Reopen("/b.pdf".into(), VIEW));
        // `--new` again opens what it made; it does not make it again.
        let argv = launch(Some("/c.md"), Some("book"));
        assert_eq!(choose(&argv, Some(&saved("/c.md", argv.clone())), ALL), Start::Reopen("/c.md".into(), VIEW));
    }

    #[test]
    fn a_command_line_asking_for_something_else_wins() {
        let s = saved("/b.pdf", launch(Some("/a.pdf"), None));
        assert_eq!(choose(&launch(Some("/c.pdf"), None), Some(&s), ALL), Start::Launch(None));
        // Naming the open document keeps its view.
        assert_eq!(choose(&launch(Some("/b.pdf"), None), Some(&s), ALL), Start::Launch(Some(VIEW)));
    }

    #[test]
    fn nothing_saved_starts_as_launched() {
        assert_eq!(choose(&launch(Some("/c.pdf"), None), None, ALL), Start::Launch(None));
        assert_eq!(choose(&Launched::default(), None, ALL), Start::Launch(None));
    }

    #[test]
    fn a_saved_document_that_has_gone_is_not_reopened() {
        let gone = |p: &Path| p != Path::new("/b.pdf");
        let argv = launch(Some("/a.pdf"), None);
        let s = saved("/b.pdf", argv.clone());
        assert_eq!(choose(&argv, Some(&s), gone), Start::Launch(None));
        assert_eq!(choose(&Launched::default(), Some(&s), gone), Start::Launch(None));
    }
}
