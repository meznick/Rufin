//! B-004: non-mutating device readiness checks — the gate between
//! `Connecting` and `Ready`.
//!
//! [`preflight`] runs the checks and returns a classified, actionable
//! [`ReadinessError`] on the first failure. The pure check logic lives in
//! [`evaluate`] so the full matrix is unit-testable without a backend; the
//! production entry point only assembles a session and reads the mount.
//!
//! # Check order (first failure wins)
//!
//! 1. **Read-only mount** (`statvfs` `ST_RDONLY`): the cheapest check and
//!    the most fundamental blocker — a read-only mount can never accept a
//!    sync write, so nothing else matters. Checked before the parse too:
//!    on a ro mount there is no point re-parsing the device.
//! 2. **Session open** (parse): access denied, unreadable, or corrupt
//!    iTunesDB.
//! 3. **Artwork verdict**: `Missing` is corruption (declared entries not
//!    parsed, ArtworkDB missing/corrupt, or a failed proof-read); `Ok`
//!    passes, including a legitimately artless device.
//!
//! # Model policy (owner decision 2026-11-10, b006 delta decision 12)
//!
//! The model gates are **not refusals** anymore. An unsupported or
//! unresolved model — and a supported Classic missing its FireWire GUID —
//! is a *limited* device, not a broken one: an empty `SysInfo` is the
//! normal state of a freshly restored iPod, and the frontend owns the
//! function gating. The verdicts ride in `DeviceFacts`
//! (`model_supported`, `checksum_possible`, computed in
//! `retrieval::collect` via [`generation_supported`] and
//! [`generation_requires_firewire`]) and reach the UI through
//! `DeviceInfo`/snapshots. Consequently the artwork verdict for an
//! unresolved model (`SkippedUnknownModel`) passes here — it means "cannot
//! be assessed", not "artless", and the UI shows the limited state instead
//! of pretending the device is fine.
//!
//! Capacity is deliberately **not** checked here: B-002's
//! `DeviceManager::set_ready_after_preflight` validates the measured
//! capacities after the callback (present, non-zero, `used <= total`).
//!
//! # Write discipline
//!
//! Preflight is strictly read-only: session reads plus one `statvfs`. It
//! never initializes a device, never writes capability files (`SysInfo`,
//! `ArtworkDB`, helper files), and never calls anything that could: a
//! missing prerequisite is **reported**, not created (plan.md). The
//! env-gated gates prove the no-write guarantee with a tree hash
//! before/after.
//!
//! libgpod owns checksum *generation* (B-008+ write path); B-004 verifies
//! the prerequisites only.

use std::ffi::CString;
use std::path::Path;

use ipod::{ArtworkStatus, ArtworkVerdict, DeviceSession, IpodBackend};

/// Supported generations — the B-001 scope (Video 5/5.5g, Classic
/// 6/6.5/7g). Values are the `Itdb_IpodGeneration` enum constants in
/// `ipod::ffi::layouts`; the test `generation_consts_match_the_wrapper`
/// pins them against the wrapper so the two cannot drift.
const GENERATION_VIDEO_1: i32 = 16;
const GENERATION_VIDEO_2: i32 = 17;
const GENERATION_CLASSIC_1: i32 = 18;
const GENERATION_CLASSIC_2: i32 = 19;
const GENERATION_CLASSIC_3: i32 = 26;

/// Whether a resolved model generation is inside the supported set.
/// `pub(crate)` because `retrieval::collect` computes the
/// `model_supported` verdict from it; not refusal logic anymore (see the
/// module docs).
pub(crate) fn generation_supported(generation: i32) -> bool {
    matches!(
        generation,
        GENERATION_VIDEO_1
            | GENERATION_VIDEO_2
            | GENERATION_CLASSIC_1
            | GENERATION_CLASSIC_2
            | GENERATION_CLASSIC_3
    )
}

/// Classic 6/6.5/7G: the database checksum (HASH58) requires the FireWire
/// GUID (itdb_hash58.c:249). Video checksums are NONE — no requirement.
/// `pub(crate)`: feeds the `checksum_possible` verdict in
/// `retrieval::collect`.
pub(crate) fn generation_requires_firewire(generation: i32) -> bool {
    matches!(
        generation,
        GENERATION_CLASSIC_1 | GENERATION_CLASSIC_2 | GENERATION_CLASSIC_3
    )
}

/// A readiness failure and its user-facing diagnostic.
///
/// ipod-sync cannot depend on gtk-library's `PreflightError` (the
/// dependency direction is the other way), so this is the crate's own
/// classification; the glue maps it: `BackendAbi` →
/// `PreflightError::BackendUnavailable`, `Check` →
/// `PreflightError::Readiness`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessError {
    /// Backend-level: a soft-accepted distro build (`abi_verified = false`).
    /// The glue consumes the ABI verdict itself and never calls
    /// [`preflight`] with a bad ABI; this variant documents the
    /// classification contract for the mapping.
    BackendAbi { detail: String },
    /// A device-level check failed.
    Check {
        kind: ReadinessCheck,
        detail: String,
    },
}

