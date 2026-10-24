//! Write-back into the `.itl`: one `itl_sync::write_library` call per batch
//! of absolute-value patches, in-process — the fork links `itl-sync` as a
//! library, the player never spawns a binary. One invocation = one atomic
//! write through the whole safe-write pipeline (flock, optimistic lock,
//! unconditional sentinel busy refusal, /proc handle check, self-validation,
//! rolling backup). The runtime flush service and the catalog merge both go
//! through `write_patches`.

use super::ItunesSourceConfig;
use itl_sync::sync::{PatchSet, TrackPatch};
use itl_sync::WriteOptions;

/// Absolute-value patch for one track; `None` = leave the field untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItunesPatch {
    pub track_id: u32,
    pub rating: Option<u8>,
    pub play_count: Option<u32>,
    pub skip_count: Option<u32>,
    /// Real unix timestamp; the write converts it to wall-clock with the
    /// source's TZ (config override or the `.itl` envelope value).
    pub play_date_unix: Option<i64>,
}

impl ItunesPatch {
    pub fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.play_count.is_none()
            && self.skip_count.is_none()
            && self.play_date_unix.is_none()
    }
}

/// Why a write-back batch failed.
#[derive(Debug)]
pub enum ItunesWriteError {
    Rejected(String),
}

impl std::fmt::Display for ItunesWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => write!(f, "iTunes write-back refused: {message}"),
        }
    }
}

impl std::error::Error for ItunesWriteError {}

/// Write one batch of absolute-value patches into the `.itl` in-process.
/// Empty patches are skipped; an empty batch is a no-op. The library is
/// never touched while iTunes holds it open — the sentinel refusal is
/// authoritative inside the write pipeline.
pub async fn write_patches(
    config: &ItunesSourceConfig,
    patches: &[ItunesPatch],
) -> Result<(), ItunesWriteError> {
    let patches = patches
        .iter()
        .filter(|patch| !patch.is_empty())
        .collect::<Vec<_>>();
    if patches.is_empty() {
        return Ok(());
    }

    let mut set = PatchSet::default();
    for patch in patches {
        let track = TrackPatch {
            rating: patch.rating,
            play_count: patch.play_count,
            skip_count: patch.skip_count,
            play_date_unix: patch.play_date_unix,
            explicit: None,
        };
        set.insert_track(track, patch.track_id)
            .map_err(ItunesWriteError::Rejected)?;
    }

    // Blocking file I/O (fingerprint, parse, flock, /proc scan, write) —
    // off the async runtime, same as the read path does with `ItlFile::open`.
    let library = config.library.clone();
    let options = WriteOptions {
        tz_offset_seconds: config.tz_offset_seconds,
        media_root: config.media_folder.clone(),
        ..WriteOptions::default()
    };
    let report = tokio::task::spawn_blocking(move || itl_sync::write_library(&library, &set, &options))
        .await
        .map_err(|e| ItunesWriteError::Rejected(format!("write task failed: {e}")))?
        .map_err(|e| ItunesWriteError::Rejected(e.to_string()))?;

    // A patch for an id the library does not hold is a silent no-op inside
    // the write pipeline (empty change set, successful exit). Treat it as a
    // refusal: the caller keeps the batch dirty instead of marking the sync
    // base as written for a value that never reached the `.itl`.
    if !report.missing_tracks.is_empty() {
        return Err(ItunesWriteError::Rejected(format!(
            "tracks missing from the library: {:?}",
            report.missing_tracks
        )));
    }
    Ok(())
}
