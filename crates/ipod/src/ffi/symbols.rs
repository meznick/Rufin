//! Typed `unsafe extern "C"` declarations for the libgpod 0.8.3 symbol set
//! the wrapper uses, and the resolution of those symbols into owned tables.
//!
//! Every signature here was copied from the pinned public header
//! (`gpod-1.0/gpod/itdb.h`, the same file [`super::layouts`] was generated
//! from). A wrong signature here is memory-unsafe at the ABI level, so each
//! declaration carries the verbatim C prototype in its doc comment. When the
//! pin moves, regenerate layouts *and* re-check these prototypes.
//!
//! The split into read/write tables implements W-1/W-6: read symbols are
//! required at load time (their absence is `BackendUnavailable`), write
//! symbols are resolved eagerly but their absence only fails the gated write
//! calls — a minimal read-only build of libgpod stays usable for reading.

use std::ffi::{c_char, c_int, c_uchar, c_void};
use std::os::raw::{c_uint, c_ulong};

use libloading::Library;

use super::layouts::{
    GError, Itdb_Artwork, Itdb_Device, Itdb_IpodGeneration, Itdb_IpodInfo, Itdb_IpodModel,
    Itdb_Playlist, Itdb_Track, Itdb_iTunesDB,
};
use crate::types::BackendUnavailable;

pub type GBoolean = c_int;

// ---------------------------------------------------------------------------
// Read-side declarations (required at load; absence = BackendUnavailable)
// ---------------------------------------------------------------------------

/// `Itdb_iTunesDB *itdb_parse (const gchar *mp, GError **error);`
///
/// Parses `iTunesDB` + `ArtworkDB` under the mount point. **Side effects are
/// possible on foreign trees** (`itdb_hash72_extract_hash_info` can create
/// `Device/HashInfo`; a missing `Artwork/` is `g_mkdir`-ed) — see the crate
/// root docs and `deltas/b007_delta.md` §1.1. Callers run this only on
/// verified candidates or disposable snapshots.
pub type FnItdbParse =
    unsafe extern "C" fn(mountpoint: *const c_char, error: *mut *mut GError) -> *mut Itdb_iTunesDB;

/// `void itdb_free (Itdb_iTunesDB *itdb);`
///
/// Frees the whole database tree: tracks, playlists, artwork, and all string
/// fields. The wrapper never frees individual objects — double-free hazard
/// documented on [`crate::DeviceSession`].
pub type FnItdbFree = unsafe extern "C" fn(db: *mut Itdb_iTunesDB);

/// `const gchar *itdb_get_mountpoint (Itdb_iTunesDB *itdb);`
pub type FnItdbGetMountpoint = unsafe extern "C" fn(db: *mut Itdb_iTunesDB) -> *const c_char;

/// `gchar *itdb_get_artworkdb_path (const gchar *mountpoint);`
///
/// Result is GLib-allocated (`g_free`). Takes the mount point, not the db.
pub type FnItdbGetArtworkdbPath = unsafe extern "C" fn(mountpoint: *const c_char) -> *mut c_char;

/// `const Itdb_IpodInfo *itdb_device_get_ipod_info (const Itdb_Device *device);`
///
/// Returns null when the device model cannot be resolved — the empty-`SysInfo`
/// case. `None` here is the W-3 "model unknown" signal and must surface as
/// such, never as a default model.
pub type FnItdbDeviceGetIpodInfo =
    unsafe extern "C" fn(device: *const Itdb_Device) -> *const Itdb_IpodInfo;

/// `gboolean itdb_device_supports_artwork (const Itdb_Device *device);`
///
/// The model gate for artwork parsing (§1.2): false on an unknown model makes
/// `itdb_parse` skip artwork silently.
pub type FnItdbDeviceSupportsArtwork = unsafe extern "C" fn(device: *const Itdb_Device) -> GBoolean;

/// `gchar *itdb_device_get_sysinfo (const Itdb_Device *device, const gchar *field);`
///
/// Result is GLib-allocated (`g_free`); null when the field is absent.
pub type FnItdbDeviceGetSysinfo =
    unsafe extern "C" fn(device: *const Itdb_Device, field: *const c_char) -> *mut c_char;

/// `gchar *itdb_device_get_uuid (const Itdb_Device *device);`
///
/// Result is GLib-allocated (`g_free`). See the crate root docs for the
/// `db->id` byte-order gotcha.
pub type FnItdbDeviceGetUuid = unsafe extern "C" fn(device: *const Itdb_Device) -> *mut c_char;

/// `const gchar *itdb_info_get_ipod_model_name_string (Itdb_IpodModel model);`
///
/// Static string, no free.
pub type FnItdbInfoModelNameString = unsafe extern "C" fn(model: Itdb_IpodModel) -> *const c_char;

