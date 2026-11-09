# Delta Spec: B-003 (mounted-device discovery using libgpod)

## Context

B-003 implements the discovery half of the connection FSM: a mount scan finds
candidate `iPod_Control` directories and passes them to the wrapper-backed
flow. Detection alone must never mark a candidate Ready — that gate stays with
B-004's preflight. Spec wording needs no change: S-02/S-05 already describe
detection, Sync-unavailable behavior, and removal handling.

Depends on B-007 (the `ipod` wrapper crate, merged). B-005 (DeviceInfo
retrieval), B-004 (preflight), and B-006 (observer/UI binding) build on top.

## Design decisions (owner-approved, 2026-11-09)

1. **New crate `crates/ipod-sync`** hosts the discovery engine: depends only
   on the `ipod` wrapper, `tracing`, and `thiserror`. No GTK, no tokio, no
   compile-time libgpod. It will grow B-008+ sync logic later; layering stays
   non-GTK below `gtk-library`.
2. **Mount scan = `/proc/self/mountinfo` parser behind a `MountSource`
   trait.** Zero new deps, synchronous, fixture-testable; removal = diff
   between scans. Linux-only matches the wrapper's Linux-only platform
   support (b001_research §3 sketch: mount-point scan, not a USB watcher).
3. **Filesystem-level detection + backend load only.** A candidate is a mount
   whose root contains an `iPod_Control/` *directory* (one non-recursive
   `symlink_metadata` probe). The backend is loaded once (lazily, retried on
   failure) for availability + `abi_verified`. Discovery never parses and
   never opens a session: wrapper research §1.1 shows parse paths can mutate
   live device trees (a missing `Artwork/` gets `g_mkdir`-ed), so zero-write
   discovery is guaranteed structurally and proven by a tree-hash fixture.
