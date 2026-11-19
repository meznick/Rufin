# B-015 acceptance fixes — the live-pair dry-run diagnosis

Status: **plan** — owner-approved directions, implementation follows on
`feature/ipod-page-task-B-015-fixes` (off `feature/ipod-page-task-B-015`
@ 6f24945).

## Context

B-015 acceptance on the real pod showed a large dry-run diff right after
a real iTunes sync, which the owner expected to be empty. Full read-only
diagnosis against the live pair (pod mounted at `/run/media/meznick/IPOD USER`,
`.itl` at `/nvme2tb/iTunes/`, fresh XML from the same sync; 2026-11-19)
decomposed the diff into five parts. **Track identity is healthy** — the
phantom-diff hypothesis is disproven:

- 3382/3382 device dbids ↔ `.itl` PIDs exact; 0 device-only dbids;
  0 missing device files; 8/8 device `mhyp` ids byte-equal the XML
  playlist persistent ids (pid-first matching matches everything).
- The whole visible diff is: 2 file adds + 2 PDF booklets counted as
  adds, 16 playlist creates + 1 update, 940 skip diffs, 220 rating
  diffs, 121 "covers to remove" — five fixable causes, not identity.

## Verified findings (research facts, → `docs/itunes-ipod.md`)

1. **iTunes never pushes skip counts to this pod.** 940/940 skip diffs
   are host>0 / device=0 on a pair synced by iTunes the same morning.
   Rufin-side skips accumulate in the `.itl` and stay there; the device
   `mhit` skipcount stays 0. Direction is one-way (device→host import
   via `Play Counts`), so a skip diff is never a syncable update.
2. **iTunes materializes the album fill-down rating on device tracks.**
   220/220 rating diffs are host=0 / device=40..100, and the device
   value equals the XML's filled-down rating exactly while the `.itl`
   track field is 0 (the known "0 = inherit album" semantics). A
   host-wins rating push comparing raw values would write 0 to those
   device tracks — a real data-loss bug.
3. **Playlist distinguished kind lives at `miph` byte offset 0x239**
   (unaligned u32 LE; guard: header ≥ 0x23D). Cross-checked against
   the XML `Distinguished Kind` — exact for all 9 XML-flagged
   built-ins: Movies=2, TV Shows=3, Music=4, Audiobooks=5, Rentals=7,
   Podcasts=10, Purchased Music=19, Genius=26, Downloaded=65/66/67 —
   plus the folder containers the XML does not export: Videos=47,
   Home Videos=48, TV & Films=64. User playlists (incl. the owner's
   smart ones) carry 0. `itl-core` 0.1.1 does not expose this field.
4. **iTunes writes cache covers for every track**, including the 121
   host files with no embedded picture — device `has_artwork` = 1 for
   all 3382. The dry run turns those 121 into "covers to remove".
5. **The dry run counts PDF booklets as adds** (4 = 2 Die Antwoord +
   2 booklets); the executor already excludes them
   (`transcode.rs` `NeverCopy`), the review does not.

## Fixes

### F1 — artwork removals are never shown (decision #6 enforcement)

`dry_run/compute.rs` artwork arm: `Absent` + device thumbnail →
`artwork_orphaned` under **every** policy; `artwork_removes` is never
populated. The destructive list stays in the model (type-compat) but is
always empty. `gtk-library` review body drops the "covers to remove"
line (dead); orphaned covers stay silent — the append-only promise is
exactly "you will never see a cover removal". Tests: the existing
`artwork_follows_evidence_thumbnail_and_policy` gate flips to assert
`artwork_orphaned` under `Remove`; a new gate pins that
`artwork_removes` is empty for every policy.

### F2 — rating fill-down before every comparison (data-loss fix)

`mapping/host.rs` `host_tracks`: `HostStats.rating` becomes the
**effective** rating — `track.rating()` when nonzero, else the album
rating (`album_persistent_id()` → `itl.albums()` map; no album or
unrated album → 0). Fill down regardless of the `miah` flag (0x01
explicit / 0x20 computed) — the XML and the device both materialize
either. This feeds the dry-run diffs *and* the merge, so a host-wins
push never writes 0 over a device rating again; on the live pair the
220 diffs become 0 and no patches are planned. The `.itl` itself is
never written (unchanged discipline). Tests: fixture album with
rating, track rating 0 → diff vs device compares the album value;
track with explicit rating unaffected; no-album track stays 0.

### F3 — skip diffs stop being actionable (owner: skips stay off the pod)

`dry_run/compute.rs` `stat_diffs`: drop the `SkipCount` arm — a
skip diff is never a stat patch (the merge already never writes
skips; the device→host import rides `Play Counts`, not this report).
Silent — no diagnostic, no count (owner review 2026-11-19: the
exclusion is routine, not a finding). `StatField::SkipCount` /
`StatCounts.skip_count` remain in the model for compatibility;
`review_summary` never counts them into `stat_updates`. Tests: the
`stat_diffs_cover_all_fields` gate drops SkipCount; a new gate pins
skip-diff emission off.

### F4 — PDF booklets leave the adds count

