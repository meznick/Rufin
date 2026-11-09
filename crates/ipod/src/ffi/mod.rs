//! The native side: typed extern symbol declarations, GLib/GLib-list access,
//! and the small set of owned-conversion helpers every unsafe path funnels
//! through.
//!
//! # Layout of this module
//!
//! - [`layouts`] — bindgen-generated struct mirrors for the pinned 0.8.3
//!   build (see that file's header for provenance and the SHA-256 tie-in).
//! - [`symbols`] — `unsafe extern "C"` function *types* and the resolution
//!   into [`symbols::ReadSymbols`] / [`symbols::WriteSymbols`] tables; the
//!   tables own the `libloading::Symbol`s.
//! - This file — the raw-pointer helpers: `GList` iteration, C string
//!   conversion (copy vs GLib-owned), and `GError` extraction.
//!
//! # Memory ownership rules (the reason all unsafe funnels through here)
//!
//! | Source | Ownership | What the wrapper does |
//! |---|---|---|
//! | struct fields (`track->title`, `playlist->name`, ...) | owned by the database, freed by `itdb_free` | copy to `String`, never free |
//! | `itdb_device_get_sysinfo`, `itdb_get_artworkdb_path` | newly allocated, freed with `g_free` | copy, then `g_free` |
//! | `itdb_device_get_uuid` | **borrowed** into the device's sysinfo hash | copy to `String`, never free |
//! | `GError` returned via `GError**` | freed with `g_error_free` | copy domain/code/message, then `g_error_free` |
//! | `g_quark_to_string` result | static, no free | copy |
//!
//! `itdb_device_get_uuid` deserves its own note: despite the non-const
//! `gchar *` return type, the implementation is
//! `g_hash_table_lookup (device->sysinfo, "FirewireGuid")` — a pointer into
//! the hash, not a copy (itdb_device.c, verified against the pinned build).
//! Freeing it would corrupt the hash (use-after-free, then a double free at
//! `itdb_free`) — which is exactly what a seeded `SysInfoExtended` fixture
//! caught in the B-005 wrapper work. The wrapper copies and never frees.
//!
//! `g_free`/`g_error_free`/`g_quark_to_string` are resolved through the
//! loaded libgpod handle (glibc `dlsym` searches the dependency tree, and
//! libglib is a `DT_NEEDED` of libgpod). If a helper is not found the
//! wrapper degrades to leaking that allocation with a `tracing::warn` — a
//! missing helper is never turned into a double free or a `free()` on glib
//! memory.

pub mod layouts;
pub mod layouts_sha;
pub mod symbols;

use std::ffi::{CStr, c_char};

pub use layouts::{
    _GError as GError, _GList as GList, GQuark, Itdb_Artwork, Itdb_IpodGeneration, Itdb_IpodInfo,
    Itdb_IpodModel, Itdb_Playlist, Itdb_SPLPref, Itdb_SPLRule, Itdb_SPLRules, Itdb_Track,
    Itdb_iTunesDB,
};
use symbols::GlibHelpers;

