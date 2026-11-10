//! B-006: the main-thread event source — `gio::VolumeMonitor` mount events
//! feed debounced scan commands into [`IpodConnection`] (owner decision:
//! event-driven, no periodic polling).
//!
//! The monitor does not inspect mounts: any mount event schedules a debounced
//! scan, and B-003's discovery is the authority on what is an iPod. The
//! debounce window (300 ms) coalesces event bursts — including the
//! mount-ready race, where the first event can fire before the filesystem
//! is fully readable — into one scan. The startup scan covers devices that
//! were mounted before the monitor existed.
//!
//! Threading contract: the monitor lives on the GTK main thread. The
//! debounce attaches its timeout to the thread-default main context (the
//! global default — the GTK main loop — when none is pushed, see
//! `Debounce::trigger`); `IpodConnection::scan` is a non-blocking command
//! dispatch, so event handlers never parse anything here.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gio::prelude::*;

use crate::ipod_connection::IpodConnection;

/// How long mount events coalesce before one scan runs.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// A one-shot debounced trigger: `trigger` schedules `on_timeout` after the
/// window; a trigger inside the window cancels the pending call, so a burst
/// coalesces into a single call.
///
/// Implementation note: the pending call is a future spawned on the target
/// main context (`glib::timeout_future` + `MainContext::spawn_local`), and
/// cancellation is a shared flag the task checks after the delay. There is
/// deliberately no `SourceId::remove` here: it looks sources up in the
/// **global default** context only and panics on a missing source — both
/// wrong for a context-bound debounce whose one-shot may already have
/// fired.
/// Module-private: only [`IpodMountMonitor`] consumes the debounce; the
/// tests live in this module too. Deliberately not part of gtk-library's
/// public API.
struct Debounce {
    delay: Duration,
    /// The pending task's join handle and the flag that cancels it.
    pending: Option<(glib::JoinHandle<()>, Arc<AtomicBool>)>,
}

impl Debounce {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            pending: None,
        }
    }

    /// Schedule (or reschedule — cancelling the pending call) the timeout on
    /// the thread-default main context (the global default — the GTK main
    /// loop — when none is pushed). `on_timeout` runs once, after the
    /// window, from that context.
    fn trigger<F: FnOnce() + Send + 'static>(&mut self, on_timeout: F) {
        self.trigger_on(&glib::MainContext::ref_thread_default(), on_timeout);
    }

    /// Test/edge seam: spawn the timeout on an explicit context instead of
    /// the thread-default one, so tests iterate a private context without
    /// touching the global default.
    fn trigger_on<F: FnOnce() + Send + 'static>(
        &mut self,
        context: &glib::MainContext,
        on_timeout: F,
    ) {
        self.cancel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_task = cancelled.clone();
        let delay = self.delay;
        let task = async move {
            glib::timeout_future(delay).await;
            if !cancelled_for_task.load(Ordering::SeqCst) {
                on_timeout();
            }
        };
        self.pending = Some((context.spawn_local(task), cancelled));
    }

    /// Cancel the pending call, if any. The callback will not run.
    fn cancel(&mut self) {
        if let Some((_task, cancelled)) = self.pending.take() {
            cancelled.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for Debounce {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The mount-event source over an [`IpodConnection`] facade. Main thread.
pub struct IpodMountMonitor {
    connection: Rc<IpodConnection>,
    debounce: Rc<RefCell<Debounce>>,
    handlers: Vec<glib::SignalHandlerId>,
}

impl IpodMountMonitor {
    /// Connect the monitor, run the startup scan, and keep the facade
    /// reachable. Takes ownership of the facade; clone via
    /// [`IpodMountMonitor::connection`] before moving when the page needs
    /// its own handle.
    pub fn start(connection: IpodConnection) -> Self {
        let connection = Rc::new(connection);
        let debounce = Rc::new(RefCell::new(Debounce::new(DEBOUNCE)));

        connection.scan();

        let monitor = gio::VolumeMonitor::get();
        let mut handlers = Vec::with_capacity(3);
        let signal_connection = connection.clone();
        let signal_debounce = debounce.clone();
        handlers.push(monitor.connect_mount_added(move |_monitor, _mount| {
            schedule(&signal_connection, &signal_debounce);
        }));
        let signal_connection = connection.clone();
        let signal_debounce = debounce.clone();
        handlers.push(monitor.connect_mount_changed(move |_monitor, _mount| {
            schedule(&signal_connection, &signal_debounce);
        }));
        let signal_connection = connection.clone();
        let signal_debounce = debounce.clone();
        handlers.push(monitor.connect_mount_removed(move |_monitor, _mount| {
            schedule(&signal_connection, &signal_debounce);
        }));

        Self {
            connection,
            debounce,
            handlers,
        }
    }

    /// The facade, for subscriptions and snapshots.
    pub fn connection(&self) -> &IpodConnection {
        &self.connection
    }
}

impl Drop for IpodMountMonitor {
    fn drop(&mut self) {
        self.debounce.borrow_mut().cancel();
        let monitor = gio::VolumeMonitor::get();
        for handler in std::mem::take(&mut self.handlers) {
            monitor.disconnect(handler);
        }
    }
}

fn schedule(connection: &IpodConnection, debounce: &RefCell<Debounce>) {
    let connection = connection.clone();
    debounce.borrow_mut().trigger(move || connection.scan());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::thread;
    use std::time::Instant;

    use ipod::BackendSearch;

    use crate::ipod_connection::IpodConnection;

    /// The debounce test uses a **private** main context via the
    /// `trigger_on` seam: touching the global default context would race
    /// parallel tests that spawn futures on it.
    fn with_private_context(test: impl FnOnce(&glib::MainContext)) {
        let context = glib::MainContext::new();
        let _acquire = context
            .acquire()
            .expect("acquire the private test main context");
        test(&context);
    }

    #[test]
    fn debounce_coalesces_a_burst_and_cancel_prevents_the_call() {
        with_private_context(|context| {
            // A burst inside the window coalesces into one call.
            let calls = Arc::new(AtomicU32::new(0));
            let mut debounce = Debounce::new(Duration::from_millis(50));
            for _ in 0..3 {
                let calls_for_closure = calls.clone();
                debounce.trigger_on(context, move || {
                    calls_for_closure.fetch_add(1, Ordering::SeqCst);
                });
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while calls.load(Ordering::SeqCst) < 1 && Instant::now() < deadline {
                context.iteration(false);
                thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the burst coalesced into one call"
            );

            // A cancelled timeout never fires.
            let calls = Arc::new(AtomicU32::new(0));
            let mut debounce = Debounce::new(Duration::from_millis(30));
            let calls_for_closure = calls.clone();
            debounce.trigger_on(context, move || {
                calls_for_closure.fetch_add(1, Ordering::SeqCst);
            });
            debounce.cancel();
            thread::sleep(Duration::from_millis(120));
            context.iteration(false);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "a cancelled timeout must not fire"
            );
        });
    }

    /// The monitor constructs against the real (headless-safe) volume
    /// monitor, queues the startup scan, and drops cleanly with handlers
    /// disconnected. No state assertions — the startup scan reads the real
    /// host mount table, which is environment-dependent.
    #[test]
    fn monitor_starts_and_drops_cleanly() {
        let connection = IpodConnection::with_backend_search(BackendSearch::Explicit(
            std::path::Path::new("/nonexistent").into(),
        ));
        let monitor = IpodMountMonitor::start(connection);
        let _ = monitor.connection().snapshot();
        drop(monitor);
    }
}
