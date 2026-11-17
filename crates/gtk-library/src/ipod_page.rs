//! B-013: the iPod page — the route container, the five connection-state
//! views (S-02..S-05), the interrupted-operation banner, and the
//! connection-snapshot subscription (b013_plan §3).
//!
//! `IpodPageView` renders six stack pages: the No-iPod view (S-01,
//! B-012), Connecting, Sync unavailable, Ready, Error, and the
//! service-dead view for a crashed `IpodConnection` worker (the b006
//! liveness debt). The Ready arm is deliberately minimal — the device
//! overview is B-014 and the sync controls are B-015.
//!
//! All state logic lives in the pure [`decide`] — display-free testable.
//! No test in this workspace constructs GTK widgets (class init asserts
//! the main thread), so the widget application in
//! [`IpodPageView::apply_snapshot`] is the thin untested layer every
//! other view here has, and `decide` carries all the semantics. The
//! subscription attaches to the thread-default main context
//! (`glib::spawn_future_local`), so the free constructor
//! [`ipod_route_view`] must run on the GTK main thread — the shell's
//! route renderer calls it there.
//!
//! Actions: `Scan again` on No-iPod / Sync unavailable / Error (scanning
//! is manual — the B-012 owner decision; `retry()` only re-applies the
//! stale last report, a replugged device needs a scan), `Retry` on Error
//! for same-device transient failures. The interrupted-operation banner
//! is display-only: S-12 reports the operation until recovery verifies,
//! and the recovery/dismiss control belongs to the B-015/B-016 sync
//! flow.

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use localization::{msgid, tr};

use crate::device_manager::ConnectionState;
use crate::ipod_connection::{ConnectionSnapshot, IpodConnection};

gtk_widgets::composite_box!(
    pub IpodPageView,
    ipod_page_view_imp,
    "RufinIpodPageView",
    "/io/github/screwys/Rufin/ui/routes/ipod_page.ui",
    {
        state_stack: gtk::Stack,
        notice_revealer: gtk::Revealer,
        notice_detail: gtk::Label,
        no_ipod_heading: gtk::Label,
        status_dot: gtk::Box,
        status_label: gtk::Label,
        scan_button: gtk::Button,
        connecting_heading: gtk::Label,
        connecting_device: gtk::Label,
        unavailable_heading: gtk::Label,
        unavailable_diagnostic: gtk::Label,
        unavailable_scan_button: gtk::Button,
        ready_heading: gtk::Label,
        ready_device: gtk::Label,
        error_heading: gtk::Label,
        error_diagnostic: gtk::Label,
        error_retry_button: gtk::Button,
        error_scan_button: gtk::Button,
        dead_heading: gtk::Label,
    }
);

/// The stack page names (the `name` property in `ipod_page.ui`).
const NO_IPOD_PAGE: &str = "no-ipod";
const CONNECTING_PAGE: &str = "connecting";
const UNAVAILABLE_PAGE: &str = "unavailable";
const READY_PAGE: &str = "ready";
const ERROR_PAGE: &str = "error";
const SERVICE_DEAD_PAGE: &str = "service-dead";

/// The status-dot modifier for the one rendered state. Disconnected is
/// muted; the state views carry their own icons/spinners instead of dot
/// modifiers.
const STATUS_DISCONNECTED_CLASS: &str = "ipod-status-disconnected";

/// The user-facing state label. Literals here (not
/// `ConnectionState::ui_label()`) so the i18n extractor sees them —
/// `ui_label` is the untranslated spec wording.
fn state_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Disconnected => msgid("Disconnected"),
        ConnectionState::Connecting => msgid("Connecting"),
        ConnectionState::BackendUnavailable => msgid("Sync unavailable"),
        ConnectionState::Ready => msgid("Ready"),
        ConnectionState::Error => msgid("Error"),
    }
}

/// Which of the six stack pages a snapshot renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderedPage {
    NoIpod,
    Connecting,
    Unavailable,
    Ready,
    Error,
    /// The `IpodConnection` worker ended (crash or exit): the stale FSM
    /// view must not pretend to be live. Overrides every state.
    ServiceDead,
}

/// The pure rendering decision for one snapshot — every semantic of the
/// page, kept widget-free so the gates can run without a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageDecision {
    pub page: RenderedPage,
    /// The heading msgid the page shows (No-iPod's fixed .ui heading is
    /// reused as the status-row label).
    pub label: &'static str,
    /// Render the diagnostics label verbatim from the snapshot
    /// (Unavailable/Error only, and only when the snapshot carries one).
    pub show_diagnostics: bool,
    /// Render the device-name label from the snapshot (Connecting/Ready
    /// only, and only when the snapshot carries a non-empty name).
    pub show_device: bool,
    /// Reveal the interrupted-operation banner. The report outlives the
    /// service — it stays visible on the dead page.
    pub notice_visible: bool,
    /// The status-dot modifier (the No-iPod view only).
    pub dot_class: Option<&'static str>,
}

