//! Backend loading and the ABI marker check (W-1 / W-6).
//!
//! `IpodBackend::load` is the only way to obtain a backend. The loader:
//!
//! 1. Locates `libgpod.so.4` — from `RUFIN_LIBGPOD_DIR` when set (the dev
//!    setup points it at the pinned install's `lib/`), else the dynamic
//!    loader's default paths (which include `LD_LIBRARY_PATH`).
//! 2. Opens it with `dlopen` and resolves the **read-side** symbol table; a
//!    missing required symbol is a hard `BackendUnavailable` (a file that is
//!    not really libgpod fails here — this is also the W-1 "incompatible
//!    library" signal).
//! 3. Resolves the real path of the library (`dladdr` on `itdb_parse`, so
//!    symlinks resolve to the file that actually provided the symbol) and
//!    applies the soft ABI policy from the crate root docs:
//!    `rufin-libgpod.abi` present and mismatched → `AbiMismatch`; present but
//!    unreadable → `AbiMismatch` (exists-but-unverifiable is treated as a
//!    mismatch, never silently accepted); absent → proceed with a warning and
//!    `IpodBackend::abi_verified == false`.
//! 4. Resolves the write-side table (absence tolerated, see
//!    [`ffi::symbols::resolve_write`]) and the GLib helpers (absence
//!    tolerated with a leak-and-warn policy, see [`ffi::symbols::GlibHelpers`]).
//!
//! Loading is serialized with every other wrapper entry point through
//! [`crate::with_native`]; it is a one-time operation but a second call
//! simply performs the checks again and returns a fresh handle.

pub use crate::types::BackendUnavailable;

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use libloading::Library;

use crate::ffi::layouts_sha::LAYOUTS_SHA256;
use crate::ffi::symbols::{GlibHelpers, ReadSymbols, WriteSymbols};
use crate::with_native;

/// File name of the ABI marker produced by the packaging step and looked up
/// next to the resolved `libgpod.so.4` (see the crate root docs).
pub const MARKER_FILE_NAME: &str = "rufin-libgpod.abi";

/// SONAME major the wrapper is written against. The soft policy trusts the
/// SONAME only when no marker is present; the strict packaged policy is the
/// follow-up of `deltas/b007_delta.md` §5.
const SONAME: &str = "libgpod.so.4";

/// Pinned upstream version the layouts were generated from.
const MARKER_VERSION: &str = "0.8.3";

/// How `load_with` locates the library. `load()` uses [`BackendSearch::FromEnv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSearch {
    /// Read the `RUFIN_LIBGPOD_DIR` environment variable; unset = fall
    /// through to [`BackendSearch::DefaultPaths`].
    FromEnv,
    /// `dlopen("libgpod.so.4")` through the loader's default paths.
    DefaultPaths,
    /// `libgpod.so.4` (or `lib/libgpod.so.4`) inside this directory. Used by
    /// tests so they never mutate the process environment.
    Explicit(PathBuf),
}

/// A loaded, checked libgpod backend. Created once via [`IpodBackend::load`].
///
/// Field order matters: the `Library` is declared **last**, so it drops after
/// the `Symbol`s that point into it.
pub struct IpodBackend {
    pub(crate) read: ReadSymbols,
    pub(crate) write: Option<WriteSymbols>,
    pub(crate) glib: GlibHelpers,
    /// Whether the ABI marker matched the pinned layout set. `false` = the
    /// marker was absent (a distro build) and the SONAME soft policy was
    /// applied — consumers (B-003's FSM) should surface the unverified-ABI
    /// state to the user, not treat it as a full pin match.
    pub abi_verified: bool,
    /// Held only to keep the library loaded for the process lifetime (the
    /// resolved function pointers in the tables above point into it).
    /// Declared **last** on purpose: fields drop in declaration order, and
    /// the library must outlive every resolved pointer.
    _library: Library,
}

impl IpodBackend {
    /// The resolved read-side symbols (required; the load gate).
    pub(crate) fn read(&self) -> &ReadSymbols {
        &self.read
    }

    /// The resolved write-side symbols, `None` when the library lacks them.
    pub(crate) fn write(&self) -> Option<&WriteSymbols> {
        self.write.as_ref()
    }

    /// The GLib helpers resolved through the libgpod dependency tree.
    pub(crate) fn glib(&self) -> &GlibHelpers {
        &self.glib
    }
}

