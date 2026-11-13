# B-010: Full bidirectional sync and recoverable coordination — implementation plan

Status: **draft for owner review** (2026-11-13). Task branch:
`feature/ipod-page-task-B-010`, based on `feature/ipod-page` at `52b7b28`
(B-009 merged and checked off). Decisions required before implementation are
collected in §11; the research-gate findings below are source-level evidence,
not yet the measured inventory (§2 closes that gap).

## 1. Goal and boundaries

B-010 (`tasks.md`, Phase 2) turns the B-009 dry-run contract into an actual
two-way sync: execute the delta on the device (files, playlists, artwork,
databases) through the safe wrapper, merge device playback statistics back
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

This task does **not**: build UI (B-011..B-016), implement AAC transcoding
(§11-2), import the active On-The-Go queue into the host (future task,
`docs/itunes-ipod.md` §6), refresh materialized smart-playlist member lists
(B-021), or claim per-model hardware acceptance (B-017). Physical-device and
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
   matches the approved plan (hash of the report). Any mismatch refuses the
   write with the specific conflict diagnostic.
2. **Journal intent** (host disk, in Rufin's state dir:
   `<user-state>/rufin/ipod-sync/<device-id>/`, where device-id is the
   FireWire ID when present else the master-pid+model hash): the operation
   record — device identity, plan hash, step list, per-step state, the
   pending host-patch set, and the backup registry — written and fsynced
   (atomic rename). The journal is the recovery truth; it never depends on
   the connection worker's notice queue (B-006 debt: the FIFO latency is
   irrelevant because recovery scans the journal at sync start, not the
   notice).
3. **Backup of the consistency set to the host** (not the device — a full
   device disk must not block the backup): `iTunesDB`, `ArtworkDB`, all
   `*.ithmb`, `Play Counts`, `Play Counts.bak`, `OTGPlaylistInfo*`,
   `iTunesStats`, device `SysInfo`/`SysInfoExtended`. This is the "last
   consistent library" S-12 can restore. Media files are not backed up
   (re-copyable from the host); their removal is deferred until the DB write
   commits.
4. **Stat import and host write** (§6): parse pending device stats, compute
   the merge, write the host patch through itl-sync's single atomic path.
   Zero-sentinel refusal or any failure → abort here, journal stays
   resumable, `Play Counts` untouched, no device write happened.
5. **Device file ops**: copies (libgpod `itdb_cp_track_to_ipod` for the
   F-name layout; per-file size verification) and removals (media files by
   device path, fs-level). Disk-full aborts with the specific error, partial
   files are cleaned, the DB is untouched and consistent.
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
   operation failed-recovered.
8. **Completion**: `Play Counts` was renamed by the write itself;
   `Play Counts.bak` is removed only after the journal marks completion (its
   content is in the host backup and the merged values are in host + device
   — leaving it would let a later Windows iTunes import the already-merged
   delta twice, §6; B-017 validates). Last-success advances only here
   (S-10); every earlier exit is a partial result with the journal retained.

**Retry and dedup.** The journal records per-track watermarks (raw values
last applied). The merge is computed as host-vs-observed-device diffs
(§6) — naturally idempotent: a retry after a host commit but before the ack
sees zero diff and skips; a retry after a device commit re-verifies and
advances; a retry after a failed step resumes from that step. Numeric
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

