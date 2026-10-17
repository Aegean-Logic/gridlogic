//! Content patches: what an edit changed, as data another copy of the workbook can apply.
//!
//! A patch is the difference between the workbook before and after an edit, whatever command
//! made it: cells (content and style, separately), rows and columns, every other sheet setting
//! (replaced whole), the sheet list (by [`Sheet::id`]) and the workbook settings. Styles travel
//! as [`Style`] values, never as [`StyleId`]s (positions in one copy's style table): each
//! distinct style once in [`Patch::styles`], cells and lines naming it by its place there.
//! Cached formula results don't travel: the receiver recalculates. What each person sees on
//! their own screen (selection, active sheet, scroll position, zoom, view mode) isn't part of it.

use std::collections::HashMap;
use std::sync::Arc;

use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, Formula, LineInfo, Sheet, Style, StyleId, StyleTable, Workbook};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{EngineError, Result};

/// What an edit changed. Empty when it changed nothing another person would see.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    /// The sheet ids in order after the edit, when it added, removed or reordered sheets. Sheets
    /// it doesn't name (added by someone else meanwhile) keep their order, after these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<u64>>,
    /// Sheets the edit deleted, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<SheetPatch>,
    /// Workbook settings (defined names, properties, calculation options…), replaced whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
    /// The styles cells and lines take, each once (formatting a large range sends one style).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<Style>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.order.is_none() && self.removed.is_empty() && self.sheets.is_empty() && self.fields.is_empty()
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

/// A setting by its name in the file format: its new value (`None`: back to the default), or for a
/// list or map of things (charts, conditional formats, comments…) the items added, changed and
/// removed, so two people adding a chart at once both keep theirs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Items>,
}

/// The items of a list or map setting that changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Items {
    pub by: By,
    /// Items added or changed (an `{k, v}` pair for a map).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<Json>,
    /// Items removed, in full (so they shift with structural edits like the rest).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<Json>,
}

/// How the items of a setting are told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum By {
    /// Objects with an `id` (charts, tables…).
    Id,
    /// Whole values (conditional formats, merges…): a change is a removal and an addition.
    Value,
    /// A map's keys (comments by cell…).
    Key,
}

/// Settings whose items merge one by one, and how.
fn merge_by(name: &str) -> Option<By> {
    match name {
        "charts" | "images" | "shapes" | "tables" | "slicers" | "pivots" => Some(By::Id),
        "merges" | "cond_formats" | "validations" | "sparklines" | "edit_ranges" | "cell_pictures" | "names" | "custom_lists" | "cell_styles" => {
            Some(By::Value)
        }
        "comments" | "hyperlinks" => Some(By::Key),
        _ => None,
    }
}

impl Items {
    /// What tells an item apart, for matching it in another list.
    pub fn key(by: By, item: &Json) -> String {
        match by {
            By::Id => item.get("id").map_or_else(|| item.to_string(), |id| format!("id:{id}")),
            By::Key => item.get("k").and_then(Json::as_str).map_or_else(|| item.to_string(), str::to_string),
            By::Value => item.to_string(),
        }
    }

    /// `list` as the setting's own JSON (an array, or for a map an object).
    pub fn container(by: By, list: &[Json]) -> Json {
        match by {
            By::Id | By::Value => Json::Array(list.to_vec()),
            By::Key => Json::Object(list.iter().filter_map(|e| Some((e.get("k")?.as_str()?.to_string(), e.get("v")?.clone()))).collect()),
        }
    }

    /// The items of the setting's own JSON.
    pub fn items(by: By, value: Option<&Json>) -> Vec<Json> {
        match (by, value) {
            (By::Id | By::Value, Some(Json::Array(a))) => a.clone(),
            (By::Key, Some(Json::Object(m))) => m.iter().map(|(k, v)| serde_json::json!({"k": k, "v": v})).collect(),
            _ => Vec::new(),
        }
    }

