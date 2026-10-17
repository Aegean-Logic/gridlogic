//! Real-time co-editing (`docs/co-editing.md`).
//!
//! Several copies of one workbook stay the same by exchanging an [`Op`] per edit: the structural
//! edits it made (so cells shift the same way everywhere, and edits made at the same time can
//! follow them), then a content [`Patch`] diffed from the workbook before and after (so every
//! command replicates without per-command code). No I/O here: the host moves ops between people.

mod patch;
mod replica;
mod structure;
mod transform;

pub use patch::{Applied, CellChange, Content, Field, Line, LineChange, Patch, SheetPatch, apply_patch, make_patch};
pub use replica::{Incoming, Outgoing, Replica};
pub use structure::{Dir, StructEdit, replay};
pub use transform::rebase;

use std::sync::Arc;

use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{DocState, Result, Session};

/// One edit as the others receive it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Op {
    /// Structural edits, in order, replayed before the patch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<StructEdit>,
    /// Everything else, against the workbook after those edits.
    #[serde(default, skip_serializing_if = "Patch::is_empty")]
    pub patch: Patch,
}

impl Op {
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.patch.is_empty()
    }
}

/// The op for an edit that turned `before` into `after` and made the structural `edits`: the
/// patch is diffed against `before` with the edits replayed, so it holds only what the shifting
/// didn't produce (an insert's "format same as above", the cells a move carried).
pub fn capture(before: &Workbook, after: &Workbook, edits: Vec<StructEdit>) -> Result<Op> {
    let mut shifted = before.clone();
    for e in &edits {
        replay(&mut shifted, e)?;
    }
    Ok(Op { patch: make_patch(&shifted, after)?, edits })
}

/// Takes the structural edits the document's commands recorded since the last call.
pub fn take_struct_log(d: &mut DocState) -> Vec<StructEdit> {
    std::mem::take(&mut d.struct_log)
}

/// Runs a command on the active document and returns its result with the op for the others.
/// The op is empty when the command changed nothing they'd see, or switched documents.
pub fn run_captured(s: &mut Session, id: &str, params: Json) -> Result<(Json, Op)> {
    let before = s.doc_mut().map(|d| {
        d.struct_log.clear();
        (d.uid, d.wb.clone())
    });
    let out = s.execute(id, params)?;
    let op = match (before, s.doc_mut()) {
        (Ok((uid, before)), Ok(d)) if d.uid == uid => {
            let edits = take_struct_log(d);
            capture(&before, &d.wb, edits)?
        }
        _ => Op::default(),
    };
    Ok((out, op))
}

/// Applies someone else's `op` to an open document and recalculates. Not undoable: the change
/// came from someone else. On an error the document is left as it was.
pub fn apply_op(d: &mut DocState, op: &Op) -> Result<()> {
    apply(d, &op.edits, &op.patch)
}

/// Applies a content patch alone (no structural edits).
pub fn apply_to_doc(d: &mut DocState, patch: &Patch) -> Result<()> {
    apply(d, &[], patch)
}

fn apply(d: &mut DocState, edits: &[StructEdit], patch: &Patch) -> Result<()> {
    if edits.is_empty() && patch.is_empty() {
        return Ok(());
    }
    let mut wb = (*d.wb).clone();
    for e in edits {
        replay(&mut wb, e)?;
    }
    let applied = apply_patch(&mut wb, patch)?;
    wb.ensure_sheet_ids();
    if applied.structural || !edits.is_empty() {
        d.calc.recalc_all(&mut wb);
    } else if !applied.cells.is_empty() {
        d.calc.cells_changed(&mut wb, &applied.cells);
    }
    // Someone else's edit neither makes this copy unsaved nor saves it: keep the flag as it was.
    let dirty = d.is_dirty();
    d.wb = Arc::new(wb);
    if !dirty {
        d.saved = d.wb.clone();
    }
    d.revision += 1;
    Ok(())
}