/// Which readiness check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessCheck {
    /// Session open failed: access denied, unreadable, or corrupt iTunesDB.
    Database,
    /// Artwork verdict `Missing` — declared entries not parsed, ArtworkDB
    /// missing/corrupt, or a failed proof-read.
    Artwork,
    /// Mount is read-only (`statvfs` `ST_RDONLY`).
    ReadOnly,
}

impl std::fmt::Display for ReadinessCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Database => "database",
            Self::Artwork => "artwork",
            Self::ReadOnly => "read-only mount",
        })
    }
}

impl std::fmt::Display for ReadinessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BackendAbi { detail } => write!(f, "incompatible libgpod backend: {detail}"),
            Self::Check { kind, detail } => write!(f, "{detail} ({kind} check failed)"),
        }
    }
}

impl std::error::Error for ReadinessError {}

const READ_ONLY_DETAIL: &str = "the device is mounted read-only; sync needs a writable mount";

/// The pure check logic (see the module docs for the order). Everything is
/// fixture-constructible, so the whole matrix runs without a backend.
fn evaluate(artwork: &ArtworkStatus, read_only: bool) -> Result<(), ReadinessError> {
    if read_only {
        return Err(ReadinessError::Check {
            kind: ReadinessCheck::ReadOnly,
            detail: READ_ONLY_DETAIL.to_string(),
        });
    }

    match artwork.verdict {
        ArtworkVerdict::Ok => Ok(()),
        ArtworkVerdict::Missing => Err(ReadinessError::Check {
            kind: ReadinessCheck::Artwork,
            detail: format!(
                "the device artwork is damaged: {}. Syncing would write into a broken ArtworkDB",
                artwork
                    .diagnostic
                    .as_deref()
                    .unwrap_or("ArtworkDB declares entries that are not parsed")
            ),
        }),
        // The wrapper skips the artwork assessment when it cannot resolve
        // the model. Under the 2026-11-10 model policy an unresolved model
        // is a limited device, not a failure — the UI gates functions, so
        // "cannot be assessed" passes through (never silently: the limited
        // state is visible in the UI, B-007 §1.2).
        ArtworkVerdict::SkippedUnknownModel => Ok(()),
    }
}

