//! Save acknowledgements in host mode (`?host=parent&ack=1`).
//!
//! Without them, a workbook handed to the parent (`gridcraft:save`) counts as saved at once, even
//! when the parent then fails to store it. With them, every save carries an `id`. The workbook
//! counts as saved only when the parent answers `gridcraft:saved { id }`, and then as it was when
//! it was handed over: edits made since stay unsaved. `gridcraft:saveFailed { id, error }` leaves
//! it unsaved and tells the user. No browser types here, so it's tested natively.

use std::sync::Arc;

use gridcraft_engine::Session;
use gridcraft_engine::model::Workbook;

/// Saves still waiting for an answer. Past this the oldest is forgotten, so a parent that never
/// answers can't grow the list without bound.
const MAX_PENDING: usize = 64;
/// The longest file name taken from the parent.
const MAX_NAME_CHARS: usize = 255;
/// The longest error text taken from the parent.
const MAX_ERROR_CHARS: usize = 500;
/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The parent's answer to a `gridcraft:save`.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// Stored, under `name` when the parent stored it under another name (a copy, after a
    /// conflict).
    Saved {
        id: u64,
        name: Option<String>,
    },
    Failed {
        id: u64,
        error: String,
    },
}

impl Reply {
    /// The answer in a message from the parent: its `type`, `id`, and `name` or `error`. `None`
    /// for any other message, or one without a valid `id`.
    pub fn parse(kind: &str, id: Option<f64>, name: Option<String>, error: Option<String>) -> Option<Reply> {
        let id = parse_id(id)?;
        match kind {
            "gridcraft:saved" => Some(Reply::Saved { id, name: name.as_deref().and_then(clean_name) }),
            "gridcraft:saveFailed" => {
                let error: String = error.unwrap_or_default().trim().chars().take(MAX_ERROR_CHARS).collect();
                Some(Reply::Failed { id, error: if error.is_empty() { "the page couldn't store it".into() } else { error } })
            }
            _ => None,
        }
    }
}

struct Pending {
    id: u64,
    name: String,
    /// A workbook (rather than an export such as a PDF): storing it saves the document.
    workbook: bool,
    /// The document and the workbook as it was handed over, recorded after the frame that saved.
    doc: Option<(u64, Arc<Workbook>)>,
}

#[derive(Default)]
pub struct SaveAcks {
    last_id: u64,
    pending: Vec<Pending>,
}

impl SaveAcks {
    /// `name` is being handed to the parent: the id its answer will carry.
    pub fn begin(&mut self, name: &str, workbook: bool) -> u64 {
        self.last_id = self.last_id.saturating_add(1);
        if self.pending.len() >= MAX_PENDING {
            self.pending.remove(0);
        }
        self.pending.push(Pending { id: self.last_id, name: name.to_string(), workbook, doc: None });
        self.last_id
    }

    /// The save `id` never reached the parent.
    pub fn forget(&mut self, id: u64) {
        self.pending.retain(|p| p.id != id);
    }

    /// After the frame that saved: its workbook saves were of the active document as it is now.
    pub fn bind(&mut self, session: &Session) {
        let Some(doc) = session.active() else { return };
        for p in self.pending.iter_mut().filter(|p| p.workbook && p.doc.is_none()) {
            p.doc = Some((doc.uid, doc.wb.clone()));
        }
    }

    /// Apply the parent's answer; the message to show the user, if any.
    pub fn apply(&mut self, reply: Reply, session: &mut Session) -> Option<String> {
        match reply {
            Reply::Saved { id, name } => {
                let index = self.pending.iter().position(|p| p.id == id)?;
                let p = self.pending.remove(index);
                let (uid, wb) = p.doc?;
                // Earlier saves of the same document are superseded: a late answer to one of
                // them mustn't mark an older workbook as the saved one.
                self.pending.retain(|q| q.id > id || q.doc.as_ref().is_none_or(|(u, _)| *u != uid));
                let doc = session.document_mut(uid)?;
                doc.saved = wb;
                if doc.path.is_none() {
                    doc.title = name.unwrap_or(p.name);
                }
                None
            }
            Reply::Failed { id, error } => {
                let name = self.pending.iter().position(|p| p.id == id).map(|i| self.pending.remove(i).name);
                Some(match name {
                    Some(name) => format!("{name} wasn't saved: {error}"),
                    None => format!("The file wasn't saved: {error}"),
                })
            }
        }
    }
}

/// A save id from the parent: a whole, non-negative number a JavaScript number holds exactly.
fn parse_id(id: Option<f64>) -> Option<u64> {
    let id = id?;
    ((0.0..=MAX_SAFE_INTEGER).contains(&id) && id.fract() == 0.0).then_some(id as u64)
}

