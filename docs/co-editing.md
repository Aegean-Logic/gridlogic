# Real-time co-editing: design

Status: draft, 2026-10-17. Fork-specific (Aegean Office). Not upstream behaviour.

Goal: several people edit one workbook at once, as in Excel Online. Each person sees the
others' edits within about a second and their selections live, and everyone ends with the same
workbook. First host: Nextcloud (`gridcraft-nextcloud`), through the web build's host bridge.

## 1. What the engine gives us, and what gets in the way

Findings from the engine (paths under `crates/`):

| Fact | Where | Consequence |
|---|---|---|
| Every edit is a command; `Session::execute` journals `(id, params)` | `engine/src/lib.rs:322-365` | A natural unit to replicate… |
| …but params are recorded **as passed**, and most commands fill gaps from the local selection and active sheet (`target_ranges`, `target_sheet`, toggles read the active cell) | `engine/src/cmd/mod.rs:237-270`, `format.rs:267-280` | Replaying another user's `{}` acts on *my* selection. Raw command replay is wrong. |
| Paste, format painter and Find read session state (clipboard, painter, last find) | `lib.rs:66-84`, `edit.rs:779-936, 1568-1594` | Same params, different result per client. |
| Undo is whole-workbook snapshots (`HistoryEntry { wb: Arc<Workbook> }`) | `lib.rs:59-64`, `edit.rs:597-633` | Cheap rollback (O(1) `Arc` swap + `calc.rebuild`). But undo restores a snapshot, which would wipe other people's edits. |
| Cells are copy-on-write 64-row bands; `CellStore::diff` skips shared bands | `model/src/store.rs` | Diffing before/after an edit is cheap. |
| Structural edits go through one vocabulary, `formula::adjust::Edit` (Insert/Delete/Move/InsertCells/DeleteCells/MoveToSheet), and `map_range` maps a range through it | `formula/src/adjust.rs:81-99`, `engine/src/cmd/sheet.rs:217, 950` | We can transform addresses with existing code. |
| No stable sheet id: sheets are found by name or index | `model/src/sheet.rs:53` | Ops need one. |
| `StyleId`s are positions in an append-only table, in interning order | `model/src/style.rs:362-406` | Style ids aren't portable between clients. Send style values. |
| Object ids are `max + 1` per workbook | `model/src/workbook.rs:258` | Two users inserting a chart at once both pick the same id. |
| The whole model is serde-serialisable | `io.rs:147-153, 212` | Snapshots and patches can be JSON. |
| `RAND` state and `NOW` are per client | `calc/src/recalc.rs:369-382, 773-794` | Volatile results can differ between screens; accepted (§6). |

## 2. Approach: send results, and transform the few structural edits

An edit made locally is sent as an **op**, which has two parts:

1. **Structural edits** (`StructEdit`): what the command did to the grid's shape, as the
   engine's own `adjust::Edit` plus a stable sheet id. Examples: insert 2 rows at 5 on sheet 3;
   delete columns C:D; move B2:C9 to F2; add, rename, move or delete a sheet.
   - They are recorded by instrumenting the places every structural command already funnels
     through: `rewrite()` / `shift_sheet_features` (`sheet.rs:115, 217`), the cut-move in
     `edit.rs:1159-1169`, and the sheet insert/delete/rename/move commands.
   - Nested commands (`data.subtotal`, insert copied cells) are covered automatically.
2. **A content patch**: the cells and features that differ between the workbook before and after
   the command, after the structural edits.
   - Per cell: the input or formula text and the full style (not the id), each optional, so a
     style-only change doesn't clobber someone's typing.
   - Per sheet: row/column info, merges, and (for the prototype) whole-field replacement of
     feature lists (conditional formats, validation, tables, charts…).
   - Workbook level: defined names and properties.
   - Cached values are not sent; each client recalculates.

This covers all ~340 commands without per-command work. Paste, fill, sort, remove duplicates,
flash fill and formatting are all just patches, however the local user's selection or clipboard
drove them. Only about a dozen structural operations need transformation rules.

View state (selection, active sheet, scroll position, frozen-pane view) stays per user and is
never part of an op. Today some of it lives in the `Workbook` (`active_sheet`, `view_*`), so the
diff excludes those fields and replay keeps the local values.

## 3. Ordering and convergence

**Server-ordered, client-transformed.** The server is a dumb, durable sequencer: it gives each
op the next sequence number and hands out the log. All transformation happens in the client
(Rust, deterministic), so every client computes the same result from the same log.

- Each op carries `base`: the last sequence number its author had applied.
- To apply op `n`, a client transforms it through ops `base+1 .. n-1` it didn't know about. The
  log is shared and the transform is deterministic, so all clients agree.
- **Local edits are optimistic.** The client keeps a confirmed state (`Arc<Workbook>` at the
  last server sequence) plus its pending ops, and shows confirmed state + pending.
- When remote ops arrive, it rolls back to confirmed (an `Arc` swap), applies the remote ops,
  then re-applies its pending ops transformed over them. When the server echoes a pending op,
  the op moves to confirmed.

**Transform rules (prototype):**

| Later op ↓ / earlier op → | Structural edit | Content patch |
|---|---|---|
| **Structural edit** | Shift positions (insert/insert at the same position: earlier sequence goes first; overlapping deletes shrink; an edit inside a deleted span vanishes) | No change |
| **Content patch** | Map every cell and range through the edit (`map_range`). Drop entries that were deleted. **Also adjust formula text** with `adjust::adjust`, so a formula typed during someone else's row insert still points at the right cells. | Last writer wins, per cell and per field (input vs style); per field for sheet features |

