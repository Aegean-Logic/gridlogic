//! Nextcloud in the File view, as OneDrive is in Excel's: the Account page signs in and out, and
//! Open and Save As browse Nextcloud's folders. The host's cloud ([`crate::share::Cloud`] with
//! `files`, the desktop) does the talking; this keeps what the pages show and turns the answers
//! into actions. A workbook opened from (or saved to) Nextcloud is a local copy tied to the
//! Nextcloud file, so opening and saving it are the ordinary `file.open` and `file.save`.

use std::collections::HashMap;

use egui::{Align2, Rect, RichText, Sense, Ui, pos2, vec2};
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::SheetApp;
use crate::icons::{self, Icon};
use crate::share::next_request_id;
use crate::theme::{self, Tokens};

/// The signed-in account, as the cloud describes it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    pub signed_in: bool,
    /// `ana on cloud.example.com`.
    pub account: Option<String>,
    pub server: Option<String>,
}

/// A file or folder in a listing.
#[derive(Clone, Debug, Deserialize)]
pub struct Item {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub folder: bool,
}

/// A workbook whose Nextcloud file someone else changed while it was being worked on here; the
/// user chooses what to keep. Pushed by the host.
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    /// The local copy (the workbook's path).
    pub local: String,
    /// The file's name in Nextcloud.
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Listing {
    path: String,
    #[serde(default)]
    items: Vec<Item>,
}

/// What a request in flight is for.
#[derive(Clone, Debug)]
enum Asked {
    Account,
    SignIn,
    SignOut,
    List(String),
    Open(String),
    SaveAs,
    Resolve(String),
}

/// The File view's Nextcloud state (on [`SheetApp`], so answers that arrive after the view was
/// closed still open or save the workbook).
#[derive(Default)]
pub struct CloudFiles {
    /// `None` until the cloud said.
    pub account: Option<Account>,
    /// The server address typed to sign in.
    pub server: String,
    /// The folder shown, relative to the user's files.
    pub folder: String,
    /// Its contents, once listed.
    pub items: Option<Vec<Item>>,
    /// Save As: the name typed, and the workbook (uid) it was first filled in for.
    pub name: String,
    named_for: Option<u64>,
    /// Save As: a file of this name is there already; waiting for Replace.
    pub replace: Option<String>,
    pub status: Option<(String, bool)>,
    /// Workbooks waiting for the user to choose between their changes and Nextcloud's.
    pub conflicts: Vec<Conflict>,
    asked: HashMap<u64, Asked>,
}

/// Whether the File view shows Nextcloud (the host's cloud can open and save files).
pub fn available(app: &SheetApp) -> bool {
    app.services.cloud.as_ref().is_some_and(|c| c.files)
}

fn ask(app: &SheetApp, cf: &mut CloudFiles, op: &str, params: Json, what: Asked) {
    let Some(cloud) = &app.services.cloud else { return };
    let id = next_request_id();
    cf.asked.insert(id, what);
    (cloud.request)(id, op, params);
}

impl CloudFiles {
    fn busy(&self) -> bool {
        self.asked.values().any(|a| !matches!(a, Asked::List(_)))
    }
    fn signing_in(&self) -> bool {
        self.asked.values().any(|a| matches!(a, Asked::SignIn))
    }
    fn signed_in(&self) -> bool {
        self.account.as_ref().is_some_and(|a| a.signed_in)
    }
}

