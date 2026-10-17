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

pub use patch::{Applied, By, CellChange, Content, Field, Items, Line, LineChange, Patch, SheetPatch, apply_patch, make_patch};
pub use replica::{Incoming, Outgoing, Replica};
pub use structure::{Dir, StructEdit, replay};
pub use transform::rebase;

use std::sync::Arc;

use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{DocState, EngineError, Result, Session};

/// The largest edit shared with the others (its op as JSON). A bigger one (a paste of hundreds of
/// thousands of cells) is taken back with a message to make it in smaller steps: the server and
/// every other copy would have to swallow it whole.
pub const MAX_SHARED_OP_BYTES: usize = 8 << 20;

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

pub(super) fn apply(d: &mut DocState, edits: &[StructEdit], patch: &Patch) -> Result<()> {
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

/// The shared document of a [`Session`] and this person's copy of it.
pub(crate) struct Shared {
    pub(crate) uid: u64,
    replica: Replica,
    /// This person can view the workbook but not edit it.
    read_only: bool,
}

/// Co-editing through a [`Session`]: once started, every edit of the shared document (from any
/// command path: ribbon, keys, agents) goes into the outgoing queue, and the host feeds the
/// server's log back in.
impl Session {
    /// Shares the active document, which is the shared workbook as of log position `seq`;
    /// `client` is this person's id from the server.
    pub fn collab_start(&mut self, client: u64, seq: u64) -> Result<()> {
        let d = self.doc()?;
        let replica = Replica::new(client, d.wb.clone(), seq)?;
        self.collab = Some(Shared { uid: d.uid, replica, read_only: false });
        Ok(())
    }

    /// This person can view the shared workbook but not edit it: an edit is taken back with a
    /// message (selecting, scrolling and switching sheets still work).
    pub fn collab_set_read_only(&mut self, read_only: bool) {
        if let Some(c) = self.collab.as_mut() {
            c.read_only = read_only;
        }
    }

    pub fn collab_read_only(&self) -> bool {
        self.collab.as_ref().is_some_and(|c| c.read_only)
    }

    pub fn collab_stop(&mut self) {
        self.collab = None;
    }

    /// This person's client id while a document is shared.
    pub fn collab_client(&self) -> Option<u64> {
        self.collab.as_ref().map(|c| c.replica.client())
    }

    /// This person has edits the server hasn't confirmed.
    pub fn collab_pending(&self) -> bool {
        self.collab.as_ref().is_some_and(|c| c.replica.has_pending())
    }

    /// The next op to send to the server (one at a time: the next once this one comes back).
    pub fn collab_outgoing(&mut self) -> Option<Outgoing> {
        self.collab.as_mut().and_then(|c| c.replica.outgoing())
    }

    /// Applies the server's next log entry. An error means this copy can't follow the log (an
    /// entry missing or out of order): rejoin from a fresh copy.
    pub fn collab_receive(&mut self, msg: Incoming) -> Result<()> {
        let Some(mut shared) = self.collab.take() else {
            return Err(EngineError::Other("co-editing: no shared document".into()));
        };
        let r = match self.docs.iter_mut().find(|d| d.uid == shared.uid) {
            Some(d) => shared.replica.receive(msg, d),
            None => Err(EngineError::Other("co-editing: the shared document was closed".into())),
        };
        self.collab = Some(shared);
        r
    }

    /// The shared workbook as everyone has it (without this person's unconfirmed edits) and its
    /// log position: what a checkpoint saves.
    pub fn collab_confirmed(&self) -> Option<(u64, Arc<Workbook>)> {
        self.collab.as_ref().map(|c| (c.replica.seq(), c.replica.confirmed().clone()))
    }

    /// Undo (or redo) in a shared document: this person's own last edit, for everyone. Their
    /// edits since stay.
    pub(crate) fn collab_undo(&mut self, redo: bool) -> Result<serde_json::Value> {
        let Some(mut shared) = self.collab.take() else { return Ok(serde_json::Value::Null) };
        let r = match self.docs.iter_mut().find(|d| d.uid == shared.uid) {
            Some(d) => shared.replica.undo(d, redo),
            None => Ok(false),
        };
        self.collab = Some(shared);
        let what = if redo { "redo" } else { "undo" };
        match r? {
            true => Ok(serde_json::Value::Null),
            false => Err(EngineError::Other(format!("Nothing of yours to {what}."))),
        }
    }

    /// Whether this person has something to undo (or redo) in the shared document; `None` when
    /// the active document isn't shared.
    pub fn collab_can_undo(&self, redo: bool) -> Option<bool> {
        let active = self.active().map(|d| d.uid);
        self.collab.as_ref().filter(|c| Some(c.uid) == active).map(|c| c.replica.can_undo(redo))
    }

    /// After a command on the shared document: its op goes to the others. If it can't be put into
    /// an op, the edit is taken back rather than left on this screen only.
    pub(crate) fn share_edit(&mut self, uid: u64, before: &Arc<Workbook>) -> Result<()> {
        let Some(mut shared) = self.collab.take() else { return Ok(()) };
        let r = match self.docs.iter_mut().find(|d| d.uid == uid) {
            Some(d) => {
                let edits = take_struct_log(d);
                let read_only = shared.read_only;
                match capture(before, &d.wb, edits).and_then(|op| {
                    if read_only && !op.is_empty() {
                        return Err(EngineError::Other("You can view this workbook but not edit it.".into()));
                    }
                    let size = serde_json::to_vec(&op).map(|v| v.len()).unwrap_or(usize::MAX);
                    if size > MAX_SHARED_OP_BYTES {
                        return Err(EngineError::Other(format!(
                            "That change is too large to share with the others ({:.1} MB). Make it in smaller steps.",
                            size as f64 / 1_048_576.0
                        )));
                    }
                    Ok(op)
                }) {
                    Ok(op) => {
                        if !op.is_empty() {
                            shared.replica.local(d, before, op);
                        }
                        Ok(())
                    }
                    Err(e) => {
                        d.wb = before.clone();
                        d.calc.rebuild(&d.wb);
                        Err(e)
                    }
                }
            }
            None => Ok(()),
        };
        self.collab = Some(shared);
        r
    }
}
