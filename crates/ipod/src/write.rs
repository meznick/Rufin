//! Gated write APIs (W-7).
//!
//! The write surface is **defined but not enabled**: every method here first
//! passes [`gate`], which refuses unless the crate was compiled with the
//! `sync-writes` cargo feature (a not-for-production flag — see the crate
//! root docs). No sync path may enable that feature before B-010's safety
//! verification, and B-007's verification criteria require exactly that
//! (writes are not reachable from any enabled sync path).
//!
//! Even with the feature on, these are thin, faithful passes to libgpod:
//! the semantic mapping (Rufin catalog ↔ device models), delta computation,
//! backups, and recovery stay in later tasks (B-008..B-010). The fixture
//! builder ([`crate::testkit`]) uses the same surface to produce disposable
//! devices for the tests — the methodology of the §1.1 experiments, now
//! inside the test suite.

use std::ffi::{CString, c_char};
use std::path::Path;

use crate::DeviceSession;
use crate::ffi::symbols::WriteSymbols;
use crate::ffi::{GListIter, Itdb_Playlist, Itdb_Track, take_gerror};
use crate::types::{ArtworkInput, PlaylistChanges, TrackMetadata};
use crate::with_native;

/// Whether the write surface is compiled in (`sync-writes` feature).
pub const SYNC_WRITES_ENABLED: bool = cfg!(feature = "sync-writes");

