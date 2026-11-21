# B-017: Per-model physical-device and iTunes acceptance

Status: owner decisions D-1..D-4 recorded (2026-11-19); step 0 done
(2026-11-19); a real-pod run happened out of order on 2026-11-21 —
recorded below as the deviation record, not an acceptance gate. A1–A8 /
B1–B3 and the D-3 flip remain open.

## 1. What this task is

B-017 is the write-protocol acceptance: the first real writes to the
owner's physical iPod, validation of the "iTunes regenerates it" claims
in real Windows iTunes (the VM), and the gate flip that makes
`ipod/sync-writes` default-on. Spec reference: R-02, R-03, R-04, S-12,
R-05, S-13; plan reference: Phase 4, Testing Strategy.

Everything before this point ran on disposable corpora: the engine
battery (b010, 5/5 green — full round-trip, failure battery,
cancel/resume/restore), the dry-run pair gate, and the live dry run on
the real pod (owner-accepted 2026-11-19: 2 adds, 1 Хай рейт update,
0 stat updates). B-017 closes the two remaining unknowns: how the
**physical device** behaves under the writes, and how **Windows iTunes**
reacts to a Rufin-written DB and the host-library copy Rufin's host-write
step produced.

The claim inventory (every "B-017 validates" across the deltas):

1. **mhod regeneration** (`b001_research` §2.2, `b010_plan` §11-11):
   a libgpod write drops 632 text mhods of 6 types (26/37/39/42/43/44).
   Claim: iTunes regenerates them from the host library at its next sync.
2. **mhbd version 0x30** (`b001_research` §2.2): libgpod hardcodes the
   DB version; claim: iTunes accepts and rewrites its own version.
3. **Topology** (`b010_plan` §2): the type-5 category lists survive a
   write with stored member lists zeroed; claim: iTunes re-materializes
   the members.
4. **Playcount dedup** (`b010_plan` §6/§7): the executor writes
   `playcount2 = merged playcount` and removes `Play Counts`(+`.bak`);
   claim: the next Windows-iTunes sync does **not** double-count the
   merged plays, and a `.bak` left by a crashed iTunes sync is imported
   exactly once.
5. **OTG files**: `OTGPlaylistInfo*` never touched by any Rufin path;
   claim: iTunes accepts the DB with them present (and the staged write
   does not serialize OTG playlists into the DB — the owner-rule
   violation the staged write exists to prevent).
6. **Device acceptance** (tasks.md verification): files, playlists,
   artwork, checksums, play/skip/rating/date round trips, interrupted
   recovery, safe ejection — plus opening the resulting disposable
   host-library copies in real Windows iTunes.

## 2. Owner decisions (2026-11-19)

- **D-1 — hardware scope: Video 5.5g only.** The owner's pod («iPod
  user», Video (Black), 5.5g) is the only device available. Per-model
  claims for untestable devices are not honest, so the supported set
  shrinks: `generation_supported` keeps the **Video family** (Video 1
  stays supported by proximity — the same iTunesDB format family, the
  same libgpod converter, NONE checksum for both generations) and
  drops **Classic 1/2/3** to unverified until a device appears. Code
  change in `preflight.rs` + its tests (the Classic-FireWire-GUID
  requirement tests become unsupported-model tests) + the model-table
  docs; Classics show the unsupported diagnostic in the UI instead of
  Ready. A Classic device later re-opens the set (B-017 follow-up).
- **D-2 — VM workflow: owner drives, assistant scripts.** The Windows
  VM stays the owner's hands; I prepare all disposable corpora,
  step-by-step checklists, byte-level verification scripts, and a
  results-capture sheet. The owner runs the iTunes clicks and pastes
  outputs back.
- **D-3 — gate flip: `sync-writes` becomes default-on after acceptance.**
  Passing B-017 removes the compile-time feature gate (feature
  default-on, flag removed or inverted); all builds can write.
- **D-4 — step 0 included.** The app-level disposable write-sync run
  (dev build with `sync-writes`, loop-mounted FAT image of the device
  tree, temp iTunes source on a `.itl` copy) is B-017 step 0, before
  the real pod is written.

## 3. Safety rules (unchanged from B-010, restated)

