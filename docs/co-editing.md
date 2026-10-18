# Real-time co-editing: design

Status: phases 1–4 done (engine, transport through Nextcloud, presence and collaborative undo,
hardening), 2026-10-17; sessions keep to one co-editing protocol (`collab::PROTOCOL`), 2026-10-18.
Fork-specific (Aegean Office). Not upstream behaviour.

## Where it stands

Everyone with a file open in Nextcloud edits it live:
- **Edits:** typing, formatting and row inserts appear for the others within about a second.
- **Presence:** each person's selection shows on the grid in their colour with their name, and
  a strip above the editor lists who's editing.
- **Undo and Redo** take back a person's own edits only, never someone else's later change.
- **Saving:** the shared workbook is saved back to the file every 20 edits or 30 seconds, and on
  Save, even when the saving member's tab is in the background.

Phase 4 pieces:
- **Objects merge one by one.** List and map settings travel as item changes (`Field::items`,
  `patch::merge_by`):
  - charts, images, shapes, tables, slicers and pivots by id
  - conditional formats, validations, merges, sparklines, names and similar by value
  - comments and hyperlinks by cell

  Two people adding a chart, a rule or a note at once both keep theirs. Items shift with
  structural edits like whole settings, and conflict only when they're the same item.
- **Collision-free object ids.** Objects made in a shared session get ids namespaced by client
  (`client << 16 | n`), as sheets do (`Replica::retag_new_objects`).
- **Read-only participants.** `Session::collab_set_read_only`: an edit is taken back with a
  message, and nothing is sent. The page passes `readOnly` from the file's permissions; only
  members who can edit write checkpoints (`gridcraft_members.can_edit`).
- **Large edits.** Each style travels once per patch (`Patch::styles`), and identical
  neighbouring cell changes travel as rectangles (`CellChange::to`). Bolding 50,000 cells is
  under 4 KB, down from 1.9 MB. An op over 8 MB (`MAX_SHARED_OP_BYTES`) is refused at the source
  with a message. A patch claiming more than 5 million cells is refused when applied
  (`MAX_PATCH_CELLS`). The page treats a push the server refuses outright as a reason to rejoin
  rather than retry.
- **Faster updates.** A remote op is applied straight to what's shown, recalculating only what
  it touches, instead of rebuilding from the confirmed copy. A copy that took this shortcut while
  its own edits were pending rebuilds once they're confirmed. 100 remote edits on 40,000
  formulas take about 2 ms, against 4.7 s before.

Phase 3 pieces:
- **Collaborative undo.** In `collab::Replica`, each local edit's inverse is computed with
  `capture` over the inverted structural edits. It is rebased over later remote ops (theirs win
  conflicts) and over later own ops, and undoing sends it like any edit. The command enablers
  `can_undo`/`can_redo` follow the shared stack.
- **Presence.** `SheetApp::presence` (`ui-egui`), painted under the own selection by
  `grid::paint_presence`. Positions travel `app → page → server (members.selection) → others`
  with each poll; colours are assigned by the page per client id.
- **Background tabs.** The web shell shares the app with the host bridge, which handles
  co-editing messages as they arrive. Hidden tabs keep up and answer checkpoint requests.

Phase 2 pieces:
- **Session hook.** `Session::collab_*` (`crates/engine/src/collab/mod.rs`): every command path
  (ribbon, keys, drags, agents) is shared through `Session::execute`, which captures the
  outermost command as one op.
- **Bridge messages.** `apps/gridcraft-web/src/host.rs` and `collab_msg.rs`; the protocol is in
  `packaging/web/README.md` § Co-editing.
- **Sequencer.** In `gridcraft-nextcloud`: `CollabService`, which gives gap-free log positions
  under a row lock and recognises a retried push, plus `CollabController` and a migration.
- **Page loop.** `gridcraft-nextcloud/src/collab.js`:
  - joins pinned to the file's ETag
  - sends ops in order, with retries
  - polls for others' entries
  - the member with the lowest client id writes the checkpoints, and the server keeps the
    checkpoint's sheet ids for joiners
- **Shared links.** Anyone with a link to the file (`/s/{token}`), or to a folder holding it,
  joins the same session as a guest. They use the link's own WebDAV address and `/apps/gridcraft/s/{token}/collab/*`.
  Membership is proved with a random key handed out on join, and edits need an edit link.
- **The desktop app.** A workbook opened from Nextcloud in GridLogic on a computer
  (`apps/gridcraft/src/nextcloud/coedit.rs`) joins the same session. It signs in with its app
  password and calls the same `/apps/gridcraft/collab/*` routes; `OCS-APIRequest` takes it through
  Nextcloud's CSRF check. A worker thread does what `collab.js` does: pushes ops in order, polls,
  and writes checkpoints. While shared, Save and AutoSave write checkpoints; they don't send the
  local copy. The engine shares one workbook at a time, so a second one from Nextcloud opens
  unshared. "Open in Desktop App" in the browser (`gridlogic://open?server=…&path=…`) opens the
  file in the desktop app, in the same session.

Engine pieces from phase 1:

