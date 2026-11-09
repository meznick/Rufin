//! B-003: feed discovery results into the connection FSM.
//!
//! The glue between `ipod_sync::Discovery` and B-002's [`DeviceManager`].
//! One [`apply_scan`] call per scan report keeps the manager in sync with
//! the mount table: a detected candidate enters `Connecting`, a missing
//! backend shows `BackendUnavailable` ("Sync unavailable") with the reason
//! as the diagnostic, and a removed mount returns to `Disconnected`.
//!
//! The glue **never marks a candidate Ready** — `set_ready_after_preflight`
//! is B-004's job and is not called anywhere in this module (asserted by
//! the tests). Blocked mounts (probe denied) are report-only: a blocked
//! mount is not a confirmed iPod, and confirmed-candidate access failures
//! are B-004's preflight territory.

use std::path::Path;

use ipod_sync::{BackendStatus, ScanReport};

use crate::device_manager::{ConnectionState, DeviceInfo, DeviceManager};

/// The `DeviceInfo` for a freshly detected candidate: only the mount point
/// is known. `name`/`model`/`generation` are empty strings meaning "not
/// retrieved yet" — B-005 replaces them via [`DeviceManager::set_device`],
/// which re-enters `Connecting`. Optional fields stay `None`, never
/// fabricated values.
fn detected_device(mount_point: &Path) -> DeviceInfo {
    DeviceInfo {
        mount_point: mount_point.to_path_buf(),
        name: String::new(),
        model: String::new(),
        generation: String::new(),
        model_number: None,
        firmware: None,
        total_capacity: None,
        used_capacity: None,
        firewire_id: None,
    }
}

/// Apply one discovery scan to the FSM. Never reaches `Ready`.
///
/// Multiple simultaneous candidates are out of scope: the first by sorted
/// mount point wins (the report itself lists all of them).
pub fn apply_scan(manager: &mut DeviceManager, report: &ScanReport) {
    // 1. Removal: the current device's mount left the candidate set.
    if let Some(mount) = manager
        .current_device()
        .map(|device| device.mount_point.clone())
    {
        if report.removed.contains(&mount) {
            manager.clear_device();
        }
    }

    // 2. Candidate handling. No candidates → nothing to do; unrelated and
    //    blocked-only scans leave the FSM alone.
    let Some(candidate) = report.candidates.first() else {
        return;
    };
    let mount = &candidate.mount_point;
    let same_device = manager
        .current_device()
        .is_some_and(|device| device.mount_point == *mount);

    match &report.backend {
        BackendStatus::Loaded { .. } => {
            if !same_device || manager.state() == ConnectionState::Disconnected {
                manager.set_device(detected_device(mount));
            } else if manager.state() == ConnectionState::BackendUnavailable {
                // The backend recovered (loads are retried until success):
                // retry means a fresh Connecting, per S-05.
                manager.set_device(detected_device(mount));
            }
            // Same candidate while Connecting/Error: leave the state and the
            // diagnostics alone — a rescan must not clobber a pending
            // preflight.
        }
        BackendStatus::Unavailable { reason } => {
            if !same_device || manager.state() == ConnectionState::Disconnected {
                manager.set_device(detected_device(mount));
            }
            if manager.state() != ConnectionState::BackendUnavailable {
                manager.set_state(ConnectionState::BackendUnavailable);
            }
            manager.set_diagnostics(reason.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipod_sync::{Candidate, ScanReport};

    fn candidate_report(candidates: Vec<Candidate>, backend: BackendStatus) -> ScanReport {
        ScanReport {
            backend,
            candidates,
            blocked: Vec::new(),
            appeared: Vec::new(),
            removed: Vec::new(),
        }
    }

    fn candidate(mount_point: &str) -> Candidate {
        Candidate {
            mount_point: mount_point.into(),
            fstype: "vfat".to_string(),
        }
    }

    const LOADED: BackendStatus = BackendStatus::Loaded { abi_verified: true };

    fn unavailable(reason: &str) -> BackendStatus {
        BackendStatus::Unavailable {
            reason: reason.to_string(),
        }
    }

    #[test]
    fn candidate_with_backend_enters_connecting() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod")
        );
        assert!(
            !manager.sync_enabled(),
            "detection alone never enables sync"
        );
    }

    #[test]
    fn candidate_without_backend_enters_backend_unavailable() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert!(
            manager.current_device().is_some(),
            "the device info is retained"
        );
        assert_eq!(manager.diagnostics(), Some("dlopen failed"));
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn removal_returns_to_disconnected_and_clears_device() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan(&mut manager, &removal);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
    }

    #[test]
    fn unrelated_volumes_leave_manager_untouched() {
        let mut manager = DeviceManager::new();
        apply_scan(&mut manager, &candidate_report(Vec::new(), LOADED));
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
        assert_eq!(manager.diagnostics(), None);
    }

    #[test]
    fn rescan_with_same_candidate_is_idempotent() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        // Simulate a pending preflight diagnostic; a rescan must not clobber
        // it or reset the state.
        manager.set_diagnostics("preflight pending".to_string());
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
        assert_eq!(manager.diagnostics(), Some("preflight pending"));
    }

    #[test]
    fn backend_recovery_returns_to_connecting() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        assert_eq!(
            manager.state(),
            ConnectionState::Connecting,
            "a recovered backend means a fresh Connecting, per S-05"
        );
    }

    #[test]
    fn reappear_after_removal_enters_connecting() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan(&mut manager, &removal);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
    }

    #[test]
    fn blocked_mounts_leave_manager_untouched() {
        let mut manager = DeviceManager::new();
        let mut report = candidate_report(Vec::new(), LOADED);
        report.blocked = vec![ipod_sync::BlockedMount {
            mount_point: "/mnt/other".into(),
            error: "Permission denied (os error 13)".to_string(),
        }];
        apply_scan(&mut manager, &report);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
    }

    #[test]
    fn glue_never_marks_ready() {
        // Walk the glue through every scenario and assert the state never
        // reaches Ready after any step.
        let mut manager = DeviceManager::new();
        let scenarios: Vec<ScanReport> = vec![
            candidate_report(Vec::new(), LOADED),
            candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
            candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            candidate_report(Vec::new(), LOADED),
            candidate_report(
                vec![candidate("/mnt/ipod")],
                BackendStatus::Loaded {
                    abi_verified: false,
                },
            ),
        ];
        for report in &scenarios {
            apply_scan(&mut manager, report);
            assert_ne!(
                manager.state(),
                ConnectionState::Ready,
                "discovery glue must never mark Ready (report: {report:?})"
            );
        }
    }
}