/// Why a gated write operation failed.
#[derive(Debug, thiserror::Error)]
pub enum OperationFailure {
    /// The `sync-writes` feature is off: writes are not reachable from any
    /// enabled sync path until B-010 (W-7).
    #[error("write APIs are gated until B-010 (the sync-writes feature is off)")]
    WritesGated,
    /// The loaded library lacks a write symbol (a read-only build).
    #[error("loaded libgpod lacks write symbol {symbol}")]
    BackendSymbolMissing { symbol: &'static str },
    /// libgpod reported a GLib error.
    #[error("{domain} {code}: {message}")]
    GError {
        domain: String,
        code: i32,
        message: String,
    },
    /// A referenced entity (track, playlist, source file) is missing.
    #[error("{0}")]
    NotFound(String),
    /// State misuse or a helper unavailability that is not a library error.
    #[error("{0}")]
    InvalidState(String),
}

/// The W-7 gate. Pure so the unit tests can pin the without-feature refusal
/// without a backend.
pub(crate) fn gate() -> Result<(), OperationFailure> {
    if SYNC_WRITES_ENABLED {
        Ok(())
    } else {
        Err(OperationFailure::WritesGated)
    }
}

impl DeviceSession<'_> {
    /// Create a track from metadata and add it to the database (W-7, gated).
    ///
    /// Returns the device dbid (assigned by `itdb_track_add`). The track is
    /// *not* written to the device until [`DeviceSession::write_library`],
    /// and *not* copied onto the pod until [`DeviceSession::copy_track`].
    pub fn create_track(&mut self, metadata: &TrackMetadata) -> Result<u64, OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.create_track_inner(metadata) })
    }

    unsafe fn create_track_inner(
        &mut self,
        metadata: &TrackMetadata,
    ) -> Result<u64, OperationFailure> {
        let write = write_symbols(self.backend_ref())?;
        // SAFETY: allocates a detached track; owned by us until
        // `itdb_track_add` transfers ownership to the database.
        let track = unsafe { (write.itdb_track_new)() };
        if track.is_null() {
            return Err(OperationFailure::InvalidState(
                "itdb_track_new returned NULL".to_string(),
            ));
        }
        // If any metadata application fails, the detached track must be
        // freed by us (the database does not own it yet).
        if let Err(e) = unsafe { apply_metadata(track, metadata, self.backend_ref()) } {
            unsafe { (write.itdb_track_free)(track) };
            return Err(e);
        }
        // SAFETY: appends to the database track list; ownership transfers to
        // the db (freed by `itdb_free`). `itdb_track_add` assigns a unique
        // dbid when the track's is zero.
        unsafe { (write.itdb_track_add)(self.db_ptr(), track, -1) };
        // SAFETY: `dbid` is plain data, valid after the add.
        Ok(unsafe { (*track).dbid })
    }

    /// Copy the source file onto the pod for an existing track (W-7, gated).
    ///
    /// This is a **device write** with the §1.1 side effects; it must never
    /// run against a live pod outside the B-010-gated flow.
    pub fn copy_track(&mut self, dbid: u64, source: &Path) -> Result<(), OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.copy_track_inner(dbid, source) })
    }

    unsafe fn copy_track_inner(
        &mut self,
        dbid: u64,
        source: &Path,
    ) -> Result<(), OperationFailure> {
        let write = write_symbols(self.backend_ref())?;
        let track = unsafe { find_track(self.db_ptr(), dbid) }
            .ok_or_else(|| OperationFailure::NotFound(format!("track {dbid} not in database")))?;
        let filename = CString::new(source.as_os_str().as_encoded_bytes()).map_err(|_| {
            OperationFailure::NotFound("source path contains a NUL byte".to_string())
        })?;
        let mut error = std::ptr::null_mut();
        // SAFETY: `track` valid for the session, `filename` valid for the
        // call, `error` a valid GError** out-param. Returns gboolean.
        let ok = unsafe { (write.itdb_cp_track_to_ipod)(track, filename.as_ptr(), &mut error) };
        if ok == 0 {
            return Err(
                match unsafe { take_gerror(&mut error, self.backend_ref().glib()) } {
                    Some((domain, code, message)) => OperationFailure::GError {
                        domain,
                        code,
                        message,
                    },
                    None => OperationFailure::InvalidState(
                        "itdb_cp_track_to_ipod failed without a GError".to_string(),
                    ),
                },
            );
        }
        Ok(())
    }

    /// Replace the writable metadata fields of an existing track
    /// (W-7, gated). `None` fields stay untouched.
    pub fn update_track(
        &mut self,
        dbid: u64,
        metadata: &TrackMetadata,
    ) -> Result<(), OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.update_track_inner(dbid, metadata) })
    }

    unsafe fn update_track_inner(
        &mut self,
        dbid: u64,
        metadata: &TrackMetadata,
    ) -> Result<(), OperationFailure> {
        let track = unsafe { find_track(self.db_ptr(), dbid) }
            .ok_or_else(|| OperationFailure::NotFound(format!("track {dbid} not in database")))?;
        unsafe { apply_metadata(track, metadata, self.backend_ref()) }
    }

    /// Remove a track from the database (W-7, gated).
    ///
    /// Detaches the track from **every** playlist first (including the master
    /// playlist), then unlinks it from the track list and frees it. The
    /// detach step is not optional: upstream `itdb_track_unlink` deliberately
    /// leaves playlist memberships alone, and a freed track still referenced
    /// by `pl->members` is a dangling pointer that the next `itdb_write`
    /// dereferences. The media file on the pod is deliberately **not**
    /// deleted — destructive removal and its recovery design are B-010's.
    pub fn remove_track(&mut self, dbid: u64) -> Result<(), OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.remove_track_inner(dbid) })
    }

    unsafe fn remove_track_inner(&mut self, dbid: u64) -> Result<(), OperationFailure> {
        let write = write_symbols(self.backend_ref())?;
        let track = unsafe { find_track(self.db_ptr(), dbid) }
            .ok_or_else(|| OperationFailure::NotFound(format!("track {dbid} not in database")))?;
        // SAFETY: the playlist list is database-owned and valid for the
        // session; removing members mutates each playlist's member list, not
        // the playlist list itself, so the iteration is safe. A playlist the
        // track is not in is a documented no-op upstream.
        for raw in unsafe { GListIter::<Itdb_Playlist>::new((*self.db_ptr()).playlists) } {
            unsafe { (write.itdb_playlist_remove_track)(raw.cast_mut(), track) };
        }
        // SAFETY: `itdb_track_unlink` removes the track from its database,
        // transferring ownership back to the caller; we free it immediately.
        // No playlist references remain after the detach loop above.
        unsafe { (write.itdb_track_unlink)(track) };
        unsafe { (write.itdb_track_free)(track) };
        Ok(())
    }

    /// Append playlist memberships (W-7, gated). Unknown playlist ids or
    /// track ids are reported, not skipped.
    pub fn update_playlists(&mut self, changes: &PlaylistChanges) -> Result<(), OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.update_playlists_inner(changes) })
    }

    unsafe fn update_playlists_inner(
        &mut self,
        changes: &PlaylistChanges,
    ) -> Result<(), OperationFailure> {
        let write = write_symbols(self.backend_ref())?;
        for (playlist_id, track_ids) in &changes.additions {
            let playlist =
                unsafe { find_playlist(self.db_ptr(), *playlist_id) }.ok_or_else(|| {
                    OperationFailure::NotFound(format!("playlist {playlist_id} not in database"))
                })?;
            for track_id in track_ids {
                let track = unsafe { find_track(self.db_ptr(), *track_id) }.ok_or_else(|| {
                    OperationFailure::NotFound(format!("track {track_id} not in database"))
                })?;
                // SAFETY: both pointers valid for the session; position -1
                // appends.
                unsafe { (write.itdb_playlist_add_track)(playlist, track, -1) };
            }
        }
        Ok(())
    }

    /// Attach artwork to a track (W-7, gated). `encoded` must be PNG/JPEG;
    /// libgpod generates the model formats on `write_library`. A track
    /// without an artwork block gets one allocated first (`itdb_artwork_new`),
    /// matching libgpod's own usage.
    pub fn set_artwork(&mut self, dbid: u64, input: &ArtworkInput) -> Result<(), OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.set_artwork_inner(dbid, input) })
    }

    unsafe fn set_artwork_inner(
        &mut self,
        dbid: u64,
        input: &ArtworkInput,
    ) -> Result<(), OperationFailure> {
        let write = write_symbols(self.backend_ref())?;
        let track = unsafe { find_track(self.db_ptr(), dbid) }
            .ok_or_else(|| OperationFailure::NotFound(format!("track {dbid} not in database")))?;
        // SAFETY: track valid for the session; `artwork` is a plain pointer
        // field and the track owns whatever we assign to it.
        let artwork = unsafe { (*track).artwork };
        let artwork = if artwork.is_null() {
            // SAFETY: allocates a detached artwork block; ownership passes to
            // the track (released by `itdb_free`).
            let fresh = unsafe { (write.itdb_artwork_new)() };
            if fresh.is_null() {
                return Err(OperationFailure::InvalidState(
                    "itdb_artwork_new returned NULL".to_string(),
                ));
            }
            unsafe { (*track).artwork = fresh };
            fresh
        } else {
            artwork
        };
        let mut error = std::ptr::null_mut();
        // SAFETY: artwork valid, image data valid for the call, error is a
        // valid out-param.
        let ok = unsafe {
            (write.itdb_artwork_set_thumbnail_from_data)(
                artwork,
                input.encoded.as_ptr(),
                input.encoded.len() as std::os::raw::c_ulong,
                input.rotation,
                &mut error,
            )
        };
        if ok == 0 {
            return Err(
                match unsafe { take_gerror(&mut error, self.backend_ref().glib()) } {
                    Some((domain, code, message)) => OperationFailure::GError {
                        domain,
                        code,
                        message,
                    },
                    None => OperationFailure::InvalidState(
                        "itdb_artwork_set_thumbnail_from_data failed without a GError".to_string(),
                    ),
                },
            );
        }
        Ok(())
    }

    /// Write `iTunesDB` + `ArtworkDB` (and the ithmb banks) back to the
    /// device (W-7, gated). The B-010 gate owns when this may run against a
    /// real pod.
    pub fn write_library(&mut self) -> Result<(), OperationFailure> {
        gate()?;
        with_native(|| unsafe { self.write_library_inner() })
    }

    unsafe fn write_library_inner(&mut self) -> Result<(), OperationFailure> {
        let write = write_symbols(self.backend_ref())?;
        let mut error = std::ptr::null_mut();
        // SAFETY: db valid for the session; error is a valid out-param.
        let ok = unsafe { (write.itdb_write)(self.db_ptr(), &mut error) };
        if ok == 0 {
            return Err(
                match unsafe { take_gerror(&mut error, self.backend_ref().glib()) } {
                    Some((domain, code, message)) => OperationFailure::GError {
                        domain,
                        code,
                        message,
                    },
                    None => OperationFailure::InvalidState(
                        "itdb_write failed without a GError".to_string(),
                    ),
                },
            );
        }
        Ok(())
    }
}