/// `const gchar *itdb_info_get_ipod_generation_string (Itdb_IpodGeneration generation);`
///
/// Static string, no free.
pub type FnItdbInfoGenerationString =
    unsafe extern "C" fn(generation: Itdb_IpodGeneration) -> *const c_char;

/// `gboolean itdb_playlist_is_mpl (Itdb_Playlist *pl);`
///
/// The master-playlist check; the `type` byte constants are private to
/// libgpod (itdb_private.h), so the wrapper uses this instead of decoding
/// the byte itself.
pub type FnItdbPlaylistIsMpl = unsafe extern "C" fn(playlist: *mut Itdb_Playlist) -> GBoolean;

/// The required read-side symbol table: plain function pointers resolved at
/// load time. The pointers live as long as the `Library` held by
/// [`crate::IpodBackend`] (see [`super::resolve_fn`]); the table itself is
/// `Copy`-free plain data.
pub struct ReadSymbols {
    pub itdb_parse: FnItdbParse,
    pub itdb_free: FnItdbFree,
    pub itdb_get_mountpoint: FnItdbGetMountpoint,
    pub itdb_get_artworkdb_path: FnItdbGetArtworkdbPath,
    pub itdb_device_get_ipod_info: FnItdbDeviceGetIpodInfo,
    pub itdb_device_supports_artwork: FnItdbDeviceSupportsArtwork,
    pub itdb_device_get_sysinfo: FnItdbDeviceGetSysinfo,
    pub itdb_device_get_uuid: FnItdbDeviceGetUuid,
    pub itdb_info_model_name_string: FnItdbInfoModelNameString,
    pub itdb_info_generation_string: FnItdbInfoGenerationString,
    pub itdb_playlist_is_mpl: FnItdbPlaylistIsMpl,
}

/// Resolves every read-side symbol; the first missing one fails the load
/// (W-1: a broken/partial library is reported, never half-used).
pub fn resolve_read(library: &Library) -> Result<ReadSymbols, BackendUnavailable> {
    Ok(ReadSymbols {
        itdb_parse: super::resolve_fn(library, "itdb_parse")?,
        itdb_free: super::resolve_fn(library, "itdb_free")?,
        itdb_get_mountpoint: super::resolve_fn(library, "itdb_get_mountpoint")?,
        itdb_get_artworkdb_path: super::resolve_fn(library, "itdb_get_artworkdb_path")?,
        itdb_device_get_ipod_info: super::resolve_fn(library, "itdb_device_get_ipod_info")?,
        itdb_device_supports_artwork: super::resolve_fn(library, "itdb_device_supports_artwork")?,
        itdb_device_get_sysinfo: super::resolve_fn(library, "itdb_device_get_sysinfo")?,
        itdb_device_get_uuid: super::resolve_fn(library, "itdb_device_get_uuid")?,
        itdb_info_model_name_string: super::resolve_fn(
            library,
            "itdb_info_get_ipod_model_name_string",
        )?,
        itdb_info_generation_string: super::resolve_fn(
            library,
            "itdb_info_get_ipod_generation_string",
        )?,
        itdb_playlist_is_mpl: super::resolve_fn(library, "itdb_playlist_is_mpl")?,
    })
}

// ---------------------------------------------------------------------------
// Write-side declarations (behind the `sync-writes` feature gate; absence
// only fails write calls, never the load)
// ---------------------------------------------------------------------------

/// `gboolean itdb_init_ipod (const gchar *mountpoint, const gchar *model_number,
///                          const gchar *ipod_name, GError **error);`
///
/// Initializes an empty device tree. Used by the fixture-builder testkit and
/// defined as a write API of the session (W-7); never called on read paths.
pub type FnItdbInitIpod = unsafe extern "C" fn(
    mountpoint: *const c_char,
    model_number: *const c_char,
    ipod_name: *const c_char,
    error: *mut *mut GError,
) -> GBoolean;

/// `Itdb_Track *itdb_track_new (void);`
pub type FnItdbTrackNew = unsafe extern "C" fn() -> *mut Itdb_Track;

/// `void itdb_track_free (Itdb_Track *track);`
///
/// Only for tracks that were **never added** to a database (a detached track
/// created by `itdb_track_new`); added tracks are freed by `itdb_free`.
pub type FnItdbTrackFree = unsafe extern "C" fn(track: *mut Itdb_Track);

/// `void itdb_track_add (Itdb_iTunesDB *itdb, Itdb_Track *track, gint32 pos);`
///
/// After this call the database owns the track (freed by `itdb_free`).
pub type FnItdbTrackAdd =
    unsafe extern "C" fn(db: *mut Itdb_iTunesDB, track: *mut Itdb_Track, pos: c_int);

