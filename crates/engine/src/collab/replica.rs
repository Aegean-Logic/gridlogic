//! One person's copy of a shared workbook.
//!
//! A server (the host's business; nothing here does I/O) orders everyone's ops into one log.
//! Each copy keeps the workbook as of its last log entry (`confirmed`), at most one op of its own
//! the server hasn't echoed yet (`inflight`), and the edits made since (`buffer`). What the person
//! sees is `confirmed` with their own pending edits on top, rebuilt whenever an op arrives.
//!
//! Convergence: log op `n` was made against log position `base`, so it's rebased over the log
//! entries `base+1 .. n-1` (its author hadn't seen them) before it's applied. That's a pure
//! function of the log, so every copy, the author's included, applies the same thing and ends
//! with the same workbook. An op the rebase or the apply refuses becomes a no-op everywhere.
//!
//! Undo is each person's own: an edit's inverse (its inserts as deletes and so on, and a patch
//! putting back what it changed) goes on their undo stack, is rebased over everything that
//! arrives afterwards (where someone else changed the same thing later, their change stays), and
//! undoing sends it to everyone like any other edit.

use std::collections::VecDeque;
use std::sync::Arc;

use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::transform::rebase;
use super::{Op, StructEdit, apply_patch, capture, replay, run_captured};
use crate::{DocState, EngineError, Result, Session};

/// Log entries kept for rebasing late ops. An op made against an older position can't be
/// placed; its author has to rejoin from a fresh copy.
const LOG_KEEP: usize = 4096;
/// Undo steps kept, as without co-editing.
const UNDO_KEEP: usize = 100;

/// What a local op is, for the undo and redo stacks.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Edit,
    Undo,
    Redo,
}

/// An op on its way to the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outgoing {
    pub client: u64,
    /// This client's count of its ops, so the echo can be matched.
    pub client_seq: u64,
    /// The last log position the op's author had applied.
    pub base: u64,
    pub op: Op,
}

/// A log entry from the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Incoming {
    pub seq: u64,
    pub client: u64,
    pub client_seq: u64,
    pub base: u64,
    pub op: Op,
}

#[derive(Clone, Debug)]
struct Pending {
    client_seq: u64,
    /// The log position it was sent against (set when it goes out).
    base: u64,
    /// As captured: what the server gets.
    original: Op,
    /// Rebased over what arrived since, for showing it on top of `confirmed`.
    current: Op,
}

pub struct Replica {
    client: u64,
    confirmed: Arc<Workbook>,
    seq: u64,
    /// Ops as applied, by log position, oldest first.
    log: VecDeque<(u64, Op)>,
    inflight: Option<Pending>,
    buffer: VecDeque<Pending>,
    next_client_seq: u64,
    next_sheet: u64,
    /// This person's undo and redo steps (inverse ops), rebased to apply to what they see now.
    undo: Vec<Op>,
    redo: Vec<Op>,
}