/// The write symbol table, or the specific W-1-compatible failure.
fn write_symbols(backend: &crate::IpodBackend) -> Result<&WriteSymbols, OperationFailure> {
    backend
        .write()
        .ok_or(OperationFailure::BackendSymbolMissing {
            symbol: "itdb_write",
        })
}

/// Find a track by dbid in the database track list.
///
/// # Safety
///
/// `db` must be a valid parsed database for the duration of the search.
unsafe fn find_track(db: *mut crate::ffi::Itdb_iTunesDB, dbid: u64) -> Option<*mut Itdb_Track> {
    // SAFETY: the list is database-owned and valid for the session.
    for raw in unsafe { GListIter::<Itdb_Track>::new((*db).tracks) } {
        // SAFETY: `raw` valid; `dbid` is plain data.
        if unsafe { (*raw).dbid } == dbid {
            return Some(raw.cast_mut());
        }
    }
    None
}

/// Find a playlist by id in the database playlist list.
///
/// # Safety
///
/// `db` must be a valid parsed database for the duration of the search.
unsafe fn find_playlist(db: *mut crate::ffi::Itdb_iTunesDB, id: u64) -> Option<*mut Itdb_Playlist> {
    // SAFETY: the list is database-owned and valid for the session.
    for raw in unsafe { GListIter::<Itdb_Playlist>::new((*db).playlists) } {
        // SAFETY: `raw` valid; `id` is plain data.
        if unsafe { (*raw).id } == id {
            return Some(raw.cast_mut());
        }
    }
    None
}

