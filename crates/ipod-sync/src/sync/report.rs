//! B-011: the progress/outcome contract (b011_plan §3/§4/§7) — the data
//! model the executor emits and the UI (B-012..B-016) consumes. Pure
//! types plus the advisory event pipe; no device I/O here.
//!
//! Two layers, deliberately:
//!
//! - The event stream ([`ProgressEvent`]) is **advisory**: it may drop
//!   events under backpressure and the UI must render the terminal state
//!   from the returned [`SyncResult`], never from the last event.
//! - [`SyncResult`] is **authoritative**: the three S-10 outcomes as one
//!   value, with what changed, what was reported, what recovery work is
//!   pending ([`RecoveryState`]), and the last-success marker.

use crate::sync::journal::LastSuccess;

/// One progress event from the executor (b011_plan §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgressEvent {
    /// A protocol-step boundary. `began` false = the step committed
    /// (journal write done), true = about to start.
    Step {
        name: &'static str,
        index: u8,
        total: u8,
        began: bool,
    },
    /// One work item inside the current step. `total == 0` marks an
    /// indeterminate phase (device parses, the staged write) — the UI
    /// shows a pulse, not a fraction.
    Item {
        current: u32,
        total: u32,
        label: String,
    },
    /// A diagnostic produced mid-run (mirrors the outcome's diagnostics).
    Notice(String),
}

/// The UI-side snapshot the executor's events fold into (b011_plan §3).
/// Defensive: counters are monotone under any event order, so a dropped
/// or reordered event can never rewind the bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncProgress {
    pub step: Option<&'static str>,
    pub step_index: u8,
    pub step_total: u8,
    pub item_current: u32,
    pub item_total: u32,
    pub item_label: Option<String>,
    /// Completed determinate items across the whole run (a running max).
    pub completed_items: u32,
}

impl SyncProgress {
    /// Apply one event, preserving the monotonicity invariants.
    pub fn fold(&mut self, event: &ProgressEvent) {
        match event {
            ProgressEvent::Step {
                name,
                index,
                total,
                began,
            } => {
                if *began {
                    self.step = Some(name);
                    self.item_current = 0;
                    self.item_total = 0;
                    self.item_label = None;
                }
                self.step_index = self.step_index.max(*index);
                self.step_total = self.step_total.max(*total);
            }
            ProgressEvent::Item {
                current,
                total,
                label,
            } => {
                self.item_label = Some(label.clone());
                if *total == 0 {
                    return;
                }
                self.item_current = self.item_current.max(*current);
                self.item_total = self.item_total.max(*total);
                self.completed_items = self.completed_items.max(*current);
            }
            ProgressEvent::Notice(_) => {}
        }
    }
}

/// Pending recovery work a result references (b011_plan §3) — the model
/// that replaces `gtk-library`'s `OperationNotice` placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryState {
    /// An interrupted operation's record is active; the next sync resumes
    /// from these journal steps.
    Resumable { completed_steps: Vec<&'static str> },
    /// The consistency set was restored from the backup; the interrupted
    /// operation is recorded failed.
    Restored,
    /// A failed operation's record is retained for diagnostics.
    FailedRecord,
    /// The journal itself was unreadable at reporting time.
    Unavailable(String),
}

/// The three outcomes S-10 distinguishes (b011_plan §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Completed,
    Partial {
        failed_step: &'static str,
        diagnostic: String,
    },
    Failed {
        step: &'static str,
        diagnostic: String,
    },
}

/// What the run actually wrote (the B-010 outcome counts).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncChanges {
    pub host_patches_written: usize,
    pub device_patches_written: usize,
    pub files_added: usize,
    pub files_removed: usize,
    pub playlists_written: usize,
    pub artwork_added: usize,
}

/// The authoritative terminal state of a sync run (b011_plan §3/§4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncResult {
    pub status: SyncStatus,
    pub changes: SyncChanges,
    pub diagnostics: Vec<String>,
    /// Pending recovery work; `None` when the device is clean.
    pub recovery: Option<RecoveryState>,
    /// The last-success marker as of return time — fresh after
    /// `Completed`, the previous marker otherwise.
    pub last_success: Option<LastSuccess>,
}

