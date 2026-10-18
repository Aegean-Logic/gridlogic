//! Live co-editing for a workbook opened from Nextcloud: the desktop joins the same session as
//! the browser editor (`gridcraft-nextcloud`'s `/apps/gridcraft/collab/*`, signed in with the
//! app password) and does what the page's `collab.js` does there. Edits go out one at a time,
//! everyone's come back in the server's order, people's positions travel with each poll, and an
//! editor saves the shared workbook to the file now and then (a checkpoint).
//!
//! A worker thread talks to the server; the UI thread ([`Coedit::poll`]) moves edits between it
//! and the engine. GridLogic shares one workbook at a time (the engine's one shared session);
//! another workbook from Nextcloud opens unshared.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gridcraft_engine::collab::{Incoming, Op, PROTOCOL};
use gridcraft_engine::core::{CellRef, RangeRef};
use gridcraft_engine::model::Workbook;
use gridcraft_ui_egui::{Presence, SheetApp};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::{Connection, State, lock, mtime, persist, tie_key};

/// Save the shared workbook after this many log entries, or this long after the first unsaved
/// one (as the browser does); after a Save here, at most this often.
const CHECKPOINT_OPS: u64 = 20;
const CHECKPOINT_SECONDS: f64 = 30.0;
const SAVE_CHECKPOINT_SECONDS: f64 = 10.0;
/// How often to ask for others' edits.
const POLL: Duration = Duration::from_millis(600);

/// Said when the session is on a newer GridLogic than this one, whose edits this one would
/// misread: at joining, and when someone joins with one.
pub(super) const OUTDATED: &str =
    "Someone is editing this workbook with a newer GridLogic, so it opened here without editing together. Update GridLogic to edit it with them.";
const OUTDATED_NOW: &str = "Someone joined with a newer GridLogic, so editing together stopped here. Update GridLogic to go on editing with them.";

/// Whether the session's build (`join`'s `build`: the newest co-editing protocol in it) is newer
/// than this one's.
fn newer_than_mine(build: Option<&Json>) -> bool {
    build.and_then(Json::as_str).and_then(|b| b.trim().parse::<u32>().ok()).is_some_and(|b| b > PROTOCOL)
}
/// The browser's colours for people, by client id (`collab.js`).
const COLORS: [(u8, u8, u8); 8] = [
    (0xC0, 0x39, 0x2B),
    (0x1F, 0x6F, 0xB2),
    (0x8E, 0x44, 0xAD),
    (0xB9, 0x77, 0x0E),
    (0x14, 0x8F, 0x77),
    (0xAD, 0x14, 0x57),
    (0x2E, 0x7D, 0x32),
    (0x6D, 0x4C, 0x41),
];

fn color_for(client: u64) -> egui::Color32 {
    let (r, g, b) = COLORS[(client.saturating_sub(1) % COLORS.len() as u64) as usize];
    egui::Color32::from_rgb(r, g, b)
}

/// Someone in the session, as the server lists them.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Member {
    client: u64,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    selection: String,
    #[serde(default = "yes")]
    can_edit: bool,
}

fn yes() -> bool {
    true
}

/// Where someone is: the same JSON the browser sends.
#[derive(Debug, Serialize, Deserialize)]
struct Position {
    sheet: u64,
    active: CellRef,
    #[serde(default)]
    ranges: Vec<RangeRef>,
}

/// What joining gave: the session the downloaded file belongs to.
#[derive(Clone, Debug)]
pub(super) struct Joined {
    session_id: u64,
    client: u64,
    seq: u64,
    entries: Vec<Incoming>,
    members: Vec<Member>,
    sheet_ids: Vec<u64>,
    can_edit: bool,
    /// The file's ETag as downloaded (no quotes): the session's checkpoint.
    etag: String,
    /// The file, relative to the user's files.
    remote: String,
}

/// Log entries as the server sends them (each op as JSON text); an op that doesn't parse
/// becomes a no-op in its place.
fn entries(v: Option<&Json>) -> Vec<Incoming> {
    v.and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let num = |k: &str| e.get(k).and_then(Json::as_u64);
            let op: Op = e.get("op").and_then(Json::as_str).and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
            Some(Incoming { seq: num("seq")?, client: num("client")?, client_seq: num("client_seq")?, base: num("base")?, op })
        })
        .collect()
}

