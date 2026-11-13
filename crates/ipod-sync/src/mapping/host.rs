//! B-008: the host side of the mapping — owned `.itl` records built from
//! `itl-core` 0.1.1 + the `itl-sync` media mapping (see
//! `deltas/b008_plan.md` §4/§5).
//!
//! Everything here is a pure read: `ItlFile` accessors, `unique_tracks`
//! (the located-record-wins dedupe) and `MediaMapping`. No `.itl` writes,
//! no player-state merge — those belong to the sources layer. Dates and
//! stats are carried **raw** (Apple-epoch local wall-clock values); the
//! single conversion to a real instant happens at the `itl-sync` write
//! boundary, never here.

use std::path::{Path, PathBuf};

use itl_core::ItlFile;
use itl_sync::paths::{MappedLocation, MediaMapping};

use super::smart::{SmartRules, SmartRulesError, parse_smart_rules};

/// Library-level host facts the mapping needs.
#[derive(Clone, Debug, PartialEq)]
pub struct HostLibrary {
    /// Library Persistent ID (envelope header) — namespaces track PIDs.
    pub library_pid: u64,
    /// Local root of the guest media folder (the iTunes "Music Folder"
    /// as seen on this host).
    pub media_root: PathBuf,
    /// The `.itl` envelope timezone offset. Carried for provenance only:
    /// this layer compares raw values and never converts.
    pub tz_offset_seconds: i32,
}

/// Where a host track's file lives, per the `itl-sync` media mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostLocation {
    /// The local path of the media file (URI form authoritative).
    Mapped(PathBuf),
    /// No location at all (purchased-not-downloaded / cloud items).
    NoLocation,
    /// The location does not start with the media-folder prefix.
    OutsideMediaFolder,
}

/// The transferable catalog fields of a host track (B-008 §4 field table).
///
/// Units mirror the device side: sizes in bytes, duration in milliseconds,
/// bitrate in kbit/s (raw `mith` value), sample rate in Hz. `None` versus
/// zero follows the source accessors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostTrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    /// File extension from the mapped local path (e.g. `m4a`), lowercased.
    /// The `.itl` `kind()` is a display string and is deliberately not used
    /// as a file type.
    pub file_extension: Option<String>,
    pub size_bytes: u32,
    pub duration_ms: u32,
    pub disc_number: Option<u16>,
    pub disc_count: Option<u16>,
    pub track_number: Option<u16>,
    pub track_count: Option<u16>,
    pub year: Option<u16>,
    pub bit_rate: u16,
    pub sample_rate: u32,
    pub bpm: Option<u16>,
}

/// The sync-relevant host statistics, all raw as stored in the `.itl`.
///
/// `play_date_raw` is the Apple-epoch local wall-clock value; Sound Check
/// is deliberately absent (read-only in `itl-core`, out of sync scope).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostStats {
    /// 0–100, iTunes scale. The raw track value — album fill-down is never
    /// materialized here.
    pub rating: u8,
    pub play_count: u32,
    pub skip_count: u32,
    /// Apple-epoch local wall-clock seconds; 0 = never played.
    pub play_date_raw: u32,
    /// Raw `mith` volume adjustment (not a percent).
    pub volume_adjustment: i32,
}

/// One host track with its identity, location, metadata and raw stats.
#[derive(Clone, Debug, PartialEq)]
pub struct HostTrack {
    pub library_pid: u64,
    /// Track Persistent ID = `mith` @0x80 = the device dbid iTunes assigns
    /// (VERIFIED, NOTES §3.5.7). 0 = unset — the mapper must report it.
    pub track_pid: u64,
    /// The `.itl` numeric track id — the key for playlist members and
    /// `itl-sync` patches. Not a cross-sync identity.
    pub itl_id: u32,
    pub location: HostLocation,
    pub metadata: HostTrackMetadata,
    pub stats: HostStats,
    /// Raw `mith` @0xA6 content-rating byte.
    pub content_rating: u8,
}

