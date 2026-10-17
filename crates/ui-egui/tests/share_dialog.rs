//! The Share dialog, driven against a fake cloud that answers at once: who you share with and
//! how, sending, copying the link, managing access, and the desktop's sign-in and upload steps.

use std::sync::{Arc, Mutex};

use egui::vec2;
use egui_kittest::kittest::Queryable;
use gridcraft_engine::Session;
use gridcraft_ui_egui::share::{Cloud, CloudInbox};
use gridcraft_ui_egui::{Services, SheetApp};
use serde_json::{Value as Json, json};

/// A cloud with a scripted state, recording every request.
#[derive(Clone)]
struct Fake {
    requests: Arc<Mutex<Vec<(String, Json)>>>,
    info: Arc<Mutex<Json>>,
}

impl Fake {
    fn new(info: Json) -> Fake {
        Fake { requests: Arc::default(), info: Arc::new(Mutex::new(info)) }
    }
    fn cloud(&self) -> Cloud {
        let inbox: CloudInbox = Arc::default();
        let answers = inbox.clone();
        let me = self.clone();
        Cloud {
            request: Box::new(move |id, op, params| {
                me.requests.lock().unwrap().push((op.to_string(), params.clone()));
                let answer = match op {
                    "share.info" => me.info.lock().unwrap().clone(),
                    "account.signIn" => {
                        *me.info.lock().unwrap() = json!({"name": "Budget.xlsx", "signedIn": true, "inCloud": false, "canShare": true});
                        me.info.lock().unwrap().clone()
                    }
                    "share.upload" => {
                        *me.info.lock().unwrap() = json!({"name": "Budget.xlsx", "signedIn": true, "inCloud": true, "canShare": true});
                        me.info.lock().unwrap().clone()
                    }
                    "share.search" => json!([
                        {"label": "Ana Lopez", "detail": "ana@example.com", "type": "user", "shareWith": "ana"},
                        {"label": "Finance", "type": "group", "shareWith": "finance"}
                    ]),
                    "share.list" => json!([
                        {"id": 7, "type": "user", "shareWith": "ben", "label": "Ben", "edit": true, "password": false},
                        {"id": 9, "type": "link", "shareWith": "", "label": "", "edit": false, "url": "https://cloud.example/s/abc", "password": false}
                    ]),
                    "share.link" => json!({"id": 9, "type": "link", "url": "https://cloud.example/s/abc"}),
                    _ => json!({}),
                };
                answers.lock().unwrap().push((id, Ok(answer)));
            }),
            inbox,
        }
    }
    fn ops(&self) -> Vec<(String, Json)> {
        self.requests.lock().unwrap().clone()
    }
}

fn harness(fake: &Fake) -> egui_kittest::Harness<'static, SheetApp> {
    let mut session = Session::new();
    session.new_workbook();
    let services = Services { cloud: Some(fake.cloud()), ..Default::default() };
    let mut h = egui_kittest::Harness::builder().with_size(vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut SheetApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        SheetApp::new(session, services),
    );
    h.run_steps(4);
    h
}

fn open(h: &mut egui_kittest::Harness<'static, SheetApp>) {
    let r = h.state_mut().run("ui.share", json!({}));
    assert!(r.is_ok(), "{r:?}");
    h.run_steps(6);
}

fn shared_file() -> Json {
    json!({"name": "Budget.xlsx", "signedIn": true, "inCloud": true, "canShare": true})
}

#[test]
fn share_with_people_and_send() {
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    assert!(h.query_by_label_contains("Budget.xlsx").is_some(), "the title names the workbook");
    assert!(h.query_by_label_contains("Shared with Ben").is_some(), "who has access, from the cloud");

    // Type a name: suggestions come from the cloud's search.
    h.get_by_label("Add a name, group, or email").click();
    h.run_steps(2);
    h.get_by_label("Add a name, group, or email").type_text("an");
    for _ in 0..20 {
        h.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert!(fake.ops().iter().any(|(op, p)| op == "share.search" && p["query"] == "an"), "{:?}", fake.ops());
    h.get_by_label_contains("Ana Lopez").click();
    h.run_steps(4);

    // A message, then Send: one share per person, with the permission and the message.
    h.get_by_label("Add a message").click();
    h.get_by_label("Add a message").type_text("Please check Q3");
    h.run_steps(2);
    h.get_by_label("  Send  ").click();
    h.run_steps(6);
    let adds: Vec<Json> = fake.ops().into_iter().filter(|(op, _)| op == "share.add").map(|(_, p)| p).collect();
    assert_eq!(adds, vec![json!({"type": "user", "shareWith": "ana", "edit": true, "note": "Please check Q3"})]);
    assert!(h.query_by_label_contains("Shared. They'll get a notification.").is_some());
}

#[test]
fn copy_link() {
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label("🔗 Copy link").click();
    h.run_steps(6);
    let link = fake.ops().into_iter().find(|(op, _)| op == "share.link").map(|(_, p)| p);
    assert_eq!(link, Some(json!({"edit": false, "expiration": null})), "a view link by default");
    assert!(h.query_by_label_contains("Link copied.").is_some());
}

#[test]
fn manage_access() {
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label_contains("Shared with Ben").click();
    h.run_steps(4);
    assert!(h.query_by_label_contains("People with access").is_some());
    if let Some(n) = h.get_all_by_label("Remove").next() {
        n.click();
    }
    h.run_steps(4);
    assert!(fake.ops().iter().any(|(op, p)| op == "share.remove" && p["id"] == 7), "{:?}", fake.ops());
}

#[test]
fn desktop_signs_in_then_uploads_before_sharing() {
    let fake = Fake::new(json!({"name": "Budget.xlsx", "signedIn": false, "inCloud": false, "canShare": true}));
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label("Nextcloud server address").click();
    h.get_by_label("Nextcloud server address").type_text("cloud.example");
    h.run_steps(2);
    h.get_by_label("Sign in to Nextcloud…").click();
    h.run_steps(6);
    h.get_by_label("Upload to Nextcloud").click();
    h.run_steps(6);
    assert!(h.query_by_label("Add a name, group, or email").is_some(), "uploaded: ready to share");
    let ops: Vec<String> = fake.ops().into_iter().map(|(op, _)| op).collect();
    assert_eq!(&ops[..3], ["share.info", "account.signIn", "share.upload"]);
    assert_eq!(fake.ops()[1].1, json!({"server": "cloud.example"}), "signs in where the user said");
}

#[test]
fn without_a_cloud_share_is_the_export_menu() {
    let mut session = Session::new();
    session.new_workbook();
    let mut app = SheetApp::new(session, Services::default());
    assert!(app.run("ui.share", json!({})).is_err(), "no cloud: no Share dialog");
}