impl IpodBackend {
    /// Load and verify the backend (W-1). Never panics; every failure mode is
    /// an actionable [`BackendUnavailable`].
    pub fn load() -> Result<Self, BackendUnavailable> {
        Self::load_with(BackendSearch::FromEnv)
    }

    /// [`IpodBackend::load`] with an explicit search strategy.
    pub fn load_with(search: BackendSearch) -> Result<Self, BackendUnavailable> {
        with_native(|| Self::load_inner(search))
    }

    fn load_inner(search: BackendSearch) -> Result<Self, BackendUnavailable> {
        #[cfg(not(unix))]
        {
            let _ = search;
            return Err(BackendUnavailable::UnsupportedPlatform);
        }
        #[cfg(unix)]
        {
            let path = locate(&search)?;
            let library = unsafe { Library::new(&path) }.map_err(|e| {
                tracing::debug!(path = %path.display(), loader_error = %e, "dlopen failed");
                BackendUnavailable::Load {
                    details: format!("dlopen({}) failed: {e}", path.display()),
                }
            })?;

            // The read table doubles as the "is this really libgpod" check.
            let read = crate::ffi::symbols::resolve_read(&library)?;

            // dladdr on itdb_parse gives the file that actually provided the
            // symbol (through symlinks), which is where the marker must live.
            let real_path = library_real_path(read.itdb_parse).unwrap_or(path.clone());
            let abi_verified = check_marker(&real_path)?;

            let write = crate::ffi::symbols::resolve_write(&library);
            if write.is_none() {
                tracing::warn!("libgpod write symbols missing; reads work, writes will fail");
            }
            let glib = GlibHelpers::resolve(&library);

            Ok(Self {
                read,
                write,
                glib,
                abi_verified,
                _library: library,
            })
        }
    }
}

impl std::fmt::Debug for IpodBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The library handle has no Debug; the useful identity is the set of
        // resolved tables, which is fixed by the load checks.
        f.debug_struct("IpodBackend").finish_non_exhaustive()
    }
}

/// Turn the search strategy into a concrete candidate path, or fail with an
/// actionable diagnostic.
#[cfg(unix)]
fn locate(search: &BackendSearch) -> Result<PathBuf, BackendUnavailable> {
    let dir = match search {
        BackendSearch::FromEnv => match std::env::var_os("RUFIN_LIBGPOD_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => return Ok(PathBuf::from(SONAME)),
        },
        BackendSearch::DefaultPaths => return Ok(PathBuf::from(SONAME)),
        BackendSearch::Explicit(dir) => dir.clone(),
    };

    // A directory was requested (env or explicit): the library must exist in
    // it, and a missing file is a distinct, actionable diagnostic.
    for candidate in [dir.join(SONAME), dir.join("lib").join(SONAME)] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(BackendUnavailable::MissingLibrary(format!(
        "no {SONAME} in {} (checked {SONAME} and lib/{SONAME})",
        dir.display()
    )))
}

/// Real path of the file that provides `itdb_parse`, via `dladdr`.
///
/// `None` on platforms/loaders where `dladdr` is unavailable or fails — the
/// caller falls back to the requested path (and the marker check then runs
/// against that).
#[cfg(unix)]
fn library_real_path(probe: crate::ffi::symbols::FnItdbParse) -> Option<PathBuf> {
    // The function pointer value is the symbol address; cast to a data
    // pointer for dladdr (the usual, load-portable idiom).
    let address = probe as *const c_void;
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `address` is a live symbol pointer from this process; `info` is
    // a valid out-pointer. dladdr only fills it.
    let rc = unsafe { libc::dladdr(address, &mut info) };
    if rc == 0 || info.dli_fname.is_null() {
        return None;
    }
    // SAFETY: dli_fname is a NUL-terminated string owned by the loader, valid
    // for the process lifetime.
    Some(PathBuf::from(
        unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
            .to_string_lossy()
            .into_owned(),
    ))
}

