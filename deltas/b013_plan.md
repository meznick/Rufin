# B-013: Connection state views

Status: owner-approved (2026-11-17); implemented as written (D-1..D-5 defaults accepted).

## 1. What this task is

B-013 replaces the temporary stub arm of the iPod page with the real
connection-state views. Spec reference: R-02 (Connection FSM), S-02
(Device connection), S-03 (preflight, no Trust step), S-05 (Error
handling). Plan reference: Phase 3, Connection FSM. Task verification:
the UI shows No iPod, Connecting, Sync unavailable, Ready, or Error from
the same five-state contract, with retry/error details and no
authorization wait/prompt.

Also owned per tasks.md and the B-006 review debts
(`deltas/b006_delta.md` "Open items"): surface worker liveness — a dead
`IpodConnection` worker must not look like a frozen page — and the retry
button that calls `connection.retry()` with the replug hint copy.

Scope is the state views and their actions only. The Ready arm stays
minimal (no overview fields, no sync button): the device overview is
B-014, the sync controls are B-015, the `ProgressPipe` subscription is
B-016. Nothing here writes to the device or the `.itl`.

## 2. What already exists (measured, not guessed)

- `crates/gtk-library/src/ipod_page.rs` (B-012): `IpodPageView` over
  `routes/ipod_page.ui` with exactly two stack pages (`no-ipod`, `stub`);
  the pure `decide()` → `PageDecision { page, label, dot_class }`; the
  `Stub` arm is explicitly marked as the B-013 takeover point; the
  snapshot subscription via `glib::spawn_future_local`; the free
  `ipod_route_view(connection)` entry the shell calls on the GTK main
  thread. `state_label()` already carries the five `msgid`s
  ("Disconnected", "Connecting", "Sync unavailable", "Ready", "Error").
  3 tests (2 `decide` matrices + 1 channel-flow test).
- `ConnectionSnapshot { state, device, diagnostics, interrupted_operation }`
  — no liveness field yet. `DeviceInfo` has `name` and is populated with
  partial information during `Connecting` (B-002 contract); the manager
  fills `diagnostics` on `BackendUnavailable`/`Error` and clears it on
  retry/device change/success (`device_manager.rs:241`, tested at :620).
- `IpodConnection` (B-006): commands `Scan`/`ScanFrom`/`Retry`/
  `SetInterrupted`/`ClearInterrupted`. `Retry` re-applies the **last
  report** (synthetic replug) — it does not rescan the mount table, so a
  physically replugged device needs `scan()`, not `retry()`. The worker
  is a detached thread that exits when the command channel disconnects;
  **there is no exit flag**. `send()` on a dead worker only logs
  (`ipod_connection.rs:290` — "command dropped") — this is the b006
  liveness debt. The facade already shares `snapshot` and `subscribers`
  behind `Arc<Mutex<_>>`, so a send-failure fan-out is available without
  new plumbing. `publish()` dedups equal views.
- `ipod_page.ui`: the `no-ipod` page has the S-01 status row (dot +
  state label); the stub page has only a heading. GtkStackPage convention
  (name on the page) already applied. CSS: `.ipod-status-dot`,
  `.ipod-status-disconnected`, and a global `.error-text` class exist.
- Profiles: `panic = "abort"` is set for release and android only
  (`Cargo.toml:178,193`); dev/test unwind. So a worker-thread panic in a
  release build kills the process (no page can help), while in dev builds
  it silently kills the worker — the liveness surface protects dev builds
  and any graceful worker exit.
- i18n: `msgid`/`tr` pattern; the pot regenerates via
  `cargo run -p xtask -- generate i18n-template`.
- Existing `ConnectionSnapshot` literal constructions that gain the new
  field: the worker's initial snapshot (`ipod_connection.rs:214`), the
  emit closure (:158), `publish()` (:175), the test helper (:413), and
  `ipod_page`'s `snapshot_with` (:162).

## 3. The design

### 3.1 Worker liveness (the b006 debt)

Add `worker_alive: bool` to `ConnectionSnapshot` (default `true` at
worker init and in all emit/publish paths). The worker thread records
its exit: a drop guard on the `run()` loop clears a shared
`Arc<AtomicBool>`; `IpodConnection` gains `is_worker_alive()` reading it.

