//! Safe, runtime-loaded Rust wrapper around **libgpod** — the C library that
//! reads and writes the iPod `iTunesDB` and `ArtworkDB` databases used by the
//! Video 5/5.5g and Classic generations (the two families Rufin targets).
//!
//! This crate implements R-06 / W-1..W-7 of `deltas/b007_delta.md`. It is the
//! *only* place in the process that touches libgpod; discovery, readiness,
//! sync, and UI tasks (B-003..B-006, B-008+) consume it. Nothing in this crate
//! depends on GTK or on a C toolchain — the backend is a shared library loaded
//! at runtime, so builds without the backend stay possible and report
//! [`BackendUnavailable`] instead of falling back to file copying.
//!
//! # Why a wrapper (and not plain FFI)
//!
//! The B-007 research (recorded in `deltas/b007_delta.md` §1, evidence in the
//! itl-player repo: `docs/LIBGPOD.md`, `NOTES.md` §20, `tools/libgpod_check.c`)
//! established three properties of libgpod 0.8.3 that a wrapper must handle:
//!
//! 1. **`itdb_write_file` side effects are not confined by the output path**
//!    (§1.1): the write path touches the device context — it rewrites
//!    `ArtworkDB`, and after success it unlinks `Play Counts`,
//!    `iTunesStats`, and `PlayCounts.plist` on the device mountpoint.
//!    Parse paths can also mutate foreign trees (`itdb_hash72_extract_hash_info`
//!    can create `Device/HashInfo`; `ipod_db_get_artwork_db_path` attempts
//!    `g_mkdir` for a missing `Artwork/`). Reads here therefore run only on
//!    disposable snapshots or verified device trees, and every read-side call
//!    is exercised against a tree hash (§ W-5, [`treehash`]).
//! 2. **Parse success never implies artwork success** (§1.2): `itdb_parse`
//!    ignores the artwork parser's result, and with an empty `SysInfo` the
//!    model is unknown, `itdb_device_supports_artwork()` returns FALSE, and
//!    artwork is silently skipped while tracks parse fine. The wrapper makes
//!    the model gate and the artwork verdict explicit ([`ArtworkStatus`]).
//! 3. **The device's numeric track ids are not a stable cross-sync identity**
//!    (plan.md / B-001): consumers must key on content, not `dbid`.
//!
//! # Loading and ABI
//!
//! [`IpodBackend::load`] searches (1) the directory named by the
//! `RUFIN_LIBGPOD_DIR` environment variable, then (2) the dynamic loader's
//! default paths for `libgpod.so.4`. Once loaded it resolves the real path of
//! the library (via `dladdr` on `itdb_parse`) and looks for a
//! `rufin-libgpod.abi` marker next to it. The marker is a small text file
//! produced by the packaging step:
//!
//! ```text
//! VERSION=0.8.3
//! LAYOUTS=sha256:<hex of src/ffi/layouts.rs>
//! ```
//!
//! A present-but-mismatched marker is a hard [`BackendUnavailable`] (the
//! struct layouts in [`ffi::layouts`] are only guaranteed for the pinned
//! build). A missing marker (a distro-packaged libgpod) is accepted with a
//! `tracing::warn`, because the strict policy is the packaging follow-up of
//! `deltas/b007_delta.md` §5. Read-side symbols are required at load time;
//! write-side symbols are resolved eagerly as well, but their absence only
//! fails write calls, never the load.
//!
//! # Safety model
//!
//! - All `unsafe` lives in [`ffi`] (symbol types, struct layouts, GList/GError
//!   access) and in the session (alloc/free pairing). Field access on the
//!   native structs goes through small private accessor fns with per-field
//!   invariant comments.
//! - `itdb_free` frees the whole database tree (tracks, playlists, artwork).
//!   The wrapper never frees objects individually and never stores native
//!   pointers beyond the session.
//! - Every entry point (load, open, read calls, writes, drop) is serialized
//!   through one global mutex; the lock is never held across user code.
//! - [`DeviceSession`] is `!Send` (it owns raw pointers). Consumers wrap it in
//!   `spawn_blocking` — that is B-003+ territory, not this crate's job.
//! - The read surface returns fully owned values ([`TrackInfo`], [`DeviceInfo`],
//!   [`PlaylistInfo`], [`ArtworkStatus`]); no GLib string, `GError`, or `GList`
//!   reference escapes a call.
//!
//! # The write gate (W-7)
//!
//! The write APIs exist on [`DeviceSession`] (the owner's sketch,
//! `deltas/b007_delta.md` §7.1) but are compiled behind the `sync-writes`
//! cargo feature and refuse at runtime unless it is enabled. No sync path may
//! turn the feature on before B-010. The feature also unlocks the
//! `testkit` module, which builds disposable fixtures with libgpod's own
//! `itdb_init_ipod` — the same methodology the §1.1 experiments used, now
//! inside the test suite.
//!
//! # Platform support
//!
//! Linux (glibc/musl) with the 64-bit LP64 layout generated from the pinned
//! 0.8.3 headers. On other platforms [`IpodBackend::load`] returns
//! `BackendUnavailable::UnsupportedPlatform` without attempting to load
//! anything. The layouts are LP64-specific; a 32-bit or non-Linux backend
//! needs the packaging follow-up and a regenerated [`ffi::layouts`].
//!
//! # References
//!
//! - `deltas/b007_delta.md` (this crate's spec; the R-06 requirements)
//! - `deltas/b001_research.md` (why libgpod at all, the R1/R2/R3 gates)
//! - itl-player repo: `docs/LIBGPOD.md` (pinned build recipe), `NOTES.md` §20
//!   (device-format facts and the cross-check harness numbers)