/// Apply the soft marker policy to the resolved library path. See the module
/// docs for the decision table.
///
/// Returns `Ok(true)` when the marker matched the pinned layout set, and
/// `Ok(false)` when it was absent (the SONAME soft policy) — the flag feeds
/// [`IpodBackend::abi_verified`].
fn check_marker(library_path: &Path) -> Result<bool, BackendUnavailable> {
    let marker_path = library_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MARKER_FILE_NAME);
    match std::fs::read_to_string(&marker_path) {
        Ok(text) => {
            let marker = parse_marker(&text).map_err(BackendUnavailable::AbiMismatch)?;
            if marker.version != MARKER_VERSION {
                return Err(BackendUnavailable::AbiMismatch(format!(
                    "{} declares VERSION={}, this crate is pinned to {MARKER_VERSION}",
                    marker_path.display(),
                    marker.version
                )));
            }
            if marker.layouts != LAYOUTS_SHA256 {
                return Err(BackendUnavailable::AbiMismatch(format!(
                    "{} declares LAYOUTS={}, this crate expects {LAYOUTS_SHA256}",
                    marker_path.display(),
                    marker.layouts
                )));
            }
            tracing::debug!(path = %marker_path.display(), "ABI marker matches");
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Soft policy: distro build without our marker — trust the SONAME
            // with a warning. Strictness is the packaging follow-up; the
            // `false` return keeps the unverified state visible to callers.
            tracing::warn!(
                path = %library_path.display(),
                "no {MARKER_FILE_NAME} next to libgpod; trusting SONAME {SONAME} only"
            );
            Ok(false)
        }
        Err(e) => Err(BackendUnavailable::AbiMismatch(format!(
            "cannot read {}: {e}",
            marker_path.display()
        ))),
    }
}

/// Parsed `rufin-libgpod.abi` content. The format is deliberately strict so a
/// corrupted or future-format marker can never be mistaken for a match:
/// `KEY=VALUE` lines, `#` comments, no unknown keys, no duplicates, and both
/// `VERSION` and `LAYOUTS` are required.
#[derive(Debug, PartialEq, Eq)]
struct AbiMarker {
    version: String,
    layouts: String,
}

