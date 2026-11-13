//! B-009: input assembly — the only I/O of the dry run
//! (`deltas/b009_plan.md` §5).
//!
//! The collectors gather the owned inputs the pure engine consumes: host
//! records from a disposable `.itl` copy (never the live file — the caller
//! opens the copy), device records through the read-only wrapper surface
//! plus filesystem probes. Everything here is a read; nothing calls a
//! libgpod write/init/stat-import API.

use std::collections::HashMap;
use std::path::Path;

use itl_core::ItlFile;
use itl_sync::paths::MediaMapping;

use crate::mapping::{HostLocation, host_library, host_playlists, host_tracks};
use crate::retrieval::DeviceFacts;

use super::compute::device_fs_path;
use super::model::{CoverEvidence, DeviceInputs, HostInputs};

/// Caller-provided host artwork evidence (owner decision §9-1): the
/// production scanner lives in `crates/sources`; the sync entry point
/// (B-010+) wires it. Tests supply fixtures or a `lofty`-based probe (the
/// same parser `sources` uses — its `inspect_embedded` is crate-private).
pub trait ArtworkProbe {
    fn cover(&self, path: &Path) -> CoverEvidence;
}

/// Assemble the host inputs from an open `.itl` copy, the media root and
/// an artwork probe. Pure reads: `ItlFile` accessors, the `itl-sync` media
/// mapping, file metadata. A library without a media-folder mapping yields
/// `HostInputs { library: None, .. }` — the engine reports it as
/// non-actionable.
pub fn collect_host(itl: &ItlFile, media_root: &Path, probe: &dyn ArtworkProbe) -> HostInputs {
    let library = host_library(itl, media_root);
    let mut tracks = Vec::new();
    let mut host_file_present = HashMap::new();
    let mut artwork = HashMap::new();
    if let (Some(mapping), Some(lib)) = (MediaMapping::new(itl, media_root), &library) {
        tracks = host_tracks(itl, &mapping, lib.library_pid);
        for track in &tracks {
            if let HostLocation::Mapped(path) = &track.location {
                host_file_present.insert(track.itl_id, path.is_file());
                artwork.insert(track.track_pid, probe.cover(path));
            }
        }
    }
    HostInputs {
        library,
        tracks,
        playlists: host_playlists(itl),
        artwork,
        host_file_present,
    }
}

/// The on-pod `Play Counts` file, mount-relative (presence-only — the
/// import semantics are B-010's).
const PLAY_COUNTS_REL: &str = "iPod_Control/iTunes/Play Counts";

/// Assemble the device inputs from an opened read-only session, the mount
/// it was opened on and the already-collected facts. Read-side wrapper
/// calls plus filesystem probes only — no libgpod write surface.
pub fn collect_device(
    session: &ipod::DeviceSession,
    mount: &Path,
    facts: &DeviceFacts,
) -> Result<DeviceInputs, ipod::IpodError> {
    let tracks = session.tracks()?;
    let playlists = session.playlists()?;
    let smart_blobs = session.smart_blobs()?;
    let artwork_status = session.artwork_status()?;
    let device_file_present = probe_device_files(mount, &tracks);
    let play_counts = mount.join(PLAY_COUNTS_REL);
    let (total_bytes, free_bytes) = match (facts.total_capacity, facts.used_capacity) {
        (Some(total), Some(used)) => (Some(total), Some(total.saturating_sub(used))),
        _ => (None, None),
    };
    Ok(DeviceInputs {
        name: facts.name.clone(),
        model: facts.model.clone(),
        total_bytes,
        free_bytes,
        tracks,
        playlists,
        smart_blobs,
        artwork_status,
        device_file_present,
        play_counts_present: play_counts.exists().then_some(play_counts),
    })
}

/// Probe whether each track's on-pod file exists under the mount.
/// `false` means the database lists the track but the file is gone (the
/// repair signal); a track without a path record is not probed and counts
/// as present (an unprobed file never fabricates a repair).
pub(crate) fn probe_device_files(mount: &Path, tracks: &[ipod::TrackInfo]) -> HashMap<u64, bool> {
    let mut present = HashMap::new();
    for track in tracks {
        if let Some(path) = track.ipod_path.as_deref() {
            let fs_path = device_fs_path(Some(path));
            if !fs_path.as_os_str().is_empty() {
                present.insert(track.dbid, mount.join(fs_path).is_file());
            }
        }
    }
    present
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct FixedProbe(CoverEvidence);

    impl ArtworkProbe for FixedProbe {
        fn cover(&self, _path: &Path) -> CoverEvidence {
            self.0
        }
    }

    #[test]
    fn probe_detects_present_and_missing_device_files() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("iPod_Control/Music/F00");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("ABCD.mp3"), b"x").unwrap();
        let tracks = vec![
            ipod::TrackInfo {
                dbid: 1,
                ipod_path: Some(":iPod_Control:Music:F00:ABCD.mp3".to_string()),
                ..Default::default()
            },
            ipod::TrackInfo {
                dbid: 2,
                ipod_path: Some(":iPod_Control:Music:F00:GONE.mp3".to_string()),
                ..Default::default()
            },
            ipod::TrackInfo {
                dbid: 3,
                ipod_path: None,
                ..Default::default()
            },
        ];
        let present = probe_device_files(dir.path(), &tracks);
        assert_eq!(present.get(&1), Some(&true));
        assert_eq!(present.get(&2), Some(&false));
        assert!(!present.contains_key(&3), "no path record → counts as present");
    }

    #[test]
    fn probe_trait_fixture_returns_fixed_evidence() {
        let probe = FixedProbe(CoverEvidence::Present { bytes: 42 });
        assert_eq!(
            probe.cover(Path::new("/whatever.mp3")),
            CoverEvidence::Present { bytes: 42 }
        );
    }

    #[test]
    fn play_counts_path_relative_form() {
        let mount = PathBuf::from("/mnt/ipod");
        assert_eq!(
            mount.join(PLAY_COUNTS_REL),
            PathBuf::from("/mnt/ipod/iPod_Control/iTunes/Play Counts")
        );
    }
}