4. **FSM glue included in B-003**, in `gtk-library` (the only crate that can
   touch B-002's `DeviceManager` without moving it). B-006 then only adds the
   observer/UI stream.

## MODIFIED

- `crates/ipod`: re-export `BackendSearch` from the crate root so consumers
  can load the backend with an explicit directory (deterministic tests
  without process-env mutation). `IpodBackend::load_with` was already public.
- Workspace root `Cargo.toml`: new `ipod-sync` workspace member dependency.
- `crates/gtk-library`: new dependency on `ipod-sync`; new public module
  `ipod_discovery` (`apply_scan`). `device_manager.rs` itself is untouched —
  the backend-independent B-002 contract is unchanged.
- New crate `crates/ipod-sync` (`src/lib.rs`, `src/mounts.rs`,
  `src/discovery.rs`, `tests/discovery.rs`, `README.md`).

## API

- `mounts`: `MountEntry { mount_point, fstype, source }`,
  `MountSource` trait, `ProcMountInfo` (`/proc/self/mountinfo` reader with
  octal-escape decoding and a skip-malformed-lines tolerance policy;
  escapes are overflow-checked — `\400`+ is malformed and skipped, never a
  panic — and the `-` separator search starts after the mandatory fields,
  so a mount point literally named `-` parses), `MountError`.
- `discovery`: `IPOD_CONTROL_DIR`, `BackendStatus::{Loaded{abi_verified},
  Unavailable{reason}}`, `Candidate { mount_point, fstype }`,
  `BlockedMount { mount_point, error }`,
  `ScanReport { backend, candidates, blocked, appeared, removed }`,
  `Discovery::{new, with_backend_search, backend, scan}`.
- `gtk-library::ipod_discovery::apply_scan(&mut DeviceManager, &ScanReport)`:
  candidate + backend → `Connecting`; candidate + unavailable backend →
  `BackendUnavailable` with the reason as the diagnostic (device retained,
  sync disabled); removal → `Disconnected` + cleared device; blocked mounts
  and unrelated volumes leave the FSM alone; rescan with the same candidate
  is idempotent (does not clobber pending preflight diagnostics); a recovered
  backend re-enters `Connecting` (S-05 retry semantics). Never `Ready` —
  `set_ready_after_preflight` is not called anywhere.

## Verification criteria

Task-verification mapping (fixtures in `crates/ipod-sync/tests/discovery.rs`
over synthetic mount tables on temp dirs; glue tests in
`crates/gtk-library/src/ipod_discovery.rs`):

- *"A mount scan finds candidate `iPod_Control` directories"* —
  `fixture_detection_finds_ipod_control`, `fixture_multiple_candidates_are_sorted_and_deduped`.
- *"and passes them to the wrapper"* — the report carries candidates plus the
  wrapper backend status; the loaded backend handle is cached on
  `Discovery` (`env_gated_backend_loaded_reports_abi_status`, B-005 consumes
  `backend()`).
- *"it does not mark a candidate Ready"* — `glue_never_marks_ready` walks
  every scenario and asserts `state != Ready` after each step; detection only
  enters `Connecting` (`candidate_with_backend_enters_connecting`).
- *detection* — as above, plus `probe_detects_ipod_control_directory`.
- *missing backend* — `fixture_backend_missing_reports_unavailable_but_finds_candidates`
  (deterministic via `BackendSearch::Explicit(empty dir)`): candidates are
  found regardless, backend reported unavailable, no file-copy fallback
  exists; glue shows Sync unavailable
  (`candidate_without_backend_enters_backend_unavailable`).
- *permissions* — `fixture_permission_denied_mount_is_blocked` (skip-as-root):
  an unprobeable mount is reported as blocked, never silently dropped, never
  a candidate; `blocked_mounts_leave_manager_untouched` on the FSM side.
- *removal* — `fixture_removal_is_reported_on_rescan`,
  `fixture_reappearance_is_reported`,
  `removal_returns_to_disconnected_and_clears_device`,
  `reappear_after_removal_enters_connecting`.
- *unrelated volumes* — `fixture_unrelated_volumes_are_ignored`,
  `fixture_ipod_control_file_is_not_a_candidate`,
  `unrelated_volumes_leave_manager_untouched`.
- *mountinfo parser tolerance* — unit tests in `mounts.rs`: well-formed
  line, octal unescaping (`\040`/`\011`/`\012`/`\134`), the optional-fields
  separator, malformed lines skipped (too short, missing `-`, non-octal
  escapes, overflowing octal escapes like `\400` — dropped, never a
  panic), and a mount point literally named `-` parsing as the mount
  point, not the separator.
- *no initialization or capability-file writes during discovery* —
  `fixture_scan_never_writes_to_candidate_tree` (tree hash before/after a
  scan and a rescan) and the same assertion inside the env-gated test with
  the backend loaded (dlopen included). Structural guarantee: the scan path
  contains no parse, no open, no mkdir.
- *real device trees* — two env-gated gates beyond the synthetic fixtures:
  `env_gated_restored_snapshot_is_detected_and_unmodified` (`IPOD_TEST_TREE`
  = the restored-device snapshot, whole-tree hash) and
  `env_gated_mounted_device_is_detected_and_unmodified` (`IPOD_TEST_MOUNT` =
  a mounted p2 of `ipod/test-device-30g.img`; the full production path
  through the real `/proc/self/mountinfo` source). The mount gate hashes the
  whole `iPod_Control/` subtree — on the Video layout that includes the
  media library (23G on the 30g image) — the strongest no-write proof,
  deliberately sized for the 30g image.
- *multiple matching devices (first-wins, owner-approved 2026-11-09)* — the
  report lists every candidate sorted; the FSM follows the first. Tested at
  both levels: `fixture_multiple_candidates_are_sorted_and_deduped` (report),
  and in the glue `multiple_candidates_only_first_gets_fsm`,
  `device_handover_when_active_removed`,
  `non_active_removal_is_invisible`, `lower_sorting_candidate_takes_over`
  (first wins, not first seen — plugging in a device that sorts before the
  current one takes the FSM over, deliberately).

## Results

Test matrix (2026-11-09):

- `ipod-sync`: 11 unit + 12 integration tests, green without any libgpod and
  green with `RUFIN_LIBGPOD_DIR` pointing at the pinned install (the
  env-gated test asserts `Loaded` with `abi_verified` mirroring the marker
  file next to the library). Two real-tree gates: the restored-device
  snapshot through a synthetic mount table, and a mounted device image
  through the real `ProcMountInfo` source — both detected, both
  byte-identical after the scan.
- `gtk-library`: 70 tests green (57 pre-existing + 13 glue, incl. the
  first-wins multi-device semantics).
- `ipod` wrapper regression: 45 unit + 10 integration green (unchanged).
- `cargo check --workspace`, `cargo fmt --check`, `git diff --check`, clippy
  (`ipod-sync`, `gtk-library` all-targets), rustdoc clean.
- Dev scenarios scripted: `scripts/ipod-dev` (test-synthetic, test-wrapper,
  test-snapshot, rebuild-snapshot, test-mounted, test-full, lint) plus the
  `just ipod-test` recipe. All scenarios live-verified on the host: the
  mounted gate runs the real losetup/mount/treehash/cleanup cycle, and the
  rebuild reproduces the canonical 328M snapshot (selective set per
  itl-player docs/LIBGPOD.md, seeded SysInfo) from the 30g image.

## Follow-ups

- B-005: replace the placeholder `DeviceInfo` (empty name/model/generation)
  via `set_device` after metadata retrieval; consume `Discovery::backend()`.
- B-006: observer stream / UI binding; periodic or event-driven re-scan
  policy.
- Multiple simultaneous candidates: first-wins by sorted mount point is the
  deliberate policy (owner-approved), with the steal/handover/invisible
  semantics tested in the glue. No revisit planned.
- tasks.md checkoff stays the owner's commit, per the B-007 convention.
- Packaging strict-marker follow-up (b007 delta §5) unchanged.