    /// The changes from `before` to `after` (the setting's JSON), or `None` if nothing changed.
    fn diff(by: By, before: Option<&Json>, after: Option<&Json>) -> Option<Items> {
        let (b, a) = (Items::items(by, before), Items::items(by, after));
        let (set, remove) = match by {
            By::Id | By::Key => {
                let old: HashMap<String, &Json> = b.iter().map(|e| (Items::key(by, e), e)).collect();
                let new: HashMap<String, &Json> = a.iter().map(|e| (Items::key(by, e), e)).collect();
                let set: Vec<Json> = a.iter().filter(|e| old.get(&Items::key(by, e)) != Some(e)).cloned().collect();
                let remove: Vec<Json> = b.iter().filter(|e| !new.contains_key(&Items::key(by, e))).cloned().collect();
                (set, remove)
            }
            By::Value => {
                // Count-aware: an item twice in one list and once in the other changed once.
                let mut left: Vec<Option<&Json>> = b.iter().map(Some).collect();
                let mut set = Vec::new();
                for e in &a {
                    match left.iter().position(|o| *o == Some(e)) {
                        Some(i) => left[i] = None,
                        None => set.push(e.clone()),
                    }
                }
                (set, left.into_iter().flatten().cloned().collect())
            }
        };
        (!set.is_empty() || !remove.is_empty()).then_some(Items { by, set, remove })
    }

    /// The setting's JSON with these changes made.
    fn apply(&self, current: Option<Json>) -> Json {
        let by = self.by;
        let mut list = Items::items(by, current.as_ref());
        match by {
            By::Id | By::Key => {
                let gone: std::collections::HashSet<String> = self.remove.iter().map(|e| Items::key(by, e)).collect();
                list.retain(|e| !gone.contains(&Items::key(by, e)));
                for s in &self.set {
                    let k = Items::key(by, s);
                    match list.iter().position(|e| Items::key(by, e) == k) {
                        Some(i) => list[i] = s.clone(),
                        None => list.push(s.clone()),
                    }
                }
            }
            By::Value => {
                for r in &self.remove {
                    if let Some(i) = list.iter().position(|e| e == r) {
                        list.remove(i);
                    }
                }
                for s in &self.set {
                    if !list.contains(s) {
                        list.push(s.clone());
                    }
                }
            }
        }
        Items::container(by, &list)
    }
}

/// The field for a setting that changed from `before` to `after` (each as [`field_of`] gives it):
/// item by item where the setting merges that way, otherwise the new value whole.
fn changed(before: Field, after: Field) -> Option<Field> {
    match merge_by(&after.name) {
        Some(by) => {
            Items::diff(by, before.value.as_ref(), after.value.as_ref()).map(|items| Field { name: after.name, value: None, items: Some(items) })
        }
        None => Some(after),
    }
}

/// One cell's change, or the same change to every cell of the rectangle `at:to` (formatting or
/// filling a range sends one entry). A part that didn't change is `None`, so a style change and a
/// value typed into the same cell by someone else both survive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellChange {
    pub at: CellRef,
    /// The bottom-right corner, when this is a rectangle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<CellRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    /// The cell's style, by its place in [`Patch::styles`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<u32>,
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

/// [`LineInfo`] with its style by its place in [`Patch::styles`].
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
    pub style: Option<u32>,
    #[serde(default)]
    pub custom: bool,
}

/// Builds [`Patch::styles`]: each distinct style once.
#[derive(Default)]
struct Styles {
    list: Vec<Style>,
    index: HashMap<Style, u32>,
}

impl Styles {
    fn id(&mut self, s: &Style) -> u32 {
        if let Some(i) = self.index.get(s) {
            return *i;
        }
        let i = self.list.len() as u32;
        self.list.push(s.clone());
        self.index.insert(s.clone(), i);
        i
    }
}