**The device observation already includes pending plays.** `itdb_parse`
merges `Play Counts` into the track model *additively* at parse time:
`playcount += delta` (with `recent_playcount = delta`), `skipcount += delta`,
`time_played` replaced when set, `rating` replaced when set (previous value
backed up to `app_rating`). The wrapper's `tracks()` therefore carries the
true device totals; the B-009 presence-only stance ends here. The raw file
(mhdp: header with entry length/count, entries of dbid+counts+times) is
parsed by a **pure-Rust reader** in ipod-sync (format is trivial, ~50 lines;
libgpod's parser stays internal to C) and cross-checked against the merged
`tracks()` values in the gate — the two must agree per dbid.

**Merge policy (owner decision §11-3), proposed per-field:**

- `play_count`: merged = `max(host, device observed)`; host patch when
  device > host; device field always written to merged.
- `skip_count`: merged = `max(host, device observed)`; both sides as above
  (the mhit has no skipcount2 baseline field, so max is the only idempotent
  rule).
- `play_date`: newest raw Apple-epoch value wins, both sides (raw space
  end-to-end, B-008 §6; conversion only at the itl-sync boundary).
- `rating`: host-wins push (Video/Classic firmware cannot author ratings;
  the device value is a copy). A device value higher than host is a stale
  copy or an out-of-band host change — never imported over the host's own
  record; the diff is reported, not silently reverted. (max-merge would
  resurrect a rating the user just downgraded in Rufin — rejected for this
  field.)
- `volume_adjustment`: out of merge scope — itl-sync has no volume patch
  field today, and the real pair shows 0 diffs; device-side push requires an
  itl-sync extension (recorded as debt, §11-4).

**The playcount2 baseline rule (cross-iTunes correctness).** iTunes uses
`playcount2` (mhit @+0x54) as the per-device last-sync baseline: it computes
the pending delta as `playcount − playcount2`. After we merge on-device plays
into the host, we **must** write `playcount2 = merged playcount`; otherwise
the next Windows iTunes sync re-adds the already-imported delta (double
count). This mirrors iTunes' own behavior; the gate pins the written value
and B-017's VM check verifies the real iTunes does not double-count. The
same reasoning drives the `Play Counts.bak` removal in §5 step 8.

**Import-first ordering** (§5 step 4) makes the S-13 second clause safe: a
device write can only consume `Play Counts` after the imported values are
durably on the host (itl-sync's atomic write + rolling backups are the host
durability); if the host refuses, nothing is consumed and the retry starts
from the same clean state.

## 7. Execution engine architecture

All new engine code in `crates/ipod-sync` under `src/sync/`, consuming the
B-009 report and the B-008 mapping, plus gated wrapper extensions in
`crates/ipod` (W-7 surface grows; nothing outside `crates/ipod` touches
raw FFI):

- `journal.rs` — the durable operation record (§5 steps 2/8): atomic
  JSON+fsync append, step state machine, per-track watermarks, backup
  registry, `last_success`. Host-side, device-id keyed.
- `backup.rs` — consistency-set backup and restore (§5 steps 3/7), verified
  by byte hashes.
- `import.rs` — pure `Play Counts`/`Play Counts.bak` reader (mhdp), dbid
  keyed, un-attributable entries preserved with diagnostics.
- `merge.rs` — pure per-field merge (§6) over B-008 host records and
  observed device stats: emits the host patch set (itl-sync shape) and the
  device patch set; the playcount2 baseline rule lives here; unit fixtures
  cover every policy combination, zero/absent values, raw-space dates.
- `plan.rs` — converts `DryRunReport` + merge output into the ordered
  execution plan and re-validates it against a fresh dry run (§5 step 1).
- `exec.rs` — the executor: revalidation, file copies/removals, wrapper
  write calls, staged DB write + rename + fsync, post-write verification.
  Coarse `SyncEvent` phases exposed for B-011 (B-010 builds no UI progress).
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
assignment); `create_playlist` (static, ordered members), smart-playlist
creation/update via the §8 translation, member reorder, `remove_playlist`,
and the OTG-exclusion write-prep + staged-write surface (§4).

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
2. **Unit suites** for `merge` (policy matrix), `import` (raw-file fixtures),
   `journal`/`backup`/`recover` (tempdir fault fixtures), `plan` (report →
   ordered ops), `exec` with a fake device layer where each step can be
   told to fail.
3. **Env-gated pair gate** (`ITL_TEST_PATH` copy, `IPOD_TEST_TREE` copy,
   pinned `RUFIN_LIBGPOD_DIR`): a full sync on the copies with the owner
   decisions applied; post-sync: device re-parse clean, `artwork_status`
   `Ok` with expected counts, host `.itl` copy carries exactly the merged
   stats (itl-sync `check`), dbids/playlist pids intact, the originals
   (snapshot + `.itl` copy source) byte-identical — the sync only ever
   touched the copies.