/// `void itdb_track_unlink (Itdb_Track *track);`
///
/// Removes the track from its database (the caller then owns it and must
/// `itdb_track_free` it). NOT the same as removing the file from the pod.
pub type FnItdbTrackUnlink = unsafe extern "C" fn(track: *mut Itdb_Track);

/// `gboolean itdb_cp_track_to_ipod (Itdb_Track *track, const gchar *filename,
///                                 GError **error);`
///
/// Copies the source file onto the pod and sets the track's iPod-side path.
/// **This is a device write** — see the §1.1 side-effect notes.
pub type FnItdbCpTrackToIpod = unsafe extern "C" fn(
    track: *mut Itdb_Track,
    filename: *const c_char,
    error: *mut *mut GError,
) -> GBoolean;

/// `void itdb_playlist_add_track (Itdb_Playlist *pl, Itdb_Track *track, gint32 pos);`
pub type FnItdbPlaylistAddTrack =
    unsafe extern "C" fn(playlist: *mut Itdb_Playlist, track: *mut Itdb_Track, pos: c_int);

/// `Itdb_Artwork *itdb_artwork_new (void);`
///
/// Allocates a detached artwork block; the caller assigns it to a track
/// (`track->artwork`). The track then owns it (`itdb_free` releases it).
pub type FnItdbArtworkNew = unsafe extern "C" fn() -> *mut Itdb_Artwork;

/// `gboolean itdb_artwork_set_thumbnail_from_data (Itdb_Artwork *artwork,
///     const guchar *image_data, gsize image_data_len, gint rotation,
///     GError **error);`
///
/// Takes **encoded** image bytes (PNG/JPEG); libgpod decodes them internally
/// and writes the ithmb banks on `itdb_write`. Used by the testkit to
/// exercise the ArtworkDB/ithmb writers end-to-end.
pub type FnItdbArtworkSetThumbnailFromData = unsafe extern "C" fn(
    artwork: *mut Itdb_Artwork,
    image_data: *const c_uchar,
    image_data_len: c_ulong,
    rotation: c_int,
    error: *mut *mut GError,
) -> GBoolean;

/// `gboolean itdb_write (Itdb_iTunesDB *itdb, GError **error);`
///
/// Writes `iTunesDB` + `ArtworkDB` (and the ithmb banks) back to the device.
/// The B-010 gate owns when this may run against a real pod.
pub type FnItdbWrite =
    unsafe extern "C" fn(db: *mut Itdb_iTunesDB, error: *mut *mut GError) -> GBoolean;

/// The write-side symbol table. `None` when any write symbol is missing —
/// in that case reads keep working and every write call fails with
/// [`OperationFailure::BackendSymbolMissing`](crate::write::OperationFailure).
pub struct WriteSymbols {
    pub itdb_init_ipod: FnItdbInitIpod,
    pub itdb_track_new: FnItdbTrackNew,
    pub itdb_track_free: FnItdbTrackFree,
    pub itdb_track_add: FnItdbTrackAdd,
    pub itdb_track_unlink: FnItdbTrackUnlink,
    pub itdb_cp_track_to_ipod: FnItdbCpTrackToIpod,
    pub itdb_playlist_add_track: FnItdbPlaylistAddTrack,
    pub itdb_artwork_new: FnItdbArtworkNew,
    pub itdb_artwork_set_thumbnail_from_data: FnItdbArtworkSetThumbnailFromData,
    pub itdb_write: FnItdbWrite,
}

/// Resolves the write set eagerly so a broken write surface is known at load
/// time, but tolerates its absence (W-1: read availability is the load gate).
pub fn resolve_write(library: &Library) -> Option<WriteSymbols> {
    let itdb_init_ipod = super::resolve_fn(library, "itdb_init_ipod").ok()?;
    let itdb_track_new = super::resolve_fn(library, "itdb_track_new").ok()?;
    let itdb_track_free = super::resolve_fn(library, "itdb_track_free").ok()?;
    let itdb_track_add = super::resolve_fn(library, "itdb_track_add").ok()?;
    let itdb_track_unlink = super::resolve_fn(library, "itdb_track_unlink").ok()?;
    let itdb_cp_track_to_ipod = super::resolve_fn(library, "itdb_cp_track_to_ipod").ok()?;
    let itdb_playlist_add_track = super::resolve_fn(library, "itdb_playlist_add_track").ok()?;
    let itdb_artwork_new = super::resolve_fn(library, "itdb_artwork_new").ok()?;
    let itdb_artwork_set_thumbnail_from_data =
        super::resolve_fn(library, "itdb_artwork_set_thumbnail_from_data").ok()?;
    let itdb_write = super::resolve_fn(library, "itdb_write").ok()?;
    Some(WriteSymbols {
        itdb_init_ipod,
        itdb_track_new,
        itdb_track_free,
        itdb_track_add,
        itdb_track_unlink,
        itdb_cp_track_to_ipod,
        itdb_playlist_add_track,
        itdb_artwork_new,
        itdb_artwork_set_thumbnail_from_data,
        itdb_write,
    })
}

