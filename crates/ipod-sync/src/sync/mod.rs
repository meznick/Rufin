//! B-010: the sync executor's home (b010_plan §7). B-010a landed the
//! transcode decision and staging lifecycle first ([`transcode`]); the
//! merge and pending-store reader follow ([`merge`], [`import`]); the
//! durable coordination layer is [`journal`] + [`backup`] + [`recover`];
//! [`plan`], [`exclusion`], [`host`] and [`exec`] are the executor;
//! [`report`] is B-011's progress/outcome contract.

pub mod backup;
pub mod exclusion;
pub mod exec;
pub mod host;
pub mod import;
pub mod journal;
pub mod merge;
pub mod plan;
pub mod recover;
pub mod report;
pub mod transcode;
