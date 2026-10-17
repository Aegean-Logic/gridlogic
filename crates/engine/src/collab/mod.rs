//! Real-time co-editing (`docs/co-editing.md`).
//!
//! Several copies of one workbook stay the same by exchanging what each edit changed: a content
//! [`Patch`] diffed from the workbook before and after the edit (so every command replicates
//! without per-command code), plus the structural edits that move cells, so edits made at the
//! same time can be shifted to where they belong. No I/O here: the host moves ops between people.

mod patch;

pub use patch::{Applied, CellChange, Content, Field, Line, LineChange, Patch, SheetPatch, apply_patch, make_patch};

use std::sync::Arc;

use crate::{DocState, Result};

/// Applies `patch` to an open document and recalculates what it changed. Not undoable: the
/// change came from someone else.
pub fn apply_to_doc(d: &mut DocState, patch: &Patch) -> Result<()> {
    if patch.is_empty() {
        return Ok(());
    }
    let mut wb = (*d.wb).clone();
    let applied = apply_patch(&mut wb, patch)?;
    wb.ensure_sheet_ids();
    if applied.structural {
        d.calc.recalc_all(&mut wb);
    } else if !applied.cells.is_empty() {
        d.calc.cells_changed(&mut wb, &applied.cells);
    }
    // Someone else's edit doesn't make this copy unsaved, nor does it saved: keep the flag as it was.
    let dirty = d.is_dirty();
    d.wb = Arc::new(wb);
    if !dirty {
        d.saved = d.wb.clone();
    }
    d.revision += 1;
    Ok(())
}
