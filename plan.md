# plan.md

## Objective
Implement an iPod page in Rufin to display device info and replicate the original iTunes sync experience (music files, artwork, playlists, and playback stats) with an iPod device.

## References and status
The owner-approved backend decision is preserved in [deltas/research-based-decisions.md](deltas/research-based-decisions.md); API evidence and its limits are in [B-001 research](deltas/b001_research.md). The [preflight delta](deltas/s03_b004.md) records the correction already incorporated into mainline `spec.md`. This is documentation cleanup, not implementation or physical-device acceptance.

## High-Level Phases

### Phase 1: Device Connection Infrastructure
- Define backend-independent `DeviceManager`, `ConnectionState`, and `DeviceInfo` types (B-002), using the contracts below.
- Integrate the small safe libgpod wrapper before discovery uses it (B-007). Handle ownership, allocation/freeing, GLib strings/lists, `GError`, ABI compatibility, and serialized access. Link-time versus runtime loading remains an owner decision before B-007; builds without the backend must remain possible and report sync unavailable, never fall back to file copying.
- Discover mounted candidates containing `iPod_Control` (B-003), then retrieve metadata and measured filesystem capacity (B-005). `SysInfo`/`SysInfoExtended` live under `iPod_Control/Device/`, not inside `iTunesDB`. No iOS pairing, Trust, or usbmuxd flow applies.
- Run non-mutating readiness checks (B-004) for access, parseable databases, supported model/artwork capabilities, capacity, and model-specific checksum prerequisites. Missing capability files can require USB/SCSI helpers and permissions; do not silently create them during discovery or dry run. Report the missing prerequisite instead.
- Let libgpod generate model-specific database checksums; Rufin verifies prerequisites rather than implementing a new checksum algorithm. Classic requires the correct FireWire ID even over USB; Video does not share that checksum requirement.
- Expose all connection states and actionable diagnostics for UI binding (B-006).

### Phase 2: Sync Core Logic (libgpod FFI)
- Use the Phase 1 wrapper to parse/write the device `iTunesDB`, maintain track/playlist references, and generate `ArtworkDB` plus model-specific `.ithmb` thumbnails.
- Rufin calculates comparisons/deltas and maps `.itl` content, artwork, playlists, and stats to libgpod models (B-008). libgpod is not a sync-policy or progress engine.
- Calculate dry-run deltas without changing the device, consuming playback statistics, or writing the host `.itl` (B-009). If an API can create auxiliary files on parse, use a disposable snapshot for that operation.
- Implement bidirectional sync plus durable recovery and stat-import coordination before enabling destructive writes (B-010; see Recovery and Statistics).
- Report application-owned progress, complete/partial/failed outcomes, and last-success time (B-011). Only complete device and host updates count as a successful sync.

### Phase 3: UI Implementation
- Keep the iPod page visible when disconnected; show No iPod, Connecting, Sync unavailable, Ready, or Error according to the FSM.
- Display name, model, firmware (or unavailable), and measured capacity.
- Bind Sync, Dry Run, progress, and error/recovery notices to manager state and operation results. Sync is enabled only in Ready; never show an iOS authorization prompt.

### Phase 4: Refinement & Testing
- Verify each supported Video/Classic model on physical hardware, including playlists, artwork, stats, failure recovery, safe flushing/ejection, and subsequent iTunes compatibility.
- Verify dry-run nonmutation and agreement with actual changes on disposable fixtures.
- Keep `.itl` as the content catalog source while importing device playback changes through `itl-sync`; never experiment on the live `.itl`.
- Keep research/decision links within this repository; companion format notes belong to `itl-player`, not a nonexistent local `NOTES.md`.

## Implementation Details

### Connection FSM
`ConnectionState` has exactly `Disconnected`, `Connecting`, `BackendUnavailable`, `Ready`, and `Error`. The UI label for `BackendUnavailable` is "Sync unavailable". This contract applies to Phase 1, B-002, and UI views alike.

- **Disconnected**: No candidate device. Initial state; sync disabled.
- **Connecting**: Candidate detected; metadata retrieval and preflight are pending. No handshake or authorization wait. Sync disabled.
- **BackendUnavailable**: Candidate mounted but required backend missing, disabled, or incompatible. Explain the backend problem; sync disabled. Retry returns to Connecting.
- **Ready**: Backend available and preflight passed for this mount/model; enable sync. A mount alone is never sufficient.
- **Error**: Access, parsing, unsupported-model, or readiness failure. Explain the cause and offer retry; sync disabled. Retry returns to Connecting.

Removal from any state returns to Disconnected and clears stale device information. If removal interrupts an operation, retain its incomplete/recovery result independently of connection state. Recheck readiness and competing-writer exclusion at the write boundary; Ready is not a transaction or a permanent lock. Operation progress and partial success are separate from `ConnectionState`.

