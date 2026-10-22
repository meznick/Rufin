//! Write-back into the `.itl`: one `itl-sync sync --json -` invocation per
//! batch of absolute-value patches. `itl-sync` remains the only writer of
//! `.itl` bytes — the source and the runtime flush service both go through
//! `write_patches`.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;

use super::ItunesSourceConfig;

/// Absolute-value patch for one track; `None` = leave the field untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItunesPatch {
    pub track_id: u32,
    pub rating: Option<u8>,
    pub play_count: Option<u32>,
    pub skip_count: Option<u32>,
    /// Real unix timestamp; `itl-sync` converts it to wall-clock with its TZ.
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

/// The `itl-sync` binary: `ITL_SYNC_BIN` environment variable, else the
/// `itl-sync` from `PATH`.
pub fn sync_binary() -> PathBuf {
    std::env::var_os("ITL_SYNC_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("itl-sync"))
}

/// Why a write-back batch failed.
#[derive(Debug)]
pub enum ItunesWriteError {
    Spawn(std::io::Error),
    Io(std::io::Error),
    Rejected(String),
}

impl std::fmt::Display for ItunesWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot run itl-sync: {error}"),
            Self::Io(error) => write!(f, "itl-sync io: {error}"),
            Self::Rejected(message) => write!(f, "itl-sync rejected the write: {message}"),
        }
    }
}

impl std::error::Error for ItunesWriteError {}

/// Write one batch of absolute-value patches into the `.itl` (atomic safe
/// write with rolling backups, inside `itl-sync`). Empty patches are
/// skipped; an empty batch is a no-op. The binary is resolved from
/// `ITL_SYNC_BIN`, else `itl-sync` on `PATH`.
pub async fn write_patches(
    config: &ItunesSourceConfig,
    patches: &[ItunesPatch],
) -> Result<(), ItunesWriteError> {
    write_patches_with(config, patches, &sync_binary()).await
}

/// `write_patches` with an explicit `itl-sync` binary path.
pub async fn write_patches_with(
    config: &ItunesSourceConfig,
    patches: &[ItunesPatch],
    binary: &std::path::Path,
) -> Result<(), ItunesWriteError> {
    let patches = patches
        .iter()
        .filter(|patch| !patch.is_empty())
        .collect::<Vec<_>>();
    if patches.is_empty() {
        return Ok(());
    }
    let tracks = patches
        .iter()
        .map(|patch| {
            let mut json = serde_json::json!({ "id": patch.track_id });
            let object = json.as_object_mut().expect("track patch object");
            if let Some(rating) = patch.rating {
                object.insert("rating".into(), rating.into());
            }
            if let Some(play_count) = patch.play_count {
                object.insert("play_count".into(), play_count.into());
            }
            if let Some(skip_count) = patch.skip_count {
                object.insert("skip_count".into(), skip_count.into());
            }
            if let Some(play_date) = patch.play_date_unix {
                object.insert("play_date".into(), play_date.into());
            }
            json
        })
        .collect::<Vec<_>>();
    let body = serde_json::json!({ "tracks": tracks }).to_string();

    let mut command = tokio::process::Command::new(binary);
    command.arg("sync").arg(&config.library).arg("--json").arg("-");
    if let Some(media_folder) = &config.media_folder {
        command.arg("--media-folder").arg(media_folder);
    }
    if let Some(tz) = config.tz_offset_seconds {
        command.arg("--tz-offset").arg(tz.to_string());
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(ItunesWriteError::Spawn)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body.as_bytes())
            .await
            .map_err(ItunesWriteError::Io)?;
    }
    let output = child.wait_with_output().await.map_err(ItunesWriteError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(ItunesWriteError::Rejected(
        stderr.trim().lines().next().unwrap_or("unknown error").to_string(),
    ))
}
