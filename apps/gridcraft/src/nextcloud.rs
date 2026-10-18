//! Nextcloud for the desktop app, as OneDrive is for Excel: sign in once (Nextcloud's Login Flow
//! v2, in the browser; the app password goes to the system keychain), open and save workbooks in
//! Nextcloud from the File view, upload a workbook to share it, and the Share dialog's operations
//! ([`gridcraft_ui_egui::share::Cloud`]) through Nextcloud's sharing API.
//!
//! A workbook in Nextcloud is worked on as a copy in this computer's cache folder, tied to the
//! Nextcloud file: saving the copy (Save, or AutoSave) sends it to Nextcloud, unless someone
//! changed the Nextcloud file in the meantime. Uploaded workbooks are tied the same way.
//!
//! The dialog's requests are queued and picked up by [`Nextcloud::poll`] on the UI thread, which
//! knows the workbook they are about; the talking happens on worker threads, which answer in the
//! dialog's inbox and ask for a repaint. The account and the uploaded files are remembered in
//! `nextcloud.json` in the settings folder; the password never is.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

mod coedit;

use gridcraft_ui_egui::SheetApp;
use gridcraft_ui_egui::share::{Cloud, CloudInbox};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

/// The keychain entry's service; its user is `login@server`.
const KEYRING_SERVICE: &str = "GridLogic Nextcloud";
/// The folder in the user's Nextcloud files that uploaded workbooks go to.
const FOLDER: &str = "GridLogic";
/// How often sign-in asks whether the user has approved it in the browser, and how many times
/// (20 minutes).
const SIGN_IN_POLL: Duration = Duration::from_secs(2);
const SIGN_IN_POLLS: u32 = 600;
/// The largest answer read from Nextcloud.
const MAX_ANSWER_BYTES: u64 = 16 << 20;
/// How often saved files are looked at for changes to send.
const SAVE_CHECK_SECONDS: f64 = 1.0;
/// Nextcloud's share types and file permissions (read; read + update).
const SHARE_TYPES: [(&str, i64); 6] = [("user", 0), ("group", 1), ("link", 3), ("email", 4), ("remote", 6), ("team", 7)];
const VIEW: i64 = 1;
const EDIT: i64 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    /// What the user typed, normalised (`https://cloud.example.com`).
    server: String,
    /// The login name the app password belongs to.
    login: String,
    /// The user id, for WebDAV paths.
    user: String,
}

impl Account {
    fn keychain(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("{}@{}", self.login, self.server)).map_err(|e| format!("The keychain isn't available: {e}"))
    }
    /// How the dialog names the account: `ana on cloud.example.com`.
    fn shown(&self) -> String {
        let host = self.server.split("://").nth(1).unwrap_or(&self.server);
        format!("{} on {host}", self.user)
    }
}

/// A workbook's copy in Nextcloud.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Remote {
    /// Relative to the user's files: `GridLogic/Budget.xlsx`.
    path: String,
    /// The version last written or read here; saving overwrites only that one.
    etag: Option<String>,
    /// The local file's modification time (ns since 1970) when it was last in step with
    /// Nextcloud: a later one is a save still to send, even one made before GridLogic last quit.
    #[serde(default)]
    synced: Option<u128>,
}

/// A file's modification time, in nanoseconds since 1970.
fn mtime(path: &Path) -> Option<u128> {
    let t = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_nanos())
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    account: Option<Account>,
    /// Uploaded workbooks by their path on this computer.
    files: HashMap<String, Remote>,
}

#[derive(Default)]
struct State {
    stored: Stored,
    /// `stored` changed since it was written.
    unsaved: bool,
    /// Uploaded workbooks that have no file on this computer yet, by document uid.
    unsaved_docs: HashMap<u64, Remote>,
    /// The app password, once read from the keychain or signed in.
    password: Option<String>,
    signing_in: bool,
    /// Files being sent to Nextcloud.
    sending: HashSet<String>,
    /// Messages for the user: (title, text, is a problem).
    notices: Vec<(String, String, bool)>,
    /// Where copies of Nextcloud files are kept (none: Nextcloud files can't be opened).
    cache: Option<PathBuf>,
    /// Local files to send as soon as they're saved (Save As to Nextcloud), and to say so.
    send_now: Vec<String>,
    announce: HashSet<String>,
    /// Local files just written from Nextcloud: their new time isn't an edit.
    fresh: Vec<String>,
    /// `nextcloud.json`, written the moment `stored` changes (a change waiting for a frame
    /// would be lost if GridLogic quit first, and the next save would look like a conflict).
    store_file: Option<PathBuf>,
    /// Local files whose Nextcloud file someone else changed: not sent until the user chooses
    /// what to keep ([`resolve`]); and those not yet put to the user.
    conflicted: HashSet<String>,
    conflicts: Vec<(String, String)>,
    /// Sessions joined for files just downloaded, waiting for their workbook to open.
    joined: Vec<(String, coedit::Joined)>,
    /// Files downloaded again to rejoin their session: reopen, then start sharing.
    rejoined: Vec<(String, coedit::Joined)>,
    /// The local copy being co-edited: its saves go to the session, not the save-back.
    shared: Option<String>,
    /// Files to open (from "Open in Desktop App").
    open_now: Vec<String>,
}

/// Records that `stored` changed and writes it out (left for the next frame if that fails).
fn persist(s: &mut State) {
    s.unsaved = true;
    let Some(file) = s.store_file.clone() else { return };
    let Ok(bytes) = serde_json::to_vec_pretty(&s.stored) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&file, bytes) {
        Ok(()) => s.unsaved = false,
        Err(e) => log::warn!("{}: {e}", file.display()),
    }
}

/// The workbook a request is about, as the UI thread saw it.
struct Doc {
    title: String,
    path: Option<String>,
    uid: u64,
    remote: Option<Remote>,
    /// Encodes the workbook as XLSX (on the worker thread).
    encode: Box<dyn FnOnce(&str) -> Result<Vec<u8>, String> + Send>,
}

pub struct Nextcloud {
    requests: Arc<Mutex<Vec<(u64, String, Json)>>>,
    answers: CloudInbox,
    state: Arc<Mutex<State>>,
    /// `nextcloud.json`, when preferences are kept.
    file: Option<PathBuf>,
    /// When local files were last looked at, and their modification times then.
    checked_at: f64,
    modified: HashMap<String, Option<SystemTime>>,
    /// Each open workbook's saved state (by document uid) when last seen: a change means it was
    /// just saved, so its file is looked at at once (an idle window draws no frames to wait for).
    saved_seen: HashMap<u64, usize>,
    open_url: fn(&str),
    /// The workbook being co-edited live, if any.
    coedit: Option<coedit::Coedit>,
    /// Joined sessions waiting for their workbook to open, since when.
    pending_joins: Vec<(String, coedit::Joined, f64)>,
    /// A `gridlogic://open` link waiting to be followed: (server, path, sign-in asked for).
    link: Option<(String, String, bool)>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

impl Nextcloud {
    /// `file` is where the account and the uploaded files are remembered, `cache` where copies of
    /// Nextcloud files are kept; `open_url` opens the sign-in page in the browser.
    pub fn new(file: Option<PathBuf>, cache: Option<PathBuf>, open_url: fn(&str)) -> Nextcloud {
        let stored = file.as_deref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Nextcloud {
            requests: Arc::default(),
            answers: CloudInbox::default(),
            state: Arc::new(Mutex::new(State { stored, cache, store_file: file.clone(), ..State::default() })),
            file,
            checked_at: f64::NEG_INFINITY,
            modified: HashMap::new(),
            saved_seen: HashMap::new(),
            open_url,
            coedit: None,
            pending_joins: Vec::new(),
            link: None,
        }
    }

