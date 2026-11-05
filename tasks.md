# tasks.md

## Phase 1: Device Connection Infrastructure

- [ ] **B-001: Research libpod and connection flow for classic iPods**
  - Spec reference: R-02
  - Plan reference: Phase 1
  - Verification: Document the specific API calls for detection, pairing (preflight), and session establishment using libpod instead of libimobiledevice. Note if an authorization/trust step exists for classic models; if not, a delta spec will be created to adjust S-03 and B-004.
- [ ] **B-002: Define DeviceManager and ConnectionState types**
  - Spec reference: R-02
  - Plan reference: Phase 1
  - Verification: Types are defined in `crates/gtk-library/src/device_manager.rs` (or similar).
  - Depends on: B-001
- [ ] **B-003: Implement Device Discovery using libpod/libusb**
  - Spec reference: R-02
  - Plan reference: Phase 1
  - Verification: `DeviceManager` can list and identify connected classic iPods.
  - Depends on: B-001
- [ ] **B-004: Implement connection handshake and authorization (Preflight)**
  - Spec reference: R-02, S-05
  - Plan reference: Phase 1
  - Verification: App correctly transitions to \"Waiting for Authorization\" and waits for user trust.
  - Depends on: B-003
- [ ] **B-005: Implement DeviceInfo retrieval**
  - Spec reference: R-03
  - Plan reference: Phase 1
  - Verification: `DeviceInfo` correctly populates name, model, firmware, and capacity.
  - Depends on: B-004
- [ ] **B-006: Expose connection status for UI binding**
  - Spec reference: R-02
  - Plan reference: Phase 1
  - Verification: UI can react to state changes (Connecting, Ready, etc.).
  - Depends on: B-005

## Phase 2: Sync Core Logic (libpod)

- [ ] **B-007: Integrate libpod into project (Contingent on B-001)**
  - Spec reference: R-04
  - Plan reference: Phase 2
  - Verification: `libpod` is linked and accessible in the sync module.
  - Depends on: B-001
- [ ] **B-008: Map .itl library data to libpod models**
  - Spec reference: R-04
  - Plan reference: Phase 2
  - Verification: `.itl` tracks/albums can be converted to `libpod` sync items.
  - Depends on: B-007
- [ ] **B-009: Implement Dry Run delta calculation**
  - Spec reference: R-04, S-08
  - Plan reference: Phase 2
  - Verification: A \"Dry Run\" produces a list of changes without writing to the device.
  - Depends on: B-008
- [ ] **B-010: Implement full bidirectional sync (Files, Art, Playlists, Stats)**
  - Spec reference: R-04, S-07, S-12, R-05, S-13
  - Plan reference: Phase 2
  - Verification: Files, metadata, and playback stats are correctly synced (bidirectional).
  - Depends on: B-009
- [ ] **B-011: Implement sync progress tracking**
  - Spec reference: R-04, S-09, S-10
  - Plan reference: Phase 2
  - Verification: Progress bar updates as `libpod` processes items.
  - Depends on: B-010

## Phase 3: UI Implementation

- [ ] **B-012: Create iPod Page base layout**
  - Spec reference: R-01
  - Plan reference: Phase 3
  - Verification: Page is rendered in Rufin.
  - Depends on: B-011
- [ ] **B-013: Implement Connection State views**
  - Spec reference: R-02
  - Plan reference: Phase 3
  - Verification: UI shows \"No iPod\", \"Connecting\", \"Waiting for Auth\", etc.
  - Depends on: B-012
- [ ] **B-014: Implement Device Overview view**
  - Spec reference: R-03
  - Plan reference: Phase 3
  - Verification: Name, Model, Firmware, and Capacity are displayed in \"Ready\" state.
  - Depends on: B-013
- [ ] **B-015: Implement Sync button with Dry Run checkbox**
  - Spec reference: R-04, S-07, S-11
  - Plan reference: Phase 3
  - Verification: Button is disabled unless \"Ready\"; \"Dry Run\" checkbox is present.
  - Depends on: B-014
- [ ] **B-016: Connect UI state to DeviceManager**
  - Spec reference: R-02
  - Plan reference: Phase 3
  - Verification: UI updates automatically when `DeviceManager` state changes.
  - Depends on: B-015

## Phase 4: Refinement & Testing

- [ ] **B-017: Conduct real-device full sync testing**
  - Spec reference: R-04
  - Plan reference: Phase 4
  - Verification: Music files, artwork, and stats are correctly updated on a physical iPod.
  - Depends on: B-016
- [ ] **B-018: Verify Dry Run accuracy**
  - Spec reference: R-04, S-08
  - Plan reference: Phase 4
  - Verification: Comparison between Dry Run output and actual Sync results matches.
  - Depends on: B-017
- [ ] **B-019: Final UI Polish and performance optimization**
  - Spec reference: R-04
  - Plan reference: Phase 4
  - Verification: UI remains responsive (FPS > 30) during heavy sync activity; sync doesn't block the main thread.
  - Depends on: B-018
- [ ] **B-020: Document B-001 research results in NOTES.md**
  - Plan reference: Phase 4
  - Verification: NOTES.md is updated with libpod connection flow details and any constraints identified.
  - Depends on: B-001