# Research-based decisions: iPod sync

Date: 2026-11-06. Project: Rufin iPod integration. Repository-local record of the owner's approved decision, originally recorded in a session-local planning note. It supersedes the rejected filesystem-only recommendation: libgpod is required, not an optional fallback. The research, plan, tasks, and deltas must conform to this decision. Recording it does not claim implementation or hardware acceptance.

## Decision: libgpod is required behind a Rust FFI wrapper

Use libgpod for the device-side database and artwork layer, exposed through a small, safe Rust FFI wrapper. Do not treat it as an optional fallback to copying files. Rufin owns sync policy, comparison/delta calculation, progress, recovery, and host-library updates through `itl-sync`.

An iPod mounting as USB mass storage does not make copying files sufficient. libgpod already uses that filesystem transport. Replacing libgpod means implementing its device database, artwork formats, model-specific checksums, and playback-stat handling ourselves. That is a separate substantial project, not an approved alternative here.

## Files and device databases must stay consistent

Stock iPod firmware uses the device's database to expose music; copying audio into the media folder alone is not a completed sync. libgpod provides `itdb_cp_track_to_ipod()` for destination selection under `Music/Fxx`, copying, and updating transfer metadata. An application-owned copy routine is possible using `itdb_cp_get_dest_filename()` and `itdb_cp_finalize()`, but the corresponding database update remains mandatory.

Use libgpod to parse/write `iTunesDB`, maintain track and playlist references, and generate `ArtworkDB` plus model-specific `.ithmb` thumbnails. Embedded covers in audio files alone do not replace the iPod artwork database. The master playlist must contain the normal music tracks exposed through the device's music menus.

## Model-specific checksums are critical

iPod Classic models require a device-specific database checksum using the so-called FireWire ID, even over USB. Upstream warns that without the correct ID/checksum the iPod can behave as if its library were empty. Older Video models do not have the same checksum requirement; exact model identification matters. Other generations can require different checksum/database formats, so support must be bounded and verified per model. Rufin verifies prerequisites; libgpod generates the checksums.

`SysInfo` and `SysInfoExtended` live under `iPod_Control/Device/`, not inside `iTunesDB`. If capability information is missing, obtaining it can require USB/SCSI helpers and additional permissions. Do not assume that scanning mount points provides every prerequisite for a write. A model-table capacity is not measured filesystem capacity, and name/firmware availability must be established rather than assumed.

## No Trust step, and no transaction from start/stop sync

Video/Classic iPods have no iOS-style pairing or “Trust this computer” step. Discovery/preflight checks the mounted device and access to its required files, not authorization on the device. The shared FSM is defined in [the plan](../plan.md#connection-fsm); a mount alone is not Ready.

`itdb_start_sync()` and `itdb_stop_sync()` explicitly do nothing on regular iPods. They are not locks, transactions, rollback, or crash protection. The earlier claim that these calls provide transactional safety was false and has been removed from the corrected research.

Treat audio files, thumbnails, artwork databases, and the music database as a multi-file update requiring application-level safety. Define backups, write ordering, interrupted-sync recovery, exclusion of competing writers, and safe flushing/ejection before enabling destructive sync. A successful database write is not proof of an atomic whole-device transaction.

## Playback-stat import needs recoverable coordination

libgpod reads device playback-stat files, including `Play Counts`, and merges their information into its parsed state. Its write path resets/cleans up consumed stat files. Preserve pending imported statistics durably before a device write can consume them, and design retries so plays/skips are neither lost nor imported twice. Do not assume a database save is side-effect-free.

The device's `iTunesDB` and the host's `iTunes Library.itl` are different databases. libgpod must never write the host `.itl`; host changes go through `itl-sync`, preserving its atomic-write, backup, and sentinel busy-refusal rules. There is no shared transaction across host and device, so partial success requires explicit recovery. Numeric track IDs may be reassigned during database writes; cross-sync identity must not rely solely on them.

## FFI and acceptance requirements

The Rust wrapper must handle ownership, allocation/freeing, GLib strings/lists, `GError`, ABI compatibility, and access serialization explicitly. Runtime loading does not remove these obligations. Link-time versus runtime loading remains undecided. Builds without the backend must remain possible; if the required backend is unavailable, report sync as unavailable rather than silently falling back to file copying.

Verification of function names is not verification of behavior or crash safety. Test against disposable copies first, then perform physical-device acceptance for every supported model, including files, playlists, artwork, stats, interrupted operations, and subsequent iTunes compatibility. Never use the live host `.itl` as an experiment target.

## Timestamp clarification from source verification

The libgpod public API exposes Unix timestamps. The deprecated `itdb_time_mac_to_host()` and `itdb_time_host_to_mac()` return their input unchanged; `itdb_time_get_mac_time()` returns Unix seconds despite its name. They do not implement the host `.itl`'s 1904 local-wall-clock conversion. Use `itl-sync`'s configured conversion for host writes, as documented in [research](b001_research.md#time-unix-seconds-at-the-public-api).

## Verified upstream references

Source reviewed at libgpod commit `7982c5554f78dde47fd006afbeff659201d6db3d`; these links are pinned, not moving maintenance claims.

[Overview](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/README.overview) describes database formats, artwork capability discovery, and checksum differences. [FireWire ID requirements](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/README.SysInfo) explain why a Classic can appear empty after an incorrectly checksummed write.

[`itdb_itunesdb.c`](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb_itunesdb.c) contains file transfer/finalization, database writes and checksum invocation, playback-stat cleanup, Unix time helpers, and the explicit regular-iPod no-op behavior of start/stop sync.

[`ithumb-writer.c`](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/ithumb-writer.c) implements device-format thumbnail generation; [`itdb_device.c`](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb_device.c) selects model-specific checksums and warns that SysInfo reload invalidates artwork. [`itdb.h`](https://github.com/gtkpod/libgpod/blob/7982c5554f78dde47fd006afbeff659201d6db3d/src/itdb.h) is the declaration/constant reference, not proof of safe wrapper behavior.

## Related artifacts

[Research](b001_research.md), [corrected decision delta](b001_plan_delta.md), [preflight delta](s03_b004.md), [plan](../plan.md), and [tasks](../tasks.md) carry the feature-specific detail. Mainline owns [constitution](../constitution.md) and [spec](../spec.md); format notes remain in the companion `itl-player` repository, not a Rufin `NOTES.md`.
