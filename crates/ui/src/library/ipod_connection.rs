//! B-006: the UI-facing connection surface — observer snapshots, the
//! serialized scan worker, and the retry/interrupted-notice commands.
//!
//! [`IpodConnection`] is a cheap cloneable facade. The main thread sends
//! commands (scan, retry, notice lifecycle) and receives
//! [`ConnectionSnapshot`] clones; a dedicated worker thread owns the
//! discovery engine, the loaded backend, and the FSM [`DeviceManager`], so
//! blocking libgpod parses (retrieval + preflight — seconds on real
//! devices) never run on the GTK main loop. Commands are FIFO and coalesce
//! naturally: a burst of mount events queues several scans, each re-checking
//! the truth (the glue's rescans are idempotent).
//!
//! Subscribers get a per-subscription `async_channel::Receiver` (glib 0.22
//! dropped `watch_channel`; async-channel is the sanctioned replacement and
//! already a workspace dependency). The worker fans each snapshot out to
//! every live subscriber and prunes closed channels along the way. Late
//! subscribers read the current view via [`IpodConnection::snapshot`] — the
//! channel carries no replay.
//!
//! The interrupted-operation notice (B-010 placeholder) is **service-owned**,
//! not FSM-owned: it is carried in every snapshot but never cleared by
//! removal, retry, or replug — only by an explicit clear (or B-010's
//! recovery). That is the "preserved separately" half of the B-006
//! contract.
//!
//! The worker exits when the last facade drops (the command channel
//! disconnects); it is deliberately detached — nothing in the library needs
//! to join it.

use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use ipod::{BackendSearch, DeviceSession, IpodError};
use ipod_sync::{
    DeviceFacts, Discovery, MountSource, ProcMountInfo, RetrieveError, ScanReport, collect,
};

use crate::device_manager::{ConnectionState, DeviceInfo, DeviceManager, PreflightError};
use crate::ipod_discovery::{apply_scan, map_readiness};

/// One observable view of the connection FSM. Latest-wins; full clones
/// (`DeviceInfo` is strings and numbers — this is the UI boundary, not a hot
/// loop).
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionSnapshot {
    pub state: ConnectionState,
    pub device: Option<DeviceInfo>,
    pub diagnostics: Option<String>,
    /// Interrupted-operation notice, service-owned and preserved across
    /// removal/retry — never cleared by the FSM (see the module docs).
    pub interrupted_operation: Option<OperationNotice>,
}

/// A user-facing interrupted-operation notice (B-010 placeholder). The
/// recovery/stat-merge model of B-010 replaces this type; the snapshot field
/// stays the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationNotice {
    pub detail: String,
}

enum Command {
    Scan,
    /// A scan against a caller-supplied mount source (tests/edge seams; the
    /// production `Scan` uses [`ProcMountInfo`]).
    ScanFrom(Box<dyn MountSource>),
    /// Full re-appearance retry: `clear_device` + re-apply the last report.
    /// The glue itself has no retry helper (B-004 decision 5) — this is a
    /// service-level composition of existing primitives, "synthetic replug".
    Retry,
    SetInterrupted(OperationNotice),
    ClearInterrupted,
}

struct Worker {
    discovery: Discovery,
    manager: DeviceManager,
    interrupted: Option<OperationNotice>,
    last_report: Option<ScanReport>,
    snapshot: Arc<Mutex<ConnectionSnapshot>>,
    subscribers: Arc<Mutex<Vec<async_channel::Sender<ConnectionSnapshot>>>>,
}

impl Worker {
    fn run(mut self, commands: mpsc::Receiver<Command>) {
        for command in commands {
            match command {
                Command::Scan => self.scan(Box::new(ProcMountInfo)),
                Command::ScanFrom(source) => self.scan(source),
                Command::Retry => {
                    if let Some(report) = self.last_report.clone() {
                        // Synthetic replug: forget the device first, so the
                        // re-apply is a fresh appearance — retrieval and
                        // preflight re-run against the current truth.
                        self.manager.clear_device();
                        self.publish();
                        self.apply(report);
                    }
                    // No scan yet → nothing to retry: stay silent.
                }
                Command::SetInterrupted(notice) => {
                    self.interrupted = Some(notice);
                    self.publish();
                }
                Command::ClearInterrupted => {
                    self.interrupted = None;
                    self.publish();
                }
            }
        }
    }

