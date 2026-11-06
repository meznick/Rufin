# Decision delta: B-001 — required libgpod backend and safety corrections

Date: 2026-11-06. This corrected delta replaces the rejected filesystem-only proposal in
this artifact; it is not a claim that the earlier proposal was safely implemented. The
[owner decision](research-based-decisions.md) is authoritative. Mainline requirement
corrections were committed in `83e0008` on `fork-master` and integrated into B-001 by
`de4fa64`; implementation remains pending. The [preflight delta](s03_b004.md) records the
associated behavioral correction. No new workflow exception is used.

## MODIFIED

### Required backend (constitution §3; R-02, R-04)
Previously the plan assumed an available Rust `libpod` dependency; a follow-up proposal
incorrectly recommended file copying with optional libgpod fallback. Now libgpod is required
behind a small safe Rust FFI wrapper. Rufin owns policy, deltas, progress, recovery, and
host updates through itl-sync. Builds without the backend remain possible; they cannot sync.

Given a mounted supported device but a missing/disabled/incompatible backend, when Rufin
attempts connection, then show Sync unavailable and keep sync disabled, without copying
files as a fallback (S-05).

### Readiness and metadata (R-02, R-03)
Given a mounted device, when preflight runs, then do not declare Ready until backend,
access, readable databases, measured capacity, model/artwork capabilities, and checksum
prerequisites are verified (S-03). A failed check produces a diagnostic and disabled sync.
There is no Trust/pairing step. Missing firmware is displayed as unavailable, not invented
and not itself a readiness failure (S-06).

### Multi-file safety and stat imports (R-04, R-05)
Given an interrupted sync, when the device returns, then recover a consistent library from
durable recovery information before another destructive write, and report the operation as
incomplete until verified (S-12). Start/stop-sync is not a transaction or lock.

Given device stats pending import, when a device write could consume their files, then
preserve pending updates durably first. Given interruption or host write refusal, when
retrying, then retain pending changes without losing or duplicating imports, including
reassigned device numeric IDs and host commit before acknowledgement (S-13). Partial
success is not a completed sync (S-10).

Given Dry Run is selected, when differences are calculated, then do not mutate device files,
consume stats, or write the host `.itl` (S-08).

## REMOVED

The rejected filesystem-only fallback, iOS authorization state, and false start/stop-sync
transaction guarantee are not current instructions. Given an attached Video/Classic iPod,
when discovered, then no authorization prompt appears and a mount alone does not enable
Sync. Given backend absence, then no fallback copy is performed.

The untracked device-manager draft is not verification evidence and is not included in
B-001. The earlier nonexistent transport-delta reference is removed rather than replaced
with another dangling document.

## Artifact alignment and verification

[Follow-up research](b001_research.md#10-follow-up-research-required-before-implementation)
is still required on write-file side effects, artwork error reporting, and the unresolved
device-name provenance discrepancy before their respective implementation tasks. The
alignment summary below does not claim those items are resolved.

[Research](b001_research.md) is corrected in place, including metadata location, unsafe
SysInfo reload, nominal versus measured capacity, Unix API timestamps, and limits of
declaration-only verification. [Plan](../plan.md) uses one FSM/DeviceInfo contract and places
FFI integration in Phase 1. [Tasks](../tasks.md) retain stable IDs but order B-002 → B-007 →
B-003 → B-005 → B-004; they add dry-run nonmutation, recovery/deduplication, and per-model
acceptance criteria. B-001 stays unchecked until reviewed/merged and checked off on the
feature branch. B-020 targets local feature artifacts, not a nonexistent Rufin NOTES.md.

Source verification uses the pinned references in the [owner decision](research-based-decisions.md#verified-upstream-references).
The earlier 65/65 API declaration check did not establish safe ownership, recovery,
transactions, or physical acceptance. No source implementation or hardware testing is
claimed by this delta.

## Remaining implementation gates

Loading/linking and packaging need owner review before B-007; runtime loading is not assumed.
The concrete multi-file recovery, competing-writer exclusion, stat-merge/idempotency, and
flush/eject protocol must be reviewed and verified in B-010 before enabling destructive
sync. Physical model/checksum/artwork/stat acceptance and subsequent iTunes compatibility
are B-017. These are tracked future work, not unresolved backend choices or prerequisites
for defining the backend-independent B-002 types after B-001 is approved and merged.
