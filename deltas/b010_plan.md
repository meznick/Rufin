# B-010: Full bidirectional sync and recoverable coordination — implementation plan

Status: **approved by the owner (2026-11-16)**; the §11 decisions are
resolved and folded into the body below. Task branch:
`feature/ipod-page-task-B-010`, based on `feature/ipod-page` at `52b7b28`
(B-009 merged and checked off). The research-gate findings in §2 are
source-level evidence; the measured inventory (§10 step 1) closes R1 before
code lands.

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

**Stat consumption: the executor's, not the write path's.** Measured: the
`itdb_write_file` path calls `playcounts_reset` on success, but it
resolves the pending names against the **mount root** (wrong directory —
the files live in `iPod_Control/iTunes/`) and is a **no-op**. The only
real upstream consumer is `itdb_write`'s `itdb_rename_files` (`Play
Counts` → `Play Counts.bak`) — the function B-010 never calls. The
executor therefore removes the pending files itself at completion (§5
step 8), after the journal marks the operation done; the import must be
durably on the host first either way.

**`itdb_write()` (the direct device write) additionally:** `itdb_start_sync`
(no-op on regular iPods), a `.ithmb` pre-write attempt whose errors are
deliberately non-fatal, then on success `itdb_rename_files()` — `Play Counts`
→ `Play Counts.bak`, **removal of `OTGPlaylistInfo`**, `iTunesShuffle`/
`iTunesStats` handling — plus a `SysInfo` rewrite if `sysinfo_changed` is
set, then `itdb_stop_sync` and a global `itdb_fsync()` (which calls `sync(2)`).

**Playlist topology survives; the category lists lose members.** Parsing
reads the user playlists from the first type-3 mhsd (fallback type 2);
writing emits them into both the type-3 and type-2 lists. The media-library
category lists («Музыка», «Фильмы», «Телешоу», «Аудиокниги») live in the
**type-5** mhsd — an earlier draft claimed a "second type-2 `mhlp`" that
libgpod drops; the measurement (step 1, below) disproved that: libgpod
parses type 5 into `priv->mhsd5_playlists` and re-writes it, so the lists
survive. What does not survive is their stored member mhips (libgpod writes
type-5 lists with zero members) — the B-021 cache-staleness class: the
criteria are live-updating, the firmware re-evaluates them, iTunes
re-materializes the members at its next sync. `mhyp` id @+28 (`pl->id`)
round-trips — it is the host playlist persistent id
(VERIFIED 12/12 on the pair, 6/6 vs XML, 2026-11-13; B-009 §6).

**Smart playlists round-trip through libgpod's own rule model — measured.**
The writer serializes `pl->splpref` (mhod 50) and `pl->splrules` (mhod 51)
from the parsed structs. Measured on the pair (step 1): mhod 51 (the SLst
rules) is **byte-exact** after a no-op parse→write (3/3); mhod 50 (SPLPREF)
is not — libgpod parses only a 14-byte window and zeroes bytes 14..71,
dropping one constant byte iTunes writes at +0x10 (0x05 on all three).
§8's verbatim-blob fallback covers created/updated SPLPREFs.

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

**The measured inventory (step 1, done 2026-11-16; full record in
`b001_research.md` R1/R2).** On disposable copies of the snapshot tree with
the pinned libgpod (harness `/tmp/opencode/r1-harness`): (a) a no-op
parse→`itdb_write_file` to a staging path — topology preserved (type-5
category lists survive, members zeroed), dbid multiset identical (3373/3373),
numeric track ids reassigned, mhit header 624→584 with the dropped tail all
zeros (non-loss), **632 mhods of six unknown-to-libgpod types dropped**
(26 plist / 37 ISRC-tag / 39 copyright / 42 `2:256` / 43 purchase email /
44 purchase name — all UTF-16 text metadata, §11-11), mhbd version hardcoded
to 0x30 (parsed 0x73 lost), ArtworkDB rewritten on the device, ithmb banks
untouched; (b) a full `itdb_write` on a copy seeded with `Play Counts` and
`OTGPlaylistInfo`: `Play Counts` → `.bak` on success, `OTGPlaylistInfo`
removed, and the injected `OTG Playlist N` serialized into the DB as an
mhyp — the violation the OTG-exclusion design prevents; pending-store
formats confirmed (`Play Counts` entries carry no dbid and align to the
mhlt order; `OTGPlaylistInfo` = mhpo + big-endian u32 track indices);
(c) a read-only `Artwork/` run: the artwork failure is silently non-fatal,
`write_file` still returns OK — R2's post-write verification is the only
detector; (d) mhod 50/51 byte-exactness as above.

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
  (wanted), and the pending store untouched by the write (measured R1 —
  consumption is the executor's completion step, import first, §6).
- **OTG exclusion is a wrapper write-prep step:** before the staged write,
  the wrapper removes the libgpod-injected `OTG Playlist N` playlists from
  `itdb->playlists` (they are identifiable structurally: libgpod itself
  creates that exact name for every `OTGPlaylistInfo` file it parses; iTunes
  names saved OTG records `On-The-Go N` and they are regular playlists that
  stay in). After the write the links are restored so the session remains
  usable. `OTGPlaylistInfo*` files are never touched by any Rufin path.
- **The topology limitation is accepted and documented:** the four category
  lists (mhsd type 5) survive a libgpod write with their stored member
  lists zeroed — the live-updating criteria re-fill them (measured, §2),
  and iTunes re-materializes the members at its next sync (iTunes accepts
  libgpod-written DBs — long-standing gtkpod behavior; B-017 validates on
  hardware). An own-format iTunesDB serializer would preserve everything
  but must reimplement the Classic checksum (§2) — recorded as the
  rejected alternative, not silently forgotten.
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
8. **Completion**: the pending store is removed by the executor only
   after the journal marks completion (measured R1: the staged write does
   not consume it — `playcounts_reset` resolves the wrong directory
   upstream; the entries were imported at step 4 and the merged values are
   in host + device — leaving the file would let a later Windows iTunes
   import the already-merged delta twice, §6; B-017 validates).
   Last-success advances only here (S-10); every earlier exit is a partial
   result with the journal retained.

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
Crash after device write but before completion: the pending file **remains**
(the staged write does not consume it — R1-measured) and the watermarks
skip every entry already merged into host and device; the completion step
removes the file. A
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
  post-write verification. The `STEPS` names were exposed as B-011's seam
  (B-010 emits no UI progress itself — B-011 landed the event model in
  `sync/report.rs`, b011_plan §13).
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

## 8. Smart playlist writing — deferred to B-023

Status (owner 2026-11-17): the translation is a **backlog task (B-023)**,
split out of B-010. The executor skips smart-playlist creates and criteria
updates with explicit diagnostics — never a silent conversion to static.
Existing smart playlists are unaffected: their `mhod` 51 rule blobs
round-trip byte-exact through the normal staged write (R1-measured 3/3),
so the pair's three smart lists keep working. What waits for B-023: new
smart playlists reaching the device, criteria edits propagating, and the
type-5 category-list duplicate guard.

The design B-023 inherits:

- the flattened `SLst` rule blob (mhod 51) — B-009's `flatten.rs` already
  produces it from host criteria, proven byte-exact against the pair — and
  the 72-byte SPLPREF (mhod 50) compiled from the host's 112-byte Smart
  Info (live-update/limit/sort/match-only settings);
- the R1-measured round-trip: mhod 51 is byte-exact through libgpod's
  parse→write (3/3), mhod 50 loses bytes 14..71 (libgpod serializes only
  the 14-byte window; iTunes writes a constant `05` at +0x10). The route —
  libgpod's rule API (`Itdb_SPLRULES` for the flattened form,
  `Itdb_SPLPREF` mapped from Smart Info) vs. a wrapper verbatim-blob write
  vs. a patched libgpod — is B-023's design decision (the splice breaks
  the Classic checksum);
- the guardrails: `CriteriaUnknown` / unflattenable criteria skip the
  criteria write and keep membership, with a diagnostic (B-008 §7);
  Genius playlists (no criteria in the `.itl`) stay unsynced, as iTunes
  itself does not sync them; and the four media-library category lists
  (Музыка/Фильмы/Телешоу/Аудиокниги) live in the type-5 list libgpod
  never surfaces — creating them as user playlists would duplicate them,
  so B-023 must skip them.

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

1. **R1/R2 measurement run** — done 2026-11-16: the full inventory is
   recorded in §2 and `b001_research.md` R1/R2; §11-1's strategy is
   confirmed by the measurement (topology survives, category-list members
   zeroed); the AAC-encoder availability check was closed by B-010a. The
   one open item is the §11-11 decision.
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

1. **Write strategy.** Resolved (owner 2026-11-16): staged
   `itdb_write_file` + own rename +
   OTG exclusion (§4), accepting the second-`mhlp` category-list loss until
   iTunes regenerates them.
2. **Transcoding.** Resolved (owner 2026-11-16): the sync transcodes
   exactly like the owner's iTunes — non-MP3 tracks with bitrate above 256
   kbps become AAC 256 (GStreamer encode to a host-side temp, then copied
   through the F-name layout); MP3 files are **never** transcoded, and
   non-MP3 at or below 256 kbps are copied verbatim. PDF booklets are
   excluded (§11-5). Encoder availability is verified in step 1 (§10).
   **Delivered by B-010a (merged 2026-11-16):** the decision + staging
   lifecycle in `ipod-sync/src/sync/transcode.rs` and
   `audio_processing::transcode_to_m4a` (`deltas/b010a_plan.md`); B-010
   consumes the provider.
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
5. **Playlist selection.** Resolved (owner 2026-11-16): sync every host
   playlist iTunes would
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
11. **Unknown-mhod metadata loss — accept.** Resolved (owner 2026-11-16):
    the measured inventory (§2) shows a no-op libgpod write drops 632 text
    mhods of six types libgpod doesn't model (copyright, ISRC-style tag,
    purchase account email/name, a plist, a `2:256`-shaped field) and
    rewrites the mhbd version to 0x30. Accepted and documented — the data
    lives in the host library, the pod firmware displays none of it,
    iTunes regenerates it at its next sync (B-017 validates the claim on
    hardware). The vendored-patched-libgpod alternative is recorded and
    rejected as out of proportion.

## 12. Implementation results (2026-11-16/17)

All steps implemented on `feature/ipod-page-task-B-010` except the §8
translation (backlog **B-023**, owner 2026-11-17 — the executor defers
smart creates/criteria updates with diagnostics, never a conversion to
static).

- **Step 1 (measured R1/R2)** done 2026-11-16: the full write-path
  inventory in §2/`b001_research.md` — topology survives via the type-5
  list (members zeroed), dbids stable, the mhit-header shrink is
  zero-padding, 632 unknown-mhod metadata drops accepted (§11-11), mhod
  51 byte-exact / mhod 50 one-byte loss, pending-store formats pinned,
  and the staged write's `playcounts_reset` measured as a **no-op** (wrong
  directory) — consumption is the executor's completion step.
- **Step 3**: pid-first playlist matching (5 tests; pair gate identical
  outcomes, 12/12).
- **Step 4**: wrapper write surface — stat-capable `TrackMetadata` with
  explicit dbid, playlist create/remove/rebuild, staged `write_library_to`
  with the OTG exclusion (gated integration tests incl. a real
  `OTGPlaylistInfo` fixture).
- **Steps 5-6**: additive merge with the drift guard + the mhdp reader
  (17 tests); on-device journal/backup/recovery (13 tests incl. crash
  survival and corrupt-journal refusal).
- **Steps 7-8**: the executor — plan hash (FNV-1a over actions), the
  exclusion layers (flock + /proc scan + writability probe), the
  import-first host write, the §5 transaction with resumable steps.
- **Steps 9-10 (gates, env-gated, disposable copies)**: the pair gate —
  one full sync: 9 adds (11 − 2 PDF booklets), 3382 tracks post-sync,
  artwork proof-read `Ok`, completed journal, originals byte-identical.
  The failure battery — seeded pending (+2 plays/+1 skip): host-refusal
  abort with the consistency set untouched, fail-after-every-step with
  the device parsing after each crash, the pending imported **exactly
  once** across the crashes, pending store consumed at completion,
  held-lock refusal, post-DbWrite crash recovered by verify-and-advance.
- **Gate-forced fixes**: one write session for mutations + DB write (a
  fresh session silently lost the adds); FileOps re-runs on resume until
  the DB write commits; recovery arms run the completion housekeeping;
  `transcode_to_m4a` tears down to Null before returning (PLAYING-drop
  criticals + a segfault under load).
- **Verification**: ipod-sync 110 unit tests; audio-processing 15/15;
  wrapper suite 48+13; test-synthetic; test-wrapper; lint exit 0;
  clippy 0 on both touched crates; `git diff --check` clean.
- **Deferred to B-023**: smart-playlist criteria translation (§8) and the
  type-5 category-list duplicate guard; **to scripts**: the `test-sync`
  FAT-image disk-full scenario (the code path is covered by the
  battery's error propagation); **to B-017**: hardware/VM acceptance of
  the whole write protocol.
