//! B-015: the sync boundary service (b015_plan §3.2).
//!
//! `IpodSyncService` owns the sync lifecycle: the gating decision rides
//! the connection snapshot ([`crate::library::ipod_page::sync_controls`]), while
//! this service runs the engine. A **per-run thread** does the blocking
//! work (the `.itl` parse, the device parse, the engine transaction) —
//! never the connection worker, whose serialized FIFO must stay free
//! for scans and notices (the b006 latency debt).
//!
//! The engine seam is the injectable [`Runner`] trait: production uses
//! [`EngineRunner`] over a [`SyncBoundary`] (the shell's wiring of the
//! selected iTunes source); the gates inject stubs, so every phase
//! transition is display-free tested. The pure [`advance`] table carries
//! the semantics. There is no review gate: the page always shows the
//! live diff (an auto-refreshing dry-run preview), so the Sync click
//! itself is the approval — the B-016 redesign the owner settled after
//! the mockup rounds (2026-11-21; see `deltas/b016_delta.md`). A
//! preview (`Start { dry_run: true }`) stops at `Review`; a real run
//! (`Start { dry_run: false }`) goes straight to `Running` once
//! prepared.
//!
//! Backend loading happens **on the run thread**: `IpodBackend` is not
//! `Send` (raw symbol tables), so each phase loads its own instance —
//! `dlopen` is refcounted and process-global, the pinned ABI check runs
//! identically per load, and the double load costs milliseconds against
//! a minutes-long sync. The soft-accept policy is applied at every load:
//! an unverified ABI refuses with the same diagnostic the FSM shows
//! (`ipod_discovery::ABI_UNVERIFIED_DIAGNOSTIC`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use ipod::{ArtworkInput, BackendSearch, DeviceSession, IpodBackend, TrackInfo};
use ipod_sync::dry_run::{
    ArtworkProbe, CopyMode, CoverEvidence, CriteriaState, DryRunReport, HostInputs, SourceKey,
    StatField, SyncOptions, collect_device, collect_host, compute,
};
use ipod_sync::mapping::{HostLocation, HostTrack};
use ipod_sync::retrieval::{DeviceFacts, collect};
use ipod_sync::sync::exec::run_sync;
use ipod_sync::sync::host::HostWriteContext;
use ipod_sync::sync::merge::StatMerge;
use ipod_sync::sync::plan::ExecutionPlan;
use ipod_sync::sync::report::{
    ProgressEvent, RecoveryState, SyncChanges, SyncProgress, SyncResult, SyncStatus,
};
use itl_core::ItlFile;

use crate::library::ipod_discovery::ABI_UNVERIFIED_DIAGNOSTIC;

/// The stored event ring's cap — the B-016 progress panel consumes the
/// stream; B-015 only retains the recent tail.
const EVENT_RING_CAPACITY: usize = 64;

// ---------------------------------------------------------------------
// The boundary: what the engine needs from the selected iTunes source.
// Supplied by the shell — gtk-library must not see `sources` (layer
// inversion), so the trait lives here and the shell implements it.
// ---------------------------------------------------------------------

/// The configured iTunes source, as the engine needs it (b015_plan
/// §3.3) — plain owned data, captured by the shell.
#[derive(Clone, Debug)]
pub struct BoundarySource {
    /// `iTunes Library.itl` path.
    pub library: PathBuf,
    /// The media folder as seen on this host.
    pub media_root: PathBuf,
    /// Wall-clock ↔ unix conversion offset (the `.itl` envelope default
    /// when `None`).
    pub tz_offset: Option<i32>,
    /// The host source identity for the engine's report context.
    pub source_key: SourceKey,
}

pub trait SyncBoundary: Send + Sync {
    /// The configured iTunes source; `Err` carries the D-4 diagnostic
    /// when none — or several — are configured (source selection is a
    /// deferred UI step).
    fn source(&self) -> Result<BoundarySource, String>;
    /// Host cover evidence for one media file (the dry-run probe).
    fn cover(&self, path: &Path) -> CoverEvidence;
    /// Encoded artwork for a host track pid (the device write path).
    fn artwork(&self, pid: u64) -> Option<ArtworkInput>;
    /// The B-010a transcode provider.
    fn transcode(&self, source: &Path, destination: &Path) -> std::io::Result<()>;
}

/// Adapts a [`SyncBoundary`] into the engine's [`ArtworkProbe`].
struct BoundaryProbe<'a>(&'a dyn SyncBoundary);

impl ArtworkProbe for BoundaryProbe<'_> {
    fn cover(&self, path: &Path) -> CoverEvidence {
        self.0.cover(path)
    }
}

// ---------------------------------------------------------------------
// The runner seam.
// ---------------------------------------------------------------------

/// What `prepare` needs: the current device mount (from the connection
/// snapshot at click time). The boundary lives on the runner itself.
#[derive(Clone, Debug)]
pub struct PrepareRequest {
    pub mount: PathBuf,
    /// Full database regeneration (no media re-transfer): see
    /// `SyncOptions::rebuild`.
    pub rebuild: bool,
}

/// Everything computed by `prepare` and consumed by `execute` — the
/// approved plan's raw material. Owned data only (`Send`), so it can
/// cross into the run thread.
#[derive(Clone, Debug)]
pub struct PreparedSync {
    /// The dry-run report the plan is built from (the reviewed plan).
    pub report: DryRunReport,
    /// Host inputs (the `.itl` snapshot + artwork evidence).
    pub host: HostInputs,
    /// The device tracks with their names — the review sheet resolves
    /// the diff's dbids to titles (the report itself carries only ids).
    pub device_tracks: Vec<TrackInfo>,
    /// The engine options this run will use.
    pub options: SyncOptions,
    /// The device facts at prepare time (identity source).
    pub facts: DeviceFacts,
    /// The mount the run targets.
    pub mount: PathBuf,
}

