//! Co-editing convergence: several people edit one workbook at once through a simulated server
//! with random delays; once everything is delivered, every copy is the same workbook.
//!
//! Soak with `GRIDCRAFT_COLLAB_SEEDS=2000 GRIDCRAFT_COLLAB_STEPS=200`; replay one failure with
//! `GRIDCRAFT_COLLAB_SEED=<n>`.

use std::collections::VecDeque;
use std::sync::Arc;

use gridcraft_engine::collab::{self, Incoming, Outgoing, Replica};
use gridcraft_engine::model::Workbook;
use gridcraft_engine::{DocState, Session};
use serde_json::{Value as Json, json};

const PEOPLE: usize = 3;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

struct Person {
    session: Session,
    replica: Replica,
    to_server: VecDeque<Outgoing>,
    /// Next log entry to deliver.
    cursor: usize,
}

fn col(c: u64) -> char {
    (b'A' + c as u8) as char
}

fn cell(rng: &mut Rng) -> String {
    format!("{}{}", col(rng.below(8)), rng.below(20) + 1)
}

fn range(rng: &mut Rng) -> String {
    let (c, r) = (rng.below(7), rng.below(18) + 1);
    format!("{}{}:{}{}", col(c), r, col(c + rng.below(2)), r + rng.below(3))
}

/// A random edit with explicit targets.
fn edit(rng: &mut Rng, sheets: &[String]) -> (&'static str, Json) {
    let r = rng.below(100);
    let row = rng.below(18) + 1;
    let c = rng.below(6);
    let other = rng.pick(sheets).clone();
    match r {
        0..=4 => ("edit.undo", json!({})),
        5..=29 => {
            let input = match rng.below(5) {
                0 => format!("{}", rng.below(1000)),
                1 => format!("text {}", rng.below(50)),
                2 => format!("={}+{}", cell(rng), cell(rng)),
                3 => format!("=SUM({})", range(rng)),
                _ => format!("='{other}'!{}*2", cell(rng)),
            };
            ("cell.set", json!({"cell": cell(rng), "input": input}))
        }
        30..=37 => ("home.bold", json!({"range": range(rng), "on": rng.below(2) == 0})),
        38..=42 => ("home.fillColor", json!({"range": range(rng), "color": *rng.pick(&["#FFEE00", "#33AA77", "none"])})),
        43..=49 => ("home.insertRows", json!({"rows": format!("{row}:{}", row + rng.below(2))})),
        50..=55 => ("home.deleteRows", json!({"rows": format!("{row}:{}", row + rng.below(3))})),
        56..=59 => ("home.insertColumns", json!({"cols": format!("{}:{}", col(c), col(c + rng.below(2)))})),
        60..=63 => ("home.deleteColumns", json!({"cols": format!("{}:{}", col(c), col(c))})),
        64..=66 => ("home.insertCells", json!({"range": range(rng), "shift": *rng.pick(&["down", "right"])})),
        67..=69 => ("home.deleteCells", json!({"range": range(rng), "shift": *rng.pick(&["up", "left"])})),
        70..=71 => ("home.mergeCenter", json!({"range": range(rng)})),
        72 => ("review.newNote", json!({"cell": cell(rng), "text": format!("note {}", rng.below(9))})),
        73..=74 => ("home.rowHeight", json!({"rows": format!("{row}:{row}"), "height": 12 + rng.below(30)})),
        75 => (
            "home.conditionalFormat",
            json!({"range": range(rng), "rule": {"type": "cellIs", "operator": "greater", "value": rng.below(50), "preset": "lightRedFill"}}),
        ),
        76..=78 => ("home.insertSheet", json!({})),
        79..=80 => ("sheet.rename", json!({"name": format!("S{}", rng.below(1000))})),
        81..=82 => ("home.deleteSheet", json!({})),
        83..=87 => ("sheet.activate", json!({"sheet": rng.below(sheets.len() as u64)})),
        88..=90 => ("edit.cut", json!({"range": range(rng)})),
        91..=93 => ("edit.paste", json!({"at": cell(rng)})),
        94 => ("data.sortAscending", json!({"range": range(rng)})),
        95 => ("insert.chart", json!({"range": range(rng), "type": "line", "at": cell(rng)})),
        96 => ("edit.redo", json!({})),
        _ => ("formulas.defineName", json!({"name": format!("N{}", rng.below(5)), "refersTo": format!("=$A${row}:$B${}", row + 2)})),
    }
}

fn doc(p: &mut Person) -> &mut DocState {
    match p.session.doc_mut() {
        Ok(d) => d,
        Err(e) => panic!("no document: {e}"),
    }
}

fn deliver(p: &mut Person, log: &[Incoming], seed: u64) {
    let Some(msg) = log.get(p.cursor).cloned() else { return };
    let client = p.replica.client();
    let d = match p.session.doc_mut() {
        Ok(d) => d,
        Err(e) => panic!("seed {seed}: client {client}: {e}"),
    };
    if let Err(e) = p.replica.receive(msg, d) {
        panic!("seed {seed}: client {client} at {}: {e}", p.cursor + 1);
    }
    p.cursor += 1;
}

