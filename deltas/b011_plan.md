# B-011: sync progress and completion reporting

Status: owner-approved (2026-11-17); everything not
overridden there is implemented as written).

## 1. What this task is

B-011 turns the B-010 executor's coarse step machinery into the app-visible
progress and outcome contract the spec and plan.md already name. Scope:
`crates/ipod-sync` only — the data model, the emission, the unified result,
the last-sync semantics, and the tests that pin them. All UI consumption is
B-012..B-016's business; this task gives them the interface and proves it
headless.

Spec reference: R-04 (S-09 Progress, S-10 Completion, S-12 Error handling),
R-05 (S-13 Bidirectional Statistics). Plan reference: Phase 2, Data Models
& Interfaces — the paragraph that defines `SyncProgress` ("current item,
total items, operation phase, and completed work") and `SyncResult`
("distinguishes complete, partial, and failed outcomes, includes
changes/errors, and references pending recovery work"). Task verification:
Rufin emits work/phase progress, including file-copy work; complete,
partial, and failed results are distinct; only a fully completed device and
host sync updates Last Sync; interrupted and partial results retain
actionable recovery information.

## 2. What already exists (measured, not guessed)

- `exec.rs` runs the §5 protocol as six journal steps (`Intent`, `Backup`,
  `HostWrite`, `FileOps`, `DbWrite`, `Verify` — the `STEPS` const) and
  returns `Result<SyncOutcome, SyncError>`. `SyncOutcome` carries the
  counts and diagnostics; `SyncStatus` has only `Completed` and
  `Partial { failed_step, diagnostic }` (the restore arm). Everything else
  fails as `Err(SyncError { step, diagnostic })`.
- The journal already persists the Last Sync source: `last-success.json`
  (`LastSuccess { device, plan_hash, completed_unix }`), written only by
  `Journal::complete` — so S-10's "only full completion advances last
  sync" is already enforced by construction; `load_last_success` is public
  but nothing calls it yet.
- `failed.json` (the once-kept failed record) and the active
  `operation.json` (resumption points via `completed_steps`) exist and are
  the raw material for the recovery reference.
- b010_plan §7 promised "coarse `SyncEvent` phases exposed for B-011" —
  that type never landed; the actual seam is the `STEPS` names and the
  journal. B-011 defines the real event model on top of them (the wording
  gets corrected in §11).
- Precedent for the pull-side UI pattern: rufin-core's
  `MetadataLookupProgress` (a `Mutex`-guarded snapshot plus updates); the
  executor-side precedent is the B-010 provider closures. `async-channel`
  is already a workspace dependency (gtk-library uses it for the B-006
  observer stream).
- The B-006 debt note (notices queue behind multi-second libgpod parses)
  constrains the design: the sync must run on its own thread and the event
  stream must be non-blocking for the emitter (§4, §10-D8).

## 3. The data model (new `sync/report.rs`)

```rust
pub enum ProgressEvent {
    /// A protocol-step boundary. `began` false = the step committed
    /// (journal write done), true = about to start.
    Step { name: &'static str, index: u8, total: u8, began: bool },
    /// One work item inside the current step. `total == 0` marks an
    /// indeterminate phase (device parses, the staged write) — the UI
    /// shows a pulse, not a fraction.
    Item { current: u32, total: u32, label: String },
    /// A diagnostic produced mid-run (mirrors the outcome's diagnostics).
    Notice(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncProgress {
    pub step: Option<&'static str>,
    pub step_index: u8,
    pub step_total: u8,
    pub item_current: u32,
    pub item_total: u32,
    pub item_label: Option<String>,
    pub completed_items: u32,
}
impl SyncProgress {
    /// Pure fold: applies an event, preserving the invariants below.
    pub fn fold(&mut self, event: &ProgressEvent);
}
```

Fold invariants (unit-tested, §7 step 1): `step_index` only advances;
`item_current`/`item_total` never decrease within a step; a `Step { began:
true }` resets the item counters; indeterminate items (`total == 0`) don't
touch the counters, only the label.

```rust
pub enum RecoveryState {
    /// An interrupted operation's record is active; the next sync resumes
    /// from these journal steps.
    Resumable { completed_steps: Vec<&'static str> },
    /// The consistency set was restored from the backup; the interrupted
    /// operation is recorded failed.
    Restored,
    /// A failed operation's record is retained for diagnostics.
    FailedRecord,
    /// The journal itself was unreadable at reporting time.
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Completed,
    Partial { failed_step: &'static str, diagnostic: String },
    Failed { step: &'static str, diagnostic: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncChanges {
    pub host_patches_written: usize,
    pub device_patches_written: usize,
    pub files_added: usize,
    pub files_removed: usize,
    pub playlists_written: usize,
    pub artwork_added: usize,
}

pub struct SyncResult {
    pub status: SyncStatus,
    pub changes: SyncChanges,
    pub diagnostics: Vec<String>,
    /// Pending recovery work; `None` when the device is clean.
    pub recovery: Option<RecoveryState>,
    /// The last-success marker as of return time — fresh after
    /// `Completed`, the previous marker otherwise (never `None` masking a
    /// prior completion when the journal is readable).
    pub last_success: Option<LastSuccess>,
}
```

## 4. The unified return (decision D-1)

`run_sync` returns `SyncResult` directly — the `Result<SyncOutcome,
SyncError>` shape dies. Rationale: plan.md names `SyncResult` as the single
model distinguishing the three outcomes; the UI switches on one type; the
half-measure (keep `Err` and assemble at the app layer) leaves the failure
path without its recovery reference. `SyncOutcome` becomes a private
accumulator inside `exec.rs`; `SyncError` folds into `SyncStatus::Failed`.
Every return site maps like this (the table is the task's contract):

| Executor site today | B-011 result |
| --- | --- |
| exclusion-guard refusal | `Failed { step: Intent }`, recovery from the journal if readable, else `None` |
| journal unreadable | `Failed { step: Intent }`, recovery `Unavailable` |
| restore arm | `Partial { failed_step: Intent }`, recovery `Restored` |
| verify-and-advance arm | `Completed` + "recovered" diagnostic, recovery `None` (the operation finished) |
| step failure / cancellation | `Failed { step }`, recovery `Resumable { completed_steps }` when a record is active |
| completion | `Completed`, recovery `None`, `last_success` = the fresh marker |

`last_success` is read via `Journal::load_last_success` at return time (it
costs one small read); after `Completed` it is the marker `complete()` just
wrote; after any failure it is the previously persisted marker. Partial and
failed runs never write it — the existing `complete()`-only writer keeps
S-10 true by construction; B-011 only *surfaces* it.

## 5. Emission points

The executor gains `reporter: &'a dyn Fn(ProgressEvent)` on `SyncContext`
(required, `Send + Sync`; the existing test helper passes `&|_| {}`). An
internal `Progress` wrapper (`phase()`, `item()`, `notice()`) keeps the
call sites one-liners. The emission schedule:

- **Recovery arms**: `Notice("resuming an interrupted operation (N/6 steps
  completed)")` and step events begin at `index = N + 1` — no replay of
  already-committed steps, the bar is monotonic (D-4). The restore arm
  emits the `Intent` step + one indeterminate item ("restoring the
  consistency set…") before returning `Partial`. Verify-and-advance emits
  the `Verify` step and the verify item, then returns `Completed`.
- **Intent**: `Step { Intent, 1, 6, began }`; the revalidation parse emits
  an indeterminate item ("re-checking the device…"); the plan-hash
  mismatch and space-check refusals add `Notice`s mirroring their
  diagnostics.
- **Backup**: `Step { Backup, 2, 6 }` + one indeterminate item ("backing
  up the device database…") — the ithmb folder can hold hundreds of
  thumbnails, per-file events would be spam (D-2).
- **HostWrite**: `Step { HostWrite, 3, 6 }`; the merge parse emits an
  indeterminate item ("reading device playback counts…"); the host write
  is one in-process itl-sync call, so exactly one item
  ("writing N stat patches to the library") with `total: 1`.
- **FileOps**: `Step { FileOps, 4, 6 }`. The phase total is computed once
  before the loop: `file_adds + file_repairs + file_removes +
  device_patches (from the HostWrite merge) + playlist_creates +
  playlist_updates + playlist_removes + artwork_adds`. Every iteration
  emits its item **before** doing the work (the UI shows the in-flight
  title): adds ("Adding <title>" / "Converting <title>" when the
  transcode rule fires), repairs ("Restoring <title>"), removes ("Removing
  <title>"), stat patches ("Syncing stats"), playlist ops (name or device
  id), artwork ("Adding artwork"). Skipped iterations — PDF booklets
  (NeverCopy), the Genius exclusion, deferred smart creates/criteria
  updates, missing artwork — still emit their item and a `Notice`, so the
  bar never stalls and the skip is visible (D-5).
- **DbWrite**: `Step { DbWrite, 5, 6 }` + indeterminate items for the
  staged serialize ("writing the device database…") and the staged-parse
  verification ("verifying the written database…").
- **Verify**: `Step { Verify, 6, 6 }` + one indeterminate item
  ("re-checking the device…", the post-write parse + artwork proof).
- Diagnostics are emitted as `Notice`s at the point they are produced, so
  the stream and the final `SyncResult.diagnostics` agree.

Failure of an item aborts the run exactly as today — the terminal state is
the returned `SyncResult`, the bar's last position is just history.

## 6. Granularity decisions (D-2, D-3)

**Per work item, never per byte.** `DeviceSession::copy_track` is libgpod's
own copy loop (wrapper `write.rs`) with no progress callback; an
app-owned copy routine would have to replicate libgpod's finalization
(FType handling on the pod) — same out-of-proportion class as the rejected
mhbd patch in b010_plan §11-11. The transcode runs inside the caller's
provider closure, opaque to the executor: one item per add, labeled
"Converting…" when the rule fires. Per-file granularity is also what
iTunes itself shows for transfers. Byte-level transcode progress would
need a provider-contract change — noted, not this task.

**Indeterminate phases are explicit.** Device parses (revalidation, merge
read, write-session open, staged verification, post-write proof) can take
seconds on large libraries; they emit `Item { total: 0, label }`. This is
the B-006-debt answer at the engine level: the UI gets an honest "parsing…"
state instead of a frozen bar, and because the sync runs on its own thread
(D-8) the UI thread is never the one blocked.

## 7. The pipe (D-7, D-8)

```rust
/// Bounded event pipe: `sender`'s reporter closure is `try_send`
/// (drop-on-full), so a slow UI can never block or fail the sync.
pub struct ProgressPipe { sender: async_channel::Sender<ProgressEvent> }
impl ProgressPipe {
    pub fn new() -> (Self, async_channel::Receiver<ProgressEvent>); // cap 128
    pub fn reporter(&self) -> impl Fn(ProgressEvent) + '_ + Send + Sync;
}
```

The channel is **advisory**: events may drop under pressure; the returned
`SyncResult` is the authoritative terminal state, and the UI (B-016) must
render the final state from the result, never from the last event. The
pipe ships in `ipod-sync` (async-channel is already a workspace dep) so
the end-to-end ordering test in §8 exists now; B-016 only subscribes. The
worker-thread wiring (`run_sync` on a dedicated thread, `reporter` moved
into the `SyncContext`) is B-015/B-016's seam — B-011 proves the reporter
is `Send + Sync` and non-blocking via the lossy-pipe test.

## 8. Verification: gates

All gates env-gated, disposable corpora, never the live `.itl` or a real
pod — the B-010 battery's env (`ITL_TEST_PATH`, `ITL_TEST_MEDIA_ROOT`,
`IPOD_TEST_TREE`, `RUFIN_LIBGPOD_DIR`).

**Step 1 — unit (`report.rs`):** fold invariants (monotone step index;
counters reset on a new step; indeterminate items leave counters alone);
`SyncProgress` clone/eq/default; `SyncResult` assembly.

**Step 2 — executor emission (`tests/b011_report.rs`):**
1. *Pair gate with a recording reporter*: the step sequence matches
   `STEPS` (one `began: true` + one `began: false` per step, in order);
   every `Item` has `current ≤ total`; the FileOps item count equals the
   phase total computed from the plan + merge; the terminal event is the
   returned `Completed` result whose `changes` match the B-010-pinned
   counts (adds minus booklets, 3373 + adds tracks, journal completed);
   `last_success` is `Some` with `completed_unix ≈ now`.
2. *Failure battery with a recording reporter*: for each injected step,
   the result is `Failed { step: injected }` with `RecoveryState::Resumable`
   holding exactly the steps committed before the injection — the emitted
   step events must match the journal's `completed_steps` one-for-one.
   The `STEP_VERIFY` injection still verify-advances to `Completed`
   (existing semantics, now with the recovery `None`).
3. *Host refusal*: `Failed { step: HostWrite }`, `Resumable { [Intent,
   Backup] }`, `last_success` = a pre-seeded old marker, byte-unchanged.
4. *Resume*: after a crash post-FileOps, the re-run's first event is the
   resume `Notice`, step events begin at index 4; the run completes and
   advances last-success.
5. *Cancellation*: `cancelled()` true at the FileOps checkpoint →
   `Failed` with `Resumable`, pending store untouched, last-success
   unchanged.
6. *Restore arm*: a corrupted post-DbWrite state (or the injected
   equivalent) → `Partial` with `RecoveryState::Restored`, last-success
   unchanged.
7. *Lossy pipe*: capacity-1 pipe with a never-read receiver — the run
   completes, the result is authoritative, the writer never blocked.
8. *Last Sync invariant sweep*: only `Completed` (including
   verify-and-advance) writes a fresh marker; `Partial`/`Failed` leave the
   file byte-identical.

**Step 3 — regression:** the full existing suite (ipod-sync 110 unit +
pair gate + failure battery + dry-run gate, audio-processing, wrapper) is
green after the signature change; `clippy` and `cargo fmt --check` zero;
`git diff --check` clean. The b010_sync.rs migration is mechanical: the
`expect_err`/`status` sites become `SyncResult` matches.

## 9. Scope boundaries (not this task)

- Any UI: the page (B-012), the five-state views (B-013), the sync button
  and its worker thread (B-015), the UI subscription to the pipe (B-016).
- Rewiring `ipod_connection.rs`: its `OperationNotice` placeholder ("the
  recovery/stat-merge model of B-010 replaces this type") is replaced by
  `RecoveryState` here at the *model* level; the connection-service field
  and its surfacing stay B-013/B-016's verification lines.
- Byte-level copy progress and transcode sub-progress (noted in §6).
- A sync-history archive: the journal keeps its last-success marker and
  single failed record; history remains dead weight (b010_plan §7).

## 10. Decisions for the owner

D-1 (unified return) — `run_sync` returns `SyncResult`, the
`Result`-shaped API dies, tests migrate.

D-2 (granularity) — per work item

D-3 (indeterminate phases) — parses and the staged write emit
`total: 0` items with labels instead of silence.

D-4 (resume bar) — no replay of committed steps; the bar starts at the
first not-done step with a resume notice.

D-5 (skipped items) — booklet/Genius/deferred-smart skips count toward
the phase total and emit a `Notice`.

D-6 (cancellation) — maps to `Failed` with the resumable journal (the
spec's three-way has no fourth state); a UI "Cancelled" chip, if wanted,
is derived in B-016 from the diagnostic, not a new status.

D-7 (advisory channel) — `try_send`/drop-on-full, cap 128; the returned
`SyncResult` is the truth, the UI renders finals from it.

D-8 (pipe location) — `ProgressPipe` ships in ipod-sync now (async-channel
is already a workspace dep); UI subscription is B-016.

## 11. Documentation follow-ups (done with the task)

- b010_plan §7: correct the "coarse `SyncEvent` phases" wording — the seam
  B-011 built on was the `STEPS` names and the journal; the event model
  now lives in `report.rs`.
- plan.md Data Models & Interfaces: done-marker for the `SyncProgress` /
  `SyncResult` paragraph (the "designed in B-008/B-010, not part of B-002"
  sentence gains the implemented-here reference).
- tasks.md: the B-011 Result line at checkoff, per convention.

## 12. Implementation steps

1. `report.rs`: the types, the fold, `RecoveryState`; unit tests (§8 step 1).
2. `exec.rs` signature change: `SyncResult` return, private accumulator,
   the §4 mapping table at every return site; migrate `b010_sync.rs`.
3. Reporter plumbing: `SyncContext.reporter`, the `Progress` wrapper,
   test-helper default; emission per §5 (recovery arms first, then the
   six steps and their items).
4. `ProgressPipe` + the lossy-pipe test.
5. The gates of §8 steps 2–3; fixes as they force.
6. Documentation (§11) and the final full-battery run.

## 13. Implementation results

(filled on completion — date, what each step produced, gate outcomes,
gate-forced fixes)