/// The injectable engine seam: tests stub both calls; production is
/// [`EngineRunner`]. Both run on the service's run thread, so the
/// non-`Send` backend never crosses threads. `report` matches the
/// engine's own reporter contract (`Fn`, not `FnMut`).
pub trait Runner: Send + Sync {
    /// Collect host + device, compute the dry run (blocking).
    fn prepare(&self, request: &PrepareRequest) -> Result<PreparedSync, String>;
    /// Execute the engine with the prepared plan (blocking).
    fn execute(
        &self,
        prepared: PreparedSync,
        report: &(dyn Fn(ProgressEvent) + Send + Sync),
    ) -> SyncResult;
}

/// The production runner: the engine over a boundary, loading its own
/// backend per phase (see the module docs).
pub struct EngineRunner {
    boundary: Arc<dyn SyncBoundary>,
    search: BackendSearch,
}

impl EngineRunner {
    pub fn new(boundary: Arc<dyn SyncBoundary>, search: BackendSearch) -> Self {
        Self { boundary, search }
    }

    /// Load the backend on this thread with the soft-accept policy —
    /// an unverified ABI refuses with the FSM's diagnostic.
    fn backend(&self) -> Result<IpodBackend, String> {
        let backend =
            IpodBackend::load_with(self.search.clone()).map_err(|error| error.to_string())?;
        if !backend.abi_verified {
            return Err(ABI_UNVERIFIED_DIAGNOSTIC.to_string());
        }
        Ok(backend)
    }
}

impl Runner for EngineRunner {
    fn prepare(&self, request: &PrepareRequest) -> Result<PreparedSync, String> {
        let source = self.boundary.source()?;
        let backend = self.backend()?;
        let session =
            DeviceSession::open(&backend, &request.mount).map_err(|error| error.to_string())?;
        let facts = collect(&session, &request.mount).map_err(|error| error.to_string())?;
        let device =
            collect_device(&session, &request.mount, &facts).map_err(|error| error.to_string())?;
        let itl = ItlFile::open(&source.library).map_err(|error| error.to_string())?;
        let host = collect_host(&itl, &source.media_root, &BoundaryProbe(&*self.boundary));
        let options = SyncOptions {
            source_key: source.source_key,
            // The owner-rule transcode default: off until an options UI
            // exists (the Параметры box stays furniture, b014_plan D-6).
            transcode_aac: false,
            rebuild: request.rebuild,
            ..SyncOptions::default()
        };
        let report = compute(&host, &device, &options);
        Ok(PreparedSync {
            report,
            host,
            device_tracks: device.tracks,
            options,
            facts,
            mount: request.mount.clone(),
        })
    }

    fn execute(
        &self,
        prepared: PreparedSync,
        report: &(dyn Fn(ProgressEvent) + Send + Sync),
    ) -> SyncResult {
        let source = match self.boundary.source() {
            Ok(source) => source,
            Err(diagnostic) => {
                return SyncResult {
                    status: SyncStatus::Failed {
                        step: "Prepare",
                        diagnostic,
                    },
                    changes: SyncChanges::default(),
                    diagnostics: Vec::new(),
                    recovery: None,
                    last_success: None,
                };
            }
        };
        let backend = match self.backend() {
            Ok(backend) => backend,
            Err(diagnostic) => {
                return SyncResult {
                    status: SyncStatus::Failed {
                        step: "Prepare",
                        diagnostic,
                    },
                    changes: SyncChanges::default(),
                    diagnostics: Vec::new(),
                    recovery: None,
                    last_success: None,
                };
            }
        };
        // D-3: the readiness recheck — the existing preflight before any
        // real write (ro mount, unparseable DB, artwork state). A refusal
        // is a failed sync with the diagnostic; the page's FSM state is
        // untouched.
        if let Err(error) = ipod_sync::preflight(&backend, &prepared.mount) {
            return SyncResult {
                status: SyncStatus::Failed {
                    step: "Preflight",
                    diagnostic: error.to_string(),
                },
                changes: SyncChanges::default(),
                diagnostics: Vec::new(),
                recovery: None,
                last_success: None,
            };
        }
        let identity = format!("{}@{}", prepared.facts.name, prepared.mount.display());
        let host_write = HostWriteContext {
            library_path: source.library,
            media_root: source.media_root,
            tz_offset_seconds: source.tz_offset,
        };
        let plan = ExecutionPlan::build(&prepared.report, StatMerge::default());
        let context = ipod_sync::sync::exec::SyncContext {
            mount: &prepared.mount,
            backend: &backend,
            host: &prepared.host,
            options: &prepared.options,
            host_write: &host_write,
            device_identity: &identity,
            plan: &plan,
            transcode: &|source, destination| self.boundary.transcode(source, destination),
            artwork: &|pid| self.boundary.artwork(pid),
            // B-016 adds the cancel button; until then a run never
            // cancels and never injects faults.
            cancelled: &|| false,
            fail_after: &|_| false,
            reporter: report,
        };
        run_sync(&context)
    }
}

// ---------------------------------------------------------------------
// The display-free core: phases, the pure advance table, the summaries.
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    Idle,
    /// The run thread is collecting (`.itl` + device + compute).
    Collecting,
    /// The live preview: the last dry-run diff, shown inline on the
    /// page. Always present when the device is ready — never a gate.
    Review {
        summary: ReviewSummary,
    },
    /// The engine transaction runs on the run thread.
    Running,
    Done(SyncOutcomeSummary),
}

impl Default for SyncPhase {
    fn default() -> Self {
        Self::Idle
    }
}

/// The observable service state — cloneable for the subscriber fan-out,
/// so `ReviewSummary`/`SyncOutcomeSummary` are plain data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncState {
    pub phase: SyncPhase,
    /// The recent progress tail (B-016 renders it; B-015 retains it).
    pub events: Vec<ProgressEvent>,
    /// The monotone progress snapshot the sheet's bar renders — folded
    /// from every event, so a dropped event can never rewind it.
    pub progress: SyncProgress,
}

