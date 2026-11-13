//! B-008: the matching layer — host records against device records (see
//! `deltas/b008_plan.md` §5/§7).
//!
//! Identity rules, all research-backed:
//!
//! - Tracks match by **host track PID = device dbid** (iTunes writes the PID
//!   as the dbid; VERIFIED 3373/3373 on the restored-device snapshot). The
//!   device dbid stays a session-local handle; a host PID of zero is
//!   reported as no-identity, never guessed from titles/paths.
//! - Playlists match by **(name, kind)** — device `mhyp` records carry no
//!   host playlist PID (u64 header scan, 2026-11-12). The device master
//!   (`is_mpl`, named after the device) is a device-owned record, reported
//!   separately, never name-matched to a host playlist.
//! - Nothing here decides a sync action. Matches are evidence with
//!   diagnostics; every ambiguous or missing entry is explicit so B-009/
//!   B-010 can never turn it into an add/remove or a stat import silently.

use std::collections::HashMap;

use super::host::{HostPlaylist, HostTrack};

/// A track-level match between the host library and the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackMatch {
    pub itl_id: u32,
    pub track_pid: u64,
    pub outcome: TrackOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackOutcome {
    /// The host PID equals an on-device dbid.
    Matched { dbid: u64 },
    /// The device carries no track with this PID (not synced, or removed).
    MissingOnDevice,
    /// `mith` @0x80 is zero: the host track has no usable identity.
    NoIdentity,
}

/// A playlist-level match between the host library and the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistMatch {
    pub host_pid: u64,
    pub name: Option<String>,
    pub smart: bool,
    pub outcome: PlaylistOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaylistOutcome {
    /// A device playlist carries the same name.
    Matched {
        device_id: u64,
        /// The smart/regular kind agrees on both sides.
        kind_agrees: bool,
    },
    MissingOnDevice,
}

/// A device playlist with no host counterpart (by name). The device master
/// is reported through [`PlaylistMapping::device_master`], not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceOnlyPlaylist {
    pub device_id: u64,
    pub name: Option<String>,
    pub is_spl: bool,
}

/// One human-readable mapping diagnostic. Diagnostics are findings, not
/// failures: the mapping itself never silently resolves an ambiguity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappingDiagnostic {
    pub message: String,
}

impl MappingDiagnostic {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// The full track-level mapping of one host library against one device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackMapping {
    pub matches: Vec<TrackMatch>,
    /// Device dbids no host PID claims (device-only tracks, e.g. added on
    /// the pod or leftover after host removal).
    pub device_only_dbids: Vec<u64>,
    pub diagnostics: Vec<MappingDiagnostic>,
}

impl TrackMapping {
    pub fn matched(&self) -> impl Iterator<Item = &TrackMatch> {
        self.matches
            .iter()
            .filter(|m| matches!(m.outcome, TrackOutcome::Matched { .. }))
    }
}

/// The full playlist-level mapping of one host library against one device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaylistMapping {
    pub matches: Vec<PlaylistMatch>,
    /// The device master playlist (`is_mpl`), when present. Device-owned:
    /// iTunes names it after the device, so it never name-matches a host
    /// playlist.
    pub device_master: Option<DeviceOnlyPlaylist>,
    /// Non-master device playlists without a host counterpart by name
    /// (e.g. `On-The-Go 1`).
    pub device_only: Vec<DeviceOnlyPlaylist>,
    pub diagnostics: Vec<MappingDiagnostic>,
}