/// A file name from the parent: its last path component, trimmed and capped.
fn clean_name(name: &str) -> Option<String> {
    let name: String = name.rsplit(['/', '\\']).next().unwrap_or_default().trim().chars().take(MAX_NAME_CHARS).collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Session {
        let mut s = Session::new();
        let _ = s.execute("file.new", json!({}));
        s
    }

    fn edit(s: &mut Session, input: &str) {
        let _ = s.execute("cell.set", json!({"cell": "A1", "input": input}));
    }

    fn dirty(s: &Session) -> bool {
        s.active().is_some_and(|d| d.is_dirty())
    }

    #[test]
    fn saved_only_after_the_parent_says_so() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let id = acks.begin("Book.xlsx", true);
        acks.bind(&s);
        assert!(dirty(&s), "handing the workbook over doesn't save it yet");
        assert_eq!(acks.apply(Reply::Saved { id, name: None }, &mut s), None);
        assert!(!dirty(&s));
        assert_eq!(s.active().map(|d| d.title.as_str()), Some("Book.xlsx"));
    }

    #[test]
    fn a_failed_save_stays_unsaved_and_tells_the_user() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let id = acks.begin("Book.xlsx", true);
        acks.bind(&s);
        let shown = acks.apply(Reply::Failed { id, error: "412 Precondition Failed".into() }, &mut s);
        assert_eq!(shown.as_deref(), Some("Book.xlsx wasn't saved: 412 Precondition Failed"));
        assert!(dirty(&s));
        // A failure for an id we don't know still reaches the user.
        let shown = acks.apply(Reply::Failed { id: 99, error: "offline".into() }, &mut s);
        assert_eq!(shown.as_deref(), Some("The file wasn't saved: offline"));
    }

    #[test]
    fn edits_after_the_handover_stay_unsaved() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let id = acks.begin("Book.xlsx", true);
        acks.bind(&s);
        edit(&mut s, "2");
        acks.apply(Reply::Saved { id, name: None }, &mut s);
        assert!(dirty(&s), "the parent stored the workbook as it was, not the later edit");
    }

    #[test]
    fn a_late_answer_to_an_older_save_is_ignored() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let first = acks.begin("Book.xlsx", true);
        acks.bind(&s);
        edit(&mut s, "2");
        let second = acks.begin("Book.xlsx", true);
        acks.bind(&s);
        acks.apply(Reply::Saved { id: second, name: None }, &mut s);
        assert!(!dirty(&s));
        acks.apply(Reply::Saved { id: first, name: None }, &mut s);
        assert!(!dirty(&s), "the older workbook didn't become the saved one");
    }

    #[test]
    fn the_answer_finds_its_document_after_switching() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let id = acks.begin("First.xlsx", true);
        acks.bind(&s);
        let _ = s.execute("file.new", json!({}));
        edit(&mut s, "x");
        acks.apply(Reply::Saved { id, name: Some("First (copy).xlsx".into()) }, &mut s);
        let first = s.documents().first();
        assert!(first.is_some_and(|d| !d.is_dirty() && d.title == "First (copy).xlsx"));
        assert!(dirty(&s), "the other document is untouched");
    }

    #[test]
    fn exports_and_unknown_or_unsent_saves_change_nothing() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let pdf = acks.begin("Book.pdf", false);
        acks.bind(&s);
        acks.apply(Reply::Saved { id: pdf, name: None }, &mut s);
        assert!(dirty(&s), "storing a PDF doesn't save the workbook");
        let unsent = acks.begin("Book.xlsx", true);
        acks.forget(unsent);
        acks.bind(&s);
        acks.apply(Reply::Saved { id: unsent, name: None }, &mut s);
        acks.apply(Reply::Saved { id: 1234, name: None }, &mut s);
        assert!(dirty(&s));
    }

    #[test]
    fn a_closed_document_is_skipped() {
        let mut s = session();
        edit(&mut s, "1");
        let mut acks = SaveAcks::default();
        let id = acks.begin("Book.xlsx", true);
        acks.bind(&s);
        s.close_document(0);
        assert_eq!(acks.apply(Reply::Saved { id, name: None }, &mut s), None);
    }

    #[test]
    fn pending_saves_are_bounded() {
        let mut acks = SaveAcks::default();
        for _ in 0..MAX_PENDING * 3 {
            acks.begin("Book.xlsx", true);
        }
        assert_eq!(acks.pending.len(), MAX_PENDING);
    }

    #[test]
    fn replies_from_the_parent_are_checked() {
        let saved = |id: f64| Reply::parse("gridcraft:saved", Some(id), None, None);
        assert_eq!(saved(3.0), Some(Reply::Saved { id: 3, name: None }));
        for bad in [-1.0, 1.5, f64::NAN, f64::INFINITY, 1e300] {
            assert_eq!(saved(bad), None, "{bad}");
        }
        assert_eq!(Reply::parse("gridcraft:saved", None, None, None), None);
        assert_eq!(Reply::parse("gridcraft:open", Some(1.0), None, None), None);
        assert_eq!(
            Reply::parse("gridcraft:saved", Some(1.0), Some(" docs/Q3/Budget.xlsx ".into()), None),
            Some(Reply::Saved { id: 1, name: Some("Budget.xlsx".into()) })
        );
        assert_eq!(Reply::parse("gridcraft:saved", Some(1.0), Some("a\\b/ ".into()), None), Some(Reply::Saved { id: 1, name: None }));
        let long = "x".repeat(10_000);
        let Some(Reply::Failed { error, .. }) = Reply::parse("gridcraft:saveFailed", Some(1.0), None, Some(long)) else {
            panic!("a failure parses");
        };
        assert_eq!(error.chars().count(), MAX_ERROR_CHARS);
        assert_eq!(
            Reply::parse("gridcraft:saveFailed", Some(1.0), None, Some("  ".into())),
            Some(Reply::Failed { id: 1, error: "the page couldn't store it".into() })
        );
    }
}