/// Parse marker text; returns a human-readable reason on any deviation.
fn parse_marker(text: &str) -> Result<AbiMarker, String> {
    let mut version: Option<String> = None;
    let mut layouts: Option<String> = None;
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {} is not KEY=VALUE", lineno + 1));
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            return Err(format!("line {} has an empty value", lineno + 1));
        }
        match key {
            "VERSION" => {
                if version.replace(value.to_string()).is_some() {
                    return Err(format!("line {}: duplicate VERSION", lineno + 1));
                }
            }
            "LAYOUTS" => {
                if layouts.replace(value.to_string()).is_some() {
                    return Err(format!("line {}: duplicate LAYOUTS", lineno + 1));
                }
            }
            other => return Err(format!("line {}: unknown key {other:?}", lineno + 1)),
        }
    }
    Ok(AbiMarker {
        version: version.ok_or("missing VERSION")?,
        layouts: layouts.ok_or("missing LAYOUTS")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_parses_minimal() {
        let marker = parse_marker("VERSION=0.8.3\nLAYOUTS=sha256:abc\n").unwrap();
        assert_eq!(marker.version, "0.8.3");
        assert_eq!(marker.layouts, "sha256:abc");
    }

    #[test]
    fn marker_accepts_comments_and_blanks() {
        let marker = parse_marker(
            "# packaging-generated\n\n  VERSION = 0.8.3  \nLAYOUTS=sha256:x\n# trailing\n",
        )
        .unwrap();
        assert_eq!(marker.version, "0.8.3");
        assert_eq!(marker.layouts, "sha256:x");
    }

    #[test]
    fn marker_rejects_unknown_keys() {
        let err = parse_marker("VERSION=0.8.3\nLAYOUTS=sha256:x\nFUTURE=1\n").unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
    }

    #[test]
    fn marker_rejects_missing_keys() {
        assert!(
            parse_marker("VERSION=0.8.3\n")
                .unwrap_err()
                .contains("missing LAYOUTS")
        );
        assert!(
            parse_marker("LAYOUTS=sha256:x\n")
                .unwrap_err()
                .contains("missing VERSION")
        );
    }

    #[test]
    fn marker_rejects_duplicates() {
        let err = parse_marker("VERSION=0.8.3\nVERSION=0.8.3\nLAYOUTS=sha256:x\n").unwrap_err();
        assert!(err.contains("duplicate VERSION"), "{err}");
    }

    #[test]
    fn marker_rejects_malformed_lines() {
        assert!(
            parse_marker("just a line\n")
                .unwrap_err()
                .contains("KEY=VALUE")
        );
        assert!(
            parse_marker("VERSION=\nLAYOUTS=sha256:x\n")
                .unwrap_err()
                .contains("empty value")
        );
    }

    /// The marker format in the crate root docs must parse — the docs are
    /// the de-facto spec for the packaging step.
    #[test]
    fn marker_doc_example_parses() {
        let doc_example = format!("VERSION={MARKER_VERSION}\nLAYOUTS={LAYOUTS_SHA256}\n");
        let marker = parse_marker(&doc_example).unwrap();
        assert_eq!(marker.version, MARKER_VERSION);
        assert_eq!(marker.layouts, LAYOUTS_SHA256);
    }

    /// W-1: an explicit directory without the library is an actionable
    /// `MissingLibrary`, before any dlopen is attempted.
    #[test]
    fn missing_library_is_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            IpodBackend::load_with(BackendSearch::Explicit(dir.path().to_path_buf())).unwrap_err();
        match err {
            BackendUnavailable::MissingLibrary(msg) => {
                assert!(msg.contains(SONAME), "{msg}");
            }
            other => panic!("expected MissingLibrary, got {other}"),
        }
    }

    /// W-1: a file that is not libgpod (here: libc) fails with
    /// `MissingSymbol`, never a panic or a half-loaded backend.
    #[test]
    fn wrong_library_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // libc.so.6 has no libgpod symbols; it is guaranteed to exist.
        let libc = PathBuf::from("/lib/x86_64-linux-gnu/libc.so.6");
        if !libc.exists() {
            return; // non-glibc layout: nothing to test
        }
        std::fs::copy(&libc, dir.path().join(SONAME)).unwrap();
        let err =
            IpodBackend::load_with(BackendSearch::Explicit(dir.path().to_path_buf())).unwrap_err();
        match err {
            BackendUnavailable::MissingSymbol { symbol, .. } => {
                assert_eq!(symbol, "itdb_parse");
            }
            other => panic!("expected MissingSymbol, got {other}"),
        }
    }

    /// W-1: a garbage file fails at dlopen, mapped to `Load`.
    #[test]
    fn garbage_library_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SONAME), b"not an elf").unwrap();
        let err =
            IpodBackend::load_with(BackendSearch::Explicit(dir.path().to_path_buf())).unwrap_err();
        assert!(matches!(err, BackendUnavailable::Load { .. }), "{err}");
    }

    /// A fake library path inside a tempdir — `check_marker` only needs the
    /// path to locate the marker next to it, so the ABI policy is testable
    /// without any `.so`.
    fn fake_library_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(SONAME)
    }

    /// W-6: the marker decision table — matching marker accepts (and reports
    /// the ABI as verified), missing marker accepts with the soft-policy path
    /// (ABI unverified), any present-but-wrong marker is a hard `AbiMismatch`.
    #[test]
    fn marker_matching_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MARKER_FILE_NAME),
            format!("VERSION={MARKER_VERSION}\nLAYOUTS={LAYOUTS_SHA256}\n"),
        )
        .unwrap();
        assert!(
            check_marker(&fake_library_path(&dir)).expect("matching marker must be accepted"),
            "a matching marker means the ABI is verified"
        );
    }

    #[test]
    fn marker_absent_is_soft_accepted() {
        let dir = tempfile::tempdir().unwrap();
        // No marker file at all: the soft policy trusts the SONAME, and the
        // result must say the ABI was NOT verified (feeds `abi_verified`).
        assert!(
            !check_marker(&fake_library_path(&dir))
                .expect("missing marker is the soft-policy path"),
            "a missing marker must report the ABI as unverified"
        );
    }

    #[test]
    fn marker_version_mismatch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MARKER_FILE_NAME),
            format!("VERSION=0.8.2\nLAYOUTS={LAYOUTS_SHA256}\n"),
        )
        .unwrap();
        let err = check_marker(&fake_library_path(&dir)).unwrap_err();
        assert!(matches!(err, BackendUnavailable::AbiMismatch(_)), "{err}");
    }

    #[test]
    fn marker_layouts_mismatch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MARKER_FILE_NAME),
            format!("VERSION={MARKER_VERSION}\nLAYOUTS=sha256:deadbeef\n"),
        )
        .unwrap();
        let err = check_marker(&fake_library_path(&dir)).unwrap_err();
        assert!(matches!(err, BackendUnavailable::AbiMismatch(_)), "{err}");
    }

    #[test]
    fn marker_unreadable_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // A directory with the marker name: exists but unreadable as a text
        // file — must be a hard refusal, never silently accepted.
        std::fs::create_dir(dir.path().join(MARKER_FILE_NAME)).unwrap();
        let err = check_marker(&fake_library_path(&dir)).unwrap_err();
        assert!(matches!(err, BackendUnavailable::AbiMismatch(_)), "{err}");
    }
}
