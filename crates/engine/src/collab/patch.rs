//! Content patches: what an edit changed, as data another copy of the workbook can apply.
//!
//! A patch is the difference between the workbook before and after an edit, whatever command
//! made it: cells (content and style, separately), rows and columns, every other sheet setting
//! (replaced whole), the sheet list (by [`Sheet::id`]) and the workbook settings. Styles travel
//! as [`Style`] values, never as [`StyleId`]s, which are positions in one copy's style table.
//! Cached formula results don't travel: the receiver recalculates. What each person sees on
//! their own screen (selection, active sheet, scroll position, zoom, view mode) isn't part of it.

use std::sync::Arc;

use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, Formula, LineInfo, Sheet, Style, StyleId, StyleTable, Workbook};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{EngineError, Result};

/// What an edit changed. Empty when it changed nothing another person would see.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    /// The sheet ids in order after the edit, when it added, removed or reordered sheets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<u64>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<SheetPatch>,
    /// Workbook settings (defined names, properties, calculation options…), replaced whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.order.is_none() && self.sheets.is_empty() && self.fields.is_empty()
    }
}

/// The changes to one sheet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SheetPatch {
    pub id: u64,
    /// The edit created this sheet: start from a blank one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    /// Sheet settings (name, merges, conditional formats, charts…), replaced whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<CellChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<LineChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cols: Vec<LineChange>,
}

impl SheetPatch {
    fn is_empty(&self) -> bool {
        !self.new && self.fields.is_empty() && self.cells.is_empty() && self.rows.is_empty() && self.cols.is_empty()
    }
}

/// A setting by its name in the file format, with its new value. `None`: back to the default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
}

/// One cell's change. A part that didn't change is `None`, so a style change and a value typed
/// into the same cell by someone else both survive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellChange {
    pub at: CellRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<Style>,
}

/// What a cell holds, without its cached formula result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Content {
    #[default]
    Empty,
    Value {
        value: Value,
    },
    Formula {
        text: String,
        /// A legacy (Ctrl+Shift+Enter) array formula's range.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        array: Option<RangeRef>,
    },
}

impl Content {
    fn of(cell: Option<&Cell>) -> Content {
        match cell {
            Some(Cell { formula: Some(f), .. }) => Content::Formula { text: f.text.clone(), array: f.array },
            Some(c) if !c.value.is_empty() => Content::Value { value: c.value.clone() },
            _ => Content::Empty,
        }
    }
}

/// One row's or column's change. `info: None`: back to the default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LineChange {
    pub at: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<Line>,
}

/// [`LineInfo`] with its style as a value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub outline: u8,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<Style>,
    #[serde(default)]
    pub custom: bool,
}

impl Line {
    fn of(info: &LineInfo, styles: &StyleTable) -> Line {
        Line {
            size: info.size,
            hidden: info.hidden,
            outline: info.outline,
            collapsed: info.collapsed,
            style: info.style.map(|s| styles.get(s).clone()),
            custom: info.custom,
        }
    }
}

/// What applying a patch changed, for recalculation.
#[derive(Debug, Default)]
pub struct Applied {
    /// Cells whose content changed, by sheet index after the patch.
    pub cells: Vec<(usize, CellRef)>,
    /// Sheets, names or other settings formulas depend on changed: recalculate everything.
    pub structural: bool,
}

// ------------------------------------------------------------------------- settings by name

/// Sheet settings that travel, by field. Everything a [`Sheet`] serialises is either here or in
/// [`SHEET_LOCAL`] (a test checks), so a field added to `Sheet` can't silently stop replicating.
macro_rules! sheet_fields {
    ($($f:ident),* $(,)?) => {
        #[cfg(test)]
        const SHEET_FIELDS: &[&str] = &[$(stringify!($f)),*];
        fn diff_sheet_fields(a: &Sheet, b: &Sheet) -> Result<Vec<Field>> {
            let mut out = Vec::new();
            $(
                if a.$f != b.$f {
                    let mut probe = Sheet::new("");
                    probe.$f = b.$f.clone();
                    out.push(field_of(&probe, stringify!($f))?);
                }
            )*
            Ok(out)
        }
        fn set_sheet_field(sh: &mut Sheet, field: &Field) -> Result<()> {
            let probe: Sheet = probe_with(&Sheet::new(""), field)?;
            match field.name.as_str() {
                $(stringify!($f) => sh.$f = probe.$f,)*
                other => return Err(EngineError::Other(format!("co-editing: unknown sheet setting `{other}`"))),
            }
            Ok(())
        }
    };
}

