//! Co-editing ops: whatever a command does, its op (structural edits plus a content patch),
//! applied to another copy of the before workbook, gives the same workbook: same content,
//! styles, settings and recalculated values.

use std::sync::Arc;

use gridcraft_engine::model::Workbook;
use gridcraft_engine::{DocState, Session, collab};
use serde_json::{Value as Json, json};

fn sales() -> Session {
    let mut s = Session::new();
    if let Err(e) = s.run("file.new", json!({"sample": "sales"})) {
        panic!("sample: {e}");
    }
    s
}

/// Runs `id` on `s` and checks that its op reproduces the result on a separate copy.
fn replicate(s: &mut Session, id: &str, params: Json) -> collab::Op {
    let before: Arc<Workbook> = s.active().map(|d| d.wb.clone()).unwrap_or_default();
    let (_, op) = collab::run_captured(s, id, params.clone()).unwrap_or_else(|e| panic!("{id} {params}: {e}"));
    let after: Arc<Workbook> = s.active().map(|d| d.wb.clone()).unwrap_or_default();

    // It travels as JSON.
    let wire = serde_json::to_string(&op).unwrap_or_else(|e| panic!("{id}: {e}"));
    let op: collab::Op = serde_json::from_str(&wire).unwrap_or_else(|e| panic!("{id}: {e}\n{wire}"));

    // Someone else's copy of the workbook as it was before the command.
    let mut other = DocState::new((*before).clone(), None, "other".into());
    collab::apply_op(&mut other, &op).unwrap_or_else(|e| panic!("{id}: apply: {e}"));

    let rest = collab::make_patch(&after, &other.wb).unwrap_or_else(|e| panic!("{id}: {e}"));
    assert!(rest.is_empty(), "{id} {params}: the copy differs after the op: {rest:?}\nop: {wire}");
    assert_same_values(id, &after, &other.wb);
    op
}

/// Every cell's value, cached formula results included, matches.
fn assert_same_values(id: &str, a: &Workbook, b: &Workbook) {
    assert_eq!(a.sheets.len(), b.sheets.len(), "{id}: sheet count");
    for (sa, sb) in a.sheets.iter().zip(&b.sheets) {
        assert_eq!(sa.id, sb.id, "{id}: sheet order");
        for (at, cell) in sa.cells.iter() {
            let other = sb.cells.get(at).map(|c| &c.value);
            assert_eq!(Some(&cell.value), other.or(Some(&Default::default())), "{id}: {}!{at} value", sa.name);
        }
        for (at, cell) in sb.cells.iter() {
            assert!(sa.cells.get(at).is_some() || cell.value.is_empty(), "{id}: {}!{at} only in the copy", sb.name);
        }
    }
}