/// Map host tracks onto the device by PID ↔ dbid.
///
/// A device dbid claimed by more than one host PID is reported in
/// diagnostics and matched to the first host record only — the `unique_tracks`
/// dedupe makes the common duplicate case (second-`mlth` purchased records)
/// impossible to hit; a real collision means corrupt data.
pub fn map_tracks(host: &[HostTrack], device_tracks: &[ipod::TrackInfo]) -> TrackMapping {
    let mut claimed_by: HashMap<u64, u32> = HashMap::new(); // dbid -> itl id
    let mut mapping = TrackMapping::default();
    for track in host {
        if track.track_pid == 0 {
            mapping.diagnostics.push(MappingDiagnostic::new(format!(
                "track #{} has no persistent id (mith @0x80 zero)",
                track.itl_id
            )));
            mapping.matches.push(TrackMatch {
                itl_id: track.itl_id,
                track_pid: 0,
                outcome: TrackOutcome::NoIdentity,
            });
            continue;
        }
        match claimed_by.entry(track.track_pid) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(track.itl_id);
                let found = device_tracks
                    .iter()
                    .find(|device| device.dbid == track.track_pid);
                mapping.matches.push(TrackMatch {
                    itl_id: track.itl_id,
                    track_pid: track.track_pid,
                    outcome: match found {
                        Some(device) => TrackOutcome::Matched { dbid: device.dbid },
                        None => TrackOutcome::MissingOnDevice,
                    },
                });
            }
            std::collections::hash_map::Entry::Occupied(slot) => {
                mapping.diagnostics.push(MappingDiagnostic::new(format!(
                    "track #{} and #{} share persistent id {}",
                    slot.get(),
                    track.itl_id,
                    track.track_pid
                )));
                mapping.matches.push(TrackMatch {
                    itl_id: track.itl_id,
                    track_pid: track.track_pid,
                    outcome: TrackOutcome::NoIdentity,
                });
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for device in device_tracks {
        if device.dbid != 0 && !claimed_by.contains_key(&device.dbid) && seen.insert(device.dbid) {
            mapping.device_only_dbids.push(device.dbid);
        }
    }
    mapping.device_only_dbids.sort_unstable();
    mapping
}

/// Map host playlists onto the device by (name, kind).
pub fn map_playlists(
    host: &[HostPlaylist],
    device_playlists: &[ipod::PlaylistInfo],
) -> PlaylistMapping {
    let mut mapping = PlaylistMapping::default();
    let mut claimed_device_ids = std::collections::HashSet::new();
    for playlist in host {
        let by_name: Vec<&ipod::PlaylistInfo> = device_playlists
            .iter()
            .filter(|device| {
                !device.is_master
                    && device.name.as_deref() == playlist.name.as_deref()
                    && device.name.is_some()
            })
            .collect();
        let outcome = match by_name.as_slice() {
            [device] => {
                claimed_device_ids.insert(device.id);
                PlaylistOutcome::Matched {
                    device_id: device.id,
                    kind_agrees: device.is_spl == playlist.smart.is_some(),
                }
            }
            [] => PlaylistOutcome::MissingOnDevice,
            several => {
                mapping.diagnostics.push(MappingDiagnostic::new(format!(
                    "device carries {} playlists named {:?}; host playlist #{} matched to none",
                    several.len(),
                    playlist.name,
                    playlist.persistent_id
                )));
                PlaylistOutcome::MissingOnDevice
            }
        };
        mapping.matches.push(PlaylistMatch {
            host_pid: playlist.persistent_id,
            name: playlist.name.clone(),
            smart: playlist.smart.is_some(),
            outcome,
        });
    }
    for device in device_playlists {
        if device.is_master {
            mapping.device_master = Some(DeviceOnlyPlaylist {
                device_id: device.id,
                name: device.name.clone(),
                is_spl: device.is_spl,
            });
        } else if !claimed_device_ids.contains(&device.id) {
            mapping.device_only.push(DeviceOnlyPlaylist {
                device_id: device.id,
                name: device.name.clone(),
                is_spl: device.is_spl,
            });
        }
    }
    mapping
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::host::{
        HostLocation, HostPlaylist, HostStats, HostTrack, HostTrackMetadata,
    };

    fn host_track(itl_id: u32, pid: u64) -> HostTrack {
        HostTrack {
            library_pid: 1,
            track_pid: pid,
            itl_id,
            location: HostLocation::NoLocation,
            metadata: HostTrackMetadata::default(),
            stats: HostStats::default(),
            content_rating: 0,
        }
    }

    fn device_track(dbid: u64) -> ipod::TrackInfo {
        ipod::TrackInfo {
            dbid,
            ..Default::default()
        }
    }

    #[test]
    fn tracks_match_by_pid_and_report_all_outcomes() {
        let host = vec![
            host_track(1, 0xAABB),
            host_track(2, 0xCCDD),
            host_track(3, 0),
        ];
        let device = vec![device_track(0xAABB), device_track(0xEEFF)];
        let mapping = map_tracks(&host, &device);
        assert_eq!(
            mapping.matches[0].outcome,
            TrackOutcome::Matched { dbid: 0xAABB }
        );
        assert_eq!(mapping.matches[1].outcome, TrackOutcome::MissingOnDevice);
        assert_eq!(mapping.matches[2].outcome, TrackOutcome::NoIdentity);
        assert_eq!(mapping.device_only_dbids, vec![0xEEFF]);
        assert_eq!(mapping.matched().count(), 1);
        assert_eq!(mapping.diagnostics.len(), 1); // the zero-pid host track
    }

    #[test]
    fn duplicate_host_pids_are_diagnosed_not_double_matched() {
        let host = vec![host_track(1, 0xAABB), host_track(2, 0xAABB)];
        let device = vec![device_track(0xAABB)];
        let mapping = map_tracks(&host, &device);
        assert_eq!(mapping.matched().count(), 1);
        assert_eq!(mapping.matches[1].outcome, TrackOutcome::NoIdentity);
        assert!(
            mapping
                .diagnostics
                .iter()
                .any(|d| d.message.contains("share persistent id"))
        );
    }

    fn host_playlist(pid: u64, name: Option<&str>, smart: bool) -> HostPlaylist {
        HostPlaylist {
            persistent_id: pid,
            name: name.map(str::to_string),
            smart: smart.then_some(crate::mapping::host::HostSmartPlaylist {
                rules: None,
                info: None,
            }),
            member_itl_ids: Vec::new(),
            is_master_candidate: false,
        }
    }

    fn device_playlist(
        id: u64,
        name: Option<&str>,
        is_spl: bool,
        is_master: bool,
    ) -> ipod::PlaylistInfo {
        ipod::PlaylistInfo {
            id,
            name: name.map(str::to_string),
            type_byte: if is_master { 1 } else { 0 },
            is_master,
            is_spl,
            flag1: 0,
            flag2: 0,
            flag3: 0,
            member_dbids: Vec::new(),
        }
    }

    #[test]
    fn playlists_match_by_name_and_kind_with_master_kept_device_owned() {
        let host = vec![
            host_playlist(10, Some("Плейлист"), false),
            host_playlist(11, Some("Хай рейт"), true),
            host_playlist(12, Some("Отсутствует"), false),
        ];
        let device = vec![
            device_playlist(1, Some("iPod user"), false, true),
            device_playlist(2, Some("Плейлист"), false, false),
            device_playlist(3, Some("Хай рейт"), false, false), // kind mismatch
            device_playlist(4, Some("On-The-Go 1"), false, false),
        ];
        let mapping = map_playlists(&host, &device);
        assert_eq!(mapping.device_master.as_ref().unwrap().device_id, 1);
        assert_eq!(
            mapping.matches[0].outcome,
            PlaylistOutcome::Matched {
                device_id: 2,
                kind_agrees: true
            }
        );
        assert_eq!(
            mapping.matches[1].outcome,
            PlaylistOutcome::Matched {
                device_id: 3,
                kind_agrees: false
            }
        );
        assert_eq!(mapping.matches[2].outcome, PlaylistOutcome::MissingOnDevice);
        assert_eq!(mapping.device_only.len(), 1);
        assert_eq!(mapping.device_only[0].name.as_deref(), Some("On-The-Go 1"));
    }

    #[test]
    fn duplicate_device_playlist_names_are_diagnosed() {
        let host = vec![host_playlist(10, Some("Дубль"), false)];
        let device = vec![
            device_playlist(2, Some("Дубль"), false, false),
            device_playlist(3, Some("Дубль"), false, false),
        ];
        let mapping = map_playlists(&host, &device);
        assert_eq!(mapping.matches[0].outcome, PlaylistOutcome::MissingOnDevice);
        assert!(
            mapping
                .diagnostics
                .iter()
                .any(|d| d.message.contains("2 playlists named"))
        );
    }

    #[test]
    fn playlist_members_resolve_to_pids_or_explicit_none() {
        use crate::mapping::resolve_playlist_members;
        let tracks = vec![host_track(1, 0xA1), host_track(2, 0xA2), host_track(3, 0)];
        let playlist = HostPlaylist {
            persistent_id: 9,
            name: None,
            smart: None,
            member_itl_ids: vec![1, 2, 1, 42],
            is_master_candidate: false,
        };
        let resolved = resolve_playlist_members(&playlist, &tracks);
        assert_eq!(
            resolved,
            vec![Some(0xA1), Some(0xA2), Some(0xA1), None],
            "order and duplicates preserved; unknown members explicit"
        );
    }
}
