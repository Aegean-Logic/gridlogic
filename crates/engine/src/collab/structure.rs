//! Structural edits: inserts, deletes and moves that shift cells.
//!
//! The commands record each one they make (`Ctx::record`) by sheet id. They travel ahead of an
//! op's content patch so that a receiver shifts its copy the same way before patching it, and so
//! that edits made at the same time can be moved to where the cells went.

use gridcraft_core::{MAX_COLS, MAX_ROWS, RangeRef};
use gridcraft_formula::adjust::{Axis, Edit};
use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result};

/// Rows or columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Dir {
    Rows,
    Cols,
}

impl From<Axis> for Dir {
    fn from(a: Axis) -> Dir {
        match a {
            Axis::Rows => Dir::Rows,
            Axis::Cols => Dir::Cols,
        }
    }
}

impl From<Dir> for Axis {
    fn from(d: Dir) -> Axis {
        match d {
            Dir::Rows => Axis::Rows,
            Dir::Cols => Axis::Cols,
        }
    }
}

/// A structural edit on the sheet with id `sheet` ([`gridcraft_model::Sheet::id`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum StructEdit {
    /// `count` rows or columns inserted before `at`.
    Insert { sheet: u64, dir: Dir, at: u32, count: u32 },
    /// `count` rows or columns deleted from `at`.
    Delete { sheet: u64, dir: Dir, at: u32, count: u32 },
    /// Cells inserted at `range`, pushing those below (`Rows`) or right (`Cols`) along.
    InsertCells { sheet: u64, dir: Dir, range: RangeRef },
    /// `range` deleted, pulling those below (`Rows`) or right (`Cols`) back.
    DeleteCells { sheet: u64, dir: Dir, range: RangeRef },
    /// A block cut and pasted with its top-left at (`to_row`, `to_col`) on sheet `to_sheet`.
    Move { sheet: u64, from: RangeRef, to_sheet: u64, to_row: u32, to_col: u32 },
}

impl StructEdit {
    /// `e` on the sheet at index `si` of `wb`. `None` when a sheet involved has no id yet (made
    /// by the same command): the content patch then carries the whole result.
    pub(crate) fn of(wb: &Workbook, si: usize, e: &Edit) -> Option<StructEdit> {
        let sheet = wb.sheet(si).map(|s| s.id).filter(|id| *id != 0)?;
        Some(match e {
            Edit::Insert { axis, at, count } => StructEdit::Insert { sheet, dir: (*axis).into(), at: *at, count: *count },
            Edit::Delete { axis, at, count } => StructEdit::Delete { sheet, dir: (*axis).into(), at: *at, count: *count },
            Edit::InsertCells { axis, range } => StructEdit::InsertCells { sheet, dir: (*axis).into(), range: *range },
            Edit::DeleteCells { axis, range } => StructEdit::DeleteCells { sheet, dir: (*axis).into(), range: *range },
            Edit::Move { from, to_row, to_col } => StructEdit::Move { sheet, from: *from, to_sheet: sheet, to_row: *to_row, to_col: *to_col },
            Edit::MoveToSheet { from, to_sheet, to_row, to_col } => {
                let to = wb.sheet_index(to_sheet).and_then(|i| wb.sheet(i)).map(|s| s.id).filter(|id| *id != 0)?;
                StructEdit::Move { sheet, from: *from, to_sheet: to, to_row: *to_row, to_col: *to_col }
            }
        })
    }

    /// The sheet it's on.
    pub fn sheet(&self) -> u64 {
        match self {
            StructEdit::Insert { sheet, .. }
            | StructEdit::Delete { sheet, .. }
            | StructEdit::InsertCells { sheet, .. }
            | StructEdit::DeleteCells { sheet, .. }
            | StructEdit::Move { sheet, .. } => *sheet,
        }
    }

    /// As the formula crate's [`Edit`], with the index of the sheet it's on in `wb`. `None` when
    /// a sheet it names is gone.
    pub fn edit(&self, wb: &Workbook) -> Option<(usize, Edit)> {
        let si = wb.sheet_by_id(self.sheet())?;
        let e = match self {
            StructEdit::Insert { dir, at, count, .. } => Edit::Insert { axis: (*dir).into(), at: *at, count: *count },
            StructEdit::Delete { dir, at, count, .. } => Edit::Delete { axis: (*dir).into(), at: *at, count: *count },
            StructEdit::InsertCells { dir, range, .. } => Edit::InsertCells { axis: (*dir).into(), range: *range },
            StructEdit::DeleteCells { dir, range, .. } => Edit::DeleteCells { axis: (*dir).into(), range: *range },
            StructEdit::Move { sheet, from, to_sheet, to_row, to_col } if sheet == to_sheet => {
                Edit::Move { from: *from, to_row: *to_row, to_col: *to_col }
            }
            StructEdit::Move { from, to_sheet, to_row, to_col, .. } => {
                let to = wb.sheet_by_id(*to_sheet).and_then(|i| wb.sheet(i))?;
                Edit::MoveToSheet { from: *from, to_sheet: to.name.clone(), to_row: *to_row, to_col: *to_col }
            }
        };
        Some((si, e))
    }
}

impl StructEdit {
    /// Not empty, and within the worksheet: an op from elsewhere is hostile input.
    pub fn is_valid(&self) -> bool {
        let lines = |dir: Dir, at: u32, count: u32| {
            let limit = match dir {
                Dir::Rows => MAX_ROWS,
                Dir::Cols => MAX_COLS,
            };
            count > 0 && at < limit && count <= limit - at
        };
        let range = |r: &RangeRef| r.start.row <= r.end.row && r.start.col <= r.end.col && r.end.row < MAX_ROWS && r.end.col < MAX_COLS;
        match self {
            StructEdit::Insert { dir, at, count, .. } | StructEdit::Delete { dir, at, count, .. } => lines(*dir, *at, *count),
            StructEdit::InsertCells { range: r, .. } | StructEdit::DeleteCells { range: r, .. } => range(r),
            StructEdit::Move { from, to_row, to_col, .. } => range(from) && *to_row < MAX_ROWS && *to_col < MAX_COLS,
        }
    }
}

/// Shifts `wb` the way `e` did on the author's copy: cells, everything positioned on the sheet,
/// and references. Moves aren't replayed (the content patch carries the moved cells); they're
/// recorded so edits made at the same time can follow them. A sheet that's gone is skipped.
pub fn replay(wb: &mut Workbook, e: &StructEdit) -> Result<()> {
    if !e.is_valid() {
        return Err(EngineError::Other(format!("co-editing: an edit outside the worksheet: {e:?}")));
    }
    if matches!(e, StructEdit::Move { .. }) {
        return Ok(());
    }
    let Some((si, edit)) = e.edit(wb) else { return Ok(()) };
    // Like the commands, never push cells off the end of the sheet.
    if let (StructEdit::Insert { dir, at, count, .. }, Some(used)) = (e, wb.sheet(si).and_then(|s| s.used_range())) {
        let (last, limit) = match dir {
            Dir::Rows => (used.end.row, MAX_ROWS),
            Dir::Cols => (used.end.col, MAX_COLS),
        };
        if last >= *at && u64::from(last) + u64::from(*count) >= u64::from(limit) {
            return Err(EngineError::Other("co-editing: the insert would push cells off the worksheet".into()));
        }
    }
    crate::cmd::sheet::apply_shift(wb, si, &edit)
}