/// One service-internal transition input.
/// The `Prepared` payload deliberately rides the event unboxed — one
/// value per collection, and `advance` must stay payload-aware (the
/// same rationale as `SyncAction::Execute`'s box in the other
/// direction; here the box would just move the allocation).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SyncEvent {
    /// A Sync click or the page's preview refresh: the device mount at
    /// click time. `dry_run` true = the live preview; false = the real
    /// run. `rebuild` true = the full database regeneration run.
    Start {
        dry_run: bool,
        rebuild: bool,
        mount: PathBuf,
    },
    /// The run thread finished collecting.
    Prepared {
        dry_run: bool,
        result: Result<PreparedSync, String>,
    },
    /// The user dismissed the result (the review is never dismissible —
    /// it is the page's live diff).
    Dismissed,
    /// A progress event from the engine.
    Progress(ProgressEvent),
    /// The engine run returned.
    Finished(SyncResult),
}

/// What the facade must do after a transition (the thin thread side).
#[derive(Debug)]
pub enum SyncAction {
    /// Spawn the prepare thread.
    Prepare {
        dry_run: bool,
        rebuild: bool,
        mount: PathBuf,
    },
    /// Spawn the execute thread with the prepared sync (boxed — the
    /// prepared inputs dwarf the other variants).
    Execute(Box<PreparedSync>),
}

/// The pure transition table. Every semantic of the service lives here;
/// the facade applies actions and fans snapshots out.
pub fn advance(state: &mut SyncState, event: SyncEvent) -> Option<SyncAction> {
    match (&state.phase, event) {
        // A start is allowed from Idle, from Done (the result closed),
        // and from Review — the live preview is never a gate: a Sync
        // click or a preview refresh restarts the collection.
        (
            SyncPhase::Idle | SyncPhase::Done(_) | SyncPhase::Review { .. },
            SyncEvent::Start {
                dry_run,
                rebuild,
                mount,
            },
        ) => {
            state.phase = SyncPhase::Collecting;
            // A fresh collection starts with a clean progress surface —
            // the previous run's tail must not bleed into the next one.
            state.progress = SyncProgress::default();
            state.events.clear();
            Some(SyncAction::Prepare {
                dry_run,
                rebuild,
                mount,
            })
        }
        (
            SyncPhase::Collecting,
            SyncEvent::Prepared {
                dry_run: true,
                result: Ok(prepared),
            },
        ) => {
            // A preview stops at the review: the diff is the deliverable.
            state.phase = SyncPhase::Review {
                summary: review_summary(&prepared.report, &prepared.host, &prepared.device_tracks),
            };
            None
        }
        (
            SyncPhase::Collecting,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared),
            },
        ) => {
            // A real run: the click was the approval (the diff was
            // already on the page) — straight to the engine.
            state.phase = SyncPhase::Running;
            Some(SyncAction::Execute(Box::new(prepared)))
        }
        (
            SyncPhase::Collecting,
            SyncEvent::Prepared {
                result: Err(diagnostic),
                ..
            },
        ) => {
            state.phase = SyncPhase::Done(SyncOutcomeSummary::failed("Prepare", diagnostic));
            None
        }
        (SyncPhase::Done(_), SyncEvent::Dismissed) => {
            state.phase = SyncPhase::Idle;
            None
        }
        (SyncPhase::Running, SyncEvent::Finished(result)) => {
            state.phase = SyncPhase::Done(outcome_summary(&result));
            None
        }
        (_, SyncEvent::Progress(event)) => {
            state.events.push(event.clone());
            if state.events.len() > EVENT_RING_CAPACITY {
                let excess = state.events.len() - EVENT_RING_CAPACITY;
                state.events.drain(..excess);
            }
            state.progress.fold(&event);
            None
        }
        // Every other combination is a no-op: a stray start while
        // collecting or running, a dismiss outside Done, a finished
        // outside Running, progress in any phase.
        _ => None,
    }
}

/// One named diff row — a track the sheet shows by title/artist/album
/// with an optional size and the transcode chip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Would-copy size (adds/repairs) or the device record's size
    /// (removals). `None` renders no size.
    pub size_bytes: Option<u64>,
    /// The add plans a transcode-to-AAC write.
    pub transcodes: bool,
}

/// One changed statistic on a matched track — the sheet's chip pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatDelta {
    pub field: StatField,
    pub host: i64,
    pub device: i64,
}

/// One matched track's stat updates, grouped per track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatTrack {
    pub title: String,
    pub artist: String,
    pub deltas: Vec<StatDelta>,
}

/// One playlist diff — a create, or an update with its named members.
/// A deferred smart list (criteria differ, B-023 translation) carries
/// its name only; the sheet says what happens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffPlaylist {
    pub name: String,
    pub adds: Vec<DiffTrack>,
    pub removes: Vec<DiffTrack>,
    pub reorder: bool,
    pub deferred: bool,
}

/// The name-resolved diff the sheet's sections render — the full
/// report, nothing capped (the dialog capped findings at five; the
/// sheet has room for all of them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewDetails {
    pub adds: Vec<DiffTrack>,
    pub repairs: Vec<DiffTrack>,
    pub removals: Vec<DiffTrack>,
    pub updates: Vec<StatTrack>,
    pub playlists: Vec<DiffPlaylist>,
    pub playlist_creates: Vec<DiffPlaylist>,
    pub playlist_removes: Vec<String>,
    pub findings: Vec<String>,
}

