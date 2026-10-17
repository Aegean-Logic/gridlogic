//! Nextcloud in the File view, against a fake cloud that answers at once: signing in from the
//! Account page, opening a workbook from a Nextcloud folder, Save As into one (with Replace), and
//! AutoSave being on for workbooks in Nextcloud.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use egui::vec2;
use egui_kittest::kittest::Queryable;
use gridcraft_engine::Session;
use gridcraft_ui_egui::share::{Cloud, CloudInbox};
use gridcraft_ui_egui::{Services, SheetApp};
use serde_json::{Value as Json, json};

/// A fake cloud: a scripted account and folder tree, copies in a temporary folder.
#[derive(Clone)]
struct Fake {
    requests: Arc<Mutex<Vec<(String, Json)>>>,
    signed_in: Arc<Mutex<bool>>,
    /// Local copies tied to cloud files: (local path, cloud path).
    tied: Arc<Mutex<Vec<(String, String)>>>,
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str, signed_in: bool) -> Fake {
        let dir = std::env::temp_dir().join(format!("gridlogic-cloud-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fake { requests: Arc::default(), signed_in: Arc::new(Mutex::new(signed_in)), tied: Arc::default(), dir }
    }

    fn cloud(&self) -> Cloud {
        let inbox: CloudInbox = Arc::default();
        let answers = inbox.clone();
        let me = self.clone();
        let tied = self.tied.clone();
        Cloud {
            request: Box::new(move |id, op, params| {
                me.requests.lock().unwrap().push((op.to_string(), params.clone()));
                let account = |on: bool| json!({"signedIn": on, "account": on.then_some("ana on cloud.example"), "server": on.then_some("https://cloud.example")});
                let answer = match op {
                    "account.info" => Ok(account(*me.signed_in.lock().unwrap())),
                    "account.signIn" => {
                        *me.signed_in.lock().unwrap() = true;
                        Ok(account(true))
                    }
                    "files.list" => {
                        let path = params["path"].as_str().unwrap_or("").to_string();
                        Ok(if path.is_empty() {
                            json!({"path": "", "items": [{"name": "Finance", "path": "Finance", "folder": true}]})
                        } else {
                            json!({"path": path, "items": [{"name": "Budget.xlsx", "path": "Finance/Budget.xlsx", "folder": false}]})
                        })
                    }
                    "files.open" => {
                        let local = me.dir.join("Budget.xlsx");
                        let mut s = Session::new();
                        s.new_workbook();
                        s.run("cell.set", json!({"cell": "A1", "input": "From the cloud"})).unwrap();
                        s.run("file.save", json!({"path": local.to_string_lossy()})).unwrap();
                        me.tied.lock().unwrap().push((local.to_string_lossy().to_string(), params["path"].as_str().unwrap().to_string()));
                        Ok(json!({"localPath": local.to_string_lossy()}))
                    }
                    "files.saveAs" => {
                        let name = params["name"].as_str().unwrap().to_string();
                        if params["replace"] != json!(true) {
                            Ok(json!({"exists": true, "name": format!("{name}.xlsx")}))
                        } else {
                            let local = me.dir.join(format!("{name}.xlsx"));
                            me.tied.lock().unwrap().push((local.to_string_lossy().to_string(), format!("Finance/{name}.xlsx")));
                            Ok(json!({"localPath": local.to_string_lossy(), "name": format!("{name}.xlsx")}))
                        }
                    }
                    "files.resolve" => {
                        let local = params["localPath"].as_str().unwrap().to_string();
                        match params["choice"].as_str() {
                            Some("theirs") => {
                                // Their version replaces the copy here.
                                let mut s = Session::new();
                                s.new_workbook();
                                s.run("cell.set", json!({"cell": "A1", "input": "Their version"})).unwrap();
                                s.run("file.save", json!({"path": local})).unwrap();
                                Ok(json!({"localPath": local, "reload": true}))
                            }
                            Some("keepBoth") => {
                                let copy = me.dir.join("Budget (copy).xlsx").to_string_lossy().to_string();
                                me.tied.lock().unwrap().retain(|(l, _)| *l != local);
                                me.tied.lock().unwrap().push((copy.clone(), "Finance/Budget (copy).xlsx".into()));
                                Ok(json!({"localPath": copy, "saveAs": true, "name": "Budget (copy).xlsx"}))
                            }
                            _ => Ok(json!({"name": "Budget.xlsx"})),
                        }
                    }
                    other => Err(format!("unexpected {other}")),
                };
                answers.lock().unwrap().push((id, answer));
            }),
            inbox,
            files: true,
            location: Some(Box::new(move |local: &str| tied.lock().unwrap().iter().find(|(l, _)| l == local).map(|(_, r)| r.clone()))),
        }
    }

    fn ops(&self) -> Vec<(String, Json)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if Arc::strong_count(&self.requests) == 1 {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn harness(fake: &Fake) -> egui_kittest::Harness<'static, SheetApp> {
    let mut session = Session::new();
    session.new_workbook();
    let services = Services { cloud: Some(fake.cloud()), ..Default::default() };
    let mut h = egui_kittest::Harness::builder().with_size(vec2(1300.0, 900.0)).build_ui_state(
        |ui, app: &mut SheetApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        SheetApp::new(session, services),
    );
    h.run_steps(4);
    h
}

fn page(h: &mut egui_kittest::Harness<'static, SheetApp>, page: &str) {
    let r = h.state_mut().run("ui.backstage", json!({"page": page}));
    assert!(r.is_ok(), "{r:?}");
    h.run_steps(6);
}

#[test]
fn sign_in_from_the_account_page() {
    let fake = Fake::new("signin", false);
    let mut h = harness(&fake);
    page(&mut h, "account");
    h.get_by_label("Nextcloud server address").click();
    h.get_by_label("Nextcloud server address").type_text("cloud.example");
    h.run_steps(2);
    h.get_by_label("Sign in to Nextcloud…").click();
    h.run_steps(6);
    let sign_in = fake.ops().into_iter().find(|(op, _)| op == "account.signIn").map(|(_, p)| p);
    assert_eq!(sign_in, Some(json!({"server": "cloud.example"})));
    assert!(h.query_by_label_contains("Signed in as ana on cloud.example").is_some());
    assert!(h.query_by_label("Sign out").is_some());
}

#[test]
fn open_a_workbook_from_a_nextcloud_folder() {
    let fake = Fake::new("open", true);
    let mut h = harness(&fake);
    page(&mut h, "open");
    h.get_by_label("Finance").click();
    h.run_steps(6);
    h.get_by_label("Budget.xlsx").click();
    h.run_steps(8);
    let open = fake.ops().into_iter().find(|(op, _)| op == "files.open").map(|(_, p)| p).unwrap();
    assert_eq!(open["path"], "Finance/Budget.xlsx");
    let app = h.state();
    assert!(app.backstage.is_none(), "the workbook shows");
    let shown = app
        .session
        .active()
        .and_then(|d| d.wb.sheets.first().and_then(|s| s.cell(gridcraft_engine::core::CellRef::parse("A1").unwrap()).map(|c| c.input_text())));
    assert_eq!(shown.as_deref(), Some("From the cloud"));
    assert_eq!(app.cloud_location().as_deref(), Some("Finance/Budget.xlsx"));
    assert!(app.autosave_on(), "workbooks in Nextcloud AutoSave");
    assert!(!app.grid.autosave(), "the AutoSave option for other workbooks is unchanged");
}

#[test]
fn save_as_into_nextcloud_asks_before_replacing() {
    let fake = Fake::new("saveas", true);
    let mut h = harness(&fake);
    h.state_mut().run("cell.set", json!({"cell": "B2", "input": "42"})).unwrap();
    page(&mut h, "saveAs");
    h.get_by_label("Finance").click();
    h.run_steps(6);
    // The name starts as the workbook's own; type another.
    assert_eq!(h.state().cloud_files.name, "Book1");
    h.state_mut().cloud_files.name.clear();
    h.run_steps(2);
    h.get_by_label("Nextcloud file name").click();
    h.get_by_label("Nextcloud file name").type_text("Forecast");
    h.run_steps(2);
    h.get_by_label("  Save  ").click();
    h.run_steps(6);
    assert!(h.query_by_label_contains("Forecast.xlsx is already there").is_some());
    h.get_by_label("Replace").click();
    h.run_steps(8);
    let asks: Vec<Json> = fake.ops().into_iter().filter(|(op, _)| op == "files.saveAs").map(|(_, p)| p).collect();
    assert_eq!(
        asks,
        [json!({"folder": "Finance", "name": "Forecast", "replace": false}), json!({"folder": "Finance", "name": "Forecast", "replace": true})]
    );
    let app = h.state();
    let d = app.session.active().unwrap();
    assert!(d.path.as_deref().is_some_and(|p| p.ends_with("Forecast.xlsx")) && !d.is_dirty(), "saved to its copy");
    assert_eq!(app.cloud_location().as_deref(), Some("Finance/Forecast.xlsx"));
}

#[test]
fn autosave_can_be_switched_off_for_one_nextcloud_workbook() {
    let fake = Fake::new("autosave", true);
    let mut h = harness(&fake);
    page(&mut h, "open");
    h.get_by_label("Finance").click();
    h.run_steps(6);
    h.get_by_label("Budget.xlsx").click();
    h.run_steps(8);
    assert!(h.state().autosave_on());
    h.state_mut().toggle_autosave();
    assert!(!h.state().autosave_on(), "off for this workbook");
    h.state_mut().run("file.new", json!({})).unwrap();
    assert!(!h.state().autosave_on(), "a new local workbook follows the AutoSave option (off)");
}

/// Opens Finance/Budget.xlsx from the fake Nextcloud, types an edit, and has the host report
/// that someone else changed the file meanwhile.
fn conflicted(name: &str) -> (Fake, egui_kittest::Harness<'static, SheetApp>, String) {
    let fake = Fake::new(name, true);
    let mut h = harness(&fake);
    page(&mut h, "open");
    h.get_by_label("Finance").click();
    h.run_steps(6);
    h.get_by_label("Budget.xlsx").click();
    h.run_steps(8);
    h.state_mut().run("cell.set", json!({"cell": "B2", "input": "My change"})).unwrap();
    h.state_mut().run("file.save", json!({})).unwrap();
    let local = h.state().session.active().and_then(|d| d.path.clone()).unwrap();
    h.state_mut().cloud_files.conflicts.push(gridcraft_ui_egui::cloud_files::Conflict { local: local.clone(), name: "Budget.xlsx".into() });
    h.run_steps(4);
    assert!(h.query_by_label_contains("Budget.xlsx changed in Nextcloud").is_some(), "the user is asked");
    (fake, h, local)
}

fn input(app: &SheetApp, cell: &str) -> String {
    let at = gridcraft_engine::core::CellRef::parse(cell).unwrap();
    app.session.active().and_then(|d| d.wb.sheets.first().and_then(|s| s.cell(at).map(|c| c.input_text()))).unwrap_or_default()
}

#[test]
fn a_conflict_can_keep_both_versions() {
    let (fake, mut h, local) = conflicted("keepboth");
    h.get_by_label("Keep both").click();
    h.run_steps(8);
    let resolve = fake.ops().into_iter().find(|(op, _)| op == "files.resolve").map(|(_, p)| p);
    assert_eq!(resolve, Some(json!({"localPath": local, "choice": "keepBoth"})));
    let app = h.state();
    assert!(app.cloud_files.conflicts.is_empty() && h.query_by_label_contains("changed in Nextcloud").is_none(), "asked once");
    assert!(app.session.active().and_then(|d| d.path.clone()).is_some_and(|p| p.ends_with("Budget (copy).xlsx")), "the workbook is now the copy");
    assert_eq!(input(app, "B2"), "My change", "with the changes made here");
    assert_eq!(app.cloud_location().as_deref(), Some("Finance/Budget (copy).xlsx"));
}

#[test]
fn a_conflict_can_take_nextclouds_version() {
    let (_fake, mut h, local) = conflicted("theirs");
    h.get_by_label("Use Nextcloud's").click();
    h.run_steps(8);
    let app = h.state();
    assert_eq!(app.session.documents().iter().filter(|d| d.path.as_deref() == Some(local.as_str())).count(), 1, "reopened, not twice");
    assert_eq!(input(app, "A1"), "Their version");
    assert_eq!(input(app, "B2"), "", "the change made here is gone");
}

#[test]
fn the_browser_has_no_nextcloud_pages() {
    let mut session = Session::new();
    session.new_workbook();
    let cloud = Cloud::sharing(Box::new(|_, _, _| {}), CloudInbox::default());
    let app = SheetApp::new(session, Services { cloud: Some(cloud), ..Default::default() });
    assert!(!gridcraft_ui_egui::cloud_files::available(&app), "the page owns the files in the browser");
}