/// Joins the session for `file_id`, pinned to the version downloaded (`etag`). `None`: the
/// session saved a newer version since; download again.
pub(super) fn join(nc: &Connection, file_id: u64, etag: &str, remote: &str) -> Result<Option<Joined>, String> {
    let etag = etag.trim_start_matches("W/").trim_matches('"').to_string();
    // `build`: what this build's ops mean; the server sends anyone on an older one to reload.
    let r = nc.app("POST", "collab/join", &json!({"fileId": file_id, "etag": etag, "build": PROTOCOL.to_string()}))?;
    if r.get("retry").and_then(Json::as_bool) == Some(true) {
        return Ok(None);
    }
    let num = |k: &str| r.get(k).and_then(Json::as_u64).ok_or_else(|| format!("Nextcloud's co-editing answer had no {k}."));
    let (session_id, client, seq) = (num("sessionId")?, num("client")?, num("seq")?);
    if newer_than_mine(r.get("build")) {
        let _ = nc.app("POST", "collab/leave", &json!({"sessionId": session_id, "client": client}));
        return Err(OUTDATED.into());
    }
    let sheet_ids = r.get("sheetIds").and_then(Json::as_str).filter(|s| !s.is_empty()).and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    Ok(Some(Joined {
        session_id,
        client,
        seq,
        entries: entries(r.get("entries")),
        members: r.get("members").cloned().and_then(|m| serde_json::from_value(m).ok()).unwrap_or_default(),
        sheet_ids,
        can_edit: r.get("canEdit").and_then(Json::as_bool) != Some(false),
        etag,
        remote: remote.to_string(),
    }))
}

/// A checkpoint for the worker to write.
struct Job {
    seq: u64,
    wb: Arc<Workbook>,
    name: String,
    sheet_ids: String,
}

enum Event {
    /// This copy can't follow the session any more: open the file again.
    Rejoin,
    /// The session can't go on (the file changed outside it).
    End(String),
    Notice(String, bool),
}

/// Between the UI thread and the worker.
#[derive(Default)]
struct Link {
    /// This person's edits, in order: (client sequence number, base, op as JSON text).
    outbox: VecDeque<(u64, u64, String)>,
    entries: Vec<Incoming>,
    members: Option<Vec<Member>>,
    checkpoint_seq: u64,
    checkpoint_etag: String,
    /// This person's position (JSON), for the next poll.
    selection: String,
    job: Option<Job>,
    writing: bool,
    events: Vec<Event>,
    stop: bool,
}

fn link(l: &Mutex<Link>) -> MutexGuard<'_, Link> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

/// What became of the session this frame.
pub(super) enum Status {
    Going,
    /// The workbook was closed, or the session ended: sharing is over.
    Over,
    /// Open the file again and rejoin.
    Rejoin,
}

/// A shared workbook (the UI thread's side).
pub(super) struct Coedit {
    uid: u64,
    /// The local copy (its record's key) and the Nextcloud file.
    pub(super) local: String,
    pub(super) remote: String,
    client: u64,
    can_edit: bool,
    link: Arc<Mutex<Link>>,
    members: Vec<Member>,
    first_unsaved_at: Option<f64>,
    saved_stamp: usize,
    last_save_checkpoint: f64,
}

impl Coedit {
    /// Shares the active workbook (just opened from the downloaded file) in the joined session.
    pub(super) fn start(app: &mut SheetApp, joined: Joined, local: String, state: Arc<Mutex<State>>, ctx: &egui::Context) -> Result<Coedit, String> {
        let uid = app.session.active().map(|d| d.uid).ok_or("The workbook didn't open.")?;
        if !joined.sheet_ids.is_empty() {
            app.session.collab_adopt_sheet_ids(&joined.sheet_ids).map_err(|e| e.to_string())?;
        }
        app.session.collab_start(joined.client, joined.seq).map_err(|e| e.to_string())?;
        app.session.collab_set_read_only(!joined.can_edit);
        let mut last_seq = joined.seq;
        for e in joined.entries {
            last_seq = e.seq;
            if let Err(e) = app.session.collab_receive(e) {
                app.session.collab_stop();
                return Err(e.to_string());
            }
        }
        if !joined.can_edit {
            app.toast = Some(("You can view this workbook but not edit it.".into(), gridcraft_ui_egui::now_ms()));
        }
        let link = Arc::new(Mutex::new(Link { checkpoint_seq: joined.seq, checkpoint_etag: joined.etag.clone(), ..Link::default() }));
        let worker = Worker {
            state,
            link: link.clone(),
            ctx: ctx.clone(),
            session_id: joined.session_id,
            client: joined.client,
            last_seq,
            remote: joined.remote.clone(),
            local: local.clone(),
            sent_selection: String::new(),
        };
        std::thread::Builder::new().name("nextcloud co-editing".into()).spawn(move || worker.run()).map_err(|e| e.to_string())?;
        let saved_stamp = app.session.documents().iter().find(|d| d.uid == uid).map_or(0, |d| Arc::as_ptr(&d.saved) as usize);
        let c = Coedit {
            uid,
            local,
            remote: joined.remote,
            client: joined.client,
            can_edit: joined.can_edit,
            link,
            members: joined.members,
            first_unsaved_at: None,
            saved_stamp,
            last_save_checkpoint: f64::NEG_INFINITY,
        };
        c.show_people(app);
        Ok(c)
    }

