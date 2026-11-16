# B-010: Full bidirectional sync and recoverable coordination — implementation plan

Status: **draft for owner review** (2026-11-16). Task branch:
`feature/ipod-page-task-B-010`, based on `feature/ipod-page` at `52b7b28`
(B-009 merged and checked off). Decisions required before implementation are
collected in §11; the research-gate findings below are source-level evidence,
not yet the measured inventory (§2 closes that gap).

## 1. Goal and boundaries

B-010 (`tasks.md`, Phase 2) turns the B-009 dry-run contract into an actual
two-way sync: execute the delta on the device (files — with the owner's
AAC-256 transcode rule, §4 — playlists, artwork, databases) through the safe
wrapper, merge device playback statistics back
into the host `.itl` through sentinel-guarded itl-sync, and — because libgpod
provides no transaction (§2) — implement the application-level safety layer:
durable operation journal, backups of the consistency set, write ordering,
competing-writer exclusion, interrupted-sync recovery, retry deduplication,
and safe flush before ejection. Traceability: `spec.md` R-04/S-07, S-10,
S-12, R-05/S-13; `plan.md` Phase 2 "Recovery and Statistics"; `constitution.md`
§3 (device writes require application-level multi-file safety) and §4
(device-write acceptance before mainline merge).

The B-009 report is the input contract; the merge policy, write ordering and
recovery protocol defined here are the ones B-011 (progress/completion
reporting) and B-015 (UI sync button) build on. The task's verification
wording is the checklist: concrete backup/journal, write-ordering,
writer-exclusion, stat-merge/deduplication, recovery and flush/eject protocol
reviewed **before** destructive writes are enabled; failure injection covers
disk full, device removal, every multi-file write boundary, competing
writers, zero sentinel, device-success/host-failure, host-commit-before-ack,
retry-twice, and reassigned numeric device track IDs; partial success never
advances last-success; test copies only; no start/stop-sync transaction
assumption.

This task does **not**: build UI (B-011..B-016), import the active On-The-Go
queue into the host (future task, `docs/itunes-ipod.md` §6) or saved
On-The-Go playlists (backlog B-022 — a device-only saved OTG is never
removed until then, §11-10), refresh materialized smart-playlist member
lists (B-021), or claim per-model hardware acceptance (B-017). Physical-device and
subsequent-iTunes acceptance of the write protocol is B-017's; B-010's gates
are the disposable-copy evidence in §9. The deferred artwork proof-read fix
branch stays untouched.

## 2. Research gate R1: the complete `itdb_write*` side-effect inventory

R1 (`deltas/b001_research.md`) demands the full inventory of
`itdb_write_file()` side effects before B-010 designs its backups and write
ordering. Source-level findings against the pinned libgpod 0.8.3-20
(`src/itdb_itunesdb.c`, work dir `/tmp/opencode/libgpod-src/libgpod-0.8.3`):

**The serializer regenerates the whole file from the in-memory model.**
`itdb_write_file_internal()` builds the output from scratch: `mhbd` header
(8 mhsds, +1 when a genius cuid is present), mhsd type 1 (tracks), type 3
(podcast list), type 2 (playlist list), type 4 (albums, `mhla`), type 8
(artists, `mhli`), empty types 6 and 10, type 5 (movie-rental lists), then
fixes the header, applies the model checksum
(`itdb_device_write_checksum`, hash58/hash72/hashAB; required from Classic
and nano-video on) and fsyncs. Compression/iTunesCDB/sqlite paths are iOS-only
(`itdb_device_supports_compressed_itunesdb` == sqlite support) — irrelevant
for Video/Classic.

**Session mutation during write.** `prepare_itdb_for_write()` reorders
`itdb->tracks` in memory (master-playlist members moved to the front) and
**reassigns `track->id`** (mhit @+0x10, sequential from 1024) — the numeric
device index is never stable across writes. `track->dbid` (mhit @+0x70, the
host track PID) is written verbatim and survives — identity is safe, the
"reassigned numeric device track IDs" wording in the task refers to
`track->id`. Album/artist/composer id tables (`mhla`/`mhli`) are rebuilt from
scratch with fresh ids.

**Artwork is written to the device regardless of the output filename.** The
serializer calls `ipod_write_artwork_db(itdb)` *before* the database bytes
when the device supports artwork: ArtworkDB + ithmb banks are written to the
**device's** `Artwork/` directory using the parsed mountpoint, even when the
DB output filename is somewhere else. Its return value is not checked (R2).
It also mutates `track->artwork_count`/`artwork_size` before serializing.