/// The review sheet's content, derived from a dry-run report — pure
/// counts plus the name-resolved [`ReviewDetails`], no wording (the
/// widget layer translates).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReviewSummary {
    pub files_added: usize,
    pub files_removed: usize,
    pub files_repaired: usize,
    pub device_only_tracks: usize,
    pub playlists_created: usize,
    pub playlists_updated: usize,
    pub playlists_removed: usize,
    pub device_only_playlists: usize,
    pub artwork_adds: usize,
    pub artwork_removes: usize,
    pub artwork_orphaned: usize,
    pub stat_updates: u32,
    pub estimate_bytes: u64,
    pub free_bytes: u64,
    pub would_fit: bool,
    pub diagnostic_count: usize,
    /// The report's own findings, capped at the legacy dialog's budget
    /// (b015_fixes F6); the sheet renders the full set from `details`.
    pub diagnostics: Vec<String>,
    /// The name-resolved diff the sheet's sections render.
    pub details: ReviewDetails,
    /// Nothing at all would change.
    pub nothing_to_do: bool,
}

impl ReviewSummary {
    /// The dialog's rendering budget for verbatim findings — the
    /// report can carry many; the review shows the first few
    /// (b015_fixes F6).
    const DIAGNOSTIC_CAP: usize = 5;

    fn from_report(report: &DryRunReport) -> Self {
        let stat_updates = report.stat_counts.rating
            + report.stat_counts.play_count
            + report.stat_counts.skip_count
            + report.stat_counts.play_date
            + report.stat_counts.volume_adjustment;
        let summary = Self {
            files_added: report.file_adds.len(),
            files_removed: report.file_removes.len(),
            files_repaired: report.file_repairs.len(),
            device_only_tracks: report.device_only_tracks.len(),
            playlists_created: report.playlist_creates.len(),
            playlists_updated: report.playlist_updates.len(),
            playlists_removed: report.playlist_removes.len(),
            device_only_playlists: report.device_only_playlists.len(),
            artwork_adds: report.artwork_adds.len(),
            artwork_removes: report.artwork_removes.len(),
            artwork_orphaned: report.artwork_orphaned.len(),
            stat_updates,
            estimate_bytes: report.capacity.estimate_bytes,
            free_bytes: report.capacity.free_bytes,
            would_fit: report.capacity.would_fit,
            diagnostic_count: report.diagnostics.len(),
            diagnostics: report
                .diagnostics
                .iter()
                .take(Self::DIAGNOSTIC_CAP)
                .cloned()
                .collect(),
            details: ReviewDetails::default(),
            nothing_to_do: false,
        };
        Self {
            nothing_to_do: summary_is_empty(&summary, report),
            ..summary
        }
    }
}

fn summary_is_empty(summary: &ReviewSummary, report: &DryRunReport) -> bool {
    summary.files_added == 0
        && summary.files_removed == 0
        && summary.files_repaired == 0
        && summary.playlists_created == 0
        && summary.playlists_updated == 0
        && summary.playlists_removed == 0
        && summary.artwork_adds == 0
        && summary.artwork_removes == 0
        && summary.stat_updates == 0
        && report.artwork_unknown == 0
}

fn file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|stem| stem.to_str())
}

/// The name fallbacks for rows whose records carry none — never a
/// blank row.
fn host_names(track: &HostTrack) -> (String, String, String) {
    let title = track
        .metadata
        .title
        .clone()
        .or_else(|| match &track.location {
            HostLocation::Mapped(path) => file_stem(path).map(str::to_string),
            _ => None,
        })
        .unwrap_or_else(|| "Unknown".to_string());
    (
        title,
        track.metadata.artist.clone().unwrap_or_default(),
        track.metadata.album.clone().unwrap_or_default(),
    )
}

fn host_diff_track(track: &HostTrack, size: Option<u64>, transcodes: bool) -> DiffTrack {
    let (title, artist, album) = host_names(track);
    DiffTrack {
        title,
        artist,
        album,
        size_bytes: size,
        transcodes,
    }
}

fn device_diff_track(track: &TrackInfo, size: Option<u64>) -> DiffTrack {
    DiffTrack {
        title: track.title.clone().unwrap_or_else(|| "Unknown".to_string()),
        artist: track.artist.clone().unwrap_or_default(),
        album: track.album.clone().unwrap_or_default(),
        size_bytes: size,
        transcodes: false,
    }
}

/// A playlist member dbid resolved to a name — device records first,
/// then the host catalog (planned adds), then the bare id (should not
/// happen: the dry run filters members to the writable set).
fn member_diff_track(
    dbid: u64,
    device_by_dbid: &HashMap<u64, &TrackInfo>,
    host_by_pid: &HashMap<u64, &HostTrack>,
) -> DiffTrack {
    if let Some(track) = device_by_dbid.get(&dbid) {
        return device_diff_track(track, Some(u64::from(track.size)));
    }
    if let Some(track) = host_by_pid.get(&dbid) {
        return host_diff_track(track, None, false);
    }
    DiffTrack {
        title: format!("#{dbid}"),
        ..DiffTrack::default()
    }
}