    /// The cloud of the Share dialog and the File view: requests wait for [`Nextcloud::poll`].
    pub fn cloud(&self) -> Cloud {
        let requests = self.requests.clone();
        let state = self.state.clone();
        Cloud {
            request: Box::new(move |id, op, params| {
                requests.lock().unwrap_or_else(|e| e.into_inner()).push((id, op.to_string(), params));
            }),
            inbox: self.answers.clone(),
            files: lock(&self.state).cache.is_some(),
            location: Some(Box::new(move |local: &str| {
                let s = lock(&state);
                tie_key(&s.stored.files, local).and_then(|k| s.stored.files.get(&k)).map(|r| r.path.clone())
            })),
        }
    }

    /// Every frame: starts the dialog's requests, sends saved workbooks to Nextcloud, and shows
    /// what came of it.
    pub fn poll(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let requests = std::mem::take(&mut *self.requests.lock().unwrap_or_else(|e| e.into_inner()));
        for (id, op, params) in requests {
            let doc = self.doc(app);
            let state = self.state.clone();
            let answers = self.answers.clone();
            let ctx = ctx.clone();
            let open_url = self.open_url;
            let spawned = std::thread::Builder::new().name("nextcloud".into()).spawn(move || {
                let answer = run(&state, &op, &params, doc, open_url);
                answers.lock().unwrap_or_else(|e| e.into_inner()).push((id, answer));
                ctx.request_repaint();
            });
            if let Err(e) = spawned {
                self.answers.lock().unwrap_or_else(|e| e.into_inner()).push((id, Err(format!("Couldn't start: {e}"))));
            }
        }
        {
            let mut s = lock(&self.state);
            for path in std::mem::take(&mut s.send_now) {
                // Never looked at: sent once saved.
                self.modified.insert(path, None);
            }
            for path in std::mem::take(&mut s.fresh) {
                // Looked at afresh: the download isn't an edit.
                self.modified.remove(&path);
            }
        }
        self.follow_link(app, ctx);
        self.poll_coedit(app, ctx);
        let mut just_saved = false;
        for d in app.session.documents() {
            let stamp = std::sync::Arc::as_ptr(&d.saved) as usize;
            just_saved |= self.saved_seen.insert(d.uid, stamp).is_some_and(|seen| seen != stamp);
        }
        let now = ctx.input(|i| i.time);
        if just_saved || now - self.checked_at >= SAVE_CHECK_SECONDS || !self.modified.values().all(Option::is_some) {
            self.checked_at = now;
            self.send_saved(app, ctx);
        }
        let (notices, unsaved) = {
            let mut s = lock(&self.state);
            for (local, name) in std::mem::take(&mut s.conflicts) {
                app.cloud_files.conflicts.push(gridcraft_ui_egui::cloud_files::Conflict { local, name });
            }
            let unsaved = std::mem::take(&mut s.unsaved).then(|| serde_json::to_vec_pretty(&s.stored).ok()).flatten();
            (std::mem::take(&mut s.notices), unsaved)
        };
        if let (Some(file), Some(bytes)) = (&self.file, unsaved) {
            if let Some(dir) = file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(file, bytes) {
                log::warn!("{}: {e}", file.display());
            }
        }
        for (title, text, problem) in notices {
            if problem {
                app.message = Some((title, text));
            } else {
                app.toast = Some((text, gridcraft_ui_egui::now_ms()));
            }
        }
    }