**Stat consumption on success.** `playcounts_reset(itdb_get_mountpoint(itdb))`
runs after a successful `wcontents_write` — it renames `Play Counts` and
`iTunesStats` to `.bak` **on the device mountpoint**, again regardless of the
output filename. A successful write therefore always consumes the pending
stats; the import must be durably on the host first.

**`itdb_write()` (the direct device write) additionally:** `itdb_start_sync`
(no-op on regular iPods), a `.ithmb` pre-write attempt whose errors are
deliberately non-fatal, then on success `itdb_rename_files()` — `Play Counts`
→ `Play Counts.bak`, **removal of `OTGPlaylistInfo`**, `iTunesShuffle`/
`iTunesStats` handling — plus a `SysInfo` rewrite if `sysinfo_changed` is
set, then `itdb_stop_sync` and a global `itdb_fsync()` (which calls `sync(2)`).

**Playlist topology is not preserved.** Parsing reads exactly one playlist
list (the first mhsd type 3 if present, else the first type 2 — a single
`mhlp` inside it); writing emits **all** `itdb->playlists` into both the
type-3 and type-2 lists. The device snapshot's second type-2 `mhlp` — the
iTunes-written media-library category lists («Музыка», «Фильмы»,
«Телешоу», «Аудиокниги» plus the duplicated nine) — is never parsed and is
dropped by a libgpod write; iTunes regenerates those lists at its next sync.
`mhyp` id @+28 (`pl->id`) round-trips — it is the host playlist persistent id
(VERIFIED 12/12 on the pair, 6/6 vs XML, 2026-11-13; B-009 §6).

**Smart playlists round-trip through libgpod's own rule model.** The writer
serializes `pl->splpref` (mhod 50) and `pl->splrules` (mhod 51) from the
parsed structs; whether that reproduces iTunes' bytes exactly for flat rules,
and how unknown fields survive, is unmeasured — it is one of the two
measurements of step 1 below.

**OTG injection on parse.** `read_OTG_playlists()` appends a playlist per
`OTGPlaylistInfo[_N]` file, named `OTG Playlist N` by libgpod itself. A plain
write would serialize these injected playlists into the iTunesDB as `mhyp`
records *and* (via `itdb_rename_files`) delete `OTGPlaylistInfo` — both are
violations of the owner rule (`docs/itunes-ipod.md` §6: never serialize
injected OTG playlists, never touch `OTGPlaylistInfo`). The write path must
exclude them (§4).

**Checksum functions are not exported** (`itdb.h` exposes no hash58/72/AB
symbols) — an own-format writer would have to reimplement or patch libgpod
for the Classic-mandatory checksum.

**The measured inventory (step 1 of §10)** closes R1: on disposable copies of
the device tree, (a) parse→`itdb_write_file` to a staging path on the same
copy, then diff the whole tree and the DB bytes — cataloging every changed
section of a *no-op* write (expected: everything, including track order and
`track->id`, `mhla`/`mhli` ids, mhsd topology, the dropped second `mhlp`);
(b) the same with `itdb_write` on a copy seeded with `Play Counts` and
`OTGPlaylistInfo` fixtures — verifying the consumption/removal claims; (c) a
failed-write run (read-only Artwork dir) to pin the partial-write behavior;
(d) mhod 50/51 byte-exactness after parse→write with no rule changes. The
results are recorded in the delta and appended to `b001_research.md` R1.

## 3. Research gate R2: artwork write failures

R2 requires detecting and reporting artwork write failures instead of
trusting parse/write success. Design chosen for B-010: the wrapper's write
surface gains no new artwork trust — after the write, the executor re-runs
the B-007 proof-read (`artwork_status()`: verdict against independently
counted ArtworkDB entries, `Ok`/`Missing`/`SkippedUnknownModel`) and compares
the thumbnail counts against the expected set the delta planned. A verdict
other than `Ok`, or a count shortfall, is a **partial success** with a
diagnostic and retained recovery state (§5) — never a completed sync (S-10).
The gate (§9) injects exactly the R2 scenarios on disposable trees: a
read-only `Artwork/` dir (the upstream "non-fatal" path — DB write succeeds,
artwork silently stale) and a missing ithmb bank — and pins the partial
outcome. The wrapper still never invents a verdict.