/// The name-resolved diff behind the sheet's sections.
fn review_details(
    report: &DryRunReport,
    host: &HostInputs,
    device_tracks: &[TrackInfo],
) -> ReviewDetails {
    let host_by_pid: HashMap<u64, &HostTrack> = host
        .tracks
        .iter()
        .map(|track| (track.track_pid, track))
        .collect();
    let device_by_dbid: HashMap<u64, &TrackInfo> = device_tracks
        .iter()
        .map(|track| (track.dbid, track))
        .collect();

    let adds = report
        .file_adds
        .iter()
        .filter_map(|add| {
            let track = host_by_pid.get(&add.pid)?;
            Some(host_diff_track(
                track,
                Some(add.size_estimate),
                add.mode == CopyMode::TranscodeToAac,
            ))
        })
        .collect();

    let repairs = report
        .file_repairs
        .iter()
        .map(|repair| match device_by_dbid.get(&repair.dbid) {
            Some(track) => device_diff_track(track, Some(repair.size_estimate)),
            None => DiffTrack {
                title: file_stem(&repair.host_path)
                    .unwrap_or("Unknown")
                    .to_string(),
                size_bytes: Some(repair.size_estimate),
                ..DiffTrack::default()
            },
        })
        .collect();

    let removals = report
        .file_removes
        .iter()
        .filter_map(|remove| device_by_dbid.get(&remove.dbid))
        .map(|track| device_diff_track(track, Some(u64::from(track.size))))
        .collect();

    // Stat diffs grouped per track, in the report's (sorted) order.
    let mut updates: Vec<StatTrack> = Vec::new();
    let mut update_index: HashMap<u64, usize> = HashMap::new();
    for diff in &report.stat_diffs {
        let index = match update_index.get(&diff.pid) {
            Some(index) => *index,
            None => {
                let (title, artist) = match host_by_pid.get(&diff.pid) {
                    Some(track) => {
                        let (title, artist, _) = host_names(track);
                        (title, artist)
                    }
                    None => ("Unknown".to_string(), String::new()),
                };
                update_index.insert(diff.pid, updates.len());
                updates.push(StatTrack {
                    title,
                    artist,
                    deltas: Vec::new(),
                });
                updates.len() - 1
            }
        };
        updates[index].deltas.push(StatDelta {
            field: diff.field,
            host: diff.host_raw,
            device: diff.device_raw,
        });
    }

    let playlists = report
        .playlist_updates
        .iter()
        .map(|update| DiffPlaylist {
            name: update.name.clone().unwrap_or_else(|| "Unnamed".to_string()),
            adds: update
                .members_add
                .iter()
                .map(|dbid| member_diff_track(*dbid, &device_by_dbid, &host_by_pid))
                .collect(),
            removes: update
                .members_remove
                .iter()
                .map(|dbid| member_diff_track(*dbid, &device_by_dbid, &host_by_pid))
                .collect(),
            reorder: update.members_reorder,
            deferred: update.is_spl && update.criteria != CriteriaState::Equal,
        })
        .collect();

    let playlist_creates = report
        .playlist_creates
        .iter()
        .map(|create| DiffPlaylist {
            name: create.name.clone().unwrap_or_else(|| "Unnamed".to_string()),
            adds: create
                .member_pids
                .iter()
                .flatten()
                .map(|pid| member_diff_track(*pid, &device_by_dbid, &host_by_pid))
                .collect(),
            ..DiffPlaylist::default()
        })
        .collect();

    let playlist_removes = report
        .playlist_removes
        .iter()
        .filter_map(|remove| remove.name.clone())
        .collect();

    ReviewDetails {
        adds,
        repairs,
        removals,
        updates,
        playlists,
        playlist_creates,
        playlist_removes,
        findings: report.diagnostics.clone(),
    }
}

/// The pure derivation the review sheet renders — counts plus the
/// name-resolved diff.
pub fn review_summary(
    report: &DryRunReport,
    host: &HostInputs,
    device_tracks: &[TrackInfo],
) -> ReviewSummary {
    let details = review_details(report, host, device_tracks);
    let mut summary = ReviewSummary::from_report(report);
    summary.details = details;
    summary
}

/// The result sheet's content: the engine outcome plus the boundary
/// failure shape (a `Prepare`-step failure that never reached the
/// engine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcomeSummary {
    pub status: SyncStatus,
    pub changes: SyncChanges,
    pub diagnostics: Vec<String>,
    /// Pending recovery work — the sheet shows the resume hint when
    /// present (a preflight refusal has none and must not promise one).
    pub recovery: Option<RecoveryState>,
}

impl SyncOutcomeSummary {
    fn failed(step: &'static str, diagnostic: String) -> Self {
        Self {
            status: SyncStatus::Failed { step, diagnostic },
            changes: SyncChanges::default(),
            diagnostics: Vec::new(),
            recovery: None,
        }
    }
}

/// The pure extraction the result sheet renders.
pub fn outcome_summary(result: &SyncResult) -> SyncOutcomeSummary {
    SyncOutcomeSummary {
        status: result.status.clone(),
        changes: result.changes.clone(),
        diagnostics: result.diagnostics.clone(),
        recovery: result.recovery.clone(),
    }
}

/// The status's own diagnostic — `Failed`/`Partial` carry the step and
/// the reason. The result dialog must show it: the acceptance run
/// showed a bare "Sync failed" while the reason sat unrendered in the
/// status (and nowhere in the console).
pub fn outcome_diagnostic(status: &SyncStatus) -> Option<String> {
    match status {
        SyncStatus::Failed { step, diagnostic } => Some(format!("{step}: {diagnostic}")),
        SyncStatus::Partial {
            failed_step,
            diagnostic,
        } => Some(format!("{failed_step}: {diagnostic}")),
        SyncStatus::Completed => None,
    }
}

// ---------------------------------------------------------------------
// The facade: cloneable, subscriber fan-out, the thread spawning.
// ---------------------------------------------------------------------

struct ServiceInner {
    state: Mutex<SyncState>,
    subscribers: Mutex<Vec<async_channel::Sender<SyncState>>>,
    runner: Arc<dyn Runner>,
}