    fn scan(&mut self, source: Box<dyn MountSource>) {
        match self.discovery.scan(source.as_ref()) {
            Ok(report) => {
                self.last_report = Some(report.clone());
                self.apply(report);
            }
            Err(error) => {
                // A mount-table read failure is not a device verdict: no
                // state change, no emission. The last known snapshot stays
                // authoritative (B-013 can add an environment banner later).
                tracing::error!(%error, "ipod connection scan failed");
            }
        }
    }

    /// Feed one scan report through the FSM glue, emitting observable
    /// snapshots to subscribers.
    fn apply(&mut self, report: ScanReport) {
        let Self {
            discovery,
            manager,
            interrupted,
            snapshot,
            subscribers,
            ..
        } = self;

        let retrieve = |mount: &Path| -> Result<DeviceFacts, RetrieveError> {
            let backend = discovery.backend().ok_or(IpodError::NotFound(
                "the libgpod backend is unavailable".to_string(),
            ))?;
            let session = DeviceSession::open(backend, mount)?;
            collect(&session, mount)
        };
        let preflight = |mount: &Path| -> Result<(), PreflightError> {
            let backend = discovery
                .backend()
                .ok_or(PreflightError::BackendUnavailable(
                    "the libgpod backend is unavailable".to_string(),
                ))?;
            ipod_sync::preflight(backend, mount).map_err(map_readiness)
        };
        let mut emit = |manager: &DeviceManager| {
            let view = ConnectionSnapshot {
                state: manager.state(),
                device: manager.current_device().cloned(),
                diagnostics: manager.diagnostics().map(str::to_string),
                interrupted_operation: interrupted.clone(),
            };
            *snapshot.lock().expect("snapshot lock poisoned") = view.clone();
            let mut subscribers = subscribers.lock().expect("subscribers lock poisoned");
            subscribers.retain(|sender| sender.try_send(view.clone()).is_ok());
        };
        apply_scan(manager, &report, &retrieve, &preflight, &mut emit);
    }

    /// Publish the current FSM view (notice lifecycle commands and the
    /// retry's synthetic-replug step: the FSM itself did not change).
    /// Idempotent — a view equal to the stored snapshot emits nothing.
    fn publish(&self) {
        let view = ConnectionSnapshot {
            state: self.manager.state(),
            device: self.manager.current_device().cloned(),
            diagnostics: self.manager.diagnostics().map(str::to_string),
            interrupted_operation: self.interrupted.clone(),
        };
        let mut slot = self.snapshot.lock().expect("snapshot lock poisoned");
        if *slot == view {
            return;
        }
        *slot = view.clone();
        drop(slot);
        let mut subscribers = self.subscribers.lock().expect("subscribers lock poisoned");
        subscribers.retain(|sender| sender.try_send(view.clone()).is_ok());
    }
}

/// The cloneable facade over the connection worker. Commands are fire-and-
/// forget (the worker is the single writer of FSM state); snapshots arrive
/// through per-subscription channels.
#[derive(Debug, Clone)]
pub struct IpodConnection {
    commands: mpsc::Sender<Command>,
    snapshot: Arc<Mutex<ConnectionSnapshot>>,
    subscribers: Arc<Mutex<Vec<async_channel::Sender<ConnectionSnapshot>>>>,
}

impl IpodConnection {
    /// Start the worker with the production backend search
    /// (`RUFIN_LIBGPOD_DIR`, then the loader's default paths).
    pub fn new() -> Self {
        Self::with_backend_search(BackendSearch::FromEnv)
    }