    /// Live co-editing: starts sessions joined for files just opened, and runs the shared one.
    fn poll_coedit(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let (rejoined, joined, open_now) = {
            let mut s = lock(&self.state);
            (std::mem::take(&mut s.rejoined), std::mem::take(&mut s.joined), std::mem::take(&mut s.open_now))
        };
        for local in open_now {
            app.open_path(&local);
        }
        for (local, j) in rejoined {
            // The copy here is behind the session: open the file as it is now in its place.
            if let Some(i) = app.session.documents().iter().position(|d| d.path.as_deref().is_some_and(|p| same_path(p, &local))) {
                app.session.close_document(i);
            }
            app.open_path(&local);
            self.pending_joins.push((local, j, now));
        }
        self.pending_joins.extend(joined.into_iter().map(|(l, j)| (l, j, now)));
        for (local, j, since) in std::mem::take(&mut self.pending_joins) {
            let open = app.session.active().and_then(|d| d.path.clone()).is_some_and(|p| same_path(&p, &local));
            if self.coedit.is_some() || (!open && now - since > 10.0) {
                // One shared workbook at a time; a workbook that never opened isn't shared.
                continue;
            }
            if !open {
                self.pending_joins.push((local, j, since));
                continue;
            }
            match coedit::Coedit::start(app, j, local.clone(), self.state.clone(), ctx) {
                Ok(c) => {
                    lock(&self.state).shared = Some(local);
                    self.coedit = Some(c);
                }
                Err(e) => {
                    log::warn!("co-editing {local}: {e}");
                    app.toast = Some(("This workbook opened without live co-editing.".into(), gridcraft_ui_egui::now_ms()));
                }
            }
        }
        let Some(c) = self.coedit.as_mut() else { return };
        if app.session.collab_pending() {
            // An edit not sent yet (made after this frame's poll): come back for it, even in an
            // idle window.
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        match c.poll(app, now) {
            coedit::Status::Going => {}
            coedit::Status::Over => {
                c.stop(app);
                self.coedit = None;
                lock(&self.state).shared = None;
            }
            coedit::Status::Rejoin => {
                c.stop(app);
                let (local, remote) = (c.local.clone(), c.remote.clone());
                self.coedit = None;
                lock(&self.state).shared = None;
                app.toast = Some(("Reconnecting to the shared workbook…".into(), gridcraft_ui_egui::now_ms()));
                let (state, ctx) = (self.state.clone(), ctx.clone());
                let _ = std::thread::Builder::new().name("nextcloud rejoin".into()).spawn(move || {
                    match download_and_join(&state, &remote, true) {
                        Ok((_, Some(j))) => lock(&state).rejoined.push((local, j)),
                        Ok((_, None)) => {}
                        Err(e) => lock(&state).notices.push(("Co-editing stopped".into(), e, true)),
                    }
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Before quitting: leave the shared session.
    pub fn shutdown(&mut self, app: &mut SheetApp) {
        if let Some(c) = self.coedit.take() {
            c.stop(app);
        }
    }

    /// "Open in Desktop App" from the browser: `gridlogic://open?server=…&path=…`. The file opens
    /// once signed in to that server (File › Account is shown, with its address, until then).
    pub fn open_link(&mut self, url: &str) -> bool {
        let Some(query) = url.strip_prefix("gridlogic://open?").or_else(|| url.strip_prefix("gridlogic://open/?")) else { return false };
        let mut server = String::new();
        let mut path = String::new();
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "server" => server = decode(&v.replace('+', " ")),
                "path" => path = decode(&v.replace('+', " ")),
                _ => {}
            }
        }
        let Ok(server) = normalise_server(&server) else { return false };
        if path.trim_matches('/').is_empty() {
            return false;
        }
        self.link = Some((server, path.trim_matches('/').to_string(), false));
        true
    }

    fn follow_link(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let Some((server, path, asked)) = self.link.clone() else { return };
        if self.server().is_some_and(|s| s.eq_ignore_ascii_case(&server)) {
            self.link = None;
            self.open_remote(ctx, path);
        } else if !asked {
            // Signed out, or into another server: sign in to this one first (once; the file
            // opens when the sign-in is done).
            self.link = Some((server.clone(), path.clone(), true));
            app.cloud_files.server = server.trim_start_matches("https://").to_string();
            app.open_backstage(gridcraft_ui_egui::backstage::Page::Account);
            app.toast = Some((format!("Sign in to {server} to open {path}."), gridcraft_ui_egui::now_ms()));
        }
    }

    /// Opens `remote` in this server's Nextcloud ("Open in Desktop App"): downloaded, joined, then
    /// opened by the next frame.
    fn open_remote(&self, ctx: &egui::Context, remote: String) {
        let (state, ctx) = (self.state.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("nextcloud open".into()).spawn(move || {
            match open_file(&state, &remote, &[]) {
                Ok(r) => {
                    if let Some(local) = r.get("localPath").and_then(Json::as_str) {
                        lock(&state).open_now.push(local.to_string());
                    }
                }
                Err(e) => lock(&state).notices.push(("Couldn't open from Nextcloud".into(), e, true)),
            }
            ctx.request_repaint();
        });
    }

    /// The server signed in to, if any (`https://cloud.example.com`).
    fn server(&self) -> Option<String> {
        lock(&self.state).stored.account.as_ref().map(|a| a.server.clone())
    }

    /// The active workbook, for a request about it.
    fn doc(&self, app: &SheetApp) -> Option<Doc> {
        let d = app.session.active()?;
        let s = lock(&self.state);
        let tied = d.path.as_deref().and_then(|p| tie_key(&s.stored.files, p));
        let remote = tied.and_then(|k| s.stored.files.get(&k)).or_else(|| s.unsaved_docs.get(&d.uid)).cloned();
        let wb = d.wb.clone();
        Some(Doc {
            title: d.display_title(),
            path: d.path.clone(),
            uid: d.uid,
            remote,
            encode: Box::new(move |name| gridcraft_engine::io::save_bytes(&wb, name).map_err(|e| e.to_string())),
        })
    }

    /// Sends uploaded workbooks saved on this computer since they were last looked at.
    fn send_saved(&mut self, app: &SheetApp, ctx: &egui::Context) {
        let mut s = lock(&self.state);
        if s.stored.account.is_none() {
            return;
        }
        // An uploaded workbook saved for the first time: from now on it's its file.
        for d in app.session.documents() {
            if let Some(path) = d.path.as_ref().filter(|p| is_workbook(p))
                && let Some(remote) = s.unsaved_docs.remove(&d.uid)
            {
                s.stored.files.insert(path.clone(), remote);
                persist(&mut s);
                // Never looked at: send it now.
                self.modified.insert(path.clone(), None);
            }
        }
        for d in app.session.documents() {
            // The record's own spelling of the path, whatever spelling the workbook was opened by.
            let Some(path) = d.path.as_deref().and_then(|p| tie_key(&s.stored.files, p)) else { continue };
            let path = &path;
            let Some(remote) = s.stored.files.get(path).cloned() else { continue };
            if s.shared.as_deref().is_some_and(|p| same_path(p, path)) {
                // Co-edited: the session saves the file; saves here only keep the copy, so its
                // record stays in step (opening it again fetches Nextcloud's version).
                let now = mtime(Path::new(path));
                if remote.synced != now
                    && let Some(r) = s.stored.files.get_mut(path)
                {
                    r.synced = now;
                    persist(&mut s);
                }
                self.modified.remove(path);
                continue;
            }
            let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
            let Some(seen) = self.modified.get(path).copied() else {
                // First look since the app started (or since it came from Nextcloud): a file saved
                // after it was last in step is sent; otherwise there's nothing to send.
                let behind = remote.synced.is_some_and(|synced| mtime(Path::new(path)) != Some(synced));
                self.modified.insert(path.clone(), if behind { None } else { modified });
                if behind {
                    ctx.request_repaint();
                }
                continue;
            };
            if modified.is_none()
                || modified == seen
                || d.is_dirty()
                || s.sending.contains(path)
                || s.conflicted.contains(path)
                || !same_kind(path, &remote.path)
            {
                continue;
            }
            self.modified.insert(path.clone(), modified);
            s.sending.insert(path.clone());
            let (state, ctx, path) = (self.state.clone(), ctx.clone(), path.clone());
            let spawned = std::thread::Builder::new().name("nextcloud save".into()).spawn(move || {
                send_file(&state, &path, &remote);
                lock(&state).sending.remove(&path);
                ctx.request_repaint();
            });
            if spawned.is_err() {
                s.sending.clear();
            }
        }
    }
}

/// Whether two spellings name the same file: on Windows, `/` and `\` are the same separator
/// and case doesn't matter.
fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.replace('/', "\\").to_lowercase() == b.replace('/', "\\").to_lowercase() } else { a == b }
}

/// The key a local file is recorded under, however `path` spells it.
fn tie_key(files: &HashMap<String, Remote>, path: &str) -> Option<String> {
    if files.contains_key(path) {
        return Some(path.to_string());
    }
    files.keys().find(|k| same_path(k, path)).cloned()
}

fn is_workbook(path: &str) -> bool {
    matches!(extension(path).as_str(), "xlsx" | "xlsm")
}

/// What the File view lists and opens from Nextcloud.
const OPENS: [&str; 7] = ["xlsx", "xlsm", "csv", "tsv", "ods", "xls", "xlsb"];

fn extension(path: &str) -> String {
    Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// A local file is only ever sent to a Nextcloud file of the same type, one GridLogic saves.
fn same_kind(local: &str, remote: &str) -> bool {
    matches!(extension(local).as_str(), "xlsx" | "xlsm" | "csv" | "tsv") && extension(local) == extension(remote)
}

/// Sends a saved file to its copy in Nextcloud, overwriting only the version last seen here.
fn send_file(state: &Mutex<State>, path: &str, remote: &Remote) {
    let name = remote.path.rsplit('/').next().unwrap_or(&remote.path).to_string();
    let mut conflict = false;
    let result = (|| {
        let nc = Connection::open(state)?;
        let sent_time = mtime(Path::new(path));
        let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {path}: {e}"))?;
        let mut headers = vec![("Content-Type", "application/octet-stream".to_string())];
        // Only over the version last seen here; a new file only if nothing's there yet.
        match &remote.etag {
            Some(etag) => headers.push(("If-Match", etag.clone())),
            None => headers.push(("If-None-Match", "*".to_string())),
        }
        let r = nc.send("PUT", &nc.dav(&remote.path), &headers, bytes.clone())?;
        match r.status {
            200..=299 => Ok((r.etag, sent_time)),
            412 if remote.etag.is_none() => {
                Err(format!("There's already a file called {name} there in Nextcloud. Your workbook is saved on this computer."))
            }
            412 => {
                // Nextcloud has another version. If it's exactly this file (sent before GridLogic
                // could note it), there's nothing to bring together.
                let theirs = nc.send("GET", &nc.dav(&remote.path), &[], Vec::new())?;
                if theirs.status == 200 && theirs.body == bytes {
                    return Ok((theirs.etag, sent_time));
                }
                conflict = true;
                Err(String::new())
            }
            404 => Err(format!("{name} isn't in Nextcloud any more. Your changes are saved on this computer.")),
            s => Err(format!("Nextcloud answered {s} while saving {name}. Your changes are saved on this computer.")),
        }
    })();
    let mut s = lock(state);
    match result {
        Ok((etag, sent_time)) => {
            if let Some(r) = s.stored.files.get_mut(path) {
                r.etag = etag;
                r.synced = sent_time;
                persist(&mut s);
            }
            // Said after Save As; AutoSave's saves go quietly.
            if s.announce.remove(path) {
                s.notices.push((String::new(), format!("Saved to Nextcloud: {}", remote.path), false));
            }
        }
        Err(_) if conflict => {
            if s.conflicted.insert(path.to_string()) {
                s.conflicts.push((path.to_string(), name));
            }
        }
        Err(e) => s.notices.push(("Couldn't save to Nextcloud".into(), e, true)),
    }
}

/// The user's choice for a conflicted file (`files.resolve`):
///
/// - `keepBoth`: their version stays; this one goes next to it under a new name, which the
///   workbook continues as (`{localPath, saveAs: true, name}`: save the workbook there).
/// - `mine`: this version replaces theirs (`{name}`).
/// - `theirs`: this computer's changes go; their version replaces the copy here
///   (`{localPath, reload: true}`: open it again).
fn resolve(state: &Mutex<State>, local: &str, choice: &str) -> Result<Json, String> {
    let local = &tie_key(&lock(state).stored.files, local).ok_or("That workbook isn't tied to Nextcloud any more.")?;
    let remote = lock(state).stored.files.get(local).cloned().ok_or("That workbook isn't tied to Nextcloud any more.")?;
    let name = remote.path.rsplit('/').next().unwrap_or(&remote.path).to_string();
    let nc = Connection::open(state)?;
    let answer = match choice {
        "mine" => {
            let sent_time = mtime(Path::new(local));
            let bytes = std::fs::read(local).map_err(|e| format!("Couldn't read {local}: {e}"))?;
            let r = nc.send("PUT", &nc.dav(&remote.path), &[("Content-Type", "application/octet-stream".to_string())], bytes)?;
            if !(200..300).contains(&r.status) {
                return Err(format!("Nextcloud answered {} while saving {name}.", r.status));
            }
            let mut s = lock(state);
            if let Some(t) = s.stored.files.get_mut(local) {
                t.etag = r.etag;
                t.synced = sent_time;
            }
            persist(&mut s);
            json!({"name": name})
        }
        "theirs" => {
            let r = nc.send("GET", &nc.dav(&remote.path), &[], Vec::new())?;
            if r.status != 200 {
                return Err(format!("Nextcloud answered {} while fetching {name}.", r.status));
            }
            std::fs::write(local, &r.body).map_err(|e| format!("Couldn't write {local}: {e}"))?;
            let mut s = lock(state);
            if let Some(t) = s.stored.files.get_mut(local) {
                t.etag = r.etag;
                t.synced = mtime(Path::new(local));
            }
            s.fresh.push(local.to_string());
            persist(&mut s);
            json!({"localPath": local, "reload": true})
        }
        "keepBoth" => {
            let (folder, file) = remote.path.rsplit_once('/').unwrap_or(("", &remote.path));
            let (stem, ext) = file.rsplit_once('.').unwrap_or((file, "xlsx"));
            let mut target = None;
            for n in 1..=50 {
                let candidate = if n == 1 { format!("{stem} (copy).{ext}") } else { format!("{stem} (copy {n}).{ext}") };
                let there = save_as_target(state, folder, &candidate, false)?;
                if there.get("exists").and_then(Json::as_bool) != Some(true) {
                    target = Some(there);
                    break;
                }
            }
            let mut there = target.ok_or_else(|| format!("{folder} in Nextcloud already has copies of {name}; rename this workbook first."))?;
            // The copy here no longer stands for their file: the next open fetches theirs.
            let mut s = lock(state);
            s.stored.files.remove(local);
            persist(&mut s);
            drop(s);
            let _ = std::fs::remove_file(local);
            there["saveAs"] = json!(true);
            there
        }
        other => return Err(format!("Unknown choice “{other}”.")),
    };
    lock(state).conflicted.remove(local);
    Ok(answer)
}

/// Carries out one of the dialog's operations.
fn run(state: &Mutex<State>, op: &str, params: &Json, doc: Option<Doc>, open_url: fn(&str)) -> Result<Json, String> {
    let text = |k: &str| params.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    match op {
        "account.signIn" => {
            sign_in(state, &text("server"), open_url)?;
            return info(state, doc.as_ref());
        }
        "account.info" => return Ok(account_info(state)),
        "account.signOut" => {
            sign_out(state);
            return Ok(account_info(state));
        }
        "files.list" => return list_files(state, &text("path")),
        "files.open" => {
            let open: Vec<String> =
                params.get("openPaths").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).map(str::to_string).collect();
            return open_file(state, &text("path"), &open);
        }
        "files.resolve" => return resolve(state, &text("localPath"), &text("choice")),
        "files.saveAs" => {
            let replace = params.get("replace").and_then(Json::as_bool).unwrap_or(false);
            return save_as_target(state, &text("folder"), &text("name"), replace);
        }
        _ => {}
    }
    let doc = doc.ok_or("There's no workbook open.")?;
    match op {
        "share.info" => info(state, Some(&doc)),
        "share.upload" => {
            let name = upload(state, doc)?;
            Ok(info_after_upload(state, &name))
        }
        _ => {
            let remote = doc.remote.as_ref().ok_or("Upload the workbook to Nextcloud first.")?;
            let nc = Connection::open(state)?;
            share_op(&nc, op, params, &format!("/{}", remote.path))
        }
    }
}

/// `{signedIn, account?, server?}` for the File view.
fn account_info(state: &Mutex<State>) -> Json {
    let account = lock(state).stored.account.clone();
    json!({
        "signedIn": account.is_some(),
        "account": account.as_ref().map(Account::shown),
        "server": account.as_ref().map(|a| a.server.clone()),
    })
}

/// Forgets the account, its password (from the keychain too) and which files were Nextcloud's.
fn sign_out(state: &Mutex<State>) {
    let mut s = lock(state);
    if let Some(old) = s.stored.account.take()
        && let Ok(entry) = old.keychain()
    {
        let _ = entry.delete_credential();
    }
    s.password = None;
    s.stored.files.clear();
    s.unsaved_docs.clear();
    persist(&mut s);
}

/// A folder in Nextcloud: `{path, items: [{name, path, folder, size}]}`, folders first, then the
/// spreadsheets GridLogic opens.
fn list_files(state: &Mutex<State>, path: &str) -> Result<Json, String> {
    let nc = Connection::open(state)?;
    let path = path.trim_matches('/');
    let body = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;
    let headers = [("Depth", "1".to_string()), ("Content-Type", "application/xml".to_string())];
    let r = nc.send("PROPFIND", &nc.dav(path), &headers, body.as_bytes().to_vec())?;
    match r.status {
        207 => {}
        404 => return Err("That folder isn't in Nextcloud any more.".into()),
        s => return Err(format!("Nextcloud answered {s} while listing the folder.")),
    }
    let base = url_path(&nc.dav(""));
    let mut items: Vec<(bool, String, Json)> = Vec::new();
    for entry in dav_entries(&r.body) {
        let Some(rel) = decode(&entry.href).strip_prefix(&decode(&base)).map(|p| p.trim_matches('/').to_string()) else { continue };
        if rel == path || rel.is_empty() {
            continue;
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        if name.starts_with('.') || (!entry.folder && !OPENS.contains(&extension(&name).as_str())) {
            continue;
        }
        items.push((entry.folder, name.to_lowercase(), json!({"name": name, "path": rel, "folder": entry.folder, "size": entry.size})));
    }
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(json!({"path": path, "items": items.into_iter().map(|(_, _, j)| j).collect::<Vec<_>>()}))
}

struct DavEntry {
    href: String,
    folder: bool,
    size: u64,
}

/// The entries of a PROPFIND answer.
fn dav_entries(xml: &[u8]) -> Vec<DavEntry> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut out = Vec::new();
    let mut current: Option<DavEntry> = None;
    let mut field = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match name.as_str() {
                    "response" => current = Some(DavEntry { href: String::new(), folder: false, size: 0 }),
                    "collection" => {
                        if let Some(c) = current.as_mut() {
                            c.folder = true;
                        }
                    }
                    _ => {}
                }
                field = name;
            }
            Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"collection"
                    && let Some(c) = current.as_mut()
                {
                    c.folder = true;
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.decode().map(|t| t.to_string()).unwrap_or_default();
                if let Some(c) = current.as_mut() {
                    match field.as_str() {
                        "href" => c.href.push_str(&text),
                        "getcontentlength" => c.size = text.trim().parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"response"
                    && let Some(c) = current.take()
                {
                    out.push(c);
                }
                field.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// The path part of a URL (`https://host/a/b` → `/a/b`).
fn url_path(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.find('/').map(|i| rest[i..].to_string()).unwrap_or_else(|| "/".into())
}

/// Undoes percent-encoding (bytes that don't make UTF-8 become U+FFFD).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where the copy of a Nextcloud file lives on this computer: the cache folder, then the server,
/// the user and the file's own path, each name made safe for any file system.
fn cache_path(state: &Mutex<State>, remote: &str) -> Result<PathBuf, String> {
    let s = lock(state);
    let cache = s.cache.clone().ok_or("Nextcloud files can't be opened here.")?;
    let account = s.stored.account.clone().ok_or("Sign in to Nextcloud first.")?;
    let host = account.server.split("://").nth(1).unwrap_or(&account.server).to_string();
    let mut path = cache.join(safe_name(&host)).join(safe_name(&account.user));
    for part in remote.split('/').filter(|p| !p.is_empty()) {
        path.push(safe_name(part));
    }
    Ok(path)
}

/// A file or folder name any system accepts: no separators, reserved characters, trailing dots
/// or spaces, and never `.` or `..`.
fn safe_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." { "_".into() } else { cleaned }
}

/// Opens a Nextcloud file: downloads it to its copy here and ties the two. `{localPath}`, with
/// `alreadyOpen` when the copy is open already (it's left as it is).
fn open_file(state: &Mutex<State>, remote: &str, open: &[String]) -> Result<Json, String> {
    let remote = remote.trim_matches('/').to_string();
    let local = cache_path(state, &remote)?;
    let local_text = local.to_string_lossy().to_string();
    if open.contains(&local_text) {
        return Ok(json!({"localPath": local_text, "alreadyOpen": true}));
    }
    // A copy with changes not yet sent (saved just before quitting, or offline) isn't
    // overwritten: it opens as it is and is sent (over the version it came from only).
    {
        let mut s = lock(state);
        let behind = s.stored.files.get(&local_text).filter(|r| r.path == remote).is_some_and(|r| local.exists() && r.synced != mtime(&local));
        if behind {
            s.send_now.push(local_text.clone());
            return Ok(json!({"localPath": local_text, "unsent": true}));
        }
    }
    let (_, joined) = download_and_join(state, &remote, false)?;
    let shared = joined.is_some();
    if let Some(j) = joined {
        lock(state).joined.push((local_text.clone(), j));
    }
    Ok(json!({"localPath": local_text, "shared": shared}))
}

/// Downloads a Nextcloud file to its copy here, tied to it, and joins its co-editing session
/// pinned to the version downloaded (unless a workbook is co-edited already and this isn't a
/// rejoin). Without the GridLogic app on the server, the file opens unshared.
fn download_and_join(state: &Mutex<State>, remote: &str, rejoin: bool) -> Result<(PathBuf, Option<coedit::Joined>), String> {
    let local = cache_path(state, remote)?;
    let local_text = local.to_string_lossy().to_string();
    let nc = Connection::open(state)?;
    let share = rejoin || lock(state).shared.is_none();
    let mut file_id = None;
    for attempt in 0..6 {
        let r = nc.send("GET", &nc.dav(remote), &[], Vec::new())?;
        match r.status {
            200 => {}
            404 => return Err("That file isn't in Nextcloud any more.".into()),
            s => return Err(format!("Nextcloud answered {s} while opening the file.")),
        }
        if let Some(dir) = local.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
        }
        std::fs::write(&local, &r.body).map_err(|e| format!("Couldn't write {}: {e}", local.display()))?;
        {
            let mut s = lock(state);
            s.stored.files.insert(local_text.clone(), Remote { path: remote.to_string(), etag: r.etag.clone(), synced: mtime(&local) });
            s.fresh.push(local_text.clone());
            persist(&mut s);
        }
        let etag = r.etag.clone().unwrap_or_default();
        if !share || etag.is_empty() {
            return Ok((local, None));
        }
        if file_id.is_none() {
            file_id = nc.file_id(remote).ok();
        }
        let Some(id) = file_id else { return Ok((local, None)) };
        match coedit::join(&nc, id, &etag, remote) {
            Ok(Some(j)) => return Ok((local, Some(j))),
            // Saved for the session since this copy was fetched: fetch it again.
            Ok(None) => std::thread::sleep(Duration::from_millis(400 << attempt)),
            Err(e) => {
                log::warn!("co-editing {remote}: {e}");
                if e == coedit::OUTDATED {
                    lock(state).notices.push(("Opened without editing together".into(), e, true));
                }
                return Ok((local, None));
            }
        }
    }
    Ok((local, None))
}

/// Save As in Nextcloud: where to save the workbook here (`{localPath}`), tied to `folder/name`
/// in Nextcloud and sent as soon as it's saved. `{exists: true}` when there's a file of that name
/// already and `replace` wasn't asked for.
fn save_as_target(state: &Mutex<State>, folder: &str, name: &str, replace: bool) -> Result<Json, String> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err("Type a name for the workbook (no slashes).".into());
    }
    let name = if is_workbook(name) { name.to_string() } else { format!("{name}.xlsx") };
    let folder = folder.trim_matches('/');
    let remote = if folder.is_empty() { name.clone() } else { format!("{folder}/{name}") };
    let nc = Connection::open(state)?;
    let there = nc.send("HEAD", &nc.dav(&remote), &[], Vec::new())?;
    let etag = match there.status {
        200 if !replace => return Ok(json!({"exists": true, "name": name})),
        200 => there.etag,
        404 => None,
        s => return Err(format!("Nextcloud answered {s} while checking the folder.")),
    };
    let local = cache_path(state, &remote)?;
    if let Some(dir) = local.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
    }
    let local_text = local.to_string_lossy().to_string();
    let mut s = lock(state);
    s.stored.files.insert(local_text.clone(), Remote { path: remote, etag, synced: None });
    s.send_now.push(local_text.clone());
    s.announce.insert(local_text.clone());
    persist(&mut s);
    Ok(json!({"localPath": local_text, "name": name}))
}

/// `{name, signedIn, inCloud, canShare, account?, server?}` for the dialog.
fn info(state: &Mutex<State>, doc: Option<&Doc>) -> Result<Json, String> {
    let account = lock(state).stored.account.clone();
    let name = doc.map(|d| d.title.clone()).unwrap_or_default();
    let remote = doc.and_then(|d| d.remote.clone());
    let can_share = match (&account, &remote) {
        (Some(_), Some(remote)) => {
            let nc = Connection::open(state)?;
            nc.can_share(&remote.path)?
        }
        _ => true,
    };
    Ok(json!({
        "name": remote.as_ref().and_then(|r| r.path.rsplit('/').next()).map(str::to_string).unwrap_or(name),
        "signedIn": account.is_some(),
        "inCloud": account.is_some() && remote.is_some(),
        "canShare": can_share,
        "account": account.as_ref().map(Account::shown),
        "server": account.as_ref().map(|a| a.server.clone()),
    }))
}

fn info_after_upload(state: &Mutex<State>, name: &str) -> Json {
    let s = lock(state);
    json!({
        "name": name,
        "signedIn": true,
        "inCloud": true,
        "canShare": true,
        "account": s.stored.account.as_ref().map(Account::shown),
    })
}

/// `https://` unless the server is this computer; no trailing slash or `index.php`.
fn normalise_server(typed: &str) -> Result<String, String> {
    let t = typed.trim().trim_end_matches('/');
    let t = t.strip_suffix("/index.php").unwrap_or(t);
    if t.is_empty() {
        return Err("Type your Nextcloud server's address.".into());
    }
    let with_scheme = if t.starts_with("http://") || t.starts_with("https://") {
        t.to_string()
    } else {
        let host = t.split(['/', ':']).next().unwrap_or("");
        let local = host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host.ends_with(".localhost");
        format!("{}://{t}", if local { "http" } else { "https" })
    };
    let rest = with_scheme.split("://").nth(1).unwrap_or("");
    if rest.is_empty() || rest.contains(char::is_whitespace) || rest.contains(['?', '#', '@']) {
        return Err(format!("“{typed}” isn't a server address."));
    }
    Ok(with_scheme)
}

/// Login Flow v2: the user approves GridLogic in the browser; Nextcloud hands over an app
/// password, which goes to the keychain.
fn sign_in(state: &Mutex<State>, typed: &str, open_url: fn(&str)) -> Result<(), String> {
    let server = normalise_server(typed)?;
    {
        let mut s = lock(state);
        if s.signing_in {
            return Err("Signing in already: finish in your browser.".into());
        }
        s.signing_in = true;
    }
    let result = sign_in_flow(&server, open_url);
    let mut s = lock(state);
    s.signing_in = false;
    let (account, password) = result?;
    account.keychain()?.set_password(&password).map_err(|e| format!("Couldn't keep the password in the keychain: {e}"))?;
    if let Some(old) = s.stored.account.replace(account.clone()) {
        // Another account: the old one's password goes, and so do its files' connections.
        if old.login != account.login
            && let Ok(entry) = old.keychain()
        {
            let _ = entry.delete_credential();
        }
        if old.server != account.server || old.user != account.user {
            s.unsaved_docs.clear();
            s.stored.files.clear();
        }
    }
    s.password = Some(password);
    persist(&mut s);
    Ok(())
}

fn sign_in_flow(server: &str, open_url: fn(&str)) -> Result<(Account, String), String> {
    let http = agent(Duration::from_secs(30));
    let start = call(&http, "POST", &format!("{server}/index.php/login/v2"), &[], Vec::new())?;
    if start.status != 200 {
        return Err(format!("{server} didn't answer as a Nextcloud server ({}).", start.status));
    }
    let start = start.json()?;
    let login = start.get("login").and_then(Json::as_str).ok_or("Nextcloud didn't offer a sign-in page.")?;
    let token = start.pointer("/poll/token").and_then(Json::as_str).ok_or("Nextcloud didn't offer a sign-in page.")?;
    let endpoint = start.pointer("/poll/endpoint").and_then(Json::as_str).ok_or("Nextcloud didn't offer a sign-in page.")?;
    if !(login.starts_with("https://") || login.starts_with("http://")) || !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
        return Err("Nextcloud's sign-in page has an unexpected address.".into());
    }
    open_url(login);
    let body = format!("token={}", encode(token)).into_bytes();
    let mut polls = 0;
    let done = loop {
        polls += 1;
        if polls > SIGN_IN_POLLS {
            return Err("Signing in took too long. Try again.".into());
        }
        std::thread::sleep(SIGN_IN_POLL);
        let r = call(&http, "POST", endpoint, &[("Content-Type", "application/x-www-form-urlencoded".into())], body.clone())?;
        if r.status == 200 {
            break r.json()?;
        }
        if r.status != 404 {
            return Err(format!("Nextcloud answered {} while signing in.", r.status));
        }
    };
    let login_name = done.get("loginName").and_then(Json::as_str).ok_or("Nextcloud didn't finish signing in.")?.to_string();
    let password = done.get("appPassword").and_then(Json::as_str).ok_or("Nextcloud didn't finish signing in.")?.to_string();
    // Everything goes to the server the user typed, whatever address Nextcloud calls itself.
    let nc = Connection { http, server: server.to_string(), user: String::new(), auth: basic(&login_name, &password) };
    let me = nc.ocs("GET", "cloud/user", &[], None)?;
    let user = me.get("id").and_then(Json::as_str).filter(|u| !u.is_empty()).ok_or("Nextcloud didn't say who you are.")?.to_string();
    Ok((Account { server: server.to_string(), login: login_name, user }, password))
}

/// Puts the workbook in the GridLogic folder under a name that's free there; its name there.
fn upload(state: &Mutex<State>, doc: Doc) -> Result<String, String> {
    let nc = Connection::open(state)?;
    let stem = Path::new(&doc.title).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("Book1").to_string();
    let ext = doc.path.as_deref().filter(|p| is_workbook(p)).map(extension).unwrap_or_else(|| "xlsx".into());
    let bytes = (doc.encode)(&format!("{stem}.{ext}"))?;
    let folder = nc.send("MKCOL", &nc.dav(FOLDER), &[], Vec::new())?;
    if !matches!(folder.status, 201 | 405) {
        return Err(format!("Couldn't make the {FOLDER} folder in Nextcloud ({}).", folder.status));
    }
    for n in 1..=50 {
        let name = if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        let path = format!("{FOLDER}/{name}");
        let headers = [("If-None-Match", "*".to_string()), ("Content-Type", "application/octet-stream".to_string())];
        let r = nc.send("PUT", &nc.dav(&path), &headers, bytes.clone())?;
        match r.status {
            412 => continue,
            200..=299 => {
                let synced = doc.path.as_deref().filter(|p| is_workbook(p)).and_then(|p| mtime(Path::new(p)));
                let remote = Remote { path, etag: r.etag, synced };
                let mut s = lock(state);
                match doc.path.filter(|p| is_workbook(p)) {
                    Some(local) => {
                        s.stored.files.insert(local, remote);
                        persist(&mut s);
                    }
                    None => {
                        s.unsaved_docs.insert(doc.uid, remote);
                    }
                }
                return Ok(name);
            }
            status => return Err(format!("Nextcloud answered {status} while uploading.")),
        }
    }
    Err(format!("{FOLDER} in Nextcloud already has workbooks called {stem}; rename this one first."))
}

/// The sharing operations, as `gridcraft-nextcloud/src/share.js` carries them out in the browser.
fn share_op(nc: &Connection, op: &str, p: &Json, path: &str) -> Result<Json, String> {
    let s = |k: &str| p.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let edit = |k: &str| if p.get(k).and_then(Json::as_bool).unwrap_or(false) { EDIT } else { VIEW };
    let id = || match p.get("id") {
        Some(Json::String(s)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()) => Ok(s.clone()),
        Some(Json::Number(n)) => Ok(n.to_string()),
        _ => Err("That share isn't known.".to_string()),
    };
    match op {
        "share.search" => {
            let mut q = vec![("search", s("query")), ("itemType", "file".into()), ("lookup", "false".into()), ("perPage", "10".into())];
            for t in [0, 1, 4, 6, 7] {
                q.push(("shareType[]", t.to_string()));
            }
            let data = nc.ocs("GET", "apps/files_sharing/api/v1/sharees", &q, None)?;
            Ok(Json::Array(sharees(&data)))
        }
        "share.list" => list(nc, path).map(Json::Array),
        "share.add" => {
            let kind = s("type");
            let share_type = SHARE_TYPES.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v).unwrap_or(0);
            let created =
                nc.share("POST", "", json!({"path": path, "shareType": share_type, "shareWith": s("shareWith"), "permissions": edit("edit")}))?;
            let note = s("note");
            if !note.is_empty()
                && let Some(id) = share_id(&created)
            {
                nc.share("PUT", &id, json!({"note": note}))?;
            }
            Ok(share_of(&created))
        }
        "share.update" => {
            let id = id()?;
            let mut last = None;
            if p.get("edit").is_some_and(|v| !v.is_null()) {
                last = Some(nc.share("PUT", &id, json!({"permissions": edit("edit")}))?);
            }
            if let Some(password) = p.get("password") {
                last = Some(nc.share("PUT", &id, json!({"password": password.as_str().unwrap_or("")}))?);
            }
            if let Some(expiration) = p.get("expiration") {
                last = Some(nc.share("PUT", &id, json!({"expireDate": expiration.as_str().unwrap_or("")}))?);
            }
            Ok(last.map(|s| share_of(&s)).unwrap_or_else(|| json!({})))
        }
        "share.remove" => {
            nc.share("DELETE", &id()?, Json::Null)?;
            Ok(json!({}))
        }
        "share.link" => {
            let existing = list(nc, path)?.into_iter().find(|s| s["type"] == "link").and_then(|s| share_id(&s));
            let password = s("password");
            let expiration = s("expiration");
            let link = match existing {
                Some(id) => {
                    nc.share("PUT", &id, json!({"permissions": edit("edit")}))?;
                    if !password.is_empty() {
                        nc.share("PUT", &id, json!({"password": password}))?;
                    }
                    nc.share("PUT", &id, json!({"expireDate": expiration}))?
                }
                None => {
                    let mut params = json!({"path": path, "shareType": 3, "permissions": edit("edit")});
                    if !password.is_empty() {
                        params["password"] = json!(password);
                    }
                    if !expiration.is_empty() {
                        params["expireDate"] = json!(expiration);
                    }
                    nc.share("POST", "", params)?
                }
            };
            Ok(share_of(&link))
        }
        _ => Err(format!("Sharing can't do “{op}” here.")),
    }
}

