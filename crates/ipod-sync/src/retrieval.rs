//! B-005: retrieve the device facts the FSM displays and B-004's preflight
//! verifies — name, model/generation, model number, firmware, capacities,
//! FireWire ID.
//!
//! # Provenance (R3 gate, owner-reviewed 2026-11-09)
//!
//! - **Name** = the iTunesDB master-playlist name. libgpod reads no volume
//!   label anywhere; `Device/SysInfo` has no name key. There is deliberately
//!   **no volume-name fallback**: a missing master-playlist name stays an
//!   explicit absence (empty string).
//! - **Model/generation** = `SysInfo` `ModelNumStr` resolved through
//!   libgpod's model table. An unresolvable model (the empty-`SysInfo` case,
//!   which is what the real device snapshot has) stays `None`-derived
//!   absence — never a default model.
//! - **Firmware** stays `None`: the installed libgpod build hides the
//!   `SysInfoExtended` parser symbols (`itdb_sysinfo_properties_get_firmware_version`
//!   = `VisibleBuildID` is unreachable without new C code). See
//!   `deltas/b005_delta.md`.
//! - **FireWire ID** = the sysinfo hash's merged `FirewireGuid` entry
//!   (libgpod merges `FireWireGUID` from `SysInfoExtended` at parse time).
//! - **Capacities** = `statvfs` on the mount point — measured filesystem
//!   statistics, never `Itdb_IpodInfo.capacity` (nominal model-table value).
//!   A `statvfs` failure yields `None` capacities (explicit unavailability),
//!   which keeps B-004's preflight from reaching Ready.
//!
//! # Write discipline
//!
//! Retrieval only reads an already-parsed session and stats the mount.
//! It never calls `itdb_device_read_sysinfo` (upstream documents that as
//! invalidating all artwork of a parsed database) and never writes — the
//! env-gated tests prove it with a tree hash before/after.

use std::ffi::CString;
use std::path::Path;

use ipod::{DeviceSession, IpodError, PlaylistInfo};

/// The subset of the wrapper's read surface retrieval needs.
///
/// Implemented for [`DeviceSession`]; tests inject a fake so the mapping
/// logic runs without a backend. Mirrors the B-003 `MountSource` pattern.
pub trait DeviceRead {
    /// Device identity and raw SysInfo strings (see `ipod::DeviceInfo`).
    fn device_info(&self) -> Result<ipod::DeviceInfo, IpodError>;
    /// All playlists, in database order (master playlist included).
    fn playlists(&self) -> Result<Vec<PlaylistInfo>, IpodError>;
}

impl DeviceRead for DeviceSession<'_> {
    fn device_info(&self) -> Result<ipod::DeviceInfo, IpodError> {
        DeviceSession::device_info(self)
    }

    fn playlists(&self) -> Result<Vec<PlaylistInfo>, IpodError> {
        DeviceSession::playlists(self)
    }
}

/// Retrieval failures. Today the only failure mode is the session read
/// (`itdb_parse` errors surface at open, before retrieval); a dedicated
/// enum would just wrap [`IpodError`]. B-004's preflight classifications
/// can replace the alias if they need their own variants.
pub type RetrieveError = IpodError;

/// Everything the FSM needs to display and preflight a device — the
/// B-002 `DeviceInfo` mirror without the GTK type. Absence is explicit:
/// empty strings for `name`/`model`/`generation`, `None` for the optional
/// fields. Never a fabricated default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceFacts {
    /// Master-playlist name; `""` when absent (no volume-name fallback).
    pub name: String,
    /// Human model name (`itdb_info_get_ipod_model_name_string`).
    pub model: String,
    /// Human generation name (`itdb_info_get_ipod_generation_string`).
    pub generation: String,
    /// Abbreviated model number (`MA446` → `A446`).
    pub model_number: Option<String>,
    /// Always `None` today: the pinned build does not export the
    /// `SysInfoExtended` parser symbols (see the module docs).
    pub firmware: Option<String>,
    /// Measured filesystem capacity (`statvfs`), or `None` when the stat
    /// failed — an explicit absence, never a nominal model-table value.
    pub total_capacity: Option<u64>,
    /// Measured used space (`statvfs`), `<= total_capacity`.
    pub used_capacity: Option<u64>,
    /// The merged `FirewireGuid`, when the device exposes one.
    pub firewire_id: Option<String>,
}