/// Smart-playlist criteria of one host playlist, when the `.itl` carries
/// them (modern iTunes: `mhoh` 0x65/0x66).
#[derive(Clone, Debug, PartialEq)]
pub struct HostSmartPlaylist {
    /// Parsed `SLst` rules, or the parse failure — a parse error is carried,
    /// never silently dropped or treated as a static playlist. `None` for
    /// legacy `SmartPlaylistXml`-only playlists (no `SLst` blob stored).
    pub rules: Option<Result<SmartRules, SmartRulesError>>,
    /// Raw Smart Info blob (live-update/limit/sort settings), if present.
    pub info: Option<Vec<u8>>,
}

/// One host playlist (`.itl` `miph`).
#[derive(Clone, Debug, PartialEq)]
pub struct HostPlaylist {
    pub persistent_id: u64,
    pub name: Option<String>,
    /// `Some` when the playlist carries smart criteria.
    pub smart: Option<HostSmartPlaylist>,
    /// Ordered member references (`.itl` numeric track ids), duplicates
    /// preserved.
    pub member_itl_ids: Vec<u32>,
    /// The first `miph` in file order — the `.itl` master playlist position
    /// (XML `Master=true` verified for playlist #1, 2026-11-12). A
    /// *candidate*, not an assertion: the device master is identified by
    /// `is_mpl` on the device side.
    pub is_master_candidate: bool,
}

/// Build the library-level facts. `None` when the library has no media
/// folder mapping (the `itl-sync` survey convention).
pub fn host_library(itl: &ItlFile, media_root: &Path) -> Option<HostLibrary> {
    MediaMapping::new(itl, media_root)?;
    Some(HostLibrary {
        library_pid: itl.library_persistent_id(),
        media_root: media_root.to_path_buf(),
        tz_offset_seconds: itl.tz_offset_seconds(),
    })
}

/// All host tracks: `unique_tracks` dedupe (the located record wins),
/// media-mapped locations, raw stats, and the verified track PID.
pub fn host_tracks(itl: &ItlFile, mapping: &MediaMapping, library_pid: u64) -> Vec<HostTrack> {
    itl_sync::survey::unique_tracks(itl)
        .into_iter()
        .map(|track| {
            let location = match mapping.to_local(track) {
                MappedLocation::Mapped { path, .. } => HostLocation::Mapped(path),
                MappedLocation::NoLocation => HostLocation::NoLocation,
                MappedLocation::OutsideMediaFolder { .. } => HostLocation::OutsideMediaFolder,
            };
            let file_extension = match &location {
                HostLocation::Mapped(path) => {
                    path.extension().map(|e| e.to_string_lossy().to_lowercase())
                }
                _ => None,
            };
            HostTrack {
                library_pid,
                track_pid: track.persistent_id(),
                itl_id: track.id(),
                location,
                metadata: HostTrackMetadata {
                    title: track.title().map(str::to_string),
                    artist: track.artist().map(str::to_string),
                    album: track.album().map(str::to_string),
                    album_artist: track.album_artist().map(str::to_string),
                    composer: track.composer().map(str::to_string),
                    genre: track.genre().map(str::to_string),
                    file_extension,
                    size_bytes: track.size_bytes(),
                    duration_ms: track.duration_ms(),
                    disc_number: track.disc_number(),
                    disc_count: track.disc_count(),
                    track_number: track.track_number(),
                    track_count: track.track_count(),
                    year: track.year(),
                    bit_rate: track.bit_rate(),
                    sample_rate: track.sample_rate(),
                    bpm: track.bpm(),
                },
                stats: HostStats {
                    rating: track.rating(),
                    play_count: track.play_count(),
                    skip_count: track.skip_count(),
                    play_date_raw: track.play_date_raw(),
                    volume_adjustment: track.volume_adjustment(),
                },
                content_rating: track.content_rating(),
            }
        })
        .collect()
}

/// All host playlists in file order, with smart criteria parsed where
/// present.
pub fn host_playlists(itl: &ItlFile) -> Vec<HostPlaylist> {
    let mut first = true;
    itl.playlists()
        .iter()
        .map(|playlist| {
            let smart = playlist.is_smart().then(|| HostSmartPlaylist {
                rules: playlist.smart_rules().map(parse_smart_rules),
                info: playlist.smart_info().map(<[u8]>::to_vec),
            });
            let record = HostPlaylist {
                persistent_id: playlist.persistent_id(),
                name: playlist.title().map(str::to_string),
                smart,
                member_itl_ids: playlist.track_ids(),
                is_master_candidate: first,
            };
            first = false;
            record
        })
        .collect()
}