impl ServiceInner {
    /// One transition: advance, fan out, act. Lock ordering matches
    /// `ipod_connection`'s `publish` (state, then subscribers), so the
    /// run thread's progress fan-in cannot deadlock with a racing
    /// subscriber. Lives on the inner value so the run threads — which
    /// hold `Arc<ServiceInner>` — can apply their own results.
    fn apply(self: &Arc<Self>, event: SyncEvent) {
        let action = {
            let mut state = self.state.lock().expect("sync state lock poisoned");
            let action = advance(&mut state, event);
            // The acceptance finding: a failed run printed nothing to
            // the console — the diagnostic sat in the result. Log every
            // terminal failure here so the console carries the reason.
            if let SyncPhase::Done(summary) = &state.phase {
                match &summary.status {
                    SyncStatus::Failed { step, diagnostic } => {
                        tracing::error!(step, diagnostic, "ipod sync failed");
                    }
                    SyncStatus::Partial {
                        failed_step,
                        diagnostic,
                    } => {
                        tracing::warn!(failed_step, diagnostic, "ipod sync incomplete");
                    }
                    SyncStatus::Completed => {}
                }
            }
            action
        };
        self.publish();
        match action {
            Some(SyncAction::Prepare {
                dry_run,
                rebuild,
                mount,
            }) => {
                let runner = self.runner.clone();
                let inner = self.clone();
                thread::Builder::new()
                    .name("ipod-sync-prepare".to_string())
                    .spawn(move || {
                        let result = runner.prepare(&PrepareRequest { mount, rebuild });
                        inner.apply(SyncEvent::Prepared { dry_run, result });
                    })
                    .expect("failed to spawn the ipod sync prepare thread");
            }
            Some(SyncAction::Execute(prepared)) => {
                let runner = self.runner.clone();
                let inner = self.clone();
                thread::Builder::new()
                    .name("ipod-sync-run".to_string())
                    .spawn(move || {
                        let reporter = {
                            let inner = inner.clone();
                            move |event: ProgressEvent| inner.apply(SyncEvent::Progress(event))
                        };
                        let result = runner.execute(*prepared, &reporter);
                        inner.apply(SyncEvent::Finished(result));
                    })
                    .expect("failed to spawn the ipod sync run thread");
            }
            None => {}
        }
    }

    /// Fan the state out; drop unreadable subscribers (the B-006
    /// pattern — `try_send`, never a blocking send from a run thread).
    fn publish(&self) {
        let view = self.state.lock().expect("sync state lock poisoned").clone();
        let mut subscribers = self.subscribers.lock().expect("subscribers lock poisoned");
        subscribers.retain(|sender| sender.try_send(view.clone()).is_ok());
    }
}

/// The sync lifecycle facade. Plain data + threads, no GTK types — the
/// gates drive it display-free. Cloneable: clones share one service.
#[derive(Clone)]
pub struct IpodSyncService {
    inner: Arc<ServiceInner>,
}

