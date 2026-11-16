//! B-010: the sync executor's home (b010_plan §7). B-010a landed the
//! transcode decision and staging lifecycle first ([`transcode`]); the
//! merge and pending-store reader follow ([`merge`], [`import`]); the
//! durable coordination layer is [`journal`] + [`backup`] + [`recover`].

pub mod backup;
pub mod import;
pub mod journal;
pub mod merge;
pub mod recover;
pub mod transcode;
