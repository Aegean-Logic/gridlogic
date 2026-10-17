//! Rebasing an op over another made at the same time (`docs/co-editing.md` §3).
//!
//! [`rebase`]`(x, over, x_later, base)` re-expresses `x` as if `over` had been applied first:
//! its structural edits and cell positions shift with `over`'s inserts and deletes, its formulas
//! are adjusted the way the replay adjusts every formula, and settings positioned on a sheet
//! (merges, conditional formats, names…) shift through [`apply_shift`] on a probe copy, so they
//! move exactly as the real replay moves them. Content both ops set goes to the later one.
//!
//! Everything here is a pure function of its inputs: every copy of the workbook that rebases the
//! same ops over the same log gets the same result, which is what makes the copies converge.

use std::sync::Arc;

use gridcraft_core::{CellRef, RangeRef};
use gridcraft_formula::adjust::Edit;
use gridcraft_model::{Sheet, Workbook};

use super::patch::{field_of, set_sheet_field, set_workbook_field};
use super::{Content, Dir, Field, Op, Patch, StructEdit};
use crate::cmd::sheet::{apply_shift, map_range};
use crate::{EngineError, Result};

/// The most structural edits an op may grow to while rebasing (a delete splits in two around an
/// insert made inside it). Past this the op is refused rather than grown without bound.
const MAX_EDITS: usize = 256;

/// `x` rebased over `over`. Both are relative to `base`; the result is relative to `base` with
/// `over` applied. `x_later`: `x` comes after `over` in the shared order, so its content wins
/// where both set the same thing, and at an equal position `over`'s inserted lines come first.
pub fn rebase(x: &Op, over: &Op, x_later: bool, base: &Workbook) -> Result<Op> {
    // 1. Structural edits, each over the other's.
    let (edits, over_after_x) = xform_lists(x.edits.clone(), over.edits.clone(), x_later)?;

    // 2. x's content moves with over's structure (expressed after x's own edits).
    let mut patch = x.patch.clone();
    for e in &over_after_x {
        patch = patch_over(&patch, e, base)?;
    }
    rename_formulas(&mut patch, &over.patch, base);

    // 3. Content both set: the later op's stays.
    if !x_later {
        let mut theirs = over.patch.clone();
        for e in &edits {
            theirs = patch_over(&theirs, e, base)?;
        }
        drop_conflicts(&mut patch, &theirs);
    }
    Ok(Op { edits, patch })
}

// ----------------------------------------------------------------------- structural edits

/// Lists of structural edits over each other: (`a` after `b`, `b` after `a`).
fn xform_lists(a: Vec<StructEdit>, b: Vec<StructEdit>, a_later: bool) -> Result<(Vec<StructEdit>, Vec<StructEdit>)> {
    if a.len() > MAX_EDITS || b.len() > MAX_EDITS {
        return Err(EngineError::Other("co-editing: too many structural edits in one op".into()));
    }
    match (a.len(), b.len()) {
        (0, _) | (_, 0) => Ok((a, b)),
        (1, 1) => match (a.into_iter().next(), b.into_iter().next()) {
            (Some(x), Some(y)) => Ok(xform(&x, &y, a_later)),
            _ => Ok((Vec::new(), Vec::new())),
        },
        (_, 1) => {
            // Each of a's edits over b, b moving past each in turn.
            let mut out = Vec::new();
            let mut b = b;
            for x in a {
                let (x2, b2) = xform_lists(vec![x], b, a_later)?;
                out.extend(x2);
                b = b2;
            }
            Ok((out, b))
        }
        _ => {
            // a over each of b's edits in turn.
            let mut a = a;
            let mut out = Vec::new();
            for y in b {
                let (a2, y2) = xform_lists(a, vec![y], a_later)?;
                a = a2;
                out.extend(y2);
            }
            Ok((a, out))
        }
    }
}

/// One structural edit over another: (`a` after `b`, `b` after `a`).
fn xform(a: &StructEdit, b: &StructEdit, a_later: bool) -> (Vec<StructEdit>, Vec<StructEdit>) {
    if let (Some(la), Some(lb)) = (Lines::of(a), Lines::of(b))
        && la.sheet == lb.sheet
        && la.dir == lb.dir
    {
        return (lines_over(&la, &lb, a_later), lines_over(&lb, &la, !a_later));
    }
    (shift_over(a, b), shift_over(b, a))
}