    /// Stops sharing: the worker leaves the session.
    pub(super) fn stop(&self, app: &mut SheetApp) {
        link(&self.link).stop = true;
        if app.session.collab_client().is_some() && app.session.documents().iter().any(|d| d.uid == self.uid) {
            app.session.collab_stop();
        }
        app.presence.clear();
    }

    /// Every frame: everyone's edits in, this person's out, positions both ways, checkpoints.
    pub(super) fn poll(&mut self, app: &mut SheetApp, now: f64) -> Status {
        let Some(doc) = app.session.documents().iter().find(|d| d.uid == self.uid) else { return Status::Over };
        let saved_stamp = Arc::as_ptr(&doc.saved) as usize;
        let just_saved = saved_stamp != self.saved_stamp;
        self.saved_stamp = saved_stamp;
        let (entries, members, events) = {
            let mut l = link(&self.link);
            (std::mem::take(&mut l.entries), l.members.take(), std::mem::take(&mut l.events))
        };
        for e in entries {
            if app.session.collab_receive(e).is_err() {
                return Status::Rejoin;
            }
        }
        for event in events {
            match event {
                Event::Rejoin => return Status::Rejoin,
                Event::End(text) => {
                    app.message = Some(("Co-editing stopped".into(), text));
                    return Status::Over;
                }
                Event::Notice(text, true) => app.message = Some(("Nextcloud".into(), text)),
                Event::Notice(text, false) => app.toast = Some((text, gridcraft_ui_egui::now_ms())),
            }
        }
        if let Some(m) = members {
            self.members = m;
            self.show_people(app);
        }
        while let Some(out) = app.session.collab_outgoing() {
            let op = serde_json::to_string(&out.op).unwrap_or_else(|_| "{}".into());
            link(&self.link).outbox.push_back((out.client_seq, out.base, op));
        }
        // This person's position, when the shared workbook is the one in front.
        if let Some(d) = app.session.active().filter(|d| d.uid == self.uid) {
            let sheet = d.wb.active().map_or(0, |s| s.id);
            let ranges: Vec<RangeRef> = d.selection.ranges.iter().take(16).copied().collect();
            if let Ok(json) = serde_json::to_string(&Position { sheet, active: d.selection.active, ranges }) {
                link(&self.link).selection = json;
            }
        }
        self.maybe_checkpoint(app, now, just_saved);
        Status::Going
    }

    /// The others, on the grid and in the title bar.
    fn show_people(&self, app: &mut SheetApp) {
        app.presence = self
            .members
            .iter()
            .filter(|m| m.client != self.client)
            .filter_map(|m| {
                let at: Position = serde_json::from_str(&m.selection).ok()?;
                let name = if m.display_name.is_empty() { m.user_id.clone() } else { m.display_name.clone() };
                Some(Presence { name, color: color_for(m.client), sheet: at.sheet, active: at.active, ranges: at.ranges })
            })
            .collect();
    }

    /// The editor with the lowest client id saves when enough has changed; a Save here saves
    /// (an editor's, at most every few seconds, as AutoSave saves often).
    fn maybe_checkpoint(&mut self, app: &SheetApp, now: f64, just_saved: bool) {
        if !self.can_edit {
            return;
        }
        let Some((seq, wb)) = app.session.collab_confirmed() else { return };
        let mut l = link(&self.link);
        if seq <= l.checkpoint_seq {
            self.first_unsaved_at = None;
            return;
        }
        let first = *self.first_unsaved_at.get_or_insert(now);
        let writer = self.members.iter().filter(|m| m.can_edit).map(|m| m.client).min().unwrap_or(self.client) == self.client;
        let due = writer && (seq - l.checkpoint_seq >= CHECKPOINT_OPS || now - first >= CHECKPOINT_SECONDS);
        let asked = just_saved && now - self.last_save_checkpoint >= SAVE_CHECKPOINT_SECONDS;
        if (due || asked) && l.job.is_none() && !l.writing {
            if asked {
                self.last_save_checkpoint = now;
            }
            let name = self.remote.rsplit('/').next().unwrap_or("Book.xlsx").to_string();
            let sheet_ids = serde_json::to_string(&wb.sheets.iter().map(|s| s.id).collect::<Vec<u64>>()).unwrap_or_default();
            l.job = Some(Job { seq, wb, name, sheet_ids });
        }
    }
}

