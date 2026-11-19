//! B-015: the sync boundary service (b015_plan §3.2).
//!
//! `IpodSyncService` owns the sync lifecycle: the gating decision rides
//! the connection snapshot ([`crate::ipod_page::sync_controls`]), while
//! this service runs the engine. A **per-run thread** does the blocking
//! work (the `.itl` parse, the device parse, the engine transaction) —
//! never the connection worker, whose serialized FIFO must stay free
//! for scans and notices (the b006 latency debt).
//!
//! The engine seam is the injectable [`Runner`] trait: production uses
//! [`EngineRunner`] over a [`SyncBoundary`] (the shell's wiring of the
//! selected iTunes source); the gates inject stubs, so every phase
//! transition is display-free tested. The pure [`advance`] table carries
//! the semantics — including the owner's S-08 delta: with
//! [`ReviewPolicy::Always`] (dev builds) a non-dry prepared sync stops
//! at `Review` for the user's dialog approval; with
//! [`ReviewPolicy::Never`] (release) the click is the approval and the
//! run starts immediately. Both policies are gated in the same test
//! profile — the policy is a parameter, not a `cfg`, so neither path is
//! untested.
//!
//! Backend loading happens **on the run thread**: `IpodBackend` is not
//! `Send` (raw symbol tables), so each phase loads its own instance —
//! `dlopen` is refcounted and process-global, the pinned ABI check runs
//! identically per load, and the double load costs milliseconds against
//! a minutes-long sync. The soft-accept policy is applied at every load:
//! an unverified ABI refuses with the same diagnostic the FSM shows
//! (`ipod_discovery::ABI_UNVERIFIED_DIAGNOSTIC`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use ipod::{ArtworkInput, BackendSearch, DeviceSession, IpodBackend};
use ipod_sync::dry_run::{
    ArtworkProbe, CoverEvidence, DryRunReport, HostInputs, SourceKey, SyncOptions, collect_device,
    collect_host, compute,
};
use ipod_sync::retrieval::{DeviceFacts, collect};
use ipod_sync::sync::exec::run_sync;
use ipod_sync::sync::host::HostWriteContext;
use ipod_sync::sync::merge::StatMerge;
use ipod_sync::sync::plan::ExecutionPlan;
use ipod_sync::sync::report::{ProgressEvent, SyncChanges, SyncResult, SyncStatus};
use itl_core::ItlFile;

use crate::ipod_discovery::ABI_UNVERIFIED_DIAGNOSTIC;

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
            ..SyncOptions::default()
        };
        let report = compute(&host, &device, &options);
        Ok(PreparedSync {
            report,
            host,
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
    /// The dry run is shown and awaits the user's decision (dev flow).
    /// `dry_run` marks the S-08 deliverable review — its dialog offers
    /// no approval, and the machine refuses one.
    Review {
        summary: ReviewSummary,
        dry_run: bool,
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
}

/// Whether a real (non-dry) run may proceed straight from prepare
/// (the owner's S-08 delta, `deltas/b015_delta.md`): `Always` in dev
/// builds (review every run), `Never` in release (the click is the
/// approval). A parameter, not a `cfg` — both arms are gated in the
/// same profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewPolicy {
    Always,
    Never,
}

/// The production policy (the S-08 delta): dev builds review every run,
/// release builds run on the click. One place — the shell wires both
/// the service and the page with it.
pub fn review_policy() -> ReviewPolicy {
    if cfg!(debug_assertions) {
        ReviewPolicy::Always
    } else {
        ReviewPolicy::Never
    }
}

/// One service-internal transition input.
#[derive(Debug)]
pub enum SyncEvent {
    /// A Sync click: the device mount at click time.
    Start { dry_run: bool, mount: PathBuf },
    /// The run thread finished collecting.
    Prepared {
        dry_run: bool,
        result: Result<PreparedSync, String>,
    },
    /// The user approved the reviewed plan (dev flow) — the prepared
    /// sync rides the event, so `advance` stays payload-aware and the
    /// approval cannot run a stale or fabricated plan.
    Approved(PreparedSync),
    /// The user dismissed the review or the result.
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
    Prepare { dry_run: bool, mount: PathBuf },
    /// Spawn the execute thread with the prepared sync (boxed — the
    /// prepared inputs dwarf the other variants).
    Execute(Box<PreparedSync>),
}

/// The pure transition table. Every semantic of the service lives here;
/// the facade applies actions and fans snapshots out.
pub fn advance(
    state: &mut SyncState,
    event: SyncEvent,
    policy: ReviewPolicy,
) -> Option<SyncAction> {
    match (&state.phase, event) {
        (SyncPhase::Idle, SyncEvent::Start { dry_run, mount }) => {
            state.phase = SyncPhase::Collecting;
            Some(SyncAction::Prepare { dry_run, mount })
        }
        (
            SyncPhase::Collecting,
            SyncEvent::Prepared {
                dry_run: true,
                result: Ok(prepared),
            },
        ) => {
            // A dry run always stops at the review: the dialog IS the
            // deliverable (S-08) — and it carries no approval.
            state.phase = SyncPhase::Review {
                summary: review_summary(&prepared.report),
                dry_run: true,
            };
            None
        }
        (
            SyncPhase::Collecting,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared),
            },
        ) => match policy {
            ReviewPolicy::Always => {
                state.phase = SyncPhase::Review {
                    summary: review_summary(&prepared.report),
                    dry_run: false,
                };
                None
            }
            ReviewPolicy::Never => {
                state.phase = SyncPhase::Running;
                Some(SyncAction::Execute(Box::new(prepared)))
            }
        },
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
        // A dry review carries no approval: the S-08 deliverable cannot
        // run the engine, whatever calls `approve`.
        (SyncPhase::Review { dry_run: true, .. }, SyncEvent::Approved(_)) => None,
        (SyncPhase::Review { dry_run: false, .. }, SyncEvent::Approved(prepared)) => {
            state.phase = SyncPhase::Running;
            Some(SyncAction::Execute(Box::new(prepared)))
        }
        (SyncPhase::Review { .. }, SyncEvent::Dismissed) => {
            state.phase = SyncPhase::Idle;
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
            state.events.push(event);
            if state.events.len() > EVENT_RING_CAPACITY {
                let excess = state.events.len() - EVENT_RING_CAPACITY;
                state.events.drain(..excess);
            }
            None
        }
        // Every other combination is a no-op: a stray start while
        // running, an approve outside Review, a finished outside
        // Running, progress in any phase.
        _ => None,
    }
}

