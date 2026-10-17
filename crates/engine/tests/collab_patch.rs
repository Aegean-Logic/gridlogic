//! Co-editing content patches: whatever a command does, the patch diffed from its before and
//! after workbooks, applied to another copy of the before workbook, gives the same workbook:
//! same content, styles, settings and recalculated values.

use std::sync::Arc;

use gridcraft_engine::model::Workbook;
use gridcraft_engine::{DocState, Session, collab};
use serde_json::{Value as Json, json};

/// Runs `id` on `s` and checks that its patch reproduces the result on a separate copy.
fn replicate(s: &mut Session, id: &str, params: Json) {
    let before: Arc<Workbook> = s.active().map(|d| d.wb.clone()).unwrap_or_default();
    if let Err(e) = s.run(id, params.clone()) {
        panic!("{id} {params}: {e}");
    }
    let after: Arc<Workbook> = s.active().map(|d| d.wb.clone()).unwrap_or_default();
    let patch = collab::make_patch(&before, &after).unwrap_or_else(|e| panic!("{id}: diff: {e}"));

    // It travels as JSON.
    let wire = serde_json::to_string(&patch).unwrap_or_else(|e| panic!("{id}: {e}"));
    let patch: collab::Patch = serde_json::from_str(&wire).unwrap_or_else(|e| panic!("{id}: {e}\n{wire}"));

    // Someone else's copy of the workbook as it was before the command.
    let mut other = DocState::new((*before).clone(), None, "other".into());
    collab::apply_to_doc(&mut other, &patch).unwrap_or_else(|e| panic!("{id}: apply: {e}"));

    let rest = collab::make_patch(&after, &other.wb).unwrap_or_else(|e| panic!("{id}: {e}"));
    assert!(rest.is_empty(), "{id} {params}: the copy differs after the patch: {rest:?}\npatch: {wire}");
    assert_same_values(id, &after, &other.wb);
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
fn every_kind_of_edit_replicates_through_its_patch() {
    let mut s = Session::new();
    if let Err(e) = s.run("file.new", json!({"sample": "sales"})) {
        panic!("sample: {e}");
    }
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
        // Rows and columns.
        ("home.rowHeight", json!({"rows": "20:21", "height": 30})),
        ("home.hideRows", json!({"rows": "23:24"})),
        ("home.insertRows", json!({"rows": "6:7"})),
        ("home.deleteColumns", json!({"cols": "H:H"})),
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
    let mut s = Session::new();
    if let Err(e) = s.run("file.new", json!({"sample": "sales"})) {
        panic!("sample: {e}");
    }
    let before = s.active().map(|d| d.wb.clone()).unwrap_or_default();
    // Selecting and switching sheets is each person's own view.
    for (id, params) in [("selection.set", json!({"range": "D7"})), ("sheet.activate", json!({"sheet": 0}))] {
        if let Err(e) = s.run(id, params) {
            panic!("{id}: {e}");
        }
    }
    let after = s.active().map(|d| d.wb.clone()).unwrap_or_default();
    let patch = collab::make_patch(&before, &after).unwrap_or_else(|e| panic!("{e}"));
    assert!(patch.is_empty(), "{patch:?}");
}
