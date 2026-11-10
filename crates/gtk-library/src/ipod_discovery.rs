//! B-003 + B-005 + B-004: feed discovery results, retrieved metadata, and
//! the readiness preflight into the connection FSM.
//!
//! The glue between `ipod_sync` (discovery + retrieval + preflight) and
//! B-002's [`DeviceManager`]. One [`apply_scan`] call per scan report keeps
//! the manager in sync with the mount table: a detected candidate enters
//! `Connecting` with its metadata retrieved (B-005) and is then run through
//! the caller-supplied preflight (B-004) — the B-002 callback boundary —
//! landing in `Ready`, `Error`, or `BackendUnavailable`. A missing backend
//! shows `BackendUnavailable` ("Sync unavailable") with the reason as the
//! diagnostic, a failed retrieval shows `Error` with the failure as the
//! diagnostic, and a removed mount returns to `Disconnected`.
//!
//! The glue never invents Ready by itself: it only calls
//! `set_ready_after_preflight` with the supplied callback (asserted by the
//! tests). The backend ABI verdict is consumed here: a soft-accepted distro
//! build (`BackendStatus::Loaded { abi_verified: false }`) is an
//! incompatible backend — the device info stays visible (retrieval still
//! runs), but the state goes `BackendUnavailable` and preflight is never
//! called, so `Ready` is unreachable. When the backend later becomes
//! unavailable, the backend's reason supersedes any earlier retrieval
//! failure diagnostic (the newer cause wins); a retrieval failure itself
//! remains visible until the next state-changing scan.
//!
//! Preflight runs exactly once per device appearance: after the retrieval
//! transitions, the state `Connecting` can only exist while retrieval or
//! preflight just ran in this same scan — rescans never produce it
//! (B-003/B-005), and preflight leaves `Ready`/`Error`/
//! `BackendUnavailable`. So `Connecting` is a transient state within one
//! `apply_scan` call: synchronous readers of `manager.state()` see only the
//! terminal state, but the B-006 `emit` callback delivers the full sequence
//! to subscribers. On a fresh detection the glue enters `Connecting` with
//! the placeholder device **before** the (multi-second) retrieval parse, so
//! observers see the candidate the moment the scan finds it (S-02). A
//! rescan never re-parses the device or clobbers a pending diagnostic.
//!
//! The `emit` callback fires after every mutating step, but only when the
//! observable snapshot — `(state, current device, diagnostics)` — changed
//! since the previous emission within the call. Idempotent rescans emit
//! nothing; a fresh appearance emits `Connecting` twice (placeholder, then
//! retrieved facts) because the device info genuinely changed. Subscribers
//! render state, not emission counts.

use std::path::Path;

use ipod_sync::{BackendStatus, DeviceFacts, RetrieveError, ScanReport};

use crate::device_manager::{ConnectionState, DeviceInfo, DeviceManager, PreflightError};

/// The user-facing diagnostic for a soft-accepted distro backend build
/// (`BackendStatus::Loaded { abi_verified: false }`): the library loads,
/// but its struct layouts are not covered by the wrapper's pinned ABI
/// marker, so syncing is not supported.
const ABI_UNVERIFIED_DIAGNOSTIC: &str = "the installed libgpod backend is a soft-accepted distro build \
     (ABI unverified) and is not supported for syncing";

/// The `DeviceInfo` for a freshly detected candidate whose metadata could
/// not be retrieved: only the mount point is known. `name`/`model`/
/// `generation` are empty strings meaning "not retrieved" — never
/// fabricated values. Optional fields stay `None`; the model-policy
/// verdicts are `false` until retrieval measures them.
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
        model_supported: false,
        checksum_possible: false,
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
        model_supported: facts.model_supported,
        checksum_possible: facts.checksum_possible,
    }
}

/// Install the current candidate into the manager with its metadata
/// retrieved. A retrieval failure keeps the mount point (the candidate is
/// still confirmed) and moves the FSM to `Error` with the failure as the
/// actionable diagnostic — never Ready, sync disabled.
///
/// Invariant this relies on: retrieval only runs when the manager has no
/// facts for the device yet (first appearance — where B-006's detection
/// step has just installed the placeholder — or backend recovery), so the
/// placeholder `set_device` on the failure path never wipes previously
/// retrieved metadata. If B-004 ever re-retrieves on a live device, this
/// must be reworked.
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