fn list(nc: &Connection, path: &str) -> Result<Vec<Json>, String> {
    let data = nc.ocs("GET", "apps/files_sharing/api/v1/shares", &[("path", path.to_string()), ("reshares", "true".into())], None)?;
    Ok(data.as_array().map(|a| a.iter().map(share_of).collect()).unwrap_or_default())
}

fn share_id(s: &Json) -> Option<String> {
    match s.get("id")? {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A Nextcloud share as the dialog wants it.
fn share_of(s: &Json) -> Json {
    let code = s.get("share_type").and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0);
    let kind = SHARE_TYPES.iter().find(|(_, v)| *v == code).map(|(k, _)| *k).unwrap_or("user");
    let permissions = s.get("permissions").and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0);
    let text = |k: &str| s.get(k).and_then(Json::as_str).unwrap_or("");
    let label = [text("share_with_displayname"), text("share_with")].into_iter().find(|t| !t.is_empty()).unwrap_or("");
    json!({
        "id": s.get("id").cloned().unwrap_or(Json::Null),
        "type": kind,
        "shareWith": text("share_with"),
        "label": label,
        "edit": permissions & 2 != 0,
        "url": s.get("url").and_then(Json::as_str),
        "password": kind == "link" && (!text("password").is_empty() || !text("share_with").is_empty()),
        "expiration": s.get("expiration").and_then(Json::as_str).filter(|e| !e.is_empty()).map(|e| e.chars().take(10).collect::<String>()),
    })
}

