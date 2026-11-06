# tasks.md

Tasks are listed in dependency order; IDs stay stable, so B-007 precedes discovery and B-005 precedes preflight. B-001 remains unchecked until its reviewed task branch is merged into `feature/ipod-page`; checkoff and the result/hash line belong on that feature branch. No runtime or hardware acceptance is claimed by this research.

## Phase 1: Device Connection Infrastructure

- [x] **B-001: Research libgpod and connection flow for Video/Classic iPods**
  - Spec reference: R-02, S-03, S-05, R-03, R-04, S-12, R-05, S-13
  - Plan reference: Phase 1, References and status, Testing Strategy
  - Result: Research only — no implementation and no physical/hardware acceptance. Backend is libgpod (C, LGPL-2.1) behind a safe Rust FFI wrapper, required not optional (owner decision). No Trust/pairing step for Video/Classic. Start/stop-sync are no-ops on regular iPods, so there is no transaction; recovery/stat-merge is B-010's open item. Metadata provenance, Unix timestamps (host writes via itl-sync), nontransactional writes, and recovery/stat-import risks are recorded in [deltas/b001_research.md](deltas/b001_research.md). Three research gates remain open and are assigned to later tasks: write-file side effects (B-007/B-009/B-010), artwork error reporting (B-004/B-010), device-name provenance (B-005).
  - Commit: c7b1363 (checkoff on feature/ipod-page)
  - Depends on: B-001
- [x] **B-002: Define DeviceManager, ConnectionState, and DeviceInfo types**
  - Spec reference: R-02, R-03, S-06
  - Plan reference: Phase 1, Connection FSM, Data Models & Interfaces
  - Result: Backend-independent types defined in `crates/gtk-library/src/device_manager.rs` and exported from `gtk-library` (`pub mod device_manager`). No discovery and no write logic in this file. `ConnectionState` has exactly the five FSM states (`Disconnected`, `Connecting`, `BackendUnavailable`, `Ready`, `Error`); the UI label for `BackendUnavailable` is "Sync unavailable"; only `Ready` enables sync. `DeviceInfo` uses the plan's exact field set, with `Option` for absent values and no fabricated defaults. No authorization state exists. 16 unit tests cover the initial Disconnected/no-device state, all five states, optional firmware/capacity/FireWire ID, owned data without C pointers, preflight and capacity gates for Ready, device replacement, and diagnostics clearing.
  - Implementation: 374fad6; reviewed branch state including follow-up fixes: 16587a2.
  - Depends on: B-001
- [ ] **B-007: Integrate the safe libgpod Rust wrapper**
  - Spec reference: R-02, S-05, R-04; constitution §3
  - Plan reference: Phase 1, Data Models & Interfaces, Testing Strategy
  - Research gate before implementation: investigate [R1 and R2](deltas/b001_research.md#10-follow-up-research-required-before-implementation), document evidence and tests, and obtain owner review of resulting wrapper/design changes.
  - Verification: Owner approves loading/linking and packaging before implementation. Wrapper tests cover ownership, allocation/freeing, GLib strings/lists, GError, ABI compatibility, and serialized access. Backend-enabled calls work on disposable fixtures; backend-disabled/missing/incompatible configurations report unavailable and never fall back to copying. Write APIs are not exposed as an enabled destructive sync until B-010 safety verification.
  - Depends on: B-002
- [ ] **B-003: Implement mounted-device discovery using libgpod**
  - Spec reference: R-02, S-02, S-05
  - Plan reference: Phase 1, Connection FSM
  - Verification: A mount scan finds candidate `iPod_Control` directories and passes them to the wrapper; it does not mark a candidate Ready. Fixtures cover detection, missing backend, permissions, removal, and unrelated volumes; no initialization or capability-file writes occur during discovery.
  - Depends on: B-007
- [ ] **B-005: Implement DeviceInfo retrieval**
  - Spec reference: R-03, S-06
  - Plan reference: Phase 1, Data Models & Interfaces
  - Research gate before implementation: resolve the device-name provenance discrepancy in [R3](deltas/b001_research.md#r3-device-name-provenance-discrepancy) with evidence and owner review; align plan/task wording before coding.
  - Verification: Name provenance/fallback, model/generation, optional firmware, and applicable FireWire ID match fixture metadata under `iPod_Control/Device/`. Capacity comes from filesystem statistics, not nominal `Itdb_IpodInfo.capacity`. Missing values are explicit; metadata retrieval does not reload SysInfo on a live parsed/artwork context. No initialization or helper writes are triggered.
  - Depends on: B-003
- [ ] **B-004: Implement device readiness preflight (no Trust step)**
  - Spec reference: R-02, S-03, S-04, S-05
  - Plan reference: Phase 1, Connection FSM
  - Research gate before implementation: revisit [R2](deltas/b001_research.md#r2-artwork-errors-and-apparent-success) for readiness and review the resulting test/design proposal with the owner.
  - Verification: Only successful non-mutating checks of backend, access, parseable databases, measured capacity, supported model/artwork capabilities, and model-specific checksum prerequisites allow Ready. libgpod owns checksum generation. Tests cover missing Classic FireWire ID, Video without that requirement, missing firmware, unsupported models, read-only mounts, corruption, unavailable backend, retries, and removal. Missing prerequisites stay non-Ready with actionable errors; no pairing prompt appears. Physical model write acceptance is B-017, not claimed here.
  - Depends on: B-005
- [ ] **B-006: Expose connection status for UI binding**
  - Spec reference: R-02, S-05
  - Plan reference: Phase 1, Connection FSM
  - Verification: Observers receive Disconnected, Connecting, BackendUnavailable ("Sync unavailable"), Ready, and Error plus diagnostics; retries/removal invalidate stale state and preserve interrupted-operation notices separately.
  - Depends on: B-004

## Phase 2: Sync Core Logic (libgpod FFI)

- [ ] **B-008: Map .itl library data to libgpod models**
  - Spec reference: R-04, S-07, R-05, S-13
  - Plan reference: Phase 2, Sync Data Mapping
  - Verification: Fixture mappings cover files, master/static/smart playlists, track references, artwork databases/thumbnails, 0–100 ratings, stable host identity, and Unix timestamps with host conversion delegated to itl-sync. Comparisons/deltas are Rufin-owned; copying files alone does not count as sync.
  - Depends on: B-006
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
  - Verification: UI shows No iPod, Connecting, Sync unavailable, Ready, or Error from the same five-state contract, with retry/error details and no authorization wait/prompt.
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