/// Apply the writable metadata fields to a native track. `None` fields are
/// left untouched; existing string fields are freed before replacement (the
/// track owns them and frees them on destruction — see the crate root
/// ownership table).
///
/// # Safety
///
/// `track` must be a valid `Itdb_Track` not concurrently accessed; the
/// backend must outlive the call.
unsafe fn apply_metadata(
    track: *mut Itdb_Track,
    metadata: &TrackMetadata,
    backend: &crate::IpodBackend,
) -> Result<(), OperationFailure> {
    let glib = backend.glib();
    // SAFETY: `track` is valid for the whole block; string fields are
    // database/track-owned.
    unsafe {
        replace_string(&mut (*track).title, metadata.title.as_deref(), glib)?;
        replace_string(&mut (*track).artist, metadata.artist.as_deref(), glib)?;
        replace_string(&mut (*track).album, metadata.album.as_deref(), glib)?;
        replace_string(
            &mut (*track).albumartist,
            metadata.albumartist.as_deref(),
            glib,
        )?;
        replace_string(&mut (*track).composer, metadata.composer.as_deref(), glib)?;
        replace_string(&mut (*track).genre, metadata.genre.as_deref(), glib)?;
        replace_string(&mut (*track).filetype, metadata.filetype.as_deref(), glib)?;
        (*track).size = metadata.size;
        (*track).tracklen = metadata.tracklen;
        (*track).cd_nr = metadata.cd_nr;
        (*track).cds = metadata.cds;
        (*track).track_nr = metadata.track_nr;
        (*track).tracks = metadata.tracks;
        (*track).year = metadata.year;
        (*track).bitrate = metadata.bitrate;
        (*track).samplerate = metadata.samplerate;
        (*track).rating = metadata.rating;
    }
    Ok(())
}

/// Replace a track-owned string field: free the old value (`g_free`), then
/// `g_strdup` the new one (or set null).
unsafe fn replace_string(
    field: &mut *mut c_char,
    new: Option<&str>,
    glib: &crate::ffi::symbols::GlibHelpers,
) -> Result<(), OperationFailure> {
    // SAFETY: the old pointer is track-owned and this is the one free of it.
    if !field.is_null() {
        unsafe { glib.g_free((*field).cast()) };
        *field = std::ptr::null_mut();
    }
    let Some(value) = new else {
        return Ok(());
    };
    let cvalue = CString::new(value)
        .map_err(|_| OperationFailure::InvalidState("string contains a NUL byte".to_string()))?;
    // SAFETY: `cvalue` is valid NUL-terminated input for g_strdup; the
    // result becomes track-owned.
    let dup = unsafe { glib.g_strdup(cvalue.as_ptr()) }
        .ok_or_else(|| OperationFailure::InvalidState("g_strdup unavailable".to_string()))?;
    *field = dup;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_matches_feature_state() {
        if SYNC_WRITES_ENABLED {
            assert!(gate().is_ok());
        } else {
            assert!(matches!(gate(), Err(OperationFailure::WritesGated)));
        }
    }
}