/// Every frame: the cloud's answers to the File view's requests.
pub fn poll(app: &mut SheetApp) {
    if app.cloud_files.asked.is_empty() {
        return;
    }
    let Some(cloud) = &app.services.cloud else { return };
    let answers: Vec<(u64, Result<Json, String>)> = {
        let mut inbox = cloud.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let (mine, rest): (Vec<_>, Vec<_>) = inbox.drain(..).partition(|(id, _)| app.cloud_files.asked.contains_key(id));
        *inbox = rest;
        mine
    };
    for (id, answer) in answers {
        let Some(what) = app.cloud_files.asked.remove(&id) else { continue };
        let answer = match answer {
            Ok(a) => a,
            Err(e) => {
                // The File view shows it; a workbook asked for after the view closed says so.
                if app.backstage.is_none() && matches!(what, Asked::Open(_) | Asked::SaveAs | Asked::Resolve(_)) {
                    app.message = Some(("Nextcloud".into(), e.clone()));
                }
                app.cloud_files.status = Some((e, true));
                continue;
            }
        };
        match what {
            Asked::Account | Asked::SignIn | Asked::SignOut => {
                let account: Account = serde_json::from_value(answer).unwrap_or_default();
                let cf = &mut app.cloud_files;
                if cf.server.is_empty() {
                    cf.server = account.server.clone().unwrap_or_default();
                }
                if !account.signed_in {
                    cf.items = None;
                    cf.folder.clear();
                }
                cf.status = match what {
                    Asked::SignIn => Some(("Signed in.".into(), false)),
                    Asked::SignOut => Some(("Signed out.".into(), false)),
                    _ => None,
                };
                cf.account = Some(account);
            }
            Asked::List(path) => {
                if let Ok(listing) = serde_json::from_value::<Listing>(answer)
                    && listing.path.trim_matches('/') == path.trim_matches('/')
                {
                    app.cloud_files.folder = path;
                    app.cloud_files.items = Some(listing.items);
                }
            }
            Asked::Open(name) => {
                let Some(local) = answer.get("localPath").and_then(Json::as_str).map(str::to_string) else { continue };
                app.cloud_files.status = Some((format!("Opened {name} from Nextcloud."), false));
                app.open_path(&local);
            }
            Asked::Resolve(name) => resolved(app, &name, &answer),
            Asked::SaveAs => {
                let name = answer.get("name").and_then(Json::as_str).unwrap_or_default().to_string();
                if answer.get("exists").and_then(Json::as_bool) == Some(true) {
                    app.cloud_files.replace = Some(name);
                    continue;
                }
                let Some(local) = answer.get("localPath").and_then(Json::as_str).map(str::to_string) else { continue };
                // Saved here, then sent to Nextcloud by the host.
                match app.run("file.save", json!({"path": local})) {
                    Ok(_) => {
                        app.cloud_files.status = Some((format!("Saving {name} to Nextcloud…"), false));
                        app.backstage = None;
                    }
                    Err(e) => app.alert(e),
                }
            }
        }
    }
}

/// Acts on the host's answer to a conflict choice.
fn resolved(app: &mut SheetApp, name: &str, answer: &Json) {
    let local = answer.get("localPath").and_then(Json::as_str).map(str::to_string);
    if answer.get("reload").and_then(Json::as_bool) == Some(true)
        && let Some(local) = local
    {
        // Nextcloud's version replaced the copy here: open it in place of the workbook.
        if let Some(i) = app.session.documents().iter().position(|d| d.path.as_deref() == Some(local.as_str())) {
            app.session.close_document(i);
        }
        app.open_path(&local);
        app.toast = Some((format!("{name}: Nextcloud's version is open."), crate::now_ms()));
    } else if answer.get("saveAs").and_then(Json::as_bool) == Some(true)
        && let Some(local) = local
    {
        // Their version stays; this one goes next to it as a copy, which the workbook becomes.
        let copy = answer.get("name").and_then(Json::as_str).unwrap_or_default().to_string();
        match app.run("file.save", json!({"path": local})) {
            Ok(_) => app.toast = Some((format!("Your changes are in {copy}; {name} keeps Nextcloud's version."), crate::now_ms())),
            Err(e) => app.alert(e),
        }
    } else {
        app.toast = Some((format!("Your version of {name} is in Nextcloud."), crate::now_ms()));
    }
}

/// The prompt for the first conflict waiting: keep both, keep mine, or take Nextcloud's. Drawn
/// over everything, wherever the user is.
pub fn show_conflict(app: &mut SheetApp, ctx: &egui::Context) {
    let Some(c) = app.cloud_files.conflicts.first().cloned() else { return };
    let busy = app.cloud_files.asked.values().any(|a| matches!(a, Asked::Resolve(_)));
    let mut choice = None;
    egui::Modal::new(egui::Id::new("nextcloud_conflict")).show(ctx, |ui| {
        let t = Tokens::get(ui.ctx());
        ui.set_width(460.0);
        ui.label(RichText::new(format!("{} changed in Nextcloud", c.name)).font(theme::ui_bold(15.0)).color(t.text));
        ui.add_space(6.0);
        ui.label("Someone else saved this workbook in Nextcloud while you were working on it here. Your changes are safe on this computer. What should happen to them?");
        ui.add_space(12.0);
        let options = [
            ("keepBoth", "Keep both", "Your changes go into a copy next to it; theirs stay in the original."),
            ("mine", "Use mine", "Your version replaces theirs (Nextcloud keeps theirs as an older version)."),
            ("theirs", "Use Nextcloud's", "Your changes are dropped and their version opens here."),
        ];
        for (id, label, detail) in options {
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(label).min_size(vec2(130.0, 0.0))).clicked() {
                    choice = Some(id);
                }
                ui.label(RichText::new(detail).color(t.text_dim));
            });
            ui.add_space(4.0);
        }
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
            });
        }
    });
    if let Some(choice) = choice {
        let mut cf = std::mem::take(&mut app.cloud_files);
        cf.conflicts.retain(|x| x != &c);
        ask(app, &mut cf, "files.resolve", json!({"localPath": c.local, "choice": choice}), Asked::Resolve(c.name.clone()));
        app.cloud_files = cf;
    }
}