/// The sharee search's answer as suggestions: exact matches first, each once.
fn sharees(data: &Json) -> Vec<Json> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let empty = json!({});
    for bucket in [data.get("exact").unwrap_or(&empty), data] {
        for key in ["users", "groups", "emails", "remotes", "circles"] {
            for e in bucket.get(key).and_then(Json::as_array).into_iter().flatten() {
                let code = e.pointer("/value/shareType").and_then(Json::as_i64).unwrap_or(-1);
                let Some(kind) = SHARE_TYPES.iter().find(|(_, v)| *v == code).map(|(k, _)| *k) else { continue };
                let with =
                    e.pointer("/value/shareWith").and_then(|v| v.as_str().map(str::to_string).or_else(|| Some(v.to_string()))).unwrap_or_default();
                if !seen.insert(format!("{kind}:{with}")) {
                    continue;
                }
                let detail =
                    ["shareWithDisplayNameUnique", "subline"].iter().filter_map(|k| e.get(*k).and_then(Json::as_str)).find(|d| !d.is_empty());
                out.push(json!({"label": e.get("label").and_then(Json::as_str).unwrap_or(&with), "detail": detail, "type": kind, "shareWith": with}));
            }
        }
    }
    out
}

/// An answer from Nextcloud.
struct Answer {
    status: u16,
    etag: Option<String>,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Result<Json, String> {
        serde_json::from_slice(&self.body).map_err(|_| format!("Nextcloud's answer ({}) wasn't understood.", self.status))
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .allow_non_standard_methods(true)
        .timeout_global(Some(timeout))
        .user_agent(concat!("GridLogic/", env!("CARGO_PKG_VERSION")))
        .tls_config(tls)
        .build()
        .into()
}

fn call(http: &ureq::Agent, method: &str, url: &str, headers: &[(&str, String)], body: Vec<u8>) -> Result<Answer, String> {
    let mut b = ureq::http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        b = b.header(*k, v.as_str());
    }
    let sent = if body.is_empty() && matches!(method, "GET" | "HEAD" | "DELETE" | "PROPFIND" | "MKCOL") {
        http.run(b.body(()).map_err(|e| e.to_string())?)
    } else {
        http.run(b.body(body).map_err(|e| e.to_string())?)
    };
    let mut r = sent.map_err(|e| format!("Couldn't reach Nextcloud: {e}"))?;
    let header = |k: &str| r.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let etag = header("OC-ETag").or_else(|| header("ETag")).map(|e| if e.starts_with('"') || e.starts_with("W/") { e } else { format!("\"{e}\"") });
    let status = r.status().as_u16();
    let body = r.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_vec().map_err(|e| format!("Couldn't read Nextcloud's answer: {e}"))?;
    Ok(Answer { status, etag, body })
}

