//! Printing through the desktop's Print portal.
//!
//! The portal works in two steps: `PreparePrint` shows the print dialog (in
//! cce, a cce-cloud panel served by cce-desktop-portal) and returns a token,
//! and `Print` hands over the document — an open PDF file — with it. Both
//! wait on the person, so they run on a thread of their own, through the
//! same ashpd build and blocking executor rfd uses for file dialogs, and the
//! outcome comes back as a `Message`.

use std::path::PathBuf;

use ashpd::desktop::print::{PageSetup, PrintProxy, Settings};

use crate::Message;

/// Print the PDF at `path`, titled `title` in the dialog and the queue.
pub fn print(path: PathBuf, title: String, notify: calloop::channel::Sender<Message>) {
    std::thread::Builder::new()
        .name("print".into())
        .spawn(move || {
            let result = pollster::block_on(run(&path, &title));
            let _ = notify.send(Message::Printed { result });
        })
        .expect("spawn the print thread");
}

/// Ok(true) when sent, Ok(false) when the dialog was dismissed.
async fn run(path: &std::path::Path, title: &str) -> Result<bool, String> {
    let proxy = PrintProxy::new().await.map_err(|e| format!("no print portal: {e}"))?;
    let prepared = proxy
        .prepare_print(None, title, Settings::default(), PageSetup::default(), None, true)
        .await
        .map_err(|e| e.to_string())?;
    let prepared = match prepared.response() {
        Ok(p) => p,
        // The person closed the dialog.
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    proxy
        .print(None, title, &file, Some(prepared.token), true)
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    Ok(true)
}