| Piece | Code | Proof |
|---|---|---|
| Stable sheet ids | `Sheet::id`, `Workbook::ensure_sheet_ids`; kept with each checkpoint and adopted on join (`Session::collab_adopt_sheet_ids`), since XLSX loses them | `tests/sheet_ids.rs`; `tests/collab_scenarios.rs` (joining from a checkpoint after a sheet was added in front) |
| Content patches | `collab::patch` (`make_patch`, `apply_patch`) | Every `Sheet`/`Workbook` field is replicated or listed as local (unit test); 47 commands replicate exactly (`tests/collab_patch.rs`) |
| Structural edits | `Ctx::record`, `cmd::sheet::apply_shift` (shared with the commands), `collab::replay` | Structural ops carry the shift, not the shifted cells; hostile ops are refused |
| Transform | `collab::rebase` | Every pair of row/column inserts and deletes commutes (exhaustive unit test) |
| Replica | `collab::Replica` (`run`, `outgoing`, `receive`) | `tests/collab_fuzz.rs`: 2,000 random 3-person sessions × 200 steps (149k edits, 42k structural ops) all converge; `tests/collab_scenarios.rs`: merges match intent |

Decisions made while building it:

- **Server-ordered, client-transformed, one op in flight per client.** An op's base is always
  past its author's previous op, so log op `n` is rebased over `base+1 .. n-1` (none its
  author's) by every copy alike: convergence needs only that `rebase` is deterministic.
- **The author confirms its own op the way everyone else does**, recomputing it from the sent
  form, not trusting its locally rebased copy.
- **Sheet deletions are explicit** (`Patch::removed`); an `order` lists the sheets the editor
  knew, and sheets added meanwhile keep their place after them.
- **Sheets made in a shared session get ids namespaced by client** (`client << 32 | n`), so two
  people adding a sheet at once can't collide.
- **Undo in a shared session is each person's own** (phase 3): snapshot undo would take back others' edits.
- **A copy missing log history must rejoin** rather than guess: placing an op without the
  entries its author hadn't seen would diverge. The server must give a joining copy the log
  from its checkpoint on.
- **A session keeps to one co-editing protocol** (`collab::PROTOCOL`, raised whenever an op
  carries something an older build would misread or drop). Every copy joins with it (the browser's
  from `gridcraft:ready`'s `collab`, the desktop app its own), never with the app version, which
  the desktop app doesn't share. The Nextcloud app keeps the newest per session and answers a
  member on an older one `{rejoin, outdated}` (gridcraft-nextcloud 0.9.0): a browser tab reloads,
  which brings the newer editor; a page that can't get newer that way says so after one reload a
  minute, and the desktop app opens the workbook unshared, or stops sharing, with a message.
  Without this a tab left open across an upgrade misreads newer ops and can write a checkpoint
  that loses the others' edits (seen in SlideLogic).
- **Received ops apply incrementally** (phase 4). A full rebuild happens only after a shortcut
  taken while this person's edits were pending.

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

Remaining limits:
- **Sort is a patch.** Someone typing in a range while another user sorts it keeps their cell
  value at its old address, not the sorted one. Excel Online has the same class of quirk. A
  permutation op would fix it.
- **A changed item replaces the item.** Two people changing the same chart (or the same
  conditional-format rule) at once: the later change wins whole, not property by property.
- **Object ids namespace the client id modulo 2¹⁶.** Two clients of one file whose ids differ by
  a multiple of 65,536 could pick the same id for a new object; client ids grow by one per join.

## 4. Undo in a shared workbook

Snapshot undo can't stay as it is: restoring a snapshot would undo other people's work too. In a
shared session:
- **Undo becomes an inverse op of your own last edit.** For a content patch that's the
  before-values of the cells you changed. It is transformed through everything since and sent
  as a new op. Redo works the same way.
- For a structural edit the inverse is the opposite edit (insert ↔ delete) plus a patch that
  restores deleted cells. It's rebased like any other step, so undoing a row insert after
  someone typed below it moves their text back up with its row (`collab_scenarios.rs`).
- Single-user sessions keep today's snapshot undo unchanged.

## 5. Where the code goes

| Piece | Location | Notes |
|---|---|---|
| Stable sheet ids | `crates/model` (`Sheet.id: u64`, assigned on load in sheet order; sheets made in a session are namespaced by client) | Serialised in JSON; not in XLSX. Load order alone isn't enough: a sheet made in a session and moved before older ones would shift theirs. So each checkpoint hands its ids to the server (`checkpoint_sheets`), and joiners adopt them before starting. |
| `collab` module: `Op`, `StructEdit`, `Patch`, recorder, `make_patch(before, after)`, `apply`, `transform`, `Replica` (confirmed + pending + rebase + undo) | `crates/engine/src/collab/` (L5) | No I/O, wasm-safe, all native-testable. Refactor the structural commands' bodies into a reusable `apply_edit(&mut Workbook, sheet_id, &Edit)`. |
| Convergence fuzz test | `crates/engine/tests/collab_fuzz.rs` | N replicas, random commands (reusing `command_sequence_fuzz.rs`), random delivery interleavings. Assert identical workbooks after `canonical()`. The test that makes or breaks this. |
| Host bridge messages | `apps/gridcraft-web/src/host.rs` | `gridcraft:collab {clientId, user, color}` (from the page) starts a session. `gridcraft:ops` (to the page) carries local ops. `gridcraft:remote` (from the page) carries ordered ops. `gridcraft:presence` goes both ways. Networking stays outside GridLogic, as today. |
| Presence UI | `crates/ui-egui/src/grid.rs` (`paint_selection` :1599) | Reuse the formula-reference highlight painting (:1619-1637, `editor::REF_COLORS`) with a name tag, plus collaborator avatars in the title bar. |
| Sequencer, op log, presence, checkpoints | `gridcraft-nextcloud`: PHP controller + DB migration (`gridcraft_sessions`, `gridcraft_ops`, `gridcraft_members`) | `POST /session/join`, `POST /session/push {base, ops}`, `GET /session/sync?since=n`. Poll about every 500 ms while active and back off when idle (the approach Nextcloud Text uses). A WebSocket relay can replace polling later without touching GridLogic. |
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
