# spec.md

## Overview
Provide a dedicated UI in the Rufin player to connect to and sync with an iPod device. This page allows users to see the device status, capacity, and replicate the original iTunes sync experience (files, artwork, playlists, and playback statistics) between the local `.itl` library and the connected iPod.

**Target Devices**: iPod Video (5/5.5 gen) and iPod Classic (6/7 gen), using stock firmware. Write support must be verified for each supported model; detecting a mounted volume alone does not establish write support.

## User Stories
- As a user, I want the iPod page to be always visible in the UI, showing the connection status (Disconnected, Connecting, Sync unavailable, Ready, Error) and device info.
- As a user, I want to see a \"No iPod detected\" view when no device is connected.
- As a user, I want to see the device's model, firmware, and storage capacity.
- As a user, I want to trigger a \"Sync\" that replicates the original iTunes sync experience, including music files, artwork, playlists, and playback statistics.
- As a user, I want a \"Dry Run\" option for the sync so I can see what changes will be applied before they are committed to the device.
- As a user, I want to see the progress of the sync operation.

## Requirements
### R-01: Page Visibility
- The iPod page is accessible in the sidebar.
- **S-01: No device connected**
  - Given no device is connected, When the page is accessed, Then show a \"No iPod detected\" view with a status indicator.

### R-02: Connection FSM
- **S-02: Device connection**
  - Given a device is physically connected, When the app detects it, Then transition to \"Connecting\".
- **S-03: Device preflight (no Trust step)**
  - Given a supported mounted iPod and an available backend, When preflight verifies access to the required files, readable device databases, and model-specific write prerequisites, Then transition from \"Connecting\" to \"Ready\" without pairing or showing an authorization prompt.
- **S-04: Ready**
  - Given preflight succeeds, When the device enters \"Ready\", Then show the device overview and enable sync.
- **S-05: Error handling**
  - Given access is denied, the device database cannot be read, or the device's model-specific write prerequisites are missing, When preflight runs, Then show the specific error and a retry option and keep sync disabled.
  - Given a mounted iPod but an unavailable backend, When connection is attempted, Then show \"Sync unavailable\", explain the backend problem, and keep sync disabled without falling back to copying files.
  - Given a detected device is removed, When removal is observed, Then return to \"Disconnected\" and disable sync; if an operation was interrupted, also report the incomplete operation as required by S-12.

### R-03: Device Info
- **S-06: Info display**
  - Given a device is in \"Ready\" state, When the user views the device overview, Then the UI displays the correct name, model, firmware, and capacity.
  - Given the device does not expose a firmware version, When its overview is displayed, Then show the firmware as unavailable rather than inventing a value.

### R-04: Sync Operation
- **S-07: Initiation**
  - Given a device is in \"Ready\" state, When the \"Sync\" button is clicked, Then initiate a full sync of music files, artwork, playlists, and playback statistics.
- **S-08: Dry Run**
  - Given the \"Dry Run\" checkbox is enabled, When a sync is started, Then calculate the differences and show them to the user without changing the device, consuming its playback statistics, or writing the host `.itl`.
- **S-09: Progress**
  - Given a sync is in progress, When it is running, Then show a real-time progress bar.
- **S-10: Completion**
  - Given all device and host updates complete, When the sync finishes, Then update the \"Last Sync\" timestamp and show a success message; partial success must not be reported as a completed sync.
- **S-11: State constraints**
  - Given the iPod is not in 'Ready' state, When the user attempts to click 'Sync', Then the 'Sync' button should be disabled and a message should indicate that the device is not ready.
- **S-12: Error handling (Sync)**
  - Given a sync is in progress, When the disk is full or the connection is lost, Then the sync must halt safely and report the specific error.
  - Given a sync is interrupted, When the device is available for recovery, Then recover the last consistent device library from durable recovery information before allowing another destructive sync. Preserve the required files and statistics for recovery and report the operation as incomplete until recovery is verified; do not assume the backend provides an atomic whole-device transaction.
  - Given another writer is using the device, When sync is requested, Then refuse the device write and report the conflict rather than modify the library concurrently.

