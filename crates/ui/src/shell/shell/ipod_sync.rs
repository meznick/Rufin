//! B-015: the shell-side sync boundary (b015_plan §3.3).
//!
//! The boundary is **stateless by design**: plain config data captured
//! at shell build (the D-4 rule — exactly one configured iTunes
//! source; none or several refuse at prepare with a diagnostic), the
//! sources crate's Lofty-based cover probe, and the B-010a transcode.
//! `artwork` returns `None` for now: the pid → encoded-cover mapping
//! needs the runtime catalog, and the B-010 gate shipped the same
//! `None` provider — the write side gains covers with a follow-up
//! that reaches the catalog.

use std::path::Path;
use std::sync::Arc;

use gtk_library::ipod_sync_service::{
    BoundarySource, EngineRunner, IpodSyncService, SyncBoundary, review_policy,
};
use ipod::{ArtworkInput, BackendSearch};
use ipod_sync::dry_run::{CoverEvidence, SourceKey};
use sources::itunes::{ITUNES_SOURCE_ID, ItunesSource};

/// The production boundary over the configured iTunes source.
pub struct ShellSyncBoundary {
    sources: Vec<BoundarySource>,
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
        Self { sources }
    }

    /// The production service: the engine runner over this boundary,
    /// with the S-08-delta policy (dev reviews, release runs silently).
    pub fn service(self) -> IpodSyncService {
        IpodSyncService::new(
            Arc::new(EngineRunner::new(Arc::new(self), BackendSearch::FromEnv)),
            review_policy(),
        )
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

    fn artwork(&self, _pid: u64) -> Option<ArtworkInput> {
        // The pid → encoded-cover mapping needs the runtime catalog —
        // the B-010 gate shipped the same `None` provider (see the
        // module docs).
        None
    }

    fn transcode(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        let uri = gstreamer::glib::filename_to_uri(source, None)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        audio_processing::transcode_to_m4a(
            &audio_processing::ResolvedStream::new(uri.as_str()),
            256,
            destination,
            || false,
        )
    }
}