/// Collect the device facts from an already-opened session and the mount
/// point it was opened on. Read-only: no writes, no SysInfo reload.
pub fn collect(device: &dyn DeviceRead, mount_point: &Path) -> Result<DeviceFacts, RetrieveError> {
    let info = device.device_info()?;
    let playlists = device.playlists()?;

    let name = master_playlist_name(&playlists);
    let (model, generation, model_number) = match &info.model {
        Some(model_info) => (
            model_info.model_name.clone(),
            model_info.generation_name.clone(),
            Some(model_info.model_number.clone()),
        ),
        // Unresolvable model (empty SysInfo): explicit absence, never a
        // default.
        None => (String::new(), String::new(), None),
    };
    let firewire_id = info.firewire_id().map(str::to_owned);
    let (total_capacity, used_capacity) = filesystem_usage(mount_point);

    Ok(DeviceFacts {
        name,
        model,
        generation,
        model_number,
        firmware: None,
        total_capacity,
        used_capacity,
        firewire_id,
    })
}

/// The display name: the master playlist's `mhyp` name. First master wins
/// if a database ever declares more than one; a missing name is the empty
/// string (deliberate — no volume-label fallback, R3 decision).
fn master_playlist_name(playlists: &[PlaylistInfo]) -> String {
    playlists
        .iter()
        .find(|playlist| playlist.is_master)
        .and_then(|playlist| playlist.name.clone())
        .unwrap_or_default()
}

