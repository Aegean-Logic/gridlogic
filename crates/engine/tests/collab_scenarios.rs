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

/// Sends whatever either has to send and delivers the log to both, until nobody has anything
/// left (a server with no delays).
fn settle(ana: &mut Person, ben: &mut Person, log: &mut Vec<Incoming>) {
    for _ in 0..50 {
        for p in [&mut *ana, &mut *ben] {
            if let Some(out) = p.replica.outgoing() {
                log.push(Incoming { seq: log.len() as u64 + 1, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op });
            }
        }
        for p in [&mut *ana, &mut *ben] {
            while (p.replica.seq() as usize) < log.len() {
                let msg = log[p.replica.seq() as usize].clone();
                let Ok(d) = p.session.doc_mut() else { panic!("no document") };
                if let Err(e) = p.replica.receive(msg, d) {
                    panic!("{e}");
                }
            }
        }
        if !ana.replica.has_pending() && !ben.replica.has_pending() {
            return;
        }
    }
    panic!("never settled");
}

#[test]
fn undo_takes_back_only_my_own_edit() {
    let (mut ana, mut ben) = two(&[]);
    let mut log = Vec::new();
    run(&mut ana, "cell.set", json!({"cell": "A1", "input": "Ana's"}));
    run(&mut ben, "cell.set", json!({"cell": "B1", "input": "Ben's"}));
    settle(&mut ana, &mut ben, &mut log);
    run(&mut ana, "edit.undo", json!({}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "A1"), "", "Ana's edit is undone for both");
    assert_eq!(shown(&ana, &ben, "B1"), "Ben's", "Ben's edit stays");
}

#[test]
fn undo_keeps_a_later_change_by_someone_else() {
    let (mut ana, mut ben) = two(&[]);
    let mut log = Vec::new();
    run(&mut ana, "cell.set", json!({"cell": "A1", "input": "Ana's"}));
    settle(&mut ana, &mut ben, &mut log);
    run(&mut ben, "cell.set", json!({"cell": "A1", "input": "Ben's correction"}));
    settle(&mut ana, &mut ben, &mut log);
    run(&mut ana, "edit.undo", json!({}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "A1"), "Ben's correction", "undo never removes someone else's later edit");
}

#[test]
fn undoing_an_insert_keeps_what_others_typed() {
    let (mut ana, mut ben) = two(&[("A10", "row ten")]);
    let mut log = Vec::new();
    run(&mut ana, "home.insertRows", json!({"rows": "3:4"}));
    run(&mut ben, "cell.set", json!({"cell": "B10", "input": "Ben's note"}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "B12"), "Ben's note");
    run(&mut ana, "edit.undo", json!({}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "A10"), "row ten", "the rows Ana inserted are gone again");
    assert_eq!(shown(&ana, &ben, "B10"), "Ben's note", "Ben's note moved back up with its row");
}

#[test]
fn undoing_a_delete_brings_the_rows_back() {
    let (mut ana, mut ben) = two(&[("A5", "keep me"), ("A6", "below")]);
    let mut log = Vec::new();
    run(&mut ana, "home.deleteRows", json!({"rows": "5:5"}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "A5"), "below");
    run(&mut ana, "edit.undo", json!({}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "A5"), "keep me");
    assert_eq!(shown(&ana, &ben, "A6"), "below");
}

#[test]
fn redo_puts_an_undone_edit_back() {
    let (mut ana, mut ben) = two(&[("C3", "old")]);
    let mut log = Vec::new();
    run(&mut ana, "cell.set", json!({"cell": "C3", "input": "new"}));
    run(&mut ana, "edit.undo", json!({}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "C3"), "old");
    run(&mut ana, "edit.redo", json!({}));
    settle(&mut ana, &mut ben, &mut log);
    assert_eq!(shown(&ana, &ben, "C3"), "new");
    // A fresh edit clears what could be redone.
    run(&mut ana, "edit.undo", json!({}));
    run(&mut ana, "cell.set", json!({"cell": "D3", "input": "x"}));
    assert!(ana.replica.run(&mut ana.session, "edit.redo", json!({})).is_err(), "nothing to redo after a fresh edit");
    // Ben has nothing of his own to undo.
    assert!(ben.replica.run(&mut ben.session, "edit.undo", json!({})).is_err(), "Ana's edits aren't Ben's to undo");
}

