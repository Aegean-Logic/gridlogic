//! Stable sheet ids: every sheet has one, it survives renames and moves, and a new or copied
//! sheet gets a fresh one (co-editing ops name sheets by id).

use gridcraft_engine::Session;
use serde_json::json;

fn ids(s: &Session) -> Vec<(String, u64)> {
    s.active().map(|d| d.wb.sheets.iter().map(|sh| (sh.name.clone(), sh.id)).collect()).unwrap_or_default()
}

fn run(s: &mut Session, id: &str, params: serde_json::Value) {
    if let Err(e) = s.run(id, params) {
        panic!("{id}: {e}");
    }
}

#[test]
fn sheets_keep_their_ids_through_renames_and_moves() {
    let mut s = Session::new();
    run(&mut s, "file.new", json!({}));
    run(&mut s, "home.insertSheet", json!({"name": "B"}));
    run(&mut s, "home.insertSheet", json!({"name": "C"}));
    let before = ids(&s);
    assert!(before.iter().all(|(_, id)| *id != 0), "{before:?}");
    let mut unique: Vec<u64> = before.iter().map(|(_, id)| *id).collect();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), before.len(), "{before:?}");

    let id_of = |s: &Session, name: &str| ids(s).into_iter().find(|(n, _)| n == name).map(|(_, id)| id);
    let b = id_of(&s, "B");
    run(&mut s, "sheet.rename", json!({"sheet": "B", "name": "Budget"}));
    assert_eq!(id_of(&s, "Budget"), b, "a rename keeps the id");
    run(&mut s, "sheet.move", json!({"sheet": "Budget", "to": 0}));
    assert_eq!(id_of(&s, "Budget"), b, "a move keeps the id");
}

#[test]
fn new_and_copied_sheets_get_fresh_ids_and_undo_restores() {
    let mut s = Session::new();
    run(&mut s, "file.new", json!({}));
    run(&mut s, "home.insertSheet", json!({"name": "B"}));
    let start = ids(&s);
    let max = start.iter().map(|(_, id)| *id).max().unwrap_or(0);

    run(&mut s, "sheet.move", json!({"sheet": "B", "to": 0, "copy": true}));
    let copied = ids(&s);
    assert_eq!(copied.len(), start.len() + 1);
    let copy = copied.iter().find(|(n, _)| n == "B (2)").map(|(_, id)| *id);
    assert_eq!(copy, Some(max + 1), "{copied:?}");
    assert_eq!(copied.iter().find(|(n, _)| n == "B"), start.iter().find(|(n, _)| n == "B"), "the original keeps its id");

    // A new sheet goes above every id in use. (Ids above that may come back after a delete;
    // co-editing gives sheets made during a session ids of their own.)
    run(&mut s, "home.insertSheet", json!({"name": "D"}));
    assert_eq!(ids(&s).iter().find(|(n, _)| n == "D").map(|(_, id)| *id), Some(max + 2));
    run(&mut s, "home.deleteSheet", json!({"sheet": "B (2)"}));

    run(&mut s, "edit.undo", json!({"steps": 3}));
    assert_eq!(ids(&s), start, "undo brings back the sheets with their ids");
}

#[test]
fn json_keeps_ids_and_a_fresh_load_numbers_sheets_in_order() {
    let mut s = Session::new();
    run(&mut s, "file.new", json!({}));
    run(&mut s, "home.insertSheet", json!({"name": "B"}));
    run(&mut s, "home.deleteSheet", json!({"sheet": 0}));
    run(&mut s, "home.insertSheet", json!({"name": "C"}));
    let edited = ids(&s);

    for (format, keeps) in [("json", true), ("xlsx", false)] {
        let saved = s.run("file.saveBytes", json!({"format": format})).unwrap_or_else(|e| panic!("{format}: {e}"));
        let mut t = Session::new();
        run(&mut t, "file.open", json!({"name": format!("book.{format}"), "base64": saved["base64"]}));
        let loaded = ids(&t);
        let names = |v: &[(String, u64)]| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        assert_eq!(names(&loaded), names(&edited), "{format}");
        if keeps {
            assert_eq!(loaded, edited, "JSON keeps the ids");
        } else {
            let fresh: Vec<u64> = (1..=loaded.len() as u64).collect();
            assert_eq!(loaded.iter().map(|(_, id)| *id).collect::<Vec<_>>(), fresh, "XLSX numbers sheets afresh");
        }
    }
}
