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

## Out of Scope
- Support for the iOS family (iPod touch) and all non-iPod devices (initial scope).
- Manual file management on the device outside of the sync process.
- Real-time synchronization (the sync is a discrete action).
