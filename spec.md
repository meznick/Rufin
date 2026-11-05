# spec.md

## Overview
Provide a dedicated UI in the Rufin player to connect to and sync with an iPod device. This page allows users to see the device status, capacity, and replicate the original iTunes sync experience (files, artwork, playlists, and playback statistics) between the local `.itl` library and the connected iPod.

**Target Devices**: Classic iPod video models (5/5.5/6/7 gen).

## User Stories
- As a user, I want the iPod page to be always visible in the UI, showing the connection status (Connecting, Authorized, Ready, Error, etc.) and device info.
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
- **S-03: Authorization/Handshake**
  - Given the handshake is successful, When the trust/ready signal arrives, Then transition to \"Waiting for Authorization\".
- **S-04: Ready**
  - Given the device is authorized/ready, When the connection is established, Then transition to \"Ready\" and show the device overview.
- **S-05: Error handling**
  - Given a connection or authorization error occurs, When it is detected, Then show the specific error and a retry option.

### R-03: Device Info
- **S-06: Info display**
  - Given a device is in \"Ready\" state, When the user views the device overview, Then the UI displays the correct name, model, firmware, and capacity.

### R-04: Sync Operation
- **S-07: Initiation**
  - Given a device is in \"Ready\" state, When the \"Sync\" button is clicked, Then initiate a full sync of music files, artwork, playlists, and playback statistics.
- **S-08: Dry Run**
  - Given the \"Dry Run\" checkbox is enabled, When a sync is started, Then calculate the differences and show them to the user without writing any changes to the device.
- **S-09: Progress**
  - Given a sync is in progress, When it is running, Then show a real-time progress bar.
- **S-10: Completion**
  - Given a sync completes, When it finishes, Then update the \"Last Sync\" timestamp and show a success message.
- **S-11: State constraints**
  - Given the iPod is not in 'Ready' state, When the user attempts to click 'Sync', Then the 'Sync' button should be disabled and a message should indicate that the device is not ready.
- **S-12: Error handling (Sync)**
  - Given a sync is in progress, When the disk is full or the connection is lost, Then the sync must halt safely and report the specific error.
  - Given a sync is in progress, When an interrupt occurs, Then the system must ensure the `iTunesDB` is not left in a corrupted state (atomic write/transactional safety).

### R-05: Data Integration
- The sync operation must replicate the iTunes \"Sync\" logic as closely as possible.
- **S-13: Bidirectional Statistics**
  - Given a sync is initiated, When it runs, Then the sync must not only push statistics to the device but also merge device-side playback statistics back into the local `.itl` file (respecting the sentinel-guarded atomic write path).

## Out of Scope
- Support for the iOS family (iPod touch) and all non-iPod devices (initial scope).
- Manual file management on the device outside of the sync process.
- Real-time synchronization (the sync is a discrete action).