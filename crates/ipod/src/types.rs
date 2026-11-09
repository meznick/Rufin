//! Owned data types crossing the FFI boundary.
//!
//! Everything here is plain Rust data: no raw pointers, no GLib strings, no
//! borrowed native memory. The conversion happens in [`crate::session`] before
//! any of these values are returned; after a call the caller holds no live
//! reference into the native database.

use std::path::PathBuf;

/// Errors produced by session reads.
///
/// Structurally mirrors the failure sources of libgpod without exposing GLib
/// handles: `GError` values are converted (domain/code/message) and freed
/// before this error is returned.
#[derive(Debug, thiserror::Error)]
pub enum IpodError {
    /// A libgpod call reported a GLib error. `domain` is the GQuark string
    /// (e.g. `itdb-file-error-quark`), `code` the quark-relative code.
    #[error("{domain} {code}: {message}")]
    GError {
        domain: String,
        code: i32,
        message: String,
    },
    /// A requested entity (path, database file, declared entry) is missing.
    #[error("{0}")]
    NotFound(String),
    /// An internal invariant of the wrapper was violated (a bug in this
    /// crate, not in the device data).
    #[error("internal: {0}")]
    Internal(String),
}

/// Why the libgpod backend could not be loaded or is unusable.
///
/// W-1: every variant carries enough context to be shown as an actionable
/// diagnostic; the caller never receives a panic or a fallback path.
#[derive(Debug, thiserror::Error)]
pub enum BackendUnavailable {
    /// This platform has no backend implementation (anything but 64-bit
    /// Linux today). Loading is not even attempted.
    #[error("libgpod backend is not supported on this platform")]
    UnsupportedPlatform,
    /// `RUFIN_LIBGPOD_DIR` was set but no usable library was found there, or
    /// the loader's default paths do not contain `libgpod.so.4`.
    #[error("libgpod library not found: {0}")]
    MissingLibrary(String),
    /// A required read-side symbol is absent from the loaded library — it is
    /// not a libgpod 0.8.3 SONAME-4 build despite the name.
    #[error("libgpod library {library} is missing required symbol {symbol}")]
    MissingSymbol {
        library: String,
        symbol: &'static str,
    },
    /// The `rufin-libgpod.abi` marker next to the library exists but does not
    /// match this crate's pinned layout set. Hard refusal: struct layouts in
    /// `ffi/layouts.rs` are only valid for the build the marker describes.
    #[error("libgpod ABI marker mismatch: {0}")]
    AbiMismatch(String),
    /// The dynamic loader accepted the file but the platform loader failed
    /// (`dlopen`/`dlsym` error). `details` says which step failed.
    #[error("libgpod loading failed: {details}")]
    Load { details: String },
}

/// Device identity and raw SysInfo strings as libgpod reports them.
///
/// Deliberately *no* name field: the name-provenance question (master
/// playlist name vs volume label vs empty `SysInfo`) is B-005's R3 gate. This
/// type only carries what the device database itself says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceInfo {
    /// The mount point the session was opened with.
    pub mount_point: PathBuf,
    /// Model information, or `None` when libgpod cannot resolve a model —
    /// the empty-`SysInfo` case. `None` here is exactly the W-3 "model
    /// unknown" signal and must never be replaced by a default model.
    pub model: Option<ModelInfo>,
    /// Device GUID (from `itdb_device_get_uuid`). Quirk of the pinned build:
    /// the implementation returns the sysinfo hash's `FirewireGuid` entry
    /// (borrowed, copied by the wrapper) rather than a database id, so this
    /// equals [`DeviceInfo::firewire_id`] whenever the device exposes one,
    /// and is `None` otherwise.
    pub uuid: Option<String>,
    /// Key/value pairs from `SysInfo` (`ModelNumStr`, `SerialNumber`, ...).
    /// Empty when `SysInfo` is absent or unreadable.
    pub sysinfo: Vec<(String, String)>,
}

impl DeviceInfo {
    /// The `ModelNumStr` SysInfo value, if present (e.g. `MA446`).
    pub fn model_num_str(&self) -> Option<&str> {
        self.sysinfo
            .iter()
            .find(|(key, _)| key == "ModelNumStr")
            .map(|(_, value)| value.as_str())
    }

