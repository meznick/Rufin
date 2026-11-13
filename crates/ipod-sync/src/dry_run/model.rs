//! B-009: the owned dry-run model — input contracts and the report
//! (`deltas/b009_plan.md` §4/§5).
//!
//! Everything here is plain data: the engine ([`super::compute::compute`])
//! consumes [`HostInputs`]/[`DeviceInputs`] and returns [`DryRunReport`]
//! with no I/O, no session, no clock. All itemized lists are sorted by
//! stable keys (track `itl_id`/`dbid`/`pid`, playlist id/pid) so the
//! report is deterministic for any input order. Removal entries are
//! inherently destructive and live in their own lists so the UI (B-015)
//! can render them distinctly.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::mapping::{HostLibrary, HostPlaylist, HostTrack, SmartRules};

/// The sync source selection (owner decision, B-008 §9): the engine takes
/// an explicitly selected source, never an implicit choice. Provenance-only
/// for the dry run; the sync entry point (B-010+) supplies it.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceKey(pub String);

/// What a sync does with device entries the host library lacks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceOnlyPolicy {
    /// Delete, like iTunes (owner decision 2026-11-13) — the default.
    #[default]
    Remove,
    /// Keep and report informationally.
    Keep,
}

/// The caller-supplied sync options. No UI, no config discovery in this
/// task; the engine echoes them in the report context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncOptions {
    pub source_key: SourceKey,
    /// iTunes' "Convert higher bitrate songs to AAC 256 kbps" (owner
    /// fact, B-008 §2a(c)): selects the copy mode and the capacity
    /// formula for would-add/repair entries.
    pub transcode_aac: bool,
    pub device_only: DeviceOnlyPolicy,
}

/// Host artwork evidence for one track, with the cover byte size — the
/// capacity formula's cover term (owner decision §9-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverEvidence {
    Present {
        bytes: u64,
    },
    Absent,
    /// The provider could not decide (unreadable file, unsupported
    /// container): informational, never an action.
    Unknown,
}

/// How a would-add/repair file is written to the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMode {
    Copy,
    TranscodeToAac,
}

/// One would-copy action: a host track the device lacks (host file present
/// on disk, location mapped inside the media folder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAdd {
    pub itl_id: u32,
    pub pid: u64,
    pub host_path: PathBuf,
    pub mode: CopyMode,
    /// Capacity estimate: exact host size for `Copy`; the owner formula
    /// (duration × 32000 + cover bytes + moov overhead) for `TranscodeToAac`.
    pub size_estimate: u64,
}

/// A matched track whose database record has no file on the pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRepair {
    pub dbid: u64,
    pub device_path: PathBuf,
    pub host_path: PathBuf,
    pub mode: CopyMode,
    pub size_estimate: u64,
}

/// A device-only track, rendered as a removal under the default policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRemove {
    pub dbid: u64,
    pub device_path: PathBuf,
}

/// The raw statistic fields the dry run compares. Directionally neutral:
/// the merge policy is B-010's decision (B-008 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatField {
    Rating,
    PlayCount,
    SkipCount,
    PlayDate,
    VolumeAdjustment,
}

/// One per-field raw difference on a matched pair. `host_raw`/`device_raw`
/// carry the raw values the report displays; play dates are compared in
/// unix-local space (see `compute`), the others raw as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatDiff {
    pub pid: u64,
    pub field: StatField,
    pub host_raw: i64,
    pub device_raw: i64,
}

/// Aggregate per-field difference counts (derived from `stat_diffs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatCounts {
    pub rating: u32,
    pub play_count: u32,
    pub skip_count: u32,
    pub play_date: u32,
    pub volume_adjustment: u32,
}

/// Smart-criteria comparison result: the host rules flattened to the
/// observed device form vs. the device blob parsed with the same parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CriteriaState {
    /// Flattened host rules equal the parsed device rules.
    Equal,
    /// A real difference — a sync would update the device criteria.
    Differ,
    /// No verdict: unparseable device blob, unflattenable host rules, or
    /// a missing blob. A diagnostic, never a silent match.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistKind {
    Static,
    Smart,
}

/// A host playlist with no device counterpart by (name, kind).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistCreate {
    pub host_pid: u64,
    pub name: Option<String>,
    pub kind: PlaylistKind,
    /// Ordered resolved members (PIDs; `None` = unresolvable member — a
    /// diagnostic, never a guess). Duplicates preserved.
    pub member_pids: Vec<Option<u64>>,
    /// The criteria a sync would write: host rules flattened to the device
    /// form. `None` when flattening had no observed equivalent (diagnostic)
    /// or the playlist is static.
    pub criteria: Option<SmartRules>,
}

/// A matched playlist whose membership, order or criteria differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistUpdate {
    pub device_id: u64,
    pub name: Option<String>,
    /// The device record's smart flag — the gate and the UI need it to
    /// distinguish truth (static membership) from cache (smart lists).
    pub is_spl: bool,
    pub members_add: Vec<u64>,
    pub members_remove: Vec<u64>,
    /// Membership sets equal, order differs.
    pub members_reorder: bool,
    pub criteria: CriteriaState,
    /// The host carries a Smart Info blob (live-update/limit/sort). The
    /// SPLPREF translation is B-010; informational here.
    pub smart_info_present: bool,
}