The event half: on `send()` failure (the channel send errors only when
the worker's receiver is gone — from the facade's side that is worker
death), the facade sets `worker_alive: false` in the shared snapshot and
fans the view out to all subscribers, bypassing `publish()`'s dedup.
Every live subscriber — the page included — gets exactly one
dead-service event. Lock ordering matches `publish()` (snapshot, then
subscribers) so the fan-out and a racing publish cannot deadlock.

Why not a facade-only `is_worker_alive()` + a page-side timer: polling
contradicts the B-012 no-polling decision and gives no event. Why not a
new `ConnectionState` variant: the FSM states are the B-002 contract;
"the service is gone" is not a connection state, and inventing one would
corrupt S-05 semantics everywhere `ConnectionState` is matched.

### 3.2 The page

The stack grows from two pages to six (all via GtkStackPage):

1. `no-ipod` (Disconnected, S-01) — unchanged composition, plus a
   **"Scan again" button** (`scan_button`, `connection.scan()`) under the
   status row. Needed because scanning is manual (B-012 decision): a pod
   plugged in while the page is open is invisible until the page
   reopens. The button is the on-page trigger.
2. `connecting` — a `GtkSpinner` (spinning) + heading "Connecting" +
   a muted device-name label when `snapshot.device` is `Some` (partial
   info is legitimate during Connecting). Transient by design; no
   actions.
3. `unavailable` (BackendUnavailable) — warning icon + heading
   "Sync unavailable" + the **diagnostic verbatim** (wrapped, muted —
   the ABI message is the "explain the backend problem" of S-05) + a
   hint label + a **"Scan again" button**. No Retry here: `retry()`
   re-runs preflight against the same dead backend and is pointless;
   after fixing the backend the user rescans.
4. `ready` — iPod icon + heading "Ready" + the device-name label.
   Deliberately minimal: no fabricated capacity/model fields (S-06 is
   B-014), no sync button (B-015). `sync_enabled()` stays unused.
5. `error` — error icon + heading "Error" + the **diagnostic verbatim**
   (`.error-text` class) + the replug hint copy + **"Retry"** (primary,
   `connection.retry()` — same-device transient failures) + **"Scan
   again"** (secondary — a replugged device needs a rescan, see §2).
6. `service-dead` — warning icon + heading "Connection service stopped"
   + body copy ("Restart Rufin to reconnect the iPod."). No actions:
   every command would be dropped by the dead worker.

The interrupted-operation notice: a `GtkRevealer` banner **above the
stack** (not per-page), always in the template — warning icon + fixed
heading "Sync was interrupted" + the notice `detail` verbatim. It shows
in **every** state while `snapshot.interrupted_operation` is `Some`
(the service never clears it except explicitly — B-006 contract) and is
**display-only**: no dismiss button. S-12 requires reporting the
incomplete operation until recovery verifies; the dismiss/recovery
control belongs to the B-015/B-016 sync flow, not here. The banner also
renders on `service-dead` — the report outlives the service.

`decide()` remains the single semantic layer, extended:

```rust
pub enum RenderedPage { NoIpod, Connecting, Unavailable, Ready, Error, ServiceDead }

pub struct PageDecision {
    pub page: RenderedPage,
    /// Heading msgid (the state label; NoIpod keeps its fixed .ui label).
    pub label: &'static str,
    /// Render the diagnostics label verbatim (Unavailable/Error only).
    pub show_diagnostics: bool,
    /// Render the device-name label when the snapshot carries one
    /// (Connecting/Ready only).
    pub show_device: bool,
    /// Reveal the interrupted-operation banner.
    pub notice_visible: bool,
    pub dot_class: Option<&'static str>,   // Disconnected only, as today
}
```

Precedence: `!worker_alive` → `ServiceDead` regardless of `state`
(notice still visible). `notice_visible` ⇔
`interrupted_operation.is_some()`. `show_diagnostics` for
Unavailable/Error only; `show_device` for Connecting/Ready only.
`apply_snapshot` applies the decision, reads `diagnostics`/`device.name`/
`detail` verbatim from the snapshot, and sets the revealer.

### 3.3 Wiring

`ipod_route_view(connection)` connects the three action buttons:
`scan_button` → `connection.scan()`; the Error view's retry →
`connection.retry()`; its scan button → `connection.scan()`. Closures
clone the facade (cheap). No template `bind()` — the composite is
constructed here and the connection is in scope.

### 3.4 Copy (proposed, translatable)