sheet_fields!(
    name,
    cell_pictures,
    default_col_width,
    default_row_height,
    merges,
    freeze,
    tab_color,
    visibility,
    show_gridlines,
    show_headings,
    show_formulas,
    show_zeros,
    right_to_left,
    cond_formats,
    validations,
    tables,
    autofilter,
    comments,
    hyperlinks,
    charts,
    images,
    shapes,
    sparklines,
    slicers,
    pivots,
    protection,
    edit_ranges,
    print,
);

/// Sheet fields that don't travel as settings: the id and the cells, rows and columns (sent
/// their own way), and each person's own view.
#[cfg(test)]
const SHEET_LOCAL: &[&str] = &["id", "cells", "rows", "cols", "zoom", "view_active", "view_top_left", "view"];

macro_rules! workbook_fields {
    ($($f:ident),* $(,)?) => {
        #[cfg(test)]
        const WORKBOOK_FIELDS: &[&str] = &[$(stringify!($f)),*];
        fn diff_workbook_fields(a: &Workbook, b: &Workbook) -> Result<Vec<Field>> {
            let mut out = Vec::new();
            $(
                if a.$f != b.$f {
                    let mut probe = Workbook::new();
                    probe.$f = b.$f.clone();
                    out.push(field_of(&probe, stringify!($f))?);
                }
            )*
            Ok(out)
        }
        fn set_workbook_field(wb: &mut Workbook, field: &Field) -> Result<()> {
            let probe: Workbook = probe_with(&Workbook::new(), field)?;
            match field.name.as_str() {
                $(stringify!($f) => wb.$f = probe.$f,)*
                other => return Err(EngineError::Other(format!("co-editing: unknown workbook setting `{other}`"))),
            }
            Ok(())
        }
    };
}

workbook_fields!(
    names,
    theme,
    date_system,
    calc,
    props,
    protected_structure,
    structure_password,
    custom_lists,
    cell_styles,
    vba_project,
    csv_delimiter,
);

/// Workbook fields that don't travel as settings: the sheets and the style table (sent their own
/// way) and the active sheet (each person's own).
#[cfg(test)]
const WORKBOOK_LOCAL: &[&str] = &["sheets", "styles", "active_sheet"];

/// One setting of `probe` (a blank sheet or workbook with that setting copied in), as it
/// serialises, so settings with their own serde format (`cellmap`, base64) keep it.
fn field_of<T: Serialize>(probe: &T, name: &str) -> Result<Field> {
    let json = serde_json::to_value(probe).map_err(|e| EngineError::Other(format!("co-editing: {name}: {e}")))?;
    let value = json.get(name).filter(|v| !v.is_null()).cloned();
    Ok(Field { name: name.to_string(), value })
}

/// `blank` with one setting replaced by `field` (or removed, for its default), deserialised.
fn probe_with<T: Serialize + for<'de> Deserialize<'de>>(blank: &T, field: &Field) -> Result<T> {
    let bad = |e: serde_json::Error| EngineError::Other(format!("co-editing: setting `{}`: {e}", field.name));
    let mut json = serde_json::to_value(blank).map_err(bad)?;
    let Some(obj) = json.as_object_mut() else {
        return Err(EngineError::Other("co-editing: a blank sheet doesn't serialise as an object".into()));
    };
    match &field.value {
        Some(v) if !v.is_null() => {
            obj.insert(field.name.clone(), v.clone());
        }
        _ => {
            obj.remove(&field.name);
        }
    }
    serde_json::from_value(json).map_err(bad)
}

// ------------------------------------------------------------------------------ diffing

/// The patch that turns `before` into `after` (two states of the same workbook).
pub fn make_patch(before: &Workbook, after: &Workbook) -> Result<Patch> {
    let ids = |wb: &Workbook| wb.sheets.iter().map(|s| s.id).collect::<Vec<_>>();
    let order = Some(ids(after)).filter(|a| *a != ids(before));
    let blank = Sheet::new("");
    let mut sheets = Vec::new();
    for sa in &after.sheets {
        let old = before.sheet_by_id(sa.id).and_then(|i| before.sheets.get(i));
        if old.is_some_and(|sb| Arc::ptr_eq(sb, sa)) {
            continue;
        }
        let sb = old.map_or(&blank, |s| s.as_ref());
        let p = diff_sheet(&before.styles, sb, &after.styles, sa, old.is_none())?;
        if !p.is_empty() {
            sheets.push(p);
        }
    }
    Ok(Patch { order, sheets, fields: diff_workbook_fields(before, after)? })
}

