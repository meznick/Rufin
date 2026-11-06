# Research: B-001 — libgpod connection flow for Video/Classic iPods

**Task**: B-001: Research libgpod and connection flow for Video/Classic iPods
**Spec reference**: R-02, S-03, S-05, R-03, R-04, S-12, R-05, S-13
**Plan reference**: Phase 1
**Date**: 2026-11-06

---

## 1. Headline finding: the planned Rust `libpod` dependency is not available

The earlier 2026-11-06 registry lookup returned no crate named `libpod`:

```
$ crates.io/api/v1/crates/libpod
{ "errors": [ { "detail": "crate `libpod` does not exist" } ] }
```

This does not establish that no third-party Rust binding exists anywhere or that none is
in development. The original plan incorrectly assumed an available iPod dependency under
that name; this research uses libgpod's actual C API instead.

### What actually exists

| Name | Language | License | Status |
|---|---|---|---|
| `libgpod` | C | LGPL-2.1-or-later | Reviewed gtkpod source at `7982c5554f78dde47fd006afbeff659201d6db3d`; no maintenance/support claim inferred from repository popularity. |
| Upstream bindings | C#/Python | See upstream | `bindings/mono` and `bindings/python`; no upstream Rust wrapper in the reviewed tree. |

**Decision**: use a small safe Rust FFI wrapper around libgpod, per the [owner decision](research-based-decisions.md).

---

## 2. Backend decision and responsibilities

libgpod is required for device database/artwork handling, not an optional fallback. Rufin
owns comparisons, sync policy, progress, recovery, and host `.itl` writes through `itl-sync`.
libgpod already accesses a mounted filesystem; replacing it with copying alone omits the
database, playlist references, artwork formats, checksums, and stat handling stock firmware needs.

The earlier filesystem-only recommendation is rejected, not a current option. Link-time
versus runtime loading remains undecided before B-007. Either approach requires ABI checks,
ownership/allocation discipline, GLib string/list handling, GError conversion, and serialized
access. `dlopen()` does not remove native runtime dependencies or FFI safety obligations.
Without the backend, report Sync unavailable; never silently copy files instead.