impl Replica {
    /// Joins at log position `seq` with the workbook as of that position. `client` comes from the
    /// server: nonzero, unique in the session, and below 2³² (it namespaces new sheet ids).
    pub fn new(client: u64, wb: Arc<Workbook>, seq: u64) -> Result<Replica> {
        if client == 0 || client > u64::from(u32::MAX) {
            return Err(EngineError::Other("co-editing: the client id must be between 1 and 2^32-1".into()));
        }
        Ok(Replica {
            client,
            confirmed: wb,
            seq,
            log: VecDeque::new(),
            inflight: None,
            buffer: VecDeque::new(),
            next_client_seq: 1,
            next_sheet: 1,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    pub fn client(&self) -> u64 {
        self.client
    }

    /// The last log position applied.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The workbook as of [`Replica::seq`], without this person's pending edits.
    pub fn confirmed(&self) -> &Arc<Workbook> {
        &self.confirmed
    }

    /// Edits of this person's the server hasn't confirmed yet.
    pub fn has_pending(&self) -> bool {
        self.inflight.is_some() || !self.buffer.is_empty()
    }

    /// Runs a command for this person: on their copy at once, and queued for the others. Undo
    /// and redo take back this person's own edits only.
    pub fn run(&mut self, s: &mut Session, id: &str, params: Json) -> Result<Json> {
        if matches!(id, "edit.undo" | "edit.redo") {
            let redo = id == "edit.redo";
            return match self.undo(s.doc_mut()?, redo)? {
                true => Ok(Json::Null),
                false => Err(EngineError::Other(format!("Nothing of yours to {}.", if redo { "redo" } else { "undo" }))),
            };
        }
        let before = s.doc()?.wb.clone();
        let (out, op) = run_captured(s, id, params)?;
        if !op.is_empty() {
            self.local(s.doc_mut()?, &before, op);
        }
        Ok(out)
    }

    /// Queues `op`, an edit this person just made in `d` (which was `before`), for the others,
    /// and puts its inverse on the undo stack.
    pub fn local(&mut self, d: &mut DocState, before: &Workbook, mut op: Op) {
        self.retag_new_sheets(d, &mut op);
        self.queue(d, before, op, Kind::Edit);
    }

    /// Undoes (or with `redo`, redoes) this person's last edit: on their copy at once, and for
    /// the others. Their edits made since stay. False when there's nothing to undo.
    pub fn undo(&mut self, d: &mut DocState, redo: bool) -> Result<bool> {
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some(op) = stack.pop() else { return Ok(false) };
        let before = d.wb.clone();
        let mut wb = (*before).clone();
        apply_wb(&mut wb, &op)?;
        d.calc.recalc_all(&mut wb);
        d.wb = Arc::new(wb);
        d.revision += 1;
        self.queue(d, &before, op, if redo { Kind::Redo } else { Kind::Undo });
        Ok(true)
    }

    /// Whether there's something of this person's to undo (or redo).
    pub fn can_undo(&self, redo: bool) -> bool {
        if redo { !self.redo.is_empty() } else { !self.undo.is_empty() }
    }

    fn queue(&mut self, d: &DocState, before: &Workbook, op: Op, kind: Kind) {
        // The steps already stacked now apply after this op (this person's own: theirs stands).
        for step in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            if let Ok(s) = rebase(step, &op, true, before) {
                *step = s;
            }
        }
        // Its inverse: undoing an undo is redoing it, and the other way round.
        if let Ok(inverse) = capture(&d.wb, before, inverse_edits(&op.edits)) {
            let stack = if kind == Kind::Undo { &mut self.redo } else { &mut self.undo };
            stack.push(inverse);
            if stack.len() > UNDO_KEEP {
                stack.remove(0);
            }
        }
        if kind == Kind::Edit {
            self.redo.clear();
        }
        let client_seq = self.next_client_seq;
        self.next_client_seq += 1;
        self.buffer.push_back(Pending { client_seq, base: 0, original: op.clone(), current: op });
    }

    /// The next op to send, if none is waiting for its echo.
    pub fn outgoing(&mut self) -> Option<Outgoing> {
        if self.inflight.is_some() {
            return None;
        }
        let mut p = self.buffer.pop_front()?;
        p.base = self.seq;
        // What the server gets is the op as it stands now, against `base`.
        p.original = p.current.clone();
        let out = Outgoing { client: self.client, client_seq: p.client_seq, base: p.base, op: p.original.clone() };
        self.inflight = Some(p);
        Some(out)
    }

    /// Applies the next log entry and shows the result in `d` (with this person's pending edits).
    pub fn receive(&mut self, msg: Incoming, d: &mut DocState) -> Result<()> {
        if msg.seq != self.seq + 1 {
            return Err(EngineError::Other(format!("co-editing: expected log entry {}, got {}", self.seq + 1, msg.seq)));
        }
        let own = msg.client == self.client;
        if own && self.inflight.as_ref().is_none_or(|p| p.client_seq != msg.client_seq) {
            return Err(EngineError::Other("co-editing: an echo of an op this copy didn't send".into()));
        }
        // Without the entries its author hadn't seen, this copy can't place the op the way the
        // others do: rejoin from a fresh copy rather than diverge.
        let oldest = self.log.front().map_or(self.seq + 1, |(s, _)| *s);
        if msg.base < self.seq && msg.base + 1 < oldest {
            return Err(EngineError::Other("co-editing: the op is older than this copy's log; rejoin the session".into()));
        }
        let before = self.confirmed.clone();
        let (op, wb) = match self.place(&msg) {
            Ok(applied) => applied,
            // Refused the same way everywhere: a no-op in the log.
            Err(_) => (Op::default(), before.clone()),
        };
        self.confirmed = wb;
        self.seq = msg.seq;
        self.log.push_back((msg.seq, op.clone()));
        while self.log.len() > LOG_KEEP {
            self.log.pop_front();
        }

        if own {
            self.inflight = None;
        } else {
            // This person's pending edits now go after the new entry.
            let mut theirs = op;
            for p in self.inflight.iter_mut().chain(self.buffer.iter_mut()) {
                let mine = p.current.clone();
                if let Ok(rebased) = rebase(&mine, &theirs, true, &before) {
                    p.current = rebased;
                }
                if let Ok(past) = rebase(&theirs, &mine, false, &before) {
                    theirs = past;
                }
            }
            // Undo steps now go after it, and where it changed the same thing, it stays.
            for step in self.undo.iter_mut().chain(self.redo.iter_mut()) {
                if let Ok(s) = rebase(step, &theirs, false, &before) {
                    *step = s;
                }
            }
        }
        self.show(d);
        Ok(())
    }

    /// Log entry `msg` rebased over the entries its author hadn't seen, and the confirmed
    /// workbook with it applied.
    fn place(&self, msg: &Incoming) -> Result<(Op, Arc<Workbook>)> {
        if msg.base >= msg.seq {
            return Err(EngineError::Other("co-editing: an op made against a later position".into()));
        }
        let mut op = msg.op.clone();
        for (_, m) in self.log.iter().filter(|(s, _)| *s > msg.base && *s < msg.seq) {
            op = rebase(&op, m, true, &self.confirmed)?;
        }
        let mut wb = (*self.confirmed).clone();
        apply_wb(&mut wb, &op)?;
        Ok((op, Arc::new(wb)))
    }

    /// Shows `confirmed` with this person's pending edits in `d`.
    fn show(&self, d: &mut DocState) {
        let mut wb = (*self.confirmed).clone();
        // Keep the person's own view: the sheet they're on.
        let active = d.wb.active().map(|s| s.id);
        for p in self.inflight.iter().chain(self.buffer.iter()) {
            let mut next = wb.clone();
            if apply_wb(&mut next, &p.current).is_ok() {
                wb = next;
            }
        }
        if let Some(i) = active.and_then(|id| wb.sheet_by_id(id)) {
            wb.active_sheet = i;
        }
        d.calc.recalc_all(&mut wb);
        let dirty = d.is_dirty();
        d.wb = Arc::new(wb);
        if !dirty {
            d.saved = d.wb.clone();
        }
        d.revision += 1;
    }

    /// Sheets this person's edit created get ids no one else's can have: the client id in the
    /// high half, a count in the low half.
    fn retag_new_sheets(&mut self, d: &mut DocState, op: &mut Op) {
        for sp in op.patch.sheets.iter_mut().filter(|sp| sp.new) {
            let old = sp.id;
            let new = (self.client << 32) | (self.next_sheet & u64::from(u32::MAX));
            self.next_sheet += 1;
            sp.id = new;
            for id in op.patch.order.iter_mut().flatten() {
                if *id == old {
                    *id = new;
                }
            }
            let wb = Arc::make_mut(&mut d.wb);
            if let Some(i) = wb.sheet_by_id(old)
                && let Some(sh) = wb.sheets.get_mut(i)
            {
                Arc::make_mut(sh).id = new;
            }
        }
    }
}

/// The structural edits that take back `edits`, last first. A move has none (its patch carries
/// the cells back).
fn inverse_edits(edits: &[StructEdit]) -> Vec<StructEdit> {
    edits
        .iter()
        .rev()
        .filter_map(|e| match *e {
            StructEdit::Insert { sheet, dir, at, count } => Some(StructEdit::Delete { sheet, dir, at, count }),
            StructEdit::Delete { sheet, dir, at, count } => Some(StructEdit::Insert { sheet, dir, at, count }),
            StructEdit::InsertCells { sheet, dir, range } => Some(StructEdit::DeleteCells { sheet, dir, range }),
            StructEdit::DeleteCells { sheet, dir, range } => Some(StructEdit::InsertCells { sheet, dir, range }),
            StructEdit::Move { .. } => None,
        })
        .collect()
}

/// Applies an op to a bare workbook (no recalculation): its structural edits, then its patch.
fn apply_wb(wb: &mut Workbook, op: &Op) -> Result<()> {
    for e in &op.edits {
        replay(wb, e)?;
    }
    apply_patch(wb, &op.patch)?;
    wb.ensure_sheet_ids();
    Ok(())
}