/// The review dialog's content, derived from a dry-run report — pure
/// counts, no wording (the widget layer translates).
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
    /// The report's own findings (mapping + dry-run diagnostics),
    /// capped at the dialog's rendering budget — verbatim, so a
    /// phantom diff names its own reason (b015_fixes F6).
    pub diagnostics: Vec<String>,
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

/// The pure derivation the review dialog renders.
pub fn review_summary(report: &DryRunReport) -> ReviewSummary {
    ReviewSummary::from_report(report)
}

/// The result dialog's content: the engine outcome plus the boundary
/// failure shape (a `Prepare`-step failure that never reached the
/// engine).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcomeSummary {
    pub status: SyncStatus,
    pub changes: SyncChanges,
    pub diagnostics: Vec<String>,
}

impl SyncOutcomeSummary {
    fn failed(step: &'static str, diagnostic: String) -> Self {
        Self {
            status: SyncStatus::Failed { step, diagnostic },
            changes: SyncChanges::default(),
            diagnostics: Vec::new(),
        }
    }
}

/// The pure extraction the result dialog renders.
pub fn outcome_summary(result: &SyncResult) -> SyncOutcomeSummary {
    SyncOutcomeSummary {
        status: result.status.clone(),
        changes: result.changes.clone(),
        diagnostics: result.diagnostics.clone(),
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
    /// The prepared sync between `Review` and `Approved` (the plan the
    /// approval runs).
    pending: Mutex<Option<PreparedSync>>,
    subscribers: Mutex<Vec<async_channel::Sender<SyncState>>>,
    runner: Arc<dyn Runner>,
    policy: ReviewPolicy,
}

impl ServiceInner {
    /// One transition: advance, fan out, act. Lock ordering matches
    /// `ipod_connection`'s `publish` (state, then subscribers), so the
    /// run thread's progress fan-in cannot deadlock with a racing
    /// subscriber. Lives on the inner value so the run threads — which
    /// hold `Arc<ServiceInner>` — can apply their own results.
    fn apply(self: &Arc<Self>, event: SyncEvent) {
        // A prepared payload entering the review is stashed for the
        // approval (one clone per run — the review path is dev-only).
        let stash = match &event {
            SyncEvent::Prepared {
                result: Ok(prepared),
                ..
            } => Some(prepared.clone()),
            _ => None,
        };
        let action = {
            let mut state = self.state.lock().expect("sync state lock poisoned");
            let action = advance(&mut state, event, self.policy);
            // The prepared payload survives only while a review holds
            // it; every other phase drops it (dismissal, the release
            // straight-through, a finished run).
            *self.pending.lock().expect("pending lock poisoned") =
                if matches!(state.phase, SyncPhase::Review { .. }) {
                    stash
                } else {
                    None
                };
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
            Some(SyncAction::Prepare { dry_run, mount }) => {
                let runner = self.runner.clone();
                let inner = self.clone();
                thread::Builder::new()
                    .name("ipod-sync-prepare".to_string())
                    .spawn(move || {
                        let result = runner.prepare(&PrepareRequest { mount });
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
    pub fn new(runner: Arc<dyn Runner>, policy: ReviewPolicy) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                state: Mutex::new(SyncState::default()),
                pending: Mutex::new(None),
                subscribers: Mutex::new(Vec::new()),
                runner,
                policy,
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

    /// A Sync click: `dry_run` is the dev checkbox state (release never
    /// passes `true` — the checkbox is not compiled); `mount` is the
    /// current device's mount point. A no-op outside `Idle`/`Done`.
    pub fn start(&self, dry_run: bool, mount: &Path) {
        self.inner.apply(SyncEvent::Start {
            dry_run,
            mount: mount.to_path_buf(),
        });
    }

    /// The review dialog's Sync button (dev flow): moves the pending
    /// prepared sync into the approval event. A dry review refuses —
    /// the S-08 deliverable has no approval path, however `approve` is
    /// called.
    pub fn approve(&self) {
        let state = self.inner.state.lock().expect("sync state lock poisoned");
        if !matches!(state.phase, SyncPhase::Review { dry_run: false, .. }) {
            return;
        }
        drop(state);
        let prepared = self
            .inner
            .pending
            .lock()
            .expect("pending lock poisoned")
            .take();
        if let Some(prepared) = prepared {
            self.inner.apply(SyncEvent::Approved(prepared));
        }
    }

    /// The review dialog's Close / the result dialog's OK.
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
        let summary = review_summary(&report_with(true));
        assert_eq!(summary.files_added, 1);
        assert_eq!(summary.playlists_created, 1);
        assert_eq!(summary.artwork_adds, 1);
        assert_eq!(summary.stat_updates, 6);
        assert_eq!(summary.estimate_bytes, 2_000);
        assert_eq!(summary.free_bytes, 10_000);
        assert!(summary.would_fit);
        assert!(!summary.nothing_to_do);

        let empty = review_summary(&report_with(false));
        assert!(empty.nothing_to_do, "no actions anywhere");
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
            ReviewPolicy::Always,
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
            })
        );
    }

    #[test]
    fn start_only_from_idle_and_dry_always_reviews() {
        let mut machine = state();
        let action = advance(
            &mut machine,
            SyncEvent::Start {
                dry_run: true,
                mount: PathBuf::from("/mnt/ipod"),
            },
            ReviewPolicy::Never,
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
                mount: PathBuf::from("/mnt/ipod"),
            },
            ReviewPolicy::Always,
        );
        assert!(action.is_none());
        assert_eq!(machine.phase, SyncPhase::Collecting);

        // A dry prepared sync reviews under both policies.
        for policy in [ReviewPolicy::Always, ReviewPolicy::Never] {
            let mut policy_state = state();
            policy_state.phase = SyncPhase::Collecting;
            let action = advance(
                &mut policy_state,
                SyncEvent::Prepared {
                    dry_run: true,
                    result: Ok(prepared()),
                },
                policy,
            );
            assert!(action.is_none(), "{policy:?}");
            assert!(
                matches!(policy_state.phase, SyncPhase::Review { .. }),
                "{policy:?}: the dry run stops at the review"
            );
        }
    }

    #[test]
    fn a_real_prepared_sync_reviews_or_runs_by_policy() {
        let mut always_state = state();
        always_state.phase = SyncPhase::Collecting;
        let action = advance(
            &mut always_state,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared()),
            },
            ReviewPolicy::Always,
        );
        assert!(action.is_none());
        assert!(matches!(always_state.phase, SyncPhase::Review { .. }));

        let mut never_state = state();
        never_state.phase = SyncPhase::Collecting;
        let action = advance(
            &mut never_state,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared()),
            },
            ReviewPolicy::Never,
        );
        assert_eq!(never_state.phase, SyncPhase::Running);
        assert!(matches!(action, Some(SyncAction::Execute(_))));
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
            ReviewPolicy::Always,
        );

        // Dismissing the review returns to Idle.
        advance(&mut state, SyncEvent::Dismissed, ReviewPolicy::Always);
        assert_eq!(state.phase, SyncPhase::Idle);

        // The full dev flow: start → review → approve → running → done.
        advance(
            &mut state,
            SyncEvent::Start {
                dry_run: false,
                mount: PathBuf::from("/mnt/ipod"),
            },
            ReviewPolicy::Always,
        );
        advance(
            &mut state,
            SyncEvent::Prepared {
                dry_run: false,
                result: Ok(prepared()),
            },
            ReviewPolicy::Always,
        );
        advance(
            &mut state,
            SyncEvent::Approved(prepared()),
            ReviewPolicy::Always,
        );
        assert_eq!(state.phase, SyncPhase::Running);
        advance(
            &mut state,
            SyncEvent::Finished(completed_result()),
            ReviewPolicy::Always,
        );
        assert_eq!(
            state.phase,
            SyncPhase::Done(outcome_summary(&completed_result()))
        );
        // Dismissing the result returns to Idle.
        advance(&mut state, SyncEvent::Dismissed, ReviewPolicy::Always);
        assert_eq!(state.phase, SyncPhase::Idle);
    }

    #[test]
    fn progress_events_append_and_the_ring_is_bounded() {
        let mut state = state();
        for index in 0..EVENT_RING_CAPACITY + 10 {
            advance(
                &mut state,
                SyncEvent::Progress(ProgressEvent::Notice(format!("event {index}"))),
                ReviewPolicy::Always,
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
    fn a_dry_review_refuses_approval() {
        let mut state = state();
        state.phase = SyncPhase::Collecting;
        advance(
            &mut state,
            SyncEvent::Prepared {
                dry_run: true,
                result: Ok(prepared()),
            },
            ReviewPolicy::Always,
        );
        assert!(matches!(
            state.phase,
            SyncPhase::Review { dry_run: true, .. }
        ));
        // The machine refuses the approval and stays on the review —
        // the S-08 deliverable has no run path.
        assert!(
            advance(
                &mut state,
                SyncEvent::Approved(prepared()),
                ReviewPolicy::Always
            )
            .is_none()
        );
        assert!(matches!(
            state.phase,
            SyncPhase::Review { dry_run: true, .. }
        ));
        // Dismissing still works.
        advance(&mut state, SyncEvent::Dismissed, ReviewPolicy::Always);
        assert_eq!(state.phase, SyncPhase::Idle);
    }

    #[test]
    fn stray_events_are_noops() {
        let mut state = state();
        assert!(
            advance(
                &mut state,
                SyncEvent::Approved(prepared()),
                ReviewPolicy::Always
            )
            .is_none()
        );
        assert!(advance(&mut state, SyncEvent::Dismissed, ReviewPolicy::Always).is_none());
        assert!(
            advance(
                &mut state,
                SyncEvent::Finished(completed_result()),
                ReviewPolicy::Always
            )
            .is_none()
        );
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
    fn the_facade_drives_the_full_dev_flow_on_real_threads() {
        let service = IpodSyncService::new(Arc::new(StubRunner), ReviewPolicy::Always);
        let receiver = service.subscribe();
        assert_eq!(service.snapshot().phase, SyncPhase::Idle);

        service.start(false, Path::new("/mnt/ipod"));
        wait_for(&receiver, |snapshot| {
            matches!(snapshot.phase, SyncPhase::Review { .. })
        });

        service.approve();
        wait_for(&receiver, |snapshot| {
            matches!(snapshot.phase, SyncPhase::Done(_))
        });
        assert_eq!(
            service.snapshot().phase,
            SyncPhase::Done(outcome_summary(&completed_result()))
        );
        // The ring caught the step event from the stub runner.
        assert_eq!(service.snapshot().events.len(), 1);
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
    fn the_facade_approval_ignores_a_dry_review() {
        let service = IpodSyncService::new(Arc::new(StubRunner), ReviewPolicy::Always);
        let receiver = service.subscribe();
        service.start(true, Path::new("/mnt/ipod"));
        wait_for(&receiver, |snapshot| {
            matches!(snapshot.phase, SyncPhase::Review { .. })
        });
        service.approve();
        // The stub runner would finish in milliseconds; a wrongly
        // approved dry review would already be Done here. A short grace
        // proves the approval never spawned it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(matches!(
            service.snapshot().phase,
            SyncPhase::Review { dry_run: true, .. }
        ));
        service.dismiss();
        wait_for(&receiver, |snapshot| snapshot.phase == SyncPhase::Idle);
    }

    #[test]
    fn the_facade_runs_straight_through_under_the_release_policy() {
        let service = IpodSyncService::new(Arc::new(StubRunner), ReviewPolicy::Never);
        service.start(false, Path::new("/mnt/ipod"));
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
