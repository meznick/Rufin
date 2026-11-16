//! B-010: the execution plan (b010_plan §7). The dry-run report is the
//! input contract; this module carries the actionable lists verbatim,
//! joins them with the stat merge, and computes the **plan hash** — a
//! stable FNV-1a digest over the actions (never the diagnostics) that the
//! executor compares against a freshly re-run dry run before the first
//! write, and that the journal records for recovery revalidation.

use crate::dry_run::{
    ArtworkAdd, DryRunReport, FileAdd, FileRemove, FileRepair, PlaylistCreate, PlaylistRemove,
    PlaylistUpdate,
};
use crate::sync::merge::StatMerge;

/// The ordered, actionable operation the executor runs.
#[derive(Clone, Debug, Default)]
pub struct ExecutionPlan {
    pub hash: String,
    pub stat_merge: StatMerge,
    pub file_adds: Vec<FileAdd>,
    pub file_repairs: Vec<FileRepair>,
    pub file_removes: Vec<FileRemove>,
    pub playlist_creates: Vec<PlaylistCreate>,
    pub playlist_updates: Vec<PlaylistUpdate>,
    pub playlist_removes: Vec<PlaylistRemove>,
    pub artwork_adds: Vec<ArtworkAdd>,
}

impl ExecutionPlan {
    pub fn build(report: &DryRunReport, stat_merge: StatMerge) -> Self {
        Self {
            hash: plan_hash(report),
            stat_merge,
            file_adds: report.file_adds.clone(),
            file_repairs: report.file_repairs.clone(),
            file_removes: report.file_removes.clone(),
            playlist_creates: report.playlist_creates.clone(),
            playlist_updates: report.playlist_updates.clone(),
            playlist_removes: report.playlist_removes.clone(),
            artwork_adds: report.artwork_adds.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stat_merge.host_patches.is_empty()
            && self.stat_merge.device_patches.is_empty()
            && self.file_adds.is_empty()
            && self.file_repairs.is_empty()
            && self.file_removes.is_empty()
            && self.playlist_creates.is_empty()
            && self.playlist_updates.is_empty()
            && self.playlist_removes.is_empty()
            && self.artwork_adds.is_empty()
    }
}

/// FNV-1a 64 — stable across builds and processes, so a journal written
/// by one run revalidates under the next.
struct Fnv1a(u64);

impl Fnv1a {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    fn write_str(&mut self, value: &str) {
        self.write(value.as_bytes());
        self.write(&[0]);
    }

    fn finish(self) -> u64 {
        self.0
    }
}

/// The stable plan hash: a digest over every action entry in the report,
/// in the report's own sorted order. Diagnostics and informational lists
/// are deliberately excluded — they cannot change an action.
pub fn plan_hash(report: &DryRunReport) -> String {
    let mut hasher = Fnv1a::new();
    for add in &report.file_adds {
        hasher.write_u64(add.pid);
        hasher.write_u64(add.size_estimate);
        hasher.write(&(add.mode as u8).to_le_bytes());
        hasher.write_str(&add.host_path.to_string_lossy());
    }
    for repair in &report.file_repairs {
        hasher.write_u64(repair.dbid);
        hasher.write(&(repair.mode as u8).to_le_bytes());
        hasher.write_str(&repair.device_path.to_string_lossy());
    }
    for remove in &report.file_removes {
        hasher.write_u64(remove.dbid);
    }
    for diff in &report.stat_diffs {
        hasher.write_u64(diff.pid);
        hasher.write(&(diff.field as u8).to_le_bytes());
        hasher.write(&diff.host_raw.to_le_bytes());
        hasher.write(&diff.device_raw.to_le_bytes());
    }
    for create in &report.playlist_creates {
        hasher.write_u64(create.host_pid);
        hasher.write(&(create.kind as u8).to_le_bytes());
        if let Some(name) = &create.name {
            hasher.write_str(name);
        }
    }
    for update in &report.playlist_updates {
        hasher.write_u64(update.device_id);
        hasher.write_u64(update.members_add.len() as u64);
        hasher.write_u64(update.members_remove.len() as u64);
        hasher.write(&(update.members_reorder as u8).to_le_bytes());
        hasher.write(&(update.criteria.clone() as u8).to_le_bytes());
    }
    for remove in &report.playlist_removes {
        hasher.write_u64(remove.device_id);
    }
    for add in &report.artwork_adds {
        hasher.write_u64(add.pid);
    }
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_deterministic_and_content_sensitive() {
        let mut report = DryRunReport::default();
        report.file_adds.push(FileAdd {
            itl_id: 1,
            pid: 0xA1,
            host_path: "/x/y.m4a".into(),
            mode: crate::dry_run::CopyMode::TranscodeToAac,
            size_estimate: 1000,
        });
        let first = plan_hash(&report);
        let second = plan_hash(&report);
        assert_eq!(first, second, "stable across calls");
        report.file_adds[0].pid = 0xA2;
        assert_ne!(first, plan_hash(&report), "content-sensitive");
        // Diagnostics never change the hash.
        let mut with_diag = report.clone();
        with_diag.diagnostics.push("anything".into());
        assert_eq!(plan_hash(&report), plan_hash(&with_diag));
    }
}