/// A signed-in connection.
struct Connection {
    http: ureq::Agent,
    server: String,
    user: String,
    auth: String,
}

impl Connection {
    fn open(state: &Mutex<State>) -> Result<Connection, String> {
        let (account, password) = {
            let s = lock(state);
            (s.stored.account.clone().ok_or("Sign in to Nextcloud first.")?, s.password.clone())
        };
        let password = match password {
            Some(p) => p,
            None => {
                let p =
                    account.keychain()?.get_password().map_err(|_| "Sign in to Nextcloud again: the password isn't in the keychain.".to_string())?;
                lock(state).password = Some(p.clone());
                p
            }
        };
        Ok(Connection {
            http: agent(Duration::from_secs(120)),
            server: account.server.clone(),
            user: account.user.clone(),
            auth: basic(&account.login, &password),
        })
    }

    fn send(&self, method: &str, url: &str, headers: &[(&str, String)], body: Vec<u8>) -> Result<Answer, String> {
        let mut all = vec![("Authorization", self.auth.clone())];
        all.extend(headers.iter().cloned());
        let r = call(&self.http, method, url, &all, body)?;
        if r.status == 401 {
            return Err("Nextcloud didn't accept GridLogic's sign-in. Sign in again.".into());
        }
        Ok(r)
    }

