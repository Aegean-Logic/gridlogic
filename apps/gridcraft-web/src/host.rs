//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! GridCraft in an `<iframe>` (such as a Nextcloud app). Messages are plain objects with a `type`:
//!
//! - `gridcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `gridcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a workbook picked with File › Open or dropped on the window.
//! - `gridcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file GridCraft would otherwise download: Save and Save As (an .xlsx workbook) and Export
//!   as PDF. The parent stores it and reports failures itself.
//! - `gridcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `gridcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! With `&ack=1` the parent confirms every save (`acks`): `gridcraft:save` also carries an `id`,
//! and the parent answers `gridcraft:saved { id, name? }` once it stored the file or
//! `gridcraft:saveFailed { id, error }` when it couldn't. A workbook counts as saved only on
//! `gridcraft:saved`; a failure is shown to the user. `gridcraft:ready` says `{ ack: true }`
//! when this is on, so a parent can tell an older GridCraft that ignores the flag. Without it, a
//! workbook counts as saved as soon as it's handed over.
//!
//! Co-editing (the page runs the session with the server; `collab_msg` checks what it sends):
//!
//! - `gridcraft:collab:start` (from the parent): `{ client, seq, readOnly?, json? }`, after
//!   `gridcraft:open`: the workbook just opened is the shared one as of log position `seq`;
//!   `json`, the session's sheet ids in order as kept with the checkpoint (XLSX loses them).
//! - `gridcraft:collab:entries` (from the parent): `{ json }`, the server's log entries in order
//!   (JSON text of `collab::Incoming`s).
//! - `gridcraft:collab:op` (to the parent): `{ json }`, this person's next op for the server
//!   (`collab::Outgoing`). The next comes once this one is back in the log.
//! - `gridcraft:collab:checkpoint` (from the parent): `{ id }`; answered with
//!   `gridcraft:collab:checkpointData { id, seq, name, sheetIds, bytes }`, the shared workbook
//!   (without this person's unconfirmed edits) as .xlsx, its log position, and its sheet ids
//!   (JSON text) for the server to keep with it.
//! - `gridcraft:collab:selection` (to the parent): `{ json }`, this person's position
//!   (`{sheet, active, ranges}`) whenever it changes; `gridcraft:collab:presence` (from the
//!   parent): `{ json }`, the others' (`[{name, color, sheet, active, ranges}]`), drawn on the grid.
//! - `gridcraft:collab:stop` (from the parent) ends sharing; `gridcraft:collab:error` (to the
//!   parent) `{ error }` says this copy can't follow the log and must rejoin.
//!
//! Sharing (the Share dialog's cloud is the page, which has the user's Nextcloud session):
//!
//! - `gridcraft:cloud` (to the parent): `{ id, op, params }`, an operation of
//!   `gridcraft_ui_egui::share::Cloud` (`params` as JSON text).
//! - `gridcraft:cloud:reply` (from the parent): `{ id, ok, json?, error? }`, its answer (`json`
//!   as JSON text when `ok`, else `error`).
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts with an empty workbook instead of a sample, and the parent guards leaving the page.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use gridcraft_ui_egui::share::{Cloud, CloudInbox};
use gridcraft_ui_egui::{Inbox, Services, SheetApp};

use crate::acks::{Reply, SaveAcks};
use crate::collab_msg::{self, FromPage, Position};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// Saving one of these hands the whole workbook to the parent, which stores it: the workbook
/// counts as saved under that name, as it would after Save on the desktop.
const WORKBOOK_EXTS: &[&str] = &["xlsx"];

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

