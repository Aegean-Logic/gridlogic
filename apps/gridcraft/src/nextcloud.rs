//! Nextcloud for the desktop app, as OneDrive is for Excel: sign in once (Nextcloud's Login Flow
//! v2, in the browser; the app password goes to the system keychain), upload a workbook to share
//! it, and the Share dialog's operations ([`gridcraft_ui_egui::share::Cloud`]) through Nextcloud's
//! sharing API. An uploaded workbook stays connected: saving it on this computer saves it to
//! Nextcloud too, unless someone changed the Nextcloud copy in the meantime.
//!
//! The dialog's requests are queued and picked up by [`Nextcloud::poll`] on the UI thread, which
//! knows the workbook they are about; the talking happens on worker threads, which answer in the
//! dialog's inbox and ask for a repaint. The account and the uploaded files are remembered in
//! `nextcloud.json` in the settings folder; the password never is.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

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
    open_url: fn(&str),
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

impl Nextcloud {
    /// `file` is where the account and the uploaded files are remembered; `open_url` opens the
    /// sign-in page in the browser.
    pub fn new(file: Option<PathBuf>, open_url: fn(&str)) -> Nextcloud {
        let stored = file.as_deref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Nextcloud {
            requests: Arc::default(),
            answers: CloudInbox::default(),
            state: Arc::new(Mutex::new(State { stored, ..State::default() })),
            file,
            checked_at: f64::NEG_INFINITY,
            modified: HashMap::new(),
            open_url,
        }
    }

    /// The Share dialog's cloud: requests wait for [`Nextcloud::poll`].
    pub fn cloud(&self) -> Cloud {
        let requests = self.requests.clone();
        Cloud {
            request: Box::new(move |id, op, params| {
                requests.lock().unwrap_or_else(|e| e.into_inner()).push((id, op.to_string(), params));
            }),
            inbox: self.answers.clone(),
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
        let now = ctx.input(|i| i.time);
        if now - self.checked_at >= SAVE_CHECK_SECONDS {
            self.checked_at = now;
            self.send_saved(app, ctx);
        }
        let (notices, unsaved) = {
            let mut s = lock(&self.state);
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

    /// The active workbook, for a request about it.
    fn doc(&self, app: &SheetApp) -> Option<Doc> {
        let d = app.session.active()?;
        let s = lock(&self.state);
        let remote = d.path.as_ref().and_then(|p| s.stored.files.get(p)).or_else(|| s.unsaved_docs.get(&d.uid)).cloned();
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
                s.unsaved = true;
                // Never looked at: send it now.
                self.modified.insert(path.clone(), None);
            }
        }
        for d in app.session.documents() {
            let Some(path) = d.path.as_ref() else { continue };
            let Some(remote) = s.stored.files.get(path).cloned() else { continue };
            let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
            let Some(seen) = self.modified.get(path).copied() else {
                // First look since the app started: changes made while it wasn't running stay here.
                self.modified.insert(path.clone(), modified);
                continue;
            };
            if modified.is_none() || modified == seen || d.is_dirty() || s.sending.contains(path) || !same_kind(path, &remote.path) {
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

fn is_workbook(path: &str) -> bool {
    matches!(extension(path).as_str(), "xlsx" | "xlsm")
}

fn extension(path: &str) -> String {
    Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// A local file is only ever sent to a Nextcloud file of the same type.
fn same_kind(local: &str, remote: &str) -> bool {
    is_workbook(local) && extension(local) == extension(remote)
}

/// Sends a saved file to its copy in Nextcloud, overwriting only the version last seen here.
fn send_file(state: &Mutex<State>, path: &str, remote: &Remote) {
    let name = remote.path.rsplit('/').next().unwrap_or(&remote.path).to_string();
    let result = (|| {
        let nc = Connection::open(state)?;
        let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {path}: {e}"))?;
        let mut headers = vec![("Content-Type", "application/octet-stream".to_string())];
        if let Some(etag) = &remote.etag {
            headers.push(("If-Match", etag.clone()));
        }
        let r = nc.send("PUT", &nc.dav(&remote.path), &headers, bytes)?;
        match r.status {
            200..=299 => Ok(r.etag),
            412 => Err(format!(
                "Someone changed {name} in Nextcloud since you last saved it here. Your changes are saved on this computer; open the Nextcloud copy to bring them together."
            )),
            404 => Err(format!("{name} isn't in Nextcloud any more. Your changes are saved on this computer.")),
            s => Err(format!("Nextcloud answered {s} while saving {name}. Your changes are saved on this computer.")),
        }
    })();
    let mut s = lock(state);
    match result {
        Ok(etag) => {
            if let Some(r) = s.stored.files.get_mut(path) {
                r.etag = etag;
                s.unsaved = true;
            }
            s.notices.push((String::new(), "Saved to Nextcloud.".into(), false));
        }
        Err(e) => s.notices.push(("Couldn't save to Nextcloud".into(), e, true)),
    }
}

/// Carries out one of the dialog's operations.
fn run(state: &Mutex<State>, op: &str, params: &Json, doc: Option<Doc>, open_url: fn(&str)) -> Result<Json, String> {
    if op == "account.signIn" {
        let server = params.get("server").and_then(Json::as_str).unwrap_or("");
        sign_in(state, server, open_url)?;
        return info(state, doc.as_ref());
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
    s.unsaved = true;
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
                let remote = Remote { path, etag: r.etag };
                let mut s = lock(state);
                match doc.path.filter(|p| is_workbook(p)) {
                    Some(local) => {
                        s.stored.files.insert(local, remote);
                        s.unsaved = true;
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
    let sent = if body.is_empty() && matches!(method, "GET" | "DELETE" | "PROPFIND" | "MKCOL") {
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
        assert_eq!(encode("a b/ü"), "a%20b%2F%C3%BC");
    }
}