- The real pod is written **only** in phase A below, only with the
  feature-enabled dev build, and only after a full backup of
  `iPod_Control/` to the host.
- The live host `.itl` is never used: the sync's host side always
  points at a disposable copy (`/tmp/opencode/...`, `.itl.copy`).
- Never simultaneous writers: the pod is unmounted from the host before
  the VM attaches it (phase B), and vice versa.
- The engine's own machinery stays active: sentinel-guarded host write,
  flock + /proc exclusion, on-device journal and consistency backup.
- Interrupted-recovery testing (A7) happens only on the owner's
  explicit go, mid-sync cable pull or kill, with the backup restored
  from the consistency set if the device is left inconsistent.

## 4. Step 0 — app-level disposable write run (mine to prep, owner's click)

1. Build the dev binary with the write surface:
   `cargo build -p rufin -F gtk-shell/sync-writes` — gtk-shell now has a
   forwarding feature `sync-writes = ["ipod/sync-writes"]` (the rufin
   binary has no direct ipod dependency, so the feature needs the
   forwarding hop; D-3 builds on the same seam).
2. Device side: a loop-mounted FAT image of the restored-device
   snapshot tree (1.5G, udisks-mounts at `/run/media/meznick/IPOD`,
   appears in the mount table the discovery reads).