/// The most save answers held between two frames; more are dropped.
const MAX_REPLIES: usize = 256;
/// The most co-editing messages held between two frames; past this the page has to rejoin.
const MAX_COLLAB_QUEUE: usize = 4096;
/// The largest answer to a sharing request taken from the page (JSON text).
const MAX_CLOUD_REPLY_BYTES: usize = 4 << 20;

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// Names of the files handed to the parent since the last frame.
    saved: Rc<RefCell<Vec<String>>>,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
    /// `&ack=1`: saves the parent hasn't answered yet; `None` when the parent doesn't answer.
    acks: Option<Rc<RefCell<SaveAcks>>>,
    /// The parent's answers since the last frame.
    replies: Rc<RefCell<Vec<Reply>>>,
    /// Co-editing messages from the parent since the last frame, in order.
    collab: Rc<RefCell<VecDeque<FromPage>>>,
    /// Asks for another frame when log entries change what's shown.
    ctx: Rc<RefCell<Option<egui::Context>>>,
    /// This person's position as last told to the parent (JSON).
    position: Rc<RefCell<String>>,
    /// The app, for handling co-editing messages as they arrive (`attach`), and the files the
    /// page sent that it hasn't opened yet.
    app: Rc<RefCell<Option<(Weak<RefCell<SheetApp>>, Inbox)>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to). `ack`: the parent
    /// confirms saves (`&ack=1`).
    pub fn new(ack: bool) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        let acks = ack.then(Rc::default);
        Some(Self {
            window,
            parent,
            origin,
            saved: Rc::default(),
            reported: Rc::default(),
            acks,
            replies: Rc::default(),
            collab: Rc::default(),
            ctx: Rc::default(),
            position: Rc::default(),
            app: Rc::default(),
        })
    }

    /// Send saved and exported files to the parent instead of downloading them, and hear files
    /// from it.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        *self.ctx.borrow_mut() = Some(ctx.clone());
        let answers: CloudInbox = CloudInbox::default();
        self.listen(inbox, answers.clone(), ctx);
        // The Share dialog's cloud: the page, which has the user's Nextcloud session.
        let bridge = self.clone();
        services.cloud = Some(Cloud {
            request: Box::new(move |id, op, params| {
                let params = params.to_string();
                bridge.send(&[
                    ("type", "gridcraft:cloud".into()),
                    ("id", JsValue::from_f64(id as f64)),
                    ("op", op.into()),
                    ("params", params.as_str().into()),
                ]);
            }),
            inbox: answers,
        });
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes) {
                log::error!("couldn't hand {name} to the page: {e}");
            }
        }));
    }

    /// Handle co-editing messages as they arrive rather than at the next frame, so a hidden tab
    /// (which draws none) keeps up with the others and answers save requests.
    pub fn attach(&self, app: &Rc<RefCell<SheetApp>>, inbox: Inbox) {
        *self.app.borrow_mut() = Some((Rc::downgrade(app), inbox));
    }

    /// Co-editing messages now, if the app is free and has opened every file the page sent (a
    /// session starts on the file that came just before it).
    fn sync_collab_now(&self) {
        let Some((app, inbox)) = self.app.borrow().clone() else { return };
        let opened = inbox.lock().map(|q| q.is_empty()).unwrap_or(false);
        if let (true, Some(app)) = (opened, app.upgrade())
            && let Ok(mut app) = app.try_borrow_mut()
        {
            self.sync_collab(&mut app);
        }
    }

    /// `?author=`: the host's name for the user signs new notes and comments.
    pub fn set_author(app: &mut SheetApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.user_name = author;
        }
    }

    pub fn post_ready(&self) {
        self.send(&[
            ("type", "gridcraft:ready".into()),
            ("version", env!("CARGO_PKG_VERSION").into()),
            ("ack", self.acks.is_some().into()),
            ("collab", true.into()),
        ]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "gridcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: a workbook handed to the parent this frame counts as saved under the
    /// name it was saved as (with `&ack=1`, once the parent says it stored it), and the parent
    /// hears when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut SheetApp) {
        if let Some(acks) = &self.acks {
            let mut acks = acks.borrow_mut();
            acks.bind(&app.session);
            let replies = std::mem::take(&mut *self.replies.borrow_mut());
            for reply in replies {
                if let Some(shown) = acks.apply(reply, &mut app.session) {
                    app.message = Some(("GridLogic".into(), shown));
                }
            }
        } else {
            let saved = std::mem::take(&mut *self.saved.borrow_mut());
            if let Some(name) = saved.iter().rev().find(|name| is_workbook(name))
                && let Some(doc) = app.session.active_mut()
            {
                doc.saved = doc.wb.clone();
                if doc.path.is_none() {
                    doc.title = name.clone();
                }
            }
        }
        self.sync_collab(app);
        let dirty = app.session.documents().iter().any(|d| d.is_dirty());
        if self.reported.get() != Some(dirty) && self.send(&[("type", "gridcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Co-editing after each frame: the page's messages in order, then this person's next op.
    fn sync_collab(&self, app: &mut SheetApp) {
        let queued: Vec<FromPage> = self.collab.borrow_mut().drain(..).collect();
        let mut changed = false;
        for msg in queued {
            match msg {
                FromPage::Start { client, seq, read_only, sheet_ids } => {
                    let adopted = if sheet_ids.is_empty() { Ok(()) } else { app.session.collab_adopt_sheet_ids(&sheet_ids) };
                    match adopted.and_then(|()| app.session.collab_start(client, seq)) {
                        Ok(()) => app.session.collab_set_read_only(read_only),
                        Err(e) => self.post_collab_error(&e.to_string()),
                    }
                }
                FromPage::Entries(entries) => {
                    for entry in entries {
                        if let Err(e) = app.session.collab_receive(entry) {
                            app.session.collab_stop();
                            self.post_collab_error(&e.to_string());
                            break;
                        }
                        changed = true;
                    }
                }
                FromPage::Checkpoint { id } => self.post_checkpoint(app, id),
                FromPage::Stop => {
                    app.session.collab_stop();
                    app.presence.clear();
                    changed = true;
                }
                FromPage::Presence(people) => {
                    app.presence = people
                        .into_iter()
                        .map(|s| {
                            let [r, g, b] = s.rgb();
                            gridcraft_ui_egui::Presence {
                                name: s.name,
                                color: egui::Color32::from_rgb(r, g, b),
                                sheet: s.sheet,
                                active: s.active,
                                ranges: s.ranges,
                            }
                        })
                        .collect();
                    changed = true;
                }
            }
        }
        self.post_position(app);
        if let Some(out) = app.session.collab_outgoing() {
            match serde_json::to_string(&out) {
                Ok(json) => {
                    self.send(&[("type", "gridcraft:collab:op".into()), ("json", json.into())]);
                }
                Err(e) => self.post_collab_error(&e.to_string()),
            }
        }
        if changed && let Some(ctx) = self.ctx.borrow().as_ref() {
            ctx.request_repaint();
        }
    }

    /// Tells the parent where this person is (their sheet and selection) when it has changed.
    fn post_position(&self, app: &SheetApp) {
        if app.session.collab_client().is_none() {
            return;
        }
        let Some(d) = app.session.active() else { return };
        let sheet = d.wb.active().map_or(0, |s| s.id);
        let position = Position::new(sheet, d.selection.active, &d.selection.ranges);
        let Ok(json) = serde_json::to_string(&position) else { return };
        if *self.position.borrow() != json && self.send(&[("type", "gridcraft:collab:selection".into()), ("json", json.as_str().into())]) {
            *self.position.borrow_mut() = json;
        }
    }

    /// The shared workbook, as everyone has it, for the page to save as the session's checkpoint.
    fn post_checkpoint(&self, app: &SheetApp, id: u64) {
        let Some((seq, wb)) = app.session.collab_confirmed() else {
            self.post_collab_error("no shared workbook to save");
            return;
        };
        let name = app.session.active().map(|d| d.display_title()).unwrap_or_else(|| "Book.xlsx".into());
        let bytes = match gridcraft_engine::io::save_bytes(&wb, &name) {
            Ok(b) => b,
            Err(e) => {
                self.post_collab_error(&format!("couldn't save the shared workbook: {e}"));
                return;
            }
        };
        // XLSX doesn't keep sheet ids: the page keeps them with the checkpoint for joiners.
        let ids: Vec<u64> = wb.sheets.iter().map(|s| s.id).collect();
        let sheet_ids = serde_json::to_string(&ids).unwrap_or_default();
        let buffer = js_sys::Uint8Array::from(bytes.as_slice()).buffer();
        let fields = [
            ("type", "gridcraft:collab:checkpointData".into()),
            ("id", JsValue::from_f64(id as f64)),
            ("seq", JsValue::from_f64(seq as f64)),
            ("name", name.as_str().into()),
            ("sheetIds", sheet_ids.as_str().into()),
            ("bytes", buffer.clone().into()),
        ];
        if let Some(m) = message(&fields)
            && self.parent.post_message_with_transfer(&m, &self.origin, &js_sys::Array::of1(&buffer)).is_err()
        {
            log::error!("couldn't hand the checkpoint to the page");
        }
    }

    fn post_collab_error(&self, error: &str) {
        log::error!("co-editing: {error}");
        self.send(&[("type", "gridcraft:collab:error".into()), ("error", error.into())]);
    }

    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let id = self.acks.as_ref().map(|acks| acks.borrow_mut().begin(&name, is_workbook(&name)));
        let mut fields = vec![("type", "gridcraft:save".into()), ("name", name.as_str().into()), ("bytes", buffer.clone().into())];
        if let Some(id) = id {
            // Ids stay far below 2^53, so a JavaScript number holds them exactly.
            fields.push(("id", JsValue::from_f64(id as f64)));
        }
        let sent = message(&fields).ok_or_else(|| "couldn't build the message".to_string()).and_then(|message| {
            self.parent
                .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
                .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
        });
        match (sent, id, &self.acks) {
            (Err(e), Some(id), Some(acks)) => {
                acks.borrow_mut().forget(id);
                Err(e)
            }
            (Err(e), ..) => Err(e),
            (Ok(()), None, _) => {
                self.saved.borrow_mut().push(name);
                Ok(())
            }
            (Ok(()), ..) => Ok(()),
        }
    }

    fn listen(&self, inbox: Inbox, answers: CloudInbox, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let acks = self.acks.is_some();
        let replies = self.replies.clone();
        let collab = self.collab.clone();
        let bridge = self.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            let Some(kind) = string(&data, "type") else { return };
            if kind == "gridcraft:cloud:reply" {
                let Some(id) = number(&data, "id").filter(|v| v.is_finite() && *v >= 0.0 && v.fract() == 0.0) else { return };
                let ok = js_sys::Reflect::get(&data, &JsValue::from_str("ok")).ok().and_then(|v| v.as_bool()).unwrap_or(false);
                let answer = if ok {
                    match string(&data, "json") {
                        Some(text) if text.len() <= MAX_CLOUD_REPLY_BYTES => serde_json::from_str(&text).map_err(|e| e.to_string()),
                        Some(_) => Err("the answer was too large".to_string()),
                        None => Ok(serde_json::Value::Null),
                    }
                } else {
                    Err(string(&data, "error").unwrap_or_else(|| "Nextcloud didn't answer".into()).chars().take(500).collect())
                };
                answers.lock().unwrap_or_else(|e| e.into_inner()).push((id as u64, answer));
                ctx.request_repaint();
                return;
            }
            if kind == "gridcraft:open" {
                let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Book.xlsx".to_string());
                let Some(bytes) = bytes(&data) else { return };
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                ctx.request_repaint();
            } else if let Some(msg) = collab_msg::parse(
                &kind,
                number(&data, "client"),
                number(&data, "seq"),
                string(&data, "json").as_deref(),
                number(&data, "id"),
                js_sys::Reflect::get(&data, &JsValue::from_str("readOnly")).ok().and_then(|v| v.as_bool()),
            ) {
                match msg {
                    Ok(msg) => {
                        let mut queue = collab.borrow_mut();
                        if queue.len() < MAX_COLLAB_QUEUE {
                            queue.push_back(msg);
                        } else {
                            bridge.post_collab_error("too many messages queued; rejoin");
                        }
                    }
                    Err(e) => bridge.post_collab_error(&e),
                }
                bridge.sync_collab_now();
                ctx.request_repaint();
            } else if acks && let Some(reply) = Reply::parse(&kind, number(&data, "id"), string(&data, "name"), string(&data, "error")) {
                let mut replies = replies.borrow_mut();
                if replies.len() < MAX_REPLIES {
                    replies.push(reply);
                }
                ctx.request_repaint();
            }
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

fn is_workbook(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| WORKBOOK_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

fn number(data: &JsValue, key: &str) -> Option<f64> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_f64()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
