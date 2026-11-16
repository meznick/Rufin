//! B-010: the durable operation journal (b010_plan §5 step 2/8, §7).
//!
//! Lives on the device, in a Rufin-owned directory the firmware and iTunes
//! ignore (`iPod_Control/Rufin/`, owner decision §11-8). Exactly one
//! active operation record at a time — the device is single-writer by the
//! executor's lock — plus a single last-success marker; a failed
//! operation's record is kept once for diagnostics. No sync-history
//! archive: the merged values are already durable in the host `.itl` and
//! the device DB, so history is dead weight.
//!
//! Every write is atomic (temp + rename) and fsynced; the journal is the
//! recovery truth — it never depends on the connection worker's notice
//! queue (B-006 debt). Accepted consequence (owner decision §11-8): a
//! device restore or corruption destroys the recovery info together with
//! the data it would recover.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The Rufin-owned directory on the device's data partition.
pub const JOURNAL_DIR: &str = "iPod_Control/Rufin";
/// The active (in-flight) operation record.
pub const ACTIVE_FILE: &str = "operation.json";
/// The last successfully completed operation (the UI's Last Sync source).
pub const LAST_SUCCESS_FILE: &str = "last-success.json";
/// A failed operation's record, kept once for diagnostics.
pub const FAILED_FILE: &str = "failed.json";

/// Per-dbid applied-delta watermarks — the additive merge's dedup memory
/// (b010_plan §6): a retry diffs the pending-store entries against these
/// and applies only entries newer than the watermark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub dbid: u64,
    pub plays: u32,
    pub skips: u32,
}

/// The in-flight operation record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    /// Device identity (FireWire ID/serial when present, else the
    /// master-pid+model hash) — the record belongs to this device only.
    pub device: String,
    /// Hash of the approved dry-run report the operation executes.
    pub plan_hash: String,
    /// Unix seconds at journal creation (last-success source).
    pub started_unix: i64,
    /// Step names completed so far, in order (the executor's resumption
    /// points: `Intent`, `Backup`, `HostWrite`, `FileOps`, `DbWrite`,
    /// `Verify`).
    pub completed_steps: Vec<String>,
    /// The applied delta set, written before the host write.
    pub watermarks: Vec<Watermark>,
    /// The consistency-set backup directory, relative to the journal
    /// root; present from the backup step on.
    pub backup_dir: Option<String>,
}

impl OperationRecord {
    pub fn new(device: String, plan_hash: String, started_unix: i64) -> Self {
        Self {
            device,
            plan_hash,
            started_unix,
            completed_steps: Vec::new(),
            watermarks: Vec::new(),
            backup_dir: None,
        }
    }

    pub fn step_done(&mut self, step: &str) {
        if !self.completed_steps.iter().any(|done| done == step) {
            self.completed_steps.push(step.to_string());
        }
    }
}

/// The journal's last-success marker (kept even when the active record is
/// cleared, so the UI always has a Last Sync source).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSuccess {
    pub device: String,
    pub plan_hash: String,
    pub completed_unix: i64,
}

/// Handle on the device's journal directory.
#[derive(Clone, Debug)]
pub struct Journal {
    root: PathBuf,
}

impl Journal {
    /// The journal root for a mount point.
    pub fn device_root(mount: &Path) -> PathBuf {
        mount.join(JOURNAL_DIR)
    }