/// The observable snapshot of the manager: everything B-006 subscribers
/// receive. `DeviceInfo` is cloned wholesale — it is strings and numbers,
/// and the emission path is the UI boundary, not a hot loop.
type Observable = (ConnectionState, Option<DeviceInfo>, Option<String>);

fn observed(manager: &DeviceManager) -> Observable {
    (
        manager.state(),
        manager.current_device().cloned(),
        manager.diagnostics().map(str::to_string),
    )
}

/// Emit the current snapshot if it changed since the previous emission
/// within this `apply_scan` call. The entry snapshot seeds `last`, so an
/// idempotent rescan (no mutating step touched the manager) emits nothing,
/// and a same-value update (e.g. an unchanged backend reason) emits nothing
/// either.
fn emit_if_changed(
    manager: &DeviceManager,
    last: &mut Option<Observable>,
    emit: &mut dyn FnMut(&DeviceManager),
) {
    let current = observed(manager);
    if last.as_ref().is_none_or(|previous| *previous != current) {
        emit(manager);
        *last = Some(current);
    }
}

/// Apply one discovery scan to the FSM. The only path to `Ready` is the
/// `preflight` callback — the glue never invents it.
///
/// `retrieve` opens a session on the candidate mount and collects its
/// metadata (B-005; in production a closure over `Discovery::backend()`).
/// It runs only when the candidate newly becomes current — new device,
/// backend recovery — so rescans never re-parse the device.
///
/// `preflight` is the B-002 callback boundary (B-004; in production
/// [`preflight_with_backend`]). It runs exactly once per device appearance,
/// right after a successful retrieval, while the state is still the
/// transient `Connecting`; a failed preflight lands in `Error`
/// (`PreflightError::Readiness`) or `BackendUnavailable`
/// (`PreflightError::BackendUnavailable`) with the diagnostic, never
/// `Ready`. On `abi_verified = false` the glue short-circuits to
/// `BackendUnavailable` itself and `preflight` is not called.
///
/// `emit` (B-006) receives the manager after every mutating step, but only
/// when the observable snapshot changed since the previous emission within
/// this call — see the module docs for the exact contract. Callers that
/// don't need the sequence pass a no-op.
///
/// Tests inject stubs; a `DeviceFacts::default()` stub behaves like the old
/// metadata-less placeholder, and an `Ok(())` preflight stub reaches
/// `Ready` only when the facts carry valid capacities (B-002's built-in
/// validation).
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
    preflight: &dyn Fn(&Path) -> Result<(), PreflightError>,
    emit: &mut dyn FnMut(&DeviceManager),
) {
    let mut last = Some(observed(manager));

    // 1. Removal: the current device's mount left the candidate set.
    if let Some(mount) = manager
        .current_device()
        .map(|device| device.mount_point.clone())
    {
        if report.removed.contains(&mount) {
            manager.clear_device();
            emit_if_changed(manager, &mut last, emit);
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
        BackendStatus::Loaded { abi_verified } => {
            // Retrieval transitions: only when the candidate newly becomes
            // current (new device, backend recovery) — a rescan must not
            // re-parse the device.
            let recovering = manager.state() == ConnectionState::BackendUnavailable && same_device;
            if !same_device || manager.state() == ConnectionState::Disconnected {
                // Detection: enter Connecting with the placeholder BEFORE
                // the (multi-second) retrieval parse, so observers see the
                // candidate the moment the scan finds it (S-02).
                manager.set_device(detected_device(mount));
                emit_if_changed(manager, &mut last, emit);
                set_retrieved_device(manager, mount, retrieve);
            } else if recovering {
                if *abi_verified {
                    // The backend recovered (loads are retried until
                    // success): retry means a fresh Connecting with a fresh
                    // retrieval, per S-05.
                    set_retrieved_device(manager, mount, retrieve);
                } else {
                    // The library loads but the ABI is unverified: the newer
                    // cause wins — without re-parsing the device.
                    manager.set_diagnostics(ABI_UNVERIFIED_DIAGNOSTIC.to_string());
                }
            }
            emit_if_changed(manager, &mut last, emit);

            // Fresh Connecting == retrieval just succeeded in this scan (see
            // the module docs): run the readiness gate exactly once per
            // device appearance. Same candidate while Ready/Error leaves the
            // state and the diagnostics alone — a rescan must not re-parse
            // the device or clobber a pending diagnostic.
            if manager.state() == ConnectionState::Connecting {
                if !abi_verified {
                    // Incompatible backend: the device info stays visible,
                    // sync stays disabled, preflight never runs.
                    manager.set_state(ConnectionState::BackendUnavailable);
                    manager.set_diagnostics(ABI_UNVERIFIED_DIAGNOSTIC.to_string());
                } else {
                    let _ =
                        manager.set_ready_after_preflight(|device| preflight(&device.mount_point));
                }
            } else if !abi_verified && manager.state() == ConnectionState::Ready {
                // Unreachable in production (abi_verified is a load-time
                // constant of the cached backend), but defensively: Ready
                // must never survive an unverified-backend report.
                manager.set_state(ConnectionState::BackendUnavailable);
                manager.set_diagnostics(ABI_UNVERIFIED_DIAGNOSTIC.to_string());
            }
            emit_if_changed(manager, &mut last, emit);
        }
        BackendStatus::Unavailable { reason } => {
            if !same_device || manager.state() == ConnectionState::Disconnected {
                manager.set_device(detected_device(mount));
                emit_if_changed(manager, &mut last, emit);
            }
            if manager.state() != ConnectionState::BackendUnavailable {
                manager.set_state(ConnectionState::BackendUnavailable);
            }
            // The newer cause wins: a backend-unavailable reason supersedes
            // any earlier retrieval-failure diagnostic (see the module
            // docs).
            manager.set_diagnostics(reason.clone());
            emit_if_changed(manager, &mut last, emit);
        }
    }
}

/// The production preflight composition: run `ipod_sync::preflight` on the
/// candidate mount with the loaded backend, mapped onto B-002's
/// [`PreflightError`].
pub fn preflight_with_backend(
    backend: &ipod::IpodBackend,
) -> impl Fn(&Path) -> Result<(), PreflightError> + '_ {
    |mount| ipod_sync::preflight(backend, mount).map_err(map_readiness)
}