    /// Start the worker with an explicit backend search. Tests pin a search
    /// that cannot load so the no-backend paths are deterministic on any
    /// host; production uses [`IpodConnection::new`].
    pub fn with_backend_search(search: BackendSearch) -> Self {
        let (commands, receiver) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(ConnectionSnapshot {
            state: ConnectionState::Disconnected,
            device: None,
            diagnostics: None,
            interrupted_operation: None,
        }));
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let worker = Worker {
            discovery: Discovery::with_backend_search(search),
            manager: DeviceManager::new(),
            interrupted: None,
            last_report: None,
            snapshot: snapshot.clone(),
            subscribers: subscribers.clone(),
        };
        thread::Builder::new()
            .name("ipod-connection".to_string())
            .spawn(move || worker.run(receiver))
            .expect("failed to spawn the ipod connection worker");
        Self {
            commands,
            snapshot,
            subscribers,
        }
    }

    /// The current observable view (late-subscriber support; the channel
    /// itself carries no replay).
    pub fn snapshot(&self) -> ConnectionSnapshot {
        self.snapshot
            .lock()
            .expect("snapshot lock poisoned")
            .clone()
    }

    /// A receiver of every future emission. Each subscription is its own
    /// channel; stale senders of dropped receivers are pruned by the worker.
    pub fn subscribe(&self) -> async_channel::Receiver<ConnectionSnapshot> {
        let (sender, receiver) = async_channel::unbounded();
        self.subscribers
            .lock()
            .expect("subscribers lock poisoned")
            .push(sender);
        receiver
    }

    /// Queue a production mount-table scan. Returns immediately; the worker
    /// runs discovery + retrieval + preflight off the caller's thread.
    pub fn scan(&self) {
        self.send(Command::Scan);
    }

    /// Queue a scan against a caller-supplied mount source (tests and edge
    /// seams; production scans use [`ProcMountInfo`]).
    pub fn scan_from(&self, source: Box<dyn MountSource>) {
        self.send(Command::ScanFrom(source));
    }

    /// Queue a full re-appearance retry (see [`Command::Retry`]): the last
    /// report is re-applied after clearing the device, so retrieval and
    /// preflight run fresh — a "synthetic replug".
    pub fn retry(&self) {
        self.send(Command::Retry);
    }

    /// Queue an interrupted-operation notice. It is carried in every
    /// subsequent snapshot and survives removal/retry until cleared.
    pub fn set_interrupted_operation(&self, notice: OperationNotice) {
        self.send(Command::SetInterrupted(notice));
    }

    /// Queue the explicit clearing of the interrupted-operation notice.
    pub fn clear_interrupted_operation(&self) {
        self.send(Command::ClearInterrupted);
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::error!("ipod connection worker is gone; command dropped");
        }
    }
}

impl Default for IpodConnection {
    /// Starts the worker thread with the production backend search, same as
    /// [`IpodConnection::new`]. Note that **constructing a connection spawns
    /// a thread** — `Default` is convenience, not for derive-heavy holder
    /// structs where a `#[derive(Default)]` would silently spawn workers.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use ipod_sync::{MountEntry, MountError};

    /// The deterministic no-backend facade: an explicit search that can
    /// never load, so the BackendUnavailable paths are stable on any host.
    fn connection() -> IpodConnection {
        IpodConnection::with_backend_search(BackendSearch::Explicit(
            Path::new("/nonexistent-libgpod").to_path_buf(),
        ))
    }

    /// A synthetic candidate mount: a real temp directory containing
    /// `iPod_Control/`, so B-003's probe confirms it as a candidate.
    struct TestMount {
        dir: std::path::PathBuf,
    }

    impl TestMount {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rufin-b006-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(dir.join("iPod_Control")).expect("create the test mount");
            Self { dir }
        }