- No-iPod button: "Scan again" (accessible label "Scan for connected iPods").
- Error hint: "Reconnect the iPod and try again."
- Unavailable hint: "Check the libgpod backend installation, then scan again."
- Dead-service heading: "Connection service stopped"; body: "Restart Rufin
  to reconnect the iPod."
- Notice heading: "Sync was interrupted".

## 4. Integration points

1. `ipod_connection.rs` — the snapshot field, the exit flag + guard,
   `is_worker_alive()`, the send-failure fan-out, the literal
   constructions.
2. `ipod_page.rs` — the six-page decision, `apply_snapshot`, the button
   wiring, the module doc (the "B-013 replaces this arm" markers go).
3. `resources/routes/ipod_page.ui` — five new stack pages, the notice
   revealer above the stack, the scan button on the No-iPod page.
4. `gtk-preferences/resources/style.css` — `.ipod-notice` banner styling;
   reuse `muted`/`error-text`/`dim-label` everywhere else.
5. `locales/rufin.pot` regeneration (i18n check).
6. No spec change → no delta file. No shell/route/topbar changes — the
   B-012 seams carry everything.

## 5. Verification: gates (unit tests, display-free)

In `ipod_page.rs` (replacing the two `decide` tests, keeping the
channel-flow test):

1. The five-state matrix: each of the five `ConnectionState`s decides its
   page, its `msgid` heading, and the correct `show_diagnostics` /
   `show_device` flags (device present vs absent, both arms).
2. Precedence: any state + `worker_alive: false` → `ServiceDead`; the
   notice stays visible on the dead page.
3. `notice_visible` ⇔ `interrupted_operation.is_some()` (both arms,
   all pages).

In `ipod_connection.rs`:

4. The crash gate: `ScanFrom` a test `MountSource` whose `mounts()`
   panics (the worker thread unwinds — honest crash simulation, no
   test-only production API); then `is_worker_alive()` is `false` and
   the next command's send failure fans out a snapshot with
   `worker_alive: false` to subscribers. libtest's process-wide capture
   swallows the worker-thread panic output. No libgpod needed — the
   panic fires in `discovery.scan` before the backend is touched.
5. Normal-flow snapshots carry `worker_alive: true`; the existing suite
   (15 tests incl. retry/notice/removal sequences) stays green with the
   field added.

Housekeeping: `cargo clippy` 0 on the touched crates, `cargo check
--workspace`, i18n check, `git diff --check`.

Widget construction is untestable in this repo (GtkWidget class init
asserts the main thread) — the button wiring and the revealer are the
thin manual-acceptance layer, same policy as B-012: `decide()` carries
every semantic.

Manual acceptance: launch with `RUFIN_LIBGPOD_DIR`, walk the states —
No-iPod + Scan again (click, pod replug, click), Connecting (plug in,
watch spinner), Ready, unplug mid-page → Disconnected, Error (a pod
whose DB is unreadable), and the dead-service page via a dev build.

## 6. Scope boundaries (not this task)

Device overview fields on Ready (B-014); sync button/dry run/readiness
recheck (B-015); progress and result rendering (B-016); notice dismissal
and recovery actions (B-015/B-016); hardware acceptance (B-017); smart
criteria (B-023). S-05's "keep sync disabled" is satisfied by there
being no sync button yet — B-015 owns the real gate.

## 7. Risks

- **Release panic=abort**: a worker panic in release kills the process,
  so the dead-service page is reachable only in dev builds or on
  graceful exit. Accepted: the debt text itself assumes unwind, and the
  surface also future-proofs a later `catch_unwind` worker boundary.
- **FIFO latency**: notice/retry commands queue behind an in-flight
  multi-second parse (b006 debt) — the banner may lag a scan. Accepted
  by design; the page never blocks on it.
- **Retry-after-replug confusion**: `retry()` uses the stale last report.
  Mitigated by pairing Retry with a Scan-again button and by the hint
  copy; documented in the module docs.
- **Long diagnostics** (the ABI message): wrapped, muted, max-width —
  no layout risk.

## 8. Decisions for the owner

- D-1: liveness = snapshot field + send-failure fan-out (+
  `is_worker_alive()`), no polling, no new FSM state (§3.1).
- D-2: the No-iPod view gains a Scan-again button (manual-scan B-012
  decision makes it necessary) (§3.2.1).