/// A row or column insert or delete.
#[derive(Clone, Copy)]
struct Lines {
    sheet: u64,
    dir: Dir,
    at: u32,
    count: u32,
    insert: bool,
}

impl Lines {
    fn of(e: &StructEdit) -> Option<Lines> {
        match *e {
            StructEdit::Insert { sheet, dir, at, count } => Some(Lines { sheet, dir, at, count, insert: true }),
            StructEdit::Delete { sheet, dir, at, count } => Some(Lines { sheet, dir, at, count, insert: false }),
            _ => None,
        }
    }
    fn edit(self, at: u32, count: u32) -> StructEdit {
        let Lines { sheet, dir, insert, .. } = self;
        if insert { StructEdit::Insert { sheet, dir, at, count } } else { StructEdit::Delete { sheet, dir, at, count } }
    }
    fn end(self) -> u32 {
        self.at.saturating_add(self.count)
    }
}

/// `a` after `b`, both line edits on the same sheet and axis. At an equal insert position the
/// earlier op's lines come first.
fn lines_over(a: &Lines, b: &Lines, a_later: bool) -> Vec<StructEdit> {
    let (p, n) = (a.at, a.count);
    match (a.insert, b.insert) {
        (true, true) => {
            let before = if a_later { b.at <= p } else { b.at < p };
            vec![a.edit(if before { p.saturating_add(b.count) } else { p }, n)]
        }
        (true, false) => {
            // An insert where lines were deleted lands at the deletion point.
            let at = if p <= b.at {
                p
            } else if p >= b.end() {
                p - b.count
            } else {
                b.at
            };
            vec![a.edit(at, n)]
        }
        (false, true) => {
            if b.at <= p {
                vec![a.edit(p.saturating_add(b.count), n)]
            } else if b.at >= a.end() {
                vec![a.edit(p, n)]
            } else {
                // Lines inserted inside the deleted span survive: delete around them, the far
                // part first so the near part keeps its position.
                let near = b.at - p;
                vec![a.edit(b.at.saturating_add(b.count), n - near), a.edit(p, near)]
            }
        }
        (false, false) => {
            // What's left of a's span once b's is gone, shifted for b.
            let overlap = a.end().min(b.end()).saturating_sub(p.max(b.at));
            let left = n - overlap;
            if left == 0 {
                return Vec::new();
            }
            let at = if p < b.at {
                p
            } else if p >= b.end() {
                p - b.count
            } else {
                b.at
            };
            vec![a.edit(at, left)]
        }
    }
}

/// `a` after `b` for anything but two line edits on one axis: ranges and positions on `b`'s
/// sheet map through it; a range `b` deleted drops `a`. A move shifts nothing (its cells travel
/// in the patch).
fn shift_over(a: &StructEdit, b: &StructEdit) -> Vec<StructEdit> {
    let Some(edit) = shift_edit(b) else { return vec![a.clone()] };
    let on = b.sheet();
    let map = |r: RangeRef| map_range(r, &edit);
    let cell = |row: u32, col: u32| map_range(RangeRef::cell(CellRef::new(row, col)), &edit).map(|r| (r.start.row, r.start.col));
    let mapped = match a.clone() {
        // Cell shifts don't move whole lines; lines on the other axis don't either.
        e @ (StructEdit::Insert { .. } | StructEdit::Delete { .. }) => Some(e),
        StructEdit::InsertCells { sheet, dir, range } if sheet == on => map(range).map(|range| StructEdit::InsertCells { sheet, dir, range }),
        StructEdit::DeleteCells { sheet, dir, range } if sheet == on => map(range).map(|range| StructEdit::DeleteCells { sheet, dir, range }),
        StructEdit::Move { sheet, from, to_sheet, to_row, to_col } => {
            let from = if sheet == on { map(from) } else { Some(from) };
            let to = if to_sheet == on { cell(to_row, to_col) } else { Some((to_row, to_col)) };
            match (from, to) {
                (Some(from), Some((to_row, to_col))) => Some(StructEdit::Move { sheet, from, to_sheet, to_row, to_col }),
                _ => None,
            }
        }
        other => Some(other),
    };
    mapped.into_iter().collect()
}