impl IpodSyncService {
    pub fn new(runner: Arc<dyn Runner>) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                state: Mutex::new(SyncState::default()),
                subscribers: Mutex::new(Vec::new()),
                runner,
            }),
        }
    }

    /// The current observable state.
    pub fn snapshot(&self) -> SyncState {
        self.inner
            .state
            .lock()
            .expect("sync state lock poisoned")
            .clone()
    }

    /// Subscribe to state changes (no replay — read [`Self::snapshot`]
    /// first, the B-006 pattern).
    pub fn subscribe(&self) -> async_channel::Receiver<SyncState> {
        let (sender, receiver) = async_channel::unbounded();
        self.inner
            .subscribers
            .lock()
            .expect("subscribers lock poisoned")
            .push(sender);
        receiver
    }

    /// A Sync click (`dry_run` false) or the page's preview refresh
    /// (`dry_run` true): `mount` is the current device's mount point.
    /// `rebuild` true = the full database regeneration run. A no-op
    /// while collecting or running.
    pub fn start(&self, dry_run: bool, rebuild: bool, mount: &Path) {
        self.inner.apply(SyncEvent::Start {
            dry_run,
            rebuild,
            mount: mount.to_path_buf(),
        });
    }

    /// The result panel's Close.
    pub fn dismiss(&self) {
        self.inner.apply(SyncEvent::Dismissed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipod_sync::dry_run::PlaylistKind;
    use ipod_sync::dry_run::model::{
        ArtworkAdd, CapacitySummary, FileAdd, PlaylistCreate, StatCounts,
    };

    fn completed_result() -> SyncResult {
        SyncResult {
            status: SyncStatus::Completed,
            changes: SyncChanges::default(),
            diagnostics: Vec::new(),
            recovery: None,
            last_success: None,
        }
    }

    fn report_with(some_action: bool) -> DryRunReport {
        let mut report = DryRunReport::default();
        if some_action {
            report.file_adds.push(FileAdd {
                itl_id: 1,
                pid: 100,
                host_path: PathBuf::from("/media/a.mp3"),
                mode: ipod_sync::dry_run::CopyMode::Copy,
                size_estimate: 4_000_000,
            });
            report.playlist_creates.push(PlaylistCreate {
                host_pid: 1,
                name: Some("New".to_string()),
                kind: PlaylistKind::Static,
                member_pids: Vec::new(),
                criteria: None,
            });
        }
        report.capacity = CapacitySummary {
            free_bytes: 10_000,
            estimate_bytes: 2_000,
            would_fit: true,
        };
        if some_action {
            report.stat_counts = StatCounts {
                rating: 1,
                play_count: 2,
                skip_count: 3,
                play_date: 0,
                volume_adjustment: 0,
            };
            report.artwork_adds.push(ArtworkAdd { pid: 100 });
        }
        report
    }

    fn prepared() -> PreparedSync {
        PreparedSync {
            report: report_with(true),
            host: HostInputs::default(),
            device_tracks: Vec::new(),
            options: SyncOptions::default(),
            facts: DeviceFacts::default(),
            mount: PathBuf::from("/mnt/ipod"),
        }
    }

    fn state() -> SyncState {
        SyncState::default()
    }

    #[test]
    fn review_summary_maps_the_report_counts() {
        let summary = review_summary(&report_with(true), &HostInputs::default(), &[]);
        assert_eq!(summary.files_added, 1);
        assert_eq!(summary.playlists_created, 1);
        assert_eq!(summary.artwork_adds, 1);
        assert_eq!(summary.stat_updates, 6);
        assert_eq!(summary.estimate_bytes, 2_000);
        assert_eq!(summary.free_bytes, 10_000);
        assert!(summary.would_fit);
        assert!(!summary.nothing_to_do);

        let empty = review_summary(&report_with(false), &HostInputs::default(), &[]);
        assert!(empty.nothing_to_do, "no actions anywhere");
    }

    #[test]
    fn review_details_resolve_names_and_deltas() {
        let mut report = report_with(true);
        report.stat_diffs.push(ipod_sync::dry_run::StatDiff {
            pid: 100,
            field: StatField::Rating,
            host_raw: 80,
            device_raw: 60,
        });
        let mut host = HostInputs::default();
        host.tracks.push(ipod_sync::mapping::HostTrack {
            library_pid: 1,
            track_pid: 100,
            itl_id: 1,
            location: ipod_sync::mapping::HostLocation::NoLocation,
            metadata: ipod_sync::mapping::HostTrackMetadata {
                title: Some("Song".to_string()),
                artist: Some("Artist".to_string()),
                album: Some("Album".to_string()),
                ..Default::default()
            },
            stats: ipod_sync::mapping::HostStats::default(),
            content_rating: 0,
        });
        let summary = review_summary(&report, &host, &[]);
        assert_eq!(
            summary.details.adds.len(),
            1,
            "the add resolves by host pid"
        );
        assert_eq!(summary.details.adds[0].title, "Song");
        assert_eq!(summary.details.adds[0].size_bytes, Some(4_000_000));
        assert_eq!(summary.details.updates.len(), 1);
        assert_eq!(summary.details.updates[0].artist, "Artist");
        assert_eq!(
            summary.details.updates[0].deltas,
            vec![StatDelta {
                field: StatField::Rating,
                host: 80,
                device: 60,
            }]
        );
        assert_eq!(summary.details.playlist_creates.len(), 1);
        assert_eq!(summary.details.playlist_creates[0].name, "New");
    }

    #[test]
    fn progress_events_fold_into_the_progress_snapshot() {
        let mut state = state();
        state.phase = SyncPhase::Running;
        advance(
            &mut state,
            SyncEvent::Progress(ProgressEvent::Step {
                name: "Files",
                index: 3,
                total: 6,
                began: true,
            }),
        );
        advance(
            &mut state,
            SyncEvent::Progress(ProgressEvent::Item {
                current: 12,
                total: 340,
                label: "Copying a track".to_string(),
            }),
        );
        assert_eq!(state.progress.step, Some("Files"));
        assert_eq!(state.progress.step_index, 3);
        assert_eq!(state.progress.step_total, 6);
        assert_eq!(state.progress.item_current, 12);
        assert_eq!(state.progress.item_total, 340);
        assert_eq!(
            state.progress.item_label.as_deref(),
            Some("Copying a track")
        );

        // A new run resets the snapshot (Start fires from Idle).
        state.phase = SyncPhase::Idle;
        advance(
            &mut state,
            SyncEvent::Start {
                dry_run: false,
                rebuild: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
        );
        assert_eq!(state.progress, SyncProgress::default());
        assert!(state.events.is_empty());
    }

    #[test]
    fn outcome_summary_passes_the_engine_result_through() {
        let result = SyncResult {
            status: SyncStatus::Completed,
            changes: SyncChanges {
                files_added: 3,
                ..SyncChanges::default()
            },
            diagnostics: vec!["mapping note".to_string()],
            recovery: None,
            last_success: None,
        };
        let summary = outcome_summary(&result);
        assert_eq!(summary.status, SyncStatus::Completed);
        assert_eq!(summary.changes.files_added, 3);
        assert_eq!(summary.diagnostics, vec!["mapping note".to_string()]);
    }

    #[test]
    fn outcome_diagnostic_carries_the_failure_reason() {
        assert_eq!(
            outcome_diagnostic(&SyncStatus::Failed {
                step: "DbWrite",
                diagnostic: "the write failed".to_string(),
            }),
            Some("DbWrite: the write failed".to_string())
        );
        assert_eq!(
            outcome_diagnostic(&SyncStatus::Partial {
                failed_step: "HostWrite",
                diagnostic: "sentinel refused".to_string(),
            }),
            Some("HostWrite: sentinel refused".to_string())
        );
        assert_eq!(outcome_diagnostic(&SyncStatus::Completed), None);
    }

    #[test]
    fn a_prepare_failure_lands_in_done_with_the_diagnostic() {
        let mut state = state();
        state.phase = SyncPhase::Collecting;
        let action = advance(
            &mut state,
            SyncEvent::Prepared {
                dry_run: false,
                result: Err("no iTunes source configured".to_string()),
            },
        );
        assert!(action.is_none());
        assert_eq!(
            state.phase,
            SyncPhase::Done(SyncOutcomeSummary {
                status: SyncStatus::Failed {
                    step: "Prepare",
                    diagnostic: "no iTunes source configured".to_string(),
                },
                changes: SyncChanges::default(),
                diagnostics: Vec::new(),
                recovery: None,
            })
        );
    }

    #[test]
    fn a_preview_stops_at_the_review_and_a_real_run_executes() {
        let mut machine = state();
        let action = advance(
            &mut machine,
            SyncEvent::Start {
                dry_run: true,
                rebuild: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
        );
        assert_eq!(machine.phase, SyncPhase::Collecting);
        assert!(matches!(
            action,
            Some(SyncAction::Prepare { dry_run: true, .. })
        ));

        // A stray second start while collecting is a no-op.
        let action = advance(
            &mut machine,
            SyncEvent::Start {
                dry_run: false,
                rebuild: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
        );
        assert!(action.is_none());
        assert_eq!(machine.phase, SyncPhase::Collecting);

        // The preview lands on the review and never executes.
        let action = advance(
            &mut machine,
            SyncEvent::Prepared {
                dry_run: true,
                result: Ok(prepared()),
            },
        );
        assert!(action.is_none());
        assert!(matches!(machine.phase, SyncPhase::Review { .. }));

        // A real run restarts straight from the live preview — the
        // preview is never a gate.
        let action = advance(
            &mut machine,
            SyncEvent::Start {
                dry_run: false,
                rebuild: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
        );
        assert!(matches!(
            action,
            Some(SyncAction::Prepare { dry_run: false, .. })
        ));
        let action = advance(
            &mut machine,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared()),
            },
        );
        assert_eq!(machine.phase, SyncPhase::Running);
        assert!(matches!(action, Some(SyncAction::Execute(_))));
    }

    #[test]
    fn preview_refreshes_restart_from_the_review() {
        let mut machine = state();
        machine.phase = SyncPhase::Review {
            summary: review_summary(&report_with(false), &HostInputs::default(), &[]),
        };
        let action = advance(
            &mut machine,
            SyncEvent::Start {
                dry_run: true,
                rebuild: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
        );
        assert_eq!(machine.phase, SyncPhase::Collecting);
        assert!(matches!(
            action,
            Some(SyncAction::Prepare { dry_run: true, .. })
        ));
    }

    #[test]
    fn review_and_done_lifecycles() {
        let mut state = state();
        state.phase = SyncPhase::Collecting;
        advance(
            &mut state,
            SyncEvent::Prepared {
                dry_run: true,
                result: Ok(prepared()),
            },
        );
        assert!(matches!(state.phase, SyncPhase::Review { .. }));

        // The full run flow: start (from the preview) → prepared →
        // running → done → dismissed → idle.
        advance(
            &mut state,
            SyncEvent::Start {
                dry_run: false,
                rebuild: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
        );
        advance(
            &mut state,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared()),
            },
        );
        assert_eq!(state.phase, SyncPhase::Running);
        advance(&mut state, SyncEvent::Finished(completed_result()));
        assert_eq!(
            state.phase,
            SyncPhase::Done(outcome_summary(&completed_result()))
        );
        // Dismissing the result returns to Idle (the page re-previews).
        advance(&mut state, SyncEvent::Dismissed);
        assert_eq!(state.phase, SyncPhase::Idle);
    }

    #[test]
    fn progress_events_append_and_the_ring_is_bounded() {
        let mut state = state();
        for index in 0..EVENT_RING_CAPACITY + 10 {
            advance(
                &mut state,
                SyncEvent::Progress(ProgressEvent::Notice(format!("event {index}"))),
            );
        }
        assert_eq!(state.events.len(), EVENT_RING_CAPACITY);
        assert_eq!(
            state.events.first(),
            Some(&ProgressEvent::Notice("event 10".to_string())),
            "the oldest events drain"
        );
        assert_eq!(
            state.events.last(),
            Some(&ProgressEvent::Notice(format!(
                "event {}",
                EVENT_RING_CAPACITY + 9
            ))),
        );
    }

    #[test]
    fn stray_events_are_noops() {
        let mut state = state();
        assert!(advance(&mut state, SyncEvent::Dismissed).is_none());
        assert!(advance(&mut state, SyncEvent::Finished(completed_result())).is_none());
        assert_eq!(state.phase, SyncPhase::Idle);
    }

    /// A stub runner for the facade gate — canned results, no engine,
    /// no backend, no device.
    struct StubRunner;

    impl Runner for StubRunner {
        fn prepare(&self, _request: &PrepareRequest) -> Result<PreparedSync, String> {
            Ok(prepared())
        }

        fn execute(
            &self,
            prepared: PreparedSync,
            report: &(dyn Fn(ProgressEvent) + Send + Sync),
        ) -> SyncResult {
            report(ProgressEvent::Step {
                name: "Intent",
                index: 1,
                total: 6,
                began: true,
            });
            let _ = prepared;
            completed_result()
        }
    }

    #[test]
    fn the_facade_drives_a_real_run_on_real_threads() {
        let service = IpodSyncService::new(Arc::new(StubRunner));
        let receiver = service.subscribe();
        assert_eq!(service.snapshot().phase, SyncPhase::Idle);

        service.start(false, false, Path::new("/mnt/ipod"));
        wait_for(&receiver, |snapshot| {
            matches!(snapshot.phase, SyncPhase::Done(_))
        });
        assert_eq!(
            service.snapshot().phase,
            SyncPhase::Done(outcome_summary(&completed_result()))
        );
        // The ring caught the step event from the stub runner.
        assert_eq!(service.snapshot().events.len(), 1);

        // Dismissing the result returns to Idle (the page re-previews).
        service.dismiss();
        wait_for(&receiver, |snapshot| snapshot.phase == SyncPhase::Idle);
    }

    /// Drain the channel until `predicate` matches, with a deadline —
    /// the facade fans every transition out, so the interesting phase
    /// is never the first snapshot.
    fn wait_for(
        receiver: &async_channel::Receiver<SyncState>,
        predicate: impl Fn(&SyncState) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match receiver.try_recv() {
                Ok(snapshot) => {
                    if predicate(&snapshot) {
                        return;
                    }
                }
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no matching snapshot within 5 s"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("the subscriber channel closed prematurely")
                }
            }
        }
    }

    #[test]
    fn the_facade_preview_never_executes_and_a_run_starts_from_it() {
        let service = IpodSyncService::new(Arc::new(StubRunner));
        let receiver = service.subscribe();
        service.start(true, false, Path::new("/mnt/ipod"));
        wait_for(&receiver, |snapshot| {
            matches!(snapshot.phase, SyncPhase::Review { .. })
        });
        // The stub runner would finish in milliseconds if an execute
        // had been spawned; a grace proves the preview never ran it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(matches!(service.snapshot().phase, SyncPhase::Review { .. }));
        // A real run starts straight from the live preview.
        service.start(false, false, Path::new("/mnt/ipod"));
        wait_for(&receiver, |snapshot| {
            matches!(snapshot.phase, SyncPhase::Done(_))
        });
    }

    #[test]
    fn the_facade_runs_straight_through_on_a_real_start() {
        let service = IpodSyncService::new(Arc::new(StubRunner));
        service.start(false, false, Path::new("/mnt/ipod"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let SyncPhase::Done(_) = service.snapshot().phase {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the run did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The ring caught the step event from the stub runner.
        assert_eq!(service.snapshot().events.len(), 1);
    }
}
