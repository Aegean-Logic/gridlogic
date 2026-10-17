//! Co-editing intent: when two people edit at the same moment, the merged workbook is what
//! they'd expect, not merely the same for both.

use gridcraft_engine::collab::{Incoming, Replica};
use gridcraft_engine::model::Workbook;
use gridcraft_engine::{DocState, Session};
use serde_json::{Value as Json, json};

struct Person {
    session: Session,
    replica: Replica,
}

/// Ana and Ben, each with a copy of a workbook with `cells`, at log position 0.
fn two(cells: &[(&str, &str)]) -> (Person, Person) {
    let make = |client: u64| {
        let mut session = Session::new();
        session.add_document(DocState::new(Workbook::new(), None, "Shared".into()));
        for (c, v) in cells {
            if let Err(e) = session.run("cell.set", json!({"cell": c, "input": v})) {
                panic!("{c}: {e}");
            }
        }
        let wb = session.active().map(|d| d.wb.clone()).unwrap_or_default();
        let replica = Replica::new(client, wb, 0).unwrap_or_else(|e| panic!("{e}"));
        Person { session, replica }
    };
    (make(1), make(2))
}

fn run(p: &mut Person, id: &str, params: Json) {
    if let Err(e) = p.replica.run(&mut p.session, id, params) {
        panic!("{id}: {e}");
    }
}

/// Both sent their edit before seeing the other's; the server ordered Ana's first. Delivers
/// everything to both.
fn race(ana: &mut Person, ben: &mut Person) {
    let mut log = Vec::new();
    for p in [&mut *ana, &mut *ben] {
        let Some(out) = p.replica.outgoing() else { panic!("nothing to send") };
        log.push(Incoming { seq: log.len() as u64 + 1, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op });
    }
    for p in [ana, ben] {
        for msg in &log {
            let Ok(d) = p.session.doc_mut() else { panic!("no document") };
            if let Err(e) = p.replica.receive(msg.clone(), d) {
                panic!("{e}");
            }
        }
    }
}

/// What both see in `cell` of the first sheet (they must agree).
fn shown(ana: &Person, ben: &Person, cell: &str) -> String {
    let get = |p: &Person| {
        let wb = p.session.active().map(|d| d.wb.clone()).unwrap_or_default();
        let at = gridcraft_engine::core::CellRef::parse(cell).unwrap_or_default();
        wb.sheets.first().and_then(|s| s.cell(at).map(|c| c.input_text())).unwrap_or_default()
    };
    let (a, b) = (get(ana), get(ben));
    assert_eq!(a, b, "Ana and Ben see different things in {cell}");
    a
}

#[test]
fn typing_below_rows_someone_inserts_moves_down_with_them() {
    let (mut ana, mut ben) = two(&[("A10", "row ten")]);
    run(&mut ana, "home.insertRows", json!({"rows": "3:4"}));
    run(&mut ben, "cell.set", json!({"cell": "B10", "input": "Ben's note"}));
    race(&mut ana, &mut ben);
    assert_eq!(shown(&ana, &ben, "A12"), "row ten");
    assert_eq!(shown(&ana, &ben, "B12"), "Ben's note", "Ben's note stays next to row ten");
    assert_eq!(shown(&ana, &ben, "B10"), "");
}

#[test]
fn typing_in_the_same_cell_keeps_the_later_entry() {
    let (mut ana, mut ben) = two(&[]);
    run(&mut ana, "cell.set", json!({"cell": "C3", "input": "Ana"}));
    run(&mut ben, "cell.set", json!({"cell": "C3", "input": "Ben"}));
    race(&mut ana, &mut ben);
    assert_eq!(shown(&ana, &ben, "C3"), "Ben", "the server took Ben's second");
}

#[test]
fn formatting_and_typing_the_same_cell_both_survive() {
    let (mut ana, mut ben) = two(&[("C3", "old")]);
    run(&mut ana, "home.bold", json!({"range": "C3", "on": true}));
    run(&mut ben, "cell.set", json!({"cell": "C3", "input": "new"}));
    race(&mut ana, &mut ben);
    assert_eq!(shown(&ana, &ben, "C3"), "new");
    let bold = ana.session.active().map(|d| {
        let at = gridcraft_engine::core::CellRef::parse("C3").unwrap_or_default();
        let sh = d.wb.sheets.first().cloned();
        sh.map(|s| d.wb.styles.get(s.style_id(at)).font.bold).unwrap_or(false)
    });
    assert_eq!(bold, Some(true), "Ana's bold survives Ben's typing");
}

#[test]
fn typing_in_rows_someone_deletes_goes_with_them() {
    let (mut ana, mut ben) = two(&[("A5", "keep"), ("A8", "after")]);
    run(&mut ana, "home.deleteRows", json!({"rows": "6:7"}));
    run(&mut ben, "cell.set", json!({"cell": "B6", "input": "lost"}));
    race(&mut ana, &mut ben);
    assert_eq!(shown(&ana, &ben, "A5"), "keep");
    assert_eq!(shown(&ana, &ben, "A6"), "after");
    assert_eq!(shown(&ana, &ben, "B6"), "", "the row Ben typed in is gone");
}

#[test]
fn a_formula_typed_during_an_insert_still_points_at_its_cells() {
    let (mut ana, mut ben) = two(&[("A5", "5"), ("A6", "6")]);
    run(&mut ana, "home.insertRows", json!({"rows": "2:2"}));
    run(&mut ben, "cell.set", json!({"cell": "C1", "input": "=SUM(A5:A6)"}));
    race(&mut ana, &mut ben);
    assert_eq!(shown(&ana, &ben, "C1"), "=SUM(A6:A7)", "the reference follows the inserted row");
}

#[test]
fn two_people_adding_a_sheet_keep_both() {
    let (mut ana, mut ben) = two(&[]);
    run(&mut ana, "home.insertSheet", json!({"name": "Ana's"}));
    run(&mut ben, "home.insertSheet", json!({"name": "Ben's"}));
    race(&mut ana, &mut ben);
    let names = |p: &Person| -> Vec<String> { p.session.active().map(|d| d.wb.sheets.iter().map(|s| s.name.clone()).collect()).unwrap_or_default() };
    let (a, b) = (names(&ana), names(&ben));
    assert_eq!(a, b);
    assert!(a.contains(&"Ana's".to_string()) && a.contains(&"Ben's".to_string()), "{a:?}");
}

#[test]
fn undo_is_refused_in_a_shared_workbook() {
    let (mut ana, _) = two(&[("A1", "1")]);
    run(&mut ana, "cell.set", json!({"cell": "A1", "input": "2"}));
    assert!(ana.replica.run(&mut ana.session, "edit.undo", json!({})).is_err(), "undo would take back other people's edits");
}
