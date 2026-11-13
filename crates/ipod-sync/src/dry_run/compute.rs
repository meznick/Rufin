//! B-009: the pure dry-run engine (`deltas/b009_plan.md` §4/§5).
//!
//! `compute` consumes owned [`HostInputs`]/[`DeviceInputs`] and returns the
//! [`DryRunReport`]: no I/O, no session, no clock, no write destinations.
//! The mapping is recomputed here with B-008's functions so the engine has
//! a single input shape and every comparison runs on owned values.
//!
//! Isolation by construction: nothing in this module can touch the device
//! or the host `.itl`; the env-gated gate proves zero mutation end to end.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::mapping::{
    HostLocation, HostPlaylist, PlaylistOutcome, TrackOutcome, map_playlists, map_tracks,
    parse_smart_rules, resolve_playlist_members,
};

use super::flatten::{self, FlattenError};
use super::model::*;

/// Apple-epoch (1904-01-01) → Unix-epoch (1970-01-01) second delta, the
/// conversion libgpod applies to device times (B-008 §2a).
const APPLE_TO_UNIX: i64 = 2_082_844_800;

/// The fixed overhead added to the AAC-256 capacity estimate for the moov
/// atoms (owner formula, decision §9-4).
const MOOV_OVERHEAD_BYTES: u64 = 8 * 1024;

/// AAC 256 kbps nominal rate in bytes per second (owner formula).
const AAC_BYTES_PER_SECOND: u64 = 32_000;

