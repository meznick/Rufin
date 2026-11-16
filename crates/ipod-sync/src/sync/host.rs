//! B-010: the host write step (b010_plan §7, §5 step 4) — one
//! `itl_sync::write_library` call per operation, in-process, exactly like
//! the sources write-back path. The sentinel refusal is unconditional
//! inside the itl-sync pipeline; a refusal here aborts the sync **before**
//! any device write (import-first ordering, §5), so `Play Counts` is
//! never consumed without the imported values durably on the host.

use std::path::PathBuf;

use itl_sync::WriteOptions;
use itl_sync::sync::{PatchSet, TrackPatch};

use crate::sync::merge::HostStatPatch;

/// What the host step needs from the selected iTunes source (supplied by
/// the sync boundary, B-015; never discovered by the engine itself).
#[derive(Clone, Debug)]
pub struct HostWriteContext {
    pub library_path: PathBuf,
    pub media_root: PathBuf,
    pub tz_offset_seconds: Option<i32>,
}

/// The host write was refused or failed — a partial result, never a
/// completed sync.
#[derive(Debug)]
pub struct HostWriteError(pub String);

impl std::fmt::Display for HostWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host .itl write refused: {}", self.0)
    }
}

impl std::error::Error for HostWriteError {}

/// Write the merged patches into the host `.itl` through itl-sync's
/// single atomic write path (sentinel-guarded, rolling backups,
/// self-validation). An empty patch set is a no-op. A patch for a track
/// id the library does not hold is a refusal — the caller keeps the
/// operation dirty instead of pretending the value landed.
pub fn write_host_patches(
    context: &HostWriteContext,
    patches: &[HostStatPatch],
) -> Result<(), HostWriteError> {
    if patches.is_empty() {
        return Ok(());
    }
    let mut set = PatchSet::default();
    for patch in patches {
        let track = TrackPatch {
            rating: None,
            play_count: patch.play_count,
            skip_count: patch.skip_count,
            play_date_unix: patch.play_date_unix,
            explicit: None,
        };
        set.insert_track(track, patch.itl_id)
            .map_err(|error| HostWriteError(error.to_string()))?;
    }
    let options = WriteOptions {
        tz_offset_seconds: context.tz_offset_seconds,
        media_root: Some(context.media_root.clone()),
        ..WriteOptions::default()
    };
    let report = itl_sync::write_library(&context.library_path, &set, &options)
        .map_err(|error| HostWriteError(error.to_string()))?;
    if !report.missing_tracks.is_empty() {
        return Err(HostWriteError(format!(
            "tracks missing from the library: {:?}",
            report.missing_tracks
        )));
    }
    Ok(())
}
