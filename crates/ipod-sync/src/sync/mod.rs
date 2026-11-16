//! B-010: the sync executor's home (b010_plan §7). B-010a landed the
//! transcode decision and staging lifecycle first ([`transcode`]); the
//! merge and pending-store reader follow ([`merge`], [`import`]).

pub mod import;
pub mod merge;
pub mod transcode;