3. Host side: a disposable `.itl` copy (fresh byte copy of the live
   library) as the source's library path, media folder unchanged —
   the owner edits the existing iTunes source in the settings screen
   (edit, don't add — the D-4 rule wants exactly one source), and
   points it back after the run.
4. Owner clicks through: scan → Ready → dev dry run (review must equal
   the engine-computed report) → real Sync → Completed.
5. Verification: byte-diff the device image before/after (journal,
   backup, staged DB write, pending files gone, the add files landed),
   hash-compare the `.itl` copy, and a second dry run reading 0 diffs.

**Fixture note (measured 2026-11-19):** the restored snapshot ships no
`Music/` media, so every matched track would compute as a repair and
the capacity verdict would refuse (73 GB vs 1.2 GB). The engine's
repair probe is existence-only (`dry_run/snapshot.rs`), and no sync
path reads device file contents — so the image is seeded with
zero-byte placeholders at every DB-listed pod path (3373 files), which
turns the pair into a no-repair mirror of the live pod. The placeholder
deviation is documented here, not hidden: file-content fidelity stays
with the real-device phase A. Measured review on this pair: 11 adds
(2 Die Antwoord + 9 tracks iTunes added to the real pod after the
snapshot), 2 playlist updates («Последние добавленные» +3, «Хай рейт»
+33/−3), 220 stat updates (rating 16 / play count 99 / play date 105),
0 creates/removes/repairs/covers, fits (~330 MB needed).

## Step 0 result (2026-11-19) — four engine bugs found, fixed, pinned

The owner's app run reported success, but the verification re-read
caught four real-write bugs the fixture gates had never seen. All
fixed in `ec6ee39` + `db5c43f`; the pair gate now re-runs the dry run
on the synced pair and asserts emptiness, so each is a permanent
regression pin.

1. **Play-date double timezone shift.** The merge and `create_track`
   converted the host raw value as `raw − epoch` only, leaving the
   envelope +3h in a value libgpod shifts again at the write boundary
   — every written play date re-read 3 hours ahead (3384 uniform
   −10800 diffs). `merge_stats` now takes `tz_offset_seconds` and
   converts host raw and pending mac with it; both sides of the merge
   live in the same offset-stripped space the dry run compares in.
2. **Smart-playlist updates were silently skipped.** The executor
   hard-`continue`d every smart update, even with Equal criteria, so
   the reviewed member updates were never written (the re-read still
   showed «Последние добавленные» +3 and «Хай рейт» +33/−3). Equal-
   criteria smart playlists now write their materialized members like
   static ones — iTunes itself materializes smart members; criteria
   translation stays deferred (B-023).
3. **Dangling playlist members.** The first member write then refused:
   a host member whose track has no device record (a
   purchased-not-downloaded track, Laura Welsh «Break The Fall
   (Remixes)») cannot be materialized. The dry run now filters playlist
   members to the device-plus-adds set (creates too), and the executor
   mirrors the guard at write time. iTunes has the same semantics.
4. **Adds carried no artwork.** The artwork section only planned
   matched tracks, so new tracks stayed cover-less and a second dry run
   surfaced them as covers-to-add (4 on this pair). Adds with host
   covers now carry `ArtworkAdd` entries; the pair gate got a real
   embedded-cover provider (lofty on the host file) so the engine path
   is proven even though the production boundary's provider is still
   the known follow-up.

**Incident (fixed in `db5c43f`):** the same app run deleted two live
library files — Snoop Dogg «Sensual Seduction» and Laura Welsh «Break
the Fall» (the add loop's staging cleanup ran `remove_file` on the
host source for direct copies; the source IS the host file when no
transcode is involved). The files were copied to the device first, and
the real pod holds both tracks, so recovery = plug in the pod and copy
them back to the media root. The add path now deletes only the
transcode staging file, and the pair gate records every add source's
bytes before the sync and asserts them byte-identical after — a
host-media regression fails the fixture gate instead of eating the
library.

## Real-pod run (2026-11-21) — out of order, deviation record

A real-pod sync happened before phase A's prep and against the live
library, not the disposable copy. Nothing broke — the engine's own
verification passed — but the two §3 rules it crossed are recorded here
instead of being silently absorbed.

What happened, in the evidence: the owner relaunched the app at 16:26
local on a binary built at 16:24 **without** the feature (a rebuild of
the B-016 stat-chip fix; the write surface is absent from the binary —
`ipod::testkit` symbols: 0). The 16:33 sync attempt walked Intent →
Backup → HostWrite and then the executor's first device write
(`create_track`) hit the compile-time gate:
`sync failed step=FileOps ... write APIs are gated until B-010`.
Nothing was written to either side — the `.itl` backup chain
(`/nvme2tb/iTunes/*.bak*`) shows no host write for that run, and the
pod's journal/backup files all carry the second run's timestamps.

The owner then asked how to enable the feature, ran the
`cargo build -p rufin -F gtk-shell/sync-writes` command (binary
16:45:22, `testkit` symbols: 14; size = the 16:24 binary + 51,560 B —
the compiled write surface over unchanged source), and re-launched at
16:46:30. The second sync completed at 16:48:36: consistency backup
16:47:44–58, host write 16:48:33, iTunesDB 16:48:34, the 2 Die
Antwoord adds landed (`F20/libgpod310292.m4a`,
`F48/libgpod141125.m4a`), ArtworkDB rewritten, `last-success.json`
written, post-write re-parse passed. The owner's observation ("first
sync didn't move the stats, second synced everything") is the gate,
not a merge bug — the first run wrote nothing at all.

The deviations:

1. **The host write went to the live production `.itl`**
   (`/nvme2tb/iTunes/iTunes Library.itl`, `.bak0` = the pre-image),
   because the app's iTunes source points at the live library — §3's
   disposable-copy rule was not followed. The write itself is the
   standard benign field update (sentinel respected, atomic, rolling
   backups), and the owner accepted the result, but phase A's first
   real-pod write was planned against a copy.
2. **No full `iPod_Control/` backup existed before the first real
   write** (§3, §5 prep). Taken after the fact on 2026-11-21 into
   `/nvme2tb/ipod-backup/IPOD-USER_2026-11-21/` (full mount root,
   ~23 GB).

Consequences carried into the remaining gates:

- Phase B's B1 host-library acceptance now targets the **live `.itl`**
  Rufin's host-write step produced (a disposable copy no longer exists
  for that file) — the known full-library re-scan on the next Windows
  iTunes open applies (benign, slow).
- A1 (adds land) and the A2 playlist update (Хай рейт) are de-facto
  exercised on the real pod; A3 covers stay open (the shell boundary's
  artwork provider still returns `None` — see §8), as do A5–A8, B2/B3
  and the D-3 flip. The run does **not** count as acceptance: the gates
  run deliberately, with records.

## 5. Phase A — device acceptance on the Video 5.5g