/// Compute the dry-run delta. Pure: identical inputs yield an identical
/// report in any order (all itemized lists are sorted by stable keys).
pub fn compute(host: &HostInputs, device: &DeviceInputs, opts: &SyncOptions) -> DryRunReport {
    let mut report = DryRunReport::default();
    let library = match &host.library {
        Some(library) => library,
        None => {
            report
                .diagnostics
                .push("host library has no media-folder mapping — nothing actionable".to_string());
            return report;
        }
    };
    report.context = ReportContext {
        library_pid: library.library_pid,
        device_name: (!device.name.is_empty()).then(|| device.name.clone()),
        device_model: (!device.model.is_empty()).then(|| device.model.clone()),
        options: opts.clone(),
        artwork_verdict: device.artwork_status.verdict,
        play_counts_present: device.play_counts_present.clone(),
        device_master: None,
        device_local_playlists: Vec::new(),
    };

    let track_mapping = map_tracks(&host.tracks, &device.tracks);
    let playlist_mapping = map_playlists(&host.playlists, &device.playlists);
    for diagnostic in track_mapping
        .diagnostics
        .iter()
        .chain(playlist_mapping.diagnostics.iter())
    {
        report.diagnostics.push(diagnostic.message.clone());
    }

    let host_by_itl: HashMap<u32, _> = host.tracks.iter().map(|t| (t.itl_id, t)).collect();
    let device_by_dbid: HashMap<u64, _> = device.tracks.iter().map(|t| (t.dbid, t)).collect();
    let tz_offset = library.tz_offset_seconds;

    for m in &track_mapping.matches {
        let Some(track) = host_by_itl.get(&m.itl_id) else {
            continue;
        };
        match &m.outcome {
            TrackOutcome::Matched { dbid } => {
                let Some(device_track) = device_by_dbid.get(dbid) else {
                    continue;
                };
                for diff in stat_diffs(track, device_track, tz_offset) {
                    report.stat_diffs.push(diff);
                }
                // File repair: the database lists the track, the file is gone.
                let file_present = device
                    .device_file_present
                    .get(dbid)
                    .copied()
                    .unwrap_or(true);
                if !file_present {
                    match &track.location {
                        HostLocation::Mapped(path) => {
                            if host_file_present(host, track.itl_id) {
                                let mode = copy_mode(opts);
                                report.file_repairs.push(FileRepair {
                                    dbid: *dbid,
                                    device_path: device_fs_path(device_track.ipod_path.as_deref()),
                                    host_path: path.clone(),
                                    mode,
                                    size_estimate: size_estimate(
                                        track,
                                        mode,
                                        host.artwork.get(&track.track_pid).copied(),
                                    ),
                                });
                            } else {
                                report.diagnostics.push(format!(
                                    "track #{}: device file missing and host file absent — \
                                     cannot repair",
                                    track.itl_id
                                ));
                            }
                        }
                        _ => report.diagnostics.push(format!(
                            "track #{}: device file missing and host has no mapped source",
                            track.itl_id
                        )),
                    }
                }
            }
            TrackOutcome::MissingOnDevice => match &track.location {
                HostLocation::Mapped(path) => {
                    if host_file_present(host, track.itl_id) {
                        let mode = copy_mode(opts);
                        report.file_adds.push(FileAdd {
                            itl_id: track.itl_id,
                            pid: track.track_pid,
                            host_path: path.clone(),
                            mode,
                            size_estimate: size_estimate(
                                track,
                                mode,
                                host.artwork.get(&track.track_pid).copied(),
                            ),
                        });
                    } else {
                        report.diagnostics.push(format!(
                            "track #{}: not on the device and its host file is missing",
                            track.itl_id
                        ));
                    }
                }
                // Cloud/purchased or outside the media folder: never a
                // candidate, never a removal (B-008 §9).
                HostLocation::NoLocation | HostLocation::OutsideMediaFolder => {}
            },
            TrackOutcome::NoIdentity => {
                report.diagnostics.push(format!(
                    "track #{}: no persistent id — not actionable",
                    track.itl_id
                ));
            }
        }
    }

    for dbid in &track_mapping.device_only_dbids {
        match opts.device_only {
            DeviceOnlyPolicy::Remove => report.file_removes.push(FileRemove {
                dbid: *dbid,
                device_path: device_fs_path(
                    device_by_dbid
                        .get(dbid)
                        .and_then(|t| t.ipod_path.as_deref()),
                ),
            }),
            DeviceOnlyPolicy::Keep => report.device_only_tracks.push(*dbid),
        }
    }

    // Playlists: `playlist_mapping.matches` is in host order — zip with the
    // host records.
    for (host_pl, m) in host.playlists.iter().zip(playlist_mapping.matches.iter()) {
        // The host master (first `miph`, Master-flagged) is represented on
        // the device by the device master (`is_mpl`) — its name is the
        // device's own, so a name match never fires (B-008 §7). It is
        // never a create candidate.
        if host_pl.is_master_candidate {
            continue;
        }
        match &m.outcome {
            PlaylistOutcome::MissingOnDevice => {
                let members = resolve_playlist_members(host_pl, &host.tracks);
                let unresolved = members.iter().filter(|m| m.is_none()).count();
                if unresolved > 0 {
                    report.diagnostics.push(format!(
                        "playlist {:?}: {unresolved} unresolvable members (excluded from the \
                         planned create)",
                        host_pl.name
                    ));
                }
                let (kind, criteria) = create_criteria(host_pl, &mut report);
                report.playlist_creates.push(PlaylistCreate {
                    host_pid: host_pl.persistent_id,
                    name: host_pl.name.clone(),
                    kind,
                    member_pids: members,
                    criteria,
                });
            }
            PlaylistOutcome::Matched {
                device_id,
                kind_agrees,
            } => {
                if !kind_agrees {
                    report.diagnostics.push(format!(
                        "playlist {:?}: host/device smart-vs-static kind disagrees — reported, \
                         never converted",
                        host_pl.name
                    ));
                    continue;
                }
                let Some(device_pl) = device.playlists.iter().find(|p| p.id == *device_id) else {
                    continue;
                };
                let resolved = resolve_playlist_members(host_pl, &host.tracks);
                let unresolved = resolved.iter().filter(|m| m.is_none()).count();
                if unresolved > 0 {
                    report.diagnostics.push(format!(
                        "playlist {:?}: {unresolved} unresolvable members (excluded from the \
                         member diff)",
                        host_pl.name
                    ));
                }
                let host_members: Vec<u64> = resolved.iter().flatten().copied().collect();
                let members_differ = host_members != device_pl.member_dbids;
                let members_add = multiset_difference(&host_members, &device_pl.member_dbids);
                let members_remove = multiset_difference(&device_pl.member_dbids, &host_members);
                let members_reorder = members_add.is_empty() && members_remove.is_empty();
                let criteria = criteria_state(host_pl, device_pl, &device.smart_blobs);
                if criteria == CriteriaState::Unknown {
                    report.diagnostics.push(format!(
                        "playlist {:?}: smart criteria comparison inconclusive — no criteria \
                         action planned",
                        host_pl.name
                    ));
                }
                let smart_info_present = host_pl.smart.as_ref().is_some_and(|s| s.info.is_some());
                if members_differ || criteria == CriteriaState::Differ {
                    report.playlist_updates.push(PlaylistUpdate {
                        device_id: *device_id,
                        name: host_pl.name.clone(),
                        members_add,
                        members_remove,
                        members_reorder,
                        criteria,
                        smart_info_present,
                    });
                }
            }
        }
    }

    if let Some(master) = &playlist_mapping.device_master {
        report.context.device_master = Some(PlaylistRemove {
            device_id: master.device_id,
            name: master.name.clone(),
        });
    }
    for device_pl in &playlist_mapping.device_only {
        let entry = PlaylistRemove {
            device_id: device_pl.device_id,
            name: device_pl.name.clone(),
        };
        if is_device_local(device_pl.name.as_deref()) {
            report.context.device_local_playlists.push(entry);
        } else {
            match opts.device_only {
                DeviceOnlyPolicy::Remove => report.playlist_removes.push(entry),
                DeviceOnlyPolicy::Keep => report.device_only_playlists.push(entry),
            }
        }
    }

    // Artwork, gated by the wrapper verdict.
    if device.artwork_status.verdict == ipod::ArtworkVerdict::Ok {
        for m in track_mapping.matched() {
            let TrackOutcome::Matched { dbid } = m.outcome else {
                continue;
            };
            let (Some(track), Some(device_track)) =
                (host_by_itl.get(&m.itl_id), device_by_dbid.get(&dbid))
            else {
                continue;
            };
            match host.artwork.get(&track.track_pid) {
                Some(CoverEvidence::Present { .. }) => {
                    if !device_track.has_thumbnail {
                        report.artwork_adds.push(ArtworkAdd {
                            pid: track.track_pid,
                        });
                    }
                }
                Some(CoverEvidence::Absent) => {
                    if device_track.has_thumbnail {
                        let entry = ArtworkRemove {
                            pid: track.track_pid,
                        };
                        match opts.device_only {
                            DeviceOnlyPolicy::Remove => report.artwork_removes.push(entry),
                            DeviceOnlyPolicy::Keep => report.artwork_orphaned.push(entry),
                        }
                    }
                }
                Some(CoverEvidence::Unknown) | None => report.artwork_unknown += 1,
            }
        }
    } else {
        report.diagnostics.push(format!(
            "artwork section gated: verdict {:?} (diagnostic: {:?})",
            device.artwork_status.verdict, device.artwork_status.diagnostic
        ));
    }

    // Capacity: add/repair estimates vs. measured free space.
    let estimate: u64 = report
        .file_adds
        .iter()
        .map(|a| a.size_estimate)
        .chain(report.file_repairs.iter().map(|r| r.size_estimate))
        .sum();
    report.capacity = CapacitySummary {
        free_bytes: device.free_bytes.unwrap_or(0),
        estimate_bytes: estimate,
        would_fit: device.free_bytes.is_some_and(|free| estimate <= free),
    };
    if device.free_bytes.is_none() {
        report
            .diagnostics
            .push("device capacity unknown — would-fit verdict unavailable".to_string());
    }

    // Deterministic ordering and derived counts.
    report.file_adds.sort_by_key(|a| a.itl_id);
    report.file_repairs.sort_by_key(|r| r.dbid);
    report.file_removes.sort_by_key(|r| r.dbid);
    report.device_only_tracks.sort_unstable();
    report.stat_diffs.sort_by_key(|d| (d.pid, d.field));
    for diff in &report.stat_diffs {
        match diff.field {
            StatField::Rating => report.stat_counts.rating += 1,
            StatField::PlayCount => report.stat_counts.play_count += 1,
            StatField::SkipCount => report.stat_counts.skip_count += 1,
            StatField::PlayDate => report.stat_counts.play_date += 1,
            StatField::VolumeAdjustment => report.stat_counts.volume_adjustment += 1,
        }
    }
    report.playlist_creates.sort_by_key(|c| c.host_pid);
    report.playlist_updates.sort_by_key(|u| u.device_id);
    report.playlist_removes.sort_by_key(|r| r.device_id);
    report.device_only_playlists.sort_by_key(|r| r.device_id);
    report
        .context
        .device_local_playlists
        .sort_by_key(|r| r.device_id);
    report.artwork_adds.sort_by_key(|a| a.pid);
    report.artwork_removes.sort_by_key(|a| a.pid);
    report.artwork_orphaned.sort_by_key(|a| a.pid);

    report
}