/// Asks the cloud about the account the first time the File view needs it.
fn ensure_account(app: &mut SheetApp, cf: &mut CloudFiles) {
    if cf.account.is_none() && !cf.asked.values().any(|a| matches!(a, Asked::Account)) {
        ask(app, cf, "account.info", json!({}), Asked::Account);
    }
}

/// Lists `cf.folder` once signed in, if it isn't listed or asked for yet.
fn ensure_listing(app: &mut SheetApp, cf: &mut CloudFiles) {
    if cf.signed_in() && cf.items.is_none() && !cf.asked.values().any(|a| matches!(a, Asked::List(_))) {
        let folder = cf.folder.clone();
        ask(app, cf, "files.list", json!({"path": folder}), Asked::List(folder));
    }
}

fn go_to(app: &mut SheetApp, cf: &mut CloudFiles, folder: &str) {
    cf.items = None;
    cf.folder = folder.trim_matches('/').to_string();
    let folder = cf.folder.clone();
    ask(app, cf, "files.list", json!({"path": folder}), Asked::List(folder));
}

fn status(ui: &mut Ui, cf: &CloudFiles) {
    if let Some((text, problem)) = &cf.status {
        let t = Tokens::get(ui.ctx());
        ui.add_space(6.0);
        ui.label(RichText::new(text).color(if *problem { t.danger } else { t.text_dim }));
    }
}

/// File › Account: sign in to Nextcloud, or see who's signed in and sign out.
pub fn account_page(app: &mut SheetApp, ui: &mut Ui) {
    if !available(app) {
        let t = Tokens::get(ui.ctx());
        ui.label(RichText::new("Nextcloud accounts are managed by the page GridLogic runs in.").color(t.text_dim));
        return;
    }
    let mut cf = std::mem::take(&mut app.cloud_files);
    ensure_account(app, &mut cf);
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new("Nextcloud").font(theme::ui_bold(15.0)).color(t.text));
    ui.add_space(8.0);
    match cf.account.clone() {
        None => {
            ui.spinner();
        }
        Some(a) if a.signed_in => {
            ui.label(format!("Signed in as {}.", a.account.unwrap_or_default()));
            ui.label(RichText::new("Open and Save As show your Nextcloud files; workbooks there save to Nextcloud as you work.").color(t.text_dim));
            ui.add_space(10.0);
            if ui.add_enabled(!cf.busy(), egui::Button::new("Sign out")).clicked() {
                ask(app, &mut cf, "account.signOut", json!({}), Asked::SignOut);
            }
        }
        Some(_) => {
            ui.label("Sign in to open and save workbooks in Nextcloud and to share them.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Server");
                let r = ui.add(egui::TextEdit::singleline(&mut cf.server).hint_text("cloud.example.com").desired_width(280.0));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Nextcloud server address"));
            });
            ui.add_space(8.0);
            let ready = !cf.busy() && !cf.server.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("Sign in to Nextcloud…")).clicked() {
                let server = cf.server.trim().to_string();
                cf.status = Some(("Your browser shows Nextcloud's sign-in page: approve GridLogic there.".into(), false));
                ask(app, &mut cf, "account.signIn", json!({"server": server}), Asked::SignIn);
            }
            if cf.signing_in() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for you to approve GridLogic in your browser…");
                });
            }
        }
    }
    status(ui, &cf);
    app.cloud_files = cf;
}

/// How a browser row was clicked.
enum Picked {
    Folder(String),
    File(Item),
}

