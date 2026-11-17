//! B-012: the iPod page — the route container, the No-iPod view (S-01),
//! and the connection-snapshot subscription (b012_plan §3).
//!
//! `IpodPageView` renders exactly two stack pages today: the No-iPod view
//! for `Disconnected` (the only state B-012 owns) and a clearly-temporary
//! stub for the other four states. B-013 replaces the stub with the real
//! connection-state views — the stack and the snapshot subscription are
//! the seams it (and B-014..B-016) ride on.
//!
//! All state logic lives in the pure [`decide`] — display-free testable.
//! No test in this workspace constructs GTK widgets (class init asserts
//! the main thread), so the widget application in [`IpodPageView::apply_snapshot`]
//! is the thin untested layer every other view here has, and `decide`
//! carries all the semantics. The subscription attaches to the
//! thread-default main context (`glib::spawn_future_local`), so the free
//! constructor [`ipod_route_view`] must run on the GTK main thread — the
//! shell's route renderer calls it there.

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
        no_ipod_heading: gtk::Label,
        status_dot: gtk::Box,
        status_label: gtk::Label,
        stub_heading: gtk::Label,
    }
);

/// The stack page names (the `name` property in `ipod_page.ui`).
const NO_IPOD_PAGE: &str = "no-ipod";
const STUB_PAGE: &str = "stub";

/// The status-dot modifier for the one rendered state. Disconnected is
/// muted; B-013 adds the modifiers for the other four states alongside
/// their views.
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

/// Which of the two stack pages a snapshot renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderedPage {
    NoIpod,
    /// The temporary arm for every non-`Disconnected` state.
    Stub,
}

/// The pure rendering decision for one snapshot — every semantic of the
/// page, kept widget-free so the gates can run without a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageDecision {
    pub page: RenderedPage,
    /// The msgid for the label the page shows (heading or status row).
    pub label: &'static str,
    /// The status-dot modifier for the rendered state.
    pub dot_class: Option<&'static str>,
}

/// Decide what one snapshot renders. The single source of truth the
/// widget layer applies; the B-013 takeover point is the `Stub` arm.
pub fn decide(snapshot: &ConnectionSnapshot) -> PageDecision {
    match snapshot.state {
        ConnectionState::Disconnected => PageDecision {
            page: RenderedPage::NoIpod,
            label: state_label(ConnectionState::Disconnected),
            dot_class: Some(STATUS_DISCONNECTED_CLASS),
        },
        other => PageDecision {
            page: RenderedPage::Stub,
            label: state_label(other),
            dot_class: None,
        },
    }
}

impl IpodPageView {
    /// Render one observable connection view: apply [`decide`] to the
    /// stack, the status row, and the dot. Called once with the initial
    /// snapshot (the subscription channel carries no replay) and on every
    /// channel event.
    pub fn apply_snapshot(&self, snapshot: &ConnectionSnapshot) {
        let decision = decide(snapshot);
        match decision.page {
            RenderedPage::NoIpod => {
                self.imp().status_label.set_label(&tr(decision.label));
                let dot = &self.imp().status_dot;
                if let Some(class) = decision.dot_class {
                    if !dot.has_css_class(class) {
                        dot.add_css_class(class);
                    }
                }
                self.imp().state_stack.set_visible_child_name(NO_IPOD_PAGE);
            }
            RenderedPage::Stub => {
                // B-013 replaces this arm with the real state views.
                self.imp()
                    .stub_heading
                    .set_label(&format!("{}…", tr(decision.label)));
                self.imp().state_stack.set_visible_child_name(STUB_PAGE);
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
/// page. Call on the GTK main thread (the subscription attaches to the
/// thread-default main context).
pub fn ipod_route_view(connection: &IpodConnection) -> gtk::Widget {
    let view = IpodPageView::new();
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
        }
    }

    #[test]
    fn a_disconnected_snapshot_decides_the_no_ipod_view() {
        let decision = decide(&snapshot_with(ConnectionState::Disconnected));
        assert_eq!(decision.page, RenderedPage::NoIpod);
        assert_eq!(decision.label, msgid("Disconnected"));
        assert_eq!(decision.dot_class, Some(STATUS_DISCONNECTED_CLASS));
    }

    #[test]
    fn every_other_state_decides_the_stub_never_the_no_ipod_view() {
        for state in [
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ] {
            let decision = decide(&snapshot_with(state));
            assert_eq!(
                decision.page,
                RenderedPage::Stub,
                "{state:?} renders the stub (the B-013 takeover point)"
            );
            assert_eq!(decision.label, state_label(state));
            assert_eq!(decision.dot_class, None);
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
    }
}