fn copy_mode(opts: &SyncOptions) -> CopyMode {
    if opts.transcode_aac {
        CopyMode::TranscodeToAac
    } else {
        CopyMode::Copy
    }
}

fn host_file_present(host: &HostInputs, itl_id: u32) -> bool {
    host.host_file_present.get(&itl_id).copied().unwrap_or(true)
}

/// The capacity estimate for one would-add/repair entry (owner formula,
/// decision §9-4): `Copy` = the exact host file size; `TranscodeToAac` =
/// duration × 32000 + embedded-cover bytes + a fixed moov overhead — a
/// conservative upper bound (real AAC 256 VBR lands at or below the
/// nominal rate).
fn size_estimate(
    track: &crate::mapping::HostTrack,
    mode: CopyMode,
    cover: Option<CoverEvidence>,
) -> u64 {
    match mode {
        CopyMode::Copy => u64::from(track.metadata.size_bytes),
        CopyMode::TranscodeToAac => {
            let cover_bytes = match cover {
                Some(CoverEvidence::Present { bytes }) => bytes,
                _ => 0,
            };
            (u64::from(track.metadata.duration_ms) / 1000) * AAC_BYTES_PER_SECOND
                + cover_bytes
                + MOOV_OVERHEAD_BYTES
        }
    }
}