/// Map the ipod-sync classification onto the B-002 error contract:
/// `BackendAbi` → `BackendUnavailable` (an incompatible backend — the glue
/// itself short-circuits before preflight, so this arm is classification
/// completeness), device-level checks → `Readiness`.
pub(crate) fn map_readiness(error: ipod_sync::ReadinessError) -> PreflightError {
    match error {
        ipod_sync::ReadinessError::BackendAbi { detail } => {
            PreflightError::BackendUnavailable(detail)
        }
        ipod_sync::ReadinessError::Check { detail, .. } => PreflightError::Readiness(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipod_sync::{Candidate, ScanReport};

    /// The old 4-arg call shape for tests that don't assert emission
    /// sequences: `apply_scan` with a no-op emitter.
    fn apply_scan_quiet(
        manager: &mut DeviceManager,
        report: &ScanReport,
        retrieve: &dyn Fn(&Path) -> Result<DeviceFacts, RetrieveError>,
        preflight: &dyn Fn(&Path) -> Result<(), PreflightError>,
    ) {
        apply_scan(manager, report, retrieve, preflight, &mut |_manager| {});
    }

    /// Collect the observable tuple — state, full device info, diagnostics —
    /// per emission, the B-006 subscriber shape. Sequence tests assert the
    /// state progression (and the device info where the placeholder/facts
    /// distinction matters).
    fn collecting<'a>(
        events: &'a mut Vec<(ConnectionState, Option<DeviceInfo>, Option<String>)>,
    ) -> impl FnMut(&DeviceManager) + 'a {
        |manager| {
            events.push((
                manager.state(),
                manager.current_device().cloned(),
                manager.diagnostics().map(str::to_string),
            ));
        }
    }

    fn states(
        events: &[(ConnectionState, Option<DeviceInfo>, Option<String>)],
    ) -> Vec<ConnectionState> {
        events.iter().map(|(state, _, _)| *state).collect()
    }

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
    const LOADED_DISTRO: BackendStatus = BackendStatus::Loaded {
        abi_verified: false,
    };

    fn unavailable(reason: &str) -> BackendStatus {
        BackendStatus::Unavailable {
            reason: reason.to_string(),
        }
    }

    /// The behavior-neutral retrieval stub: empty facts, the shape of the
    /// pre-B-005 placeholder device — and therefore no capacities, so
    /// B-002 refuses Ready even after a successful preflight.
    fn no_retrieval(_mount: &Path) -> Result<DeviceFacts, RetrieveError> {
        Ok(DeviceFacts::default())
    }

    /// Retrieval stub with valid measured capacities, so a successful
    /// preflight can actually reach Ready.
    fn retrieved_with_capacity(_mount: &Path) -> Result<DeviceFacts, RetrieveError> {
        Ok(DeviceFacts {
            total_capacity: Some(30 * 1024 * 1024 * 1024),
            used_capacity: Some(23 * 1024 * 1024 * 1024),
            model_supported: true,
            checksum_possible: true,
            ..DeviceFacts::default()
        })
    }

    /// The behavior-neutral preflight stub: every readiness check passes.
    fn ok_preflight(_mount: &Path) -> Result<(), PreflightError> {
        Ok(())
    }

    /// A preflight stub that fails every call with a readiness diagnostic,
    /// counting its invocations.
    fn failing_preflight<'a>(
        calls: &'a std::cell::Cell<u32>,
        diagnostic: &'a str,
    ) -> impl Fn(&Path) -> Result<(), PreflightError> + 'a {
        |_mount| {
            calls.set(calls.get() + 1);
            Err(PreflightError::Readiness(diagnostic.to_string()))
        }
    }

    /// A passing preflight stub that counts its invocations.
    fn counting_preflight<'a>(
        calls: &'a std::cell::Cell<u32>,
    ) -> impl Fn(&Path) -> Result<(), PreflightError> + 'a {
        |_mount| {
            calls.set(calls.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn candidate_with_ok_preflight_reaches_ready() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod")
        );
        assert!(
            manager.sync_enabled(),
            "a verified candidate is sync-enabled"
        );
    }

    #[test]
    fn candidate_without_backend_enters_backend_unavailable() {
        let calls = std::cell::Cell::new(0);
        let preflight = failing_preflight(&calls, "should not run");
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
            &no_retrieval,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert!(
            manager.current_device().is_some(),
            "the device info is retained"
        );
        assert_eq!(manager.diagnostics(), Some("dlopen failed"));
        assert_eq!(calls.get(), 0, "preflight must not run without a backend");
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn removal_returns_to_disconnected_and_clears_device() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan_quiet(&mut manager, &removal, &no_retrieval, &ok_preflight);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
    }

    #[test]
    fn unrelated_volumes_leave_manager_untouched() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(Vec::new(), LOADED),
            &no_retrieval,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
        assert_eq!(manager.diagnostics(), None);
    }

    #[test]
    fn rescan_while_ready_is_idempotent() {
        let calls = std::cell::Cell::new(0);
        let preflight = counting_preflight(&calls);
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert_eq!(calls.get(), 1, "preflight runs once per appearance");
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(calls.get(), 1, "a rescan must not re-run preflight");
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert_eq!(manager.diagnostics(), None);
    }

    #[test]
    fn rescan_preserves_preflight_error_diagnostic() {
        let calls = std::cell::Cell::new(0);
        let preflight = failing_preflight(&calls, "ArtworkDB corrupt");
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("ArtworkDB corrupt"));
        assert_eq!(calls.get(), 1);
        // A rescan must not clobber the diagnostic or re-run preflight.
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("ArtworkDB corrupt"));
        assert_eq!(calls.get(), 1);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn preflight_failure_lands_error_with_diagnostic() {
        let calls = std::cell::Cell::new(0);
        let preflight = failing_preflight(&calls, "device database corrupt");
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("device database corrupt"));
        assert_eq!(calls.get(), 1);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn preflight_backend_unavailable_error_maps_to_backend_unavailable() {
        let preflight =
            |_mount: &Path| Err(PreflightError::BackendUnavailable("no lib".to_string()));
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert_eq!(manager.diagnostics(), Some("no lib"));
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn abi_unverified_backend_lands_backend_unavailable() {
        let calls = std::cell::Cell::new(0);
        let preflight = counting_preflight(&calls);
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert!(
            manager.diagnostics().unwrap().contains("ABI unverified"),
            "diagnostic must name the ABI verdict: {:?}",
            manager.diagnostics()
        );
        assert_eq!(
            calls.get(),
            0,
            "preflight must not run on an unverified ABI"
        );
        assert!(
            manager.current_device().is_some(),
            "the device info is retained"
        );
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn abi_unverified_rescan_does_not_reparse_or_reclobber() {
        let retrieved = std::cell::Cell::new(0);
        let retrieve = |_mount: &Path| {
            retrieved.set(retrieved.get() + 1);
            Ok(DeviceFacts::default())
        };
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert_eq!(retrieved.get(), 1, "appearance retrieves once");
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(
            retrieved.get(),
            1,
            "a rescan under an unverified ABI must not re-parse"
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
    }

    #[test]
    fn abi_unverified_with_failed_retrieval_keeps_retrieval_diagnostic() {
        let retrieve = |_mount: &Path| Err(RetrieveError::NotFound("iTunesDB corrupt".to_string()));
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("iTunesDB corrupt"));
    }

    #[test]
    fn ready_downgrades_on_a_flipped_unverified_report() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert!(manager.diagnostics().unwrap().contains("ABI unverified"));
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn missing_capacity_refuses_ready_despite_ok_preflight() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &no_retrieval,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert!(
            manager.diagnostics().unwrap().contains("capacity"),
            "the B-002 capacity validation must refuse Ready: {:?}",
            manager.diagnostics()
        );
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn missing_firmware_does_not_prevent_ready() {
        // plan.md S-06: missing firmware alone never blocks readiness. The
        // retrieval facts always carry firmware: None today.
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert_eq!(manager.current_device().unwrap().firmware, None);
        assert!(manager.sync_enabled());
    }

    #[test]
    fn backend_recovery_reruns_preflight_and_reaches_ready() {
        let calls = std::cell::Cell::new(0);
        let preflight = counting_preflight(&calls);
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
            &no_retrieval,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert_eq!(calls.get(), 0);
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert_eq!(
            calls.get(),
            1,
            "a recovered backend means a fresh Connecting with a fresh preflight, per S-05"
        );
        assert!(manager.sync_enabled());
    }

    #[test]
    fn reappear_after_removal_runs_fresh_preflight() {
        let calls = std::cell::Cell::new(0);
        let preflight = counting_preflight(&calls);
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan_quiet(&mut manager, &removal, &no_retrieval, &preflight);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert_eq!(calls.get(), 2, "reappearance re-runs preflight");
    }

    #[test]
    fn replug_retry_after_error_reaches_ready() {
        // The owner-approved retry story: no glue retry helper — retry is a
        // replug (removal → reappearance), which re-runs the whole gate.
        let calls = std::cell::Cell::new(0);
        let failing = failing_preflight(&calls, "mounted read-only");
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &failing,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("mounted read-only"));
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan_quiet(&mut manager, &removal, &no_retrieval, &failing);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        let ok = counting_preflight(&calls);
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieved_with_capacity,
            &ok,
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert!(manager.sync_enabled());
    }

    #[test]
    fn blocked_mounts_leave_manager_untouched() {
        let mut manager = DeviceManager::new();
        let mut report = candidate_report(Vec::new(), LOADED);
        report.blocked = vec![ipod_sync::BlockedMount {
            mount_point: "/mnt/other".into(),
            error: "Permission denied (os error 13)".to_string(),
        }];
        apply_scan_quiet(&mut manager, &report, &no_retrieval, &ok_preflight);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
    }

    /// First-wins policy: with two candidates in one scan, only the first
    /// (sorted by mount point) becomes the current device.
    #[test]
    fn multiple_candidates_only_first_gets_fsm() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
    }

    /// Removing the active device hands the FSM over to the next candidate.
    #[test]
    fn device_handover_when_active_removed() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
        // The first device unmounts; the second is still there.
        let mut next = candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED);
        next.removed = vec!["/mnt/ipod-aaa".into()];
        apply_scan_quiet(&mut manager, &next, &retrieved_with_capacity, &ok_preflight);
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-bbb"),
            "the FSM must hand over to the remaining candidate"
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
    }

    /// Removing a non-current device leaves the FSM alone: the active
    /// candidate is still mounted and stays current.
    #[test]
    fn non_active_removal_is_invisible() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        let mut next = candidate_report(vec![candidate("/mnt/ipod-aaa")], LOADED);
        next.removed = vec!["/mnt/ipod-bbb".into()];
        apply_scan_quiet(&mut manager, &next, &retrieved_with_capacity, &ok_preflight);
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
    }

    /// First wins, not first seen: plugging in a device that sorts before
    /// the current one takes the FSM over (deliberate, owner-approved).
    #[test]
    fn lower_sorting_candidate_takes_over() {
        let mut manager = DeviceManager::new();
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-bbb")
        );
        apply_scan_quiet(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &retrieved_with_capacity,
            &ok_preflight,
        );
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa"),
            "the first candidate by mount point wins the FSM"
        );
        assert_eq!(manager.state(), ConnectionState::Ready);
    }

    /// The full scenario walk: Ready is reachable only through the
    /// preflight callback with a verified backend and valid capacities.
    #[test]
    fn state_machine_walks_the_full_scenario_matrix() {
        let mut manager = DeviceManager::new();
        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        let scenarios: Vec<(ScanReport, ConnectionState)> = vec![
            (
                candidate_report(Vec::new(), LOADED),
                ConnectionState::Disconnected,
            ),
            (
                candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                ConnectionState::Ready,
            ),
            (
                candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
                ConnectionState::BackendUnavailable,
            ),
            (
                candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                ConnectionState::Ready,
            ),
            (removal, ConnectionState::Disconnected),
            (
                candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
                ConnectionState::BackendUnavailable,
            ),
        ];
        for (report, expected) in &scenarios {
            apply_scan_quiet(
                &mut manager,
                report,
                &retrieved_with_capacity,
                &ok_preflight,
            );
            assert_eq!(
                manager.state(),
                *expected,
                "unexpected state after report {report:?}"
            );
        }
    }

    /// B-005: retrieved facts replace the placeholder — name, model,
    /// generation, and measured capacities land in the manager's device,
    /// and a successful preflight takes it to Ready.
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
                model_supported: true,
                checksum_possible: true,
            })
        };
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &ok_preflight,
        );
        let device = manager.current_device().unwrap();
        assert_eq!(device.mount_point, std::path::PathBuf::from("/mnt/ipod"));
        assert_eq!(device.name, "iPod user");
        assert_eq!(device.model, "Video (Black)");
        assert_eq!(device.generation, "Video (2nd Gen.)");
        assert_eq!(device.model_number.as_deref(), Some("A446"));
        assert_eq!(device.total_capacity, Some(30 * 1024 * 1024 * 1024));
        assert_eq!(device.used_capacity, Some(23 * 1024 * 1024 * 1024));
        assert!(
            device.model_supported,
            "the retrieval verdict must ride along"
        );
        assert!(device.checksum_possible, "Video needs no checksum");
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert!(manager.sync_enabled());
    }

    /// B-005: a failed retrieval keeps the candidate (mount point known)
    /// and lands the FSM in `Error` with the failure as the diagnostic —
    /// preflight never runs, sync disabled.
    #[test]
    fn retrieval_failure_enters_error() {
        let calls = std::cell::Cell::new(0);
        let preflight = counting_preflight(&calls);
        let mut manager = DeviceManager::new();
        let retrieve = |_mount: &Path| {
            Err(ipod_sync::RetrieveError::NotFound(
                "iTunesDB corrupt".to_string(),
            ))
        };
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("iTunesDB corrupt"));
        assert_eq!(
            manager.current_device().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod"),
            "the confirmed candidate is retained with the failure"
        );
        assert_eq!(
            calls.get(),
            0,
            "preflight must not run after a failed retrieval"
        );
        assert!(!manager.sync_enabled());
        // A rescan with the same candidate must not clobber the diagnostic
        // or re-attempt retrieval.
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &preflight,
        );
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("iTunesDB corrupt"));
        assert_eq!(calls.get(), 0);
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

        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(calls.get(), 1, "appearance retrieves once");
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(calls.get(), 1, "a rescan must not re-parse");

        let mut removal = candidate_report(Vec::new(), LOADED);
        removal.removed = vec!["/mnt/ipod".into()];
        apply_scan_quiet(&mut manager, &removal, &retrieve, &ok_preflight);
        assert_eq!(calls.get(), 1, "removal does not retrieve");
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(calls.get(), 2, "reappearance retrieves again");

        // Backend recovery re-enters Connecting: a fresh retrieval.
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
            &retrieve,
            &ok_preflight,
        );
        apply_scan_quiet(
            &mut manager,
            &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
            &retrieve,
            &ok_preflight,
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

        apply_scan_quiet(
            &mut manager,
            &candidate_report(
                vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                LOADED,
            ),
            &retrieve,
            &ok_preflight,
        );
        assert_eq!(
            *retrieved.borrow(),
            vec![std::path::PathBuf::from("/mnt/ipod-aaa")],
            "only the FSM winner is retrieved"
        );

        let mut next = candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED);
        next.removed = vec!["/mnt/ipod-aaa".into()];
        apply_scan_quiet(&mut manager, &next, &retrieve, &ok_preflight);
        assert_eq!(
            *retrieved.borrow(),
            vec![
                std::path::PathBuf::from("/mnt/ipod-aaa"),
                std::path::PathBuf::from("/mnt/ipod-bbb"),
            ],
            "handover retrieves the new winner"
        );
    }

    /// B-004: the ReadinessError → PreflightError mapping matches the B-002
    /// contract exactly.
    #[test]
    fn readiness_error_mapping_matches_the_b002_contract() {
        assert_eq!(
            map_readiness(ipod_sync::ReadinessError::BackendAbi {
                detail: "distro build".to_string(),
            }),
            PreflightError::BackendUnavailable("distro build".to_string())
        );
        assert_eq!(
            map_readiness(ipod_sync::ReadinessError::Check {
                kind: ipod_sync::ReadinessCheck::ReadOnly,
                detail: "ro mount".to_string(),
            }),
            PreflightError::Readiness("ro mount".to_string())
        );
    }

    /// B-006: the fresh-candidate sequence — `Connecting` (placeholder, at
    /// detection) → `Connecting` (retrieved facts) → `Ready`.
    #[test]
    fn emissions_follow_the_fresh_candidate_sequence() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![
                ConnectionState::Connecting,
                ConnectionState::Connecting,
                ConnectionState::Ready,
            ]
        );
        assert!(events.iter().all(|(_, device, _)| {
            device
                .as_ref()
                .is_some_and(|device| device.mount_point == *std::path::Path::new("/mnt/ipod"))
        }));
    }

    /// B-006: the detection emission carries the placeholder device (no
    /// facts yet — no name, no capacities), the retrieval emission carries
    /// the facts.
    #[test]
    fn fresh_detection_emits_placeholder_then_facts() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(events[0].1.as_ref().unwrap().name, "");
        assert_eq!(events[0].1.as_ref().unwrap().total_capacity, None);
        assert_eq!(
            events[1].1.as_ref().unwrap().total_capacity,
            Some(30 * 1024 * 1024 * 1024),
            "the retrieval emission carries the measured capacity"
        );
    }

    /// B-006: a failed retrieval emits `Connecting` at detection, then
    /// `Error` with the retrieval diagnostic.
    #[test]
    fn retrieval_failure_emits_connecting_then_error() {
        let retrieve = |_mount: &Path| Err(RetrieveError::NotFound("iTunesDB corrupt".to_string()));
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieve,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![ConnectionState::Connecting, ConnectionState::Error]
        );
        assert_eq!(events[1].2.as_deref(), Some("iTunesDB corrupt"));
    }

    /// B-006: an unverified ABI emits the detection `Connecting`, the
    /// retrieval `Connecting`, then `BackendUnavailable` with the ABI
    /// diagnostic — preflight never runs.
    #[test]
    fn abi_unverified_emits_connecting_then_backend_unavailable() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![
                ConnectionState::Connecting,
                ConnectionState::Connecting,
                ConnectionState::BackendUnavailable,
            ]
        );
        assert!(
            events[2].2.as_deref().unwrap().contains("ABI unverified"),
            "the terminal diagnostic must name the ABI verdict"
        );
    }

    /// B-006: a first scan without a backend emits `Connecting` (the
    /// candidate is detected), then `BackendUnavailable` ("Sync
    /// unavailable") with the load reason.
    #[test]
    fn unavailable_backend_emits_connecting_then_backend_unavailable() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
                &no_retrieval,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![
                ConnectionState::Connecting,
                ConnectionState::BackendUnavailable,
            ]
        );
        assert_eq!(events[1].2.as_deref(), Some("dlopen failed"));
    }

    /// B-006: removal emits `Disconnected` with the device cleared.
    #[test]
    fn removal_emits_disconnected() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(manager.state(), ConnectionState::Ready);
        events.clear();
        {
            let mut removal = candidate_report(Vec::new(), LOADED);
            removal.removed = vec!["/mnt/ipod".into()];
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &removal,
                &no_retrieval,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(states(&events), vec![ConnectionState::Disconnected]);
        assert!(events[0].1.is_none(), "the device info is cleared");
    }

    /// B-006: an idempotent rescan emits nothing — no mutating step ran.
    #[test]
    fn rescan_while_ready_emits_nothing() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(manager.state(), ConnectionState::Ready);
        events.clear();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert!(events.is_empty(), "a rescan must not emit: {events:?}");
    }

    /// B-006: a rescan in Error preserves the diagnostic silently — the
    /// subscriber sees no new emission.
    #[test]
    fn rescan_while_error_emits_nothing() {
        let calls = std::cell::Cell::new(0);
        let failing = failing_preflight(&calls, "mounted read-only");
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &failing,
                &mut emit,
            );
        }
        assert_eq!(manager.state(), ConnectionState::Error);
        events.clear();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &failing,
                &mut emit,
            );
        }
        assert!(
            events.is_empty(),
            "an Error rescan must not emit: {events:?}"
        );
    }

    /// B-006: a rescan with the same backend-unavailable reason emits
    /// nothing — the snapshot did not change (the reason re-set is the
    /// same value).
    #[test]
    fn unavailable_backend_rescan_with_same_reason_emits_nothing() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
                &no_retrieval,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(events.len(), 2);
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
                &no_retrieval,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(events.len(), 2, "an unchanged reason must not emit");
    }

    /// B-006: backend recovery emits `Connecting` (fresh retrieval) then
    /// `Ready`.
    #[test]
    fn backend_recovery_emits_connecting_then_ready() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
                &no_retrieval,
                &ok_preflight,
                &mut emit,
            );
        }
        events.clear();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![ConnectionState::Connecting, ConnectionState::Ready]
        );
    }

    /// B-006: removing the active device hands the FSM over to the next
    /// candidate — `Disconnected`, then the fresh sequence on the new mount.
    #[test]
    fn handover_emits_disconnected_then_fresh_sequence() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(
                    vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                    LOADED,
                ),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        events.clear();
        {
            let mut next = candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED);
            next.removed = vec!["/mnt/ipod-aaa".into()];
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &next,
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![
                ConnectionState::Disconnected,
                ConnectionState::Connecting,
                ConnectionState::Connecting,
                ConnectionState::Ready,
            ]
        );
        assert_eq!(
            events[1].1.as_ref().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-bbb"),
            "the handover emits the fresh sequence for the new winner"
        );
    }

    /// B-006: a lower-sorting candidate taking over emits the fresh
    /// sequence on the new mount.
    #[test]
    fn takeover_emits_connecting_then_ready() {
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod-bbb")], LOADED),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        events.clear();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(
                    vec![candidate("/mnt/ipod-aaa"), candidate("/mnt/ipod-bbb")],
                    LOADED,
                ),
                &retrieved_with_capacity,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(
            states(&events),
            vec![
                ConnectionState::Connecting,
                ConnectionState::Connecting,
                ConnectionState::Ready,
            ]
        );
        assert_eq!(
            events[1].1.as_ref().unwrap().mount_point,
            std::path::PathBuf::from("/mnt/ipod-aaa")
        );
    }

    /// B-006: the newer-cause-wins diagnostic refresh is observable — the
    /// recovering unverified rescan emits `BackendUnavailable` with the ABI
    /// diagnostic replacing the earlier load reason, without re-parsing.
    #[test]
    fn recovering_unverified_refresh_emits_newer_diagnostic() {
        let retrieved = std::cell::Cell::new(0);
        let retrieve = |_mount: &Path| {
            retrieved.set(retrieved.get() + 1);
            Ok(DeviceFacts::default())
        };
        let mut manager = DeviceManager::new();
        let mut events = Vec::new();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], unavailable("dlopen failed")),
                &retrieve,
                &ok_preflight,
                &mut emit,
            );
        }
        events.clear();
        {
            let mut emit = collecting(&mut events);
            apply_scan(
                &mut manager,
                &candidate_report(vec![candidate("/mnt/ipod")], LOADED_DISTRO),
                &retrieve,
                &ok_preflight,
                &mut emit,
            );
        }
        assert_eq!(states(&events), vec![ConnectionState::BackendUnavailable]);
        assert!(
            events[0].2.as_deref().unwrap().contains("ABI unverified"),
            "the newer cause wins: {:?}",
            events[0].2
        );
        assert_eq!(retrieved.get(), 0, "no retrieval on the unverified rescan");
    }
}