`dry_run/compute.rs` `MissingOnDevice` arm: host file with extension
`pdf` is silently skipped — never a `FileAdd` (owner review: no text,
just don't count them). Mirrors `transcode.rs` `NeverCopy` so the
review and the executor agree (4 → 2 on the live pair). Test: fixture
booklet adds nothing, no add.

### F5 — built-in playlists never become creates (owner: host trash)

- `itl-core` (itl-player repo): new `Playlist::distinguished_kind()`
  reading raw header bytes 0x239..0x23D (u32 LE), `0` when the header
  is too short; unit test with a crafted header. Publish **0.1.2**;
  bump `itl-core` in the rufin workspace (`gtk-library`, `sources`,
  `ipod-sync`) + `Cargo.lock`.
- `mapping/host.rs`: `HostPlaylist` gains `distinguished_kind`.
- `dry_run/compute.rs`: a `MissingOnDevice` playlist with
  `distinguished_kind` in the built-in set {2,3,4,5,7,10,26,47,48,64,
  65,66,67}, or with zero members, is silently skipped — never a
  `PlaylistCreate` (owner review 2026-11-19: no per-playlist notes,
  just exclude). **Kept:** `19` (Purchased Music — the
  owner's Покупки is deliberately synced to the pod) and the owner's
  own smart playlists (kind 0).
- Matching and device-only classification use the **full** host list,
  unchanged — the device's Покупки/On-The-Go 1 still pid-match and can
  never become "playlist to remove" because of the exclusion.
- Tests: built-in skip + empty skip + Purchased-kept + matching-
  unaffected gates.

### F6 — review dialog shows the report diagnostics

`gtk-library` `review_body`: after the counts, render up to 5 verbatim
`report.diagnostics` lines (mapping + dry-run findings). A future
phantom diff names its own reason instead of asking for a diagnosis
session. Test: summary fixture with diagnostics renders them.

## Expected live-pair review after F1–F6

2 files to add (Die Antwoord — real: in the `.itl`, not on the pod),
0 to remove/repair, 0 playlists to create (the 8 synced ones all
match; the remaining user playlists are the built-ins now excluded or
empties), 1 playlist to update (Хай рейт — genuine smart-criteria
staleness, +5/−4 members), 0 statistic updates, no cover lines. That
is the owner's "synced with iTunes ⇒ empty diff" acceptance criterion,
modulo two real entries.

## Verification

- `cargo test -p ipod-sync` (updated gates), `-p gtk-library` battery,
  clippy on touched crates, template validate.
- `itl-core` 0.1.2: crate tests + `itl-sync check` golden on the real
  library (no behavioral change).
- Live acceptance: dev-build dry run on the real pod — dialog must
  show exactly the expected lines above; console carries no failure.
- Owner iTunes cross-check unchanged (writes remain gated, no
  device/host mutation on the read path).

## Hygiene

- `docs/itunes-ipod.md` §9: findings 1–5 as research facts + the
  post-fix live-pair status; B-015 Result entry.
- `tasks.md`: check off the acceptance-fix items; backlog entries for
  anything deferred (none expected).
- itl-player `NOTES.md`: record `miph` @0x239 (VERIFIED vs XML
  `Distinguished Kind`, 9/9 + folder kinds) next to the other `miph`
  offsets.

## Round 2 — live-pair acceptance findings (2026-11-19)

The first live run after F1–F6 showed 3 creates / 3 updates / 765 stat
updates. Three more causes, all fixed and live-verified (final review:
2 adds, 1 update, **0 statistic updates**, no cover lines):

- **itl-core offset bug**: `raw_header` starts at miph+8 (sig+len
  excluded), so the verified item offset 0x239 lives at raw_header
  0x231 — 0.1.2 read 8 bytes late and returned 0 for everything.
  Fixed in itl-core 0.1.3, together with `Album::album_artist()`
  (mhoh 302, 301 fallback).
- **F2 was keyed by the album id, which is unusable** — `mith` @0x9C
  is zero for most tracks and `album_persistent_id()` reads the
  date-added bytes. The fill-down now keys albums by the lowercase
  (name, album artist) pair, like the sources catalog. And it is
  **explicit-only** (VERIFIED 2026-11-19 on the live pair): iTunes
  materializes the album rating onto device tracks only for explicit
  album ratings (`miah` flag 0x01) — 236/236 explicit matches,
  540/540 computed (0x20) never materialized. Computed albums fill
  down to 0, mirroring the device.
- **Play-date 1970 artifact**: 545 tracks carry the raw Apple value
  `APPLE_TO_UNIX + tz` (1970-01-01 local) on both sides; libgpod's
  conversion squashes the device side to 0 while the old asymmetry arm
  compared the raw host value — phantom diffs. The comparison now
  converts both sides uniformly; both collapse to the same 0.
- **Smart-playlist order is not user data**: iTunes writes smart
  members sorted by the smart sort order, so the device order never
  matches the host insertion order («Последние добавленные» and
  «Самые популярные» reorder-only noise). Reorder now only applies to
  static playlists; membership diffs still surface (Хай рейт +3/−2
  stays).

The `dry_run_pair_gate` rating pin moved from ≤250 to ≤600 with the
mechanism documented: the env pair (live-`.itl` copy vs the
restored-device snapshot) is mismatched, and the fill-down exposes the
genuine corpus drift — the pin is a catastrophic-detection bound until
the ACCEPT corpus is restored; the live pair measures 0.
