# B-015: Sync button with Dry Run checkbox

Status: owner-approved (2026-11-17); D-1/D-2 amended by the owner —
Dry Run is dev-only (the S-08 delta, `deltas/b015_delta.md`), release
builds run silently.

## 1. What this task is

B-015 turns the greyed Sync placeholder (B-014's documented takeover
point) into the real sync initiation: the enabled Sync button, the
dev-build Dry Run option and review step, and the worker-thread sync
service that runs the engine. Spec reference: R-04 (Sync Operation),
S-07 (Initiation), S-08 (Dry Run — MODIFIED by the owner, see
`deltas/b015_delta.md`), S-11 (State constraints). Plan reference:
Phase 3. Task verification (amended with the delta): sync is disabled
outside Ready, readiness is rechecked before writes, and Dry Run is
available and nonmutating **in dev builds** — release builds omit the
option entirely.

The engine already exists end to end (B-008..B-011: `compute`,
`collect_host`/`collect_device`, `run_sync` with the journal, the
progress pipe, the recovery protocol). B-015 is the **boundary**: the
UI that gates and triggers it, and the production wiring from the
iTunes source into `SyncContext`. The engine's own comment names this
exactly — `HostWriteContext` is "supplied by the sync boundary, B-015;
never discovered by the engine itself".

Scope is initiation + dry run + the review/approval step. Live
progress rendering, the results panel, the Last Sync timestamp, the
recovery notices, and removal-during-sync handling are B-016. Nothing
here invents engine behavior; a real (non-dry) run with the approved
plan is the B-010 engine on disposable corpora and behind the
existing gates — no hardware writes are claimed by this task.

## 2. What already exists (measured, not guessed)

- **UI seam**: the greyed `sync_button` in the Ready bottom bar
  (`ipod_page.ui`, marked "B-015's takeover point"), bound in
  `composite_box!`. The Параметры box stays greyed furniture — B-015
  adds the *real* option (Dry Run), but the demo's five checkboxes
  remain furniture.
- **Engine surface** (`ipod-sync`):
  - `dry_run::compute(host, device, opts) -> DryRunReport` — pure, no
    I/O, structurally nonmutating (b009_plan; proven by tree hashes);
  - `dry_run::snapshot::collect_host(&ItlFile, media_root, &dyn
    ArtworkProbe) -> HostInputs` and `collect_device(session,
    ...) -> DeviceInputs` — read-side only;
  - `sync::exec::run_sync(&SyncContext) -> SyncResult` with the
    B-011 unified result (`Completed`/`Partial`/`Failed`,
    `SyncChanges`, diagnostics, `RecoveryState`, `LastSuccess`);
  - `SyncContext` is fully caller-supplied: `mount`, `backend`,
    `host: &HostInputs`, `options: &SyncOptions`, `host_write:
    &HostWriteContext`, `device_identity`, `plan: &ExecutionPlan`
    ("the approved plan — the dry run the user reviewed (hash +
    lists)"), `transcode`, `artwork`, `cancelled`, `fail_after`,
    `reporter`;
  - `sync::host::HostWriteContext { library_path, media_root,
    tz_offset_seconds }` — one `itl_sync::write_library` call per
    operation, sentinel-guarded in-process (import-first ordering);
  - `ExecutionPlan::build(&DryRunReport, StatMerge)` + `plan_hash`
    (the plan the run verifies);
  - `ProgressPipe` (b011_plan §5) for the event stream; `preflight
    (backend, mount)` for the readiness recheck.
- **Host side**: `crates/sources/src/itunes` — `ItunesSourceConfig {
  library, media_folder, tz_offset_seconds }` and the cheap
  `library_busy()` sentinel pre-check (the authoritative refusal
  lives inside itl-sync on every write). The B-010a provider is
  `audio_processing::transcode_to_m4a`. The pair gate
  (`ipod-sync/tests/b010_sync.rs`) is the reference wiring of all of
  the above with a real backend on a disposable tree.
- **Connection side**: `IpodConnection` is a serialized worker
  (multi-second parses) — the b006 FIFO latency debt. The snapshot
  subscription pattern (B-006) is the template for the new service.
- The b008 debt: "sync-source selection is a UI step (future work),
  not covered by spec.md" — B-015 does not build the selector; it
  defines the single-source rule (D-4).
- i18n msgid/tr + `tr_with`; pot regen. Widget construction is
  untestable — the pure-layer policy stands.