Known prototype limits:
- Two users inserting an object (chart, table) at the same moment get the same id. The later
  one gets a fresh id, assigned deterministically during transform.
- Feature lists are last-writer-wins as a whole field. Per-object ops come in phase 4.
- Sort is a patch: someone typing in a range while another user sorts it keeps their cell value
  at its old address, not the sorted one. Excel Online has the same class of quirk. Phase 4 can
  turn sort into a permutation op.

## 4. Undo in a shared workbook

Snapshot undo can't stay as it is: restoring a snapshot would undo other people's work too. In a
shared session:
- **Undo becomes an inverse op of your own last edit.** For a content patch that's the
  before-values of the cells you changed. It is transformed through everything since and sent
  as a new op. Redo works the same way.
- For a structural edit the inverse is the opposite edit (insert ↔ delete) plus a patch that
  restores deleted cells. The prototype allows structural undo only when nobody else has edited
  since; otherwise it says "Can't undo: others have edited since."
- Single-user sessions keep today's snapshot undo unchanged.

## 5. Where the code goes

| Piece | Location | Notes |
|---|---|---|
| Stable sheet ids | `crates/model` (`Sheet.id: u32`, assigned on load in sheet order, new sheets `max + 1`) | Serialised in JSON; not in XLSX (reassigned on load, which is deterministic). |
| `collab` module: `Op`, `StructEdit`, `Patch`, recorder, `make_patch(before, after)`, `apply`, `transform`, `Replica` (confirmed + pending + rebase + undo) | `crates/engine/src/collab/` (L5) | No I/O, wasm-safe, all native-testable. Refactor the structural commands' bodies into a reusable `apply_edit(&mut Workbook, sheet_id, &Edit)`. |
| Convergence fuzz test | `crates/engine/tests/collab_fuzz.rs` | N replicas, random commands (reusing `command_sequence_fuzz.rs`), random delivery interleavings. Assert identical workbooks after `canonical()`. The test that makes or breaks this. |
| Host bridge messages | `apps/gridcraft-web/src/host.rs` | `gridcraft:collab {clientId, user, color}` (from the page) starts a session. `gridcraft:ops` (to the page) carries local ops. `gridcraft:remote` (from the page) carries ordered ops. `gridcraft:presence` goes both ways. Networking stays outside GridCraft, as today. |
| Presence UI | `crates/ui-egui/src/grid.rs` (`paint_selection` :1599) | Reuse the formula-reference highlight painting (:1619-1637, `editor::REF_COLORS`) with a name tag, plus collaborator avatars in the title bar. |
| Sequencer, op log, presence, checkpoints | `gridcraft-nextcloud`: PHP controller + DB migration (`gridcraft_sessions`, `gridcraft_ops`, `gridcraft_members`) | `POST /session/join`, `POST /session/push {base, ops}`, `GET /session/sync?since=n`. Poll about every 500 ms while active and back off when idle (the approach Nextcloud Text uses). A WebSocket relay can replace polling later without touching GridCraft. |
| Saving | Client side | The server can't write .xlsx (no Rust there). One elected client, the longest-connected, writes a checkpoint every N ops or T seconds: `PUT` with `If-Match`, then records `(seq, etag)`. A joiner loads the checkpoint file and applies ops after its `seq`. If the file changes outside the session (ETag mismatch), the session ends and the newer file wins. |

## 6. Accepted differences from Excel Online

- Volatile functions (`RAND`, `NOW`) may show different values on different screens until the
  next checkpoint reload. Calculation is per client; Excel Online calculates on the server.
- About one second of latency with polling.
- Password-protection hashes are salted per client. Protecting or unprotecting a sheet is a
  last-writer-wins field.

## 7. Phases

| Phase | Scope | Done when | Estimate |
|---|---|---|---|
| **1. Engine foundations** | Sheet ids; structural-edit recorder and `apply_edit` refactor; `make_patch` / `apply`; transform rules; `Replica`; convergence fuzz | Fuzz: 3 replicas × 10k random commands converge; existing CI green | 2–2.5 weeks |
| **2. Transport** | Nextcloud sequencer endpoints and migration; bridge messages; join from checkpoint; checkpoint writer; reconnect (resend pending) | Two browsers on one file in your Nextcloud see each other's edits, formatting and row/column inserts within about 1 s, and the saved file matches | 1–1.5 weeks |
| **3. Presence and undo** | Others' selections with names and colours; collaborator list; collaborative undo/redo | Selections show live; undo never removes another person's edit | 1 week |
| **4. Hardening** | Per-object ops for charts, tables and CF; sort as a permutation; large-patch compression and caps; read-only participants; offline buffer; WebSocket transport; performance | | 4–8 weeks |

Prototype (phases 1–3): **about 4–5 weeks.** Close to Excel Online (phase 4): **2–3 months in total.**

## 8. Risks

- **Upstream merges get harder.** `collab` hooks into structural commands, and `Sheet` gains a
  field. Keep the hooks small: a recorder call at existing choke points.
- **Gaps the engine already has show up faster with more editors.** Conditional-format and
  validation formulas, pivot sources, freeze panes and print titles aren't adjusted on row
  insert today, even for one user (found during this review). Fix them in phase 1, since every
  client must agree.
- **Big edits** (paste or sort over 100k cells) make big patches. Cap them, compress them, and
  fall back to "reload from checkpoint" above a threshold.
- **Every client must agree on every edit.** Any non-deterministic step in `apply` breaks this
  (std `HashMap` order in engine code was flagged; calc already uses a deterministic hasher).
  The convergence fuzz test is the guard.
