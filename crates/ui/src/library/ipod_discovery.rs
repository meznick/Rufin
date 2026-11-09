//! B-003 + B-005: feed discovery results and retrieved metadata into the
//! connection FSM.
//!
//! The glue between `ipod_sync` (discovery + retrieval) and B-002's
//! [`DeviceManager`]. One [`apply_scan`] call per scan report keeps the
//! manager in sync with the mount table: a detected candidate enters
//! `Connecting` with its metadata retrieved (B-005), a missing backend
//! shows `BackendUnavailable` ("Sync unavailable") with the reason as the
//! diagnostic, a failed retrieval shows `Error` with the failure as the
//! diagnostic, and a removed mount returns to `Disconnected`.
//!
//! The glue **never marks a candidate Ready** — `set_ready_after_preflight`
//! is B-004's job and is not called anywhere in this module (asserted by
//! the tests). Retrieval runs once per device appearance (new candidate or
//! backend recovery): a same-candidate rescan must not re-parse the device,
//! and must not clobber a pending diagnostic. Blocked mounts (probe denied)
//! are report-only: a blocked mount is not a confirmed iPod, and
//! confirmed-candidate access failures are B-004's preflight territory.

use std::path::Path;

use ipod_sync::{BackendStatus, DeviceFacts, RetrieveError, ScanReport};

use crate::device_manager::{ConnectionState, DeviceInfo, DeviceManager};

/// The `DeviceInfo` for a freshly detected candidate whose metadata could
/// not be retrieved: only the mount point is known. `name`/`model`/
/// `generation` are empty strings meaning "not retrieved" — never
/// fabricated values. Optional fields stay `None`.
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

/// Map retrieved B-005 facts into the B-002 [`DeviceInfo`] the manager
/// carries. The mount point comes from the scan candidate (the facts are
/// mount-agnostic on purpose).
fn device_info_from_facts(facts: DeviceFacts, mount_point: &Path) -> DeviceInfo {
    DeviceInfo {
        mount_point: mount_point.to_path_buf(),
        name: facts.name,
        model: facts.model,
        generation: facts.generation,
        model_number: facts.model_number,
        firmware: facts.firmware,
        total_capacity: facts.total_capacity,
        used_capacity: facts.used_capacity,
        firewire_id: facts.firewire_id,
    }
}

/// Install the current candidate into the manager with its metadata
/// retrieved. A retrieval failure keeps the mount point (the candidate is
/// still confirmed) and moves the FSM to `Error` with the failure as the
/// actionable diagnostic — never Ready, sync disabled.
fn set_retrieved_device(
    manager: &mut DeviceManager,
    mount: &Path,
    retrieve: &dyn Fn(&Path) -> Result<DeviceFacts, RetrieveError>,
) {
    match retrieve(mount) {
        Ok(facts) => manager.set_device(device_info_from_facts(facts, mount)),
        Err(error) => {
            manager.set_device(detected_device(mount));
            manager.set_state(ConnectionState::Error);
            manager.set_diagnostics(error.to_string());
        }
    }
}