/// Decide what one snapshot renders. The single source of truth the
/// widget layer applies.
pub fn decide(snapshot: &ConnectionSnapshot) -> PageDecision {
    let notice_visible = snapshot.interrupted_operation.is_some();
    let device_shown = snapshot
        .device
        .as_ref()
        .is_some_and(|device| !device.name.is_empty());
    let diagnostics_shown = snapshot
        .diagnostics
        .as_ref()
        .is_some_and(|diagnostics| !diagnostics.is_empty());

    let mut decision = match snapshot.state {
        ConnectionState::Disconnected => PageDecision {
            page: RenderedPage::NoIpod,
            label: state_label(ConnectionState::Disconnected),
            show_diagnostics: false,
            show_device: false,
            notice_visible,
            dot_class: Some(STATUS_DISCONNECTED_CLASS),
        },
        ConnectionState::Connecting => PageDecision {
            page: RenderedPage::Connecting,
            label: state_label(ConnectionState::Connecting),
            show_diagnostics: false,
            show_device: device_shown,
            notice_visible,
            dot_class: None,
        },
        ConnectionState::BackendUnavailable => PageDecision {
            page: RenderedPage::Unavailable,
            label: state_label(ConnectionState::BackendUnavailable),
            show_diagnostics: diagnostics_shown,
            show_device: false,
            notice_visible,
            dot_class: None,
        },
        ConnectionState::Ready => PageDecision {
            page: RenderedPage::Ready,
            label: state_label(ConnectionState::Ready),
            show_diagnostics: false,
            show_device: device_shown,
            notice_visible,
            dot_class: None,
        },
        ConnectionState::Error => PageDecision {
            page: RenderedPage::Error,
            label: state_label(ConnectionState::Error),
            show_diagnostics: diagnostics_shown,
            show_device: false,
            notice_visible,
            dot_class: None,
        },
    };

    if !snapshot.worker_alive {
        decision.page = RenderedPage::ServiceDead;
        decision.label = msgid("Connection service stopped");
        decision.show_diagnostics = false;
        decision.show_device = false;
        decision.dot_class = None;
    }
    decision
}

fn set_device_label(label: &gtk::Label, snapshot: &ConnectionSnapshot, shown: bool) {
    if shown {
        if let Some(device) = &snapshot.device {
            if !device.name.is_empty() {
                label.set_label(&device.name);
                label.set_visible(true);
                return;
            }
        }
    }
    label.set_visible(false);
}

fn set_diagnostics_label(label: &gtk::Label, snapshot: &ConnectionSnapshot, shown: bool) {
    if shown {
        if let Some(diagnostics) = &snapshot.diagnostics {
            if !diagnostics.is_empty() {
                label.set_label(diagnostics);
                label.set_visible(true);
                return;
            }
        }
    }
    label.set_visible(false);
}

impl IpodPageView {
    /// Render one observable connection view: apply [`decide`] to the
    /// stack, the heading/diagnostics/device labels, and the notice
    /// banner. Called once with the initial snapshot (the subscription
    /// channel carries no replay) and on every channel event.
    pub fn apply_snapshot(&self, snapshot: &ConnectionSnapshot) {
        let decision = decide(snapshot);
        let imp = self.imp();

        // The interrupted-operation banner: display-only, every state.
        imp.notice_revealer
            .set_reveal_child(decision.notice_visible);
        if decision.notice_visible {
            if let Some(notice) = &snapshot.interrupted_operation {
                imp.notice_detail.set_label(&notice.detail);
            }
        }

        match decision.page {
            RenderedPage::NoIpod => {
                imp.status_label.set_label(&tr(decision.label));
                let dot = &imp.status_dot;
                if let Some(class) = decision.dot_class {
                    if !dot.has_css_class(class) {
                        dot.add_css_class(class);
                    }
                }
                imp.state_stack.set_visible_child_name(NO_IPOD_PAGE);
            }
            RenderedPage::Connecting => {
                imp.connecting_heading.set_label(&tr(decision.label));
                set_device_label(&imp.connecting_device, snapshot, decision.show_device);
                imp.state_stack.set_visible_child_name(CONNECTING_PAGE);
            }
            RenderedPage::Unavailable => {
                imp.unavailable_heading.set_label(&tr(decision.label));
                set_diagnostics_label(
                    &imp.unavailable_diagnostic,
                    snapshot,
                    decision.show_diagnostics,
                );
                imp.state_stack.set_visible_child_name(UNAVAILABLE_PAGE);
            }
            RenderedPage::Ready => {
                imp.ready_heading.set_label(&tr(decision.label));
                set_device_label(&imp.ready_device, snapshot, decision.show_device);
                imp.state_stack.set_visible_child_name(READY_PAGE);
            }
            RenderedPage::Error => {
                imp.error_heading.set_label(&tr(decision.label));
                set_diagnostics_label(&imp.error_diagnostic, snapshot, decision.show_diagnostics);
                imp.state_stack.set_visible_child_name(ERROR_PAGE);
            }
            RenderedPage::ServiceDead => {
                imp.dead_heading.set_label(&tr(decision.label));
                imp.state_stack.set_visible_child_name(SERVICE_DEAD_PAGE);
            }
        }
    }
}

