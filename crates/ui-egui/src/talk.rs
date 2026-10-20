//! "Chat about this file": the workbook's conversation in Nextcloud Talk, opened in the browser,
//! as Excel opens a file's Teams chat. A native chat panel can come later; the browser already
//! has Talk's chat and calls.
//!
//! The host's cloud ([`crate::share::Cloud`]) says whether its server has Talk (`talk`) and
//! answers one operation about the workbook that's open:
//!
//! - `talk.open` `{}` → `{url}`: opens the workbook's conversation in the browser; an error saying
//!   to share the workbook first when Talk has no conversation for it (it isn't shared yet)

use serde_json::{Value as Json, json};

use crate::SheetApp;
use crate::share::next_request_id;

/// The chat being opened, if it is.
#[derive(Clone, Debug, Default)]
pub struct Talk {
    asked: Option<u64>,
}

impl Talk {
    /// The chat is being opened (the host hasn't answered yet).
    pub fn opening(&self) -> bool {
        self.asked.is_some()
    }
}

/// Whether "Chat about this file" is offered: the workbook is in Nextcloud and its server has
/// Talk.
pub fn available(app: &SheetApp) -> bool {
    app.services.cloud.as_ref().is_some_and(|c| c.has_talk()) && app.cloud_location().is_some()
}

/// Opens the workbook's conversation in the browser (the host's answer arrives later).
pub fn open(app: &mut SheetApp) -> Result<(), String> {
    if !available(app) {
        return Err("Chat is for workbooks in Nextcloud, on a server with Nextcloud Talk.".into());
    }
    if app.talk.opening() {
        return Ok(());
    }
    let Some(cloud) = &app.services.cloud else { return Err("Chat needs a Nextcloud connection.".into()) };
    let id = next_request_id();
    (cloud.request)(id, "talk.open", json!({}));
    app.talk.asked = Some(id);
    app.toast = Some(("Opening the chat in your browser…".into(), crate::now_ms()));
    Ok(())
}

/// Takes the host's answer. Every frame.
pub fn show(app: &mut SheetApp, ctx: &egui::Context) {
    let Some(id) = app.talk.asked else { return };
    let Some(cloud) = &app.services.cloud else {
        app.talk.asked = None;
        return;
    };
    let answer: Option<Result<Json, String>> = {
        let mut inbox = cloud.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.iter().position(|(i, _)| *i == id).map(|at| inbox.remove(at).1)
    };
    let Some(answer) = answer else { return };
    app.talk.asked = None;
    match answer {
        Ok(_) => app.toast = Some(("The chat opened in your browser.".into(), crate::now_ms())),
        Err(e) => app.message = Some(("Chat".into(), crate::clean_error(&e))),
    }
    ctx.request_repaint();
}
