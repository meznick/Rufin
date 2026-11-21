//! B-015: the shell-side sync boundary (b015_plan §3.3).
//!
//! The boundary is **stateless by design**: plain config data captured
//! at shell build (the D-4 rule — exactly one configured iTunes
//! source; none or several refuse at prepare with a diagnostic), the
//! sources crate's Lofty-based cover probe, and the B-010a transcode.
//! The artwork provider resolves the pid → host path lazily from the
//! configured `.itl` itself (the runtime catalog isn't available to
//! the boundary) and reads the embedded cover the same way `cover()`
//! does — the B-017 A3 enabler.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::library::ipod_sync_service::{
    BoundarySource, EngineRunner, IpodSyncService, SyncBoundary,
};
use ipod::{ArtworkInput, BackendSearch};
use ipod_sync::dry_run::{CoverEvidence, SourceKey};
use sources::itunes::{ITUNES_SOURCE_ID, ItunesSource};

/// The pid → local-path resolution for the artwork provider: every
/// located track in the library, mapped with the same media-folder
/// machinery the sources crate's catalog uses. `None` when the library
/// fails to open or has no media mapping.
fn resolve_host_paths(library: &Path, media_root: &Path) -> Option<HashMap<u64, PathBuf>> {
    let itl = itl_core::ItlFile::open(library).ok()?;
    let mapping = itl_sync::paths::MediaMapping::new(&itl, media_root)?;
    let mut paths = HashMap::new();
    for track in itl_sync::survey::unique_tracks(&itl) {
        if let itl_sync::paths::MappedLocation::Mapped { path, .. } = mapping.to_local(track) {
            paths.insert(track.persistent_id(), path);
        }
    }
    Some(paths)
}

/// The production boundary over the configured iTunes source.
pub struct ShellSyncBoundary {
    sources: Vec<BoundarySource>,
    /// Lazy pid → host-path map, built on the first artwork request —
    /// which lands on the sync thread, so the `.itl` parse stays off
    /// the UI thread.
    host_paths: OnceLock<Option<HashMap<u64, PathBuf>>>,
}

impl ShellSyncBoundary {
    /// Capture every configured iTunes source's engine inputs.
    pub fn from_configured(configured: &[sources::SourceConfiguration]) -> Self {
        let sources = configured
            .iter()
            .filter(|source| source.kind == ITUNES_SOURCE_ID)
            .filter_map(|source| {
                let itunes = ItunesSource::from_configuration(source).ok()?;
                let config = itunes.config();
                Some(BoundarySource {
                    library: config.library.clone(),
                    media_root: config.media_folder.clone().unwrap_or_else(|| {
                        config
                            .library
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_default()
                    }),
                    tz_offset: config.tz_offset_seconds,
                    source_key: SourceKey(source.source_id.as_str().to_string()),
                })
            })
            .collect();
        Self {
            sources,
            host_paths: OnceLock::new(),
        }
    }

    /// The production service: the engine runner over this boundary.
    /// The Sync click is the approval — the page's live preview is the
    /// review (B-016).
    pub fn service(self) -> IpodSyncService {
        IpodSyncService::new(Arc::new(EngineRunner::new(
            Arc::new(self),
            BackendSearch::FromEnv,
        )))
    }
}

impl SyncBoundary for ShellSyncBoundary {
    fn source(&self) -> Result<BoundarySource, String> {
        match self.sources.as_slice() {
            [] => Err("no iTunes source configured — add one in the settings".to_string()),
            [source] => Ok(source.clone()),
            _ => Err(
                "several iTunes sources configured — source selection is not implemented yet"
                    .to_string(),
            ),
        }
    }

    fn cover(&self, path: &Path) -> CoverEvidence {
        match sources::inspect_embedded_cover(path) {
            Some(cover) => CoverEvidence::Present {
                bytes: cover.bytes.len() as u64,
            },
            // No readable picture (or an unreadable file) — treated as
            // absent; the engine's Unknown arm stays for a finer probe
            // later.
            None => CoverEvidence::Absent,
        }
    }

    fn artwork(&self, pid: u64) -> Option<ArtworkInput> {
        let source = self.source().ok()?;
        let paths = self
            .host_paths
            .get_or_init(|| resolve_host_paths(&source.library, &source.media_root))
            .as_ref()?;
        let cover = sources::inspect_embedded_cover(paths.get(&pid)?)?;
        Some(ArtworkInput {
            encoded: cover.bytes,
            rotation: cover.rotation,
        })
    }

    fn transcode(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        let uri = glib::filename_to_uri(source, None)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        audio_processing::transcode_to_m4a(
            &audio_processing::ResolvedStream::new(uri.as_str()),
            256,
            destination,
            || false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env-gated corpus (the same default the sources crate's
    /// iTunes catalog tests use): the full production snapshot.
    fn test_library() -> Option<PathBuf> {
        std::env::var_os("ITL_TEST_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                let default = PathBuf::from("/nvme2tb/iTunesACCEPT/iTunes Library.itl");
                default.exists().then_some(default)
            })
    }

    /// The A3 pin: the boundary's artwork resolution maps the located
    /// tracks to paths that exist — every mapped track must resolve, or
    /// an add's cover silently drops at sync time.
    #[test]
    fn resolve_host_paths_maps_every_located_track() {
        let Some(library) = test_library() else {
            // Env-gated; the crate's lint profile denies stderr printing,
            // so the skip is silent.
            return;
        };
        let media_root = library.parent().unwrap().to_path_buf();
        let paths = resolve_host_paths(&library, &media_root)
            .expect("the snapshot library must open and map");
        assert!(!paths.is_empty(), "the library has located tracks");
        for (pid, path) in &paths {
            assert!(path.exists(), "track {pid} maps to a missing file {path:?}");
        }
    }
}
