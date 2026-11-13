//! B-009: dry-run delta calculation (see `deltas/b009_plan.md`).
//!
//! The dry run computes what a sync would do without changing anything:
//! the engine ([`compute`]) is pure — it consumes owned host/device inputs
//! and returns an owned report, performs no I/O, holds no session and
//! reads no clock — and the collectors ([`snapshot`]) use only the
//! read-side wrapper surface. Isolation is enforced structurally and
//! proven by before/after tree hashes in the env-gated gate.

pub mod compute;
pub mod flatten;
pub mod model;
pub mod snapshot;

pub use model::{
    ArtworkAdd, ArtworkRemove, CapacitySummary, CopyMode, CoverEvidence, CriteriaState,
    DeviceInputs, DeviceOnlyPolicy, DryRunReport, FileAdd, FileRemove, FileRepair, HostInputs,
    PlaylistCreate, PlaylistKind, PlaylistRemove, PlaylistUpdate, ReportContext, SourceKey,
    StatCounts, StatDiff, StatField, SyncOptions,
};
