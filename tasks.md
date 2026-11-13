# tasks.md

Tasks are listed in dependency order; IDs stay stable, so B-007 precedes discovery and B-005 precedes preflight. All Phase 1 tasks (B-001 through B-006) and B-007 are merged into `feature/ipod-page`; task-branch hashes are recorded in the entries that carry them. B-001 research does not claim runtime or hardware acceptance.

## Phase 1: Device Connection Infrastructure

- [x] **B-001: Research libgpod and connection flow for Video/Classic iPods**
  - Spec reference: R-02, S-03, S-05, R-03, R-04, S-12, R-05, S-13
  - Plan reference: Phase 1, References and status, Testing Strategy
  - Result: Research only — no implementation and no physical/hardware acceptance. Backend is libgpod (C, LGPL-2.1) behind a safe Rust FFI wrapper, required not optional (owner decision). No Trust/pairing step for Video/Classic. Start/stop-sync are no-ops on regular iPods, so there is no transaction; recovery/stat-merge is B-010's open item. Metadata provenance, Unix timestamps (host writes via itl-sync), nontransactional writes, and recovery/stat-import risks are recorded in [deltas/b001_research.md](deltas/b001_research.md). Three research gates remain open and are assigned to later tasks: write-file side effects (B-007/B-009/B-010), artwork error reporting (B-007/B-004/B-010), device-name provenance (B-005).
  - Commit: c7b1363 (checkoff on feature/ipod-page)
  - Depends on: None
- [x] **B-002: Define DeviceManager, ConnectionState, and DeviceInfo types**
  - Spec reference: R-02, R-03, S-06
  - Plan reference: Phase 1, Connection FSM, Data Models & Interfaces
  - Verification: Backend-independent types are defined and exported from `crates/gtk-library` (for example `src/device_manager.rs`) without discovery or write logic. Tests cover initial Disconnected/no-device state, the five planned states including BackendUnavailable, optional firmware/capacity/FireWire ID, and owned data without C pointers. The plan's exact DeviceInfo fields are represented; no authorization state exists.
  - Result: Implemented in `crates/gtk-library/src/device_manager.rs` with no discovery, backend dependency, or write logic. All five states and their labels are defined; only `Ready` enables the manager's sync predicate. `Ready` requires a caller-supplied preflight and present, valid capacity values; backend-unavailable failures retain their diagnostics. Actual backend/model checks and UI binding remain future work. 21 device-manager tests passed; `cargo check --package gtk-library`, formatting, and `git diff --check` passed. Test follow-ups: verify label uniqueness across every pair and add a zero-total-capacity rejection test.
  - Commit: eea14b0 (task-branch HEAD)
  - Depends on: B-001