Prep: full backup of `iPod_Control/` (DBs + `Music/` + `iTunes/`) to
the host; record device facts (model string from SysInfo, serial,
capacity, mount, library track count, firmware — SysInfo fields only;
the real firmware version needs the OSOS read, backlog B-025).

The backup is taken (2026-11-21, after the out-of-order run — see the
deviation record above): `/nvme2tb/ipod-backup/IPOD-USER_2026-11-21/`.

The pod currently matches the live library's dry run (2 adds,
1 update). Phase A first syncs **that plan** (Die Antwoord adds +
Хай рейт update) with the feature-enabled build against a disposable
`.itl` copy — the smallest possible real write set.

| # | Check | What proves it |
|---|-------|----------------|
| A1 | Files land | `iPod_Control/Music/Fxx/*.m4a` exist for the 2 adds; audio plays on the pod |
| A2 | Playlists | Хай рейт updated on-device (members +3/−2, smart criteria intact — `mhod` 51 byte-exact); static playlist round-trip on the fixture numbers |
| A3 | Artwork | ArtworkDB entries for added tracks visible on-device; ithmb banks untouched when nothing changed |
| A4 | Checksums | Video = NONE (nothing to verify); the Classic HASH58 requirement is out of the supported set per D-1 |
| A5 | Play round trip | Play a track on the pod → re-sync → pending store imported, host `.itl` copy gets the play, device `playcount2 = merged` (the pinned gate value), `Play Counts`(+`.bak`) removed |
| A6 | Rating/date/skip | Set rating and play on the pod → re-sync → the disposable `.itl` copy reflects them; host rating/play-date pushes land on the device; skips stay host-side |
| A7 | Interrupted recovery | Mid-sync cable pull (owner-go): re-plug → resume from the journal / restore from the consistency backup; device parses clean after |
| A8 | Safe ejection | umount → re-mount → parse clean; no journal/pending leftovers after a completed sync |

## 6. Phase B — Windows iTunes in the VM (owner-driven)

Prep (mine): a USB-passthrough checklist (host-side umount first), the
disposable `.itl` copy + its media layout copied into the VM, before/
after byte-harness scripts, and the results sheet.

| # | Check | What proves it |
|---|-------|----------------|
| B1 | Host-library acceptance | Open the disposable `.itl` copy **Rufin's host-write step produced** in the VM's real iTunes — accepts, no damage, values as expected |
| B2 | Device acceptance | Attach the Rufin-synced pod to the VM's iTunes, run an iTunes sync: mhbd rewritten to iTunes' version; the 632 dropped mhods regenerated from the host library (type counts back); type-5 category lists re-materialized; **no double-count** of the A5 plays (playcount2 baseline rule proven against real iTunes) |
| B3 | Records | Exact iTunes version, Windows version, device firmware/version fields, library sizes, sync timings — captured into the results sheet |

Any B-row failure reopens the corresponding engine design point, not
just the docs.

## 7. Exit criteria and the gate flip (D-3)

All A1–A8 pass on the 5.5g, all B1–B3 pass in the VM, records
captured. Then one commit flips the gate: `sync-writes` default-on
(feature removed or inverted, all builds write), the D-1 supported-set
shrink lands with it if not already separate, docs update (§9 +
tasks.md Result + NOTES where format facts surfaced). Until that
commit lands, real writes stay feature-gated.

## 8. Open items / follow-ups

- Classic support is unverified (D-1): a device re-opens it — backlog.
- Firmware version from the OSOS image — B-025.
- The shell boundary's artwork provider (`ShellSyncBoundary.artwork`)
  still returns `None` — adds land cover-less and A3 stays open until
  it's wired to the runtime catalog's embedded-cover reader.
- The feature flag is silently sticky per build: a plain
  `cargo build -p rufin` reverts to the gated binary with no warning
  until a sync fails mid-run (the 2026-11-21 incident). Worth a guard
  with the D-3 flip: a build-script assertion that release builds carry
  `sync-writes`, or an iPod-page "writes compiled out" notice.
- Any format fact discovered during phases A/B (e.g. an unexpected
  iTunes byte the engine doesn't model) goes straight into itl-player
  `NOTES.md` with the VERIFIED marker and the measurement.
