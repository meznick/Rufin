//! B-010: the sync executor's home (b010_plan §7). B-010a (the transcode
//! sub-task) lands the pure decision and staging lifecycle here first —
//! see [`transcode`].

pub mod transcode;
