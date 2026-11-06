//! B-002: backend-independent device connection types.
//!
//! This module defines the `ConnectionState`, `DeviceInfo`, and `DeviceManager`
//! types and their contracts. It deliberately contains **no discovery, no backend
//! dependency, and no write logic** — those belong to B-003, B-004, B-007, and B-010.
//! This file is backend-independent on purpose: a build without libgpod must compile.

use std::path::PathBuf;

/// The exact connection state contract from `plan.md`'s Connection FSM.
///
/// The UI label for `BackendUnavailable` is "Sync unavailable". A mounted volume
/// alone is never sufficient for `Ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    BackendUnavailable,
    Ready,
    Error,
}

impl ConnectionState {
    /// The UI-visible label, matching spec.md's user-story wording.
    pub fn ui_label(&self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::BackendUnavailable => "Sync unavailable",
            Self::Ready => "Ready",
            Self::Error => "Error",
        }
    }

    /// Whether sync is enabled while in this state. Only `Ready` enables it.
    pub fn sync_enabled(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Whether this state permits a non-mutating readiness preflight (B-004).
    pub fn permits_preflight(&self) -> bool {
        matches!(self, Self::Connecting)
    }
}

/// Owned device information.
///
/// Every value that may be absent is `Option`. `None` means unavailable, never a
/// fabricated zero or default. Partial information can be exposed while Connecting
/// or in Error; `Ready` requires identified supported-model capabilities and measured
/// capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub mount_point: PathBuf,
    pub name: String,
    pub model: String,
    pub generation: String,
    pub model_number: Option<String>,
    pub firmware: Option<String>,
    pub total_capacity: Option<u64>,
    pub used_capacity: Option<u64>,
    pub firewire_id: Option<String>,
}

impl DeviceInfo {
    // No model-specific helpers here: model capability checks (checksums,
    // FireWire ID requirements) belong to libgpod's model table, not to this
    // backend-independent type contract.
}

/// A readiness check failure and its user-facing diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightError {
    /// The required backend is missing, disabled, or incompatible.
    BackendUnavailable(String),
    /// A device access, parsing, model, or other readiness check failed.
    Readiness(String),
}

/// Manages connection state and owned device information.
///
/// B-002 defines the type and contract only. The manager performs no discovery,
/// no backend dependency, and no write logic.
#[derive(Debug, Clone)]
pub struct DeviceManager {
    current_device: Option<DeviceInfo>,
    state: ConnectionState,
    diagnostics: Option<String>,
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self {
            current_device: None,
            state: ConnectionState::Disconnected,
            diagnostics: None,
        }
    }
}

impl DeviceManager {
    /// Starts with no current device and the `Disconnected` state.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current connection state.
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// The current device information, or `None` when no device is attached.
    pub fn current_device(&self) -> Option<&DeviceInfo> {
        self.current_device.as_ref()
    }

    /// The current diagnostic detail, or `None` when there is none.
    pub fn diagnostics(&self) -> Option<&str> {
        self.diagnostics.as_deref()
    }

    /// Whether sync is enabled in the current state.
    pub fn sync_enabled(&self) -> bool {
        self.state.sync_enabled()
    }

    /// Move into a non-ready connection state.
    ///
    /// `Ready` is deliberately excluded and leaves the manager unchanged: use
    /// `set_ready_after_preflight` so supported-model validation and backend
    /// preflight are explicit. Any accepted non-ready transition clears prior
    /// diagnostics; set new diagnostics after changing the state.
    pub fn set_state(&mut self, state: ConnectionState) {
        if state == ConnectionState::Ready {
            return;
        }

        if state == ConnectionState::Disconnected {
            self.clear_device();
            return;
        }

        self.state = state;
        self.clear_diagnostics();
    }