/// The folder browser shared by Open and Save As. Returns true when the Account page is wanted
/// (not signed in).
fn browser(app: &mut SheetApp, cf: &mut CloudFiles, ui: &mut Ui) -> (bool, Option<Picked>) {
    ensure_account(app, cf);
    ensure_listing(app, cf);
    let t = Tokens::get(ui.ctx());
    let mut picked = None;
    match &cf.account {
        None => {
            ui.spinner();
            return (false, None);
        }
        Some(a) if !a.signed_in => {
            ui.label(RichText::new("Sign in to Nextcloud to open and save workbooks there.").color(t.text_dim));
            let account = ui.link("Sign in…").clicked();
            return (account, None);
        }
        Some(_) => {}
    }
    // Where: Nextcloud › folder › subfolder, each a link.
    ui.horizontal_wrapped(|ui| {
        if ui.link("Nextcloud").clicked() {
            picked = Some(Picked::Folder(String::new()));
        }
        let mut path = String::new();
        for part in cf.folder.split('/').filter(|p| !p.is_empty()) {
            ui.label(RichText::new("›").color(t.text_dim));
            path = if path.is_empty() { part.to_string() } else { format!("{path}/{part}") };
            if ui.link(part).clicked() {
                picked = Some(Picked::Folder(path.clone()));
            }
        }
    });
    ui.add_space(6.0);
    match &cf.items {
        None => {
            ui.spinner();
        }
        Some(items) if items.is_empty() => {
            ui.label(RichText::new("No folders or workbooks here.").color(t.text_dim));
        }
        Some(items) => {
            for item in items {
                if row(ui, if item.folder { Icon::Folder } else { Icon::Book }, &item.name).clicked() {
                    picked = Some(if item.folder { Picked::Folder(item.path.clone()) } else { Picked::File(item.clone()) });
                }
            }
        }
    }
    (false, picked)
}

/// One row of the browser: an icon and a name.
fn row(ui: &mut Ui, icon: Icon, name: &str) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    let width = ui.available_width().min(crate::backstage::CONTENT_W);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name));
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, t.hover);
    }
    icons::paint(p, Rect::from_center_size(pos2(rect.left() + 18.0, rect.center().y), vec2(18.0, 18.0)), icon, t.text);
    p.text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, name, theme::ui_font(13.0), t.text);
    resp
}

/// File › Open › Nextcloud. Returns true when the Account page is wanted.
pub fn open_section(app: &mut SheetApp, ui: &mut Ui) -> bool {
    let mut cf = std::mem::take(&mut app.cloud_files);
    let (account, picked) = browser(app, &mut cf, ui);
    match picked {
        Some(Picked::Folder(f)) => go_to(app, &mut cf, &f),
        Some(Picked::File(item)) => {
            let open: Vec<String> = app.session.documents().iter().filter_map(|d| d.path.clone()).collect();
            cf.status = Some((format!("Opening {}…", item.name), false));
            ask(app, &mut cf, "files.open", json!({"path": item.path, "openPaths": open}), Asked::Open(item.name));
        }
        None => {}
    }
    status(ui, &cf);
    app.cloud_files = cf;
    account
}

/// File › Save As › Nextcloud: pick a folder, type a name, Save. Returns true when the Account
/// page is wanted.
pub fn save_as_section(app: &mut SheetApp, ui: &mut Ui) -> bool {
    let mut cf = std::mem::take(&mut app.cloud_files);
    // The workbook's own name to start with, once per workbook (an emptied box stays empty).
    let uid = app.session.active().map(|d| d.uid);
    if cf.named_for != uid {
        cf.named_for = uid;
        let title = app.session.active().map(|d| d.display_title()).unwrap_or_else(|| "Book1".into());
        cf.name = std::path::Path::new(&title).file_stem().and_then(|s| s.to_str()).unwrap_or("Book1").to_string();
    }
    let (account, picked) = browser(app, &mut cf, ui);
    match picked {
        Some(Picked::Folder(f)) => go_to(app, &mut cf, &f),
        // Clicking a workbook takes its name (Save then asks before replacing it).
        Some(Picked::File(item)) => cf.name = item.name,
        None => {}
    }
    if cf.signed_in() {
        ui.add_space(10.0);
        let mut save = false;
        ui.horizontal(|ui| {
            ui.label("File name");
            let r = ui.add(egui::TextEdit::singleline(&mut cf.name).desired_width(280.0));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Nextcloud file name"));
            if r.changed() {
                cf.replace = None;
            }
            save = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            save |= ui.add_enabled(!cf.busy() && !cf.name.trim().is_empty(), egui::Button::new("  Save  ")).clicked();
        });
        let mut replace = false;
        if let Some(name) = cf.replace.clone() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(format!("{name} is already there. Replace it?"));
                replace = ui.button("Replace").clicked();
                if ui.button("Cancel").clicked() {
                    cf.replace = None;
                }
            });
        }
        if (save || replace) && app.commit_edit(0, 0, false, false) {
            let (folder, name) = (cf.folder.clone(), cf.name.trim().to_string());
            cf.replace = None;
            ask(app, &mut cf, "files.saveAs", json!({"folder": folder, "name": name, "replace": replace}), Asked::SaveAs);
        }
    }
    status(ui, &cf);
    app.cloud_files = cf;
    account
}