        fn entry(&self) -> MountEntry {
            MountEntry {
                mount_point: self.dir.clone(),
                fstype: "vfat".to_string(),
                source: "/dev/fake".to_string(),
            }
        }
    }

    impl Drop for TestMount {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A mount source over a fixed entry list.
    #[derive(Debug)]
    struct FakeMounts(Vec<MountEntry>);

    impl MountSource for FakeMounts {
        fn mounts(&self) -> Result<Vec<MountEntry>, MountError> {
            Ok(self.0.clone())
        }
    }

    /// A mount source whose table read always fails.
    #[derive(Debug)]
    struct FailingMounts;

    impl MountSource for FailingMounts {
        fn mounts(&self) -> Result<Vec<MountEntry>, MountError> {
            Err(MountError(std::io::Error::other("synthetic failure")))
        }
    }

    fn expect(receiver: &async_channel::Receiver<ConnectionSnapshot>) -> ConnectionSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match receiver.try_recv() {
                Ok(snapshot) => return snapshot,
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for a snapshot"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(async_channel::TryRecvError::Closed) => panic!("snapshot channel closed"),
            }
        }
    }

    /// Assert that no snapshot arrives within the window. The FIFO marker
    /// argument must have been consumed already, proving the worker
    /// processed everything queued before the silent operation.
    fn expect_silence(receiver: &async_channel::Receiver<ConnectionSnapshot>) {
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            match receiver.try_recv() {
                Ok(snapshot) => panic!("unexpected snapshot: {snapshot:?}"),
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(async_channel::TryRecvError::Empty) => return,
                // A closed channel here means the worker dropped the sender
                // — the worker died. Silence must never mask that.
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("snapshot channel closed: the worker died during the silence window")
                }
            }
        }
    }

    #[test]
    fn starts_disconnected_without_a_device() {
        let connection = connection();
        assert_eq!(
            connection.snapshot(),
            ConnectionSnapshot {
                state: ConnectionState::Disconnected,
                device: None,
                diagnostics: None,
                interrupted_operation: None,
            }
        );
    }

    #[test]
    fn scan_without_backend_emits_connecting_then_backend_unavailable() {
        let mount = TestMount::new("fresh");
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));

        let connecting = expect(&snapshots);
        assert_eq!(connecting.state, ConnectionState::Connecting);
        assert_eq!(connecting.device.as_ref().unwrap().mount_point, mount.dir);
        assert_eq!(connecting.device.as_ref().unwrap().name, "");

        let unavailable = expect(&snapshots);
        assert_eq!(unavailable.state, ConnectionState::BackendUnavailable);
        assert_eq!(unavailable.state.ui_label(), "Sync unavailable");
        assert!(
            unavailable
                .diagnostics
                .as_deref()
                .is_some_and(|d| !d.is_empty()),
            "the load reason must ride along"
        );
    }

    #[test]
    fn removal_scan_returns_to_disconnected() {
        let mount = TestMount::new("removed");
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));
        expect(&snapshots);
        expect(&snapshots);

        connection.scan_from(Box::new(FakeMounts(Vec::new())));
        let disconnected = expect(&snapshots);
        assert_eq!(disconnected.state, ConnectionState::Disconnected);
        assert!(disconnected.device.is_none());
    }

    #[test]
    fn retry_reapplies_the_last_report_as_a_fresh_appearance() {
        let mount = TestMount::new("retry");
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));
        expect(&snapshots);
        expect(&snapshots);

        connection.retry();
        let disconnected = expect(&snapshots);
        assert_eq!(
            disconnected.state,
            ConnectionState::Disconnected,
            "retry is a synthetic replug: Disconnected comes first"
        );
        assert!(disconnected.device.is_none());
        let connecting = expect(&snapshots);
        assert_eq!(connecting.state, ConnectionState::Connecting);
        assert_eq!(connecting.device.as_ref().unwrap().mount_point, mount.dir);
        let unavailable = expect(&snapshots);
        assert_eq!(unavailable.state, ConnectionState::BackendUnavailable);
    }

    #[test]
    fn retry_before_any_scan_is_silent() {
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.retry();
        // FIFO marker: the notice emission proves the retry was processed
        // first, and emitted nothing.
        connection.set_interrupted_operation(OperationNotice {
            detail: "marker".to_string(),
        });
        let snapshot = expect(&snapshots);
        assert_eq!(snapshot.state, ConnectionState::Disconnected);
        assert_eq!(
            snapshot.interrupted_operation,
            Some(OperationNotice {
                detail: "marker".to_string()
            })
        );
    }

    #[test]
    fn scan_error_emits_nothing_and_leaves_state_unchanged() {
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FailingMounts));
        // FIFO marker after the failing scan: it must arrive without any
        // snapshot in between.
        connection.set_interrupted_operation(OperationNotice {
            detail: "marker".to_string(),
        });
        let snapshot = expect(&snapshots);
        assert_eq!(snapshot.state, ConnectionState::Disconnected);
        assert!(snapshot.diagnostics.is_none());
        assert!(snapshot.interrupted_operation.is_some());
        expect_silence(&snapshots);
    }

    #[test]
    fn interrupted_notice_survives_retry() {
        let mount = TestMount::new("notice-retry");
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));
        expect(&snapshots);
        expect(&snapshots);

        connection.set_interrupted_operation(OperationNotice {
            detail: "sync interrupted".to_string(),
        });
        let noticed = expect(&snapshots);
        assert_eq!(noticed.state, ConnectionState::BackendUnavailable);
        assert_eq!(
            noticed.interrupted_operation,
            Some(OperationNotice {
                detail: "sync interrupted".to_string()
            })
        );

        connection.retry();
        let disconnected = expect(&snapshots);
        assert_eq!(disconnected.state, ConnectionState::Disconnected);
        assert_eq!(
            disconnected.interrupted_operation,
            Some(OperationNotice {
                detail: "sync interrupted".to_string()
            }),
            "retry must not clear the notice"
        );
        let connecting = expect(&snapshots);
        assert_eq!(connecting.state, ConnectionState::Connecting);
        assert_eq!(
            connecting.interrupted_operation,
            Some(OperationNotice {
                detail: "sync interrupted".to_string()
            }),
            "the notice rides along the fresh appearance"
        );
    }

    #[test]
    fn interrupted_notice_survives_removal() {
        let mount = TestMount::new("notice-removed");
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));
        expect(&snapshots);
        expect(&snapshots);

        connection.set_interrupted_operation(OperationNotice {
            detail: "sync interrupted".to_string(),
        });
        expect(&snapshots);

        connection.scan_from(Box::new(FakeMounts(Vec::new())));
        let disconnected = expect(&snapshots);
        assert_eq!(disconnected.state, ConnectionState::Disconnected);
        assert!(
            disconnected.device.is_none(),
            "removal clears the device info"
        );
        assert_eq!(
            disconnected.interrupted_operation,
            Some(OperationNotice {
                detail: "sync interrupted".to_string()
            }),
            "removal must not clear the notice"
        );
    }

    #[test]
    fn interrupted_notice_is_cleared_only_explicitly() {
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.set_interrupted_operation(OperationNotice {
            detail: "sync interrupted".to_string(),
        });
        expect(&snapshots);

        connection.clear_interrupted_operation();
        let cleared = expect(&snapshots);
        assert_eq!(cleared.interrupted_operation, None);
        assert_eq!(cleared.state, ConnectionState::Disconnected);
    }

    #[test]
    fn subscribers_receive_the_same_sequence() {
        let mount = TestMount::new("broadcast");
        let connection = connection();
        let first = connection.subscribe();
        let second = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));

        let a = expect(&first);
        let b = expect(&second);
        assert_eq!(a, b);
        assert_eq!(a.state, ConnectionState::Connecting);
        assert_eq!(expect(&first), expect(&second));
    }

    #[test]
    fn late_subscribers_read_the_current_snapshot_without_replay() {
        let mount = TestMount::new("late");
        let connection = connection();
        let snapshots = connection.subscribe();
        connection.scan_from(Box::new(FakeMounts(vec![mount.entry()])));
        expect(&snapshots);
        expect(&snapshots);

        let late = connection.subscribe();
        expect_silence(&late);
        assert_eq!(
            connection.snapshot().state,
            ConnectionState::BackendUnavailable,
            "the facade snapshot reflects the latest state"
        );
    }

    #[test]
    fn facade_clones_share_one_worker() {
        let mount = TestMount::new("clone");
        let connection = connection();
        let snapshots = connection.subscribe();
        let clone = connection.clone();
        clone.scan_from(Box::new(FakeMounts(vec![mount.entry()])));
        let connecting = expect(&snapshots);
        assert_eq!(connecting.state, ConnectionState::Connecting);
    }
}