#[test]
fn every_kind_of_edit_replicates_through_its_op() {
    let mut s = sales();
    let steps: Vec<(&str, Json)> = vec![
        // Cells: constants, formulas, a dependent recalculation, clearing.
        ("cell.set", json!({"cell": "B20", "input": "Notes"})),
        ("cell.set", json!({"cell": "C20", "input": "=SUM(C5:C10)*2"})),
        ("cell.set", json!({"cell": "C5", "input": "99999"})),
        ("range.setValues", json!({"range": "E20:F21", "values": [[1, "two"], [true, 4.5]]})),
        ("edit.clearContents", json!({"range": "E21:F21"})),
        // Formatting.
        ("home.bold", json!({"range": "B20:C20"})),
        ("home.fillColor", json!({"range": "B20", "color": "#FFEE00"})),
        ("home.fontSize", json!({"range": "C20", "size": 15})),
        ("home.numberFormat", json!({"range": "C20", "format": "Currency"})),
        ("home.borders", json!({"range": "B20:C21", "preset": "thickOutside"})),
        ("home.wrapText", json!({"range": "B20", "on": true})),
        ("home.mergeCenter", json!({"range": "B22:D22"})),
        // Rows, columns and cells: shifts and moves.
        ("home.rowHeight", json!({"rows": "20:21", "height": 30})),
        ("home.hideRows", json!({"rows": "23:24"})),
        ("home.insertRows", json!({"rows": "6:7"})),
        ("home.deleteColumns", json!({"cols": "H:H"})),
        ("home.deleteRows", json!({"rows": "25:25,27:28"})),
        ("home.insertCells", json!({"range": "C30:D31", "shift": "down"})),
        ("home.deleteCells", json!({"range": "C30:C31", "shift": "left"})),
        ("sheet.moveColumns", json!({"cols": "E:E", "before": 1})),
        ("edit.cut", json!({"range": "B20:C21"})),
        ("edit.paste", json!({"at": "B40"})),
        // Sheet features.
        (
            "home.conditionalFormat",
            json!({"range": "C5:C12", "rule": {"type": "cellIs", "operator": "greater", "value": 50000, "preset": "lightRedFill"}}),
        ),
        ("data.validation", json!({"range": "B30:B40", "type": "list", "formula1": "\"Yes,No\""})),
        ("review.newNote", json!({"cell": "B20", "text": "Check this", "author": "Ana"})),
        ("insert.chart", json!({"range": "B4:C10", "type": "line", "at": "K2"})),
        ("insert.sparkline", json!({"range": "C5:F5", "location": "J5"})),
        ("view.freezePanes", json!({"cell": "C5"})),
        ("formulas.defineName", json!({"name": "Q1Sales", "refersTo": "=Sales!$C$5:$C$12"})),
        ("cell.set", json!({"cell": "C24", "input": "=SUM(Q1Sales)"})),
        // Sheets: add, rename, reorder, copy, delete, colour.
        ("home.insertSheet", json!({"name": "Plan"})),
        ("cell.set", json!({"cell": "Plan!A1", "input": "=Sales!C20+1"})),
        ("sheet.rename", json!({"sheet": "Plan", "name": "Plan 2027"})),
        ("sheet.move", json!({"sheet": "Plan 2027", "to": 0})),
        ("sheet.move", json!({"sheet": "Sales", "to": 0, "copy": true})),
        ("sheet.tabColor", json!({"sheet": "Plan 2027", "color": "#3366FF"})),
        ("home.deleteSheet", json!({"sheet": "Plan 2027"})),
        ("sheet.activate", json!({"sheet": "Sales"})),
        // Tables and protection.
        ("cell.set", json!({"cell": "M2", "input": "Region"})),
        ("cell.set", json!({"cell": "N2", "input": "Target"})),
        ("cell.set", json!({"cell": "M3", "input": "North"})),
        ("cell.set", json!({"cell": "N3", "input": "10"})),
        ("insert.table", json!({"range": "M2:N3", "name": "Targets"})),
        ("review.protectSheet", json!({})),
        ("review.unprotectSheet", json!({})),
        // Undo and redo are edits too, as far as the others are concerned.
        ("edit.undo", json!({})),
        ("edit.redo", json!({})),
    ];
    for (id, params) in steps {
        replicate(&mut s, id, params);
    }
}

#[test]
fn an_edit_that_changes_nothing_sends_nothing() {
    let mut s = sales();
    // Selecting and switching sheets is each person's own view.
    for (id, params) in [("selection.set", json!({"range": "D7"})), ("sheet.activate", json!({"sheet": 0}))] {
        let (_, op) = collab::run_captured(&mut s, id, params).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(op.is_empty(), "{id}: {op:?}");
    }
}

/// A structural edit travels as the shift, not as every cell it moved: the others replay it.
#[test]
fn a_structural_edit_sends_the_shift_not_the_cells() {
    let mut s = sales();
    for (id, params) in [
        ("home.insertRows", json!({"rows": "3:4"})),
        ("home.deleteColumns", json!({"cols": "B:B"})),
        ("home.insertCells", json!({"range": "M20:M21", "shift": "right"})),
    ] {
        let op = replicate(&mut s, id, params);
        assert_eq!(op.edits.len(), 1, "{id}: {:?}", op.edits);
        let cells: usize = op.patch.sheets.iter().map(|sp| sp.cells.len()).sum();
        assert!(cells <= 4, "{id}: {cells} cells in the patch: the shift should account for them");
    }
}

/// An op naming a place outside the worksheet is refused, and the copy stays as it was.
#[test]
fn a_hostile_op_is_refused() {
    let s = sales();
    let Some(d) = s.active() else { panic!("no document") };
    let sheet = d.wb.sheets.first().map(|sh| sh.id).unwrap_or_default();
    let mut other = DocState::new((*d.wb).clone(), None, "other".into());
    let before = other.wb.clone();
    for edit in [
        json!({"kind": "insert", "sheet": sheet, "dir": "rows", "at": 0, "count": 0}),
        json!({"kind": "insert", "sheet": sheet, "dir": "rows", "at": 5, "count": 2_000_000_000u32}),
        json!({"kind": "delete", "sheet": sheet, "dir": "cols", "at": 20_000, "count": 1}),
        json!({"kind": "insertCells", "sheet": sheet, "dir": "rows", "range": {"start": {"row": 9, "col": 0}, "end": {"row": 2, "col": 0}}}),
        // Pushes the sales data off the bottom of the sheet.
        json!({"kind": "insert", "sheet": sheet, "dir": "rows", "at": 0, "count": 1_048_570}),
    ] {
        let op: collab::Op = serde_json::from_value(json!({"edits": [edit]})).unwrap_or_else(|e| panic!("{e}"));
        assert!(collab::apply_op(&mut other, &op).is_err(), "{op:?} was accepted");
        assert!(Arc::ptr_eq(&other.wb, &before), "a refused op changed the copy");
    }
}
