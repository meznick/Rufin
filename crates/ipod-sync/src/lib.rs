//! Mounted-device discovery and metadata retrieval for Rufin's iPod
//! integration (B-003 discovery, B-005 retrieval).
//!
//! This crate finds mounted candidate volumes — mount points whose root
//! contains an [`IPOD_CONTROL_DIR`] directory — and reports them together
//! with the availability of the libgpod backend (the [`ipod`] wrapper crate).
//! It is the first consumer of the wrapper: discovery, readiness, sync, and
//! UI tasks (B-003..B-006, B-008+) build on it.
//!
//! # What discovery is and is not
//!
//! Detection is **filesystem-level only**: one non-recursive
//! `symlink_metadata` probe per mount. A candidate is a mount whose root has
//! an `iPod_Control/` *directory*; the mount's fstype rides along for
//! diagnostics. Everything else (unrelated volumes, `iPod_Control` files,
//! transient probe errors) is skipped; a mount whose probe fails with a
//! permission error is reported as blocked, never silently dropped and never
//! treated as a confirmed iPod.
//!
//! Discovery **never parses a database and never opens a session** on a
//! candidate. The wrapper's research (`deltas/b007_delta.md` §1.1) shows
//! libgpod parse paths can mutate live device trees (a missing `Artwork/`
//! gets `g_mkdir`-ed), so a discovery-time parse would not be provably
//! write-free. Zero-write discovery is guaranteed structurally — the only
//! operations are mount-table reads, one stat per mount, and the backend
//! `dlopen` — and it is proven by a tree-hash fixture.
//!
//! Parsing candidates, retrieving metadata, and measuring capacity belong to
//! B-005/B-004 (see [`retrieval`]); discovery must never mark a candidate
//! Ready. The FSM glue in `gtk-library` (`ipod_discovery::apply_scan`)
//! asserts exactly that.
//!
//! # Backend availability
//!
//! [`Discovery`] loads the backend once (lazily, on the first [`Discovery::scan`])
//! and caches success; a failed load is retried on every scan, so a backend can
//! only transition from unavailable to loaded at runtime. Every
//! [`ScanReport`] carries the status explicitly:
//! [`BackendStatus::Loaded`] with `IpodBackend::abi_verified` (the ABI
//! marker verdict — `false` means a distro build was soft-accepted and the
//! consumer should surface the unverified state), or
//! [`BackendStatus::Unavailable`] with the load failure as the reason.
//! Discovery keeps working without a backend: candidates are found and
//! reported regardless; there is no fallback to file copying anywhere.
//!
//! # Platform support
//!
//! The only production [`MountSource`] reads `/proc/self/mountinfo`
//! (Linux-only), matching the wrapper's Linux-only platform support. The
//! trait boundary exists so tests feed synthetic mount tables.
//!
//! # References
//!
//! - `deltas/b003_delta.md` (this crate's spec and the fixture matrix)
//! - `deltas/b001_research.md` §3 (detection research)
//! - `plan.md` Connection FSM (the consumer contract)

pub mod discovery;
pub mod mounts;
pub mod preflight;
pub mod retrieval;

pub use discovery::{
    BackendStatus, BlockedMount, Candidate, Discovery, IPOD_CONTROL_DIR, ScanReport,
};
pub use mounts::{MountEntry, MountError, MountSource, ProcMountInfo};
pub use preflight::{ReadinessCheck, ReadinessError, preflight};
pub use retrieval::{DeviceFacts, DeviceRead, RetrieveError, collect};