    fn dav(&self, path: &str) -> String {
        let encoded: Vec<String> = path.split('/').filter(|p| !p.is_empty()).map(encode).collect();
        format!("{}/remote.php/dav/files/{}/{}", self.server, encode(&self.user), encoded.join("/"))
    }

    /// An OCS call; its `data`.
    fn ocs(&self, method: &str, endpoint: &str, query: &[(&str, String)], body: Option<Json>) -> Result<Json, String> {
        let mut url = format!("{}/ocs/v2.php/{endpoint}?format=json", self.server);
        for (k, v) in query {
            url.push('&');
            url.push_str(&encode(k).replace("%5B%5D", "[]"));
            url.push('=');
            url.push_str(&encode(v));
        }
        let mut headers = vec![("OCS-APIRequest", "true".to_string()), ("Accept", "application/json".to_string())];
        let body = match body {
            Some(b) if !b.is_null() => {
                headers.push(("Content-Type", "application/json".into()));
                serde_json::to_vec(&b).map_err(|e| e.to_string())?
            }
            _ => Vec::new(),
        };
        let r = self.send(method, &url, &headers, body)?;
        let answer = r.json().ok();
        let meta = answer.as_ref().and_then(|a| a.pointer("/ocs/meta"));
        let code = meta.and_then(|m| m.get("statuscode")).and_then(Json::as_i64).unwrap_or(0);
        if !(200..300).contains(&r.status) || code >= 300 {
            let message = meta.and_then(|m| m.get("message")).and_then(Json::as_str).filter(|m| !m.is_empty());
            return Err(message.map(str::to_string).unwrap_or_else(|| format!("Nextcloud answered {}.", r.status)));
        }
        Ok(answer.and_then(|mut a| a.pointer_mut("/ocs/data").map(Json::take)).unwrap_or(Json::Null))
    }

    /// A call to the sharing API: `shares` or `shares/{id}`.
    fn share(&self, method: &str, id: &str, body: Json) -> Result<Json, String> {
        let endpoint =
            if id.is_empty() { "apps/files_sharing/api/v1/shares".to_string() } else { format!("apps/files_sharing/api/v1/shares/{}", encode(id)) };
        self.ocs(method, &endpoint, &[], Some(body))
    }

