# Delta Spec: B-006 (expose connection status for UI binding)

**Task**: B-006: Expose connection status for UI binding (tasks.md, Phase 1).
**Spec reference**: R-02, S-05. **Plan reference**: Phase 1, Connection FSM.
**Status**: implemented 2026-11-09 on `feature/ipod-page-task-B-006`; owner
review pending; tasks.md checkoff stays the owner's commit (B-007 convention).

## Context

B-006 is the last Phase 1 task: the connection FSM (B-002 manager,
B-003/B-005/B-004 glue) worked, but nothing observed it and nothing drove it —
`apply_scan` had no production callers, and `Connecting` was deliberately
transient inside one call. B-006 ships the observer stream, the event-driven
scan driver, and the service-level retry. The UI page itself is Phase 3
(B-012/B-013) — like B-003/B-005 shipped glue without production callers,
B-006 ships the library surface (and the gates) only.

## Design decisions

Owner-approved 2026-11-09 (implementation plan
`~/.opencode/plan/b006-connection-status-implementation.md`):

1. **Event-driven scan driver via `gio::VolumeMonitor`** (no periodic
   polling; the hybrid poll fallback was explicitly rejected). Mount events
   (added/changed/removed) → 300 ms debounce → one scan command, plus a
   startup scan. Events are not filtered by fstype — B-003's discovery is
   the authority. The debounce also absorbs the mount-ready race (the first
   event can fire before the filesystem is readable); the glue's idempotent
   rescans make an early partial scan benign.
2. **Retry = full re-appearance at the service level** (`clear_device` +
   re-apply the last report — a "synthetic replug", retrieval and preflight
   re-run fresh). The glue itself gains no retry helper (B-004 decision 5
   unchanged); the preflight-only re-run on cached facts was rejected (stale
   capacity, un-healable retrieval-Error).
3. **Observer payload = snapshot, not transition events.**
   `ConnectionSnapshot { state, device, diagnostics, interrupted_operation }`
   — full clones, latest-wins; late subscribers read `snapshot()`, no replay.
4. **Emissions live in the glue, with per-call dedup.** `apply_scan` gained
   a fifth parameter `emit: &mut dyn FnMut(&DeviceManager)`; it fires after
   every mutating step but only when the observable tuple
   `(state, current_device, diagnostics)` changed since the previous emission
   within the call (the entry snapshot seeds the comparison). Idempotent
   rescans emit nothing; a same-value diagnostic re-set emits nothing.
5. **The service owns a serialized worker thread.** Blocking libgpod parses
   (retrieval + preflight — seconds) never run on the GTK main loop. The
   facade takes FIFO commands (`Scan`, `ScanFrom`, `Retry`,
   `SetInterrupted`, `ClearInterrupted`); the worker owns `Discovery`, the
   backend, the manager, and the last report. Nothing backend-related
   crosses the thread boundary; the worker exits detached when the last
   facade drops.
6. **`MountError` (mount-table read failure) → `tracing::error!`, no
   emission, no state change** — not a device verdict; the last known
   snapshot stays authoritative. Effectively unreachable on supported Linux.
