# Delta Spec: B-007 — safe libgpod wrapper integration (research gate closure + wrapper design)

**Task**: B-007: Integrate the safe libgpod Rust wrapper
**Spec reference**: R-02, S-05, R-04; new R-06 (below); constitution §3
**Plan reference**: Phase 1, Data Models & Interfaces, Testing Strategy, Risks and Tradeoffs
**Date**: 2026-11-08
**Status**: approved by owner 2026-11-08 as the B-007 research-gate closure; wrapper implementation remains pending

This single file closes the B-007 research gate (R1 and R2 of
[B-001 follow-up research](b001_research.md#10-follow-up-research-required-before-implementation)),
records the backend pin, carries the delta sections (ADDED / MODIFIED / REMOVED), and
preserves the owner's wrapper sketch as an appendix (§7). The standalone B-007 research
notes and the wrapper sketch that fed this file are folded in here and are not committed
separately. Companion evidence in the itl-player repo: pinned libgpod 0.8.3 build recipe
(`docs/LIBGPOD.md`), the read-only harness (`tools/libgpod_check.c`), and the
restored-device snapshot cross-checks (`NOTES.md` §20), all 2026-11-08. No hardware
acceptance is claimed here; physical per-model acceptance is B-017.

## 1. Research gate closure

### 1.1 R1 — `itdb_write_file()` side effects: CLOSED

Mechanism: the pinned upstream `itdb_write_file_internal` writes artwork against the
database's device context before writing the requested file; after successful file output
it calls `playcounts_reset` on the device mountpoint, which resolves and unlinks
`Play Counts`, `iTunesStats`, and `PlayCounts.plist` relative to that mountpoint. The
code also has model-dependent SQLite, Shuffle, and checksum paths. An output-path change
alone does not isolate a device tree.

Fixture experiments (2026-11-06, disposable trees; Arch libgpod `0.8.3-20` extracted under
`/tmp/opencode`, package SHA-256 `7525ccde…` — a distro build, superseded as the reference
by the pinned build in §1.3). The fixture was created from scratch with `itdb_init_ipod`
using Video model `MA446`; no device or production library was touched. The harness
compared SHA-256 for every regular file in the fixture and the external output directory
before and after each operation; empty-directory and metadata-only changes were not
captured — W-5 below strengthens this to a full tree hash including directories.

Results: with a parsed empty fixture and all three sentinel files at the mount root, a
successful external `itdb_write_file` created the output and removed all three sentinels.
With a nonexistent output parent and a missing ArtworkDB, the call returned failure but
created `iPod_Control/Artwork/ArtworkDB` first. With a single track and generated artwork,
both successful and failed external writes changed the device tree's ArtworkDB.

Synthetic Classic probes (not valid Classic acceptance): a copy of the Video fixture had
its SysInfo changed to `MB029`, with and without a fabricated FireWire ID. With the ID,
the external write succeeded, changed ArtworkDB, and removed a root `Play Counts`
sentinel. Without it, the write failed with a missing-ID error but still changed
ArtworkDB. Separately, a synthetic `MB029` fixture with a fabricated ID and newly
generated Classic-format thumbnails produced four `.ithmb` files; both successful and
failed external writes changed its ArtworkDB. (The missing-FireWire-ID write gate itself
is already recorded in tasks.md B-004 and [s03_b004.md](s03_b004.md).)

Parse-side mutation (source evidence, not a reproduced Video/Classic fixture result):
the pinned `itdb_parse_internal` also invokes `itdb_hash72_extract_hash_info`; on
HASH72/SQLite devices that code can create `Device/HashInfo`, and its `read_hash_info`
helper can unlink an existing correctly sized file whose UUID mismatches. Treating every
parse as intrinsically non-mutating is therefore unsafe.

Additional finding from the real snapshot (2026-11-08): `itdb_parse()` on a tree whose
`Artwork/` directory is missing attempts `g_mkdir()` to create it
(`ipod_db_get_artwork_db_path`). On the real restored pod `Artwork/` always exists, so
this did not fire; on a rw-mounted foreign tree it would.

**Consequence for the wrapper:** no API implies that an output filename confines side
effects. Rather than rewriting or forking libgpod, minimal known side effects on
disposable or backed-up contexts are tolerated, and the wrapper makes them explicit.
Read paths run only against disposable snapshots or verified device trees; B-007 exposes
no destructive write path for sync (§2 W-7); dry-run isolation is B-009; the concrete
backup/recovery design remains B-010. Future dry-run experiments use a complete
disposable copy of the device context and compare the entire tree.

### 1.2 R2 — artwork errors and apparent success: CLOSED (with a new model gate)

Mechanism: the pinned `itdb_parse` ignores the artwork parser result, and the write path
ignores the artwork writer's result; in the pinned `db-artwork-writer.c`,
`ipod_buffer_destroy` also discards the result of the final buffer flush.

Fixture experiments (same setup as §1.1): on a one-track Video fixture with generated
`F1028_1.ithmb` and `F1029_1.ithmb`, removing or corrupting ArtworkDB still allowed
`itdb_parse` to return a database with one track but no attached thumbnail. A subsequent
external `itdb_write_file` returned success, rewrote or created ArtworkDB, and removed
both `.ithmb` files. Removing one `.ithmb` instead also allowed parse to succeed and
report an attached thumbnail; the probe did not establish that its image data was
readable. Replacing ArtworkDB with a directory after a successful parse made the
external database write report success even though ArtworkDB remained a directory.
These are fixture observations, not a proof of all artwork failure modes.

New finding from the real snapshot: artwork can be silently skipped even on a perfectly
healthy device. The restored pod's `SysInfo` is a 0-byte file (confirmed iTunes 12.13
behavior: it stays empty after restore + sync). Without a `ModelNumStr`,
`itdb_device_get_ipod_info()` returns Invalid and `itdb_device_supports_artwork()`
returns FALSE, so `ipod_parse_artwork_db` exits early: `itdb_parse()` succeeds with
3373 tracks / 9 playlists but **zero** artwork attached, with no error. Seeding the
snapshot's `SysInfo` with `ModelNumStr: MA446` restores model A446 / generation VIDEO_2
and parses 3356 tracks with thumbnails — matching the ArtworkDB entry count and our own
independent ArtworkDB parser. A successful parse therefore never implies successful
artwork handling, and an unknown model must never be treated as "artless".

**Chosen validation strategy:** the wrapper reports artwork state explicitly (§2 W-4) —
parsed thumbnail coverage compared against the ArtworkDB's declared entries via an
independent count, plus a proof-read of at least one thumbnail per format when declared.
Full per-thumbnail image verification stays out of B-007 scope; a legitimately artless
device must not be reported as corrupt either — the verdicts distinguish "no entries
declared" from "entries declared but not parsed".

### 1.3 Backend pin and build evidence

- **Version**: libgpod **0.8.3** (last upstream release). Source: sourceforge tarball
  sha256 `638a7959d04e95f1e62abad02bd33702e4e8dfef98485ac7d9d50395c37e955d` + the
  8-patch set of Arch `libgpod 0.8.3-20` (packaging commit `a74dc08b`, patch sha256s in
  `../itl-player/docs/LIBGPOD.md`). The earlier 4-patch pkgrel-15 set does **not** build
  with gcc 14+ (incompatible-pointer-types is an error); the -20 set fixes it.
- **Build features**: `--enable-udev --with-mono=no --with-python=no
  --without-libimobiledevice`, gdk-pixbuf and libxml enabled, udev-dir scoped to the
  install prefix, CFLAGS `-Wno-incompatible-pointer-types -Wno-implicit-int` (recipe and
  dependency matrix in `../itl-player/docs/LIBGPOD.md`). iPhone/iPod-touch code paths are
  compiled out; Video/Classic need no libimobiledevice.
- **Read-only function check on the real restored-device snapshot** (copies from the
  test-device image, never the live pod; harness `../itl-player/tools/libgpod_check.c`):
  `itdb_parse` → version 0x73, tzoffset +10800, 3373 tracks / 9 playlists, smart
  playlists detected, Russian names intact; track stats bit-exact against the host `.itl`
  mith fields for 3/3 spot tracks (rating, playcount, soundcheck 1889/13244/6231,
  volume); `time_played` is Unix seconds in local wall-clock time = `.itl` 1904 value −
  2082844800; `explicit_flag` is u16 LE at mhit 0xA6 with the low byte equal to the
  `.itl` content-rating byte — independent confirmation of both offsets. Decoded cover
  art is pixel-identical to our independent RGB565-LE decoder (PSNR ∞) after fixing two
  bugs in our own tool (BMP BGR order, art-id addressing) — documented in
  `../itl-player/NOTES.md` §20. Endianness gotcha for the wrapper: libgpod's `db->id` is
  the byte-reverse of our device GUID reading (`4c88c91a643b3f7a` = rev
  `7a3f3b641ac9884c`).
- **Loading**: runtime loading of the separately packaged backend, per the owner
  direction (the preliminary wrapper direction is preserved in §7.1). A library SONAME
  or a successful symbol lookup alone is not proof that public struct layouts are
  compatible; the packaged ABI/SONAME policy and its verification remain a packaging
  follow-up (§5), not a blocker for the wrapper surface below.

## 2. ADDED

### R-06: Safe backend wrapper and non-mutating reads

#### W-1: Backend loading and unavailability

Given Rufin needs the backend, when the shared library or a required symbol is missing,
incompatible, or fails to load, then `IpodBackend::load()` returns an actionable
`BackendUnavailable` result and never panics, crashes, or falls back to file copying.
(S-14, ties to spec S-05.)

- Given a mounted supported iPod but an unavailable backend, when connection is
  attempted, then the FSM shows Sync unavailable with the backend diagnostic and keeps
  sync disabled.

#### W-2: Session surface (read side)

Given a mount point, when `open()` succeeds, then `DeviceSession` provides owned results
for `device_info`, `tracks`, `playlists`, `artwork_status`, and `playback_stats`, each
returning structured `IpodError` on failure instead of exposing native pointers, GLib
lists, or `GError` handles. All borrowed native strings/lists are converted to owned
Rust values before any call returns; `itdb_free` is paired with the session drop.
(S-15.)

- Given a parseable fixture, when `tracks()` is called, then counts and field values
  match the fixture (3373 tracks / 9 playlists on the real snapshot).
- Given a parse failure, when a read call is made, then the session reports the
  diagnostic without leaking or double-freeing native memory.

#### W-3: Model identification is honest

Given a device whose `SysInfo` is empty or unreadable, when `device_info()` runs, then
the model is reported as unknown (`None`), never silently resolved to a default model,
and the FSM must not reach Ready on that basis alone (spec S-03/S-05; readiness is
B-004). (S-16.)

- Given a fixture with `ModelNumStr: MA446`, when `device_info()` runs, then model A446
  and generation VIDEO_2 are reported.
- Given an empty `SysInfo`, when the wrapper parses the tree, then the silent
  artwork-skip behavior documented in §1.2 must be visible in `artwork_status` (W-4),
  not hidden by parse success.

#### W-4: Artwork status is explicit

Given a parsed session, when `artwork_status()` runs, then the wrapper reports
(a) whether artwork parsing ran at all (model gate), (b) how many tracks carry
decoded thumbnails, and (c) a verdict against the independently counted ArtworkDB
entries — `Ok`, `Missing` (declared entries without parsed thumbnails), or
`SkippedUnknownModel` — with a diagnostic, rather than relying on `itdb_parse`'s
unchecked success. (S-17.)

- Given the real snapshot with seeded model, when `artwork_status()` runs, then 3356
  tracks with thumbnails is reported as `Ok`.
- Given the same snapshot with empty SysInfo, when `artwork_status()` runs, then
  `SkippedUnknownModel` is reported even though `itdb_parse` succeeded.
- Given a fixture with ArtworkDB removed, when `artwork_status()` runs, then `Missing`
  is reported while track parsing still succeeds.

#### W-5: Reads are provably non-mutating in the wrapper

Given any read-side wrapper call, when it runs against a disposable snapshot, then the
snapshot tree hash (files and directories) is unchanged; the wrapper never invokes
libgpod entry points that can create or unlink device files (`itdb_init_ipod`,
`itdb_write*`, hash-info extraction, artwork-dir creation) on read paths. Where an
upstream read can mutate (see §1.1), the wrapper documents it and tests run only on
disposable copies. (S-18; dry-run nonmutation against the real write path is B-009.)

#### W-6: Serialized access and ABI check

Given concurrent calls, when any wrapper function runs, then access to the native
library is serialized (single mutex around load/session/drop), and
`IpodBackend::load()` verifies the loaded library's declared version/ABI before use;
an ABI mismatch yields `BackendUnavailable`, not undefined behavior. (S-19.)

#### W-7: Write operations are defined but gated

Given the sync engine, when it needs create/update/remove track, playlist changes,
artwork set, or `write_library`, then the wrapper defines these on `DeviceSession`
(per the owner's sketch in §7.1) but they are not reachable from any enabled
sync path until B-010's safety verification; B-007 ships them behind a
not-for-production gate. (S-20, ties to R-04/S-07 and the B-007 verification line.)

## 3. MODIFIED

- **plan.md, Phase 1** — "Link-time versus runtime loading remains an owner decision
  before B-007" → resolved: runtime loading of the separately packaged backend, with
  the unavailable/mismatch behavior of W-1/W-6. Proposed replacement wording:
  "Runtime loading of the separately packaged backend is the selected direction
  (owner-reviewed in `deltas/b007_delta.md`); builds without the backend remain
  possible and report sync unavailable, never fall back to file copying. The packaged
  ABI/SONAME policy is finalized with the packaging follow-up."
- **plan.md, Phase 1 preflight** — add: an empty `SysInfo` must surface as unknown
  model and is a readiness question for B-004, never a silent artless parse (§1.2).
- **tasks.md B-007** — extend the Verification line with the R1/R2 closure and the new
  checks (proposed wording in §6). The research-gate sentence becomes "closed in
  `deltas/b007_delta.md`; owner review of the wrapper design recorded there".

## 4. REMOVED

- **External-output isolation**: the idea that pointing `itdb_write_file` at a filename
  outside the device confines side effects is refuted (§1.1) and must not be used as a
  dry-run or safety mechanism (B-009/B-010 own the real isolation).
- **Parse success = artwork success**: no artifact may treat `itdb_parse` success as
  evidence of artwork readiness (§1.2); the wrapper's `artwork_status` (W-4) replaces
  that assumption.
- **Artless-by-default for unknown models**: with empty SysInfo the device is not
  artless; artwork parsing was silently skipped (§1.2, W-3/W-4).

## 5. Open items (later tasks, not blockers for B-007 approval)

- R3 device-name provenance stays open for B-005; new evidence collected: on the
  restored pod the master playlist is named "iPod user" and the volume label is
  "IPOD USER" while `SysInfo` is empty.
- Packaging/SONAME policy for runtime loading and how the ABI check is verified
  (W-6) — separate packaging follow-up.
- Whether Rufin ever seeds `SysInfo` itself (e.g. writing `ModelNumStr` after an
  explicit user model choice) — B-004/B-005 decision; B-007 only surfaces the gate.
- Backup/recovery, stat-merge, write-ordering — B-010. Per-thumbnail artwork
  verification beyond `artwork_status` — B-004/B-010.

## 6. Proposed B-007 verification criteria (supersedes the tasks.md line pending approval)

Wrapper tests cover ownership, allocation/freeing, GLib strings/lists, GError
conversion, ABI compatibility, and serialized access (W-2/W-6). Backend
missing/incompatible configurations report unavailable without fallback (W-1).
Fixture tests cover the model gate and artwork status (W-3/W-4): empty SysInfo,
seeded `MA446`, missing/corrupt ArtworkDB, removed `.ithmb`. Read-path nonmutation is
proven by tree-hash comparison on disposable snapshots (W-5); parse is never run
against a live device. Write APIs are defined but not enabled for sync until B-010
(W-7). Cross-check fixtures use the pinned 0.8.3 build and the documented
`tools/libgpod_check.c` harness; no physical-device acceptance is claimed here
(B-017).

## 7. Appendix: owner's wrapper sketch (implementation aids)

Normative requirements are W-1..W-7 above; this appendix preserves the owner's sketch
that motivated them, plus implementation aids for the coding phase. Rationale from the
sketch: rather than rewriting or forking libgpod, minimal side effects are tolerated as
long as they take place on disposable or backed-up contexts (see §1.1).

### 7.1 API sketch

```rust
pub struct IpodBackend;
pub struct DeviceSession;

impl IpodBackend {
    fn load() -> Result<Self, BackendUnavailable>;
    fn open(&self, mountpoint: &Path) -> Result<DeviceSession, IpodError>;
}

impl DeviceSession {
    // session must be properly opened and closed
    fn device_info(&self) -> Result<DeviceInfo, IpodError>;
    fn tracks(&self) -> Result<Vec<TrackInfo>, IpodError>;
    fn playlists(&self) -> Result<Vec<PlaylistInfo>, IpodError>;
    fn artwork_status(&self) -> Result<ArtworkStatus, IpodError>;
    fn playback_stats(&self) -> Result<Vec<PlaybackStats>, IpodError>;

    fn create_track(&mut self, metadata: TrackMetadata) // itdb_track_new, itdb_track_add?
        -> Result<TrackId, IpodError>;

    fn copy_track(&mut self, id: TrackId, source: &Path) // itdb_cp_track_to_ipod?
        -> Result<(), OperationFailure>;

    fn update_track(&mut self, id: TrackId, metadata: TrackMetadata) // after metadata update: delete + create/copy?
        -> Result<(), IpodError>;

    fn remove_track(&mut self, id: TrackId) // libgpod unused
        -> Result<(), OperationFailure>;

    fn update_playlists(&mut self, changes: PlaylistChanges) // itdb_playlist_add_track
        -> Result<(), IpodError>;

    fn set_artwork(&mut self, id: TrackId, artwork: Artwork) // ???
        -> Result<(), OperationFailure>;

    fn write_library(&mut self) // itdb_write
        -> Result<(), OperationFailure>;
}
```

### 7.2 Sync plan flow (input for B-008/B-009/B-010, not B-007 scope)

```
Connect iPod to PC
     |
Check availability — the pod can mount in "normal-mode" showing 0 capacity
     |  (after unmounting); manual reconnect fixes this? (open question)
     +-- mounted
     +-- mountpoint available
     +-- have read/write access
     +-- has free space
     |
Preflight
     +-- model supported
     +-- databases readable
     |
Read host .itl
     |
Calculate delta (order to be decided)
     +-- statistics to merge
     +-- tracks to remove
     +-- tracks to add
     +-- artwork updates
     +-- metadata updates
     +-- playlist changes
     |
Display proposed changes
     |
Prepare device: backup state (to know side effects and revert them if sync fails)
     |
Apply if not dry-run
     |
Verify sync correct: compare local and device lib
     |
Cleanup
```

### 7.3 Single-track addition schema

```
itdb_track_new() → itdb_track_add() → itdb_cp_track_to_ipod()
    → itdb_playlist_add_track() → itdb_write()
```

### 7.4 libgpod functions to wrap (read/preflight side; the write side arrives with B-010)

```c
itdb_device_read_sysinfo()          // read SysInfo (or itdb_device_get_sysinfo for a field)
itdb_device_get_ipod_info()         // model + generation
itdb_device_supports_artwork()      // model gate for artwork parsing (§1.2)
itdb_device_supports_compressed_itunesdb()
itdb_device_supports_sqlite_db()
itdb_parse()                        // parse iTunesDB + ArtworkDB (check parseability)
itdb_free()                         // close
```

### 7.5 Device findings from the sketch (details in itl-player NOTES.md §20)

- Empty SysInfo on custom firmware: the model string can instead come from
  SCSI/descriptor, the firmware partition (OSOS image), or the iTunesDB header. The
  Rufin-side decision is B-004/B-005; B-007 only surfaces the gate (W-3).
- ithmb filenames are `F<format_id>_<generation>`; banks are append-only logs that
  iTunes compacts only on a full restore. ArtworkDB holds offsets/sizes into them.
- Track dbids are not guaranteed static across restores — matches plan.md's note that
  numeric device track IDs may be reassigned and cannot be the sole cross-sync
  identity.
- `iTunesPrefs.plist` `EstimatedDeviceTotals` is a free capacity source for the device
  card (B-005); the frpd blobs name the last syncing machine and iTunes version.
- Backup/recovery: WIP, owned by B-010 (see §5).
