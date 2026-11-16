//! B-010a: the transcode decision and staging lifecycle
//! (`deltas/b010a_plan.md` §3.1/§3.3).
//!
//! The device sync writes files like the owner's iTunes (owner decision,
//! `deltas/b010_plan.md` §11-2): **non-MP3 tracks with a bitrate above
//! 256 kbps are transcoded to AAC 256**, MP3 files are never transcoded,
//! non-MP3 at or below 256 kbps are copied verbatim, and PDF Digital
//! Booklets are never copied at all (b010_plan §11-5).
//!
//! Everything here is pure data and path logic — no GStreamer, no I/O
//! beyond the staging helpers' own filesystem calls. The encoder itself is
//! caller-supplied (`crates/audio-processing` provides it; the B-010
//! executor wires it), so this crate keeps its production dependency
//! graph read-side.

use std::io;
use std::path::{Path, PathBuf};

/// What a sync does with a host file on the way to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileWriteMode {
    /// PDF Digital Booklets — iTunes never copies them (b010_plan §11-5).
    NeverCopy,
    /// Copy the file as-is (MP3, or non-MP3 at or below 256 kbps).
    CopyVerbatim,
    /// Transcode to AAC 256 before copying (non-MP3 above 256 kbps).
    TranscodeToAac256,
}

/// The [`decide_file_write`] outcome: the mode plus an optional finding
/// attached to the non-obvious arms (the conservative unknown-bitrate
/// default). Diagnostics are findings, never failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWriteDecision {
    pub mode: FileWriteMode,
    pub diagnostic: Option<String>,
}

/// The AAC target the owner's iTunes setting implies and the B-010a
/// encoder writes.
pub const TRANSCODE_TARGET_KBPS: u32 = 256;

/// Lossless host codecs — their nominal bitrate is above the target by
/// definition, so the rule's literal form and the codec-family form agree.
const LOSSLESS: &[&str] = &["alac", "flac", "wav", "aiff"];

/// The pure transcode trigger (b010a_plan §3.1):
///
/// - PDF booklets → [`FileWriteMode::NeverCopy`];
/// - MP3 → [`FileWriteMode::CopyVerbatim`], always (owner rule);
/// - lossless (ALAC/FLAC/WAV/AIFF) → [`FileWriteMode::TranscodeToAac256`];
/// - other non-MP3 with bitrate above 256 kbps → transcode; at or below →
///   verbatim (no lossy→lossy re-encode);
/// - unknown bitrate on a non-MP3 lossy → transcode with a diagnostic
///   (conservative default: an oversized copy is worse than the rare
///   mis-transcode).
pub fn decide_file_write(filetype: Option<&str>, bitrate_kbps: Option<u32>) -> FileWriteDecision {
    let ext = filetype
        .map(str::trim)
        .unwrap_or("")
        .trim_start_matches('.')
        .to_ascii_lowercase();

    let plain = |mode| FileWriteDecision {
        mode,
        diagnostic: None,
    };
    if ext == "pdf" {
        return plain(FileWriteMode::NeverCopy);
    }
    if ext == "mp3" {
        return plain(FileWriteMode::CopyVerbatim);
    }
    if LOSSLESS.contains(&ext.as_str()) {
        return plain(FileWriteMode::TranscodeToAac256);
    }
    match bitrate_kbps {
        Some(bitrate) if bitrate > TRANSCODE_TARGET_KBPS => plain(FileWriteMode::TranscodeToAac256),
        Some(_) => plain(FileWriteMode::CopyVerbatim),
        None => FileWriteDecision {
            mode: FileWriteMode::TranscodeToAac256,
            diagnostic: Some(
                "host bitrate unknown for a non-MP3 file; transcoding conservatively".to_string(),
            ),
        },
    }
}

/// The staging directory name prefix under the system temp dir. The
/// B-010 executor sweeps this subtree for orphans at sync start.
pub const STAGING_PREFIX: &str = "rufin-ipod-sync";

/// The per-device staging root: `<temp_base>/rufin-ipod-sync/<device-id>/`
/// (`device-id` = the b010_plan §5 identity: FireWire ID when present,
/// else master-pid+model hash).
pub fn staging_root(temp_base: &Path, device_id: &str) -> PathBuf {
    temp_base.join(STAGING_PREFIX).join(device_id)
}

/// Create the staging directory (idempotent).
pub fn create_staging_dir(root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(root)
}