/// Is the mount flagged read-only by the kernel?
///
/// A `statvfs` failure returns `false` with a warning — the read-only
/// verdict is then unknown, and either the session open or B-002's capacity
/// validation (failed `statvfs` at retrieval → `None` capacities) produces
/// the actionable failure instead. Never a fabricated read-only verdict.
fn mount_is_read_only(mount: &Path) -> bool {
    let Ok(path) = CString::new(mount.as_os_str().as_encoded_bytes()) else {
        tracing::warn!(mount = %mount.display(), "statvfs: mount point contains a NUL byte");
        return false;
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string; `stat` is a valid
    // out-parameter. `statvfs` performs no writes to the mount.
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if result != 0 {
        tracing::warn!(
            mount = %mount.display(),
            "statvfs failed; read-only state unknown"
        );
        return false;
    }
    stat.f_flag & libc::ST_RDONLY != 0
}

/// Run the readiness checks on `mount` with the loaded backend.
///
/// Read-only, never writes (see the module docs). The backend ABI verdict
/// is deliberately not a parameter: the glue consumes `BackendStatus`
/// itself and never calls this function with a soft-accepted build.
pub fn preflight(backend: &IpodBackend, mount: &Path) -> Result<(), ReadinessError> {
    if mount_is_read_only(mount) {
        return Err(ReadinessError::Check {
            kind: ReadinessCheck::ReadOnly,
            detail: READ_ONLY_DETAIL.to_string(),
        });
    }

    let session = DeviceSession::open(backend, mount).map_err(|error| ReadinessError::Check {
        kind: ReadinessCheck::Database,
        detail: format!("the device database cannot be opened or parsed: {error}"),
    })?;
    let artwork = session
        .artwork_status()
        .map_err(|error| ReadinessError::Check {
            kind: ReadinessCheck::Artwork,
            detail: format!("the artwork state cannot be assessed: {error}"),
        })?;

    evaluate(&artwork, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artwork(verdict: ArtworkVerdict, diagnostic: Option<&str>) -> ArtworkStatus {
        ArtworkStatus {
            verdict,
            tracks_with_thumbnails: 0,
            declared_entries: None,
            proof_read: None,
            diagnostic: diagnostic.map(str::to_string),
        }
    }

    const RW: bool = false;
    const RO: bool = true;

    fn check(result: Result<(), ReadinessError>, kind: ReadinessCheck) -> ReadinessError {
        match result {
            Ok(()) => panic!("expected a {kind} failure, got Ok"),
            Err(error) => {
                let ReadinessError::Check {
                    kind: actual,
                    detail,
                } = &error
                else {
                    panic!("expected a Check failure, got {error}");
                };
                assert_eq!(*actual, kind, "unexpected check kind: {error}");
                assert!(!detail.is_empty(), "the diagnostic must be non-empty");
                error
            }
        }
    }

    /// The local generation consts must not drift from the wrapper's
    /// `Itdb_IpodGeneration` enum.
    #[test]
    fn generation_consts_match_the_wrapper() {
        use ipod::ffi::layouts;
        assert_eq!(
            GENERATION_VIDEO_1,
            layouts::Itdb_IpodGeneration_ITDB_IPOD_GENERATION_VIDEO_1 as i32
        );
        assert_eq!(
            GENERATION_VIDEO_2,
            layouts::Itdb_IpodGeneration_ITDB_IPOD_GENERATION_VIDEO_2 as i32
        );
        assert_eq!(
            GENERATION_CLASSIC_1,
            layouts::Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_1 as i32
        );
        assert_eq!(
            GENERATION_CLASSIC_2,
            layouts::Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_2 as i32
        );
        assert_eq!(
            GENERATION_CLASSIC_3,
            layouts::Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_3 as i32
        );
    }

    /// The supported set is exactly the B-001 scope: Video 5/5.5G and
    /// Classic 6/6.5/7G — everything else is classified unsupported. The
    /// classification feeds the `model_supported` verdict, not a refusal.
    #[test]
    fn supported_generation_set_matches_the_b001_scope() {
        for generation in [
            GENERATION_VIDEO_1,
            GENERATION_VIDEO_2,
            GENERATION_CLASSIC_1,
            GENERATION_CLASSIC_2,
            GENERATION_CLASSIC_3,
        ] {
            assert!(
                generation_supported(generation),
                "generation {generation} must be inside the supported set"
            );
        }
        for generation in [0, 11, 12, 20, 27] {
            assert!(
                !generation_supported(generation),
                "generation {generation} must be outside the supported set"
            );
        }
    }

    /// Only the Classic set requires the FireWire GUID for the HASH58
    /// checksum; Video needs none. The classification feeds the
    /// `checksum_possible` verdict, not a refusal.
    #[test]
    fn firewire_requirement_matches_the_classic_set() {
        assert!(!generation_requires_firewire(GENERATION_VIDEO_1));
        assert!(!generation_requires_firewire(GENERATION_VIDEO_2));
        for generation in [
            GENERATION_CLASSIC_1,
            GENERATION_CLASSIC_2,
            GENERATION_CLASSIC_3,
        ] {
            assert!(generation_requires_firewire(generation));
        }
    }

    #[test]
    fn artwork_missing_fails_and_carries_the_verdict_diagnostic() {
        let error = check(
            evaluate(
                &artwork(
                    ArtworkVerdict::Missing,
                    Some("ArtworkDB declares 3 entries, 1 track carries thumbnails"),
                ),
                RW,
            ),
            ReadinessCheck::Artwork,
        );
        assert!(
            error.to_string().contains("3 entries"),
            "the wrapper diagnostic must survive: {error}"
        );
    }

    /// Under the 2026-11-10 model policy an unresolved model is a limited
    /// device, so the wrapper's skipped artwork assessment passes — the UI
    /// shows the limited state, never a silent artless device.
    #[test]
    fn artwork_skipped_unknown_model_passes() {
        evaluate(&artwork(ArtworkVerdict::SkippedUnknownModel, None), RW).unwrap();
    }

    #[test]
    fn legitimately_artless_artwork_passes() {
        evaluate(&artwork(ArtworkVerdict::Ok, None), RW).unwrap();
    }

    #[test]
    fn read_only_fails_first() {
        // The ro verdict wins over any other failure (here: a damaged
        // artwork verdict): it is the most fundamental blocker (see the
        // module docs).
        let error = check(
            evaluate(&artwork(ArtworkVerdict::Missing, None), RO),
            ReadinessCheck::ReadOnly,
        );
        assert!(
            error.to_string().contains("read-only"),
            "diagnostic must name the mount state: {error}"
        );
    }

    #[test]
    fn display_naming_and_labels() {
        let error = ReadinessError::Check {
            kind: ReadinessCheck::Database,
            detail: "boom".to_string(),
        };
        assert_eq!(error.to_string(), "boom (database check failed)");
        assert_eq!(
            ReadinessError::BackendAbi {
                detail: "distro build".to_string(),
            }
            .to_string(),
            "incompatible libgpod backend: distro build"
        );
        assert_eq!(ReadinessCheck::ReadOnly.to_string(), "read-only mount");
    }
}
