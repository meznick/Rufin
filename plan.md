# plan.md

## Objective
Implement an iPod page in Rufin to display device info and replicate the original iTunes sync experience (music files, artwork, playlists, and playback stats) with an iPod device.

## High-Level Phases

### Phase 1: Device Connection Infrastructure
- Research target for USB communication for classic iPod identification using `libpod` and `libusb`.
- Define the `DeviceManager` and its connection state machine (Connecting, WaitingForAuth, Ready, Error).
- Implement device detection and authorization logic (specifically for classic models, skipping `libimobiledevice` flow).
- Define data structures for `DeviceInfo` (name, model, firmware, capacity).
- Expose connection status for UI binding.

### Phase 2: Sync Core Logic (libpod)
- Integrate `libpod` to handle core synchronization logic for classic iPods.
- Map `.itl` library data (files, artwork, playlists, stats) to the formats required by `libpod`.
- Implement the \"Dry Run\" logic to calculate deltas without writing to the device.
- Implement bidirectional sync:
  - Push library data (files, art, playlists, stats) to device.
  - Pull device playback statistics and merge them back into the local `.itl` file via `itl-sync`.
- Implement progress tracking for the sync operation.

### Phase 3: UI Implementation
- Create the iPod page layout in Rufin.
- Implement the connection state view (No iPod, Connecting, Waiting for Authorization, Ready, Error).
- Implement the main device overview (Name, Model, Firmware, Capacity).
- Implement the Sync button with a \"Dry Run\" checkbox and progress bar.
- Handle page visibility based on connection state (always visible, showing \"No iPod detected\" when disconnected).

### Phase 4: Refinement & Testing
- Conduct real-device testing for connection and full sync (bidirectional).
- Verify that \"Dry Run\" correctly identifies changes without modifying the iPod.
- Ensure `.itl` data is the source of truth for all synced content.
- Final Polish.

## Implementation Details

### Connection FSM
- **Disconnected**: No device detected.
- **Connecting**: Device detected, attempting to establish handshake.
- **WaitingForAuth**: Handshake successful, waiting for user to authorize on device (if applicable).
- **Ready**: Authorized, ready for sync.
- **Error**: Connection or authorization failed.

### Sync Data Mapping
- Use `libpod` to perform the comparison between the local library and the device.
- Map `.itl` track/album data to `libpod` metadata structures.
- Handle file existence and metadata updates (rating, play count) within the `libpod` sync loop.
- Track progress based on the number of files/entries being processed.

### Data Models & Interfaces
- `DeviceInfo`: `name: String`, `model: String`, `firmware: String`, `used_capacity: u64`, `total_capacity: u64`.
- `SyncProgress`: `current_item: Option<TrackId>`, `total_items: usize`, `status: SyncStatus`.
- `SyncResult`: `changes: Vec<Change>`, `errors: Vec<SyncError>`.

### Testing Strategy
- **Unit Tests**: Mock `libpod` calls and `itl-core` readers.
- **Integration Tests**: Env-gated tests using `ITL_TEST_PATH` snapshot and a mocked storage backend.
- **VM Acceptance**: Perform real-device round-trip sync in a Windows VM to verify `.itl` consistency and `libpod` behavior.

### Risks and Tradeoffs
- **libpod Maturity**: `libpod` is the core of the sync; stability depends on its handling of specific iPod hardware revisions.
- **Corrupted iTunesDB**: Simultaneous writes or interrupted syncs could corrupt the device's `iTunesDB`. Sync must be atomic/transactional where possible.
- **Sentinel Conflict**: Ensure `itl-sync`'s sentinel check is correctly integrated into the write-back path for device statistics.
- **Bidirectional Complexity**: Merging device-side stats into `.itl` requires careful handling of play counts and timestamps to avoid overwriting manual user edits.
- **Data Source**: The iPod page will use the existing Rufin `iTunesSource` implementation to read the `.itl` file.