/// Convert an on-pod colon-form path (`:iPod_Control:Music:F00:ABCD.mp3`)
/// to a filesystem-relative path. Pure string transform — no I/O.
pub fn device_fs_path(ipod_path: Option<&str>) -> PathBuf {
    let Some(path) = ipod_path else {
        return PathBuf::new();
    };
    let mut parts = path.split(':').filter(|p| !p.is_empty());
    let mut out = PathBuf::new();
    for part in &mut parts {
        if part == ".." || part.is_empty() {
            continue; // never a traversal into the report path
        }
        out.push(part);
    }
    out
}

/// Per-field raw stat differences on one matched pair. Play dates are
/// compared in unix-local space (the wrapper's `time_played` is already
/// libgpod-converted; the host raw Apple-epoch value is converted with the
/// library envelope offset — B-008 §6, the offset rides along and is never
/// silently assumed to be UTC). `0` on either side means "never played" and
/// only differs when the other side is nonzero.
fn stat_diffs(
    host: &crate::mapping::HostTrack,
    device: &ipod::TrackInfo,
    tz_offset: i32,
) -> Vec<StatDiff> {
    let pid = host.track_pid;
    let stats = &host.stats;
    let mut out = Vec::new();
    if u32::from(stats.rating) != device.rating {
        out.push(StatDiff {
            pid,
            field: StatField::Rating,
            host_raw: i64::from(stats.rating),
            device_raw: i64::from(device.rating),
        });
    }
    if stats.play_count != device.playcount {
        out.push(StatDiff {
            pid,
            field: StatField::PlayCount,
            host_raw: i64::from(stats.play_count),
            device_raw: i64::from(device.playcount),
        });
    }
    if stats.skip_count != device.skipcount {
        out.push(StatDiff {
            pid,
            field: StatField::SkipCount,
            host_raw: i64::from(stats.skip_count),
            device_raw: i64::from(device.skipcount),
        });
    }
    match (stats.play_date_raw == 0, device.time_played == 0) {
        (true, true) => {}
        (false, false) => {
            let host_unix = i64::from(stats.play_date_raw) - APPLE_TO_UNIX - i64::from(tz_offset);
            if host_unix != device.time_played {
                out.push(StatDiff {
                    pid,
                    field: StatField::PlayDate,
                    host_raw: host_unix,
                    device_raw: device.time_played,
                });
            }
        }
        (host_zero, _device_zero) => out.push(StatDiff {
            pid,
            field: StatField::PlayDate,
            host_raw: if host_zero {
                0
            } else {
                i64::from(stats.play_date_raw)
            },
            device_raw: device.time_played,
        }),
    }
    if stats.volume_adjustment != device.volume {
        out.push(StatDiff {
            pid,
            field: StatField::VolumeAdjustment,
            host_raw: i64::from(stats.volume_adjustment),
            device_raw: i64::from(device.volume),
        });
    }
    out
}

/// Sorted unique multiset difference `a − b`.
fn multiset_difference(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut counts: HashMap<u64, i64> = HashMap::new();
    for value in a {
        *counts.entry(*value).or_default() += 1;
    }
    for value in b {
        *counts.entry(*value).or_default() -= 1;
    }
    let mut out: Vec<u64> = counts
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(value, _)| value)
        .collect();
    out.sort_unstable();
    out
}

/// Device-local playlists (owner decision §9-3, evidence in
/// `deltas/b009_plan.md` §8 step 2a): iTunes writes `On-The-Go 1` as an
/// ordinary `mhyp` with all-zero header flags, so the name convention is
/// the only available signal; libgpod additionally injects `OTG Playlist N`
/// from the device `OTGPlaylistInfo` file at parse time. The master is
/// excluded upstream of this check.
pub fn is_device_local(name: Option<&str>) -> bool {
    name.is_some_and(|name| name.starts_with("On-The-Go") || name.starts_with("OTG Playlist"))
}

/// The criteria a planned playlist create would write: host rules
/// flattened to the device form. Unflattenable rules yield `None` plus a
/// diagnostic — never a silent static conversion (B-008 §7).
fn create_criteria(
    host_pl: &HostPlaylist,
    report: &mut DryRunReport,
) -> (PlaylistKind, Option<crate::mapping::SmartRules>) {
    let Some(smart) = &host_pl.smart else {
        return (PlaylistKind::Static, None);
    };
    let Some(rules) = &smart.rules else {
        return (PlaylistKind::Smart, None);
    };
    match rules {
        Ok(parsed) => match flatten::flatten(parsed) {
            Ok(flat) => (PlaylistKind::Smart, Some(flat)),
            Err(FlattenError::Unflattenable) => {
                report.diagnostics.push(format!(
                    "playlist {:?}: criteria have no observed device form — create would \
                     carry them unflattened",
                    host_pl.name
                ));
                (PlaylistKind::Smart, None)
            }
        },
        Err(error) => {
            report.diagnostics.push(format!(
                "playlist {:?}: unparseable criteria ({error})",
                host_pl.name
            ));
            (PlaylistKind::Smart, None)
        }
    }
}