    /// The device FireWire ID, if the device exposed one.
    ///
    /// libgpod merges `FireWireGUID` from `SysInfoExtended` into the plain
    /// sysinfo hash under the key `FirewireGuid` at parse time, so `None`
    /// here means the device carries no `SysInfoExtended` entry — the common
    /// case on the Video snapshot. B-005 consumes this; the Classic/Nano
    /// Video database checksum (the B-008+ sync write path, not B-004's
    /// preflight) keys off the same value.
    pub fn firewire_id(&self) -> Option<&str> {
        self.sysinfo
            .iter()
            .find(|(key, _)| key == "FirewireGuid")
            .map(|(_, value)| value.as_str())
    }
}

/// Resolved model identity of a device.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// Abbreviated model number (`MA446` → `A446`; see `Itdb_IpodInfo` in
    /// itdb.h for the abbreviation rule).
    pub model_number: String,
    /// Human model name via `itdb_info_get_ipod_model_name_string`
    /// (e.g. "Video (Black)").
    pub model_name: String,
    /// Raw `Itdb_IpodModel` enum value; interpret against the enum in
    /// `ffi::layouts` or via `model_name`.
    pub ipod_model: i32,
    /// Raw `Itdb_IpodGeneration` enum value (e.g. VIDEO_2 = 17).
    pub generation: i32,
    /// Human generation name via `itdb_info_get_ipod_generation_string`.
    pub generation_name: String,
    /// Nominal capacity in GB from libgpod's static model table. **Not**
    /// usable for free-space decisions — B-005 measures filesystem capacity;
    /// this is display-only.
    pub capacity_gb: Option<f64>,
}

/// One track as read from the device `iTunesDB`.
///
/// Field semantics follow libgpod's `Itdb_Track` (itdb.h) and were verified
/// against the host `.itl` by the B-007 cross-checks (itl-player `NOTES.md`
/// §20): stats are bit-exact for the tested tracks, `time_played` is Unix
/// seconds in local wall-clock time (= the `.itl` 1904 value − 2082844800),
/// and the low byte of `explicit_flag` equals the `.itl` mith 0xA6 content
/// byte (0x01 store-explicit / 0x02 clean / 0x04 manual-explicit).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackInfo {
    /// Device-side database id. **Not a stable cross-sync identity** — may be
    /// reassigned across restores (B-001/plan.md); sync must key on content.
    pub dbid: u64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub albumartist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    /// File extension string, e.g. `m4a` (itdb.h `filetype`).
    pub filetype: Option<String>,
    pub size: u32,
    /// Length in milliseconds.
    pub tracklen: i32,
    pub cd_nr: i32,
    pub cds: i32,
    pub track_nr: i32,
    pub tracks: i32,
    pub year: i32,
    pub bitrate: i32,
    pub samplerate: u16,
    pub bpm: i16,
    /// 0–100, iTunes scale (stars × 20).
    pub rating: u32,
    pub playcount: u32,
    pub playcount2: u32,
    pub recent_playcount: u32,
    pub skipcount: u32,
    pub recent_skipcount: u32,
    /// Unix seconds, local wall-clock time of the machine that wrote it.
    pub time_played: i64,
    pub time_added: i64,
    /// Volume adjustment, raw `.itl`/iTunesDB value (−100..100 range in
    /// practice; `0` = none). iTunes exports the raw value, not percent.
    pub volume: i32,
    /// Sound Check gain, raw value; `0` = unanalyzed. iTunes recomputes it
    /// on playback without rewriting file tags, so device and host may
    /// legitimately differ.
    pub soundcheck: u32,
    /// u16 little-endian at mhit 0xA6; the low byte is the content-rating
    /// byte. `1` = explicit, `2` = clean, `4` = manual (not exported to XML).
    pub explicit_flag: u16,
    /// Whether a thumbnail is attached in the parsed artwork data (i.e.
    /// `track->artwork && track->artwork->thumbnail`). Artwork can be
    /// silently absent on an unknown model — see [`ArtworkStatus`].
    pub has_thumbnail: bool,
    /// Sparse-artwork link into the ArtworkDB (`mhii_link`, since libgpod
    /// 0.7.0); `0` when unused.
    pub mhii_link: u32,
}

/// Playback statistics of one track — the subset of [`TrackInfo`] that sync
/// merges. Kept separate so stat merging (B-008/B-010) never accidentally
/// carries catalog fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackStats {
    pub dbid: u64,
    pub rating: u32,
    pub playcount: u32,
    pub playcount2: u32,
    pub recent_playcount: u32,
    pub skipcount: u32,
    pub recent_skipcount: u32,
    pub time_played: i64,
    pub soundcheck: u32,
    pub volume: i32,
    pub explicit_flag: u16,
}