fn diff_sheet(sb_styles: &StyleTable, sb: &Sheet, sa_styles: &StyleTable, sa: &Sheet, new: bool) -> Result<SheetPatch> {
    let style_of = |styles: &StyleTable, cell: Option<&Cell>| styles.get(cell.map_or(StyleId::DEFAULT, |c| c.style)).clone();
    let mut cells = Vec::new();
    for at in sb.cells.diff(&sa.cells) {
        let (cb, ca) = (sb.cells.get(at), sa.cells.get(at));
        let content = Some(Content::of(ca)).filter(|c| *c != Content::of(cb));
        let style = Some(style_of(sa_styles, ca)).filter(|s| *s != style_of(sb_styles, cb));
        // A cell whose cached result changed (a dependent of the edit) has nothing to send.
        if content.is_some() || style.is_some() {
            cells.push(CellChange { at, content, style });
        }
    }
    cells.sort_by_key(|c| c.at);
    Ok(SheetPatch {
        id: sa.id,
        new,
        fields: diff_sheet_fields(sb, sa)?,
        cells,
        rows: diff_lines(sb_styles, &sb.rows, sa_styles, &sa.rows),
        cols: diff_lines(sb_styles, &sb.cols, sa_styles, &sa.cols),
    })
}

fn diff_lines(
    b_styles: &StyleTable,
    b: &std::collections::BTreeMap<u32, LineInfo>,
    a_styles: &StyleTable,
    a: &std::collections::BTreeMap<u32, LineInfo>,
) -> Vec<LineChange> {
    let mut keys: Vec<u32> = b.keys().chain(a.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .filter_map(|at| {
            let (lb, la) = (b.get(&at).map(|i| Line::of(i, b_styles)), a.get(&at).map(|i| Line::of(i, a_styles)));
            (lb != la).then_some(LineChange { at, info: la })
        })
        .collect()
}

// ----------------------------------------------------------------------------- applying

/// Applies `patch` to `wb`. Changes to sheets that are gone (deleted meanwhile) are skipped. The
/// caller recalculates what [`Applied`] lists.
pub fn apply_patch(wb: &mut Workbook, patch: &Patch) -> Result<Applied> {
    let mut applied = Applied::default();
    let active_id = wb.active().map(|s| s.id);

    // Sheets the patch creates join at the end until `order` places them.
    for sp in patch.sheets.iter().filter(|sp| sp.new) {
        if sp.id != 0 && wb.sheet_by_id(sp.id).is_none() {
            let mut sh = Sheet::new("");
            sh.id = sp.id;
            wb.sheets.push(Arc::new(sh));
            applied.structural = true;
        }
    }
    if let Some(order) = &patch.order {
        let mut sheets = Vec::with_capacity(order.len());
        for id in order {
            if let Some(i) = wb.sheet_by_id(*id)
                && let Some(sh) = wb.sheets.get(i)
                && !sheets.iter().any(|s: &Arc<Sheet>| s.id == *id)
            {
                sheets.push(sh.clone());
            }
        }
        // Never leave a workbook without sheets: an order naming none of them is ignored.
        if !sheets.is_empty() {
            wb.sheets = sheets;
        }
        applied.structural = true;
    }
    // The active sheet is each person's own: keep showing the same one if it's still there.
    wb.active_sheet = active_id.and_then(|id| wb.sheet_by_id(id)).unwrap_or(0).min(wb.sheets.len().saturating_sub(1));

    for sp in &patch.sheets {
        let Some(si) = wb.sheet_by_id(sp.id) else { continue };
        let cell_styles: Vec<Option<StyleId>> = sp.cells.iter().map(|c| c.style.clone().map(|s| wb.styles.intern(s))).collect();
        let line_style = |wb: &mut Workbook, l: &LineChange| -> Option<LineInfo> {
            l.info.as_ref().map(|i| LineInfo {
                size: i.size,
                hidden: i.hidden,
                outline: i.outline,
                collapsed: i.collapsed,
                style: i.style.clone().map(|s| wb.styles.intern(s)),
                custom: i.custom,
            })
        };
        let rows: Vec<(u32, Option<LineInfo>)> = sp.rows.iter().map(|l| (l.at, line_style(wb, l))).collect();
        let cols: Vec<(u32, Option<LineInfo>)> = sp.cols.iter().map(|l| (l.at, line_style(wb, l))).collect();
        let Some(sh) = wb.sheet_mut(si) else { continue };
        for f in &sp.fields {
            set_sheet_field(sh, f)?;
            if f.name == "name" || f.name == "tables" {
                applied.structural = true;
            }
        }
        for (map, lines) in [(&mut sh.rows, rows), (&mut sh.cols, cols)] {
            for (at, info) in lines {
                match info {
                    Some(info) => map.insert(at, info),
                    None => map.remove(&at),
                };
            }
        }
        for (change, style) in sp.cells.iter().zip(cell_styles) {
            let mut cell = sh.cells.get(change.at).cloned().unwrap_or_default();
            if let Some(content) = &change.content {
                match content {
                    Content::Empty => {
                        cell.value = Value::Empty;
                        cell.formula = None;
                    }
                    Content::Value { value } => {
                        cell.value = value.clone();
                        cell.formula = None;
                    }
                    Content::Formula { text, array } => {
                        let mut f = Formula::new(text);
                        f.array = *array;
                        cell.value = Value::Empty;
                        cell.formula = Some(Arc::new(f));
                    }
                }
            }
            if let Some(style) = style {
                cell.style = style;
            }
            if cell.is_blank() {
                sh.cells.remove(change.at);
            } else {
                sh.cells.set(change.at, cell);
            }
            applied.cells.push((si, change.at));
        }
    }
    for f in &patch.fields {
        set_workbook_field(wb, f)?;
        applied.structural = true;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn keys<T: Serialize>(v: &T) -> BTreeSet<String> {
        serde_json::to_value(v).ok().and_then(|j| j.as_object().map(|o| o.keys().cloned().collect())).unwrap_or_default()
    }

    /// Every serialised field of a sheet or workbook either travels or is listed as local, so a
    /// field added to the model (here or upstream) can't silently stop replicating.
    #[test]
    fn every_field_is_replicated_or_local() {
        let mut sheet = Sheet::new("S");
        // Fields skipped when empty must show up too.
        sheet
            .cell_pictures
            .insert(CellRef::new(0, 0), Arc::new(gridcraft_model::CellPicture { data: vec![1], mime: "image/png".into(), alt: String::new() }));
        sheet.edit_ranges.push(gridcraft_model::EditRange { title: "R".into(), ranges: vec![], password: None, security_descriptor: None });
        let listed: BTreeSet<String> = SHEET_FIELDS.iter().chain(SHEET_LOCAL).map(|s| s.to_string()).collect();
        assert_eq!(keys(&sheet), listed, "a Sheet field isn't in sheet_fields! or SHEET_LOCAL");

        let mut wb = Workbook::new();
        wb.structure_password = Some(gridcraft_model::PasswordHash::Legacy(1));
        wb.csv_delimiter = Some(b';');
        wb.vba_project = Some(vec![1]);
        let listed: BTreeSet<String> = WORKBOOK_FIELDS.iter().chain(WORKBOOK_LOCAL).map(|s| s.to_string()).collect();
        assert_eq!(keys(&wb), listed, "a Workbook field isn't in workbook_fields! or WORKBOOK_LOCAL");
    }

    #[test]
    fn a_patch_round_trips_through_json() {
        let p = Patch {
            order: Some(vec![2, 1]),
            sheets: vec![SheetPatch {
                id: 1,
                new: false,
                fields: vec![Field { name: "name".into(), value: Some(Json::String("Budget".into())) }, Field { name: "freeze".into(), value: None }],
                cells: vec![
                    CellChange { at: CellRef::new(0, 0), content: Some(Content::Value { value: Value::Number(1.5) }), style: None },
                    CellChange {
                        at: CellRef::new(1, 0),
                        content: Some(Content::Formula { text: "A1*2".into(), array: None }),
                        style: Some(Style::default()),
                    },
                    CellChange { at: CellRef::new(2, 0), content: Some(Content::Empty), style: None },
                ],
                rows: vec![LineChange { at: 3, info: None }],
                cols: vec![],
            }],
            fields: vec![],
        };
        let json = serde_json::to_string(&p).unwrap_or_default();
        let back: Patch = serde_json::from_str(&json).unwrap_or_default();
        assert_eq!(back, p, "{json}");
    }
}