/// Host-vs-device smart-criteria comparison: flatten the host rules, parse
/// the device blob, compare. Any gap (no blob, unparseable, unflattenable)
/// is `Unknown` — a diagnostic, never a silent match (B-008 §7).
fn criteria_state(
    host_pl: &HostPlaylist,
    device_pl: &ipod::PlaylistInfo,
    blobs: &HashMap<u64, Vec<u8>>,
) -> CriteriaState {
    let Some(host_smart) = &host_pl.smart else {
        return if device_pl.is_spl {
            CriteriaState::Unknown
        } else {
            CriteriaState::Equal
        };
    };
    if !device_pl.is_spl {
        return CriteriaState::Unknown;
    }
    let (Some(Ok(host_rules)), Some(blob)) = (&host_smart.rules, blobs.get(&device_pl.id)) else {
        return CriteriaState::Unknown;
    };
    let (Ok(flat), Ok(device_rules)) = (flatten::flatten(host_rules), parse_smart_rules(blob))
    else {
        return CriteriaState::Unknown;
    };
    if flat == device_rules {
        CriteriaState::Equal
    } else {
        CriteriaState::Differ
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::{HostStats, HostTrack, HostTrackMetadata};

    fn library() -> crate::mapping::HostLibrary {
        crate::mapping::HostLibrary {
            library_pid: 0xABCD,
            media_root: PathBuf::from("/media"),
            tz_offset_seconds: 3 * 3600,
        }
    }

    fn host_track(itl_id: u32, pid: u64, location: HostLocation) -> HostTrack {
        HostTrack {
            library_pid: 0xABCD,
            track_pid: pid,
            itl_id,
            location,
            metadata: HostTrackMetadata {
                size_bytes: 10_000_000,
                duration_ms: 200_000,
                ..Default::default()
            },
            stats: HostStats {
                rating: 80,
                play_count: 5,
                skip_count: 1,
                play_date_raw: APPLE_TO_UNIX as u32 + 3 * 3600 + 100,
                volume_adjustment: 0,
            },
            content_rating: 0,
        }
    }

    fn mapped_track(itl_id: u32, pid: u64) -> HostTrack {
        host_track(
            itl_id,
            pid,
            HostLocation::Mapped(PathBuf::from("/media/song.m4a")),
        )
    }

    fn device_track(dbid: u64, path: Option<&str>, has_thumbnail: bool) -> ipod::TrackInfo {
        ipod::TrackInfo {
            dbid,
            ipod_path: path.map(str::to_string),
            filetype: Some("m4a".to_string()),
            size: 10_000_000,
            rating: 80,
            playcount: 5,
            skipcount: 1,
            time_played: 100,
            volume: 0,
            has_thumbnail,
            ..Default::default()
        }
    }

    fn host_inputs(tracks: Vec<HostTrack>) -> HostInputs {
        HostInputs {
            library: Some(library()),
            tracks,
            playlists: Vec::new(),
            artwork: HashMap::new(),
            host_file_present: HashMap::new(),
        }
    }

    fn device_inputs(tracks: Vec<ipod::TrackInfo>) -> DeviceInputs {
        DeviceInputs {
            name: "iPod user".to_string(),
            model: "iPod Video".to_string(),
            total_bytes: Some(100_000_000),
            free_bytes: Some(50_000_000),
            tracks,
            playlists: Vec::new(),
            smart_blobs: HashMap::new(),
            artwork_status: ipod::ArtworkStatus {
                verdict: ipod::ArtworkVerdict::Ok,
                ..Default::default()
            },
            device_file_present: HashMap::new(),
            play_counts_present: None,
        }
    }

    fn opts() -> SyncOptions {
        SyncOptions {
            source_key: SourceKey("itunes".to_string()),
            transcode_aac: false,
            device_only: DeviceOnlyPolicy::Remove,
        }
    }

    #[test]
    fn missing_host_track_with_mapped_file_becomes_file_add() {
        let host = host_inputs(vec![mapped_track(7, 0xA1)]);
        let device = device_inputs(vec![]);
        let report = compute(&host, &device, &opts());
        assert_eq!(report.file_adds.len(), 1);
        let add = &report.file_adds[0];
        assert_eq!((add.itl_id, add.pid, add.mode), (7, 0xA1, CopyMode::Copy));
        assert_eq!(add.size_estimate, 10_000_000);
        assert!(report.file_repairs.is_empty());
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn transcode_mode_uses_the_owner_formula() {
        let mut host = host_inputs(vec![mapped_track(7, 0xA1)]);
        host.artwork
            .insert(0xA1, CoverEvidence::Present { bytes: 60_000 });
        let device = device_inputs(vec![]);
        let mut options = opts();
        options.transcode_aac = true;
        let report = compute(&host, &device, &options);
        let add = &report.file_adds[0];
        assert_eq!(add.mode, CopyMode::TranscodeToAac);
        // 200 s × 32000 + cover 60000 + moov 8192.
        assert_eq!(add.size_estimate, 200 * 32_000 + 60_000 + 8 * 1024);
    }

    #[test]
    fn missing_host_file_is_a_diagnostic_not_an_action() {
        let mut host = host_inputs(vec![mapped_track(7, 0xA1)]);
        host.host_file_present.insert(7, false);
        let report = compute(&host, &device_inputs(vec![]), &opts());
        assert!(report.file_adds.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("host file is missing"))
        );
    }

    #[test]
    fn cloud_and_outside_tracks_are_never_candidates() {
        let host = host_inputs(vec![
            host_track(1, 0xA1, HostLocation::NoLocation),
            host_track(2, 0xA2, HostLocation::OutsideMediaFolder),
        ]);
        let report = compute(&host, &device_inputs(vec![]), &opts());
        assert!(report.file_adds.is_empty());
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn no_identity_track_is_a_diagnostic() {
        let host = host_inputs(vec![mapped_track(3, 0)]);
        let report = compute(&host, &device_inputs(vec![]), &opts());
        assert!(report.file_adds.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("no persistent id"))
        );
    }

    #[test]
    fn missing_device_file_becomes_file_repair() {
        let mut host = host_inputs(vec![mapped_track(7, 0xA1)]);
        host.host_file_present.insert(7, true);
        let mut device = device_inputs(vec![device_track(
            0xA1,
            Some(":iPod_Control:Music:F00:A.mp3"),
            false,
        )]);
        device.device_file_present.insert(0xA1, false);
        let report = compute(&host, &device, &opts());
        assert_eq!(report.file_repairs.len(), 1);
        let repair = &report.file_repairs[0];
        assert_eq!(repair.dbid, 0xA1);
        assert_eq!(
            repair.device_path,
            PathBuf::from("iPod_Control/Music/F00/A.mp3")
        );
    }

    #[test]
    fn device_only_tracks_follow_the_policy() {
        let host = host_inputs(vec![]);
        let device = device_inputs(vec![
            device_track(0xA1, Some(":iPod_Control:Music:F00:A.mp3"), false),
            device_track(0xA2, Some(":iPod_Control:Music:F00:B.mp3"), false),
        ]);
        let report = compute(&host, &device, &opts());
        assert_eq!(report.file_removes.len(), 2);
        assert!(report.device_only_tracks.is_empty());

        let mut keep = opts();
        keep.device_only = DeviceOnlyPolicy::Keep;
        let report = compute(&host, &device, &keep);
        assert!(report.file_removes.is_empty());
        assert_eq!(report.device_only_tracks, vec![0xA1, 0xA2]);
    }

    #[test]
    fn stat_diffs_cover_all_fields_with_play_date_zero_semantics() {
        let mut host_track = mapped_track(7, 0xA1);
        host_track.stats = HostStats {
            rating: 60,
            play_count: 3,
            skip_count: 2,
            play_date_raw: 0,
            volume_adjustment: -50,
        };
        let device_track = ipod::TrackInfo {
            dbid: 0xA1,
            rating: 80,
            playcount: 5,
            skipcount: 1,
            time_played: 100,
            volume: 0,
            ..Default::default()
        };
        let host = host_inputs(vec![host_track]);
        let device = device_inputs(vec![device_track]);
        let report = compute(&host, &device, &opts());
        let fields: Vec<StatField> = report.stat_diffs.iter().map(|d| d.field).collect();
        assert_eq!(
            fields,
            vec![
                StatField::Rating,
                StatField::PlayCount,
                StatField::SkipCount,
                StatField::PlayDate,
                StatField::VolumeAdjustment,
            ]
        );
        assert_eq!(report.stat_counts.rating, 1);
        assert_eq!(report.stat_counts.play_date, 1);
    }

    #[test]
    fn equal_play_dates_are_not_a_diff() {
        // Host raw = unix + offset; device carries the converted value.
        let mut host_track = mapped_track(7, 0xA1);
        host_track.stats.play_date_raw = APPLE_TO_UNIX as u32 + 3 * 3600 + 100;
        let device_track = ipod::TrackInfo {
            dbid: 0xA1,
            rating: 80,
            playcount: 5,
            skipcount: 1,
            time_played: 100,
            ..Default::default()
        };
        let host = host_inputs(vec![host_track]);
        let device = device_inputs(vec![device_track]);
        let report = compute(&host, &device, &opts());
        assert!(
            !report
                .stat_diffs
                .iter()
                .any(|d| d.field == StatField::PlayDate)
        );
    }

    fn host_playlist(pid: u64, name: &str, smart: Option<(u32, i64)>) -> HostPlaylist {
        HostPlaylist {
            persistent_id: pid,
            name: Some(name.to_string()),
            smart: smart.map(|(field, value)| crate::mapping::HostSmartPlaylist {
                rules: Some(Ok(crate::mapping::SmartRules {
                    unk004: 0x0001_0001,
                    match_operator: 0,
                    rules: vec![crate::mapping::SmartRule::Group(
                        crate::mapping::SmartRules {
                            unk004: 0x0001_0001,
                            match_operator: 0,
                            rules: vec![crate::mapping::SmartRule::Leaf(
                                crate::mapping::SmartLeaf {
                                    field,
                                    action: 0x10,
                                    from_value: value,
                                    from_date: 0,
                                    from_units: 1,
                                    to_value: value,
                                    to_date: 0,
                                    to_units: 1,
                                },
                            )],
                        },
                    )],
                })),
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
        members: Vec<u64>,
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
            member_dbids: members,
        }
    }

    #[test]
    fn missing_playlist_becomes_create_with_members_and_kind() {
        let mut host = host_inputs(vec![mapped_track(1, 0xA1), mapped_track(2, 0xA2)]);
        host.playlists = vec![{
            let mut pl = host_playlist(10, "Плейлист", None);
            pl.member_itl_ids = vec![1, 2, 42];
            pl
        }];
        let report = compute(&host, &device_inputs(vec![]), &opts());
        assert_eq!(report.playlist_creates.len(), 1);
        let create = &report.playlist_creates[0];
        assert_eq!(create.host_pid, 10);
        assert_eq!(create.kind, PlaylistKind::Static);
        assert_eq!(create.member_pids, vec![Some(0xA1), Some(0xA2), None]);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("unresolvable members"))
        );
    }

    #[test]
    fn kind_mismatch_is_a_diagnostic_never_a_conversion() {
        let mut host = host_inputs(vec![]);
        host.playlists = vec![host_playlist(10, "Хай рейт", Some((0x19, 69)))];
        let device = DeviceInputs {
            playlists: vec![device_playlist(1, Some("Хай рейт"), false, false, vec![])],
            ..device_inputs(vec![])
        };
        let report = compute(&host, &device, &opts());
        assert!(report.playlist_updates.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("kind disagrees"))
        );
    }

    #[test]
    fn matched_playlist_reports_member_and_criteria_diffs() {
        let mut host = host_inputs(vec![
            mapped_track(1, 0xA1),
            mapped_track(2, 0xA2),
            mapped_track(3, 0xA3),
        ]);
        host.playlists = vec![{
            let mut pl = host_playlist(10, "Хай рейт", Some((0x19, 69)));
            pl.member_itl_ids = vec![1, 3];
            pl
        }];
        let mut device = device_inputs(vec![]);
        device.playlists = vec![device_playlist(
            1,
            Some("Хай рейт"),
            true,
            false,
            vec![0xA2, 0xA1],
        )];
        let report = compute(&host, &device, &opts());
        assert_eq!(report.playlist_updates.len(), 1);
        let update = &report.playlist_updates[0];
        assert_eq!(update.members_add, vec![0xA3]);
        assert_eq!(update.members_remove, vec![0xA2]);
        assert!(!update.members_reorder);
        // Host has rules, device carries no blob → criteria Unknown +
        // diagnostic, still reported as an update because members differ.
        assert_eq!(update.criteria, CriteriaState::Unknown);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("criteria comparison inconclusive"))
        );
    }

    #[test]
    fn reorder_only_change_is_reported_without_add_remove() {
        let mut host = host_inputs(vec![mapped_track(1, 0xA1), mapped_track(2, 0xA2)]);
        host.playlists = vec![{
            let mut pl = host_playlist(10, "Плейлист", None);
            pl.member_itl_ids = vec![2, 1];
            pl
        }];
        let mut device = device_inputs(vec![]);
        device.playlists = vec![device_playlist(
            1,
            Some("Плейлист"),
            false,
            false,
            vec![0xA1, 0xA2],
        )];
        let report = compute(&host, &device, &opts());
        assert_eq!(report.playlist_updates.len(), 1);
        let update = &report.playlist_updates[0];
        assert!(update.members_add.is_empty());
        assert!(update.members_remove.is_empty());
        assert!(update.members_reorder);
    }

    #[test]
    fn master_and_device_local_playlists_are_never_removed() {
        let host = host_inputs(vec![]);
        let device = DeviceInputs {
            playlists: vec![
                device_playlist(1, Some("iPod user"), false, true, vec![]),
                device_playlist(2, Some("On-The-Go 1"), false, false, vec![]),
                device_playlist(3, Some("Старое"), false, false, vec![]),
            ],
            ..device_inputs(vec![])
        };
        let report = compute(&host, &device, &opts());
        assert_eq!(report.context.device_master.as_ref().unwrap().device_id, 1);
        assert_eq!(report.context.device_local_playlists.len(), 1);
        assert_eq!(report.context.device_local_playlists[0].device_id, 2);
        assert_eq!(report.playlist_removes.len(), 1);
        assert_eq!(report.playlist_removes[0].device_id, 3);

        let mut keep = opts();
        keep.device_only = DeviceOnlyPolicy::Keep;
        let report = compute(&host, &device, &keep);
        assert!(report.playlist_removes.is_empty());
        assert_eq!(report.device_only_playlists.len(), 1);
    }

    #[test]
    fn artwork_follows_evidence_thumbnail_and_policy() {
        let mut host = host_inputs(vec![
            mapped_track(1, 0xA1),
            mapped_track(2, 0xA2),
            mapped_track(3, 0xA3),
        ]);
        host.artwork
            .insert(0xA1, CoverEvidence::Present { bytes: 10 });
        host.artwork.insert(0xA2, CoverEvidence::Absent);
        host.artwork.insert(0xA3, CoverEvidence::Unknown);
        let device = device_inputs(vec![
            device_track(0xA1, Some(":iPod_Control:Music:F00:A.mp3"), false),
            device_track(0xA2, Some(":iPod_Control:Music:F00:B.mp3"), true),
            device_track(0xA3, Some(":iPod_Control:Music:F00:C.mp3"), true),
        ]);
        let report = compute(&host, &device, &opts());
        assert_eq!(report.artwork_adds.len(), 1);
        assert_eq!(report.artwork_adds[0].pid, 0xA1);
        assert_eq!(report.artwork_removes.len(), 1);
        assert_eq!(report.artwork_removes[0].pid, 0xA2);
        assert_eq!(report.artwork_unknown, 1);

        let mut keep = opts();
        keep.device_only = DeviceOnlyPolicy::Keep;
        let report = compute(&host, &device, &keep);
        assert!(report.artwork_removes.is_empty());
        assert_eq!(report.artwork_orphaned.len(), 1);
    }

    #[test]
    fn artwork_section_is_gated_by_the_verdict() {
        let mut host = host_inputs(vec![mapped_track(1, 0xA1)]);
        host.artwork
            .insert(0xA1, CoverEvidence::Present { bytes: 10 });
        let device = DeviceInputs {
            artwork_status: ipod::ArtworkStatus {
                verdict: ipod::ArtworkVerdict::Missing,
                diagnostic: Some("ArtworkDB gone".to_string()),
                ..Default::default()
            },
            ..device_inputs(vec![device_track(
                0xA1,
                Some(":iPod_Control:Music:F00:A.mp3"),
                false,
            )])
        };
        let report = compute(&host, &device, &opts());
        assert!(report.artwork_adds.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("artwork section gated"))
        );
    }

    #[test]
    fn capacity_summarizes_adds_and_repairs() {
        let mut host = host_inputs(vec![mapped_track(1, 0xA1), mapped_track(2, 0xA2)]);
        host.host_file_present.insert(1, true);
        host.host_file_present.insert(2, true);
        let mut device = device_inputs(vec![device_track(
            0xA2,
            Some(":iPod_Control:Music:F00:B.mp3"),
            false,
        )]);
        device.device_file_present.insert(0xA2, false);
        let report = compute(&host, &device, &opts());
        // add for #1 (10 MB) + repair for #2 (10 MB) = 20 MB <= 50 MB.
        assert_eq!(report.capacity.estimate_bytes, 20_000_000);
        assert!(report.capacity.would_fit);
    }

    #[test]
    fn host_master_candidate_is_never_a_create() {
        let mut host = host_inputs(vec![]);
        host.playlists = vec![{
            let mut pl = host_playlist(1, "Library", None);
            pl.is_master_candidate = true;
            pl
        }];
        let device = DeviceInputs {
            playlists: vec![device_playlist(1, Some("iPod user"), false, true, vec![])],
            ..device_inputs(vec![])
        };
        let report = compute(&host, &device, &opts());
        assert!(report.playlist_creates.is_empty());
        assert_eq!(report.context.device_master.as_ref().unwrap().device_id, 1);
    }

    #[test]
    fn report_is_deterministic_for_shuffled_inputs() {
        let mut host = host_inputs(vec![
            mapped_track(1, 0xA1),
            mapped_track(2, 0xA2),
            mapped_track(3, 0xA3),
        ]);
        host.playlists = vec![{
            let mut pl = host_playlist(10, "Плейлист", None);
            pl.member_itl_ids = vec![1, 2, 3];
            pl
        }];
        let mut device = device_inputs(vec![
            device_track(0xA1, Some(":iPod_Control:Music:F00:A.mp3"), false),
            device_track(0xA2, Some(":iPod_Control:Music:F00:B.mp3"), false),
            device_track(0xA3, Some(":iPod_Control:Music:F00:C.mp3"), false),
        ]);
        device.playlists = vec![device_playlist(
            1,
            Some("Плейлист"),
            false,
            false,
            vec![0xA1, 0xA2],
        )];
        let a = compute(&host, &device, &opts());
        host.tracks.reverse();
        host.playlists[0].member_itl_ids.reverse();
        device.tracks.reverse();
        let b = compute(&host, &device, &opts());
        assert_eq!(a, b);
    }
}
