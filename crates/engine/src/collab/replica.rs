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

use std::collections::VecDeque;
use std::sync::Arc;

use gridcraft_model::Workbook;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::transform::rebase;
use super::{Op, apply_patch, replay, run_captured};
use crate::{DocState, EngineError, Result, Session};

/// Log entries kept for rebasing late ops. An op made against an older position can't be
/// placed; its author has to rejoin from a fresh copy.
const LOG_KEEP: usize = 4096;

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
}

impl Replica {
    /// Joins at log position `seq` with the workbook as of that position. `client` comes from the
    /// server: nonzero, unique in the session, and below 2³² (it namespaces new sheet ids).
    pub fn new(client: u64, wb: Arc<Workbook>, seq: u64) -> Result<Replica> {
        if client == 0 || client > u64::from(u32::MAX) {
            return Err(EngineError::Other("co-editing: the client id must be between 1 and 2^32-1".into()));
        }
        Ok(Replica { client, confirmed: wb, seq, log: VecDeque::new(), inflight: None, buffer: VecDeque::new(), next_client_seq: 1, next_sheet: 1 })
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

    /// Runs a command for this person: on their copy at once, and queued for the others.
    pub fn run(&mut self, s: &mut Session, id: &str, params: Json) -> Result<Json> {
        if matches!(id, "edit.undo" | "edit.redo") {
            // Snapshot undo would take back other people's edits too (docs/co-editing.md §4).
            return Err(EngineError::Other("Undo isn't available in a shared workbook yet.".into()));
        }
        let (out, mut op) = run_captured(s, id, params)?;
        if !op.is_empty() {
            let d = s.doc_mut()?;
            self.retag_new_sheets(d, &mut op);
            let client_seq = self.next_client_seq;
            self.next_client_seq += 1;
            self.buffer.push_back(Pending { client_seq, base: 0, original: op.clone(), current: op });
        }
        Ok(out)
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

/// Applies an op to a bare workbook (no recalculation): its structural edits, then its patch.
fn apply_wb(wb: &mut Workbook, op: &Op) -> Result<()> {
    for e in &op.edits {
        replay(wb, e)?;
    }
    apply_patch(wb, &op.patch)?;
    wb.ensure_sheet_ids();
    Ok(())
}