    /// A call to the GridLogic app's own routes (`/index.php/apps/gridcraft/…`): its status and
    /// JSON answer. `OCS-APIRequest` lets an app-password call through Nextcloud's CSRF check.
    fn app_status(&self, method: &str, endpoint: &str, body: &Json) -> Result<(u16, Json), String> {
        let url = format!("{}/index.php/apps/gridcraft/{endpoint}", self.server);
        let mut headers = vec![("OCS-APIRequest", "true".to_string()), ("Accept", "application/json".to_string())];
        let body = if body.is_null() {
            Vec::new()
        } else {
            headers.push(("Content-Type", "application/json".to_string()));
            serde_json::to_vec(body).map_err(|e| e.to_string())?
        };
        let r = self.send(method, &url, &headers, body)?;
        Ok((r.status, r.json().unwrap_or(Json::Null)))
    }

    /// [`Connection::app_status`], an error unless it succeeded.
    fn app(&self, method: &str, endpoint: &str, body: &Json) -> Result<Json, String> {
        let (status, answer) = self.app_status(method, endpoint, body)?;
        if (200..300).contains(&status) {
            return Ok(answer);
        }
        let why = answer.get("error").and_then(Json::as_str).map(str::to_string);
        Err(why.unwrap_or_else(|| format!("Nextcloud answered {status}.")))
    }

    /// The Nextcloud file id of a file (co-editing sessions are per file id).
    fn file_id(&self, path: &str) -> Result<u64, String> {
        let body = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>"#;
        let headers = [("Depth", "0".to_string()), ("Content-Type", "application/xml".to_string())];
        let r = self.send("PROPFIND", &self.dav(path), &headers, body.as_bytes().to_vec())?;
        let text = String::from_utf8_lossy(&r.body);
        text.split("fileid>")
            .nth(1)
            .and_then(|t| t.split('<').next())
            .and_then(|id| id.trim().parse().ok())
            .ok_or_else(|| "Nextcloud didn't give the file's id.".into())
    }

    /// Whether the user may share the file (its WebDAV permissions include R).
    fn can_share(&self, path: &str) -> Result<bool, String> {
        let body =
            r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:permissions/></d:prop></d:propfind>"#;
        let headers = [("Depth", "0".to_string()), ("Content-Type", "application/xml".to_string())];
        let r = self.send("PROPFIND", &self.dav(path), &headers, body.as_bytes().to_vec())?;
        if r.status == 404 {
            return Err("The workbook isn't in Nextcloud any more.".into());
        }
        let text = String::from_utf8_lossy(&r.body);
        let permissions = text.split("<oc:permissions>").nth(1).and_then(|t| t.split('<').next()).unwrap_or("");
        Ok(permissions.contains('R'))
    }
}

fn basic(login: &str, password: &str) -> String {
    format!("Basic {}", gridcraft_engine::io::base64_encode(format!("{login}:{password}").as_bytes()))
}

/// Percent-encodes everything but unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_addresses() {
        assert_eq!(normalise_server("cloud.example.com").as_deref(), Ok("https://cloud.example.com"));
        assert_eq!(normalise_server(" https://cloud.example.com/index.php/ ").as_deref(), Ok("https://cloud.example.com"));
        assert_eq!(normalise_server("localhost:8080").as_deref(), Ok("http://localhost:8080"));
        assert_eq!(normalise_server("http://nc.test/nextcloud").as_deref(), Ok("http://nc.test/nextcloud"));
        assert!(normalise_server("").is_err());
        assert!(normalise_server("user@evil.example").is_err());
        assert!(normalise_server("a b").is_err());
    }

    #[test]
    fn shares_and_sharees_read_like_the_browser() {
        let s = share_of(
            &json!({"id": "12", "share_type": 3, "permissions": 1, "url": "https://c/s/x", "share_with": "", "expiration": "2026-12-01 00:00:00"}),
        );
        assert_eq!(
            s,
            json!({"id": "12", "type": "link", "shareWith": "", "label": "", "edit": false, "url": "https://c/s/x", "password": false, "expiration": "2026-12-01"})
        );
        let found = sharees(&json!({
            "exact": {"users": [{"label": "Ana", "value": {"shareType": 0, "shareWith": "ana"}}]},
            "users": [{"label": "Ana", "value": {"shareType": 0, "shareWith": "ana"}}],
            "groups": [{"label": "Finance", "value": {"shareType": 1, "shareWith": "finance"}}],
        }));
        assert_eq!(found.len(), 2);
        assert_eq!(found[1]["type"], "group");
    }

    #[test]
    fn local_files_go_only_to_the_same_type() {
        assert!(same_kind("C:/x/Budget.xlsx", "GridLogic/Budget.xlsx"));
        assert!(!same_kind("C:/x/Budget.csv", "GridLogic/Budget.xlsx"));
        assert!(!same_kind("C:/x/Budget.xlsm", "GridLogic/Budget.xlsx"));
        assert!(!same_kind("C:/x/Budget.ods", "GridLogic/Budget.ods"), "GridLogic doesn't write .ods");
        assert_eq!(encode("a b/ü"), "a%20b%2F%C3%BC");
        assert_eq!(decode("a%20b%2F%C3%BC"), "a b/ü");
    }

    #[test]
    fn desktop_links_name_a_server_and_a_file() {
        let mut nc = Nextcloud::new(None, None, |_| {});
        assert!(nc.open_link("gridlogic://open?server=http%3A%2F%2Flocalhost%3A8080&path=%2FGridCraft%20Test%2FBudget.xlsx"));
        assert_eq!(nc.link, Some(("http://localhost:8080".to_string(), "GridCraft Test/Budget.xlsx".to_string(), false)));
        assert!(!nc.open_link("gridlogic://open?server=cloud.example"), "no file");
        assert!(!nc.open_link("https://cloud.example/x"), "not a GridLogic link");
        assert!(!nc.open_link("gridlogic://open?server=a%40evil&path=x.xlsx"), "not a server address");
    }

    #[test]
    fn records_match_any_spelling_of_a_path() {
        let mut files = HashMap::new();
        files.insert(r"C:\Cache\Sales.xlsx".to_string(), Remote { path: "Sales.xlsx".into(), etag: None, synced: None });
        assert_eq!(tie_key(&files, r"C:\Cache\Sales.xlsx").as_deref(), Some(r"C:\Cache\Sales.xlsx"));
        let other = tie_key(&files, "c:/cache/Sales.xlsx");
        if cfg!(windows) {
            assert_eq!(other.as_deref(), Some(r"C:\Cache\Sales.xlsx"));
        } else {
            assert_eq!(other, None);
        }
    }

    #[test]
    fn folder_listings_read() {
        let xml = br#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"><d:response><d:href>/remote.php/dav/files/ana/Docs/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response><d:response><d:href>/remote.php/dav/files/ana/Docs/Q3%20Budget.xlsx</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>5271</d:getcontentlength></d:prop></d:propstat></d:response></d:multistatus>"#;
        let e = dav_entries(xml);
        assert_eq!(e.len(), 2);
        assert!(e[0].folder && !e[1].folder);
        assert_eq!((decode(&e[1].href), e[1].size), ("/remote.php/dav/files/ana/Docs/Q3 Budget.xlsx".to_string(), 5271));
        assert_eq!(url_path("https://cloud.example/nc/remote.php/dav/files/ana/"), "/nc/remote.php/dav/files/ana/");
    }

    #[test]
    fn cache_names_are_safe() {
        assert_eq!(safe_name("a:b*c?.xlsx"), "a_b_c_.xlsx");
        assert_eq!(safe_name(".."), "_");
        assert_eq!(safe_name("Report. "), "Report");
        assert_eq!(safe_name("localhost:8080"), "localhost_8080");
    }
}
