//! Dynamic arrays whose spill areas collide: the one that already spilled keeps its cells and the
//! newcomers are #SPILL!, and a full recalculation (or an XLSX save and reload) agrees. Found by
//! `command_sequence_fuzz` (seed 0): the full recalculation laid the spills out again in row-major
//! order and gave F13's array the cells of C14's.

use gridcraft_core::{CellError, CellRef, Value};
use gridcraft_engine::calc::Calc;
use gridcraft_engine::model::Workbook;
use gridcraft_engine::{Session, io};
use serde_json::json;

fn value(wb: &Workbook, sheet: &str, address: &str) -> Value {
    wb.sheets.iter().find(|s| s.name == sheet).unwrap().value(CellRef::parse(address).unwrap())
}

#[test]
fn filled_arrays_over_an_existing_spill_are_blocked_after_a_full_recalc_too() {
    let mut s = Session::new();
    s.execute("file.new", json!({"sample": "sales"})).unwrap();
    s.execute("home.insertSheet", json!({"before": 1})).ok();
    s.execute("cell.set", json!({"cell": "C14", "input": "=TRANSPOSE(B18:B24)", "sheet": "Sheet1"})).ok();
    s.execute("range.fill", json!({"input": "=D14:E18", "range": "E13:F13"})).ok();
    let wb = s.doc().unwrap().wb.clone();

    let mut fresh = (*wb).clone();
    Calc::new().recalc_all(&mut fresh);
    let bytes = io::save_bytes(&wb, "spill.xlsx").unwrap();
    let (mut back, _) = io::open_bytes("spill.xlsx", &bytes).unwrap();
    Calc::new().recalc_all(&mut back);

    for (what, w) in [("in memory", &*wb), ("after a full recalc", &fresh), ("after a save and reload", &back)] {
        // C14 spills over C14:I14; E13's and F13's arrays would cover E14:G14 (and F13 itself).
        assert_eq!(value(w, "Sheet1", "C14"), Value::Number(0.0), "C14 {what}");
        assert_eq!(value(w, "Sheet1", "I14"), Value::Number(0.0), "I14 {what}");
        assert_eq!(value(w, "Sheet1", "E13"), Value::Error(CellError::Spill), "E13 {what}");
        assert_eq!(value(w, "Sheet1", "F13"), Value::Error(CellError::Spill), "F13 {what}");
        assert_eq!(value(w, "Sheet1", "G13"), Value::Empty, "G13 {what}");
    }
}

#[test]
fn the_array_entered_first_keeps_its_spill() {
    // A2's array runs across B2, B1's down through it: whichever was entered first spills.
    for (first, second) in [("A2", "B1"), ("B1", "A2")] {
        let mut s = Session::new();
        s.new_workbook();
        for at in [first, second] {
            let input = if at == "A2" { "=SEQUENCE(1,3)" } else { "=SEQUENCE(3)" };
            s.execute("cell.set", json!({"cell": at, "input": input})).unwrap();
        }
        let wb = s.doc().unwrap().wb.clone();
        let mut fresh = (*wb).clone();
        Calc::new().recalc_all(&mut fresh);
        let name = wb.sheets[0].name.clone();
        for (what, w) in [("in memory", &*wb), ("after a full recalc", &fresh)] {
            assert_eq!(value(w, &name, first), Value::Number(1.0), "{first} {what}");
            assert_eq!(value(w, &name, second), Value::Error(CellError::Spill), "{second} {what}");
            assert_eq!(value(w, &name, "B2"), Value::Number(2.0), "B2 {what}");
        }
    }
}