/// Copy a NUL-terminated C string into an owned `String`. `None` for a null
/// pointer. Non-UTF-8 content is replaced lossily (U+FFFD), matching the
/// crate-wide policy of never failing a read because of one invalid string.
///
/// # Safety
///
/// `ptr` must be a valid NUL-terminated string or null, and the pointee must
/// stay valid for the duration of the call. Does not free anything.
pub unsafe fn cstr_copy(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: precondition — `ptr` is a valid NUL-terminated string.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Copy a GLib-owned string and free it with `g_free` (see the ownership
/// table). Returns the copy; the native allocation is released either way
/// (unless `g_free` itself is unavailable, in which case it leaks with a
/// warning).
///
/// # Safety
///
/// `ptr` must be a valid NUL-terminated string allocated by GLib, or null.
pub unsafe fn gstr_take(ptr: *mut c_char, glib: &GlibHelpers) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let copy = unsafe { cstr_copy(ptr) };
    // SAFETY: precondition — `ptr` is GLib-allocated and we are handing it
    // back to GLib exactly once.
    unsafe { glib.g_free(ptr.cast()) };
    copy
}

/// Extract the domain/code/message of a `GError` produced by a libgpod call
/// and free the error itself with `g_error_free`.
///
/// Returns `None` when `*error` was left null (the success case). Never
/// returns `None` for a set error: the message may be absent or non-UTF-8,
/// but the domain and code are always returned; the wrapper reports the
/// structure either way.
///
/// # Safety
///
/// `error` must be a valid out-parameter of type `GError **` that the C call
/// may have populated. The pointee, when non-null, is a heap-allocated
/// `GError` that must be freed exactly once — this function does the freeing.
pub unsafe fn take_gerror(
    error: *mut *mut GError,
    glib: &GlibHelpers,
) -> Option<(String, i32, String)> {
    // SAFETY: precondition — `error` is a valid `GError **` out-param.
    let err = unsafe { *error };
    if err.is_null() {
        return None;
    }
    // SAFETY: `err` is a valid `GError` produced by the C call. Its `domain`
    // and `code` are plain integers; `message` follows the same rules as
    // `cstr_copy`.
    let (domain, code, message) = unsafe {
        let domain = glib.quark_name((*err).domain);
        let code = (*err).code;
        let message =
            cstr_copy((*err).message).unwrap_or_else(|| "<non-UTF-8 message>".to_string());
        (domain, code, message)
    };
    // SAFETY: precondition — `err` is the heap `GError` this function is
    // responsible for freeing exactly once.
    unsafe { glib.g_error_free(err) };
    Some((domain, code, message))
}

/// Iterator over a native `GList`, yielding the data pointers in list order.
///
/// The list itself is owned by the database; this iterator borrows it and
/// never mutates or frees anything. `T` is the pointee type of `data`.
pub struct GListIter<T> {
    current: *const GList,
    _marker: std::marker::PhantomData<*const T>,
}

impl<T> GListIter<T> {
    /// # Safety
    ///
    /// `list` must be null or a valid `GList` whose `data` pointers are
    /// valid `T`s for the iterator's lifetime. Null `data` pointers are NOT
    /// supported — every caller dereferences the yielded pointer.
    pub unsafe fn new(list: *const GList) -> Self {
        Self {
            current: list,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T> Iterator for GListIter<T> {
    type Item = *const T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }
        // SAFETY: invariant — `current` is always null or a valid `GList`
        // node belonging to the borrowed list.
        let data = unsafe { (*self.current).data as *const T };
        // SAFETY: same invariant; `next` is the following node or null.
        self.current = unsafe { (*self.current).next };
        Some(data)
    }
}

/// Resolve a raw function pointer from a loaded library, converting the
/// loader error into the crate's `BackendUnavailable::MissingSymbol`.
///
/// The returned pointer is a plain `Copy` value with no borrow of the
/// library: `libloading::Symbol` is a thin handle with a trivial drop, so
/// copying the pointed-to value out and letting the handle go is sufficient
/// — the symbol table entry lives exactly as long as the `Library`, which
/// the backend owns. This avoids the classic self-referential struct problem
/// without leaking the library.
pub fn resolve_fn<T: Copy>(
    library: &libloading::Library,
    name: &'static str,
) -> Result<T, crate::types::BackendUnavailable> {
    // SAFETY: the caller passes a function type that matches the C symbol's
    // actual signature; `libloading` validates only that the symbol exists.
    // `T: Copy` (all callers use fn-pointer types), so `*sym` copies the
    // pointer value out and the handle's drop is trivial — the symbol table
    // entry lives exactly as long as the `Library`, which the backend owns.
    let sym: libloading::Symbol<'_, T> =
        unsafe { library.get::<T>(name.as_bytes()) }.map_err(|e| {
            tracing::debug!(%name, loader_error = %e, "required libgpod symbol is missing");
            crate::types::BackendUnavailable::MissingSymbol {
                library: "libgpod.so.4".to_string(),
                symbol: name,
            }
        })?;
    Ok(*sym)
}
