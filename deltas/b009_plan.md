# B-009: Dry Run delta calculation — implementation plan

Status: **approved by the owner (2026-11-13)**; the §9 decisions are resolved
and folded into the body below. Task branch: `feature/ipod-page-task-B-009`,
based on `feature/ipod-page` at `35db42f`.

## 1. Goal and boundaries

B-009 (`tasks.md`, Phase 2) computes the sync delta without changing anything:
it reports what a sync **would** do — files, playlists, artwork, statistics —
while provably writing nothing to the device and nothing to the host `.itl`.
Traceability: `spec.md` R-04/S-08; `plan.md` Phase 2 ("Calculate dry-run
deltas without changing the device, consuming playback statistics, or writing
the host `.itl`") and Testing Strategy. The B-008 mapping output is the input;
the report is the input contract for B-010 (writes) and B-011/B-015 (UI).

This task does **not**: write to the device, write to the host `.itl`, consume
or import `Play Counts`, implement stat-merge policy (B-010), translate smart
criteria into device SPLPREF form (B-010), copy files, or bind UI. Accuracy of
the report against a real subsequent sync is B-018's acceptance, per-model
hardware acceptance is B-017; the report itself must be exact against its own
fixtures. `sync-writes` stays unreachable. Tests use synthetic inputs or
disposable copies; never the live iPod or the production `.itl`. Do not touch
the deferred artwork proof-read fix branch.

## 2. Research gate: R1 revisited for dry-run isolation

R1 (`deltas/b001_research.md`) asks for the complete side-effect inventory of
`itdb_write_file()` and for how dry run avoids live-device effects. The
resolution for B-009 is structural, and the write-path inventory itself stays
open for B-010:

**Dry run never calls any libgpod write API.** The delta is a pure comparison
of owned parse results. There is no point in the pipeline where
`itdb_write_file` (or `itdb_init_ipod`, `itdb_write*`, hash-info extraction,
stat-file cleanup) is reachable: the engine consumes owned inputs (see §4/§5)
and the collectors use only the read-side `DeviceSession` surface that B-007
already proved non-mutating on well-formed trees (R-06/S-18, tree-hash
fixtures). R1's write-path findings therefore gate B-010's backups/write
ordering, not B-009 — recorded, not resolved, in the R1 note (§8 item 1).

**The one read-side residual is inherited, not new.** B-007 §1.1 established
that libgpod parse paths can `g_mkdir` a missing `Artwork/` on some trees.
Dry run opens the same session the preflight already opened (dry run requires
Ready), so on a live device it adds nothing beyond the existing read path; the
zero-mutation proof runs on disposable well-formed snapshots, exactly as the
B-009 verification wording requires ("potentially mutating parse operations
run only on disposable snapshots").

**Playback statistics are not consumed.** The wrapper's `playback_stats()`
re-reads `tracks()` and never touches the on-pod `Play Counts` file
(`crates/ipod/src/session.rs`). Dry run reports the file's *presence* only
(§4), with import semantics left to B-010 — so no libgpod stat-import API
(`itdb_playback_*`) is called anywhere in B-009.

Consequence for design: isolation is enforced twice — structurally (the pure
engine cannot write, it has no session, no `PathBuf` destinations, no
filesystem calls) and by measurement (before/after tree hash on the disposable
device copy, before/after byte hash of the disposable `.itl` copy, §6).

## 3. Inputs available and gaps to close

**From B-008 (merged):** `host_library` / `host_tracks` / `host_playlists` /
`resolve_playlist_members` (owned host records, pure reads of a disposable
`.itl`), `map_tracks` / `map_playlists` (PID↔dbid and name+kind matching with
explicit `NoIdentity` / `MissingOnDevice` / `MissingOnDevice` playlist
outcomes, device-only reports, diagnostics). Nothing here decides a sync
action; B-009 consumes it exactly as documented.

**From the wrapper (read side):** `DeviceSession` `device_info`, `tracks`
(`TrackInfo`: dbid, `ipod_path` colon-form, `filetype`, `size`, `bitrate`,
metadata, raw stats, `has_thumbnail`, `mediatype`), `playlists`
(`PlaylistInfo`: id, name, `is_master`, `is_spl`, `member_dbids`),
`artwork_status` (`ArtworkVerdict`, thumbnail counts, proof-read),
`DeviceFacts` (retrieval: measured capacities).

Gaps B-009 must close, all read-only:

- **A — device smart criteria.** `PlaylistInfo` carries no `mhod`-51 payload,
  so a criteria diff is impossible today. Wrapper extension: a read-only
  `smart_blob: Option<Vec<u8>>` (verbatim SLst payload of `mhod` type 51),
  parsed in `ipod-sync` with the existing `parse_smart_rules`. FFI/A-B-I care
  per B-007 conventions (owned bytes, layouts.rs, ownership tests, tree-hash
  read proof); no raw FFI outside `crates/ipod`.
- **B — host artwork evidence.** `HostTrack` carries none (B-008 left the
  extraction trace open, §7 of its plan). The production embedded-cover
  scanner lives in `crates/sources` (`file/local/artwork::inspect_embedded`,
  already used by the iTunes source in 64-file batches). Evidence carries
  **presence plus the cover's byte size** — the capacity formula (§4) needs
  the size term, not just a boolean. Options and recommendation in §9-1.
- **C — on-pod `Play Counts` presence.** Reported as informational metadata
  only (`fs::metadata` on the mount path); no content parse.
- **D — sync options.** `SourceKey` (owner decision, B-008 §9: the sync
  engine takes an explicitly selected source, never an implicit choice),
  the transcode flag (owner fact: "Convert higher bitrate songs to AAC
  256 kbps" — B-008 §2a(c) assigns the option to B-009/B-010), and the
  device-only policy (§4, default `Remove` per owner decision §9-3). All
  caller-supplied; no UI, no config discovery in this task.

## 4. Delta model

The report is one owned, deterministic, serializable structure
(`DryRunReport`), pure data: no clock reads, no timestamps, all itemized
lists sorted by stable keys (`itl_id` for tracks, playlist name+pid for
playlists), counts derived from the lists. It contains:

**Context.** Library PID, device name/model (from `DeviceFacts`), the echoed
`SyncOptions`, and the artwork verdict (`artwork_status`) — an artwork verdict
other than `Ok` gates the artwork section with a diagnostic instead of a
delta.

**Capacity.** Free bytes vs. the sum of the per-entry estimates for `FileAdd`
and `FileRepair` entries; a `would_fit` verdict. Estimate per entry (owner
decision §9-4): `Copy` mode = the exact host file size; `TranscodeToAac`
mode = `duration_ms / 1000 × 32000 + embedded-cover bytes + small moov
overhead` (32000 = nominal 256 kbps in bytes/s; a small fixed overhead, e.g.
8 KiB, covers the moov atoms). The transcode formula is a conservative upper
bound (real AAC 256 VBR lands at or below the nominal rate), so the verdict
wording distinguishes the two failure directions: fits = "fits even by the
conservative estimate"; not fits = "estimate exceeds free space" — never a
promise of encode success or failure.

**Tracks (files).**

- `FileAdd { itl_id, pid, host_path, mode: Copy | TranscodeToAac, size_estimate }`
  for host tracks with outcome `MissingOnDevice` and a `Mapped` location whose
  file exists on the host disk.
- `FileRepair { dbid, device_path, host_path }` for matched tracks whose
  `ipod_path` (colon-form converted to a filesystem path under the mount) does
  not exist on the pod — the database lists the track but the file is gone.
- Device-only dbids: with policy `Remove` (the default, owner decision
  §9-3 — delete what the host library does not contain, like iTunes) each
  becomes a `FileRemove { dbid, device_path }` action entry; with policy
  `Keep` they are a reported informational list (`DeviceOnlyTracks`). Write
  behavior stays B-010; B-009 only renders the intended action. Removal
  entries are destructive by construction — the report's type system keeps
  them in separate lists so the UI (B-015) can render them distinctly.
- Host tracks with `NoLocation` (cloud/purchased) or `OutsideMediaFolder` are
  **never** candidates and are never reported as removals (B-008 §9); a
  `Mapped` track whose host file is missing on disk is a diagnostic
  (`HostFileMissing`), not a silent skip.
- `NoIdentity` host tracks are diagnostics, never actions.

**Tracks (statistics).** Per matched pair, a `StatDiff` whenever any raw field
differs: rating, play_count, skip_count, play date, volume adjustment —
`{ pid, field, host_raw, device_raw }`, plus aggregate counts per field and
direction. B-009 is directionally neutral on purpose: the merge policy is
B-010's decision (B-008 §5), so the report says "differs on N tracks" with
both raw values, and explicitly does not label push/pull. The report is still
actionable for the UI: counts and itemized values are all a policy needs.

**Playlists.** For each host playlist:

- `MissingOnDevice` → `PlaylistCreate { host_pid, name, kind: Static | Smart,
  member_pids, criteria }`.
- `Matched { kind_agrees: false }` → `PlaylistKindMismatch` diagnostic
  (never a silent conversion — B-008 §7).
- `Matched { kind_agrees: true }` → member diff against
  `resolve_playlist_members` (ordered, duplicates preserved) vs. device
  `member_dbids`: `members_add`, `members_remove`, `members_reorder`
  (order-only change, no adds/removes), or `members_equal`. Smart playlists
  additionally get a criteria comparison (§ below) and `smart_info_present`
  (informational: Smart Info → SPLPREF translation is B-010).
- The device master (`device_master`) is device-owned and informational in
  every policy — never a removal candidate. Other device-only playlists are
  classified first: **device-local** playlists (what iTunes itself preserves
  on the pod — `On-The-Go 1` and any playlist the device marks as
  device-managed; identified from the `mhyp` header flags, owner decision
  §9-3) are informational in every policy. Non-local device-only playlists
  become `PlaylistRemove` action entries under the default `Remove` policy
  and informational entries under `Keep`. Classification evidence is a
  research item of this task (§8 step 2a), never a name guess.

**Smart criteria comparison.** Host rules (nested `SLst`, parsed) vs. device
blob (flattened, parsed). Because iTunes flattens nested groups on device
write (B-008 §7), structural equality alone would flag every nested playlist
on an in-sync pair as "changed" — a false positive. B-009 therefore implements
the **flattening normalization** as a pure function: host nested rules →
the flat form iTunes produces (the group `(video kind IS 1) OR (video kind IS
0x20)` → `video kind binary-AND 0x21`; companion leaves as-is; B-008 §7
documents the exact observed mapping). The snapshot pair is the ground truth
for this function: every device `mhod`-51 blob is iTunes' own flattened
output. Comparison is then flattened-host vs. device: `CriteriaEqual`,
`CriteriaDiffer` (report a criteria-update entry), or `CriteriaUnknown`
(unparseable device blob or `Undecided` constructs — diagnostic, never a
silent conversion to static; B-008 §7).

**Artwork.** Per matched track, host evidence (cover present/absent with byte
size, §3-B) vs. device `has_thumbnail`:

- Host has cover, device lacks thumbnail → `ArtworkAdd { pid }`.
- Device has thumbnail, host evidence absent → `ArtworkRemove { pid }` under
  the default `Remove` policy, informational under `Keep`. Honest caveat:
  iTunes can write device thumbnails from its own art cache even for tracks
  with no embedded cover, so an in-sync pair legitimately shows such
  removals — the pair gate asserts the count equals the computed set, and
  the finding is surfaced for B-010's write-policy review.
- Evidence unknown (provider returned `Unknown`) → informational count, no
  action.

**Statistics import.** `PlayCountsFilePresent { path }` (informational, from
presence check); the import itself, its semantics and its safety are B-010.

**Diagnostics.** The mapping diagnostics are echoed verbatim, plus B-009's own
(`HostFileMissing`, `CriteriaUnknown`, `ArtworkVerdictGated`, capacity
`WouldNotFit`). Diagnostics are findings, never failures of the dry run
itself.

## 5. Proposed architecture

All new code in `crates/ipod-sync` (pure) plus one read-only wrapper
extension in `crates/ipod`. Module layout:

- `ipod-sync/src/dry_run/model.rs` — `SyncOptions { source_key, transcode_aac,
  device_only: DeviceOnlyPolicy }` with `DeviceOnlyPolicy::Remove` the
  default (owner decision §9-3), `CoverEvidence { Present { bytes }, Absent,
  Unknown }`, `DryRunReport` and every entry type from §4.
- `ipod-sync/src/dry_run/compute.rs` — the pure engine:
  `compute(host: &HostInputs, device: &DeviceInputs, opts: &SyncOptions) ->
  DryRunReport`. No I/O, no session, no `PathBuf` writes, no clock. All
  comparisons on owned values in raw Apple-epoch space (B-008 §6: conversion
  happens only at the `itl-sync` boundary — the report carries raw values
  verbatim).
- `ipod-sync/src/dry_run/flatten.rs` — the SLst flattening normalization
  (§4), pure, tested against the snapshot pair's device blobs.
- `ipod-sync/src/dry_run/snapshot.rs` — the only I/O: `collect_host`
  (disposable `.itl` copy via the B-008 host adapters + the caller-provided
  artwork evidence with cover sizes + host-file existence probes) and
  `collect_device` (read-side session calls + `DeviceFacts` + `Play Counts`
  presence + per-track file-existence probes under the mount). Host and
  device collection are separate functions so the engine and the fixtures
  can compose them independently.
- `crates/ipod` — `PlaylistInfo.smart_blob: Option<Vec<u8>>` (read-only,
  verbatim `mhod`-51 payload; extraction alongside the existing playlist
  parse; owned bytes, no pointer escapes) and the `mhyp` header flag bytes
  (`flag1`/`flag2`/`flag3`, read-only) for the device-local playlist
  classification (§4).

`HostInputs`/`DeviceInputs` are the owned snapshot records the engine needs:
the B-008 records plus B-009's additions (artwork evidence, smart blobs,
existence probes, capacities, `Play Counts` presence). The mapping is
recomputed inside `compute` from these inputs (deterministic, matches B-008's
functions exactly) rather than stored, so the engine has a single input shape.

No new dependency for the engine. The env-gated integration test's artwork
provider may add a **dev-dependency** on `crates/sources` (workspace path
dep, no cycle: `sources` does not depend on `ipod-sync`) — see §9-1.

## 6. Isolation and non-mutation verification

1. **Structural:** `compute` takes owned inputs and returns an owned report;
   it cannot open a session, cannot construct a write destination, performs no
   filesystem or clock access. Clippy/API review confirms no `std::fs` write
   calls in `dry_run/`. `sync-writes` is not enabled by any B-009 path.
2. **Unit fixtures:** synthetic `HostInputs`/`DeviceInputs` with exact
   expected reports (§8) — including the no-action case, so a report that
   invents changes fails loudly.
3. **Env-gated gate (the B-009 verification core):** disposable copies of the
   device tree (`IPOD_TEST_TREE`) and the host `.itl` (`ITL_TEST_PATH`), pinned
   `RUFIN_LIBGPOD_DIR`. Full dry run; then byte-hash the whole device tree
   (files and directories, the same tree-hash method B-007 uses) and the host
   `.itl` copy before and after — both must be identical, including
   `Play Counts` and every auxiliary file. Explicitly assert: no file created,
   deleted, or mutated on the device side; host `.itl` bytes unchanged.
4. **Pair expectation (accuracy smoke):** the snapshot pair is an in-sync
   iTunes dump, so the gate asserts the report is a (nearly) no-op: zero
   `FileAdd`/`FileRepair`/`PlaylistCreate`/`FileRemove` (the pair has no
   orphan dbids), zero playlist member diffs for the flat playlists, zero
   `CriteriaDiffer` and zero `CriteriaUnknown` after flattening (this is also
   the flattening function's ground-truth test), and zero `PlaylistRemove`
   after the device-local classification (an in-sync iTunes dump must have
   none). The known benign divergences: «Самые популярные»'s stale
   materialized list is *equal* on both sides (both hold the old 25), so it
   correctly produces no action; stat diffs are permitted only in the count
   ranges B-008 measured; `ArtworkRemove` entries are permitted only where
   host evidence is `Absent` and the device carries a thumbnail, and their
   count must equal the computed set (the iTunes art-cache caveat, §4).
   A wrong report (e.g. 3373 re-copies) fails the gate.
5. **Backend-disabled:** with no backend the collector reports unavailable
   (same contract as B-003/B-006); never a silent empty report.

## 7. Traceability and non-goals recap

S-08's three prohibitions map to: (a) no device change — §6 items 1–4;
(b) no playback-statistics consumption — §2 (presence-only `Play Counts`,
no stat-import API); (c) no host `.itl` write — the collectors read a
disposable copy and the `.itl` byte-hash assertion. The report is the input
contract for B-010/B-011/B-015; nothing in this task pre-decides their
policy.

## 8. Work sequence and verification

1. Append the R1 resolution note to `deltas/b001_research.md` (precedent:
   R3's "Resolved" note): dry run resolved structurally for B-009; the full
   `itdb_write_file` side-effect inventory remains a B-010 gate.
2. Wrapper extension: `smart_blob` + `mhyp` flag bytes with ownership/layout
   tests and a tree-hash read proof on the device snapshot; verify
   `itdb_parse` behavior on the fixture tree is unchanged.
2a. Device-local playlist classification (owner decision §9-3): inspect the
    `mhyp` headers in the snapshot pair and the pinned libgpod source to
    determine how device-managed playlists (`On-The-Go 1`, master, possibly
    others) are marked; record the evidence in this delta; implement the
    classification as a pure function over the wrapper's flag bytes — never
    a name guess. The pair gate then proves zero `PlaylistRemove` on the
    in-sync dump.
3. `flatten.rs`: implement the normalization from B-008 §7's observed
   mapping; unit tests from the snapshot's own blobs (host nested → device
   flat byte-exact); plus fixtures for leaves-only, multi-group and unknown
   constructs (unknowns stay explicit).
4. `model.rs` + `compute.rs`: implement §4; pure unit fixtures covering every
   entry type, every outcome variant, policy `Remove` vs `Keep` (Remove the
   default), `NoIdentity`/`NoLocation`/`OutsideMediaFolder`/`HostFileMissing`
   handling, the transcode capacity formula (Copy exact size vs AAC-256
   estimate with cover term), `WouldNotFit`, artwork gating by verdict,
   device-local playlist exemption, deterministic ordering (shuffled inputs →
   identical report).
5. `snapshot.rs`: collectors; unit tests with temp-tree fixtures for
   path-colon conversion and existence probes.
6. Env-gated gate per §6 (pair expectation + tree hash + `.itl` hash), with
   skip messages pointing at `scripts/build-libgpod.sh` (B-008 §8 item 7
   convention).
7. Full verification: `cargo test -p ipod-sync` (unit), the env-gated gate,
   `./scripts/ipod-dev test-synthetic`, `test-wrapper`, `lint`, `git diff
   --check`. Record actual results; request review before merging.

## 9. Decisions (resolved by the owner, 2026-11-13)

1. **Artwork evidence provider (gap B).** Resolved: the engine takes a
   caller-provided `ArtworkEvidence` map carrying presence and cover byte
   size; the env-gated test builds it with `crates/sources`'
   `inspect_embedded` via a dev-dependency (production wiring at the sync
   boundary, B-010+).
2. **Criteria comparison via flattening normalization.** Resolved:
   implement `flatten.rs` in B-009 with the snapshot pair as ground truth
   (it is B-010's translation prerequisite anyway); the pair gate then
   requires zero `CriteriaDiffer` and zero `CriteriaUnknown`.
3. **Device-only policy default.** Resolved: delete tracks and playlists
   that are not present in the host library, except the cases the device
   owns locally — the on-the-go playlist (and possibly some others) — alike
   original iTunes. Default policy `Remove`; device-local playlists are
   classified from the `mhyp` flags (§8 step 2a) and never removed; the
   master is never removed.
4. **Transcode size estimate.** Resolved: the capacity estimate is
   `duration × 32000 + measured cover bytes + small overhead for the moov
   atoms` (owner formula); `Copy` mode uses the exact source size; the
   formula is a conservative upper bound and the verdict wording reflects
   that (§4).
5. **Statistics directionality.** Resolved: directional-neutral raw diffs
   now (merge policy is B-010's, per B-008 §5); the report carries all
   values a later policy needs.
6. **`Play Counts`.** Resolved: presence-only informational in B-009; no
   parse, no import, no libgpod stat API.
7. **R1 note placement.** Resolved: the dry-run resolution is appended to
   `b001_research.md` R1 on this task branch (R3 precedent), keeping the
   write-path inventory open for B-010.
