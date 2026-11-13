//! The device session: open, the owned read surface, close (W-2 / W-3).
//!
//! A [`DeviceSession`] is a handle to one parsed device database
//! (`Itdb_iTunesDB`). Everything returned by it is owned Rust data; no GLib
//! string, `GError`, or `GList` reference escapes a call. The session owns
//! the native database pointer and frees it exactly once in `Drop` via
//! `itdb_free` (which releases the *whole* tree — tracks, playlists,
//! artwork, strings; the wrapper therefore never frees individual objects).
//!
//! # Threading contract
//!
//! `DeviceSession` is deliberately `!Send + !Sync` (raw pointers inside), so
//! it stays on the thread that opened it; consumers offload blocking parses
//! with `spawn_blocking` around the session's owner. Every method is
//! additionally serialized through the crate-wide native lock (W-6), which
//! is what makes libgpod's non-reentrancy safe.
//!
//! # Mutation honesty (W-5)
//!
//! `open()` itself calls `itdb_parse`, which has documented upstream
//! mutation paths on *foreign* trees (`itdb_hash72_extract_hash_info` can
//! create `Device/HashInfo`; a missing `Artwork/` gets `g_mkdir`-ed — see
//! `deltas/b007_delta.md` §1.1 and the crate root docs). The wrapper never
//! opens a tree it hasn't been given, and the integration tests prove that
//! on a well-formed tree the reads are tree-hash-stable.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use crate::IpodBackend;
use crate::artwork::{classify, parse_artwork_db, proof_read};
use crate::ffi::symbols::ReadSymbols;
use crate::ffi::{
    GListIter, Itdb_IpodInfo, Itdb_Playlist, Itdb_Track, Itdb_iTunesDB, cstr_copy, gstr_take,
    take_gerror,
};
use crate::types::{
    ArtworkStatus, ArtworkVerdict, DeviceInfo, IpodError, ModelInfo, PlaybackStats, PlaylistInfo,
    TrackInfo,
};
use crate::with_native;

/// SysInfo keys surfaced in [`DeviceInfo::sysinfo`]. Kept small on purpose:
/// the wrapper reports what the device database says, the name-provenance
/// question itself is B-005's.
///
/// `FirewireGuid` is not usually written into the plain `SysInfo` by the
/// device: libgpod merges it in from `SysInfoExtended` (key `FireWireGUID`)
/// at parse time (`itdb_device_read_sysinfo_extended`, itdb_device.c) — so
/// the key appears exactly when the device actually exposes one.
const SYSINFO_KEYS: [&str; 3] = ["ModelNumStr", "SerialNumber", "FirewireGuid"];

/// A parsed device session. See the module docs for the ownership and
/// threading contracts.
pub struct DeviceSession<'a> {
    backend: &'a IpodBackend,
    /// Owned native database; freed exactly once, in `Drop`.
    db: *mut Itdb_iTunesDB,
    mount_point: PathBuf,
}

impl<'a> DeviceSession<'a> {
    /// Parse the device databases under `mount_point` (W-2).
    ///
    /// The mount point must name the root of a mounted iPod filesystem (the
    /// directory containing `iPod_Control/`). Discovery of candidates is
    /// B-003's job — the session takes the path as given.
    pub fn open(backend: &'a IpodBackend, mount_point: &Path) -> Result<Self, IpodError> {
        with_native(|| unsafe { Self::open_inner(backend, mount_point) })
    }

    unsafe fn open_inner(backend: &'a IpodBackend, mount_point: &Path) -> Result<Self, IpodError> {
        let mp = CString::new(mount_point.as_os_str().as_encoded_bytes())
            .map_err(|_| IpodError::NotFound("mount point contains a NUL byte".to_string()))?;
        let mut error = std::ptr::null_mut();
        // SAFETY: `mp` is a valid NUL-terminated string for the duration of
        // the call; `error` is a valid GError** out-param. `itdb_parse`
        // returns an owned database or null with the error set.
        let db = unsafe { (backend.read().itdb_parse)(mp.as_ptr(), &mut error) };
        if db.is_null() {
            return Err(match unsafe { take_gerror(&mut error, backend.glib()) } {
                Some((domain, code, message)) => IpodError::GError {
                    domain,
                    code,
                    message,
                },
                None => {
                    IpodError::Internal("itdb_parse returned NULL without a GError".to_string())
                }
            });
        }
        Ok(Self {
            backend,
            db,
            mount_point: mount_point.to_path_buf(),
        })
    }

    /// The mount point the session was opened with.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Raw database pointer for the gated write surface
    /// ([`crate::write`]). Crate-private: the pointer never escapes.
    pub(crate) fn db_ptr(&self) -> *mut Itdb_iTunesDB {
        self.db
    }