### R-05: Data Integration
- The sync operation must replicate the iTunes \"Sync\" logic as closely as possible.
- **S-13: Bidirectional Statistics**
  - Given a sync is initiated, When it runs, Then the sync must not only push statistics to the device but also merge device-side playback statistics back into the local `.itl` file (respecting the sentinel-guarded atomic write path).
  - Given pending device playback statistics, When a device write could consume them, Then preserve those statistics durably first so retrying an interrupted sync neither loses nor imports them twice, even if numeric device track IDs change.
  - Given the device update succeeds but host write-back is refused or fails, When the operation is reported, Then retain the pending host changes for recovery and report partial success; do not bypass the host sentinel check or replay already-imported statistics.

### R-06: Safe backend wrapper and non-mutating reads
The libgpod backend is accessed exclusively through the safe `ipod` wrapper crate. Reads are provably non-mutating on well-formed trees, the device model is never invented, artwork state is reported explicitly instead of trusting parse success, native access is serialized with an ABI check at load, and write APIs exist but are not reachable from any enabled sync path until B-010.

- **S-14: Backend loading and unavailability**
  - Given Rufin needs the backend, When the shared library or a required symbol is missing, incompatible, or fails to load, Then `IpodBackend::load()` returns an actionable `BackendUnavailable` and never panics, crashes, or falls back to file copying.
  - Given a mounted supported iPod but an unavailable backend, When connection is attempted, Then the FSM shows "Sync unavailable" with the backend diagnostic and keeps sync disabled.

- **S-15: Session surface (read side)**
  - Given a mount point, When `open()` succeeds, Then `DeviceSession` provides owned results for `device_info`, `tracks`, `playlists`, `artwork_status`, and `playback_stats`, each returning a structured `IpodError` on failure instead of exposing native pointers, GLib lists, or `GError` handles. All borrowed native strings/lists are converted to owned Rust values before any call returns; `itdb_free` is paired with the session drop.
  - Given a parse failure, When a read call is made, Then the session reports the diagnostic without leaking or double-freeing native memory.

- **S-16: Model identification is honest**
  - Given a device whose `SysInfo` is empty or unreadable, When `device_info()` runs, Then the model is reported as unknown (`None`), never silently resolved to a default model, and the FSM must not reach Ready on that basis alone (S-03/S-05; readiness is B-004).
  - Given an empty `SysInfo`, When the wrapper parses the tree, Then the silent artwork-skip behavior documented in the B-007 research must be visible in `artwork_status` (S-17), not hidden by parse success.

- **S-17: Artwork status is explicit**
  - Given a parsed session, When `artwork_status()` runs, Then the wrapper reports (a) whether artwork parsing ran at all (model gate), (b) how many tracks carry decoded thumbnails, and (c) a verdict against the independently counted ArtworkDB entries — `Ok`, `Missing` (declared entries without parsed thumbnails), or `SkippedUnknownModel` — with a diagnostic, rather than relying on `itdb_parse`'s unchecked success.
  - Given the restored-device snapshot with seeded model, When `artwork_status()` runs, Then 3356 tracks with thumbnails is reported as `Ok`.
  - Given the same snapshot with empty SysInfo, When `artwork_status()` runs, Then `SkippedUnknownModel` is reported even though `itdb_parse` succeeded.
  - Given a fixture with the ArtworkDB removed, When `artwork_status()` runs, Then `Missing` is reported while track parsing still succeeds.

- **S-18: Reads are provably non-mutating**
  - Given any read-side wrapper call, When it runs against a disposable snapshot, Then the snapshot tree hash (files and directories) is unchanged; the wrapper never invokes libgpod entry points that can create or unlink device files (`itdb_init_ipod`, `itdb_write*`, hash-info extraction, artwork-dir creation) on read paths. Where an upstream read can mutate, the wrapper documents it and tests run only on disposable copies.

- **S-19: Serialized access and ABI check**
  - Given concurrent calls, When any wrapper function runs, Then access to the native library is serialized (single mutex around load/session/drop), and `IpodBackend::load()` verifies the loaded library's declared version/ABI before use; an ABI mismatch yields `BackendUnavailable`, not undefined behavior.

- **S-20: Write operations are defined but gated**
  - Given the sync engine, When it needs create/update/remove track, playlist changes, artwork set, or `write_library`, Then the wrapper defines these on `DeviceSession` but they are not reachable from any enabled sync path until B-010's safety verification; B-007 ships them behind a not-for-production gate.

## Out of Scope
- Support for the iOS family (iPod touch) and all non-iPod devices (initial scope).
- Manual file management on the device outside of the sync process.
- Real-time synchronization (the sync is a discrete action).