    /// Mark the current device ready only after the caller's backend/model
    /// validation and preflight succeed, and its measured capacities are valid.
    ///
    /// This can only be called while Connecting. The callback must perform the
    /// libgpod-backed supported-model and mount-specific readiness checks and
    /// return a classified, actionable diagnostic on failure. The callback runs
    /// before capacity validation so an unavailable backend is not masked by
    /// missing measurements. This method additionally requires both capacity
    /// values and rejects `used_capacity > total_capacity`.
    pub fn set_ready_after_preflight(
        &mut self,
        preflight: impl FnOnce(&DeviceInfo) -> Result<(), PreflightError>,
    ) -> bool {
        if !self.state.permits_preflight() {
            return false;
        }

        let Some(device) = self.current_device.as_ref() else {
            self.clear_device();
            return false;
        };

        let result = preflight(device).and_then(|()| {
            let diagnostic = match (device.total_capacity, device.used_capacity) {
                (None, _) => Some("Total device capacity is unavailable.".to_string()),
                (_, None) => Some("Used device capacity is unavailable.".to_string()),
                (Some(0), _) => Some("Measured total device capacity is zero.".to_string()),
                (Some(total), Some(used)) if used > total => Some(format!(
                    "Used device capacity ({used} bytes) exceeds total capacity ({total} bytes)."
                )),
                _ => None,
            };
            match diagnostic {
                Some(diagnostic) => Err(PreflightError::Readiness(diagnostic)),
                None => Ok(()),
            }
        });

        match result {
            Ok(()) => {
                self.state = ConnectionState::Ready;
                self.clear_diagnostics();
                true
            }
            Err(PreflightError::BackendUnavailable(diagnostic)) => {
                self.state = ConnectionState::BackendUnavailable;
                self.set_diagnostics(diagnostic);
                false
            }
            Err(PreflightError::Readiness(diagnostic)) => {
                self.state = ConnectionState::Error;
                self.set_diagnostics(diagnostic);
                false
            }
        }
    }

    /// Replace the current device information.
    ///
    /// A replaced device is a new candidate, so preflight is pending again.
    pub fn set_device(&mut self, info: DeviceInfo) {
        self.current_device = Some(info);
        self.state = ConnectionState::Connecting;
        self.clear_diagnostics();
    }

    /// Forget the current device and clear stale device information.
    ///
    /// Clearing a device always returns to `Disconnected`; a device must exist
    /// before sync can be enabled.
    pub fn clear_device(&mut self) {
        self.current_device = None;
        self.state = ConnectionState::Disconnected;
        self.clear_diagnostics();
    }

    /// Set the current diagnostic detail.
    pub fn set_diagnostics(&mut self, diagnostics: String) {
        self.diagnostics = Some(diagnostics);
    }

    /// Clear diagnostic detail after a retry, device change, or successful
    /// readiness check.
    pub fn clear_diagnostics(&mut self) {
        self.diagnostics = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_disconnected_without_device() {
        let manager = DeviceManager::new();
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
        assert!(!manager.sync_enabled());
        assert!(!manager.state().sync_enabled());
    }

    #[test]
    fn ready_requires_a_device() {
        let mut manager = DeviceManager::new();
        manager.set_state(ConnectionState::Ready);
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn direct_ready_request_leaves_every_state_unchanged() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ] {
            let mut manager = DeviceManager::new();
            if state != ConnectionState::Disconnected {
                manager.set_device(DeviceInfo {
                    mount_point: PathBuf::from("/mnt/ipod"),
                    name: "iPod".to_string(),
                    model: "iPod Video 5G".to_string(),
                    generation: "5G".to_string(),
                    model_number: None,
                    firmware: None,
                    total_capacity: Some(1_000),
                    used_capacity: Some(500),
                    firewire_id: None,
                });
                if state == ConnectionState::Ready {
                    assert!(manager.set_ready_after_preflight(|_| Ok(())));
                } else {
                    manager.set_state(state);
                }
            }
            manager.set_diagnostics("existing diagnostic".to_string());
            let device = manager.current_device().cloned();
            let sync_enabled = manager.sync_enabled();

            manager.set_state(ConnectionState::Ready);

            assert_eq!(manager.state(), state);
            assert_eq!(manager.current_device(), device.as_ref());
            assert_eq!(manager.diagnostics(), Some("existing diagnostic"));
            assert_eq!(manager.sync_enabled(), sync_enabled);
        }
    }