7. **Interrupted-operation notice = service-owned placeholder, never
   FSM-owned.** `OperationNotice { detail }` carried in every snapshot;
   removal/retry/replug never clear it — only `clear_interrupted_operation`
   (or B-010's recovery) does. B-010 replaces the placeholder type; the
   snapshot field is the seam.
8. **`DeviceManager` (B-002) stays pure** — no channel, no glib dependency;
   its 21 tests untouched.

Technical decisions made during implementation (recorded here as the spec
record):

9. **`Connecting` is emitted at detection, before retrieval.** The plan
   initially emitted only after retrieval; the first implementation pass
   showed the gap: on a fresh plug, observers would stay on the previous
   state (Disconnected) for the whole multi-second parse, violating S-02
   ("when the app detects it, transition to Connecting"). The glue now does
   `set_device(placeholder)` + emit **before** the retrieval parse in the
   fresh-candidate branch. Consequence, documented in the module docs: a
   fresh appearance emits `Connecting` twice — placeholder, then facts —
   because the device info genuinely changed. Subscribers render state, not
   emission counts. (Backend recovery emits `Connecting` once, with the
   fresh facts — no placeholder wipe of the already-visible device info.)
10. **Observer transport = `async_channel` per-subscription channels.**
    glib 0.22 (workspace pin) removed `watch_channel`/`Sender`/`Receiver`
    entirely; `async_channel` is the sanctioned replacement and already a
    workspace dependency. Subscribers get a private unbounded channel; the
    worker broadcasts with pruning (`retain(|sender| sender.try_send(..).is_ok())`).
    The UI delivers to the main loop via `MainContext::spawn_local` in
    Phase 3.
11. **Debounce = future + cancellation flag, not `SourceId::remove`.** Two
    live glib 0.22 traps found while implementing the monitor:
    - `glib::timeout_add_local*` attaches to the **global default** main
      context (`g_timeout_add_full`), not the thread-default — undocumented
      surprise for tests and a lie for the private-context iteration.
    - `SourceId::remove` looks sources up in the **global default** context
      only, and **panics** on a missing source ("programmer error") — so a
      mount event arriving after a fired one-shot timeout would have crashed
      the GTK main thread.
    The debounce therefore spawns `glib::timeout_future(delay)` +
    `MainContext::spawn_local` with an `Arc<AtomicBool>` cancellation flag
    the task checks after the delay. No removal, no panics, testable on a
    private context (`trigger_on` seam) without touching the global default
    (which would race parallel tests that spawn futures on it).

Owner decisions from the B-006 code review (2026-11-10):

12. **Model policy: limitations are informational, not refusals.**
    Unsupported or unresolved models — and a supported Classic missing its
    FireWire GUID — reach `Ready`; the frontend knows the device is found
    but limited and gates sync functions accordingly. Rationale: research
    showed an empty `SysInfo` is the *normal* state of a freshly restored
    iPod. Consequences, implemented on this branch:
    - `ipod_sync::preflight` no longer refuses on model or checksum axes
      (remaining refusals: read-only mount, database parse, damaged
      artwork; `SkippedUnknownModel` artwork passes — "cannot be
      assessed", the limited state is visible in the UI, so B-007 §1.2's
      "never a silent artless device" holds).
    - The classifications moved into the facts: `DeviceFacts` gains
      `model_supported` and `checksum_possible` (computed by
      `retrieval::collect` from the preflight module's generation consts —
      the supported set lives in exactly one place), mirrored in B-002's
      `DeviceInfo` and mapped by the glue, so every B-006 snapshot carries
      the verdicts for the UI.
    - B-002's `DeviceManager` is untouched in behavior: `Ready` still
      requires capacities + a passed preflight; the verdicts ride along.
    - Recorded as an addendum in `deltas/b004_delta.md` (supersedes its
      `ChecksumPrerequisite`/`Model` fixture rows).

## Design

```
crates/gtk-library/src/ipod_discovery.rs   MODIFIED: apply_scan + emit param, dedup, detection-Connecting
crates/gtk-library/src/ipod_connection.rs  NEW: ConnectionSnapshot, OperationNotice, IpodConnection
crates/gtk-library/src/ipod_monitor.rs     NEW: IpodMountMonitor, Debounce
crates/gtk-library/tests/ipod_connection_gate.rs  NEW: env-gated end-to-end gates
scripts/ipod-dev                            MODIFIED: gates wired into test-snapshot/test-mounted; lint covers new files
```

### Emission contract (`apply_scan`)

Emissions happen at every mutating step, deduped per call against
`(state, current_device, diagnostics)`:

- removal → `Disconnected` (device cleared);
- fresh detection (loaded backend) → `Connecting` placeholder **before**
  retrieval, then `Connecting` facts / `Error` after it;
- backend unavailable on first scan → `Connecting` placeholder, then
  `BackendUnavailable` with the load reason;
- ABI-unverified → `Connecting` placeholder, `Connecting` facts,
  `BackendUnavailable` with the ABI diagnostic;
- preflight gate → `Ready` / `Error` / `BackendUnavailable`;
- recovery (verified) → `Connecting` facts → `Ready`; recovery (unverified)
  → one `BackendUnavailable` with the newer diagnostic (no re-parse);
- defensive Ready-downgrade on a flipped unverified report → emitted;
- idempotent rescans → **no emissions**; same-value reason re-set → none.

### Service (`IpodConnection`)

Cloneable facade + detached worker (see decision 5). `retry()` emits
`Disconnected` first (the clear is published), then the fresh sequence — the
synthetic replug is observable in full. Publishes are idempotent (a view
equal to the stored snapshot emits nothing). The retrieve/preflight closures
are hardcoded over `discovery.backend()` — no test seams in the production
API; the no-backend paths are unit-tested via `with_backend_search(
Explicit(<nonexistent>))`, and Ready-path coverage lives in the env-gated
gates (same pattern as B-004).

### Monitor (`IpodMountMonitor`)

Main-thread-only: `VolumeMonitor::get()`, three signals → debounced
`connection.scan()`, startup scan in `start()`. `Drop` disconnects handlers
and cancels the debounce. `Debounce` per decision 11.

## Fixture matrix

| Layer | Fixture | Covers |
|---|---|---|
| Glue (unit, 13 new + 28 migrated) | stub closures, `Cell` counters | exact emission sequences: fresh, retrieval-failure, ABI-unverified, backend-unavailable, removal, idempotent rescans, recovery, handover, takeover, newer-cause refresh |
| Service (unit, 12) | `Explicit(/nonexistent)` search, temp `iPod_Control` dirs, synthetic `MountSource`, FIFO-marker proofs | startup state, Connecting→BackendUnavailable, removal, retry (fresh + no-op), scan-error silence, notice survival across retry/removal, explicit clear, broadcast equality, late-subscriber no-replay, facade clones |
| Monitor (unit, 2) | private `MainContext` + `trigger_on` seam | burst coalescing, cancel; headless construct/drop |
| Gates (env-gated, 2) | `IPOD_TEST_TREE` snapshot, `IPOD_TEST_MOUNT` ro image, `RUFIN_LIBGPOD_DIR` | Ready end-to-end through the channel + no-write tree hash; ro-mount Error path end-to-end |

## Results

All verification points of tasks.md B-006 are covered: observers receive all
five states plus diagnostics (sequence tests + service tests + gates);
`BackendUnavailable` labels "Sync unavailable" (snapshots carry the B-002
`ui_label` contract verbatim); removal invalidates stale state (device
cleared, sync disabled via B-002); retries invalidate stale state (fresh
Connecting cycle, fresh retrieval + preflight); interrupted-operation
notices are preserved separately (service-owned, survive removal/retry,
explicit clear only).

Green: 112 gtk-library unit tests (was 85: +13 glue, +12 service, +2
monitor) plus the 2 env-gated observer gates as a separate test target,
fmt/clippy clean on both crates, `ipod-dev test-full` exit 0 with the gates
wired into the snapshot and mounted scenarios, mount state clean.

Implementation findings that shaped the code (recorded in decisions 9–11):
the Connecting-before-retrieval gap (plan deviation, flagged for owner
review), the glib 0.22 `watch_channel` removal, the
`timeout_add_local`-attaches-to-global-default behavior, and the
`SourceId::remove` panic trap.

## Open items

- **Worker-thread liveness (B-013)**: a crashed worker is currently
  indistinguishable from an idle one — a panic in the worker thread (e.g. a
  libgpod FFI crash) drops subsequent commands with a `tracing::error!`
  while `snapshot()` keeps serving the last stored view. B-013 owns the
  health surface: an atomic exit flag cleared when the worker loop ends,
  exposed as `is_worker_alive()` (or equivalent) on the facade, so the UI
  can surface a dead service instead of a frozen page. Found in the B-006
  code review.
- **Command FIFO latency**: notice/retry commands queue behind multi-second
  libgpod parses (the worker processes commands serially). Acceptable by
  design, but B-010/B-013 must expect the interrupted-operation notice to
  arrive with the delay of a scan already in flight.
- `IpodConnection`/`IpodMountMonitor` get their production owner in the
  shell when the page lands (B-012/B-013); until then they are library
  surface, same as B-003/B-005's glue.
- B-013's retry button calls `connection.retry()`; B-013 owns the replug
  hint copy.
- The `OperationNotice` placeholder is replaced by B-010's recovery model;
  the snapshot field stays.
- **Model identity for restored iPods (research, B-005/B-007 follow-up)**:
  an empty `SysInfo` is normal on a freshly restored device, so the model
  name needs an alternate source (owner direction from the B-006 review;
  firmware/USB descriptors were floated — unverified). Until then the UI
  shows the unresolved model as the limited state.
- Packaging follow-up (b007 delta §5) unchanged.