The shared states are Disconnected, Connecting, BackendUnavailable (UI: Sync unavailable),
Ready, and Error, as defined in the [plan](../plan.md#connection-fsm). There is no Trust state.

---

## 3. Detection (how we find the device)

libgpod's `itdb_parse()` takes a mount point, not a USB device handle. A mount-point scan
is a candidate-discovery method, not a complete readiness check. USB events may trigger
discovery but do not by themselves supply every write prerequisite.

**B-002 status (2026-11-06)**: the backend-independent types are implemented and merged into
`feature/ipod-page` as commit `eea14b0`. `crates/gtk-library/src/device_manager.rs` defines
`ConnectionState`, `DeviceInfo`, `PreflightError`, and `DeviceManager` with no discovery, no
libgpod dependency, and no write logic. The five-state FSM, the `Ready`-only sync gate, the
`Connecting`-only preflight gate, and the `set_ready_after_preflight()` callback contract are
implemented and unit-tested. What this research still owes B-007 and B-004 is the *real*
libgpod-backed content of the preflight callback — backend availability, access, parseable
databases, measured capacity, model/artwork capabilities, and checksum prerequisites. B-002
supplies the contract and the classification; it does not supply the checks.

```c
// itdb_parse(mountpoint, &error)  — libgpod takes a mount point, not a device handle
// itdb_get_control_dir(mountpoint)
// itdb_get_music_dir(mountpoint)
// itdb_get_artwork_dir(mountpoint)
```

**API calls for detection:**
- `findmnt -l` (or a `udev`/`gudev` watcher) → scan for mount points containing
  `iPod_Control/`.
- `itdb_get_control_dir(mountpoint)` → returns the `iPod_Control` path.
- `itdb_get_music_dir(mountpoint)` → `Music/` path.
- `itdb_get_artwork_dir(mountpoint)` → `Artwork/` path.

**Device identification** uses metadata under `iPod_Control/Device/SysInfo` and
`SysInfoExtended`, plus libgpod's model table; these files are not inside `iTunesDB`:
- `itdb_device_get_ipod_info(device)` → a model-table `Itdb_IpodInfo` entry, using extended
  metadata/serial information or `ModelNumStr`. Its capacity is nominal model capacity,
  not measured filesystem capacity or usage.
- `itdb_info_get_ipod_model_name_string(model)` → libgpod's model label.
- `itdb_info_get_ipod_generation_string(generation)` → libgpod's generation label, which
  need not match retail generation numbering verbatim.
- `itdb_device_read_sysinfo(device)` reloads device metadata; upstream warns that calling
  it on a device used by a parsed database invalidates artwork and can cause memory
  corruption. It is not a harmless refresh. Rebuild the owned parse context instead.

Use filesystem statistics for actual total/used bytes. Read the device name from the master
playlist when available, with an explicit volume-name fallback. Firmware is optional and
must only be displayed if actually exposed. SysInfoExtended is not a guaranteed source of
all name/firmware/capacity fields. See the [DeviceInfo contract](../plan.md#data-models--interfaces).

**USB/capability discovery**: Video/Classic devices use mass storage, not an iOS pairing
protocol. Missing capability data/FireWire ID can require USB/SCSI helpers and extra
permissions. Upstream helpers may write SysInfoExtended; do not invoke them as a hidden
side effect of discovery or dry run. Classic requires FireWire-ID checksums even over
USB; libgpod generates those checksums once prerequisites are available.

---

## 4. Preflight / pairing

**There is no pairing step for Video/Classic iPods.** They mount as USB mass-storage
volumes. The iOS Trust/pairing path using libimobiledevice/usbmuxd does not apply to them.

**This is verified in libgpod's own source.** In `src/itdb_sqlite.c`:

```c
#ifdef HAVE_LIBIMOBILEDEVICE
    if (itdb_device_is_iphone_family(itdb->device)) {
        /* get SQL post process commands via lockdown (iPhone/iPod Touch) */
        lockdownd_client_t client = NULL;
        ...
```

The iPhone-family lockdown path is behind `itdb_device_is_iphone_family()` and
`HAVE_LIBIMOBILEDEVICE`. Classic models never reach it. The `libimobiledevice` dependency
in libgpod is *optional* and only for iOS family.

**Preflight is not reduced to finding a mount.** Before Ready, check backend availability,
required access, readable databases, measured capacity, supported model/artwork capabilities,
and model-specific checksum prerequisites. Missing Classic FireWire ID, unknown model,
corrupt databases, or denied access must keep sync disabled with a specific diagnostic.
Missing firmware alone does not block readiness. The [corrected delta](s03_b004.md) covers
these outcomes, retries/removal, and the absence of a Trust step.

---

## 5. Parse/write/free lifecycle (not a transaction)

### Parse
- `itdb_parse(const gchar *mp, GError **error)` — parse iTunesDB + ArtworkDB from mount point.
- `itdb_parse_file(const gchar *filename, GError **error)` — parse a specific file.
- `itdb_start_sync(itdb)` / `itdb_stop_sync(itdb)` — paired hints; no-ops on regular iPods,
  not session locks, a commit boundary, or protection against power loss.

### Write and release
- `itdb_write(Itdb_iTunesDB *itdb, GError **error)` — write back to the device.
- `itdb_write_file(Itdb_iTunesDB *itdb, const gchar *filename, GError **error)` — write to a file.
- `itdb_free(Itdb_iTunesDB *itdb)` — release owned state, not a save/commit operation.

**Safety**: `itdb_write()` is not an atomic whole-device transaction. Audio, thumbnails,
artwork databases, music databases, and stat files need coordinated backup/write ordering,
competing-writer exclusion, durable recovery, and flushing/ejection in Rufin before
destructive sync is enabled. Device writes can consume `Play Counts` and related files;
persist pending host imports first and deduplicate retries even if device numeric IDs change.
Host writes use only sentinel-guarded `itl-sync`. Device success plus host refusal/failure is
partial success with retained pending changes, never a completed sync.

Do not assume a parse API is universally side-effect-free: upstream can create auxiliary
HashInfo data for devices that need it. Dry-run implementations must verify nonmutation,
using a disposable snapshot where an API can mutate device files. `itdb_write_file()` is
not a replacement for a validated multi-file recovery protocol either.

---

## 6. API surface we need

### Device info (R-03: name, model, firmware, capacity)
- `itdb_device_read_sysinfo(Itdb_Device *device)` — reload SysInfo metadata; see the artwork
  invalidation warning in §3. Not a normal post-parse refresh.
- `itdb_device_get_sysinfo(const Itdb_Device *device, const gchar *field)` — get a field;
  the returned string must be freed according to the API ownership contract.
- `itdb_device_set_sysinfo(Itdb_Device *device, const gchar *field, const gchar *value)` —
  mutate metadata in memory; not a discovery/preflight requirement.
- `itdb_device_get_ipod_info(const Itdb_Device *device)` → `struct _Itdb_IpodInfo {
    const gchar *model_number; const double capacity; const Itdb_IpodModel ipod_model;
    const Itdb_IpodGeneration ipod_generation; ... }`
- `itdb_device_get_uuid(const Itdb_Device *device)` — device UUID.

### Tracks (files + playback stats)
- `itdb_track_new()` / `itdb_track_free(track)` / `itdb_track_duplicate(tr)`
- `itdb_track_add(itdb, track, pos)` / `itdb_track_remove(track)` / `itdb_track_unlink(track)`
- `itdb_track_by_id(itdb, id)`
- `itdb_track_set_thumbnails_from_data(track, data, len)` — artwork
- `itdb_track_set_thumbnails(track, filename)`
- `itdb_track_set_thumbnails_from_pixbuf(track, pixbuf)`
- `itdb_track_remove_thumbnails(track)`
- `itdb_track_has_thumbnails(track)`
- Track fields include `title`, `artist`, `album`, `albumartist`, `genre`, `tracklen`,
  `size`, `bitrate`, `samplerate`, `year`, `volume`, `rating`, `playcount`, `playcount2`,
  `recent_playcount`, `skipcount`, `recent_skipcount`, `last_skipped`, `explicit_flag`,
  `mediatype`, `ipod_path`.

### Playlists (static + smart)
- `itdb_playlist_new(title, spl)` / `itdb_playlist_free(pl)` / `itdb_playlist_duplicate(pl)`
- `itdb_playlist_add(itdb, pl, pos)` / `itdb_playlist_move(pl, pos)` / `itdb_playlist_remove(pl)`
- `itdb_playlist_add_track(pl, track, pos)` / `itdb_playlist_remove_track(pl, track)`
- `itdb_playlist_by_id(itdb, id)` / `itdb_playlist_by_name(itdb, name)`
- `itdb_playlist_mpl(itdb)` / `itdb_playlist_set_mpl(pl)` — master playlist
- `itdb_playlist_podcasts(itdb)` / `itdb_playlist_set_podcasts(pl)` — podcast playlist
- `itdb_playlist_is_mpl(pl)` / `itdb_playlist_is_podcasts(pl)`
- `itdb_splr_new()` / `itdb_splr_add(pl, splr, pos)` / `itdb_splr_remove(pl, splr)`
- `itdb_splr_add_new(pl, pos)` / `itdb_splr_validate(splr)`
- `itdb_splr_eval(splr, track)` — evaluate a rule against a track
- `itdb_spl_update(pl)` / `itdb_spl_update_all(itdb)` / `itdb_spl_update_live(itdb)`
- `itdb_splr_get_field_type(splr)` / `itdb_splr_get_action_type(splr)`

### Smart playlist rules (mapping to the .itl criteria)
```text
// ItdbSPLField — NOTE: values are non-contiguous, several are skipped
ITDB_SPLFIELD_SONG_NAME      = 0x02,  ALBUM      = 0x03,  ARTIST      = 0x04,
ITDB_SPLFIELD_BITRATE        = 0x05,  SAMPLE_RATE= 0x06,  YEAR       = 0x07,
ITDB_SPLFIELD_GENRE          = 0x08,  KIND       = 0x09,  DATE_MODIFIED= 0x0a,
ITDB_SPLFIELD_TRACKNUMBER    = 0x0b,  SIZE       = 0x0c,  TIME        = 0x0d,
ITDB_SPLFIELD_COMMENT        = 0x0e,  DATE_ADDED = 0x10,  COMPOSER    = 0x12,
ITDB_SPLFIELD_PLAYCOUNT      = 0x16,  LAST_PLAYED= 0x17,  DISC_NUMBER = 0x18,
ITDB_SPLFIELD_RATING         = 0x19,  COMPILATION= 0x1f,  BPM         = 0x23,
ITDB_SPLFIELD_GROUPING       = 0x27,  PLAYLIST   = 0x28,  PURCHASE    = 0x29,
ITDB_SPLFIELD_DESCRIPTION    = 0x36,  CATEGORY   = 0x37,  PODCAST     = 0x39,
ITDB_SPLFIELD_VIDEO_KIND     = 0x3c,  TVSHOW     = 0x3e,  SEASON_NR   = 0x3f,
ITDB_SPLFIELD_SKIPCOUNT      = 0x44,  LAST_SKIPPED= 0x45, ALBUMARTIST = 0x47,
ITDB_SPLFIELD_SORT_SONG_NAME = 0x4e,  SORT_ALBUM = 0x4f,  SORT_ARTIST = 0x50,
ITDB_SPLFIELD_SORT_ALBUMARTIST= 0x51, SORT_COMPOSER= 0x52, SORT_TVSHOW = 0x53,
ITDB_SPLFIELD_ALBUM_RATING   = 0x5a

// ItdbSPLAction
#define ITDB_SPLACTION_IS_INT              0x00000001
#define ITDB_SPLACTION_IS_GREATER_THAN     0x00000010
#define ITDB_SPLACTION_IS_LESS_THAN        0x00000040
#define ITDB_SPLACTION_IS_IN_THE_RANGE     0x00000100
#define ITDB_SPLACTION_IS_IN_THE_LAST      0x00000200
#define ITDB_SPLACTION_BINARY_AND          0x00000400
#define ITDB_SPLACTION_IS_STRING           0x01000001
#define ITDB_SPLACTION_CONTAINS            0x01000002
#define ITDB_SPLACTION_STARTS_WITH         0x01000004
#define ITDB_SPLACTION_ENDS_WITH           0x01000008
#define ITDB_SPLACTION_IS_NOT_INT          0x02000001
#define ITDB_SPLACTION_IS_NOT_GREATER_THAN 0x02000010
#define ITDB_SPLACTION_IS_NOT_LESS_THAN    0x02000040
#define ITDB_SPLACTION_IS_NOT_IN_THE_RANGE 0x02000100
#define ITDB_SPLACTION_IS_NOT_IN_THE_LAST  0x02000200
#define ITDB_SPLACTION_NOT_BINARY_AND      0x02000400
#define ITDB_SPLACTION_IS_NOT              0x03000001
#define ITDB_SPLACTION_DOES_NOT_CONTAIN    0x03000002
#define ITDB_SPLACTION_DOES_NOT_START_WITH 0x03000004
#define ITDB_SPLACTION_DOES_NOT_END_WITH   0x03000008

// ItdbSPLActionType
#define ITDB_SPLAT_STRING   1
#define ITDB_SPLAT_INT      2
#define ITDB_SPLAT_DATE     3
#define ITDB_SPLAT_RANGE_INT 4
#define ITDB_SPLAT_RANGE_DATE 5
#define ITDB_SPLAT_INTHELAST 6
#define ITDB_SPLAT_PLAYLIST 7
#define ITDB_SPLAT_NONE     8
#define ITDB_SPLAT_INVALID  9
#define ITDB_SPLAT_UNKNOWN 10
#define ITDB_SPLAT_BINARY_AND 11

// ItdbSPLFieldType
#define ITDB_SPLFT_STRING    1
#define ITDB_SPLFT_INT       2
#define ITDB_SPLFT_BOOLEAN   3
#define ITDB_SPLFT_DATE      4
#define ITDB_SPLFT_PLAYLIST  5
#define ITDB_SPLFT_UNKNOWN   6
#define ITDB_SPLFT_BINARY_AND 7
```

The field/action listings above abbreviate names for readability; they are not a standalone
C header. Use the pinned upstream `itdb.h` (or generated bindings), not copied definitions.

**Important**: `ITDB_SPLFIELD_COMPILATION = 0x1f` is a field identifier, not a bit mask or
sequential slot. The enum skips values. Mapping
criteria onto the iPod's smart-playlist engine must use these exact constants, not an
assumed sequential index.

**Rating mapping** (important for our .itl ↔ iPod rating conversion):
`#define ITDB_RATING_STEP 20` — so a 5-star rating = `5 * 20 = 100`.

### Artwork
- `itdb_artwork_new()` / `itdb_artwork_duplicate()` / `itdb_artwork_free()`
- `itdb_artwork_set_thumbnail_from_data(artwork, data, len, rotation, &error)`
- `itdb_artwork_get_pixbuf(device, artwork, width, height)`

### Time (Unix seconds at the public API)
libgpod exposes Unix timestamps, not raw `.itl` 1904 local-wall-clock values.
- `time_t itdb_time_get_mac_time(void)` — deprecated; returns current Unix seconds despite the name.
- `time_t itdb_time_mac_to_host(time_t mac)` — deprecated; returns the input unchanged.
- `time_t itdb_time_host_to_mac(time_t host)` — deprecated; returns the input unchanged.

Use `itl-sync`'s configured timestamp conversion on host writes. The compatibility helpers
do not implement the host library's epoch/timezone conversion; test playback-date round trips.

### Init
- `itdb_init_ipod(mountpoint, model_number, ipod_name, &error)` — initialize a blank iPod.
  This mutates the device; it must never be called automatically for discovery/preflight
  or dry run. Blank-device initialization is not added to this task's scope.

---

## 7. Constraints and risks

1. **Native backend/ABI**: validate the chosen packaged version, distro patches, build
   features, and GLib/native dependencies. Do not infer active maintenance from fork counts.
   Runtime loading still needs installed native dependencies and safe ownership handling.
2. **Required Rust wrapper**: implement and test the small safe FFI boundary in Phase 1,
   before discovery consumes it. Loading/linking remains an owner decision before B-007.
3. **No whole-device atomicity**: start/stop-sync offers no protection. A backup of two DB
   files alone is not a proven recovery protocol for media/art/stats across a multi-file write.
4. **Model support**: libgpod must generate the applicable checksum from verified device
   identity/capabilities. Model-table support is not physical acceptance.
5. **Recoverable host imports**: `.itl` and `iTunesDB` are distinct. Persist pending stats
   before consumption and deduplicate retries, including a crash after host commit but
   before acknowledgement. The concrete protocol requires review in B-010.
6. **Host format notes**: `MITH_FIELDS` in `tools/itl_inspect.py` and `NOTES.md` belong to
   the companion `itl-player` repository, not paths in Rufin. All host writes use `itl-sync`.

---

## 8. What was verified, and what was not

Source evidence is pinned at `7982c5554f78dde47fd006afbeff659201d6db3d`; links are collected
in the [owner decision](research-based-decisions.md#verified-upstream-references).

- The earlier registry lookup did not find a crate named `libpod`. This is not an exhaustive
  claim about every third-party Rust project.
- The earlier 65/65 API-name check checked declarations only. It did not verify ownership,
  wrapper soundness, transactions, recovery, or physical acceptance.
- Source/overview verification confirms mount-point parsing, device metadata location,
  model-table capacity, SysInfo reload's artwork warning, and model-specific checksum handling.
- Start/stop-sync explicitly does nothing on regular iPods. Source confirms Unix-time
  compatibility helpers and device stat-file cleanup on writes.
- No iOS Trust/pairing flow applies to the supported models. The corrected [preflight
  delta](s03_b004.md) requires more than a mount before Ready.
- Documentation checks can establish consistent states, references, and task dependencies;
  they are not runtime tests. No Rust wrapper, failure-injection suite, physical-device
  round trip, or Windows iTunes acceptance was completed as part of B-001.

Cleanup validation on 2026-11-06: 65 public API declarations and 79 numeric constants matched
the pinned header; the specific source semantics above were rechecked separately. Local
checks reported eight documents, 33 relative links/anchors, all 20 tasks in dependency
order, requirement references, the five-state FSM, and nine DeviceInfo fields. A later review
found an invalid B-001 self-dependency in that task list, now corrected. The feature's
constitution/spec match fork-master. These counts are documentation/source checks, not
unit-test or hardware-test counts; no Cargo tests were run for this Markdown-only cleanup.

## 9. Next steps

B-001 and B-002 have been merged into `feature/ipod-page`; the B-002 task-branch HEAD is
`eea14b0` and its contract is documented in the [plan](../plan.md). Before B-007, investigate
R1/R2 and obtain owner review of the resulting design, including loading and packaging.
B-007 integrates FFI before discovery.
B-010 must validate its concrete recovery/stat-merge protocol before destructive writes;
B-017 is per-model physical-device and subsequent iTunes acceptance.

## 10. Follow-up research required before implementation

These are open research items, not implemented fixes or approved design decisions. Before
starting each affected task, investigate its relevant items, record the evidence and test
plan, and review any resulting design/requirements changes with the owner. Use disposable
device-tree fixtures and host-library copies only; do not experiment on a connected live
device or production `.itl`. Backend-independent B-002 types do not resolve these items.

### R1: `itdb_write_file()` side effects

**Before B-007, and revisit before B-009/B-010:** Investigate the complete side effects of
`itdb_write_file()` when its output filename is outside an iPod but its `Itdb_iTunesDB`
still references a device mount. The [pinned source](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb_itunesdb.c#L5976-L6017)
invokes artwork writing using the database/device context, and its [write path](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb_itunesdb.c#L6141-L6147)
also calls stat-file cleanup using that mountpoint. The one-line description in §5 is not
a guarantee that writing to a separate filename confines all effects to that file.

Determine the exact files touched, cleanup paths, library/build/model conditions, and
failure behavior. Compare an entire disposable device tree and output directory before and
after successful and failed calls; include a separately targeted output file. Document how
test isolation, dry run, backups, and the wrapper will avoid unwanted live-device effects;
do not assume an isolated output filename is sufficient. No particular mitigation is
selected yet.

### R2: Artwork errors and apparent success

**Before B-007/B-004 and revisit before B-010:** Investigate how to detect and report
artwork failures in the chosen libgpod build. [`itdb_parse()`](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb_itunesdb.c#L3360-L3384)
does not check the artwork-parse result; the [write path](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb_itunesdb.c#L6005-L6017)
also calls the artwork writer without checking its result. Successful database parse/write
alone therefore does not establish successful artwork handling.

Research build-feature differences, error propagation, and suitable independent validation
or other approaches. On disposable fixtures, inject missing/corrupt artwork and write
failures and verify how they surface. Bring the resulting readiness, recovery, partial
success, and test-contract proposal for owner review before implementing the affected
wrapper, preflight, or sync behavior. No validation strategy is chosen here.

### R3: Device-name provenance discrepancy

**Before B-005:** Reconcile `tasks.md` B-005's wording that includes name in metadata
under `iPod_Control/Device/` with `plan.md`'s master-playlist name and volume-name fallback.
Examine libgpod sources and disposable real-device snapshots for where the display name is
actually available, including absent or conflicting values. Propose the source precedence,
missing-value behavior, and fixture assertions for owner review; then align the affected
plan/task wording before implementing metadata retrieval. The discrepancy remains open;
neither the task wording nor the plan's fallback is established by this note as the final
decision.