/// The worker thread's side.
struct Worker {
    state: Arc<Mutex<State>>,
    link: Arc<Mutex<Link>>,
    ctx: egui::Context,
    session_id: u64,
    client: u64,
    last_seq: u64,
    remote: String,
    local: String,
    sent_selection: String,
}

impl Worker {
    fn run(mut self) {
        let nc = match Connection::open(&self.state) {
            Ok(nc) => nc,
            Err(e) => return self.event(Event::End(e)),
        };
        loop {
            if link(&self.link).stop {
                let _ = nc.app("POST", "collab/leave", &json!({"sessionId": self.session_id, "client": self.client}));
                return;
            }
            if !self.push(&nc) {
                return;
            }
            let job = link(&self.link).job.take();
            if let Some(job) = job {
                link(&self.link).writing = true;
                let ok = self.checkpoint(&nc, job);
                link(&self.link).writing = false;
                if !ok {
                    return;
                }
            }
            match self.sync(&nc) {
                Some(true) => continue,
                Some(false) => std::thread::sleep(POLL),
                None => return,
            }
        }
    }

    fn event(&self, e: Event) {
        link(&self.link).events.push(e);
        self.ctx.request_repaint();
    }

    /// Sends queued edits in order. False when the session is over for this copy.
    fn push(&self, nc: &Connection) -> bool {
        loop {
            let Some((client_seq, base, op)) = link(&self.link).outbox.front().cloned() else { return true };
            let body = json!({"sessionId": self.session_id, "client": self.client, "clientSeq": client_seq, "base": base, "op": op});
            match nc.app_status("POST", "collab/push", &body) {
                Ok((200..=299, _)) => {
                    link(&self.link).outbox.pop_front();
                }
                Ok((403, _)) => {
                    link(&self.link).outbox.clear();
                    self.event(Event::Notice("You can view this workbook but not edit it: your changes aren't shared.".into(), true));
                }
                Ok((409, body)) => {
                    self.event(if outdated(&body) { Event::End(OUTDATED_NOW.into()) } else { Event::Rejoin });
                    return false;
                }
                Ok((400, body)) => {
                    let why = body.get("error").and_then(Json::as_str).unwrap_or("it was refused").to_string();
                    self.event(Event::Notice(format!("An edit couldn't be shared: {why}"), true));
                    self.event(Event::Rejoin);
                    return false;
                }
                // Offline or a hiccup: the server recognises a retried edit.
                _ => {
                    std::thread::sleep(Duration::from_secs(1));
                    return !link(&self.link).stop;
                }
            }
        }
    }

    /// Others' edits, who's here, and where the file was last saved. `Some(true)`: more to
    /// fetch at once; `None`: the session is over for this copy.
    fn sync(&mut self, nc: &Connection) -> Option<bool> {
        let selection = link(&self.link).selection.clone();
        let sel = if selection != self.sent_selection { selection.clone() } else { String::new() };
        let query = format!("collab/sync?sessionId={}&client={}&since={}&sel={}", self.session_id, self.client, self.last_seq, super::encode(&sel));
        match nc.app_status("GET", &query, &Json::Null) {
            Ok((200..=299, r)) => {
                if r.get("rejoin").and_then(Json::as_bool) == Some(true) {
                    self.event(Event::Rejoin);
                    return None;
                }
                if !sel.is_empty() {
                    self.sent_selection = selection;
                }
                let new = entries(r.get("entries"));
                let more = !new.is_empty();
                if let Some(last) = new.last() {
                    self.last_seq = last.seq;
                }
                let mut l = link(&self.link);
                l.entries.extend(new);
                l.members = r.get("members").cloned().and_then(|m| serde_json::from_value(m).ok());
                let seq = r.get("checkpointSeq").and_then(Json::as_u64).unwrap_or(0);
                if seq > l.checkpoint_seq {
                    l.checkpoint_seq = seq;
                }
                if let Some(etag) = r.get("checkpointEtag").and_then(Json::as_str).filter(|e| !e.is_empty() && *e != l.checkpoint_etag) {
                    l.checkpoint_etag = etag.to_string();
                    let etag = etag.to_string();
                    drop(l);
                    self.settle(&etag);
                }
                self.ctx.request_repaint();
                Some(more)
            }
            Ok((409, body)) => {
                self.event(if outdated(&body) { Event::End(OUTDATED_NOW.into()) } else { Event::Rejoin });
                None
            }
            _ => Some(false),
        }
    }