/// A non-local device-only playlist, rendered as a removal under the
/// default policy. The master and device-local playlists are never in
/// this list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistRemove {
    pub device_id: u64,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkAdd {
    pub pid: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkRemove {
    pub pid: u64,
}

/// Host inputs — fully owned, assembled by `snapshot::collect_host`.
#[derive(Clone, Debug, Default)]
pub struct HostInputs {
    pub library: Option<HostLibrary>,
    pub tracks: Vec<HostTrack>,
    pub playlists: Vec<HostPlaylist>,
    /// PID → cover evidence. The provider is caller-supplied (decision
    /// §9-1); the engine only consumes it.
    pub artwork: HashMap<u64, CoverEvidence>,
    /// `itl_id` → whether the mapped host file exists on disk. Absent
    /// entries count as present: an unprobed file never fabricates an
    /// add/repair.
    pub host_file_present: HashMap<u32, bool>,
}

/// Device inputs — fully owned, assembled by `snapshot::collect_device`
/// through the read-only wrapper surface.
#[derive(Clone, Debug, Default)]
pub struct DeviceInputs {
    /// Master-playlist display name (empty = absent).
    pub name: String,
    /// Human model name (empty = unresolved).
    pub model: String,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub tracks: Vec<ipod::TrackInfo>,
    pub playlists: Vec<ipod::PlaylistInfo>,
    /// Playlist id → verbatim `mhod`-51 payload (the wrapper walk).
    pub smart_blobs: HashMap<u64, Vec<u8>>,
    pub artwork_status: ipod::ArtworkStatus,
    /// `dbid` → whether the on-pod file exists. Absent entries count as
    /// present (no path record or unprobed — never a fabricated repair).
    pub device_file_present: HashMap<u64, bool>,
    /// The on-pod `Play Counts` file, presence only (B-010 owns the
    /// import semantics).
    pub play_counts_present: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportContext {
    pub library_pid: u64,
    pub device_name: Option<String>,
    pub device_model: Option<String>,
    pub options: SyncOptions,
    /// The wrapper verdict; anything but `Ok` gates the artwork section
    /// with a diagnostic.
    pub artwork_verdict: ipod::ArtworkVerdict,
    pub play_counts_present: Option<PathBuf>,
    /// The device master playlist (`is_mpl`) — device-owned, never a
    /// removal candidate in any policy.
    pub device_master: Option<PlaylistRemove>,
    /// Device-local playlists (On-The-Go and friends) — device-owned,
    /// informational in every policy.
    pub device_local_playlists: Vec<PlaylistRemove>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapacitySummary {
    pub free_bytes: u64,
    /// Sum of the add/repair estimates. With `TranscodeToAac` this is a
    /// conservative upper bound (owner formula).
    pub estimate_bytes: u64,
    /// `estimate_bytes <= free_bytes`. The verdict wording reflects the
    /// estimate direction — see the plan §4.
    pub would_fit: bool,
}

/// The full dry-run report. Every itemized list is sorted by its stable
/// key; counts are derived, never stored separately (except the stat
/// counts, for UI convenience).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DryRunReport {
    pub context: ReportContext,
    pub capacity: CapacitySummary,
    pub file_adds: Vec<FileAdd>,
    pub file_repairs: Vec<FileRepair>,
    /// Destructive: device-only tracks under `Remove`.
    pub file_removes: Vec<FileRemove>,
    /// Informational device-only dbids under `Keep`.
    pub device_only_tracks: Vec<u64>,
    pub stat_diffs: Vec<StatDiff>,
    pub stat_counts: StatCounts,
    pub playlist_creates: Vec<PlaylistCreate>,
    pub playlist_updates: Vec<PlaylistUpdate>,
    /// Destructive: non-local device-only playlists under `Remove`.
    pub playlist_removes: Vec<PlaylistRemove>,
    /// Informational device-only playlists under `Keep`.
    pub device_only_playlists: Vec<PlaylistRemove>,
    /// Destructive: device thumbnails the host lacks, under `Remove`.
    pub artwork_removes: Vec<ArtworkRemove>,
    /// Informational orphaned thumbnails under `Keep`.
    pub artwork_orphaned: Vec<ArtworkRemove>,
    pub artwork_adds: Vec<ArtworkAdd>,
    /// Tracks whose cover evidence is `Unknown` — informational.
    pub artwork_unknown: u32,
    /// Mapping diagnostics echoed verbatim plus the dry run's own
    /// (`HostFileMissing`, `CriteriaUnknown`, `ArtworkVerdictGated`,
    /// `WouldNotFit`, unresolvable playlist members, …). Findings, never
    /// failures of the dry run itself.
    pub diagnostics: Vec<String>,
}