- [x] **B-007: Integrate the safe libgpod Rust wrapper**
  - Spec reference: R-02, S-05, R-04; constitution §3
  - Plan reference: Phase 1, Data Models & Interfaces, Testing Strategy
  - Research gate before implementation: investigate [R1 and R2](deltas/b001_research.md#10-follow-up-research-required-before-implementation), document evidence and tests, and obtain owner review of resulting wrapper/design changes.
  - Verification: Owner approves loading/linking and packaging before implementation. Wrapper tests cover ownership, allocation/freeing, GLib strings/lists, GError, ABI compatibility, and serialized access. Backend-enabled calls work on disposable fixtures; backend-disabled/missing/incompatible configurations report unavailable and never fall back to copying. Write APIs are not exposed as an enabled destructive sync until B-010 safety verification.
  - Debt (found 2026-11-10, [b006 delta](deltas/b006_delta.md)): `ffi/layouts_sha.rs` carries `cd93…` while `sha256sum ffi/layouts.rs` is `7c4b…`, though the regeneration doc says they move together; reconcile the constant at the next `layouts.rs` regeneration.
  - Depends on: B-002
- [x] **B-003: Implement mounted-device discovery using libgpod**
  - Spec reference: R-02, S-02, S-05
  - Plan reference: Phase 1, Connection FSM
  - Verification: A mount scan finds candidate `iPod_Control` directories and passes them to the wrapper; it does not mark a candidate Ready. Fixtures cover detection, missing backend, permissions, removal, and unrelated volumes; no initialization or capability-file writes occur during discovery.
  - Depends on: B-007
- [x] **B-005: Implement DeviceInfo retrieval**
  - Spec reference: R-03, S-06
  - Plan reference: Phase 1, Data Models & Interfaces
  - Research gate before implementation: R3 resolved 2026-11-09 with owner review — the display name is the iTunesDB master-playlist name (type-1 `mhyp`; libgpod exposes it and reads no volume label anywhere). On the device snapshot the FAT32 label `IPOD USER` (uppercase-only format) is a derived lossy copy of the master playlist's `iPod user`; `Device/SysInfo` has no name key, and the real `SysInfo` is empty. No volume-name fallback (owner decision); a missing name stays explicit. Evidence in [deltas/b005_delta.md](deltas/b005_delta.md).
  - Verification: The name comes from the master playlist's name; model/generation from `SysInfo` `ModelNumStr` via libgpod's model table; optional firmware and applicable FireWire ID only from device metadata that actually contains them (`SysInfoExtended`). Capacity comes from filesystem statistics, not nominal `Itdb_IpodInfo.capacity`. Missing values are explicit; metadata retrieval does not reload SysInfo on a live parsed/artwork context. No initialization or helper writes are triggered.
  - Debt (from the B-006 review, [b006 delta](deltas/b006_delta.md)): an empty `SysInfo` is the normal state of a freshly restored iPod — model identity needs an alternate source (firmware/USB descriptors floated, unverified). Until then the UI shows the unresolved model as limited (`model_supported == false`).
  - Depends on: B-003
- [x] **B-004: Implement device readiness preflight (no Trust step)**
  - Spec reference: R-02, S-03, S-04, S-05
  - Plan reference: Phase 1, Connection FSM
  - Research gate before implementation: revisit [R2](deltas/b001_research.md#r2-artwork-errors-and-apparent-success) for readiness and review the resulting test/design proposal with the owner.
  - Verification: Only successful non-mutating checks of backend, access, parseable databases, measured capacity, supported model/artwork capabilities, and model-specific checksum prerequisites allow Ready. libgpod owns checksum generation. Tests cover missing Classic FireWire ID, Video without that requirement, missing firmware, unsupported models, read-only mounts, corruption, unavailable backend, retries, and removal. Missing prerequisites stay non-Ready with actionable errors; no pairing prompt appears. Physical model write acceptance is B-017, not claimed here.
  - Depends on: B-005
- [x] **B-006: Expose connection status for UI binding**
  - Spec reference: R-02, S-05
  - Plan reference: Phase 1, Connection FSM
  - Verification: Observers receive Disconnected, Connecting, BackendUnavailable ("Sync unavailable"), Ready, and Error plus diagnostics; retries/removal invalidate stale state and preserve interrupted-operation notices separately.
  - Result: Observer snapshots (`ConnectionSnapshot { state, device, diagnostics, interrupted_operation }`) on per-subscription async-channel receivers, emitted by the FSM glue (`apply_scan` gained an `emit` callback with per-call dedup; `Connecting` is emitted at detection, before the retrieval parse — a plan deviation, owner-approved in review — so a fresh plug emits Connecting twice: placeholder, then facts). A cloneable `IpodConnection` service with a serialized worker thread (blocking libgpod parses never touch the GTK main loop), full-re-appearance `retry()`, and a service-owned interrupted-operation notice that removal/retry never clear. An event-driven `IpodMountMonitor` (gio VolumeMonitor → 300 ms debounce → scan; no periodic polling — owner decision). Review follow-ups (owner decisions 2026-11-10): the model policy — unsupported/unresolved models and Classic-without-FireWire reach Ready, the `model_supported`/`checksum_possible` verdicts ride in `DeviceFacts`/`DeviceInfo` for UI gating, preflight refuses only on ro mount/database/artwork ([b006 delta](deltas/b006_delta.md) decision 12, [b004 delta](deltas/b004_delta.md) addendum); a third env-gated gate proves Ready on a genuine rw mount with the no-write tree hash. 112 gtk-library unit tests + 3 gates; test-full and test-mounted-rw green. Debts carried below: worker liveness → B-013, notice FIFO latency → B-010, model-identity research → B-005, layouts_sha reconciliation → B-007.
  - Commit: 3a1a367 (task-branch HEAD, fast-forwarded into feature/ipod-page)
  - Depends on: B-004

## Phase 2: Sync Core Logic (libgpod FFI)

- [x] **B-008: Map .itl library data to libgpod models**
  - Spec reference: R-04, S-07, R-05, S-13
  - Plan reference: Phase 2, Sync Data Mapping; design and research in [deltas/b008_plan.md](deltas/b008_plan.md)
  - Verification: Fixture mappings cover files, master/static/smart playlists, track references, artwork databases/thumbnails, 0–100 ratings, stable host identity, and Unix timestamps with host conversion delegated to itl-sync. Comparisons/deltas are Rufin-owned; copying files alone does not count as sync.
  - Depends on: B-006
  - Blocker (cross-repo): the host track PID lives at `mith` @0x80 (= XML track Persistent ID = device dbid, verified on the 2026-11-10 snapshot pair); `itl-core` 0.1.0 reads `persistent_id()` from the wrong offset and does not expose the smart-rule payloads (mhoh 0x65/0x66). The host adapter needs the itl-core fix released and the Rufin dependency bumped ([b008 plan](deltas/b008_plan.md) §2a/§9).
  - Debt (owner 2026-11-12): smart-rule field 0x85 — purchase/iCloud status bitmask, bit 0 = downloaded, per snapshot evidence; confirmation is backlog (iTunes UI check or a device-write experiment). The mapper represents it explicitly and never rejects on it; device-side rewrite of 0x85 rules stays B-010.
  - Debt: sync-source selection is a UI step (future work) and is not covered by spec.md — close with the sync-UI task via a delta under R-04 ([b008 plan](deltas/b008_plan.md) §9).
- [ ] **B-009: Implement Dry Run delta calculation**
  - Spec reference: R-04, S-08
  - Plan reference: Phase 2, Sync Data Mapping, Recovery and Statistics
  - Research gate before implementation: revisit [R1](deltas/b001_research.md#r1-itdb_write_file-side-effects) for dry-run isolation and review the resulting test/design proposal with the owner.
  - Verification: Dry Run reports expected changes; before/after tree comparisons prove no creation, deletion, or mutation of device files (including stats/auxiliary files) and no host `.itl` write. Potentially mutating parse operations run only on disposable snapshots.
  - Depends on: B-008
- [ ] **B-010: Implement full bidirectional sync and recoverable coordination**
  - Spec reference: R-04, S-07, S-10, S-12, R-05, S-13
  - Plan reference: Phase 2, Sync Data Mapping, Recovery and Statistics
  - Research gate before implementation: revisit [R1 and R2](deltas/b001_research.md#10-follow-up-research-required-before-implementation) for device-write side effects, artwork failure reporting, and recovery; obtain owner review of the resulting proposal.
  - Verification: Review the concrete backup/journal, write-ordering, writer-exclusion, stat-merge/deduplication, recovery, and flush/eject protocol before enabling destructive writes. Fixtures verify files, playlists, artwork, model-specific libgpod checksums, and host stats via sentinel-guarded itl-sync only. Failure injection covers disk full, removal, each multi-file write boundary, competing writers, zero sentinel, device success/host failure, host commit before acknowledgement, retrying twice, and reassigned numeric device track IDs. Recover the last consistent library and preserve pending changes without lost or duplicate imports. Partial success never advances last-success time. Test copies only; no start/stop-sync transaction assumption.
  - Debt (from the B-006 review, [b006 delta](deltas/b006_delta.md)): the interrupted-operation notice is carried by the serialized connection worker, so it queues behind multi-second libgpod parses and can arrive with the delay of an in-flight scan; the recovery protocol must account for the FIFO latency.
  - Depends on: B-009
- [ ] **B-011: Implement sync progress and completion reporting**
  - Spec reference: R-04, S-09, S-10, S-12, R-05, S-13
  - Plan reference: Phase 2, Data Models & Interfaces
  - Verification: Rufin emits work/phase progress, including file-copy work; complete, partial, and failed results are distinct. Only a fully completed device and host sync updates Last Sync; interrupted and partial results retain actionable recovery information.
  - Depends on: B-010

## Phase 3: UI Implementation

- [ ] **B-012: Create iPod Page base layout**
  - Spec reference: R-01, S-01
  - Plan reference: Phase 3
  - Verification: Page is always accessible in Rufin, including its No iPod view when disconnected.
  - Depends on: B-011
- [ ] **B-013: Implement Connection State views**
  - Spec reference: R-02, S-05
  - Plan reference: Phase 3, Connection FSM
  - Verification: UI shows No iPod, Connecting, Sync unavailable, Ready, or Error from the same five-state contract, with retry/error details and no authorization wait/prompt. Also owns the B-006 review debts ([b006 delta](deltas/b006_delta.md)): a crashed `IpodConnection` worker is currently silent (commands dropped with an error log, `snapshot()` keeps serving the last view) — surface worker liveness (`is_worker_alive()` or equivalent); the retry button calls `connection.retry()`, with the replug hint copy.
  - Depends on: B-012
- [ ] **B-014: Implement Device Overview view**
  - Spec reference: R-03, S-06
  - Plan reference: Phase 3, Data Models & Interfaces
  - Verification: Ready view displays actual name/model, firmware or unavailable, and measured capacity without fabricated defaults or nominal-capacity substitution.
  - Depends on: B-013
- [ ] **B-015: Implement Sync button with Dry Run checkbox**
  - Spec reference: R-04, S-07, S-08, S-11
  - Plan reference: Phase 3
  - Verification: Sync is disabled outside Ready, readiness is rechecked before writes, and Dry Run is available and nonmutating.
  - Depends on: B-014
- [ ] **B-016: Connect UI state to DeviceManager**
  - Spec reference: R-02, S-05, R-04, S-09, S-10, S-12, R-05, S-13
  - Plan reference: Phase 3, Connection FSM
  - Verification: UI follows state/progress changes without blocking, clears stale device info on removal, and surfaces partial results, pending host write-back, and recovery notices without reporting success.
  - Depends on: B-015

## Phase 4: Refinement & Testing

- [ ] **B-017: Conduct per-model physical-device and iTunes acceptance**
  - Spec reference: R-02, R-03, R-04, S-12, R-05, S-13
  - Plan reference: Phase 4, Testing Strategy
  - Verification: On each supported Video/Classic model, verify files, playlists, artwork, checksums, play/skip/rating/date round trips, interrupted recovery, and safe ejection. Check subsequent iTunes device compatibility and open resulting disposable host-library copies in real Windows iTunes. Record exact hardware/library versions and limits; never use the live host `.itl` or simultaneous VM/host device writers.
  - Depends on: B-016
- [ ] **B-018: Verify Dry Run accuracy and nonmutation**
  - Spec reference: R-04, S-08
  - Plan reference: Phase 4, Testing Strategy
  - Verification: Dry Run output matches actual fixture sync changes; device-tree/stat-file and host-library comparisons show no mutation from Dry Run itself.
  - Depends on: B-017
- [ ] **B-019: Final UI Polish and performance optimization**
  - Spec reference: R-04, S-09
  - Plan reference: Phase 4
  - Verification: UI remains responsive (FPS > 30) during heavy sync activity; sync doesn't block the main thread.
  - Depends on: B-018
- [ ] **B-020: Finalize feature documentation and acceptance records**
  - Spec reference: R-02, R-04, S-12, R-05, S-13
  - Plan reference: Phase 4, References and status, Testing Strategy
  - Verification: This repository's plan/tasks link [research](deltas/b001_research.md), [owner decision](deltas/research-based-decisions.md), and the current deltas. Record actual implementation/acceptance results and remaining limits there; do not refer to nonexistent Rufin NOTES.md or session-local plans. Follow the constitution/SDD artifact lifecycle when the feature completes, preserving history rather than inventing a local format-notes file.
  - Depends on: B-019
- [ ] **B-021 (backlog): Refresh smart-playlist materialized member lists after host stat writes**
  - Spec reference: R-05, S-13 — spec delta to review at implementation time
  - Plan reference: [b008_plan.md](deltas/b008_plan.md) §2a/§7 (the materialized-lists finding)
  - Depends on: B-010
  - Why it exists: in the `.itl` every smart playlist stores both its criteria (mhoh 0x65/0x66 `SLst` + Smart Info) and a materialized member list (mtph entries) — a cache of the criteria evaluation that iTunes computes when it opens the library or syncs a device. `itl-sync` writes ratings/counts/play dates straight into `mith` fields and never touches playlist blocks, so after every Rufin write the member lists are stale until iTunes itself re-opens the library. Measured 2026-11-12 on the reference library: between the 09:03 XML export and the 10:33 `.itl` backup, 216 members of «Хай рейт» (rating > 3.5★) dropped from 80/100 to 0/20 (the whole The Notorious B.I.G. album — e.g. #6973 «Intro» 100→0) and 6 tracks rose to ≥70 without entering the list: the cache went stale in both directions within 90 minutes of an out-of-band rating write. The criteria are the truth; iTunes re-materializes on its next open and the iPod firmware re-evaluates live-updating playlists itself, but any consumer of the stored lists (including a fallback sync path) would see phantom members.
  - Goal: after host stat writes (or as a sync step), re-evaluate the smart criteria with iTunes semantics — effective rating with album fill-down, boundary-minus-1 encoding, media-kind bits, Smart Info limits/sort, unknown fields (e.g. 0x85) handled conservatively — and write back only the member lists of the playlists whose evaluation changed.
  - Hazards to remember: playlist-block mutation walks into the `reindex()` second-`mlth` merge (itl-player AGENTS.md) — never add/remove `mith` records while refreshing lists; rewrite only the `mtph` entries and preserve every `miph` raw byte; the evaluator must first be byte-verified against materialized lists on synchronized data (the B-008 evaluator cross-check).
  - Verification: fixtures with known criteria and stats; member lists refresh to match the evaluation; the result opens in iTunes; playlists whose evaluation did not change stay byte-identical.
