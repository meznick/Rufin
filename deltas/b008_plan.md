# B-008: iTunes-to-iPod data mapping — implementation plan

Status: proposed, awaiting owner review. No implementation is authorized by
this document until the open decisions below are resolved and the plan is
approved. Task branch: `feature/ipod-page-task-B-008`, based on
`feature/ipod-page` at `1e239fd`.

## 1. Goal and boundaries

B-008 (`tasks.md`, Phase 2) maps the `.itl` library and a parsed iPod library
into owned, explicit records that later sync stages can use. The verification
target covers files, master/static/smart playlists and track references,
artwork/thumbnail inputs, 0–100 ratings, stable host identity and timestamps.
Traceability: `spec.md` R-04/S-07 and R-05/S-13; `plan.md` Phase 2 and Sync
Data Mapping. libgpod handles device databases and artwork; Rufin owns mapping
and policy.

This task does **not** implement B-009's dry-run delta, B-010's sync/recovery
protocol, host or device writes, file copying, UI, automatic discovery, or
physical-device acceptance. `sync-writes` remains unavailable to production
sync. A mapped record is not a promise that the device can safely be written.
Tests use synthetic inputs or disposable copies; never the live iPod, production
`.itl`, or writable iTunesACCEPT corpus. Do not touch the deferred artwork
proof-read fix branch.

## 2. Evidence and gaps to address first

1. `sources/src/itunes/catalog.rs` opens `.itl`, uses
   `itl_sync::survey::unique_tracks` and `MediaMapping`, and identifies catalog
   tracks with the local `track.id()`. Its `acquire` also merges player state,
   updates SQLite, and may queue `.itl` patches: **do not call it as a pure
   mapping read**. The source currently does not publish iTunes playlists.
2. Registry `itl-core` 0.1.0 exposes library, track and playlist persistent
   IDs, playlist member track IDs, and track metadata. Its `Track::persistent_id`
   and `Playlist::is_smart` are both **wrong for this corpus** (see §2a); no
   verified smart-rule reader is exposed. Do not use them as authoritative or
   silently present smart playlists as static.
3. `ipod::TrackInfo` owns metadata and stats but currently exposes neither
   `ipod_path` nor a host persistent ID. `PlaylistInfo` contains `is_spl` and
   member device dbids but no smart rules. `TrackMetadata` carries only a
   subset of fields; `PlaylistChanges` only appends members to an existing
   playlist. These APIs do not yet implement playlist creation, reordering,
   smart-rule translation, or full-stat writing.
4. Preflight parses can have side effects on some trees (B-007 research).
   Mapping tests that invoke libgpod use disposable complete device copies;
   snapshot comparison alone does not establish live-device safety.

## 2a. Research findings on the 2026-11-10 snapshot pair

Read-only investigation of the owner's `.itl` backup from 2026-11-10
(`iTunes Library.itl.bak8`, iTunes 12.13.10.3, library PID `4DF55DAB7C856A78`),
the same day's `iTunes Library.xml`, and the post-restore device image
(`ipod/test-device-30g.img`; `iTunesDB`, `Preferences`, `SysInfo`, `iTunesPrefs`
extracted from the FAT32 image by a pure-Python reader, no mount, no writes).
The device dump carries 3373 tracks / 22 `mhyp` records in two `mhlp` lists,
`SysInfo` empty, `Preferences` 2924 bytes.