/// A line's settings with its style as a value, for comparing two copies.
#[derive(PartialEq)]
struct LineValue<'a> {
    size: Option<f32>,
    hidden: bool,
    outline: u8,
    collapsed: bool,
    style: Option<&'a Style>,
    custom: bool,
}

impl<'a> LineValue<'a> {
    fn of(info: &LineInfo, styles: &'a StyleTable) -> LineValue<'a> {
        LineValue {
            size: info.size,
            hidden: info.hidden,
            outline: info.outline,
            collapsed: info.collapsed,
            style: info.style.map(|s| styles.get(s)),
            custom: info.custom,
        }
    }
    fn line(&self, styles: &mut Styles) -> Line {
        Line {
            size: self.size,
            hidden: self.hidden,
            outline: self.outline,
            collapsed: self.collapsed,
            style: self.style.map(|s| styles.id(s)),
            custom: self.custom,
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
                    let (mut pa, mut pb) = (Sheet::new(""), Sheet::new(""));
                    pa.$f = a.$f.clone();
                    pb.$f = b.$f.clone();
                    out.extend(changed(field_of(&pa, stringify!($f))?, field_of(&pb, stringify!($f))?));
                }
            )*
            Ok(out)
        }
        /// A setting of `sh` as JSON, by name.
        pub(super) fn sheet_field(sh: &Sheet, name: &str) -> Result<Field> {
            match name {
                $(stringify!($f) => {
                    let mut probe = Sheet::new("");
                    probe.$f = sh.$f.clone();
                    field_of(&probe, name)
                })*
                other => Err(EngineError::Other(format!("co-editing: unknown sheet setting `{other}`"))),
            }
        }
        pub(super) fn set_sheet_field(sh: &mut Sheet, field: &Field) -> Result<()> {
            let merged;
            let field = match &field.items {
                Some(items) => {
                    let current = sheet_field(sh, &field.name)?.value;
                    merged = Field { name: field.name.clone(), value: Some(items.apply(current)), items: None };
                    &merged
                }
                None => field,
            };
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
                    let (mut pa, mut pb) = (Workbook::new(), Workbook::new());
                    pa.$f = a.$f.clone();
                    pb.$f = b.$f.clone();
                    out.extend(changed(field_of(&pa, stringify!($f))?, field_of(&pb, stringify!($f))?));
                }
            )*
            Ok(out)
        }
        /// A setting of `wb` as JSON, by name.
        pub(super) fn workbook_field(wb: &Workbook, name: &str) -> Result<Field> {
            match name {
                $(stringify!($f) => {
                    let mut probe = Workbook::new();
                    probe.$f = wb.$f.clone();
                    field_of(&probe, name)
                })*
                other => Err(EngineError::Other(format!("co-editing: unknown workbook setting `{other}`"))),
            }
        }
        pub(super) fn set_workbook_field(wb: &mut Workbook, field: &Field) -> Result<()> {
            let merged;
            let field = match &field.items {
                Some(items) => {
                    let current = workbook_field(wb, &field.name)?.value;
                    merged = Field { name: field.name.clone(), value: Some(items.apply(current)), items: None };
                    &merged
                }
                None => field,
            };
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
pub(super) fn field_of<T: Serialize>(probe: &T, name: &str) -> Result<Field> {
    let json = serde_json::to_value(probe).map_err(|e| EngineError::Other(format!("co-editing: {name}: {e}")))?;
    let value = json.get(name).filter(|v| !v.is_null()).cloned();
    Ok(Field { name: name.to_string(), value, items: None })
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
    let mut styles = Styles::default();
    let mut sheets = Vec::new();
    for sa in &after.sheets {
        let old = before.sheet_by_id(sa.id).and_then(|i| before.sheets.get(i));
        if old.is_some_and(|sb| Arc::ptr_eq(sb, sa)) {
            continue;
        }
        let sb = old.map_or(&blank, |s| s.as_ref());
        let p = diff_sheet(&before.styles, sb, &after.styles, sa, old.is_none(), &mut styles)?;
        if !p.is_empty() {
            sheets.push(p);
        }
    }
    let removed = before.sheets.iter().map(|s| s.id).filter(|id| after.sheet_by_id(*id).is_none()).collect();
    Ok(Patch { order, removed, sheets, fields: diff_workbook_fields(before, after)?, styles: styles.list })
}

fn diff_sheet(sb_styles: &StyleTable, sb: &Sheet, sa_styles: &StyleTable, sa: &Sheet, new: bool, styles: &mut Styles) -> Result<SheetPatch> {
    let style_of = |table: &StyleTable, cell: Option<&Cell>| table.get(cell.map_or(StyleId::DEFAULT, |c| c.style)).clone();
    let mut cells = Vec::new();
    for at in sb.cells.diff(&sa.cells) {
        let (cb, ca) = (sb.cells.get(at), sa.cells.get(at));
        let content = Some(Content::of(ca)).filter(|c| *c != Content::of(cb));
        let style = Some(style_of(sa_styles, ca)).filter(|s| *s != style_of(sb_styles, cb)).map(|s| styles.id(&s));
        // A cell whose cached result changed (a dependent of the edit) has nothing to send.
        if content.is_some() || style.is_some() {
            cells.push(CellChange { at, to: None, content, style });
        }
    }
    cells.sort_by_key(|c| c.at);
    let cells = compress_cells(cells);
    Ok(SheetPatch {
        id: sa.id,
        new,
        fields: diff_sheet_fields(sb, sa)?,
        cells,
        rows: diff_lines(sb_styles, &sb.rows, sa_styles, &sa.rows, styles),
        cols: diff_lines(sb_styles, &sb.cols, sa_styles, &sa.cols, styles),
    })
}

fn diff_lines(
    b_styles: &StyleTable,
    b: &std::collections::BTreeMap<u32, LineInfo>,
    a_styles: &StyleTable,
    a: &std::collections::BTreeMap<u32, LineInfo>,
    styles: &mut Styles,
) -> Vec<LineChange> {
    let mut keys: Vec<u32> = b.keys().chain(a.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .filter_map(|at| {
            let (lb, la) = (b.get(&at).map(|i| LineValue::of(i, b_styles)), a.get(&at).map(|i| LineValue::of(i, a_styles)));
            (lb != la).then(|| LineChange { at, info: la.map(|l| l.line(styles)) })
        })
        .collect()
}

// ----------------------------------------------------------------------- cell rectangles

/// The most cells one patch may change, its rectangles counted in full. A real edit stays far
/// below (an op is capped at a few megabytes); a garbled one claiming a whole sheet is refused.
pub const MAX_PATCH_CELLS: u64 = 5_000_000;

impl CellChange {
    /// The cells it covers.
    pub fn range(&self) -> RangeRef {
        RangeRef::new(self.at, self.to.unwrap_or(self.at))
    }
    fn area(&self) -> u64 {
        let r = self.range();
        u64::from(r.end.row.saturating_sub(r.start.row))
            .saturating_add(1)
            .saturating_mul(u64::from(r.end.col.saturating_sub(r.start.col)).saturating_add(1))
    }
}

/// One entry per cell (rectangles taken apart), in row-major order.
pub fn expand_cells(cells: &[CellChange]) -> Vec<CellChange> {
    let mut out = Vec::new();
    for c in cells {
        let r = c.range();
        for row in r.start.row..=r.end.row {
            for col in r.start.col..=r.end.col {
                out.push(CellChange { at: CellRef::new(row, col), to: None, content: c.content.clone(), style: c.style });
            }
        }
    }
    out.sort_by_key(|c| c.at);
    out
}

/// Single-cell changes (in row-major order) with identical neighbours merged into rectangles:
/// runs along each row, then runs of the same columns down consecutive rows.
pub fn compress_cells(cells: Vec<CellChange>) -> Vec<CellChange> {
    let same = |a: &CellChange, b: &CellChange| a.content == b.content && a.style == b.style;
    // Runs along rows.
    let mut runs: Vec<CellChange> = Vec::new();
    for c in cells {
        if let Some(last) = runs.last_mut() {
            let end = last.to.unwrap_or(last.at);
            if end.row == c.at.row && end.col.checked_add(1) == Some(c.at.col) && same(last, &c) {
                last.to = Some(c.at);
                continue;
            }
        }
        runs.push(c);
    }
    // Runs of the same columns and change, down consecutive rows.
    let mut out: Vec<CellChange> = Vec::new();
    let mut open: HashMap<(u32, u32, String), usize> = HashMap::new();
    for run in runs {
        let end = run.to.unwrap_or(run.at);
        let key = (run.at.col, end.col, serde_json::to_string(&(&run.content, run.style)).unwrap_or_default());
        if let Some(&i) = open.get(&key)
            && let Some(rect) = out.get_mut(i)
            && rect.to.unwrap_or(rect.at).row.checked_add(1) == Some(run.at.row)
        {
            rect.to = Some(CellRef::new(run.at.row, end.col));
            continue;
        }
        open.insert(key, out.len());
        out.push(run);
    }
    for c in out.iter_mut() {
        if c.to == Some(c.at) {
            c.to = None;
        }
    }
    out
}

// ----------------------------------------------------------------------------- applying

/// Applies `patch` to `wb`. Changes to sheets that are gone (deleted meanwhile) are skipped. The
/// caller recalculates what [`Applied`] lists.
pub fn apply_patch(wb: &mut Workbook, patch: &Patch) -> Result<Applied> {
    let mut applied = Applied::default();
    let active_id = wb.active().map(|s| s.id);

    // Deleted sheets go first, but a workbook always keeps one sheet.
    for id in &patch.removed {
        if wb.sheets.len() > 1
            && let Some(i) = wb.sheet_by_id(*id)
        {
            wb.sheets.remove(i);
            applied.structural = true;
        }
    }
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
        let mut sheets = Vec::with_capacity(wb.sheets.len());
        for id in order {
            if let Some(i) = wb.sheet_by_id(*id)
                && let Some(sh) = wb.sheets.get(i)
                && !sheets.iter().any(|s: &Arc<Sheet>| s.id == *id)
            {
                sheets.push(sh.clone());
            }
        }
        // Sheets the order doesn't name (someone else's, added meanwhile) follow, as they were.
        let rest: Vec<Arc<Sheet>> = wb.sheets.iter().filter(|s| !sheets.iter().any(|t| t.id == s.id)).cloned().collect();
        sheets.extend(rest);
        wb.sheets = sheets;
        applied.structural = true;
    }
    // The active sheet is each person's own: keep showing the same one if it's still there.
    wb.active_sheet = active_id.and_then(|id| wb.sheet_by_id(id)).unwrap_or(0).min(wb.sheets.len().saturating_sub(1));

    let cells: u64 = patch.sheets.iter().flat_map(|sp| sp.cells.iter()).map(CellChange::area).fold(0, u64::saturating_add);
    if cells > MAX_PATCH_CELLS {
        return Err(EngineError::Other(format!("co-editing: an edit of {cells} cells is more than one edit may change")));
    }
    // The patch's styles in this copy's table. A style number past the table (a garbled op)
    // leaves that cell's or line's style as it is.
    let ids: Vec<StyleId> = patch.styles.iter().map(|s| wb.styles.intern(s.clone())).collect();
    let style_id = |i: Option<u32>| i.and_then(|i| ids.get(i as usize).copied());
    for sp in &patch.sheets {
        let Some(si) = wb.sheet_by_id(sp.id) else { continue };
        let cell_styles: Vec<Option<StyleId>> = sp.cells.iter().map(|c| style_id(c.style)).collect();
        let line = |l: &LineChange| -> Option<LineInfo> {
            l.info.as_ref().map(|i| LineInfo {
                size: i.size,
                hidden: i.hidden,
                outline: i.outline,
                collapsed: i.collapsed,
                style: style_id(i.style),
                custom: i.custom,
            })
        };
        let rows: Vec<(u32, Option<LineInfo>)> = sp.rows.iter().map(|l| (l.at, line(l))).collect();
        let cols: Vec<(u32, Option<LineInfo>)> = sp.cols.iter().map(|l| (l.at, line(l))).collect();
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
            let r = change.range();
            for at in (r.start.row..=r.end.row).flat_map(|row| (r.start.col..=r.end.col).map(move |col| CellRef::new(row, col))) {
                let mut cell = sh.cells.get(at).cloned().unwrap_or_default();
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
                    sh.cells.remove(at);
                } else {
                    sh.cells.set(at, cell);
                }
                applied.cells.push((si, at));
            }
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
            removed: vec![3],
            sheets: vec![SheetPatch {
                id: 1,
                new: false,
                fields: vec![
                    Field { name: "name".into(), value: Some(Json::String("Budget".into())), items: None },
                    Field { name: "freeze".into(), value: None, items: None },
                    Field { name: "merges".into(), value: None, items: Some(Items { by: By::Value, set: vec![Json::Null], remove: vec![] }) },
                ],
                cells: vec![
                    CellChange { at: CellRef::new(0, 0), to: None, content: Some(Content::Value { value: Value::Number(1.5) }), style: None },
                    CellChange {
                        at: CellRef::new(1, 0),
                        to: Some(CellRef::new(1, 3)),
                        content: Some(Content::Formula { text: "A1*2".into(), array: None }),
                        style: Some(0),
                    },
                    CellChange { at: CellRef::new(2, 0), to: None, content: Some(Content::Empty), style: None },
                ],
                rows: vec![LineChange { at: 3, info: None }],
                cols: vec![],
            }],
            fields: vec![],
            styles: vec![Style::default()],
        };
        let json = serde_json::to_string(&p).unwrap_or_default();
        let back: Patch = serde_json::from_str(&json).unwrap_or_default();
        assert_eq!(back, p, "{json}");
    }

    #[test]
    fn identical_neighbours_travel_as_rectangles() {
        let bold = |row: u32, col: u32| CellChange { at: CellRef::new(row, col), to: None, content: None, style: Some(0) };
        let typed = |row: u32, col: u32, v: f64| CellChange {
            at: CellRef::new(row, col),
            to: None,
            content: Some(Content::Value { value: Value::Number(v) }),
            style: None,
        };
        // A 3x4 block of bold, a different change in its middle row's end, and a lone cell.
        let mut cells = Vec::new();
        for row in 0..3 {
            for col in 0..4 {
                cells.push(if row == 1 && col == 3 { typed(row, col, 7.0) } else { bold(row, col) });
            }
        }
        cells.push(typed(9, 9, 1.0));
        let packed = compress_cells(cells.clone());
        assert!(packed.len() <= 5, "{packed:?}");
        assert_eq!(expand_cells(&packed), cells, "nothing lost or added");
        let rects: u64 = packed.iter().map(CellChange::area).sum();
        assert_eq!(rects, cells.len() as u64);
    }

    #[test]
    fn a_patch_claiming_a_whole_sheet_is_refused() {
        let mut wb = Workbook::new();
        wb.ensure_sheet_ids();
        let id = wb.sheets.first().map(|s| s.id).unwrap_or_default();
        let huge = CellChange { at: CellRef::new(0, 0), to: Some(CellRef::new(1_048_575, 16_383)), content: Some(Content::Empty), style: None };
        let p = Patch { sheets: vec![SheetPatch { id, cells: vec![huge], ..Default::default() }], ..Default::default() };
        assert!(apply_patch(&mut wb, &p).is_err());
    }
}