    /// The session's backend, for the gated write surface.
    pub(crate) fn backend_ref(&self) -> &'a IpodBackend {
        self.backend
    }

    /// Device identity and raw SysInfo strings (W-2 / W-3).
    ///
    /// The model is `None` exactly when libgpod cannot resolve it (empty
    /// `SysInfo`); the caller must surface that as "model unknown", never as
    /// a default (W-3, §1.2).
    pub fn device_info(&self) -> Result<DeviceInfo, IpodError> {
        with_native(|| unsafe { self.device_info_inner() })
    }

    unsafe fn device_info_inner(&self) -> Result<DeviceInfo, IpodError> {
        let read = self.backend.read();
        let glib = self.backend.glib();
        // SAFETY: `self.db` is a valid parsed database for the session's
        // lifetime; `device` is the Itdb_Device it owns (or null).
        let device = unsafe { (*self.db).device };

        let model = if device.is_null() {
            None
        } else {
            // SAFETY: `device` is a valid Itdb_Device.
            let info = unsafe { (read.itdb_device_get_ipod_info)(device) };
            // Note: this function never returns null for a valid device — an
            // unresolvable model is reported as the INVALID/UNKNOWN table
            // entry, so the W-3 gate checks the model *value*, not the
            // pointer (verified against itdb_device.c, 2026-11-08).
            if info.is_null() {
                None
            } else {
                // SAFETY: `info` points into libgpod's static model table.
                let ipod_model = unsafe { (*info).ipod_model };
                if ipod_model == crate::ffi::layouts::Itdb_IpodModel_ITDB_IPOD_MODEL_INVALID
                    || ipod_model == crate::ffi::layouts::Itdb_IpodModel_ITDB_IPOD_MODEL_UNKNOWN
                {
                    None
                } else {
                    // SAFETY: `info` points into libgpod's static model table.
                    Some(unsafe { model_from_info(info, read) })
                }
            }
        };

        let uuid = if device.is_null() {
            None
        } else {
            // SAFETY: `device` valid. `itdb_device_get_uuid` returns a
            // BORROWED pointer into the sysinfo hash (see the symbol docs:
            // the pinned build implements it as a plain hash lookup), so it
            // is copied without freeing — freeing it would corrupt the hash.
            unsafe { cstr_copy((read.itdb_device_get_uuid)(device)) }
        };

        let mut sysinfo = Vec::new();
        if !device.is_null() {
            for key in SYSINFO_KEYS {
                let ckey = CString::new(key).expect("SYSINFO_KEYS has no NUL bytes");
                // SAFETY: `device` valid, `ckey` valid for the call; result
                // is GLib-allocated and consumed by `gstr_take` once.
                if let Some(value) = unsafe {
                    gstr_take((read.itdb_device_get_sysinfo)(device, ckey.as_ptr()), glib)
                } {
                    sysinfo.push((key.to_string(), value));
                }
            }
        }

        Ok(DeviceInfo {
            mount_point: self.mount_point.clone(),
            model,
            uuid,
            sysinfo,
        })
    }

    /// All tracks, in database order (W-2).
    pub fn tracks(&self) -> Result<Vec<TrackInfo>, IpodError> {
        with_native(|| unsafe { Ok(self.tracks_inner()) })
    }

    unsafe fn tracks_inner(&self) -> Vec<TrackInfo> {
        let mut out = Vec::new();
        // SAFETY: `(*self.db).tracks` is the database-owned track list, valid
        // for the session's lifetime; the iterator borrows it without
        // mutation.
        for raw in unsafe { GListIter::<Itdb_Track>::new((*self.db).tracks) } {
            out.push(unsafe { track_to_owned(raw) });
        }
        out
    }

    /// All playlists, in database order (W-2). Membership is carried as
    /// device dbids in playlist order.
    pub fn playlists(&self) -> Result<Vec<PlaylistInfo>, IpodError> {
        with_native(|| unsafe { Ok(self.playlists_inner()) })
    }

    unsafe fn playlists_inner(&self) -> Vec<PlaylistInfo> {
        let read = self.backend.read();
        let mut out = Vec::new();
        // SAFETY: `(*self.db).playlists` is the database-owned playlist list,
        // valid for the session's lifetime.
        for raw in unsafe { GListIter::<Itdb_Playlist>::new((*self.db).playlists) } {
            // SAFETY: `raw` is a valid playlist pointer for the session's
            // lifetime; all fields are plain data or database-owned strings.
            let pl = unsafe { &*raw };
            let mut member_dbids = Vec::new();
            // SAFETY: `pl.members` is the playlist's member list, valid for
            // the session's lifetime.
            for member in unsafe { GListIter::<Itdb_Track>::new(pl.members) } {
                // SAFETY: `member` is a valid track pointer; `dbid` is a
                // plain u64 field.
                member_dbids.push(unsafe { (*member).dbid });
            }
            out.push(PlaylistInfo {
                id: pl.id,
                name: unsafe { cstr_copy(pl.name) },
                type_byte: pl.type_,
                // SAFETY: `raw` valid; `itdb_playlist_is_mpl` only reads the
                // type byte (no mutation — see db-playlist.c in the pin).
                is_master: unsafe { (read.itdb_playlist_is_mpl)(raw.cast_mut()) != 0 },
                is_spl: pl.is_spl != 0,
                member_dbids,
            });
        }
        out
    }

    /// Playback statistics for every track — the subset of [`TrackInfo`]
    /// that sync merges (B-008/B-010 will consume this). Reuses the track
    /// read; no second parse.
    pub fn playback_stats(&self) -> Result<Vec<PlaybackStats>, IpodError> {
        Ok(self
            .tracks()?
            .into_iter()
            .map(|track| PlaybackStats {
                dbid: track.dbid,
                rating: track.rating,
                playcount: track.playcount,
                playcount2: track.playcount2,
                recent_playcount: track.recent_playcount,
                skipcount: track.skipcount,
                recent_skipcount: track.recent_skipcount,
                time_played: track.time_played,
                soundcheck: track.soundcheck,
                volume: track.volume,
                explicit_flag: track.explicit_flag,
            })
            .collect())
    }

    /// The explicit artwork verdict (W-4).
    ///
    /// Compares the tracks that actually carry parsed thumbnails against the
    /// ArtworkDB's declared entries counted by this crate's own parser
    /// (independent of libgpod), and proof-reads at least one thumbnail per
    /// declared format straight from the `.ithmb` banks. `itdb_parse`
    /// success alone never implies `Ok` here: with an unknown model the
    /// verdict is [`ArtworkVerdict::SkippedUnknownModel`] even though the
    /// parse succeeded (B-007 §1.2).
    pub fn artwork_status(&self) -> Result<ArtworkStatus, IpodError> {
        with_native(|| unsafe { self.artwork_status_inner() })
    }

    unsafe fn artwork_status_inner(&self) -> Result<ArtworkStatus, IpodError> {
        // Inner helpers on purpose: the native lock is not reentrant, so the
        // public methods (which lock) must not be called from here.
        let info = unsafe { self.device_info_inner()? };
        let tracks = unsafe { self.tracks_inner() };
        let with_thumbnails = tracks.iter().filter(|t| t.has_thumbnail).count() as u64;
        let model_known = info.model.is_some();

        let db_path = unsafe { self.artwork_db_path() };
        let data = match std::fs::read(&db_path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (verdict, diagnostic) = classify(model_known, None, with_thumbnails, None);
                return Ok(ArtworkStatus {
                    verdict,
                    tracks_with_thumbnails: with_thumbnails,
                    declared_entries: None,
                    proof_read: None,
                    diagnostic,
                });
            }
            Err(e) => {
                // Exists but unreadable: never silently accepted as artless.
                return Ok(ArtworkStatus {
                    verdict: ArtworkVerdict::Missing,
                    tracks_with_thumbnails: with_thumbnails,
                    declared_entries: None,
                    proof_read: None,
                    diagnostic: Some(format!("ArtworkDB unreadable: {e}")),
                });
            }
        };
        let parsed = match parse_artwork_db(&data) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Ok(ArtworkStatus {
                    verdict: ArtworkVerdict::Missing,
                    tracks_with_thumbnails: with_thumbnails,
                    declared_entries: None,
                    proof_read: None,
                    diagnostic: Some(format!("ArtworkDB parse failed: {e}")),
                });
            }
        };
        let declared = parsed.declared;

        // The proof-read runs only when there is something to prove and the
        // model gate let libgpod attach artwork at all.
        let proof_read = if model_known && declared > 0 {
            let artwork_dir = db_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.mount_point.join("iPod_Control").join("Artwork"));
            Some(proof_read(&artwork_dir, &parsed.first_per_format))
        } else {
            None
        };
        let (verdict, diagnostic) = classify(
            model_known,
            Some(declared),
            with_thumbnails,
            proof_read
                .as_ref()
                .and_then(|report| report.failed.as_ref()),
        );
        Ok(ArtworkStatus {
            verdict,
            tracks_with_thumbnails: with_thumbnails,
            declared_entries: Some(declared),
            proof_read,
            diagnostic,
        })
    }

    /// Resolve the ArtworkDB path: libgpod's own resolution first (handles
    /// its device-dir logic), the canonical location as fallback.
    ///
    /// # Safety
    ///
    /// Same session invariants as the other inner helpers.
    unsafe fn artwork_db_path(&self) -> PathBuf {
        let read = self.backend.read();
        let glib = self.backend.glib();
        // SAFETY: the mount point was validated NUL-free in `open`.
        let mp = CString::new(self.mount_point.as_os_str().as_encoded_bytes())
            .expect("mount point validated at open");
        // SAFETY: `mp` valid for the call; the result is GLib-allocated and
        // consumed by `gstr_take` exactly once.
        let path = unsafe { (read.itdb_get_artworkdb_path)(mp.as_ptr()) };
        if !path.is_null() {
            if let Some(resolved) = unsafe { gstr_take(path, glib) } {
                return PathBuf::from(resolved);
            }
        }
        self.mount_point
            .join("iPod_Control")
            .join("Artwork")
            .join("ArtworkDB")
    }
}