/// Bounded event pipe (b011_plan §7): the sender's reporter closure is
/// `try_send` (drop-on-full), so a slow UI can never block or fail the
/// sync. Events are advisory — the returned [`SyncResult`] is the truth.
pub struct ProgressPipe {
    sender: async_channel::Sender<ProgressEvent>,
}

impl ProgressPipe {
    /// The default capacity-128 pipe: the sender and its receiver.
    pub fn new() -> (Self, async_channel::Receiver<ProgressEvent>) {
        Self::with_capacity(128)
    }

    /// A pipe with an explicit capacity (the drop-behaviour tests use 1).
    pub fn with_capacity(capacity: usize) -> (Self, async_channel::Receiver<ProgressEvent>) {
        let (sender, receiver) = async_channel::bounded(capacity);
        (Self { sender }, receiver)
    }

    /// The reporter closure to pass into `SyncContext::reporter`. Owns a
    /// cloned sender — `Send + Sync + 'static` so it can ride into the
    /// sync worker thread (the B-015 seam).
    pub fn reporter(&self) -> impl Fn(ProgressEvent) + Send + Sync + 'static {
        let sender = self.sender.clone();
        move |event| {
            let _ = sender.try_send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &'static str, index: u8, began: bool) -> ProgressEvent {
        ProgressEvent::Step {
            name,
            index,
            total: 6,
            began,
        }
    }

    fn item(current: u32, total: u32) -> ProgressEvent {
        ProgressEvent::Item {
            current,
            total,
            label: format!("item {current}/{total}"),
        }
    }

    #[test]
    fn fold_is_monotone_on_steps() {
        let mut progress = SyncProgress::default();
        progress.fold(&step("Intent", 1, true));
        progress.fold(&step("Intent", 1, false));
        assert_eq!(progress.step_index, 1);
        assert_eq!(progress.step, Some("Intent"));
        progress.fold(&step("Backup", 2, true));
        // An out-of-order event can never rewind the bar.
        progress.fold(&step("Intent", 1, false));
        assert_eq!(progress.step_index, 2);
        assert_eq!(progress.step_total, 6);
        assert_eq!(progress.step, Some("Backup"));
    }

    #[test]
    fn a_new_step_resets_the_item_counters() {
        let mut progress = SyncProgress::default();
        progress.fold(&step("FileOps", 4, true));
        progress.fold(&item(3, 7));
        assert_eq!(progress.item_current, 3);
        assert_eq!(progress.item_total, 7);
        progress.fold(&step("DbWrite", 5, true));
        assert_eq!(progress.item_current, 0);
        assert_eq!(progress.item_total, 0);
        assert_eq!(progress.item_label, None);
    }

    #[test]
    fn indeterminate_items_touch_only_the_label() {
        let mut progress = SyncProgress::default();
        progress.fold(&step("Intent", 1, true));
        progress.fold(&ProgressEvent::Item {
            current: 0,
            total: 0,
            label: "parsing…".into(),
        });
        assert_eq!(progress.item_label.as_deref(), Some("parsing…"));
        assert_eq!(progress.item_current, 0);
        assert_eq!(progress.item_total, 0);
        assert_eq!(progress.completed_items, 0);
    }

    #[test]
    fn completed_items_accumulate_across_steps() {
        let mut progress = SyncProgress::default();
        progress.fold(&step("FileOps", 4, true));
        progress.fold(&item(5, 9));
        progress.fold(&step("DbWrite", 5, true));
        progress.fold(&item(1, 1));
        assert_eq!(progress.completed_items, 5);
        // A lower item from a stale event cannot decrease the run total.
        progress.fold(&item(2, 9));
        assert_eq!(progress.completed_items, 5);
    }

    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    #[test]
    fn the_reporter_closure_is_send_sync() {
        let (pipe, _receiver) = ProgressPipe::new();
        let reporter = pipe.reporter();
        assert_send_sync(&reporter);
    }

    #[test]
    fn a_full_pipe_drops_events_instead_of_blocking() {
        let (pipe, receiver) = ProgressPipe::with_capacity(1);
        let reporter = pipe.reporter();
        // Nobody reads: the writer must never block; every send after the
        // first is dropped.
        for current in 0..5 {
            reporter(item(current, 5));
        }
        assert_eq!(
            receiver.try_recv().ok(),
            Some(item(0, 5)),
            "the first event survives; the rest dropped"
        );
    }
}