/// Apply one discovery scan to the FSM. Never reaches `Ready`.
///
/// `retrieve` opens a session on the candidate mount and collects its
/// metadata (B-005; in production a closure over `Discovery::backend()`).
/// It runs only when the candidate newly becomes current — new device,
/// backend recovery — so rescans never re-parse the device. Tests inject a
/// stub; a `DeviceFacts::default()` stub behaves like the old
/// metadata-less placeholder.
///
/// Multiple simultaneous candidates are out of scope and follow the
/// deliberate **first-wins** policy (owner-approved): the report carries all
/// of them, but the FSM follows the first by sorted mount point — and only
/// that one is retrieved. Removing the current device hands the FSM over to
/// the next candidate; removing a non-current one is invisible to the FSM;
/// plugging in a device that sorts before the current one takes the FSM
/// over (first wins, not first seen). All of it is tested below.
pub fn apply_scan(
    manager: &mut DeviceManager,
    report: &ScanReport,
    retrieve: &dyn Fn(&Path) -> Result<DeviceFacts, RetrieveError>,
) {
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
                set_retrieved_device(manager, mount, retrieve);
            } else if manager.state() == ConnectionState::BackendUnavailable {
                // The backend recovered (loads are retried until success):
                // retry means a fresh Connecting with a fresh retrieval,
                // per S-05.
                set_retrieved_device(manager, mount, retrieve);
            }
            // Same candidate while Connecting/Error: leave the state and the
            // diagnostics alone — a rescan must not re-parse the device or
            // clobber a pending diagnostic.
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

    /// The behavior-neutral retrieval stub: empty facts, the shape of the
    /// pre-B-005 placeholder device.
    fn no_retrieval(_mount: &Path) -> Result<DeviceFacts, RetrieveError> {
        Ok(DeviceFacts::default())
    }

    #[test]
    fn candidate_with_backend_enters_connecting() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &no_retrieval,
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
            &no_retrieval,
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
            &no_retrieval,
        );
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan(&mut manager, &removal, &no_retrieval);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
    }

    #[test]
    fn unrelated_volumes_leave_manager_untouched() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(Vec::new(), LOADED),
            &no_retrieval,
        );
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
            &no_retrieval,
        );
        // Simulate a pending preflight diagnostic; a rescan must not clobber
        // it or reset the state.
        manager.set_diagnostics("preflight pending".to_string());
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &no_retrieval,
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
            &no_retrieval,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &no_retrieval,
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
            &no_retrieval,
        );
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan(&mut manager, &removal, &no_retrieval);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &no_retrieval,
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
        apply_scan(&mut manager, &report, &no_retrieval);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
    }

    /// First-wins policy: with two candidates in one scan, only the first
    /// (sorted by mount point) becomes the current device.
    #[test]
    fn multiple_candidates_only_first_gets_fsm() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &no_retrieval,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
    }

    /// Removing the active device hands the FSM over to the next candidate.
    #[test]
    fn device_handover_when_active_removed() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &no_retrieval,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
        // The first device unmounts; the second is still there.
        let mut next = candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED);
        next.removed = vec!["/mnt/ipod-aaa".into()];
        apply_scan(&mut manager, &next, &no_retrieval);
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-bbb"),
            "the FSM must hand over to the remaining candidate"
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
    }

    /// Removing a non-current device leaves the FSM alone: the active
    /// candidate is still mounted and stays current.
    #[test]
    fn non_active_removal_is_invisible() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &no_retrieval,
        );
        let mut next = candidate_report(vec![candidate("/mnt/ipod-aaa")], LOADED);
        next.removed = vec!["/mnt/ipod-bbb".into()];
        apply_scan(&mut manager, &next, &no_retrieval);
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
    }

    /// First wins, not first seen: plugging in a device that sorts before
    /// the current one takes the FSM over (deliberate, owner-approved).
    #[test]
    fn lower_sorting_candidate_takes_over() {
        let mut manager = DeviceManager::new();
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED),
            &no_retrieval,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-bbb")
        );
        apply_scan(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &no_retrieval,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa"),
            "the first candidate by mount point wins the FSM"
        );
        assert_eq!(manager.state(), ConnectionState::Connecting);
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
            apply_scan(&mut manager, report, &no_retrieval);
            assert_ne!(
                manager.state(),
                ConnectionState::Ready,
                "discovery glue must never mark Ready (report: {report:?})"
            );
        }
    }

    /// B-005: retrieved facts replace the placeholder — name, model,
    /// generation, and measured capacities land in the manager's device,
    /// still Connecting (preflight is B-004's job) and never Ready.
    #[test]
    fn retrieved_facts_populate_device() {
        let mut manager = DeviceManager::new();
        let retrieve = |_mount: &Path| {
            Ok(DeviceFacts {
                name: "iPod user".to_string(),
                model: "Video (Black)".to_string(),
                generation: "Video (2nd Gen.)".to_string(),
                model_number: Some("A446".to_string()),
                firmware: None,
                total_capacity: Some(30 * 1024 * 1024 * 1024),
                used_capacity: Some(23 * 1024 * 1024 * 1024),
                firewire_id: None,
            })
        };
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        let device = manager.current_device().unwrap();
        assert_eq!(device.mount_point, std::path::PathBuf::from("/mnt/ipod"));
        assert_eq!(device.name, "iPod user");
        assert_eq!(device.model, "Video (Black)");
        assert_eq!(device.generation, "Video (2nd Gen.)");
        assert_eq!(device.model_number.as_deref(), Some("A446"));
        assert_eq!(device.total_capacity, Some(30 * 1024 * 1024 * 1024));
        assert_eq!(device.used_capacity, Some(23 * 1024 * 1024 * 1024));
        assert_eq!(manager.state(), ConnectionState::Connecting);
        assert!(!manager.sync_enabled());
    }

    /// B-005: a failed retrieval keeps the candidate (mount point known)
    /// and lands the FSM in `Error` with the failure as the diagnostic —
    /// never Ready, sync disabled.
    #[test]
    fn retrieval_failure_enters_error() {
        let mut manager = DeviceManager::new();
        let retrieve = |_mount: &Path| {
            Err(ipod_sync::RetrieveError::NotFound(
                "iTunesDB corrupt".to_string(),
            ))
        };
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("iTunesDB corrupt"));
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod"),
            "the confirmed candidate is retained with the failure"
        );
        assert!(!manager.sync_enabled());
        // A rescan with the same candidate must not clobber the diagnostic
        // or re-attempt retrieval.
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("iTunesDB corrupt"));
    }

    /// B-005: retrieval runs once per device appearance — not per scan.
    #[test]
    fn retrieval_runs_once_per_appearance() {
        let calls = std::cell::Cell::new(0);
        let retrieve = |_mount: &Path| {
            calls.set(calls.get() + 1);
            Ok(DeviceFacts::default())
        };
        let mut manager = DeviceManager::new();

        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        assert_eq!(calls.get(), 1, "appearance retrieves once");
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        assert_eq!(calls.get(), 1, "a rescan must not re-parse");

        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan(&mut manager, &removal, &retrieve);
        assert_eq!(calls.get(), 1, "removal does not retrieve");
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        assert_eq!(calls.get(), 2, "reappearance retrieves again");

        // Backend recovery re-enters Connecting: a fresh retrieval.
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
            &retrieve,
        );
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
        );
        assert_eq!(calls.get(), 3, "backend recovery retrieves again");
    }

    /// B-005 + first-wins: only the candidate that wins the FSM is
    /// retrieved, and a handover retrieves the new winner.
    #[test]
    fn only_the_first_winning_candidate_is_retrieved() {
        let retrieved = std::cell::RefCell::new(Vec::new());
        let retrieve = |mount: &Path| {
            retrieved.borrow_mut().push(mount.to_path_buf());
            Ok(DeviceFacts::default())
        };
        let mut manager = DeviceManager::new();

        apply_scan(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &retrieve,
        );
        assert_eq!(
            *retrieved.borrow(),
            vec![std::path::PathBuf::from("/mnt/ipod-aaa")],
            "only the FSM winner is retrieved"
        );

        let mut next = candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED);
        next.removed = vec!["/mnt/ipod-aaa".into()];
        apply_scan(&mut manager, &next, &retrieve);
        assert_eq!(
            *retrieved.borrow(),
            vec![
                std::path::PathBuf::from("/mnt/ipod-aaa"),
                std::path::PathBuf::from("/mnt/ipod-bbb"),
            ],
            "handover retrieves the new winner"
        );
    }

    /// B-005: the failure path must never reach Ready either.
    #[test]
    fn retrieval_failure_never_marks_ready() {
        let mut manager = DeviceManager::new();
        let failing = |_mount: &Path| Err(ipod_sync::RetrieveError::NotFound("boom".to_string()));
        apply_scan(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &failing,
        );
        assert_ne!(manager.state(), ConnectionState::Ready);
    }
}