/// Attach the page to the connection's snapshot stream: apply the current
/// view (no replay on the channel) and re-apply on every event. Runs the
/// receiver on the thread-default main context.
fn subscribe_to(view: &IpodPageView, connection: &IpodConnection) {
    view.apply_snapshot(&connection.snapshot());
    let receiver = connection.subscribe();
    let view = view.clone();
    glib::spawn_future_local(async move {
        while let Ok(snapshot) = receiver.recv().await {
            view.apply_snapshot(&snapshot);
        }
    });
}

/// The shell's entry point for `Route::Ipod`: the built and subscribed
/// page with the action wiring. Call on the GTK main thread (the
/// subscription attaches to the thread-default main context).
pub fn ipod_route_view(connection: &IpodConnection) -> gtk::Widget {
    let view = IpodPageView::new();
    let imp = view.imp();

    // Scan again (No-iPod / Sync unavailable / Error): a fresh
    // mount-table scan — `retry()` re-applies the stale last report, so
    // a replugged device needs this, not retry.
    {
        let connection = connection.clone();
        imp.scan_button.connect_clicked(move |_| connection.scan());
    }
    {
        let connection = connection.clone();
        imp.unavailable_scan_button
            .connect_clicked(move |_| connection.scan());
    }
    {
        let connection = connection.clone();
        imp.error_scan_button
            .connect_clicked(move |_| connection.scan());
    }
    // Retry (Error): same-device transient failures — a synthetic replug
    // re-running retrieval + preflight against the last report.
    {
        let connection = connection.clone();
        imp.error_retry_button
            .connect_clicked(move |_| connection.retry());
    }

    subscribe_to(&view, connection);
    view.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use ipod::BackendSearch;

    use crate::ipod_connection::OperationNotice;

    fn snapshot_with(state: ConnectionState) -> ConnectionSnapshot {
        ConnectionSnapshot {
            state,
            device: None,
            diagnostics: None,
            interrupted_operation: None,
            worker_alive: true,
        }
    }

    /// A snapshot with a non-empty device name and a diagnostic — the
    /// populated variants the label flags test.
    fn rich_snapshot(state: ConnectionState) -> ConnectionSnapshot {
        ConnectionSnapshot {
            state,
            device: Some(crate::device_manager::DeviceInfo {
                mount_point: PathBuf::from("/run/media/test/IPOD"),
                name: "Test iPod".to_string(),
                model: "Classic".to_string(),
                generation: "6G".to_string(),
                model_number: None,
                firmware: None,
                total_capacity: None,
                used_capacity: None,
                firewire_id: None,
                model_supported: true,
                checksum_possible: false,
            }),
            diagnostics: Some("the pod database is unreadable".to_string()),
            interrupted_operation: None,
            worker_alive: true,
        }
    }

    #[test]
    fn the_five_states_decide_their_pages() {
        let cases = [
            (
                ConnectionState::Disconnected,
                RenderedPage::NoIpod,
                msgid("Disconnected"),
            ),
            (
                ConnectionState::Connecting,
                RenderedPage::Connecting,
                msgid("Connecting"),
            ),
            (
                ConnectionState::BackendUnavailable,
                RenderedPage::Unavailable,
                msgid("Sync unavailable"),
            ),
            (ConnectionState::Ready, RenderedPage::Ready, msgid("Ready")),
            (ConnectionState::Error, RenderedPage::Error, msgid("Error")),
        ];
        for (state, page, label) in cases {
            let decision = decide(&snapshot_with(state));
            assert_eq!(decision.page, page, "{state:?} renders {page:?}");
            assert_eq!(decision.label, label, "{state:?} carries its heading");
            assert!(!decision.notice_visible, "{state:?} without a notice");
        }
    }

    #[test]
    fn a_disconnected_snapshot_decides_the_no_ipod_view_with_the_dot() {
        let decision = decide(&snapshot_with(ConnectionState::Disconnected));
        assert_eq!(decision.page, RenderedPage::NoIpod);
        assert_eq!(decision.label, msgid("Disconnected"));
        assert_eq!(decision.dot_class, Some(STATUS_DISCONNECTED_CLASS));
        assert!(!decision.show_diagnostics);
        assert!(!decision.show_device);
    }

    #[test]
    fn diagnostics_show_only_on_unavailable_and_error() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Ready,
        ] {
            let decision = decide(&rich_snapshot(state));
            assert!(
                !decision.show_diagnostics,
                "{state:?} must not render the diagnostics label"
            );
        }
        for state in [ConnectionState::BackendUnavailable, ConnectionState::Error] {
            let decision = decide(&rich_snapshot(state));
            assert!(
                decision.show_diagnostics,
                "{state:?} renders the diagnostics label"
            );
        }
        // An empty diagnostics field renders nothing even on Error.
        let mut snapshot = rich_snapshot(ConnectionState::Error);
        snapshot.diagnostics = None;
        assert!(!decide(&snapshot).show_diagnostics);
    }

    #[test]
    fn the_device_name_shows_only_on_connecting_and_ready() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::BackendUnavailable,
            ConnectionState::Error,
        ] {
            let decision = decide(&rich_snapshot(state));
            assert!(
                !decision.show_device,
                "{state:?} must not render the device-name label"
            );
        }
        for state in [ConnectionState::Connecting, ConnectionState::Ready] {
            let decision = decide(&rich_snapshot(state));
            assert!(decision.show_device, "{state:?} renders the device name");
        }
        // An absent device (or an empty name) renders nothing even on Ready.
        assert!(!decide(&snapshot_with(ConnectionState::Ready)).show_device);
        let mut snapshot = rich_snapshot(ConnectionState::Ready);
        snapshot.device.as_mut().unwrap().name = String::new();
        assert!(!decide(&snapshot).show_device);
    }

    #[test]
    fn the_notice_banner_visibility_tracks_the_notice_presence() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ] {
            let mut snapshot = snapshot_with(state);
            snapshot.interrupted_operation = Some(OperationNotice {
                detail: "interrupted".to_string(),
            });
            assert!(
                decide(&snapshot).notice_visible,
                "{state:?} reveals the banner when a notice is present"
            );
            assert!(
                !decide(&snapshot_with(state)).notice_visible,
                "{state:?} hides the banner without a notice"
            );
        }
    }

    #[test]
    fn a_dead_worker_overrides_every_state_and_keeps_the_notice() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ] {
            let mut snapshot = rich_snapshot(state);
            snapshot.worker_alive = false;
            snapshot.interrupted_operation = Some(OperationNotice {
                detail: "interrupted".to_string(),
            });
            let decision = decide(&snapshot);
            assert_eq!(decision.page, RenderedPage::ServiceDead, "{state:?}");
            assert_eq!(decision.label, msgid("Connection service stopped"));
            assert!(!decision.show_diagnostics, "{state:?} hides diagnostics");
            assert!(!decision.show_device, "{state:?} hides the device name");
            assert_eq!(decision.dot_class, None);
            assert!(
                decision.notice_visible,
                "the report outlives the service ({state:?})"
            );
        }
    }

    #[test]
    fn snapshots_flow_from_the_connection_through_the_channel() {
        // An unreachable backend dir: the deterministic no-backend path —
        // no device I/O, the worker thread is the real one.
        let connection = IpodConnection::with_backend_search(BackendSearch::Explicit(
            PathBuf::from("/tmp/rufin-b012-missing-backend"),
        ));
        let receiver = connection.subscribe();
        assert_eq!(
            decide(&connection.snapshot()).page,
            RenderedPage::NoIpod,
            "the initial snapshot is Disconnected"
        );

        // The interrupted-notice command fans a snapshot out to every
        // subscriber — the plumbing B-013..B-016 ride on, proven without
        // a display.
        connection.set_interrupted_operation(OperationNotice {
            detail: "gate interrupted".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            match receiver.try_recv() {
                Ok(snapshot) => break snapshot,
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "no snapshot arrived within 5 s");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("the subscriber channel closed prematurely")
                }
            }
        };
        assert_eq!(
            snapshot
                .interrupted_operation
                .as_ref()
                .map(|notice| notice.detail.as_str()),
            Some("gate interrupted"),
            "the notice rides the same stream the state views will use"
        );
        assert_eq!(
            decide(&snapshot).page,
            RenderedPage::NoIpod,
            "the notice does not change the connection state"
        );
        assert!(snapshot.worker_alive, "a live worker emits live snapshots");
    }
}