## 3. The design

### 3.1 The gating layer (pure)

A display-free function deciding the sync controls from one snapshot:

```rust
pub struct SyncControls {
    /// The Sync button is enabled: Ready + a supported model (the
    /// b006 policy verdicts, decision 12 — unsupported/unresolved
    /// models stay sync-disabled) + not already running.
    pub sync_enabled: bool,
    /// The reason msgid when the button is disabled and the verdict
    /// is the cause (S-11's "a message should indicate…").
    pub gating_note: Option<&'static str>,
    /// A sync run is in flight (button insensitive, checkbox locked).
    pub running: bool,
}

pub fn sync_controls(snapshot: &ConnectionSnapshot, running: bool)
    -> SyncControls;
```

`sync_enabled` ⇔ `state == Ready && device.model_supported &&
!running`. `gating_note` reuses the D-8 warning copy (already on the
page) — no new message surface; the disabled button plus the existing
warning line is S-11's indication. `checksum_possible` does **not**
disable the button: it limits only the write path, and the engine/
preflight refuse there with their own diagnostics (an unresolved
checksum still permits a dry run — honest and useful).

### 3.2 The sync service (worker thread, display-free core)

A new `IpodSyncService` in `gtk-library` (module `ipod_sync_service.rs`),
mirroring `IpodConnection`'s shape — cloneable facade, snapshot-style
state, observer channel — but with a **per-run thread**, not a
serialized worker: a sync runs for minutes and the connection worker
must stay responsive (the b006 FIFO debt; scans/notices must not queue
behind it).

```rust
pub enum SyncPhase {
    Idle,
    Collecting,        // .itl read + host/device collection
    Review { summary: ReviewSummary },   // dry run shown, awaiting decision
    Running,           // approved run in the worker thread
    Done(SyncOutcomeSummary),
}

pub struct SyncState {
    pub phase: SyncPhase,
    /// The latest ProgressEvents (bounded ring) — B-016 renders; B-015
    /// stores them so the seam is proven.
    pub events: Vec<ProgressEvent>,
}
```

Public actions: `start(dry_run: bool)` (no-op unless
`sync_controls` allows), `approve()` (run the reviewed plan),
`dismiss()` (back to Idle). All transitions run in a pure
`advance(state, event) -> SyncState` the gates drive; the thread
boundary is the thin side.

The engine seam inside the service is an injectable runner so the
gates never touch a backend:

```rust
/// The production runner: collect from the boundary, compute, run.
/// Tests inject stubs; the real one lives behind `Boundary`.
pub type RunFn = Box<dyn Fn(RunRequest) -> SyncResult + Send + Sync>;
```

### 3.3 The boundary (production wiring, shell-side)

`gtk-library` defines the trait; the shell implements it over the
iTunes source — gtk-library cannot see `sources` (layer inversion):

```rust
/// Everything the engine needs from the selected iTunes source —
/// supplied by the shell (B-015 boundary), matching `SyncContext`.
pub trait SyncBoundary: Send + Sync {
    fn library(&self) -> PathBuf;      // ItunesSourceConfig.library
    fn media_root(&self) -> PathBuf;   // config.media_folder or parent-of-.itl
    fn tz_offset(&self) -> Option<i32>;// config.tz_offset_seconds
    fn artwork(&self, pid: u64) -> Option<ipod::ArtworkInput>;
    fn transcode(&self, from: &Path, to: &Path) -> std::io::Result<()>;
}
```

`ipod_route_view` gains the service parameter (like `connection`),
constructed by the shell: `IpodSyncService::new(boundary, runner)`.
The shell's implementation reads `ItunesSourceConfig` from the active
iTunes source, reuses the sources-crate artwork layer for
`ArtworkInput`, and `audio_processing::transcode_to_m4a` for the
transcode. `device_identity` = the device name + mount (the b011
gate-forced per-test identity key); `cancelled = |_| false` until
B-016 adds the cancel button; `fail_after = |_| false` (test-only
fault injection).

The runner: open the `.itl` (`itl_core::ItlFile`) on the run thread,
`collect_host`, open the device session, `collect_device`,
`compute` → `ExecutionPlan::build` → (dry run: stop, the report is
the result; real: `run_sync`). The readiness recheck (S-08/S-11
verification): `preflight(backend, mount)` runs **before** any real
run, and a refusal becomes a `Failed` outcome with the diagnostic —
the FSM state stays whatever it is (the recheck gates the sync, not
the page). The sentinel pre-check (`library_busy()`) fires an early
refusal with the itl-sync wording; the authoritative check remains
inside the engine's host-write step.

