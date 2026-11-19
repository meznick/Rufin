# B-017: Per-model physical-device and iTunes acceptance

Status: owner decisions D-1..D-4 recorded (2026-11-19); plan pending
owner review. This is the gate that flips real device writes on.

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

## 5. Phase A — device acceptance on the Video 5.5g

Prep: full backup of `iPod_Control/` (DBs + `Music/` + `iTunes/`) to
the host; record device facts (model string from SysInfo, serial,
capacity, mount, library track count, firmware — SysInfo fields only;
the real firmware version needs the OSOS read, backlog B-025).

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
- Any format fact discovered during phases A/B (e.g. an unexpected
  iTunes byte the engine doesn't model) goes straight into itl-player
  `NOTES.md` with the VERIFIED marker and the measurement.
