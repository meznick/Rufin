//! B-010: recovery planning from the journal (b010_plan §5/§7).
//!
//! The executor scans the journal at sync start (and on Ready for the
//! device id) and picks the route: a fresh sync, a resume from the first
//! un-completed step (the watermarks make the re-import dedup-safe), a
//! verify-and-advance when the DB write already committed but completion
//! crashed, or a restore-from-backup when the device DB no longer parses.
//! The decision is pure — the device-state probe (does the DB parse) is
//! the caller's, through the read-side session.

use crate::sync::journal::OperationRecord;

/// What the next sync must do about an interrupted operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryPlan {
    /// No active operation — a fresh sync may start.
    None,
    /// Continue from the first un-completed step. Covers the crash before
    /// the backup existed too: the record's watermarks are the dedup
    /// memory that keeps the pending-store import idempotent.
    Resume(OperationRecord),
    /// The DB write step completed but the operation never finished:
    /// re-verify the device, then advance to completion.
    VerifyAndAdvance(OperationRecord),
    /// The device DB does not parse: restore the consistency set from the
    /// recorded backup, then resume the operation from the backup step.
    Restore(OperationRecord),
}

/// Plan the recovery. `db_parses` = a fresh read-side session opened on
/// the device without error (the executor's probe).
pub fn plan_recovery(active: Option<OperationRecord>, db_parses: bool) -> RecoveryPlan {
    let Some(record) = active else {
        return RecoveryPlan::None;
    };
    if record.backup_dir.is_none() {
        // Crashed before the backup existed: write ordering guarantees the
        // device was never written, so the device is consistent — but the
        // host write may already have applied the watermarked deltas, so
        // the record (with its watermarks) must survive the resume.
        return RecoveryPlan::Resume(record);
    }
    if !db_parses {
        return RecoveryPlan::Restore(record);
    }
    if record.completed_steps.iter().any(|step| step == "DbWrite") {
        return RecoveryPlan::VerifyAndAdvance(record);
    }
    RecoveryPlan::Resume(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(steps: &[&str], backup: bool) -> OperationRecord {
        let mut record = OperationRecord::new("device-1".into(), "plan-hash".into(), 1000);
        for step in steps {
            record.step_done(step);
        }
        if backup {
            record.backup_dir = Some("backup".into());
        }
        record
    }

    #[test]
    fn no_active_operation_is_a_fresh_start() {
        assert_eq!(plan_recovery(None, true), RecoveryPlan::None);
    }

    #[test]
    fn an_interrupted_operation_resumes() {
        let active = record(&["Intent", "Backup", "HostWrite"], true);
        assert_eq!(
            plan_recovery(Some(active.clone()), true),
            RecoveryPlan::Resume(active)
        );
    }

    #[test]
    fn a_crash_before_the_backup_keeps_the_watermarks() {
        // backup_dir None: the device was never written, but the record
        // (and its dedup watermarks) must survive — resume, not discard.
        let active = record(&["Intent", "HostWrite"], false);
        assert_eq!(
            plan_recovery(Some(active.clone()), true),
            RecoveryPlan::Resume(active)
        );
    }

    #[test]
    fn a_broken_db_restores_from_the_backup() {
        let active = record(&["Intent", "Backup", "FileOps"], true);
        assert_eq!(
            plan_recovery(Some(active.clone()), false),
            RecoveryPlan::Restore(active)
        );
    }

    #[test]
    fn a_committed_db_verify_advances() {
        let active = record(&["Intent", "Backup", "FileOps", "DbWrite"], true);
        assert_eq!(
            plan_recovery(Some(active.clone()), true),
            RecoveryPlan::VerifyAndAdvance(active.clone())
        );
        // Even with a broken DB the restore wins over verify-and-advance.
        assert_eq!(
            plan_recovery(Some(active.clone()), false),
            RecoveryPlan::Restore(active)
        );
    }
}