### 3.4 The UI: bottom bar + the dev review dialog

1. **Dry Run checkbox** (`dry_run_check`, `GtkCheckButton`) — **dev
   builds only** (`cfg!(debug_assertions)`; the delta's S-08), in the
   bottom bar between the storage bar and the Sync button (the
   Параметры box keeps its furniture — D-1). Release builds compile
   the checkbox and the review dialog out entirely.
2. **Sync button**: `apply_snapshot` sets its sensitivity from
   `sync_controls.sync_enabled` (and the checkbox, where compiled,
   from `!running`).
3. **The flow** (D-2): in dev builds every Sync click computes first
   (collect + `compute`; pure, seconds on the real library) and shows
   the **review dialog**: the change summary (files
   added/removed/repaired, playlists created/updated/removed,
   capacity delta, stat counts, art adds/removes) with "Sync" and
   "Close". With Dry Run checked, "Sync" is absent — the dialog is
   the deliverable, Close discards (S-08: shown, nothing changed).
   Unchecked, "Sync" approves the reviewed plan and starts the run.
   In release builds the click **is** the approval: the plan computed
   at that moment runs immediately, no dialog — and the engine still
   re-verifies the plan hash against the device before any write.
   Dismissing a dev review discards the plan; the next click
   recomputes.
4. **Running**: the button and checkbox lock (`running`), the page
   shows a minimal in-flight indicator (spinner in the bottom bar).
   The outcome surfaces in a simple result dialog from
   `SyncOutcomeSummary` (status, counts, diagnostics) — the rich
   panel/progress bar/Last Sync/recovery notices are B-016.

The dialog contents come from pure fns:
`review_summary(&DryRunReport) -> ReviewSummary` and
`outcome_summary(&SyncResult) -> SyncOutcomeSummary` — the widget
layer only renders them. The service and the engine path are
**identical across builds**; only the dialog surface differs, so no
second sync code path exists behind the `cfg`.

### 3.5 Copy (proposed, translatable)

- Checkbox: "Dry Run" (tooltip: "Show what the sync would change
  without writing to the iPod or the library.").
- Dialog title: "Sync {name}"; section labels: "Files", "Playlists",
  "Capacity", "Statistics"; buttons: "Sync", "Close".
- Outcome: "Sync completed" / "Sync incomplete" / "Sync failed";
  the engine diagnostics verbatim (the B-013 convention).

## 4. Integration points

1. `crates/gtk-library/src/ipod_sync_service.rs` — the service, the
   phase machine, `SyncBoundary`, the summary fns (new module).
2. `crates/gtk-library/src/ipod_page.rs` — `sync_controls`,
   `apply_snapshot` arm for the button/checkbox, the review/result
   dialogs, the route-view wiring.
3. `crates/gtk-library/resources/routes/ipod_page.ui` — the checkbox,
   the in-flight spinner, the furniture comment on `sync_button` goes.
4. `crates/gtk-shell` — construct the boundary from the active iTunes
   source and pass the service into `ipod_route_view`.
5. `crates/sources` (if the artwork layer needs a small public
   helper for `ArtworkInput`) — else untouched.
6. `locales/rufin.pot` regen. No engine changes, no spec change → no
   delta file; tasks.md checkoff + docs §9 entry land post-FF.

## 5. Verification: gates (display-free)

In `ipod_page.rs` / the new module:

1. `sync_controls` matrix: Ready + supported + idle → enabled;
   every other state → disabled; `model_supported: false` on Ready →
   disabled with the gating note; `running` → disabled regardless;
   dead worker → disabled.
2. `review_summary` from a synthetic `DryRunReport`: the counts map
   to the summary fields (files, playlists, capacity, stats, art);
   an empty report renders the "nothing to do" shape.
3. `outcome_summary` from the three `SyncStatus` arms + the
   diagnostics passthrough.
4. The phase machine: `Idle --start(dry)--> Collecting --> Review`;
   `approve` → `Running --> Done`; `dismiss` → `Idle`; `start` while
   `Running` is a no-op; a runner returning `Failed` yields
   `Done(Failed)` with the recovery state intact; the bounded event
   ring keeps the last N events and never blocks a stalled consumer
   (the b011 pipe policy).
5. The boundary is shell-side glue (thin, untestable by policy) — the
   existing pair gate remains the proof that the engine wiring it
   mirrors is real.

Housekeeping: clippy 0 on touched crates, workspace check, i18n
check, `git diff --check`, fmt on touched files, `gtk4-builder-tool
validate` on the template (the B-014 lesson).

Manual acceptance (owner, env-gated): Ready + Dry Run checked →
dialog with real counts for the iTunesACCEPT library, nothing on the
pod or the `.itl` changes (tree hash); unchecked → the same dialog,
Sync runs the engine to completion on the disposable snapshot; gating
walks (non-Ready states, unsupported fake-tree model).

## 6. Scope boundaries (not this task)

Live progress bar and the results panel (B-016); Last Sync display
(B-016); recovery notices and the interrupted-banner clearing
(B-016); the cancel button (`cancelled` stays false); sync-source
selection UI (b008 debt — the single-iTunes-source rule, D-4); smart
criteria (B-023); device re-read (B-024); firmware version (B-025);
hardware acceptance (B-017). The five greyed Параметры checkboxes
stay furniture.

## 7. Risks

- **Layer inversion**: gtk-library must not see `sources` — the
  `SyncBoundary` trait keeps it so; the shell is the only place both
  exist.
- **The .itl parse on the run thread**: a multi-second block on a
  dedicated thread — fine; the connection worker stays untouched (the
  whole reason for the per-run thread).
- **Sentinel refusals**: the engine already returns partial results;
  the early `library_busy()` pre-check only moves the refusal sooner.
  Never bypasses itl-sync's authoritative check.
- **Stale review**: the plan is approved at dialog time; the engine
  re-verifies (`plan_hash`) against the device at run time — a
  mismatch fails the run, never silently applies a stale plan.
- **Double clicks**: `running` locks the controls; the phase machine
  ignores `start` outside `Idle`/`Review`-dismissed.
- **Dialog conventions**: no MessageDialog precedent in gtk-library —
  pick the repo's adwaita dialog pattern at implementation (checked
  against gtk-shell's dialogs).
- **The dev/release surface split** (the S-08 delta): only the dialog
  surface differs — the service, the gating, and the engine path are
  compiled identically in both builds, so the release silent run is
  not an untested second path (the plan-hash re-verification at run
  time holds in every build). The `cfg` gate is a surface difference,
  not a behavioral fork.

## 8. Decisions for the owner

- D-1: the Dry Run checkbox lives in the bottom bar next to Sync (the
  Параметры box keeps its greyed furniture) — **dev builds only**
  (`cfg!(debug_assertions)`, owner 2026-11-17); release builds omit
  the checkbox and the review dialog (the S-08 delta).
- D-2: dev builds show the review dialog on every Sync click —
  checked: review only; unchecked: review with the Sync button
  approving the reviewed plan. **Release builds run silently on the
  click** — the click is the approval, the engine re-verifies the
  plan hash at run time (owner 2026-11-17).
- D-3: the readiness recheck is the existing `preflight` before any
  real run; a refusal is a failed sync with the diagnostic, the page
  state stays.
- D-4: the sync target is **the** configured iTunes source — exactly
  one; with none (or several), the button stays disabled with a
  gating note. Source selection UI stays deferred (the b008 debt).

## 9. Documentation follow-ups (done with the task)

- Remove the B-015 takeover marker in `ipod_page.ui`; record the
  boundary in the module docs.
- The S-08 delta (`deltas/b015_delta.md`) is part of this task and
  ships with it; tasks.md's B-015 verification line is amended to
  match (dev-build Dry Run) in this branch.
- Pot regen; tasks.md checkoff + docs §9 entry land with the merge
  hygiene (post-FF).

## 10. Implementation steps

1. The service module: `SyncPhase`/`SyncState`/`advance`, the runner
   seam, the summary fns + gates (§5.1–5.4).
2. `sync_controls` + the page tests; `apply_snapshot` arm; the
   checkbox + spinner in the .ui; template validation.
3. The review/result dialogs (thin widget layer) + the click wiring
   (`start` with the checkbox state, `approve`, `dismiss`).
4. The shell-side boundary implementation + `ipod_route_view`
   signature change; workspace check.
5. Pot regen; housekeeping battery.
6. Manual acceptance run (owner, env-gated).

## 11. Implementation results

(filled after implementation — gates, corrections, battery)