impl std::fmt::Debug for DeviceSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSession")
            .field("mount_point", &self.mount_point)
            .finish_non_exhaustive()
    }
}

impl Drop for DeviceSession<'_> {
    fn drop(&mut self) {
        // `itdb_free` releases the whole tree exactly once, under the native
        // lock. Drop runs on the owning thread and the session is !Send, so
        // no call can be in flight here.
        with_native(|| unsafe {
            (self.backend.read().itdb_free)(self.db);
        });
    }
}

/// Convert one native track into owned data, copying every string field.
///
/// # Safety
///
/// `t` must be a valid `Itdb_Track` owned by the parsed database and live for
/// the duration of the call. String fields are database-owned (released by
/// `itdb_free`), so they are copied and never freed here.
unsafe fn track_to_owned(t: *const Itdb_Track) -> TrackInfo {
    // SAFETY: precondition above; every field read is plain data or a
    // database-owned string.
    unsafe {
        let track = &*t;
        // The artwork pointer chain: a thumbnail is attached iff both the
        // artwork block and its thumbnail entry exist. The thumbnail itself
        // is opaque (never dereferenced) — see ffi/layouts.rs.
        let artwork = track.artwork;
        let has_thumbnail = !artwork.is_null() && !(*artwork).thumbnail.is_null();
        TrackInfo {
            dbid: track.dbid,
            ipod_path: cstr_copy(track.ipod_path),
            title: cstr_copy(track.title),
            artist: cstr_copy(track.artist),
            album: cstr_copy(track.album),
            albumartist: cstr_copy(track.albumartist),
            composer: cstr_copy(track.composer),
            genre: cstr_copy(track.genre),
            filetype: cstr_copy(track.filetype),
            size: track.size,
            tracklen: track.tracklen,
            cd_nr: track.cd_nr,
            cds: track.cds,
            track_nr: track.track_nr,
            tracks: track.tracks,
            year: track.year,
            bitrate: track.bitrate,
            samplerate: track.samplerate,
            bpm: track.BPM,
            rating: track.rating,
            playcount: track.playcount,
            playcount2: track.playcount2,
            recent_playcount: track.recent_playcount,
            skipcount: track.skipcount,
            recent_skipcount: track.recent_skipcount,
            // `time_t` on LP64; the value is Unix seconds in local wall-clock
            // time (verified against the host .itl, NOTES.md §20).
            time_played: track.time_played,
            time_added: track.time_added,
            volume: track.volume,
            soundcheck: track.soundcheck,
            explicit_flag: track.explicit_flag,
            has_thumbnail,
            mhii_link: track.mhii_link,
        }
    }
}

/// Build the owned model description from a libgpod model-table entry.
///
/// # Safety
///
/// `info` must be a valid `Itdb_IpodInfo` (a static entry of libgpod's model
/// table, valid for the process lifetime). The name functions return static
/// strings.
unsafe fn model_from_info(info: *const Itdb_IpodInfo, read: &ReadSymbols) -> ModelInfo {
    // SAFETY: precondition above.
    unsafe {
        let ipod_model = (*info).ipod_model;
        let ipod_generation = (*info).ipod_generation;
        let capacity = (*info).capacity;
        ModelInfo {
            model_number: cstr_copy((*info).model_number).unwrap_or_default(),
            // SAFETY: model/generation names are static strings.
            model_name: cstr_copy((read.itdb_info_model_name_string)(ipod_model))
                .unwrap_or_default(),
            generation_name: cstr_copy((read.itdb_info_generation_string)(ipod_generation))
                .unwrap_or_default(),
            ipod_model: ipod_model as i32,
            generation: ipod_generation as i32,
            // `0.0` = the model table carries no nominal capacity. Display
            // only — free-space decisions are filesystem stats (B-005).
            capacity_gb: (capacity != 0.0).then_some(capacity),
        }
    }
}