4. **Failure-injection battery** (the task's checklist, each on disposable
   copies): disk full (small FAT loop image, per the `scripts/ipod-dev`
   mount pattern — a new `test-sync` scenario); removal (FS ops failing
   mid-sequence); each multi-file write boundary (fail after step N for
   every N, then recover); competing writers (`/proc` fixture + held
   `flock`); zero sentinel (host refusal — assert abort **before** any
   device change); device-success/host-failure (assert unreachable by
   ordering, §5, and that the host-refusal path is the tested equivalent);
   host-commit-before-ack (journal step replay → no double import);
   retry-twice (same journal replayed twice → identical final state);
   reassigned numeric track ids (fixture with rewritten `mhit` @+0x10 ids →
   merge/backup unaffected, keyed by dbid).
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
   §11-1 with the measured evidence.
2. Owner decisions (§11) folded into this plan (B-009 precedent).
3. Wrapper write extensions (§7) with ownership/ABI/layout tests and the
   OTG-exclusion prep; staged-write surface.
4. `merge.rs` + `import.rs` with unit fixtures; cross-check `import` against
   the wrapper's merged `tracks()` in a small env-gated test.
5. `journal.rs` + `backup.rs` + `recover.rs` with tempdir fault fixtures.
6. `plan.rs` + `exec.rs`: the ordered executor with revalidation, copies,
   removals, staged DB write, verification, flush (§5).
7. `host.rs`: the itl-sync host write step with sentinel-refusal abort.
8. Env-gated pair gate (§9-3) with pinned expectations from the snapshot
   pair (the 11 known adds minus policy exclusions, the playlist creates
   minus exclusions, the stat merges by policy).
9. Failure-injection battery (§9-4/§9-5) + `scripts/ipod-dev` `test-sync`
   scenario.
10. Full verification, results recorded, review requested.

## 11. Decisions required before implementation

1. **Write strategy.** Proposed: staged `itdb_write_file` + own rename +
   OTG exclusion (§4), accepting the second-`mhlp` category-list loss until
   iTunes regenerates them. Alternative: own iTunesDB serializer (preserves
   topology, requires reimplementing the Classic checksum — rejected on
   cost/risk). Confirm the acceptance of the topology limitation.
2. **Transcoding.** Proposed: B-010 implements `Copy` mode only; the
   owner's "Convert higher bitrate songs to AAC 256" becomes its own task
   (GStreamer encode + measurement-based capacity + device acceptance).
   Alternative: transcode inside B-010 — significantly larger scope; needs
   a decision on the encoder and the fallback for un-encodable tracks.
3. **Stat merge policy.** Proposed per-field rules of §6 (max for counts
   and date, host-wins rating, playcount2 baseline reset). Alternatives:
   iTunes-delta arithmetic for counts; max for rating (rejected: resurrects
   downgrades).
4. **Volume adjustment.** Proposed: out of scope (0 diffs on the real pair;
   itl-sync lacks the field). Alternative: extend itl-sync with a volume
   patch first (cross-repo work + VM acceptance).
5. **Playlist selection.** Proposed: sync every host playlist iTunes would
   (all static and smart `miph`, including folder playlists), exclude PDF
   Digital Booklets from file copies (iTunes never copies them — B-009 gate
   evidence) and criteria-less Genius playlists; selection UI later.
6. **Artwork removals.** Proposed: remove artwork only for tracks being
   removed entirely; keep the 99 cache-written thumbnails the B-009 gate
   surfaced (iTunes' own art cache writes them; fighting it is churn).
7. **`Play Counts.bak` removal.** Proposed: remove after journal
   completion (§5 step 8) to prevent a later Windows-iTunes double-import;
   B-017 validates the real behavior. Alternative: leave it and accept
   iTunes-side dedup (unverified).
8. **Backup/journal location.** Proposed: host state dir keyed by device
   identity (§5). Alternative: on-device (fails on disk-full — rejected).
9. **Matching stays name+kind.** pid-first playlist matching (true
   renames) remains the recorded follow-up task (B-009 §6); confirm B-010
   does not implement it.
10. **Active OTG import.** The active queue stays device-owned and
    untouched (owner rule); transferring it into the host remains a future
    task. Confirm it is not in B-010.