    /// Open (create if needed) the journal directory under `mount`.
    pub fn open(mount: &Path) -> io::Result<Self> {
        let root = Self::device_root(mount);
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load the active operation, if one exists.
    pub fn load_active(&self) -> io::Result<Option<OperationRecord>> {
        let path = self.root.join(ACTIVE_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::other(format!("journal is corrupt: {error}"))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Persist the active record atomically (temp + rename + fsync file
    /// and directory).
    pub fn write(&self, record: &OperationRecord) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| io::Error::other(format!("journal serialization: {error}")))?;
        self.atomic_write(ACTIVE_FILE, &bytes)
    }

    /// The completion step: rotate the active record into the
    /// last-success marker and clear the active file (b010_plan §5 step 8 —
    /// last-success advances only here).
    pub fn complete(&self, completed_unix: i64) -> io::Result<()> {
        let active = self.load_active()?.ok_or_else(|| {
            io::Error::other("cannot complete: no active operation in the journal")
        })?;
        let marker = LastSuccess {
            device: active.device,
            plan_hash: active.plan_hash,
            completed_unix,
        };
        let bytes = serde_json::to_vec_pretty(&marker)
            .map_err(|error| io::Error::other(format!("journal serialization: {error}")))?;
        self.atomic_write(LAST_SUCCESS_FILE, &bytes)?;
        match std::fs::remove_file(self.root.join(ACTIVE_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// A failed operation: keep the record once for diagnostics (the
    /// active slot clears so the next sync can start clean).
    pub fn mark_failed(&self) -> io::Result<()> {
        let active = self.load_active()?;
        let Some(record) = active else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|error| io::Error::other(format!("journal serialization: {error}")))?;
        self.atomic_write(FAILED_FILE, &bytes)?;
        match std::fs::remove_file(self.root.join(ACTIVE_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn load_last_success(&self) -> io::Result<Option<LastSuccess>> {
        let path = self.root.join(LAST_SUCCESS_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| io::Error::other(format!("last-success is corrupt: {error}"))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn atomic_write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let target = self.root.join(name);
        let temp = self.root.join(format!(".{name}.tmp"));
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, &target)?;
        // Fsync the directory so the rename itself is durable.
        if let Ok(dir) = std::fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(step: &str) -> OperationRecord {
        let mut record = OperationRecord::new("device-1".into(), "plan-hash".into(), 1000);
        record.step_done(step);
        record
    }

    #[test]
    fn atomic_round_trip_and_completion() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("pod");
        std::fs::create_dir_all(&mount).unwrap();
        let journal = Journal::open(&mount).unwrap();
        assert!(journal.load_active().unwrap().is_none());

        journal.write(&record("Backup")).unwrap();
        let loaded = journal.load_active().unwrap().expect("active record");
        assert_eq!(loaded.device, "device-1");
        assert_eq!(loaded.completed_steps, vec!["Backup".to_string()]);
        assert_eq!(journal.load_last_success().unwrap(), None);

        journal.complete(2000).unwrap();
        assert!(
            journal.load_active().unwrap().is_none(),
            "active clears on completion"
        );
        let marker = journal
            .load_last_success()
            .unwrap()
            .expect("last-success marker");
        assert_eq!(marker.completed_unix, 2000);
        assert_eq!(marker.plan_hash, "plan-hash");

        // No leftover temp files.
        let entries: Vec<String> = std::fs::read_dir(journal.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!entries.iter().any(|name| name.contains(".tmp")));
    }

    #[test]
    fn a_crash_keeps_the_active_record_resumable() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Journal::open(temp.path()).unwrap();
        journal.write(&record("HostWrite")).unwrap();
        // "Crash": reopen without completing.
        let journal = Journal::open(temp.path()).unwrap();
        let active = journal.load_active().unwrap().expect("the record survives");
        assert_eq!(active.completed_steps, vec!["HostWrite".to_string()]);
    }

    #[test]
    fn failed_records_are_kept_once_for_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Journal::open(temp.path()).unwrap();
        journal.write(&record("FileOps")).unwrap();
        journal.mark_failed().unwrap();
        assert!(journal.load_active().unwrap().is_none());
        let failed: OperationRecord =
            serde_json::from_slice(&std::fs::read(journal.root().join(FAILED_FILE)).unwrap())
                .unwrap();
        assert_eq!(failed.completed_steps, vec!["FileOps".to_string()]);
    }

    #[test]
    fn watermarks_survive_the_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Journal::open(temp.path()).unwrap();
        let mut record = record("Intent");
        record.watermarks = vec![
            Watermark {
                dbid: 0xA1,
                plays: 3,
                skips: 1,
            },
            Watermark {
                dbid: 0xA2,
                plays: 0,
                skips: 2,
            },
        ];
        journal.write(&record).unwrap();
        let loaded = journal.load_active().unwrap().unwrap();
        assert_eq!(loaded.watermarks, record.watermarks);
    }

    #[test]
    fn corrupt_journal_is_an_error_not_a_silent_resume() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Journal::open(temp.path()).unwrap();
        std::fs::write(journal.root().join(ACTIVE_FILE), b"{not json").unwrap();
        let error = journal.load_active().unwrap_err();
        assert!(error.to_string().contains("corrupt"));
    }
}