- D-3: the interrupted-notice banner is display-only, no dismiss —
  recovery (B-015/B-016) owns clearing (§3.2).
- D-4: Unavailable gets Scan-again instead of Retry; Error gets both
  Retry (primary) and Scan-again (secondary) (§3.2.3/3.2.5).
- D-5: the crash gate uses a panicking `MountSource` as the worker-kill
  injection — no test-only command variant in production code (§5.4).

## 9. Documentation follow-ups (done with the task)

- Remove the "B-013 replaces this arm" markers (`ipod_page.rs` module
  doc + the stub arm comment).
- `deltas/b006_delta.md` open items: mark the two B-013-owned entries
  (worker liveness, retry/replug copy) as resolved with a pointer to
  this plan.
- `locales/rufin.pot` regen; tasks.md checkoff + docs §9 entry land with
  the merge hygiene (post-FF), not on the task branch.

## 10. Implementation steps

1. `ipod_connection.rs`: the snapshot field + exit flag + guard +
   `is_worker_alive()` + send-failure fan-out; update all literal
   constructions; the crash gate; run the suite.
2. `ipod_page.rs`: the extended `decide`/`RenderedPage`/`PageDecision`,
   the matrices, the ServiceDead precedence; run the suite.
3. `ipod_page.ui`: the five new pages + notice revealer + scan button.
4. `apply_snapshot`: diagnostics/device/notice application; `composite_box!`
   field updates.
5. `ipod_route_view`: button wiring (scan ×3, retry).
6. CSS `.ipod-notice`; pot regen; clippy/fmt/workspace/i18n checks.
7. Manual acceptance run (owner machine, env-gated backend).

## 11. Implementation results

**Done 2026-11-17** on `feature/ipod-page-task-B-013`, per §10, with all
five owner decisions as written:

1. `ipod_connection.rs`: `worker_alive` in `ConnectionSnapshot`
   (D-1); the worker carries a shared exit flag with an `AliveGuard`
   clearing it when the loop ends (graceful or unwind); the facade's
   `is_worker_alive()` reads it; a failed command send announces the
   death once — flip the shared snapshot and fan out, lock ordering
   identical to `publish()`. New gates: normal snapshots carry a live
   worker; the crash gate injects a panicking `MountSource` (D-5),
   polls the flag, asserts the fan-out snapshot, the facade-snapshot
   flip, and the idempotent re-announce silence.
2. `ipod_page.rs`: six-page `RenderedPage` (NoIpod, Connecting,
   Unavailable, Ready, Error, ServiceDead), the extended `PageDecision`
   (label, `show_diagnostics`, `show_device`, `notice_visible`,
   `dot_class`), the ServiceDead precedence (keeps the notice — the
   report outlives the service), `apply_snapshot` (headings, verbatim
   diagnostics/device/notice, revealer), and the action wiring in
   `ipod_route_view`: Scan again on No-iPod/Unavailable/Error, Retry on
   Error (D-2/D-4). 7 decide-matrix gates replace the two B-012 ones;
   the channel-flow gate stays.
3. `ipod_page.ui`: six stack pages via GtkStackPage, the
   display-only `GtkRevealer` notice banner above the stack (D-3),
   state icons/spinners, hint copies.
4. `style.css`: `.ipod-notice` (accent-bg mix — no warning token
   exists in the theme). `locales/rufin.pot` regenerated.
5. `deltas/b006_delta.md`: the two B-013-owned open items marked
   resolved with pointers.

Battery: gtk-library 121 lib tests (115 + 6) + 3 env-gated connection
gates — green; clippy 0 on gtk-library; workspace check + i18n check
green; my files fmt-clean (the pre-existing toolchain drift stays
untouched, reverted after `cargo fmt --all` churn as in B-012).

**Gate-forced corrections:**

- The workspace denies `clippy::panic_in_result_fn` — the deliberate
  crash in `PanickingMounts::mounts` carries a documented
  `#[allow]`: the panic is the point of the injection, not an
  accidental panic-in-Result.
- No other corrections: the design held as planned; the composite
  fields cross-check (19 bound ids vs the .ui) passes, so the template
  instantiates without the B-012 class of stack-page mismatch.

Manual acceptance (owner machine, env-gated backend) remains: the
five-state walk, the notice banner, Scan again / Retry clicks, and the
dead-service view in a dev build.