/// The deterministic staging path for one track: `<root>/<dbid>.m4a`.
/// dbid = the host track PID (the value the created device track carries),
/// so a retry encodes into the same path and never duplicates.
pub fn staging_path_for(root: &Path, dbid: u64) -> PathBuf {
    root.join(format!("{dbid}.m4a"))
}

/// Remove the per-device staging directory — success, abort and recovery
/// all call this; a missing directory is not an error.
pub fn cleanup_staging(root: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Sweep this device's staging subtree at sync start (orphaned temp
/// encodes from a crashed run). Only the device's own directory is
/// touched — other devices' staging is never recursed into.
pub fn sweep_orphans(temp_base: &Path, device_id: &str) -> io::Result<()> {
    cleanup_staging(&staging_root(temp_base, device_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(mode: FileWriteMode) -> FileWriteDecision {
        FileWriteDecision {
            mode,
            diagnostic: None,
        }
    }

    #[test]
    fn decision_matrix() {
        // PDF booklets are never copied, regardless of bitrate.
        assert_eq!(
            decide_file_write(Some("pdf"), Some(64)),
            plain(FileWriteMode::NeverCopy)
        );
        assert_eq!(
            decide_file_write(Some("PDF"), None),
            plain(FileWriteMode::NeverCopy)
        );
        // MP3 is never transcoded, even above the target (owner rule).
        assert_eq!(
            decide_file_write(Some("mp3"), Some(320)),
            plain(FileWriteMode::CopyVerbatim)
        );
        assert_eq!(
            decide_file_write(Some(".MP3"), Some(128)),
            plain(FileWriteMode::CopyVerbatim)
        );
        // Lossless is transcoded by codec family — bitrate irrelevant.
        for lossless in ["alac", "flac", "wav", "aiff"] {
            assert_eq!(
                decide_file_write(Some(lossless), None),
                plain(FileWriteMode::TranscodeToAac256),
                "{lossless}"
            );
        }
        // Non-MP3 lossy: above the target transcodes, at or below copies.
        assert_eq!(
            decide_file_write(Some("m4a"), Some(320)),
            plain(FileWriteMode::TranscodeToAac256)
        );
        assert_eq!(
            decide_file_write(Some("m4a"), Some(256)),
            plain(FileWriteMode::CopyVerbatim)
        );
        assert_eq!(
            decide_file_write(Some("ogg"), Some(128)),
            plain(FileWriteMode::CopyVerbatim)
        );
        // Unknown bitrate on non-MP3 lossy: conservative transcode with a
        // diagnostic.
        let unknown = decide_file_write(Some("m4a"), None);
        assert_eq!(unknown.mode, FileWriteMode::TranscodeToAac256);
        assert!(unknown.diagnostic.is_some());
        // Unknown filetype with a known bitrate still follows the rule.
        assert_eq!(
            decide_file_write(None, Some(320)),
            plain(FileWriteMode::TranscodeToAac256)
        );
        assert_eq!(
            decide_file_write(Some(""), Some(192)),
            plain(FileWriteMode::CopyVerbatim)
        );
    }

    #[test]
    fn staging_paths_are_deterministic_and_scoped() {
        let temp = Path::new("/tmp/x");
        let root = staging_root(temp, "device-1");
        assert_eq!(root, PathBuf::from("/tmp/x/rufin-ipod-sync/device-1"));
        assert_eq!(
            staging_path_for(&root, 42),
            PathBuf::from("/tmp/x/rufin-ipod-sync/device-1/42.m4a")
        );
        // A different device id never collides.
        assert_ne!(
            staging_root(temp, "device-1"),
            staging_root(temp, "device-2")
        );
    }

    #[test]
    fn staging_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("tmp");
        let root = staging_root(&base, "device-1");

        // Sweep on a never-created dir is a no-op.
        sweep_orphans(&base, "device-1").unwrap();

        create_staging_dir(&root).unwrap();
        let path = staging_path_for(&root, 7);
        std::fs::write(&path, b"encoded").unwrap();
        assert!(path.is_file());

        cleanup_staging(&root).unwrap();
        assert!(!root.exists());

        // Sweep removes only this device's subtree, never siblings.
        let sibling = staging_root(&base, "device-2");
        create_staging_dir(&sibling).unwrap();
        std::fs::write(staging_path_for(&sibling, 1), b"x").unwrap();
        create_staging_dir(&root).unwrap();
        std::fs::write(staging_path_for(&root, 2), b"y").unwrap();
        sweep_orphans(&base, "device-1").unwrap();
        assert!(!root.exists());
        assert!(staging_path_for(&sibling, 1).is_file());
    }
}
