//! The discovery engine: scan mounts, detect `iPod_Control` candidates, and
//! report the backend status.
//!
//! See the crate root docs for the guarantees: filesystem-level detection
//! only (no parse, no session, no writes), backend loaded once and retried on
//! failure, and a per-scan diff so removal is observable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ipod::{BackendSearch, IpodBackend};

use crate::mounts::{MountEntry, MountError, MountSource};

/// Name of the marker directory at a candidate mount's root. Presence (as a
/// real directory) is the only detection signal.
pub const IPOD_CONTROL_DIR: &str = "iPod_Control";

/// Backend availability as of the last scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendStatus {
    /// The library loaded. `abi_verified` = the ABI marker matched the pinned
    /// layout set. `false` means a distro build was soft-accepted by the
    /// SONAME policy — consumers must surface the unverified state, not trust
    /// it silently.
    Loaded { abi_verified: bool },
    /// The load failed; the reason is the `BackendUnavailable` display string.
    Unavailable { reason: String },
}

/// A mount whose root could not be probed because access was denied.
///
/// Reported, never silently dropped — but NOT a candidate (unconfirmed):
/// preflight (B-004) owns access failures of confirmed candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedMount {
    pub mount_point: PathBuf,
    pub error: String,
}

/// A confirmed candidate: the mount root contains an [`IPOD_CONTROL_DIR`]
/// directory. Detection only — the candidate is never parsed and never Ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub mount_point: PathBuf,
    pub fstype: String,
}

/// One scan's findings plus the diff against the previous scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Always present: `Loaded` or `Unavailable` with the reason.
    pub backend: BackendStatus,
    /// Confirmed candidates, sorted by mount point (deduplicated).
    pub candidates: Vec<Candidate>,
    /// Mounts whose probe failed with a permission error, sorted by mount
    /// point.
    pub blocked: Vec<BlockedMount>,
    /// Candidate mount points present now and absent in the previous scan
    /// (all of them on the first scan), sorted.
    pub appeared: Vec<PathBuf>,
    /// Candidate mount points present in the previous scan and absent now,
    /// sorted.
    pub removed: Vec<PathBuf>,
}

/// Mounted-device discovery over a mount source.
///
/// Owns one shared backend handle: loads lazily on the first scan, caches
/// success, retries failure on every scan (cheap, and it gives B-004's retry
/// semantics a hook — a backend can only transition unavailable → loaded at
/// runtime, never back). Never parses candidate databases.
#[derive(Debug)]
pub struct Discovery {
    /// The loaded backend, cached after the first successful load.
    backend: Option<IpodBackend>,
    /// The last load failure's display string; retried on every scan until
    /// the load succeeds.
    load_error: Option<String>,
    /// How the backend is located. Tests use `BackendSearch::Explicit` so
    /// they never mutate the process environment; production uses
    /// `BackendSearch::FromEnv`.
    search: BackendSearch,
    /// Candidate mount points of the previous scan; `None` before the first
    /// scan.
    last_candidates: Option<BTreeSet<PathBuf>>,
}

impl Default for Discovery {
    fn default() -> Self {
        Self::new()
    }
}

impl Discovery {
    /// Production discovery: the backend is located via `RUFIN_LIBGPOD_DIR`
    /// or the dynamic loader's default paths.
    pub fn new() -> Self {
        Self::with_backend_search(BackendSearch::FromEnv)
    }

    /// Discovery with an explicit backend location (used by tests; never
    /// touches the process environment).
    pub fn with_backend_search(search: BackendSearch) -> Self {
        Self {
            backend: None,
            load_error: None,
            search,
            last_candidates: None,
        }
    }

    /// The loaded backend, `None` until a scan has loaded it successfully.
    /// B-005 consumes this to open sessions for metadata retrieval.
    pub fn backend(&self) -> Option<&IpodBackend> {
        self.backend.as_ref()
    }