/// Filesystem usage via `statvfs`: total = blocks × fragment size, used =
/// (blocks − free blocks) × fragment size, clamped at zero against
/// filesystem quirks. A failed stat is `None` + a warning — capacity is
/// measured or explicitly unavailable, never estimated.
fn filesystem_usage(mount_point: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(path) = CString::new(mount_point.as_os_str().as_encoded_bytes()) else {
        tracing::warn!(mount = %mount_point.display(), "statvfs: mount point contains a NUL byte");
        return (None, None);
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid NUL-terminated string; `stat` is a valid
    // out-parameter. `statvfs` performs no writes to the mount.
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut stat) };
    if result != 0 {
        tracing::warn!(
            mount = %mount_point.display(),
            "statvfs failed; capacities unavailable"
        );
        return (None, None);
    }
    let fragment = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    } as u64;
    let total = stat.f_blocks as u64 * fragment;
    let used = (stat.f_blocks as u64).saturating_sub(stat.f_bfree as u64) * fragment;
    (Some(total), Some(used.min(total)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipod::ModelInfo;

    /// A canned read surface: no backend, no files, fully deterministic.
    struct FakeDevice {
        info: ipod::DeviceInfo,
        playlists: Vec<PlaylistInfo>,
        /// When `Some`, every read fails with this message (as a
        /// `NotFound` error — the variant carries no other state).
        error: Option<String>,
    }

    impl DeviceRead for FakeDevice {
        fn device_info(&self) -> Result<ipod::DeviceInfo, IpodError> {
            match &self.error {
                Some(message) => Err(IpodError::NotFound(message.clone())),
                None => Ok(self.info.clone()),
            }
        }

        fn playlists(&self) -> Result<Vec<PlaylistInfo>, IpodError> {
            match &self.error {
                Some(message) => Err(IpodError::NotFound(message.clone())),
                None => Ok(self.playlists.clone()),
            }
        }
    }

    fn playlist(name: Option<&str>, is_master: bool) -> PlaylistInfo {
        PlaylistInfo {
            name: name.map(str::to_owned),
            is_master,
            ..PlaylistInfo::default()
        }
    }

    fn model_info(model_number: &str, model_name: &str, generation_name: &str) -> ModelInfo {
        ModelInfo {
            model_number: model_number.to_string(),
            model_name: model_name.to_string(),
            ipod_model: 15,
            generation: 17,
            generation_name: generation_name.to_string(),
            capacity_gb: Some(30.0),
        }
    }

    fn fake_with(model: Option<ModelInfo>, playlists: Vec<PlaylistInfo>) -> FakeDevice {
        FakeDevice {
            info: ipod::DeviceInfo {
                mount_point: Path::new("/mnt/ipod").to_path_buf(),
                model,
                uuid: None,
                sysinfo: Vec::new(),
            },
            playlists,
            error: None,
        }
    }

    #[test]
    fn name_comes_from_master_playlist() {
        let fake = fake_with(
            None,
            vec![
                playlist(Some("On-The-Go"), false),
                playlist(Some("iPod user"), true),
                playlist(Some("Other"), false),
            ],
        );
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.name, "iPod user");
    }

    #[test]
    fn missing_master_playlist_is_empty_name() {
        let fake = fake_with(None, vec![playlist(Some("On-The-Go"), false)]);
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.name, "");
    }

    #[test]
    fn nameless_master_playlist_is_empty_name() {
        let fake = fake_with(None, vec![playlist(None, true)]);
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.name, "");
    }

    #[test]
    fn first_master_wins_if_database_declares_two() {
        let fake = fake_with(
            None,
            vec![
                playlist(Some("first"), true),
                playlist(Some("second"), true),
            ],
        );
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.name, "first");
    }

    #[test]
    fn model_fills_strings_and_number() {
        let fake = fake_with(
            Some(model_info("A446", "Video (Black)", "iPod Video 5.5g")),
            vec![playlist(Some("pod"), true)],
        );
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.model, "Video (Black)");
        assert_eq!(facts.generation, "iPod Video 5.5g");
        assert_eq!(facts.model_number.as_deref(), Some("A446"));
    }

    #[test]
    fn unresolvable_model_stays_explicitly_absent() {
        let fake = fake_with(None, vec![playlist(Some("pod"), true)]);
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.model, "");
        assert_eq!(facts.generation, "");
        assert_eq!(facts.model_number, None);
        assert_eq!(facts.firmware, None);
    }

    #[test]
    fn firewire_id_passes_through() {
        let mut fake = fake_with(None, vec![playlist(Some("pod"), true)]);
        fake.info.sysinfo = vec![
            ("ModelNumStr".to_string(), "MA446".to_string()),
            ("FirewireGuid".to_string(), "0x000A270012345678".to_string()),
        ];
        let facts = collect(&fake, Path::new("/mnt/ipod")).unwrap();
        assert_eq!(facts.firewire_id.as_deref(), Some("0x000A270012345678"));
    }

    #[test]
    fn session_errors_propagate() {
        let fake = FakeDevice {
            info: ipod::DeviceInfo::default(),
            playlists: Vec::new(),
            error: Some("no database".to_string()),
        };
        let error = collect(&fake, Path::new("/mnt/ipod")).unwrap_err();
        assert_eq!(error.to_string(), "no database");
    }

    /// Real `statvfs` on a tempdir (any mounted filesystem): the invariants
    /// are total > 0 and used <= total; the values are the host's, not a
    /// device's — device-realistic totals are asserted in the env-gated
    /// mounted gate instead.
    #[test]
    fn statvfs_measures_real_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let (total, used) = filesystem_usage(dir.path());
        let total = total.expect("tempdir must be on a stat-able filesystem");
        let used = used.expect("tempdir must be on a stat-able filesystem");
        assert!(total > 0);
        assert!(used <= total);
    }

    #[test]
    fn statvfs_failure_is_explicit_none() {
        let (total, used) = filesystem_usage(Path::new("/nonexistent/definitely/not/here"));
        assert_eq!(total, None);
        assert_eq!(used, None);
    }
}