// ---------------------------------------------------------------------------
// GLib process-wide helpers, resolved through libgpod's own dependency tree
// ---------------------------------------------------------------------------

/// `void g_free (gpointer mem);`
type FnGFree = unsafe extern "C" fn(mem: *mut c_void);

/// `void g_error_free (GError *error);`
type FnGErrorFree = unsafe extern "C" fn(error: *mut GError);

/// `gchar *g_strdup (const gchar *str);`
type FnGStrdup = unsafe extern "C" fn(str: *const c_char) -> *mut c_char;

/// `const gchar *g_quark_to_string (GQuark quark);`
type FnGQuarkToString = unsafe extern "C" fn(quark: c_uint) -> *const c_char;

/// GLib allocation/error helpers, resolved through the libgpod handle.
///
/// glibc `dlsym` searches the handle's dependency tree and libglib is a
/// `DT_NEEDED` of libgpod, so these resolve without linking glib. Each field
/// is optional: when `g_free`/`g_error_free` are missing the wrapper degrades
/// to leaking that allocation (with a one-time warning) instead of risking a
/// wrong free; when `g_strdup` is missing, string-owning writes fail cleanly.
pub struct GlibHelpers {
    pub g_free: Option<FnGFree>,
    pub g_error_free: Option<FnGErrorFree>,
    pub g_strdup: Option<FnGStrdup>,
    pub g_quark_to_string: Option<FnGQuarkToString>,
    warned_missing: std::sync::atomic::AtomicBool,
}

impl GlibHelpers {
    /// Resolve all four helpers through `library` (the libgpod handle).
    pub fn resolve(library: &Library) -> Self {
        Self {
            g_free: super::resolve_fn(library, "g_free").ok(),
            g_error_free: super::resolve_fn(library, "g_error_free").ok(),
            g_strdup: super::resolve_fn(library, "g_strdup").ok(),
            g_quark_to_string: super::resolve_fn(library, "g_quark_to_string").ok(),
            warned_missing: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// # Safety
    /// `mem` must be a pointer allocated by GLib (`g_malloc`/`g_strdup`).
    pub unsafe fn g_free(&self, mem: *mut c_void) {
        match &self.g_free {
            Some(f) => unsafe { f(mem) },
            None => self.warn_missing("g_free"),
        }
    }

    /// # Safety
    /// `error` must be a heap `GError` produced by a C call, not freed yet.
    pub unsafe fn g_error_free(&self, error: *mut GError) {
        match &self.g_error_free {
            Some(f) => unsafe { f(error) },
            None => self.warn_missing("g_error_free"),
        }
    }

    /// Duplicate a C string into GLib-owned memory (to be released with
    /// [`GlibHelpers::g_free`] by the owner — the database in the write
    /// path's case). Returns `None` when the helper is unavailable (the
    /// caller fails the write cleanly) or on allocation failure (null).
    ///
    /// # Safety
    /// `src` must be a valid NUL-terminated string or null.
    pub unsafe fn g_strdup(&self, src: *const c_char) -> Option<*mut c_char> {
        match &self.g_strdup {
            Some(f) => {
                let dup = unsafe { f(src) };
                (!dup.is_null()).then_some(dup)
            }
            None => {
                self.warn_missing("g_strdup");
                None
            }
        }
    }

    /// Returns the name of `quark`, or `Some("<quark N>")` when
    /// `g_quark_to_string` is unavailable or the quark has no name.
    pub fn quark_name(&self, quark: c_uint) -> String {
        match &self.g_quark_to_string {
            Some(f) => {
                // SAFETY: g_quark_to_string returns a static NUL-terminated
                // string for any valid GQuark (or null for an unnamed quark).
                match unsafe { super::cstr_copy(f(quark)) } {
                    Some(name) => name,
                    None => format!("<quark {quark}>"),
                }
            }
            None => {
                self.warn_missing("g_quark_to_string");
                format!("<quark {quark}>")
            }
        }
    }

    fn warn_missing(&self, what: &str) {
        if !self
            .warned_missing
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                %what,
                "GLib helper unavailable in the loaded libgpod; affected calls degrade (leak or fail)"
            );
        }
    }
}