/// Runs one session; returns (edits that ran, log entries, structural log entries).
fn run_seed(seed: u64, steps: usize) -> (usize, usize, usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut start = Workbook::new();
    let _ = &mut start;
    let mut people: Vec<Person> = (0..PEOPLE)
        .map(|i| {
            let mut session = Session::new();
            session.add_document(DocState::new(start.clone(), None, "Shared".into()));
            for (c, v) in [("A1", "1"), ("B2", "=A1*10"), ("C3", "x")] {
                let _ = session.execute("cell.set", json!({"cell": c, "input": v}));
            }
            let wb = session.active().map(|d| d.wb.clone()).unwrap_or_default();
            let replica = match Replica::new(i as u64 + 1, wb, 0) {
                Ok(r) => r,
                Err(e) => panic!("{e}"),
            };
            Person { session, replica, to_server: VecDeque::new(), cursor: 0 }
        })
        .collect();
    let mut log: Vec<Incoming> = Vec::new();
    let mut ran = 0;

    for _ in 0..steps {
        let who = rng.below(PEOPLE as u64) as usize;
        match rng.below(10) {
            0..=3 => {
                let p = &mut people[who];
                let sheets: Vec<String> = p.session.active().map(|d| d.wb.sheets.iter().map(|s| s.name.clone()).collect()).unwrap_or_default();
                let (id, params) = edit(&mut rng, &sheets);
                if p.replica.run(&mut p.session, id, params).is_ok() {
                    ran += 1;
                }
            }
            4..=5 => {
                let p = &mut people[who];
                if let Some(out) = p.replica.outgoing() {
                    p.to_server.push_back(out);
                }
            }
            6 => {
                // The server takes the next op from whoever's network delivers first.
                if let Some(out) = people[who].to_server.pop_front() {
                    log.push(Incoming { seq: log.len() as u64 + 1, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op });
                }
            }
            _ => deliver(&mut people[who], &log, seed),
        }
    }

    // Everything settles: send, sequence and deliver until nobody has anything left.
    for _ in 0..10_000 {
        let mut busy = false;
        for p in people.iter_mut() {
            if let Some(out) = p.replica.outgoing() {
                p.to_server.push_back(out);
            }
        }
        for p in people.iter_mut() {
            while let Some(out) = p.to_server.pop_front() {
                log.push(Incoming { seq: log.len() as u64 + 1, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op });
                busy = true;
            }
        }
        for p in people.iter_mut() {
            while p.cursor < log.len() {
                deliver(p, &log, seed);
                busy = true;
            }
            busy |= p.replica.has_pending();
        }
        if !busy {
            break;
        }
    }

    let first = people[0].replica.confirmed().clone();
    for p in people.iter_mut() {
        let client = p.replica.client();
        assert!(!p.replica.has_pending(), "seed {seed}: client {client} still has edits pending");
        let diff = collab::make_patch(&first, p.replica.confirmed()).unwrap_or_else(|e| panic!("{e}"));
        assert!(diff.is_empty(), "seed {seed}: client {client}'s workbook differs from client 1's: {diff:?}");
        let confirmed = p.replica.confirmed().clone();
        let shown = doc(p).wb.clone();
        let diff = collab::make_patch(&confirmed, &shown).unwrap_or_else(|e| panic!("{e}"));
        assert!(diff.is_empty(), "seed {seed}: client {client} shows something other than the shared workbook: {diff:?}");
    }
    // The same content recalculates to the same values on every screen.
    let values = |p: &mut Person| -> Vec<(u64, String, String)> {
        let wb: Arc<Workbook> = doc(p).wb.clone();
        wb.sheets.iter().flat_map(|s| s.cells.iter().map(move |(at, c)| (s.id, at.to_string(), format!("{:?}", c.value)))).collect()
    };
    let expected = values(&mut people[0]);
    for p in people.iter_mut().skip(1) {
        assert_eq!(values(p), expected, "seed {seed}: values differ");
    }
    (ran, log.len(), log.iter().filter(|m| !m.op.edits.is_empty()).count())
}

fn env(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

#[test]
fn concurrent_editors_converge() {
    let steps = env("GRIDCRAFT_COLLAB_STEPS").unwrap_or(80) as usize;
    if let Some(seed) = env("GRIDCRAFT_COLLAB_SEED") {
        let stats = run_seed(seed, steps);
        eprintln!("seed {seed}: {stats:?}");
        return;
    }
    let (mut ran, mut ops, mut structural) = (0, 0, 0);
    for seed in 1..=env("GRIDCRAFT_COLLAB_SEEDS").unwrap_or(60) {
        let (r, o, s) = run_seed(seed, steps);
        ran += r;
        ops += o;
        structural += s;
    }
    eprintln!("collab fuzz: {ran} edits ran, {ops} ops through the log, {structural} with structural edits");
    // The test only proves something if people actually edited, concurrently, structurally.
    assert!(ops >= ran / 2 && structural >= ops / 20, "too little traffic: {ran} edits, {ops} ops, {structural} structural");
}