    /// The local copy's record follows the session's saves: once sharing ends, opening the file
    /// again fetches Nextcloud's version rather than sending this copy.
    fn settle(&self, etag: &str) {
        let mut s = lock(&self.state);
        if let Some(key) = tie_key(&s.stored.files, &self.local)
            && let Some(t) = s.stored.files.get_mut(&key)
        {
            t.etag = Some(format!("\"{etag}\""));
            t.synced = mtime(std::path::Path::new(&key));
            persist(&mut s);
        }
    }

    /// Saves the shared workbook to the file. False when the session can't go on.
    fn checkpoint(&mut self, nc: &Connection, job: Job) -> bool {
        let before = link(&self.link).checkpoint_etag.clone();
        let bytes = match gridcraft_engine::io::save_bytes(&job.wb, &job.name) {
            Ok(b) => b,
            Err(e) => {
                self.event(Event::Notice(format!("Couldn't save the shared workbook: {e}"), true));
                return true;
            }
        };
        let headers = [("If-Match", format!("\"{before}\"")), ("Content-Type", "application/octet-stream".to_string())];
        let r = match nc.send("PUT", &nc.dav(&self.remote), &headers, bytes) {
            Ok(r) => r,
            Err(_) => return true,
        };
        if r.status == 412 {
            // Someone else in the session saved first (fine), or the file changed outside it.
            let _ = self.sync(nc);
            if link(&self.link).checkpoint_etag != before {
                return true;
            }
            self.event(Event::End(
                "The file was changed outside this editing session in Nextcloud. Open it again to see it; edits since the last save weren't written to it.".into(),
            ));
            return false;
        }
        if !(200..300).contains(&r.status) {
            return true;
        }
        let etag = r.etag.as_deref().unwrap_or("").trim_start_matches("W/").trim_matches('"').to_string();
        let body = json!({"sessionId": self.session_id, "client": self.client, "seq": job.seq, "etag": etag, "sheetIds": job.sheet_ids});
        if nc.app_status("POST", "collab/checkpoint", &body).is_ok_and(|(s, _)| (200..300).contains(&s)) {
            let mut l = link(&self.link);
            l.checkpoint_seq = l.checkpoint_seq.max(job.seq);
            l.checkpoint_etag = etag.clone();
            drop(l);
            self.settle(&etag);
        }
        true
    }
}

/// A 409 that says a newer GridLogic is in the session: rejoining wouldn't help.
fn outdated(body: &Json) -> bool {
    body.get("outdated").and_then(Json::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_read_like_the_page_does() {
        let e = entries(Some(&json!([
            {"seq": 4, "client": 2, "client_seq": 1, "base": 3, "op": "{\"patch\":{}}"},
            {"seq": 5, "client": 3, "client_seq": 1, "base": 4, "op": "not json"},
            {"client": 3}
        ])));
        assert_eq!(e.len(), 2, "an entry without a position is dropped");
        assert_eq!(e[1].op, Op::default(), "an op that doesn't parse is a no-op");
    }

    #[test]
    fn a_newer_protocol_in_the_session_is_told_apart() {
        let newer = (PROTOCOL + 1).to_string();
        assert!(newer_than_mine(Some(&json!(newer))));
        assert!(!newer_than_mine(Some(&json!(PROTOCOL.to_string()))), "the same protocol edits together");
        assert!(!newer_than_mine(Some(&json!(""))), "a session nobody named a protocol for");
        assert!(!newer_than_mine(None), "a server from before protocols");
        assert!(outdated(&json!({"rejoin": true, "outdated": true})));
        assert!(!outdated(&json!({"rejoin": true})), "a lost session is rejoined");
    }

    #[test]
    fn positions_read_and_write_like_the_browser() {
        let p: Position = serde_json::from_str(r#"{"sheet":1,"active":{"row":2,"col":3},"ranges":[]}"#).unwrap();
        assert_eq!((p.sheet, p.active.row, p.active.col), (1, 2, 3));
        assert_eq!(color_for(1), egui::Color32::from_rgb(0xC0, 0x39, 0x2B));
        assert_eq!(color_for(9), color_for(1));
    }
}