    #[test]
    fn ready_requires_measured_capacity() {
        let mut manager = DeviceManager::new();
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None, // unmeasured: preflight is pending
            used_capacity: None,
            firewire_id: None,
        });
        let preflight_passed = manager.set_ready_after_preflight(|_| Ok(()));
        assert!(!preflight_passed);
        // Missing measurements fail readiness with an actionable diagnostic.
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(
            manager.diagnostics(),
            Some("Total device capacity is unavailable.")
        );
        assert!(!manager.sync_enabled());

        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(14_907_051_520), // measured
            used_capacity: Some(2_048_000_000),
            firewire_id: None,
        });
        assert!(manager.set_ready_after_preflight(|info| {
            if info.model == "iPod Video 5G" {
                Ok(())
            } else {
                Err(PreflightError::Readiness(
                    "Unsupported iPod model.".to_string(),
                ))
            }
        }));
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert!(manager.sync_enabled());
    }

    #[test]
    fn replacing_device_resets_ready() {
        let mut manager = DeviceManager::new();
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(14_907_051_520),
            used_capacity: Some(2_048_000_000),
            firewire_id: None,
        });
        assert!(manager.set_ready_after_preflight(|_| Ok(())));
        assert!(manager.sync_enabled());

        // A new device is a new candidate: preflight is pending again.
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb2"),
            name: "iPod 2".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(14_907_051_520),
            used_capacity: Some(2_048_000_000),
            firewire_id: None,
        });
        assert_eq!(manager.state(), ConnectionState::Connecting);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn replacing_device_in_error_or_disconnected_resets_to_connecting() {
        let mut manager = DeviceManager::new();
        manager.set_state(ConnectionState::Error);
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb2"),
            name: "iPod 2".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(14_907_051_520),
            used_capacity: Some(2_048_000_000),
            firewire_id: None,
        });
        assert_eq!(manager.state(), ConnectionState::Connecting);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn ready_requires_successful_preflight_and_consistent_capacities() {
        let mut manager = DeviceManager::new();
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "unsupported model".to_string(),
            generation: "unknown".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: Some(500),
            firewire_id: None,
        });
        manager.set_state(ConnectionState::Ready);
        assert_eq!(manager.state(), ConnectionState::Connecting);
        assert!(!manager.sync_enabled());

        assert!(!manager.set_ready_after_preflight(|_| {
            Err(PreflightError::Readiness(
                "Unsupported iPod model.".to_string(),
            ))
        }));
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(manager.diagnostics(), Some("Unsupported iPod model."));
        assert!(!manager.sync_enabled());

        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: Some(1_001),
            firewire_id: None,
        });
        assert!(!manager.set_ready_after_preflight(|_| Ok(())));
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(
            manager.diagnostics(),
            Some("Used device capacity (1001 bytes) exceeds total capacity (1000 bytes).")
        );

        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: None,
            firewire_id: None,
        });
        assert!(!manager.set_ready_after_preflight(|_| Ok(())));
        assert_eq!(manager.state(), ConnectionState::Error);
        assert_eq!(
            manager.diagnostics(),
            Some("Used device capacity is unavailable.")
        );
    }

    #[test]
    fn backend_failure_is_unavailable_even_without_capacity() {
        for capacity in [Some(1_000), None] {
            let mut manager = DeviceManager::new();
            manager.set_device(DeviceInfo {
                mount_point: PathBuf::from("/mnt/ipod"),
                name: "iPod".to_string(),
                model: "iPod Video 5G".to_string(),
                generation: "5G".to_string(),
                model_number: None,
                firmware: None,
                total_capacity: capacity,
                used_capacity: capacity.map(|_| 500),
                firewire_id: None,
            });
            let device = manager.current_device().cloned();

            assert!(!manager.set_ready_after_preflight(|_| {
                Err(PreflightError::BackendUnavailable(
                    "libgpod is unavailable.".to_string(),
                ))
            }));
            assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
            assert_eq!(manager.state().ui_label(), "Sync unavailable");
            assert_eq!(manager.current_device(), device.as_ref());
            assert_eq!(manager.diagnostics(), Some("libgpod is unavailable."));
            assert!(!manager.sync_enabled());
        }
    }

    #[test]
    fn retry_after_backend_failure_requires_fresh_preflight() {
        let mut manager = DeviceManager::new();
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/mnt/ipod"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: Some(500),
            firewire_id: None,
        });
        assert!(
            !manager.set_ready_after_preflight(|_| Err(PreflightError::BackendUnavailable(
                "libgpod is unavailable.".to_string()
            )))
        );

        manager.set_state(ConnectionState::Connecting);
        assert_eq!(manager.diagnostics(), None);
        assert!(!manager.sync_enabled());
        assert!(manager.set_ready_after_preflight(|_| Ok(())));
        assert_eq!(manager.state(), ConnectionState::Ready);
        assert!(manager.sync_enabled());
    }

    #[test]
    fn preflight_only_runs_while_connecting() {
        let mut manager = DeviceManager::new();
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: Some(500),
            firewire_id: None,
        });

        for state in [ConnectionState::BackendUnavailable, ConnectionState::Error] {
            manager.set_state(state);
            manager.set_diagnostics("state diagnostic".to_string());
            let mut callback_ran = false;
            assert!(!manager.set_ready_after_preflight(|_| {
                callback_ran = true;
                Ok(())
            }));
            assert!(!callback_ran);
            assert_eq!(manager.state(), state);
            assert_eq!(manager.diagnostics(), Some("state diagnostic"));
            assert!(!manager.sync_enabled());
        }
    }

    #[test]
    fn entering_disconnected_clears_device_and_blocks_stale_preflight() {
        let mut manager = DeviceManager::new();
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: Some(500),
            firewire_id: None,
        });
        assert!(manager.set_ready_after_preflight(|_| Ok(())));
        manager.set_diagnostics("stale diagnostic".to_string());

        manager.set_state(ConnectionState::Disconnected);

        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(manager.current_device().is_none());
        assert_eq!(manager.diagnostics(), None);
        let mut callback_ran = false;
        assert!(!manager.set_ready_after_preflight(|_| {
            callback_ran = true;
            Ok(())
        }));
        assert!(!callback_ran);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn diagnostics_clear_on_retry_device_change_and_success() {
        let mut manager = DeviceManager::new();
        manager.set_diagnostics("old error".to_string());
        manager.set_state(ConnectionState::Connecting);
        assert_eq!(manager.diagnostics(), None);

        manager.set_diagnostics("stale error".to_string());
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: Some(1_000),
            used_capacity: Some(500),
            firewire_id: None,
        });
        assert_eq!(manager.diagnostics(), None);

        manager.set_diagnostics("preflight error".to_string());
        assert!(manager.set_ready_after_preflight(|_| Ok(())));
        assert_eq!(manager.diagnostics(), None);

        manager.set_diagnostics("device error".to_string());
        manager.clear_device();
        assert_eq!(manager.diagnostics(), None);
    }

    #[test]
    fn all_five_states_have_distinct_labels() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ];
        let labels = states
            .iter()
            .map(|state| state.ui_label())
            .collect::<Vec<_>>();
        assert!(labels.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(
            ConnectionState::BackendUnavailable.ui_label(),
            "Sync unavailable"
        );
    }

    #[test]
    fn only_ready_enables_sync() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Error,
        ] {
            assert!(
                !state.sync_enabled(),
                "sync unexpectedly enabled in {state:?}"
            );
        }
        assert!(ConnectionState::Ready.sync_enabled());
    }

    #[test]
    fn preflight_permits_only_connecting() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ] {
            assert!(
                !state.permits_preflight(),
                "preflight unexpectedly permitted in {state:?}"
            );
        }
        assert!(ConnectionState::Connecting.permits_preflight());
    }

    #[test]
    fn optional_fields_can_be_present_or_absent() {
        let info = DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: Some("12345".to_string()),
            firmware: Some("5.1.2".to_string()),
            total_capacity: Some(1490705152),
            used_capacity: Some(2048000000),
            firewire_id: None,
        };
        assert_eq!(info.firmware.as_deref(), Some("5.1.2"));
        assert_eq!(info.total_capacity, Some(1490705152));
        assert_eq!(info.used_capacity, Some(2048000000));
        assert_eq!(info.firewire_id, None);

        let sparse = DeviceInfo {
            mount_point: PathBuf::from("/dev/sdc1"),
            name: "iPod".to_string(),
            model: "iPod Video 5.5G".to_string(),
            generation: "5.5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None,
            used_capacity: None,
            firewire_id: None,
        };
        assert_eq!(sparse.firmware, None);
        assert_eq!(sparse.total_capacity, None);
        assert_eq!(sparse.used_capacity, None);

        let firewire_present = DeviceInfo {
            mount_point: PathBuf::from("/dev/sdc1"),
            name: "iPod".to_string(),
            model: "iPod Classic 6G".to_string(),
            generation: "6G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None,
            used_capacity: None,
            firewire_id: Some("A2000000:00008832:00010000:00000001-0x10000".to_string()),
        };
        assert_eq!(
            firewire_present.firewire_id,
            Some("A2000000:00008832:00010000:00000001-0x10000".to_string())
        );

        let firewire_absent = DeviceInfo {
            mount_point: PathBuf::from("/dev/sdc1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None,
            used_capacity: None,
            firewire_id: None,
        };
        assert_eq!(firewire_absent.firewire_id, None);
    }

    #[test]
    fn owned_data_without_raw_pointers() {
        let manager = DeviceManager::new();
        assert!(format!("{:?}", manager).contains("None"));

        // Every field of DeviceInfo and DeviceManager is a Rust-owned type
        // (String, PathBuf, u64, Option<...>); no raw pointers can leak into
        // the owned representation. Cloning must produce independent data.
        let info = DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None,
            used_capacity: None,
            firewire_id: None,
        };
        let mut clone = info.clone();
        assert_eq!(clone, info);
        // Cloning must produce independent owned data: mutating the clone must
        // not affect the original.
        clone.name = "changed".to_string();
        assert_eq!(info.name, "iPod".to_string());
    }

    #[test]
    fn clears_stale_device_information() {
        let mut manager = DeviceManager::new();
        manager.set_state(ConnectionState::Ready);
        manager.set_device(DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "old".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None,
            used_capacity: None,
            firewire_id: None,
        });
        manager.clear_device();
        assert!(manager.current_device().is_none());
        // A device must exist before sync can be enabled.
        assert_eq!(manager.state(), ConnectionState::Disconnected);
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn transitions_to_backend_unavailable() {
        let mut manager = DeviceManager::new();
        manager.set_state(ConnectionState::Connecting);
        manager.set_state(ConnectionState::BackendUnavailable);
        assert_eq!(manager.state(), ConnectionState::BackendUnavailable);
        assert_eq!(manager.state().ui_label(), "Sync unavailable");
        assert!(!manager.sync_enabled());
    }

    #[test]
    fn error_state_keeps_sync_disabled() {
        let mut manager = DeviceManager::new();
        manager.set_state(ConnectionState::Error);
        assert!(!manager.sync_enabled());
        assert_eq!(manager.state().ui_label(), "Error");
    }

    #[test]
    fn device_info_matches_plan_fields() {
        // The plan's exact field set, including every Option.
        let _ = DeviceInfo {
            mount_point: PathBuf::from("/dev/sdb1"),
            name: "iPod".to_string(),
            model: "iPod Video 5G".to_string(),
            generation: "5G".to_string(),
            model_number: None,
            firmware: None,
            total_capacity: None,
            used_capacity: None,
            firewire_id: None,
        };
    }
}