/// One playlist as read from the device `iTunesDB`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaylistInfo {
    /// Device playlist id (`Itdb_Playlist.id`) — the key for
    /// [`crate::DeviceSession::update_playlists`].
    pub id: u64,
    pub name: Option<String>,
    /// Raw `Itdb_Playlist.type` byte: `0` normal, `1` master (MPL). The
    /// constants are private to libgpod (itdb_private.h), so this byte is
    /// carried raw and `is_master` is computed via the exported
    /// `itdb_playlist_is_mpl` helper.
    pub type_byte: u8,
    /// Master playlist (contains every track). From `itdb_playlist_is_mpl`.
    pub is_master: bool,
    /// Smart playlist flag (`Itdb_Playlist.is_spl`).
    pub is_spl: bool,
    /// Device dbids of member tracks, in playlist order.
    pub member_dbids: Vec<u64>,
}

/// The explicit artwork verdict for a parsed session (W-4).
///
/// libgpod's `itdb_parse` ignores the artwork parser's result, so parse
/// success alone says nothing about artwork. This type compares the tracks
/// that actually carry thumbnails against the ArtworkDB's independently
/// counted declared entries, and adds a proof-read of at least one thumbnail
/// per declared format (the parser/decoder live in the private `artwork`
/// module).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtworkStatus {
    pub verdict: ArtworkVerdict,
    /// Tracks with a parsed thumbnail attached.
    pub tracks_with_thumbnails: u64,
    /// Entries declared in the ArtworkDB, counted by this crate's own mhii
    /// parser (independent of libgpod). `None` = ArtworkDB absent or
    /// unreadable.
    pub declared_entries: Option<u64>,
    /// Proof-read report when declared entries exist and the model gate
    /// allowed artwork parsing.
    pub proof_read: Option<ProofReadReport>,
    /// Human-readable detail when the verdict is not `Ok`.
    pub diagnostic: Option<String>,
}

/// Outcome of [`ArtworkStatus`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArtworkVerdict {
    /// Declared entries match parsed thumbnails (or the device is
    /// legitimately artless: ArtworkDB present with zero entries).
    #[default]
    Ok,
    /// ArtworkDB declares entries that are not parsed, the ArtworkDB itself
    /// is missing, or a proof-read failed. Never a parse-level error.
    Missing,
    /// The model could not be resolved (empty `SysInfo`), so libgpod skipped
    /// artwork parsing entirely even though `itdb_parse` succeeded. The
    /// device must not be treated as artless (B-007 §1.2).
    SkippedUnknownModel,
}

/// Result of the one-thumbnail-per-format proof-read (B-007 §1.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProofReadReport {
    /// Distinct format ids seen in the ArtworkDB.
    pub formats_checked: Vec<u32>,
    /// The first format whose pixel decode failed, with the reason.
    pub failed: Option<ProofReadFailure>,
}

/// Which proof-read step failed and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProofReadFailure {
    pub format_id: u32,
    pub reason: String,
}

/// The writable metadata subset for `create_track`/`update_track` (W-7).
///
/// Field semantics mirror [`TrackInfo`]; `None` = leave unset. The sync
/// mapping (B-008) extends this with the fields the delta computation
/// needs; B-007 only needs a faithful, safe pass-through to the native
/// struct.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub albumartist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    /// File extension string, e.g. `m4a`.
    pub filetype: Option<String>,
    pub size: u32,
    pub tracklen: i32,
    pub cd_nr: i32,
    pub cds: i32,
    pub track_nr: i32,
    pub tracks: i32,
    pub year: i32,
    pub bitrate: i32,
    pub samplerate: u16,
    /// 0–100, iTunes scale.
    pub rating: u32,
}

/// Encoded artwork input for `set_artwork` (W-7).
///
/// `encoded` must be a PNG/JPEG libgpod's gdk-pixbuf loader understands;
/// libgpod decodes it internally and generates every model format on
/// `write_library`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkInput {
    pub encoded: Vec<u8>,
    pub rotation: i32,
}

/// Playlist membership changes for `update_playlists` (W-7).
///
/// Keyed by playlist id (`Itdb_Playlist.id`); the semantic mapping of
/// Rufin playlists onto device playlists is B-008's job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaylistChanges {
    /// `(playlist id, track dbids to append)`.
    pub additions: Vec<(u64, Vec<u64>)>,
}