#![deny(unsafe_op_in_unsafe_fn)]
// The workspace denies `unsafe_code` globally; this crate is the single FFI
// boundary to libgpod and cannot exist without it. All `unsafe` is contained
// in `ffi/` + `session` (per-call safety docs); the rest of the crate stays
// safe.
#![allow(unsafe_code)]

mod artwork;
mod backend;
pub mod ffi;
mod session;
#[cfg(feature = "sync-writes")]
#[doc(hidden)]
pub mod testkit;
pub mod treehash;
mod types;
mod write;

use std::sync::{Mutex, OnceLock};

/// The one serialization point for all native access (W-6).
///
/// Held for the duration of every wrapper entry point — load, open, reads,
/// gated writes, and drop — and never across user code. libgpod is not
/// reentrant by contract, and the side-effect research (§1.1) makes
/// accidental interleaving worse than a bottleneck here. Poisoning is
/// recovered (`into_inner`): a panic while holding the lock must not brick
/// every later call.
static NATIVE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Test-only instrumentation: counts the calls currently *inside* the native
/// lock. Any overlap or nesting is an immediate assertion — that is what the
/// serialization unit test (W-6) relies on. Compiled out for non-test builds
/// and for integration-test builds of the library (the production path is
/// untouched).
#[cfg(test)]
static ACTIVE_NATIVE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn with_native<T>(f: impl FnOnce() -> T) -> T {
    let lock = NATIVE_LOCK.get_or_init(|| Mutex::new(()));
    let guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    #[cfg(test)]
    {
        let active = ACTIVE_NATIVE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            active, 0,
            "with_native is not serialized: a call overlapped or nested inside the lock"
        );
    }
    let result = f();
    #[cfg(test)]
    {
        ACTIVE_NATIVE_CALLS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
    let _guard = guard;
    result
}

pub use backend::{BackendUnavailable, IpodBackend, MARKER_FILE_NAME};
pub use session::DeviceSession;
pub use treehash::{TreeHashError, tree_hash};
pub use types::{
    ArtworkInput, ArtworkStatus, ArtworkVerdict, DeviceInfo, IpodError, ModelInfo, PlaybackStats,
    PlaylistChanges, PlaylistInfo, ProofReadReport, TrackInfo, TrackMetadata,
};
pub use write::OperationFailure;

#[cfg(test)]
mod tests {
    /// W-6: `with_native` must serialize — concurrent callers may never be
    /// inside the lock at the same time, and no call may nest inside another
    /// (the crate's invariant: every entry point locks, inner helpers never
    /// do). The overlap counter in `with_native` asserts both; this test
    /// hammers it from many threads. If the mutex were missing or released
    /// early, the assertion would fire (and a nested call would too).
    #[test]
    fn with_native_serializes_concurrent_calls() {
        use super::with_native;
        let mut threads = Vec::new();
        for _ in 0..8 {
            threads.push(std::thread::spawn(|| {
                for _ in 0..200 {
                    with_native(|| {
                        // Keep the lock held long enough that unsynchronized
                        // threads would realistically overlap.
                        std::hint::spin_loop();
                    });
                }
            }));
        }
        for thread in threads {
            thread.join().expect("worker thread panicked");
        }
    }
}