/// Co-editing through the session itself: every command path (here `Session::run`, as the UI
/// and agents use it) shares its edits, a command that runs other commands sends one op, and
/// the two copies end up the same.
#[test]
fn a_shared_session_shares_every_edit() {
    let open = |client: u64| {
        let mut s = Session::new();
        s.add_document(DocState::new(Workbook::new(), None, "Shared".into()));
        for (c, v) in [("A1", "Region"), ("B1", "Sales"), ("A2", "North"), ("B2", "10"), ("A3", "North"), ("B3", "20"), ("A4", "South"), ("B4", "5")]
        {
            if let Err(e) = s.run("cell.set", json!({"cell": c, "input": v})) {
                panic!("{e}");
            }
        }
        if let Err(e) = s.collab_start(client, 0) {
            panic!("{e}");
        }
        s
    };
    let (mut ana, mut ben) = (open(1), open(2));
    for (id, params) in [
        ("cell.set", json!({"cell": "C1", "input": "Notes"})),
        // Subtotal inserts rows, sets cells, bolds and groups: one command, one op.
        ("data.subtotal", json!({"range": "A1:B4", "groupBy": "A", "columns": ["B"]})),
    ] {
        if let Err(e) = ana.run(id, params) {
            panic!("{id}: {e}");
        }
    }
    if let Err(e) = ben.run("cell.set", json!({"cell": "D2", "input": "Ben was here"})) {
        panic!("{e}");
    }
    // Ana's undo takes back her Subtotal (her last edit), not Ben's note.
    assert!(ana.run("edit.undo", json!({})).is_ok());
    assert!(ben.run("selection.set", json!({"range": "B2"})).is_ok());

    // A server: sequence whatever's sent, deliver to both, until quiet.
    let mut log: Vec<Incoming> = Vec::new();
    let mut seen = [0usize; 2];
    for _ in 0..50 {
        for s in [&mut ana, &mut ben] {
            if let Some(out) = s.collab_outgoing() {
                log.push(Incoming { seq: log.len() as u64 + 1, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op });
            }
        }
        for (i, s) in [&mut ana, &mut ben].into_iter().enumerate() {
            while let Some(msg) = log.get(seen[i]).cloned() {
                if let Err(e) = s.collab_receive(msg) {
                    panic!("{e}");
                }
                seen[i] += 1;
            }
        }
        if !ana.collab_pending() && !ben.collab_pending() {
            break;
        }
    }
    assert_eq!(log.len(), 4, "four edits, four ops (the subtotal's inner commands aren't sent separately)");
    let (Some((sa, a)), Some((sb, b))) = (ana.collab_confirmed(), ben.collab_confirmed()) else { panic!("not shared") };
    assert_eq!(sa, sb);
    let diff = gridcraft_engine::collab::make_patch(&a, &b).unwrap_or_else(|e| panic!("{e}"));
    assert!(diff.is_empty(), "{diff:?}");
    let shown = |s: &Session| s.active().map(|d| d.wb.clone()).unwrap_or_default();
    assert!(gridcraft_engine::collab::make_patch(&shown(&ana), &shown(&ben)).map(|p| p.is_empty()).unwrap_or(false));
    let input = |cell: &str| {
        let at = gridcraft_engine::core::CellRef::parse(cell).unwrap_or_default();
        shown(&ana).sheets.first().and_then(|s| s.cell(at).map(|c| c.input_text())).unwrap_or_default()
    };
    assert_eq!(input("A4"), "South", "the Subtotal is undone");
    assert_eq!(input("C1"), "Notes");
    assert_eq!(input("D2"), "Ben was here");
}