## 4. Write strategy: staged libgpod write

Recommended and proposed as the base design (owner decision §11-1):

- **Never call `itdb_write` directly** — its `itdb_rename_files` step
  removes `OTGPlaylistInfo`, which the owner rule forbids.
- **Use `itdb_write_file` to a staging filename inside the device's
  `iPod_Control/iTunes/` directory** (same filesystem → the final rename is
  atomic), then rename into place ourselves. This gives: DB crash-safety via
  staging, no `OTGPlaylistInfo` removal, artwork still written to the device
  (wanted), `Play Counts` still consumed on success (wanted — import first,
  §6).
- **OTG exclusion is a wrapper write-prep step:** before the staged write,
  the wrapper removes the libgpod-injected `OTG Playlist N` playlists from
  `itdb->playlists` (they are identifiable structurally: libgpod itself
  creates that exact name for every `OTGPlaylistInfo` file it parses; iTunes
  names saved OTG records `On-The-Go N` and they are regular playlists that
  stay in). After the write the links are restored so the session remains
  usable. `OTGPlaylistInfo*` files are never touched by any Rufin path.
- **The topology limitation is accepted and documented:** the four category
  lists live in the second `mhlp` libgpod never parses, so a libgpod write
  drops them until the next iTunes sync regenerates them (iTunes accepts
  libgpod-written DBs — long-standing gtkpod behavior; B-017 validates on
  hardware). An own-format iTunesDB serializer would preserve them but must
  reimplement the Classic checksum (§2) — recorded as the rejected
  alternative, not silently forgotten.
- The wrapper write surface stays gated behind `sync-writes`; the B-010
  engine is the first (and until B-015 the only) consumer that enables it.

**File writes: the transcode rule (owner decision §11-2).** The sync writes
files exactly like the owner's iTunes: non-MP3 tracks with a bitrate above
256 kbps are transcoded to AAC 256 before copying (GStreamer encode to a
host-side temp file, then copied through the F-name layout via
`itdb_cp_track_to_ipod`); MP3 files are **never** transcoded and are copied
verbatim, as are non-MP3 files already at or below 256 kbps. PDF Digital
Booklets are never copied (§11-5). Encoder plugin availability (avenc_aac
or equivalent in the existing GStreamer stack) is a step-1 verification
(§10). Temp encodes live on the host and are removed on success, abort and
recovery; the B-009 capacity estimate stays the conservative upper bound
until the real encoded size is measured post-encode, which then becomes the
authoritative free-space check.

## 5. The sync transaction and recovery protocol

No shared host/device transaction exists; the protocol below is what makes
interruption recoverable. **Ordering is import-first:** the host write of
imported stats precedes any device write, so a host refusal (zero sentinel)
or failure aborts *before* anything on the device changes — the dangerous
"device success + host failure" interleaving becomes unreachable by
construction, and the spec's clause is satisfied by that invariant plus the
journal (the fault-injection gate asserts the invariant, §9).

1. **Pre-write revalidation** (S-11/S-12): mount still present, writable,
   and the *same* device (identity re-check: master-pid/serial/FireWire ID
   vs. the session that produced the delta), backend alive, no competing
   writer (§ below), and a re-run of the dry run over a fresh session
   matches the approved plan (hash of the report). A **free-space check**
   verifies the device can hold the planned adds *plus* the on-device
   overhead — the consistency-set backup, the staged DB copy and the
   journal (owner decision §11-8). Any mismatch refuses the write with the
   specific conflict diagnostic.
2. **Journal intent** (on the device, owner decision §11-8): a Rufin-owned
   directory on the pod's data partition (exact path pinned at
   implementation; the firmware and iTunes ignore it) holds the operation
   record — device identity, plan hash, step list, per-step state, the
   pending host-patch set, and the backup registry — written and fsynced
   (atomic rename). The journal is the recovery truth; it never depends on
   the connection worker's notice queue (B-006 debt: the FIFO latency is
   irrelevant because recovery scans the journal at sync start, not the
   notice). Accepted consequence (§11-8): a device restore or filesystem
   corruption destroys the recovery info together with the data it would
   recover.