/// The shift a structural edit makes, as the formula crate's edit (`None` for a move).
fn shift_edit(e: &StructEdit) -> Option<Edit> {
    Some(match *e {
        StructEdit::Insert { dir, at, count, .. } => Edit::Insert { axis: dir.into(), at, count },
        StructEdit::Delete { dir, at, count, .. } => Edit::Delete { axis: dir.into(), at, count },
        StructEdit::InsertCells { dir, range, .. } => Edit::InsertCells { axis: dir.into(), range },
        StructEdit::DeleteCells { dir, range, .. } => Edit::DeleteCells { axis: dir.into(), range },
        StructEdit::Move { .. } => return None,
    })
}

// -------------------------------------------------------------------------- content patches

/// `p` after the structural edit `e`: cells and lines on `e`'s sheet move (or go, if deleted),
/// formulas everywhere adjust, and settings shift as the replay would shift them.
fn patch_over(p: &Patch, e: &StructEdit, base: &Workbook) -> Result<Patch> {
    let Some(edit) = shift_edit(e) else { return Ok(p.clone()) };
    let target = e.sheet();
    let name_of = |id: u64| base.sheet_by_id(id).and_then(|i| base.sheet(i)).map(|s| s.name.clone());
    let Some(target_name) = name_of(target) else { return Ok(p.clone()) };
    let line_dir = match *e {
        StructEdit::Insert { dir, .. } | StructEdit::Delete { dir, .. } => Some(dir),
        _ => None,
    };

    let mut out = p.clone();
    for sp in out.sheets.iter_mut() {
        let host = name_of(sp.id).unwrap_or_default();
        if sp.id == target {
            sp.cells.retain_mut(|c| match map_range(RangeRef::cell(c.at), &edit) {
                Some(r) => {
                    c.at = r.start;
                    true
                }
                None => false,
            });
            if let Some(dir) = line_dir {
                let lines = match dir {
                    Dir::Rows => &mut sp.rows,
                    Dir::Cols => &mut sp.cols,
                };
                lines.retain_mut(|l| match map_line(l.at, &edit) {
                    Some(at) => {
                        l.at = at;
                        true
                    }
                    None => false,
                });
            }
        }
        for c in sp.cells.iter_mut() {
            if let Some(Content::Formula { text, .. }) = c.content.as_mut()
                && let Ok(expr) = gridcraft_formula::parse(text)
            {
                *text = gridcraft_formula::print(&gridcraft_formula::adjust::adjust(expr, &host, &target_name, &edit));
            }
        }
    }
    shift_settings(&mut out, base, target, &edit)?;
    Ok(out)
}

/// A line index after a whole-line insert or delete (`None`: deleted).
fn map_line(v: u32, e: &Edit) -> Option<u32> {
    match *e {
        Edit::Insert { at, count, .. } => Some(if v >= at { v.saturating_add(count) } else { v }),
        Edit::Delete { at, count, .. } => {
            if v < at {
                Some(v)
            } else if v >= at.saturating_add(count) {
                Some(v - count)
            } else {
                None
            }
        }
        _ => Some(v),
    }
}

/// Shifts the settings `p` replaces (per sheet and workbook-wide) for `edit` on sheet `target`,
/// by loading them into a probe copy of `base`'s sheet list (no cells) and running the replay's
/// own shift on it.
fn shift_settings(p: &mut Patch, base: &Workbook, target: u64, edit: &Edit) -> Result<()> {
    let has_sheet_fields = p.sheets.iter().any(|sp| !sp.fields.is_empty());
    if !has_sheet_fields && p.fields.is_empty() {
        return Ok(());
    }
    let mut probe = Workbook::new();
    probe.sheets = base
        .sheets
        .iter()
        .map(|s| {
            let mut sh = Sheet::new(s.name.clone());
            sh.id = s.id;
            Arc::new(sh)
        })
        .collect();
    let Some(ti) = probe.sheet_by_id(target) else { return Ok(()) };
    for sp in p.sheets.iter().filter(|sp| !sp.fields.is_empty()) {
        let Some(si) = probe.sheet_by_id(sp.id) else { continue };
        if let Some(sh) = probe.sheet_mut(si) {
            for f in &sp.fields {
                set_sheet_field(sh, f)?;
            }
        }
    }
    for f in &p.fields {
        set_workbook_field(&mut probe, f)?;
    }
    apply_shift(&mut probe, ti, edit)?;
    for sp in p.sheets.iter_mut().filter(|sp| !sp.fields.is_empty()) {
        let Some(sh) = probe.sheet_by_id(sp.id).and_then(|i| probe.sheet(i)) else { continue };
        let names: Vec<String> = sp.fields.iter().map(|f| f.name.clone()).collect();
        sp.fields = names.iter().map(|n| field_of(sh, n)).collect::<Result<_>>()?;
    }
    let names: Vec<String> = p.fields.iter().map(|f| f.name.clone()).collect();
    p.fields = names.iter().map(|n| field_of(&probe, n)).collect::<Result<_>>()?;
    Ok(())
}