### Data Models & Interfaces
`DeviceManager` starts with no current device and Disconnected state. It exposes owned device information, connection state, and diagnostic details without C pointers or a mandatory backend dependency; B-002 defines types/contracts only, not scanning or writes.

`DeviceInfo` uses `mount_point: PathBuf`, `name: String`, `model: String`, `generation: String`, `model_number: Option<String>`, `firmware: Option<String>`, `total_capacity: Option<u64>`, `used_capacity: Option<u64>`, and `firewire_id: Option<String>`. Capacities are measured bytes; `None` means unavailable, never a fabricated zero. Partial information can be exposed while Connecting or in Error; Ready requires identified supported-model capabilities and measured capacity. Missing firmware alone does not prevent Ready. FireWire ID is mandatory where that model's checksum requires it, not for every Video model.

Metadata provenance: libgpod identifies model/generation from device metadata and its model table; `Itdb_IpodInfo.capacity` is nominal model-table capacity, not measured storage usage. Use filesystem statistics for actual total/used space, the master playlist's name when available (otherwise a clearly identified volume-name fallback), and an optional firmware value only if actually exposed by device metadata. Do not assume SysInfoExtended contains every display field. Do not call `itdb_device_read_sysinfo()` as a refresh on a parsed database: upstream warns it invalidates artwork; rebuild the owned parse context instead.

`SyncProgress` reports the current item, total items, operation phase, and completed work. `SyncResult` distinguishes complete, partial, and failed outcomes, includes changes/errors, and references pending recovery work. Detailed sync records are designed in B-008/B-010, not part of B-002.

### Sync Data Mapping
- Use the existing iTunes source's `.itl` catalog and stable host persistent IDs; compare against parsed libgpod models, not just files on disk.
- Maintain the master playlist, playlist references, transfer metadata, and device artwork databases alongside copied audio. Embedded covers alone are insufficient.
- Use libgpod copying/finalization APIs or an application-owned copy routine with equivalent finalization. Rufin owns scheduling, delta calculation, progress, and policy.
- Ratings use the 0–100 scale (`ITDB_RATING_STEP = 20`). libgpod's public timestamps are Unix seconds; `.itl` uses local wall-clock seconds since 1904. Route host date writes through `itl-sync`'s configured conversion; do not use libgpod's deprecated no-op time converters.

### Recovery and Statistics
There is no shared transaction between device and host, and libgpod's start/stop-sync calls do nothing on regular iPods. B-010 must establish and test the backup set, durable operation record, write ordering, exclusion of competing writers, recovery procedure, and safe flushing/ejection before destructive sync is enabled. Preserve all files needed to restore a consistent previous library, not just `iTunesDB`; delay destructive removal of old media/art until it is recoverable.

Before a device write can consume `Play Counts` or related files, persist pending imported changes durably with stable host/device mapping and a retry-deduplication record. Numeric device track IDs may be reassigned and cannot be the sole cross-sync identity. Host updates use only `itl-sync`, including rolling backups and unconditional zero-sentinel refusal. A host write failure after device success is partial success with retained pending updates, not completion. Retry must neither lose changes nor import them twice, including interruption after a host commit but before its acknowledgement. The concrete recovery/deduplication protocol needs review in B-010 before writes; it is not claimed solved by this research.

### Testing Strategy
- **B-001 documentation checks**: references, state/type consistency, topological task order, upstream declarations and the specific source semantics cited in research. These are not runtime tests or hardware acceptance.
- **Unit Tests**: Backend-independent B-002 defaults/data contracts; mock discovery, FFI outcomes, and `itl-core` readers for later tasks. Cover each FSM transition and missing backend/prerequisites.
- **Integration Tests**: Env-gated `.itl` snapshot and disposable device-tree copies; wrapper ownership/error handling; dry-run byte-for-byte nonmutation; failure injection at recovery/stat-import boundaries, including repeat retries and reassigned track IDs.
- **Physical Device + VM Acceptance**: Per supported model, verify files, playlists, artwork, checksums, playback/skip/rating/date round trips, interrupted operations, and safe ejection. Open resulting disposable host-library copies in real Windows iTunes and verify device compatibility there. Never permit concurrent VM/host device writers or write the live `.itl` as a test.

### Risks and Tradeoffs
- **Old C backend**: Library version, distro patches, ABI, build features, and ownership need verification; fork activity is not evidence of maintenance or hardware support.
- **Multi-file writes**: A successful database save is not an atomic whole-device update. Recovery and safe flushing/ejection remain application obligations.
- **Model support**: Incorrect/missing Classic checksum identity can make the library appear empty. Unknown models/capabilities must not be treated as write-ready.
- **Playback-stat import**: Device writes can consume stat files; host write refusal and interrupted acknowledgements demand durable recovery and deduplication.
- **Open implementation decisions**: Loading/linking and packaging are deferred to owner review before B-007; the concrete recovery/stat-merge protocol to review before B-010. Neither blocks backend-independent B-002 types, and neither is silently settled by these documentation fixes.