3. **Backup of the consistency set** (on the device, in the journal
   directory, owner decision §11-8): `iTunesDB`, `ArtworkDB`, all `*.ithmb`,
   `Play Counts`, `Play Counts.bak`, `OTGPlaylistInfo*`, `iTunesStats`,
   device `SysInfo`/`SysInfoExtended`. This is the "last consistent library"
   S-12 can restore; the step-1 space check guarantees room for it. Media
   files are not backed up (re-copyable from the host); their removal is
   deferred until the DB write commits.
4. **Stat import and host write** (§6): parse pending device stats from
   `Play Counts` and `Play Counts.bak` (both — §6), compute the merge, write
   the host patch through itl-sync's single atomic path. Zero-sentinel
   refusal or any failure → abort here, journal stays resumable,
   `Play Counts` untouched, no device write happened.
5. **Device file ops**: transcode-then-copy for non-MP3 above 256 kbps,
   verbatim copies otherwise (§4), both through libgpod
   `itdb_cp_track_to_ipod` for the F-name layout with per-file size
   verification; removals are media files by device path, fs-level.
   Disk-full aborts with the specific error, partial files (and host-side
   temp encodes) are cleaned, the DB is untouched and consistent.
6. **Device DB write**: in-memory model updated (tracks, playlists, artwork
   inputs, stat fields with the playcount2 baseline rule §6), OTG excluded,
   staged `itdb_write_file` → verify the staged file parses on a disposable
   copy → atomic rename into place → fsync file and parent directory,
   `syncfs` the mount (libgpod's `itdb_fsync` alone is not the contract;
   eject remains the OS/UI's job, B-016).
7. **Post-write verification**: fresh session on the device: parse succeeds,
   `artwork_status()` returns `Ok` with the expected counts (§3), spot
   identity checks (dbids unchanged, playlist pids intact). On failure:
   restore the consistency set from the step-3 backup, journal marks the
   operation failed-recovered. Restoring the DBs does not roll back media
   files — orphans and missing files are exactly the states the next sync's
   delta engine repairs (device-only entries and `FileRepair`), so the pair
   returns to consistency by construction.
8. **Completion**: `Play Counts` was renamed by the write itself;
   `Play Counts.bak` is removed only after the journal marks completion (its
   entries were imported at step 4 and the merged values are in host +
   device — leaving it would let a later Windows iTunes import the
   already-merged delta twice, §6; B-017 validates). Last-success advances
   only here (S-10); every earlier exit is a partial result with the journal
   retained.

**Retry and dedup.** The journal records the applied delta set before the
host write: per-dbid watermarks (applied play and skip deltas) plus the
consumed file identity. The additive merge (§6) is idempotent under those
watermarks: a retry after a host commit but before the ack diffs the
still-live pending files against them and applies only entries newer than
the watermark; a retry after a device commit re-verifies and advances (the
renamed `.bak` is re-read but the watermarks skip every already-applied
entry); a retry after a failed step resumes from that step. Numeric
`track->id` reassignment (§2) cannot break anything because merge keys are
dbids (= host PIDs) everywhere; Play Counts entries with un-attributable
dbids are preserved in the journal with diagnostics, never dropped.

**Competing-writer exclusion** (S-12): refuse when (a) the mount is not
writeable or not the same volume identity as at preflight; (b) any process
holds open file handles under `iPod_Control/` (the `/proc` scan pattern
already used for the host `.itl`); (c) a Rufin-owned lock (`flock` on the
device-id lock file in the journal dir) is held elsewhere. In the owner's
environment the device is attached to one OS at a time (VM passthrough), so
the realistic competitors are local processes — the scan covers them.

## 6. Stat import and merge

**The pending delta store.** Every counter merge is additive because the
device keeps a dedicated pending-delta file: `Play Counts` (mhdp, header
0x60). Each entry carries playcount @+0, last-played timestamp @+4, bookmark
@+8, and — depending on the entry length — rating @+12 (≥0x10), an unknown
u32 @+16 (≥0x14), skipcount @+20 and last-skipped @+24 (≥0x1c). The mhit
counters are totals; the file is the "since last sync" delta store, and
iTunes consumes it (renames to .bak) in the same sync that adds the deltas
to the host. libgpod's parse merges it the same way, additively:
`playcount += delta`, `skipcount += delta`, `time_played` replaced; its
`recent_playcount`/`recent_skipcount` hold the per-sync deltas ("for
information only" per upstream). The wrapper's `tracks()` therefore already
carries the true device totals. The raw files are parsed by a **pure-Rust
reader** in ipod-sync (the entry layout above is the whole format); the gate
cross-checks per dbid: the file's delta sums must equal the wrapper's merged
values minus the stored mhit values (rating excluded from that check —
libgpod applies entry ratings at parse while our reader ignores them, so a
mismatch there *is* the stray-entry diagnostic below).

**Merge policy (owner decision §11-3), per-field (corrected 2026-11-16 —
additive, not max):**

- `play_count` — **additive**: host = host + device delta, where the delta
  is the per-dbid sum of the pending `Play Counts` entries, cross-checked
  against `mhit.playcount − mhit.playcount2` (playcount2 = the last-sync
  baseline iTunes itself maintains). A max merge would silently lose plays:
  with the host advanced by Rufin plays (100→103) and two pending device
  plays (102), max keeps 103 and drops the two — iTunes sums to 105. After
  the merge the device is written with playcount = merged total and
  **playcount2 = merged total** (baseline reset; cross-iTunes rule below).
- `skip_count` — **additive** the same way via the entry skipcount field.
  The mhit has no skipcount2 baseline — the pending file is the delta store
  and the mhit skipcount is a total. Device written to the merged total.
- `play_date` — **newest wins**, both directions: timestamps merge by
  recency and both sides author plays (max is correct for timestamps, unlike
  for counters). Raw Apple-epoch space end-to-end (B-008 §6); conversion
  only at the itl-sync boundary.
- `rating` — **host-wins push**: Video/Classic firmware has no rating UI,
  so a device rating is never device-authored — it is a stale copy iTunes
  wrote, and iTunes' own rule ("newer modification wins", keyed on the
  device by `mhit` @+0x20 `time_modified` and on the host by the `Date
  Modified` the XML exports) degenerates to host-wins because the host is
  the only author. The host-side mith offset for that timestamp is **not
  yet verified** in itl-player NOTES — a merge-prerequisite research item
  that matters only if a device rating author ever appears. The
  `Play Counts` entry rating field is **ignored** on Video/Classic: these
  firmwares never write it, and a file carrying one is reported as a
  diagnostic with the values never applied.
- `volume_adjustment` and `skip_date` — out of merge scope: itl-sync has
  patch fields for neither (rating, play/skip counts, play date only), and
  the real pair shows 0 volume diffs. Both defer to the itl-sync-extension
  decision (§11-4); the device values are carried read-only.

**The playcount2 baseline rule (cross-iTunes correctness).** iTunes uses
`playcount2` (mhit @+0x54) as the per-device last-sync baseline: the pending
delta is `playcount − playcount2`. After we merge on-device plays into the
host, we **must** write `playcount2 = merged playcount`; otherwise the next
Windows iTunes sync re-adds the already-imported delta (double count). This
mirrors iTunes' own behavior; the gate pins the written value and B-017's
VM check verifies the real iTunes does not double-count. The same reasoning
drives the `Play Counts.bak` removal in §5 step 8.

**The additive rule needs the journal (dedup).** Additive import is only
safe because the journal records the applied delta set before the host write
(§5 step 2): per-(dbid, applied play delta, applied skip delta) watermarks
plus the consumed file identity. The import reads **both** `Play Counts` and
`Play Counts.bak` and diffs the entries against the watermarks: entries
already applied are skipped, only newer ones are applied. Crash after host
write but before device write: the pending file is still live on the pod —
the retry applies only entries newer than the watermarks (no double import).
Crash after device write but before completion: only the `.bak` remains,
and the watermarks skip every entry already merged into host and device. A
`.bak` left behind by a crashed **iTunes** sync (renamed but never imported
by iTunes) carries entries no watermark covers — those get imported, which
is exactly right: the plays were never merged by anyone. All keys are
dbids; the reassigned numeric `mhit` ids never participate.

`iTunesStats` (the shuffle-era second pending store) is **not imported**:
if present on Video/Classic it is a diagnostic, it is included in the
backup set, and it is never consumed by our paths. `bookmark_time`
(audiobook resume positions) is carried read-only and never merged — out of
scope like volume and skip date.

**Import-first ordering** (§5 step 4) makes the S-13 second clause safe: a
device write can only consume `Play Counts` after the imported deltas are
durably on the host (itl-sync's atomic write + rolling backups are the host
durability); if the host refuses, nothing is consumed and the retry starts
from the same clean state.

## 7. Execution engine architecture

All new engine code in `crates/ipod-sync` under `src/sync/`, consuming the
B-009 report and the B-008 mapping, plus gated wrapper extensions in
`crates/ipod` (W-7 surface grows; nothing outside `crates/ipod` touches
raw FFI):

- `journal.rs` — the durable operation record (§5 steps 2/8): atomic
  JSON+fsync append, step state machine, per-dbid watermarks, backup
  registry, `last_success`. On-device, in the Rufin-owned journal
  directory (§5 step 2).
- `backup.rs` — consistency-set backup and restore (§5 steps 3/7) into the
  on-device journal directory, verified by byte hashes.
- `import.rs` — pure `Play Counts`/`Play Counts.bak` reader (mhdp), dbid
  keyed, watermark-diffed, un-attributable entries preserved with
  diagnostics.
- `merge.rs` — pure per-field merge (§6) over B-008 host records and
  observed device stats: emits the host patch set (itl-sync shape) and the
  device patch set; the playcount2 baseline rule lives here; unit fixtures
  cover every policy combination, zero/absent values, raw-space dates.
- `plan.rs` — converts `DryRunReport` + merge output into the ordered
  execution plan and re-validates it against a fresh dry run (§5 step 1).
- `exec.rs` — the executor: revalidation, transcode pipeline + file
  copies/removals, wrapper write calls, staged DB write + rename + fsync,
  post-write verification. Coarse `SyncEvent` phases exposed for B-011
  (B-010 builds no UI progress).
- `exclusion.rs` — mount/identity re-check, `/proc` handle scan, `flock`.
- `recover.rs` — journal scan at sync start (and on Ready for the device id):
  resume / verify-and-advance / restore-from-backup per §5.
- `host.rs` — the host write step: caller-supplied `HostWriteContext`
  (library path, media root, tz offset — from the selected iTunes source
  config at the B-015 boundary) driving `itl_sync::write_library` in-process,
  exactly like the sources write-back path; sentinel refusal surfaces as the
  abort diagnostic. Caller-supplied artwork provider (`pid → ArtworkInput`)
  on the same pattern as B-009's evidence map — the engine stays decoupled
  from `crates/sources`.

Wrapper write extensions (all gated, all with ownership/ABI tests per B-007
conventions): stat-capable `TrackMetadata` (rating, playcount, playcount2,
skipcount, time_played, volume, bookmark) in `update_track`; explicit dbid
assignment in `create_track` (dbid = host PID — never libgpod's random
assignment); created tracks must cover the full `mhit` field set iTunes
writes — including `mediatype` (@+0xD0, the smart-evaluator's VideoKind
input and the pod's media-menu key), `compilation` and `filetype` — and the
gate pins a created `mhit` against iTunes' own form (§9); `create_playlist`
(static, ordered members; `podcastflag` set for «Подкасты» so the pod treats
it as podcasts), smart-playlist creation/update via the §8 translation,
member reorder, `remove_playlist`, and the OTG-exclusion write-prep +
staged-write surface (§4).

## 8. Smart playlist writing

New smart playlists (`PlaylistCreate { kind: Smart }`) and criteria updates
(`CriteriaDiffer`) need the two device forms iTunes writes: the flattened
`SLst` rule blob (mhod 51) — B-009's `flatten.rs` already produces it from
host criteria, proven byte-exact against the pair — and the 72-byte SPLPREF
(mhod 50) compiled from the host's 112-byte Smart Info (live-update/limit/
sort/match-only settings). Step 1 of §10 measures whether libgpod's own
parse→write of these two mhods is byte-exact on the pair's three smart
playlists; if yes, the translation is host-criteria → libgpod's rule API
(`Itdb_SPLRULES` built for the *flattened* form, `Itdb_SPLPREF` fields mapped
from Smart Info) and the gate re-verifies the emitted bytes against iTunes'
own device blobs. If libgpod's round-trip is lossy, the fallback is a
wrapper verbatim-blob write (the write-side counterpart of B-009's read-side
`smart_blob`) — the plan commits to byte-equality with iTunes' output either
way, and to never silently converting to static: `CriteriaUnknown` /
unflattenable criteria skip the criteria write and keep membership, with a
diagnostic (B-008 §7). Genius playlists (no criteria in the `.itl`) stay
unsynced, as iTunes itself does not sync them.

## 9. Verification: gates and failure injection

1. **R1/R2 measured inventory** (§2/§3) on disposable trees — recorded in
   the delta and `b001_research.md`.
2. **Unit suites** for `merge` (policy matrix), `import` (raw-file fixtures
   incl. a pre-existing `.bak`), `journal`/`backup`/`recover` (tempdir
   fault fixtures), `plan` (report → ordered ops), `exec` with a fake
   device layer where each step can be told to fail.
3. **Env-gated pair gate** (`ITL_TEST_PATH` copy, `IPOD_TEST_TREE` copy,
   pinned `RUFIN_LIBGPOD_DIR`): a full sync on the copies with the owner
   decisions applied; post-sync: device re-parse clean, `artwork_status`
   `Ok` with expected counts, host `.itl` copy carries exactly the merged
   stats (itl-sync `check`), dbids/playlist pids intact, transcoded adds
   land as AAC (filetype/size verified), MP3 and ≤256 adds land verbatim,
   the originals (snapshot + `.itl` copy source) byte-identical — the sync
   only ever touched the copies.
4. **Failure-injection battery** (the task's checklist, each on disposable
   copies): disk full (small FAT loop image, per the `scripts/ipod-dev`
   mount pattern — a new `test-sync` scenario — including the on-device
   backup/journal overhead in the pre-write space check); removal (FS ops
   failing mid-sequence); each multi-file write boundary (fail after step N
   for every N, then recover); competing writers (`/proc` fixture + held
   `flock`); zero sentinel (host refusal — assert abort **before** any
   device change); device-success/host-failure (assert unreachable by
   ordering, §5, and that the host-refusal path is the tested equivalent);
   host-commit-before-ack (journal step replay → no double import); a
   pre-existing iTunes `.bak` (imported exactly once, no double count);
   retry-twice (same journal replayed twice → identical final state);
   reassigned numeric track ids (fixture with rewritten `mhit` @+0x10 ids →
   merge/backup unaffected, keyed by dbid); created tracks carry full
   `mhit` coverage, pinned against iTunes' own record form.
5. **Post-write flush proof**: after the staged rename, files and parent
   directory fsynced, `syncfs` on the mount — asserted via the test image
   remount (pull-the-plug simulation reads the same bytes back).
6. Full battery: `cargo test -p ipod-sync`, wrapper suite with
   `sync-writes`, `scripts/ipod-dev test-synthetic/test-wrapper/test-full`,
   `lint`, `git diff --check`. Record actual results in the delta; request
   owner review before merge; `tasks.md` checkoff only after the merge.

No physical-device or Windows-iTunes claim is made here — those are B-017.

## 10. Work sequence

1. **R1/R2 measurement run** (§2/§3) on disposable trees; record the full
   inventory; append the R1/R2 resolutions to `b001_research.md`; resolve
   §11-1 with the measured evidence. Verify the AAC encoder is available in
   the pinned GStreamer stack (avenc_aac or equivalent) — the transcode
   path's prerequisite.
2. Owner decisions (§11) folded into this plan (B-009 precedent).
3. **pid-first playlist matching** in `map_playlists` (device `mhyp` id
   @+28 ↔ host persistent id, name+kind fallback; owner decision §11-9);
   the pair gate pins identical outcomes on the snapshot pair (both keys
   agree 12/12).
4. Wrapper write extensions (§7) with ownership/ABI/layout tests, full-mhit
   created-track coverage, the OTG-exclusion prep; staged-write surface.
5. `merge.rs` + `import.rs` (both files, watermark dedup) with unit
   fixtures; cross-check `import` against the wrapper's merged `tracks()`
   in a small env-gated test.
6. `journal.rs` + `backup.rs` + `recover.rs` (on-device) with tempdir fault
   fixtures.
7. `plan.rs` + `exec.rs`: the ordered executor with revalidation, the
   transcode pipeline, copies, removals, staged DB write, verification,
   flush (§5).
8. `host.rs`: the itl-sync host write step with sentinel-refusal abort.
9. Env-gated pair gate (§9-3) with pinned expectations from the snapshot
   pair (the 11 known adds minus policy exclusions, transcoded vs verbatim
   split, the playlist creates minus exclusions, the stat merges by policy).
10. Failure-injection battery (§9-4/§9-5) + `scripts/ipod-dev` `test-sync`
    scenario.
11. Full verification, results recorded, review requested.

## 11. Decisions required before implementation

1. **Write strategy.** staged `itdb_write_file` + own rename +
   OTG exclusion (§4), accepting the second-`mhlp` category-list loss until
   iTunes regenerates them.
2. **Transcoding.** Resolved (owner 2026-11-16): the sync transcodes
   exactly like the owner's iTunes — non-MP3 tracks with bitrate above 256
   kbps become AAC 256 (GStreamer encode to a host-side temp, then copied
   through the F-name layout); MP3 files are **never** transcoded, and
   non-MP3 at or below 256 kbps are copied verbatim. PDF booklets are
   excluded (§11-5). Encoder availability is verified in step 1 (§10).
3. **Stat merge policy.** Resolved in draft (corrected 2026-11-16, owner
   review): additive deltas for play and skip counts — the `Play Counts`
   file is the pending-delta store, the host sums the per-dbid deltas, and
   the journal watermarks make retries dedup-safe; newest-wins for play
   date (timestamps merge by recency); host-wins push for rating (no
   device rating author on Video/Classic; `Play Counts` entry ratings
   ignored with a diagnostic; the host-side date-modified mith offset is a
   research prerequisite for the timestamp-newer-wins generalization);
   playcount2 baseline reset on every device write; volume and skip date
   deferred to an itl-sync extension (§6). Alternatives considered and
   rejected: max for counts (loses plays/skips), max for rating
   (resurrects downgrades).
4. **Volume adjustment and skip date.** both out of scope (0
   volume diffs on the real pair; itl-sync has patch fields for neither).
5. **Playlist selection.** sync every host playlist iTunes would
   (all static and smart `miph`, including folder playlists), exclude PDF
   Digital Booklets from file copies (iTunes never copies them — B-009 gate
   evidence) and criteria-less Genius playlists; selection UI later.
6. **Artwork removals — none, ever.** Resolved in draft (corrected
   2026-11-16, owner review): the executor performs no artwork deletions.
   iTunes treats ArtworkDB as add-only — entries are never individually
   cleaned, the file is only occasionally recreated whole — and libgpod's
   own writer regenerates ArtworkDB + ithmb banks from the in-memory model
   on every write with no per-entry deletion path. The B-009
   `ArtworkRemove` list (the 99 cache-written thumbnails on the pair) is
   therefore informational under every policy, including the default
   `Remove`; `Remove` never applies to artwork. Matched tracks' artwork
   state is never touched, so iTunes' cache-written thumbnails survive the
   regeneration unchanged; adds are the only artwork mutation, and artwork
   of removed tracks disappears only because the track leaves the model.
7. **`Play Counts.bak` removal.** Resolved: remove after journal
   completion (§5 step 8) to prevent a later Windows-iTunes double-import;
   the import step reads both files with watermark dedup (§6), so a `.bak`
   left by a crashed iTunes sync gets imported rather than lost; B-017
   validates the real behavior.
8. **Backup/journal location — on-device.** Resolved (owner 2026-11-16):
   the journal and the consistency-set backup live in a Rufin-owned
   directory on the device's data partition. The pre-write space check
   (§5 step 1) counts the planned adds plus the backup, staged-DB and
   journal overhead — a full disk refuses the sync before any write.
   Accepted consequence: a device restore or corruption destroys the
   recovery info together with the data it would recover.
9. **Playlist matching — pid-first.** Resolved (owner 2026-11-16): match
   device `mhyp` id @+28 against the host playlist persistent id (`miph`
   @+432) with name+kind as the fallback — a rename in iTunes then survives
   the sync as a true rename instead of remove+create. Implemented in B-010
   (§10 step 3); the pair gate pins identical outcomes on the snapshot pair
   (both keys agree 12/12). Supersedes the earlier "candidate follow-up
   task" note.
10. **On-The-Go.** The active queue stays device-owned and untouched (owner
    rule); importing it into the host remains a future task. Saved
    `On-The-Go N` playlists: importing them into the host library is
    deferred to backlog **B-022** — the host side needs a playlist-add
    capability that itl-core deliberately does not have (`reindex()` would
    merge the second `mlth`). Interim rule (owner 2026-11-16): a saved OTG
    playlist that exists only on the device is **never removed** in any
    policy — it is informational with an import-deferred diagnostic until
    B-022 lands.
