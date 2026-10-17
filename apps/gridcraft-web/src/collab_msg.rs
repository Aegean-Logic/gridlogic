//! Co-editing messages from the page (`?host=parent`), checked before they reach the session.
//! No browser types here, so it's tested natively. The protocol is in
//! `packaging/web/README.md`.

use gridcraft_engine::collab::Incoming;

/// The largest log batch taken in one message (JSON text).
pub const MAX_JSON_BYTES: usize = 64 << 20;
/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// What the page asks of the shared session.
#[derive(Debug, PartialEq)]
pub enum FromPage {
    /// `gridcraft:collab:start {client, seq}`: the workbook just opened is the shared one as of
    /// log position `seq`; this person is `client`.
    Start { client: u64, seq: u64 },
    /// `gridcraft:collab:entries {json}`: log entries, in order, as JSON text.
    Entries(Vec<Incoming>),
    /// `gridcraft:collab:checkpoint {id}`: send the shared workbook to save.
    Checkpoint { id: u64 },
    /// `gridcraft:collab:stop`: the session ended.
    Stop,
}

/// The co-editing message in a page message of type `kind`, if it is one. `Err` for one that's
/// malformed (told back to the page).
pub fn parse(kind: &str, client: Option<f64>, seq: Option<f64>, json: Option<&str>, id: Option<f64>) -> Option<Result<FromPage, String>> {
    let need = |v: Option<f64>, what: &str| whole(v).ok_or_else(|| format!("{kind}: `{what}` must be a whole number"));
    Some(match kind {
        "gridcraft:collab:start" => need(client, "client").and_then(|client| Ok(FromPage::Start { client, seq: need(seq, "seq")? })),
        "gridcraft:collab:entries" => match json {
            None => Err(format!("{kind}: `json` is missing")),
            Some(text) if text.len() > MAX_JSON_BYTES => Err(format!("{kind}: more than {MAX_JSON_BYTES} bytes")),
            Some(text) => entries(text).map(FromPage::Entries).map_err(|e| format!("{kind}: {e}")),
        },
        "gridcraft:collab:checkpoint" => need(id, "id").map(|id| FromPage::Checkpoint { id }),
        "gridcraft:collab:stop" => Ok(FromPage::Stop),
        _ => return None,
    })
}

/// Log entries from JSON text. An entry whose op doesn't parse (a bug or a tampered client
/// elsewhere) keeps its place in the log as a no-op: every copy reads it the same way, so they
/// stay together, where refusing the batch would stall them all.
fn entries(text: &str) -> Result<Vec<Incoming>, String> {
    let raw: Vec<serde_json::Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
    raw.into_iter()
        .map(|v| {
            let num = |key: &str| v.get(key).and_then(serde_json::Value::as_u64).ok_or_else(|| format!("an entry without `{key}`"));
            let op = v.get("op").cloned().and_then(|o| serde_json::from_value(o).ok()).unwrap_or_default();
            Ok(Incoming { seq: num("seq")?, client: num("client")?, client_seq: num("client_seq")?, base: num("base")?, op })
        })
        .collect()
}

fn whole(v: Option<f64>) -> Option<u64> {
    let v = v?;
    ((0.0..=MAX_SAFE_INTEGER).contains(&v) && v.fract() == 0.0).then_some(v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_from_the_page_are_checked() {
        assert_eq!(parse("gridcraft:open", None, None, None, None), None, "not a co-editing message");
        assert_eq!(parse("gridcraft:collab:start", Some(3.0), Some(0.0), None, None), Some(Ok(FromPage::Start { client: 3, seq: 0 })));
        assert!(matches!(parse("gridcraft:collab:start", Some(3.5), Some(0.0), None, None), Some(Err(_))));
        assert!(matches!(parse("gridcraft:collab:start", Some(3.0), None, None, None), Some(Err(_))));
        assert_eq!(parse("gridcraft:collab:checkpoint", None, None, None, Some(7.0)), Some(Ok(FromPage::Checkpoint { id: 7 })));
        assert_eq!(parse("gridcraft:collab:stop", None, None, None, None), Some(Ok(FromPage::Stop)));

        let entry = Incoming { seq: 1, client: 2, client_seq: 1, base: 0, op: Default::default() };
        let entries = serde_json::to_string(&vec![entry]).unwrap_or_default();
        match parse("gridcraft:collab:entries", None, None, Some(&entries), None) {
            Some(Ok(FromPage::Entries(e))) => assert_eq!(e.len(), 1),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("gridcraft:collab:entries", None, None, Some("[{]"), None), Some(Err(_))));
        // A garbled op is a no-op in its place, not a stalled session.
        let garbled = r#"[{"seq":4,"client":2,"client_seq":9,"base":3,"op":{"edits":"nonsense"}}]"#;
        match parse("gridcraft:collab:entries", None, None, Some(garbled), None) {
            Some(Ok(FromPage::Entries(e))) => assert!(e.len() == 1 && e[0].seq == 4 && e[0].op.is_empty()),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("gridcraft:collab:entries", None, None, Some(r#"[{"client":2}]"#), None), Some(Err(_))), "no seq");
        assert!(matches!(parse("gridcraft:collab:entries", None, None, None, None), Some(Err(_))));
    }
}