    /// Scan all mounts, detect candidates, and diff against the previous scan.
    pub fn scan(&mut self, source: &dyn MountSource) -> Result<ScanReport, MountError> {
        self.ensure_backend();
        let mounts = source.mounts()?;

        // Probe every mount; dedupe by mount point (later entries win).
        let mut found: BTreeMap<PathBuf, MountEntry> = BTreeMap::new();
        let mut blocked: BTreeMap<PathBuf, String> = BTreeMap::new();
        for entry in mounts {
            match probe_candidate(&entry.mount_point) {
                ProbeResult::Candidate => {
                    found.insert(entry.mount_point.clone(), entry);
                }
                ProbeResult::Blocked(error) => {
                    blocked.insert(entry.mount_point.clone(), error);
                }
                ProbeResult::NotCandidate => {}
            }
        }

        let current: BTreeSet<PathBuf> = found.keys().cloned().collect();
        let previous = self.last_candidates.replace(current.clone());
        let (appeared, removed) = match previous {
            Some(previous) => (
                current.difference(&previous).cloned().collect(),
                previous.difference(&current).cloned().collect(),
            ),
            // First scan: everything found appeared, nothing was removed.
            None => (current.iter().cloned().collect(), Vec::new()),
        };

        let backend = match &self.backend {
            Some(backend) => BackendStatus::Loaded {
                abi_verified: backend.abi_verified,
            },
            None => BackendStatus::Unavailable {
                reason: self
                    .load_error
                    .clone()
                    .unwrap_or_else(|| "backend not loaded".to_string()),
            },
        };

        Ok(ScanReport {
            backend,
            candidates: found
                .into_values()
                .map(|entry| Candidate {
                    mount_point: entry.mount_point,
                    fstype: entry.fstype,
                })
                .collect(),
            blocked: blocked
                .into_iter()
                .map(|(mount_point, error)| BlockedMount { mount_point, error })
                .collect(),
            appeared,
            removed,
        })
    }

    /// Load the backend if it is not loaded yet. A failed load is retried on
    /// every scan; success caches the handle for the process lifetime.
    fn ensure_backend(&mut self) {
        if self.backend.is_some() {
            return;
        }
        match IpodBackend::load_with(self.search.clone()) {
            Ok(backend) => {
                tracing::info!(
                    abi_verified = backend.abi_verified,
                    "libgpod backend loaded"
                );
                self.backend = Some(backend);
                self.load_error = None;
            }
            Err(error) => {
                tracing::debug!(%error, "libgpod backend unavailable");
                self.load_error = Some(error.to_string());
            }
        }
    }
}

/// The outcome of probing one mount root for [`IPOD_CONTROL_DIR`].
enum ProbeResult {
    /// The mount root contains an `iPod_Control/` directory.
    Candidate,
    /// The probe failed with a permission error: reported as blocked.
    Blocked(String),
    /// No `iPod_Control` directory: unrelated volume (or a transient probe
    /// error that carries no iPod signal).
    NotCandidate,
}

/// Probe a mount root for the `iPod_Control/` directory (a single
/// non-recursive stat). Never writes.
fn probe_candidate(root: &Path) -> ProbeResult {
    match std::fs::symlink_metadata(root.join(IPOD_CONTROL_DIR)) {
        Ok(meta) if meta.file_type().is_dir() => ProbeResult::Candidate,
        Ok(_) => {
            tracing::debug!(
                mount = %root.display(),
                "{IPOD_CONTROL_DIR} exists but is not a directory; not a candidate"
            );
            ProbeResult::NotCandidate
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProbeResult::NotCandidate,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::debug!(mount = %root.display(), %error, "mount probe denied");
            ProbeResult::Blocked(error.to_string())
        }
        Err(error) => {
            tracing::debug!(
                mount = %root.display(),
                %error,
                "mount probe failed; skipping (transient error, not an iPod signal)"
            );
            ProbeResult::NotCandidate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_detects_ipod_control_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(IPOD_CONTROL_DIR)).unwrap();
        assert!(matches!(
            probe_candidate(dir.path()),
            ProbeResult::Candidate
        ));
    }

    #[test]
    fn probe_ignores_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_candidate(dir.path()),
            ProbeResult::NotCandidate
        ));
    }

    #[test]
    fn probe_ignores_ipod_control_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(IPOD_CONTROL_DIR), b"not a directory").unwrap();
        assert!(matches!(
            probe_candidate(dir.path()),
            ProbeResult::NotCandidate
        ));
    }
}