/// Sheets `over` renamed: `p`'s formulas that name them follow.
fn rename_formulas(p: &mut Patch, over: &Patch, base: &Workbook) {
    let renames: Vec<(String, String)> = over
        .sheets
        .iter()
        .filter_map(|sp| {
            let new = sp.fields.iter().find(|f| f.name == "name")?.value.as_ref()?.as_str()?.to_string();
            let old = base.sheet_by_id(sp.id).and_then(|i| base.sheet(i))?.name.clone();
            (old != new).then_some((old, new))
        })
        .collect();
    if renames.is_empty() {
        return;
    }
    for c in p.sheets.iter_mut().flat_map(|sp| sp.cells.iter_mut()) {
        if let Some(Content::Formula { text, .. }) = c.content.as_mut()
            && let Ok(mut expr) = gridcraft_formula::parse(text)
        {
            for (old, new) in &renames {
                expr = gridcraft_formula::adjust::rename_sheet(expr, old, new);
            }
            *text = gridcraft_formula::print(&expr);
        }
    }
}

/// Drops from `mine` what `theirs` (a later op, in the same coordinates) also sets.
fn drop_conflicts(mine: &mut Patch, theirs: &Patch) {
    if theirs.order.is_some() {
        mine.order = None;
    }
    for sp in mine.sheets.iter_mut() {
        let Some(other) = theirs.sheets.iter().find(|o| o.id == sp.id) else { continue };
        sp.fields.retain(|f| !other.fields.iter().any(|g: &Field| g.name == f.name));
        sp.rows.retain(|l| !other.rows.iter().any(|m| m.at == l.at));
        sp.cols.retain(|l| !other.cols.iter().any(|m| m.at == l.at));
        for c in sp.cells.iter_mut() {
            if let Some(o) = other.cells.iter().find(|o| o.at == c.at) {
                if o.content.is_some() {
                    c.content = None;
                }
                if o.style.is_some() {
                    c.style = None;
                }
            }
        }
        sp.cells.retain(|c| c.content.is_some() || c.style.is_some());
    }
    mine.fields.retain(|f| !theirs.fields.iter().any(|g| g.name == f.name));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(at: u32, count: u32) -> StructEdit {
        StructEdit::Insert { sheet: 1, dir: Dir::Rows, at, count }
    }
    fn del(at: u32, count: u32) -> StructEdit {
        StructEdit::Delete { sheet: 1, dir: Dir::Rows, at, count }
    }

    /// Where row `v` ends up after a list of row edits (`None`: deleted).
    fn after(v: u32, edits: &[StructEdit]) -> Option<u32> {
        edits.iter().try_fold(v, |v, e| map_line(v, &shift_edit(e)?))
    }

    /// For every pair of line edits: applying a then b-after-a moves every surviving row where b
    /// then a-after-b does (the transform's defining property, TP1).
    #[test]
    fn line_edits_commute_through_their_transforms() {
        let mut edits = Vec::new();
        for at in 0..6 {
            for count in 1..4 {
                edits.push(ins(at, count));
                edits.push(del(at, count));
            }
        }
        for a in &edits {
            for b in &edits {
                for a_later in [false, true] {
                    let (a2, b2) = xform(a, b, a_later);
                    let one: Vec<StructEdit> = std::iter::once(b.clone()).chain(a2.clone()).collect();
                    let two: Vec<StructEdit> = std::iter::once(a.clone()).chain(b2.clone()).collect();
                    for v in 0..20 {
                        assert_eq!(after(v, &one), after(v, &two), "row {v}: a={a:?} b={b:?} a_later={a_later} a'={a2:?} b'={b2:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn an_insert_inside_a_delete_survives_it() {
        // Rows 3..7 deleted by one person while another inserts 2 rows at 5.
        let (d2, i2) = xform(&del(3, 4), &ins(5, 2), false);
        assert_eq!(d2, vec![del(7, 2), del(3, 2)], "the delete splits around the new rows");
        assert_eq!(i2, vec![ins(3, 2)], "the new rows land where the deleted ones were");
    }
}
