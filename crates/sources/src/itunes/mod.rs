//! iTunes library (`.itl`) source: the catalog is read from the iTunes
//! database itself, and stats (rating, play/skip counts, last played) sync
//! both ways through the `itl-sync` CLI (the only writer of `.itl` bytes).

pub mod catalog;
pub mod write;

use std::path::{Path, PathBuf};

use crate::config::{decode_provider_payload, encode_provider_payload, require_payload_version};
use crate::{SourceConfiguration, SourceError, SourceId, SourceResult};

pub const ITUNES_SOURCE_ID: &str = "itunes";
const SOURCE_CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ItunesSourceConfig {
    /// Path to `iTunes Library.itl`.
    pub library: PathBuf,
    /// Local root of the guest media folder (the iTunes "Music Folder"
    /// location as seen on this host). Default: the parent of the `.itl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_folder: Option<PathBuf>,
    /// Timezone offset for wall-clock ↔ unix conversions. Default: the
    /// offset stored in the `.itl` envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz_offset_seconds: Option<i32>,
}

/// The editable subset of [`ItunesSourceConfig`] shown in the source settings
/// form. Plain strings only, so it travels over the bindings layer (uniffi)
/// without path-type plumbing.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "bindings", derive(uniffi::Record))]
pub struct ItunesEditableSettings {
    /// `iTunes Library.itl` path. Empty = the form has no library yet.
    pub library: String,
    /// Optional media-folder root override; empty in the form = default.
    pub media_folder: Option<String>,
}

#[derive(serde::Deserialize)]
struct ItunesSourcePayload {
    version: u32,
    library: PathBuf,
    #[serde(default)]
    media_folder: Option<PathBuf>,
    #[serde(default)]
    tz_offset_seconds: Option<i32>,
}

impl ItunesSourceConfig {
    /// Whether iTunes currently has the library open, per its own `sentinel`
    /// busy marker (verified semantics: zeroed = open, token = closed). The
    /// authoritative check happens inside `itl-sync` on every write; this is
    /// the cheap in-process pre-check used for user-facing notices.
    pub fn library_busy(&self) -> bool {
        matches!(
            itl_sync::sentinel_state(&self.library),
            Ok(itl_sync::SentinelState::Busy)
        )
    }

    pub fn from_configuration(stored: &SourceConfiguration) -> SourceResult<Self> {
        if stored.kind != ITUNES_SOURCE_ID {
            return Err(SourceError::InvalidConfig(format!(
                "expected {ITUNES_SOURCE_ID}, found {}",
                stored.kind
            )));
        }
        let payload: ItunesSourcePayload = decode_provider_payload(stored)?;
        require_payload_version(payload.version, SOURCE_CONFIG_VERSION)?;
        if payload.library.as_os_str().is_empty() {
            return Err(SourceError::InvalidConfig(
                "iTunes library path is empty".to_string(),
            ));
        }
        Ok(Self {
            library: payload.library,
            media_folder: payload.media_folder,
            tz_offset_seconds: payload.tz_offset_seconds,
        })
    }

    pub(crate) fn into_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "version": SOURCE_CONFIG_VERSION,
            "library": self.library,
            "media_folder": self.media_folder,
            "tz_offset_seconds": self.tz_offset_seconds,
        })
    }

    /// Local root of the guest media folder: the configured media folder,
    /// else the parent directory of the `.itl`.
    pub fn local_root(&self) -> PathBuf {
        self.media_folder.clone().unwrap_or_else(|| {
            self.library
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        })
    }

    /// Timezone offset for Apple-epoch ↔ unix conversions; falls back to
    /// the offset stored in the `.itl` envelope.
    pub fn tz_offset(&self, itl: &itl_core::ItlFile) -> i32 {
        self.tz_offset_seconds
            .unwrap_or_else(|| itl.tz_offset_seconds())
    }
}

pub struct ItunesSource {
    config: ItunesSourceConfig,
}

impl ItunesSource {
    pub fn from_configuration(configuration: &SourceConfiguration) -> SourceResult<Self> {
        Ok(Self {
            config: ItunesSourceConfig::from_configuration(configuration)?,
        })
    }

    pub fn config(&self) -> ItunesSourceConfig {
        self.config.clone()
    }

    pub(crate) async fn acquire_catalog(
        &self,
        catalog: &mut library::Catalog<'_>,
        progress: &(dyn Fn(crate::SourceReadProgress) + Send + Sync),
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> SourceResult<()> {
        catalog::acquire(self, catalog, progress, cancelled).await
    }

    /// Stream the locally mapped file. The observation stored the host path
    /// in `tracks.source_path`; `database.track` reads it back.
    pub(crate) async fn stream(
        &self,
        database: &library::Database,
        media_uri: &str,
    ) -> SourceResult<audio_processing::ResolvedStream> {
        let track = database
            .track(media_uri, &library::ReadCancellation::new())
            .await?
            .ok_or(SourceError::NotFound)?;
        let path = track.access.source_path.ok_or(SourceError::NotFound)?;
        let uri = url::Url::from_file_path(&path)
            .map_err(|_| SourceError::NotFound)?
            .to_string();
        Ok(audio_processing::ResolvedStream::new(uri))
    }
}

/// Encode a configured iTunes source. The media folder defaults to the
/// parent directory of the `.itl`; the TZ offset defaults to the `.itl`
/// envelope value (leave `None` for both unless the user overrides them).
pub fn connect(
    source_id: SourceId,
    library: PathBuf,
    media_folder: Option<PathBuf>,
    tz_offset_seconds: Option<i32>,
) -> SourceResult<SourceConfiguration> {
    if library.as_os_str().is_empty() {
        return Err(SourceError::InvalidRequest(
            "iTunes library path is empty",
        ));
    }
    let config = ItunesSourceConfig {
        library,
        media_folder,
        tz_offset_seconds,
    };
    Ok(encode_provider_payload(
        source_id,
        ITUNES_SOURCE_ID,
        "iTunes",
        config.into_payload(),
    ))
}