**Track identity (resolves plan §5's main question).** Every device `mhit` dbid
@0x70 (LE u64) equals the XML track `Persistent ID` (3373/3373, all nonzero and
unique), and the host `.itl` `mith` @0x80 (LE u64) carries the same value
(3248/3248 verified where the XML pairing was unambiguous; the remaining 147
records are second-`mlth` purchased duplicates sharing the same numeric track
id — the known duplicate-groups behavior). `dbid2` @0xA8 equals `dbid` on this
device. Consequence: **host track PID = `mith` @0x80 = device dbid**, and
iTunes writes that PID as the dbid it assigns. `itl-core 0.1.0`'s
`Track::persistent_id()` reads `mith` @0x20 and matches nothing (0/3248) — it
is a bug to be fixed in the itl-player repo (its `album_persistent_id()`
currently reads the correct bytes under a wrong name). Until that fix ships,
no mapping may rely on the registry accessor.

**Playlist identity.** Device `mhyp` records carry no value that matches the
`.itl` playlist PID (u64 scan of the whole header); device playlists are
matched by name+kind, as iTunes itself syncs them. Device playlist names match
host names (`iPod user` master, `Плейлист`, `Покупки`, `On-The-Go 1`, smart:
`Последние добавленные`, `Самые популярные`, `Хай рейт`, `Аудиокниги`,
`Музыка`, `Телешоу`, `Фильмы`). The `.itl` has 24 `miph` (20 in XML; the
extra 4 are service/folder playlists that still carry smart-criteria fields).

**Smart criteria are present and machine-readable.** The `.itl` `mhoh` 0x65
payload is the same `SLst` blob as the device `mhod` type 51 (byte-identical
for 3/3 compared playlists; iTunes re-serializes the others on device write) —
big-endian rules, parsed by libgpod's own algorithm. `mhoh` 0x66 holds the
112-byte Smart Info blob (byte-identical to the XML `Smart Info` data);
the device-side `mhod` type 50 is a 72-byte SPLPREF with different layout, so
writing playlist metadata needs a conversion (B-010 territory). The corpus'
criteria decode to 23 rules: field 0x3C (video kind, binary-AND) fully known;
**fields 0x00 (action IS, value `MUIS`-ish 64-bit marker — likely "Media Kind
is Music") and 0x85 (action binary-AND, value 1 — likely purchase/download
status) are unknown to libgpod 0.8.3** and must be either identified or
surfaced as unsupported. `Playlist::is_smart()` (0x02BC) is false for all 11
smart playlists in this corpus — do not use it.

**Stats and dates.** Device raw `rating`/`playcount`/`skipcount`/`time_played`
equal the host raw values for the large majority of unchanged tracks
(3009/3179/2341/2663 of 3227 unambiguous pairs); differences are directional
evidence (device can hold newer on-device plays; host holds later sync-time
values). Device `size`/`bitrate` can be stale versus the host (the host moved
to ALAC after the pod was last synced: device still says AAC for 2536 lossless
tracks) — files must never be matched by these fields. Titles match
device↔host 3373/3373. Both the host envelope and the device `Preferences`
(offset 0xB22, raw code 660) resolve to UTC+3 in this snapshot; libgpod
converts device times as `raw − 2082844800 − timezone_shift`. Merging stats in
**raw Apple-epoch space** and converting only at the `itl-sync` host-write
boundary avoids the timezone question entirely for comparisons; historical
offset variance remains an `itl-sync`-side caveat, not a mapper bug.

**Files.** Device `mhit` `mhod` type 2 holds the on-pod path (4-char names on
the restored store). The wrapper does not expose it yet — B-008's file mapping
and B-009's delta need a safe read-only `ipod_path` in `TrackInfo`.

## 3. Proposed architecture

Put backend-independent mapping and owned contracts in `crates/ipod-sync`,
which already depends on the safe `ipod` wrapper. Add host adapters with an
explicit dependency boundary for `itl-core`/`itl-sync`, or put the host
adapter in `sources` if dependency review favors that. Reuse the existing
source configuration, media mapping and date conversion semantics rather than
forking them. Confirm the dependency graph and backend-disabled build before
choosing the final module layout; do not introduce a dependency cycle.

The proposed core is deterministic and side-effect-free for supplied owned
inputs: no device session, file writes, SQLite updates, `.itl` patches or UI.
I/O adapters construct the inputs separately. Distinguish these records:

| Record | Proposed contents and invariant |
|---|---|
| Host library | Library persistent ID, configured media root and explicit timestamp interpretation; reject missing/ambiguous identity rather than guess. |
| Host track | Library + track persistent IDs, current `.itl` track ID, transferable metadata, raw/stat/date provenance, mapped local file or explicit unavailable reason. Validate nonzero IDs and duplicate identity; library ID namespaces tracks. |
| Device track | Owned `TrackInfo` and session-local dbid, plus any newly verified identifiers and device path exposed by the wrapper. Keep identity confidence/provenance distinct from metadata. |
| Playlist | Host persistent ID, name, kind with an explicit unknown state, ordered host-track references including duplicates; device playlist identity/members remain separate until resolved. |
| Artwork | Source reference or owned encoded cover, explicit absent/unavailable/error states; device `ArtworkStatus` and thumbnail evidence remain separate from desired artwork. |
| Mapping result | Records plus structured diagnostics/unresolved references; never claim successful equivalence for an ambiguous match. No diff, write instruction, or sync-complete status. |

Choose whether incomplete mappings are represented as per-item errors or a
whole-result failure during review. Either way, no unsafe match may silently
become an add/remove, overwrite, or stat import later. This plan recommends
returning diagnostics with identifiable records and marking unresolved data
as non-actionable; B-009/B-010 must respect that state.

## 4. Host tracks, media and field mapping

1. Read an `.itl` snapshot through `ItlFile`; use `unique_tracks` and
   `MediaMapping::new`/`to_local` exactly as the source does. Carry the
   `Mapped`, `NoLocation` (including cloud items) and `OutsideMediaFolder`
   distinctions. File missing/unreadable and unsupported media format are
   distinct from path mapping; do not treat an inaccessible track as deleted.
   Validate safe paths at the later copy boundary, not by constructing
   destination filenames from metadata here.
2. Define a field table from actual `itl-core` accessors to wrapper fields:
   title, artist, album, album artist, composer, genre, track/disc numbers and
   totals, duration (ms), file size (bytes), year, bitrate, sample rate,
   BPM, rating, counts and dates. Preserve `None` versus zero where source
   semantics allow. Verify units against the pinned libgpod header/fixtures
   before conversion; use checked integer conversions, reject overflows and
   avoid silent truncation. `kind()` is a display string, not necessarily a
   file extension or codec — derive/validate file type separately.
3. Map ratings in the iTunes 0–100 scale, validate range, do not invent
   star rounding. Track rating 0 can inherit an album rating in iTunes/XML;
   distinguish raw track rating from any effective display value and never
   materialize album fill-down into `.itl` track fields. Carry source flags
   separately; preserve explicit/clean bits if they become supported.
4. `TrackMetadata` currently lacks several fields. Produce an owned mapping
   model that records gaps; propose wrapper read/write extensions only when
   evidence and tests show they are needed. Extending gated APIs does not
   enable them, and integration with actual writes remains B-010.

## 5. Identity and playback statistics

The snapshot research (§2a) resolves the identity question for this library
generation: **host track PID at `mith` @0x80 == device dbid**, written by
iTunes itself; device playlists match by name+kind; library PID cross-checks
with the device `mhbd` @0x24 in reversed byte order (already recorded in
NOTES §20). The mapping design must therefore:

- key host tracks by `(library PID, track PID)` with the PID read from
  `mith` @0x80, **after** the `itl-core` accessor is fixed and released
  (currently `persistent_id()` reads the wrong offset — see §2a); the
  `.itl` numeric `id()` stays the key for playlist members and `itl-sync`
  patches, resolved to the PID at mapping time;
- treat the device dbid as a session-local handle that happens to carry the
  host PID on iTunes-written devices, never as a permanent fact: restores
  can reassign dbids (NOTES §20), and newly added tracks get libgpod-assigned
  random dbids;
- reject zero/duplicate PIDs and duplicate-numeric-id groups per the existing
  `unique_tracks` policy, with the located record winning and conflicts
  reported (survey already implements this);
- never fall back to title/path/size matching for identity (size/bitrate are
  proven stale on this corpus, §2a);
- carry device stats as device observations in raw space; the merge policy
  (max/device-wins/etc.) is B-010's decision, and host writes stay
  `itl-sync`-only with its sentinel and timezone conversion.

## 6. Dates and times

Research result (§2a): host raw play date (Apple epoch, local wall clock) and
device raw `time_played` are the same numeric space — the large majority of
unchanged tracks are bit-equal, and this snapshot's host envelope and device
`Preferences` both resolve to UTC+3. The mapping therefore keeps **raw
Apple-epoch values end-to-end**: comparisons and merges run in raw space,
conversion to a real instant happens once, at the `itl-sync` host-write
boundary (its `wallclock_to_unix`/configured offset). Distinct types/fields
for raw host value, raw device value, and zero/unset sentinels; validated
against fixtures with nonzero offsets, unset dates, boundary values and the
known historical-offset caveat. Never call the deprecated libgpod no-op time
converters. Any place that needs an instant before the write boundary (e.g.
dry-run display) must carry the offset alongside the raw value, never a
silently assumed UTC instant.

## 7. Playlists and artwork

For playlists, preserve host playlist PID, name, kind, ordered member IDs and
duplicates; resolve numeric members to host persistent keys only when unique.
Missing members and duplicate/zero PIDs become explicit diagnostics. Device
playlists are matched by **(name, kind)** — the snapshot proves device `mhyp`
records carry no host playlist PID (§2a). The master playlist is the device
`is_mpl` record (named after the device, `iPod user` here); on the host side
it is the `Master`-flagged playlist, not a name or empty-membership guess.
Device-only playlists (`On-The-Go 1`) and service playlists are reported, not
silently adopted or deleted.

Smart criteria are verified present and machine-readable (§2a): `.itl` `mhoh`
0x65 = device `mhod` 51 `SLst` blob, parsed with libgpod's rule layout; 0x66 =
Smart Info. The remaining research is identification of fields 0x00 and 0x85
(likely "Media Kind is Music" and purchase/download status) — either identify
them against additional sources or surface them as explicit unsupported
criteria; no implicit conversion to static, no silent loss of live behavior.
Genius playlists carry no criteria in the `.itl` and are out of scope.
Writing smart playlists to the device (Smart Info → SPLPREF conversion,
rule re-serialization) is B-010 territory and not part of this mapping.
The `itl-core` work to expose the 0x65/0x66 payloads lives in the itl-player
repo with its own review; do not silently substitute a local fork or
unverified raw offsets.

For artwork, trace the existing embedded-cover extraction and supported
encoded formats; carry a reproducible reference and distinguish no cover,
unreadable cover and unsupported encoding. Verify any conversion before
supplying `ipod::ArtworkInput` (PNG/JPEG). No raw `.itc2` dependency is
assumed. Test associations between a track and its intended artwork, including
duplicate/shared artwork and absent covers. Thumbnail generation, ArtworkDB
writing and post-write proof are libgpod/B-010 responsibilities; use only
disposable gated wrapper fixtures to demonstrate existing model-specific
capabilities, never claim an in-memory mapping created an ArtworkDB.

## 8. Work sequence and verification

1. Inventory pinned API fields and smart/identity evidence; document coverage
   and unresolved decisions before selecting an on-disk identity scheme.
2. Add small pure fixtures and failing tests for identity, conversions and
   references; define owned records and diagnostics. Implement host adapter
   using safe read-only APIs. No production write feature.
3. Add device observation adapter through the wrapper; if extra read-only
   fields are required, add only safe owned accessors with ABI/ownership tests.
   Do not expand raw FFI outside `crates/ipod`.
4. Implement approved playlist and artwork mapping; represent unsupported
   data explicitly and stop at unresolved research gates rather than claim
   full support. Keep actual write operations gated.
5. Cover with fixtures: multiple libraries and colliding/zero IDs; reordered
   device dbids and duplicate metadata; missing/outside/cloud files and
   overflow; all metadata units, rating 0/20/100 and invalid values; explicit
   versus effective album rating; dates with timezone/zero/overflow; master,
   static, duplicate/ordered/missing member and known smart rules; unsupported
   smart rule; missing/invalid/shared artwork and model-specific thumbnail
   evidence. Test no silent actionable result for ambiguous input.
6. Integration: use `ITL_TEST_PATH` only with a disposable `.itl` copy and
   `IPOD_TEST_TREE` only with a disposable complete device-tree copy and
   pinned `RUFIN_LIBGPOD_DIR`. Compare mapping to independent expected fixture
   values; record before/after host bytes and full device tree hash. Check
   backend-disabled and gated-write configurations. Explicitly report skipped
   env-gated tests; no synthetic result is hardware acceptance.
7. Run targeted `cargo test` for affected crates, the relevant existing source
   and wrapper suites, formatting and clippy, plus `git diff --check`. Verify
   no write-capable production path was enabled. Record actual results and
   outstanding blockers, then request review before merging; update
   `tasks.md` only on the feature branch after a verified merge.

## 9. Decisions required before implementation

**Identity: resolved by research** (§2a): `mith` @0x80 = track PID = device
dbid. The blocker is the `itl-core` accessor bug. Decision needed: fix
`Track::persistent_id()` (and expose the 0x65/0x66 raw payloads) in the
itl-player repo and publish 0.1.x, then bump the Rufin dependency — or vendor
the patched crate per the constitution. The fixture evidence and cross-check
numbers above become the acceptance for that fix. A Rufin-owned persistent
mapping table is not needed for iTunes-written devices; device dbids stay
session-local handles.

**Smart playlists: mostly resolved** (§2a): criteria live in the `.itl` as
the same `SLst` blobs libgpod parses; `mhoh` 0x66 = Smart Info. Remaining
unknowns are the two field ids (0x00, 0x85) and the Smart Info → SPLPREF
conversion (writes only). Decision: identify 0x00/0x85 now (small follow-up
research against the XML text or newer libgpod/gtkpod sources), or ship them
as explicit "unsupported rule" diagnostics and revisit. Genius playlists have
no criteria in the `.itl` — they are out of smart-mapping scope, like iTunes
itself does not sync Genius as criteria.

**Timestamp ambiguity: resolved for comparisons** (§2a): merge in raw
Apple-epoch space; convert only at the `itl-sync` write boundary. Device
`Preferences`-based conversion matches the host envelope in this snapshot;
historical variance stays an `itl-sync` caveat. No guessed UTC conversion.

**Source selection:** how is the iTunes source selected when more than one is
configured, and how should unavailable/cloud tracks affect mapping? Until
decided, the mapper receives an explicitly selected source and reports
unavailable records without treating them as removals; selection UI is outside
B-008.

**Wrapper extensions (small, read-only or gated):** `TrackInfo.ipod_path`
(read-side, safe) for file mapping; possibly `dbid2`. Extending gated write
APIs is not required for mapping itself.
