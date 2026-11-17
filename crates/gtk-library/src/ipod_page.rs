//! B-013: the iPod page — the route container, the five connection-state
//! views (S-02..S-05), the interrupted-operation banner, and the
//! connection-snapshot subscription (b013_plan §3).
//!
//! `IpodPageView` renders six stack pages: the No-iPod view (S-01,
//! B-012), Connecting, Sync unavailable, Ready, Error, and the
//! service-dead view for a crashed `IpodConnection` worker (the b006
//! liveness debt).
//!
//! The Ready arm is B-014's iTunes-style device shell (b014_plan §3):
//! a sidebar (device identity + greyed nav furniture), the Обзор page
//! (the S-06 info box + the greyed Параметры box), and the bottom bar
//! (storage + greyed Sync). The sync controls themselves are B-015's
//! takeover point.
//!
//! All state logic lives in the pure [`decide`] — display-free testable.
//! No test in this workspace constructs GTK widgets (class init asserts
//! the main thread), so the widget application in
//! [`IpodPageView::apply_snapshot`] is the thin untested layer every
//! other view here has, and `decide` plus the B-014 pure helpers
//! ([`overview_rows`], [`format_gb`], [`ready_heading`], ...) carry all
//! the semantics. The subscription attaches to the thread-default main
//! context (`glib::spawn_future_local`), so the free constructor
//! [`ipod_route_view`] must run on the GTK main thread — the shell's
//! route renderer calls it there.
//!
//! Actions: `Scan again` on No-iPod / Sync unavailable / Error and the
//! Ready sidebar's refresh icon (scanning is manual — the B-012 owner
//! decision; `retry()` only re-applies the stale last report, a
//! replugged device needs a scan), `Retry` on Error for same-device
//! transient failures. The interrupted-operation banner is display-only:
//! S-12 reports the operation until recovery verifies, and the
//! recovery/dismiss control belongs to the B-015/B-016 sync flow.

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use localization::{msgid, tr, tr_with};

use crate::device_manager::{ConnectionState, DeviceInfo};
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
        sidebar_name: gtk::Label,
        sidebar_badge: gtk::Label,
        sidebar_scan_button: gtk::Button,
        capacity_value: gtk::Label,
        serial_value: gtk::Label,
        format_value: gtk::Label,
        firmware_value: gtk::Label,
        warning_label: gtk::Label,
        storage_bar: gtk::ProgressBar,
        sync_button: gtk::Button,
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
            // The Ready shell carries its own identity labels (sidebar
            // name, model heading) — the connecting-style name label
            // does not apply here (B-014).
            show_device: false,
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

// ---------------------------------------------------------------------
// B-014: the Ready shell's pure semantics — every display decision the
// widget layer applies, kept GTK-free so the gates run without a
// display. Unit composition (the translatable "GB") lives in the thin
// widget layer on purpose: the pure layer carries numbers and verbatim
// device content only.
// ---------------------------------------------------------------------

/// One row of the Обзор info box: a translatable label and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverviewRow {
    /// Row label (msgid, `tr()` at application time).
    pub label: &'static str,
    pub value: OverviewValue,
}

/// A row value. Absence renders as the owner's placeholder rule
/// (2026-11-17, b014_plan D-4): a simple "Unavailable", never invented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverviewValue {
    /// Verbatim device content — always plain text at application,
    /// never markup (the strings are device-controlled).
    Data(String),
    /// Measured capacity in decimal GB: the raw number from
    /// [`format_gb`]; the application appends the translatable GB unit.
    CapacityGb(String),
    /// The "Unavailable" placeholder — translated at application.
    Unavailable,
}

/// The four Обзор rows in UI order (b014_plan §3.3): Capacity, Serial,
/// Format, Firmware. Capacity shows the measured total; Format shows the
/// mounted filesystem's type ("FAT" for a Windows iPod — the honest
/// measured fact behind iTunes' "Windows" wording); the absent-optional
/// rule (D-4) covers the rest.
pub fn overview_rows(device: &DeviceInfo) -> Vec<OverviewRow> {
    let capacity = device
        .total_capacity
        .map(|total| OverviewValue::CapacityGb(format_gb(total)))
        .unwrap_or(OverviewValue::Unavailable);
    let serial = device
        .serial_number
        .clone()
        .filter(|serial| !serial.is_empty())
        .map(OverviewValue::Data)
        .unwrap_or(OverviewValue::Unavailable);
    let filesystem = device
        .filesystem
        .clone()
        .filter(|filesystem| !filesystem.is_empty())
        .map(OverviewValue::Data)
        .unwrap_or(OverviewValue::Unavailable);
    let firmware = device
        .firmware
        .clone()
        .filter(|firmware| !firmware.is_empty())
        .map(OverviewValue::Data)
        .unwrap_or(OverviewValue::Unavailable);
    vec![
        OverviewRow {
            label: msgid("Capacity"),
            value: capacity,
        },
        OverviewRow {
            label: msgid("Serial number"),
            value: serial,
        },
        OverviewRow {
            label: msgid("Format"),
            value: filesystem,
        },
        OverviewRow {
            label: msgid("Firmware"),
            value: firmware,
        },
    ]
}

/// The Обзор heading: the model name, or the generic "iPod" when the
/// model is unresolvable. Returns either verbatim device content (a
/// model string — `tr()` passes it through unchanged) or a msgid; the
/// application calls `tr()` on the result (b014_plan D-3).
pub fn ready_heading(device: &DeviceInfo) -> String {
    if !device.model.is_empty() {
        device.model.clone()
    } else {
        msgid("iPod").to_string()
    }
}

/// The sidebar device name: the master-playlist name, or the generic
/// "iPod" when absent. Same `tr()` contract as [`ready_heading`].
pub fn sidebar_name(device: &DeviceInfo) -> String {
    if !device.name.is_empty() {
        device.name.clone()
    } else {
        msgid("iPod").to_string()
    }
}

/// The sidebar capacity badge: the measured total rounded to whole
/// decimal GB, number-only (the application appends the unit). `None`
/// when the measurement is absent — the badge hides (a summary must
/// not invent the value it summarizes).
pub fn capacity_badge(device: &DeviceInfo) -> Option<String> {
    device.total_capacity.map(|total| format_whole_gb(total))
}

/// Free space: total − used. `None` when either measurement is absent;
/// clamped at zero against contract violations.
pub fn free_bytes(device: &DeviceInfo) -> Option<u64> {
    match (device.total_capacity, device.used_capacity) {
        (Some(total), Some(used)) => Some(total.saturating_sub(used)),
        _ => None,
    }
}

/// The storage-bar fraction (used / total), clamped to `1.0`. `None`
/// when either measurement is absent or total is zero.
pub fn capacity_fraction(device: &DeviceInfo) -> Option<f64> {
    match (device.total_capacity, device.used_capacity) {
        (Some(total), Some(used)) if total > 0 => Some((used as f64 / total as f64).min(1.0)),
        _ => None,
    }
}

/// The model-policy warning for the info box (b014_plan D-8): the
/// 2026-11-10 verdicts (b006 delta decision 12), display-only — the
/// actual sync gating is B-015. `None` when the model is supported and
/// the checksum is possible.
pub fn support_warning(device: &DeviceInfo) -> Option<&'static str> {
    if !device.model_supported {
        Some(msgid(
            "This model is not in the supported set (Video 5/5.5G, Classic 6/7G).",
        ))
    } else if !device.checksum_possible {
        Some(msgid(
            "No FireWire ID — the device database checksum cannot be generated for this Classic.",
        ))
    } else {
        None
    }
}

/// Decimal GB, exactly two decimals, round half-up, number-only
/// (b014_plan D-5/D-10: the separator is hardcoded `.` — the repo has
/// no locale machinery; the unit is appended by the widget layer).
///
/// Uses u128 arithmetic so absurd byte counts cannot overflow the
/// ×100 scaling (capacities are far below even u64's limits in
/// practice).
pub fn format_gb(bytes: u64) -> String {
    let hundredths = ((u128::from(bytes) * 100 + 500_000_000) / 1_000_000_000) as u64;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// Whole decimal GB, round half-up, number-only — the badge form of
/// [`format_gb`].
fn format_whole_gb(bytes: u64) -> String {
    (((u128::from(bytes) + 500_000_000) / 1_000_000_000) as u64).to_string()
}

/// The translatable GB unit around a pure-layer number ("2.05" →
/// "2.05 GB" / "2,05 ГБ"). Thin-layer composition on purpose — the
/// pure layer carries numbers only.
fn unit_gb_text(number: &str) -> String {
    tr_with("{value} GB", &[("value", number)])
}

/// The bottom-bar free text ("Free: 36.61 GB") — same thin-layer
/// composition contract.
fn free_space_text(free: u64) -> String {
    tr_with(
        "Free: {value}",
        &[("value", &unit_gb_text(&format_gb(free)))],
    )
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
                if let Some(device) = &snapshot.device {
                    imp.ready_heading.set_label(&tr(&ready_heading(device)));
                    imp.sidebar_name.set_label(&tr(&sidebar_name(device)));
                    match capacity_badge(device) {
                        Some(badge) => {
                            imp.sidebar_badge.set_label(&unit_gb_text(&badge));
                            imp.sidebar_badge.set_visible(true);
                        }
                        None => imp.sidebar_badge.set_visible(false),
                    }
                    self.apply_overview(device);
                    match support_warning(device) {
                        Some(warning) => {
                            imp.warning_label.set_label(&tr(warning));
                            imp.warning_label.set_visible(true);
                        }
                        None => imp.warning_label.set_visible(false),
                    }
                    match capacity_fraction(device) {
                        Some(fraction) => imp.storage_bar.set_fraction(fraction),
                        None => imp.storage_bar.set_fraction(0.0),
                    }
                    match free_bytes(device) {
                        Some(free) => {
                            imp.storage_bar.set_show_text(true);
                            imp.storage_bar.set_text(Some(&free_space_text(free)));
                        }
                        None => imp.storage_bar.set_show_text(false),
                    }
                }
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

    /// Apply the pure [`overview_rows`] to the four value labels
    /// (b014_plan §3.3). The Capacity row appends the translatable GB
    /// unit; the other rows are verbatim device data or the translated
    /// placeholder.
    fn apply_overview(&self, device: &DeviceInfo) {
        let imp = self.imp();
        let rows = overview_rows(device);
        let labels = [
            &imp.capacity_value,
            &imp.serial_value,
            &imp.format_value,
            &imp.firmware_value,
        ];
        for (row, label) in rows.iter().zip(labels) {
            let text = match &row.value {
                OverviewValue::Data(data) => data.clone(),
                OverviewValue::CapacityGb(number) => unit_gb_text(number),
                OverviewValue::Unavailable => tr(msgid("Unavailable")),
            };
            label.set_label(&text);
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
    // The Ready sidebar's refresh icon (B-014): scanning is manual, so
    // the overview needs its own on-page trigger.
    {
        let connection = connection.clone();
        imp.sidebar_scan_button
            .connect_clicked(move |_| connection.scan());
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
                serial_number: None,
                filesystem: None,
                model_supported: true,
                checksum_possible: false,
            }),
            diagnostics: Some("the pod database is unreadable".to_string()),
            interrupted_operation: None,
            worker_alive: true,
        }
    }

    /// A device with every fact populated — the full-overview fixture
    /// for the B-014 pure-layer gates.
    fn full_device() -> crate::device_manager::DeviceInfo {
        crate::device_manager::DeviceInfo {
            mount_point: PathBuf::from("/run/media/test/IPOD"),
            name: "iPod user".to_string(),
            model: "Video (Black)".to_string(),
            generation: "5G".to_string(),
            model_number: Some("A446".to_string()),
            firmware: Some("1.3".to_string()),
            total_capacity: Some(14_907_051_520),
            used_capacity: Some(2_048_000_000),
            firewire_id: Some("0x000A270012345678".to_string()),
            serial_number: Some("YM6312345Y5N".to_string()),
            filesystem: Some("FAT".to_string()),
            model_supported: true,
            checksum_possible: true,
        }
    }

    #[test]
    fn format_gb_renders_two_decimals_decimal_gb() {
        assert_eq!(format_gb(0), "0.00");
        assert_eq!(format_gb(1), "0.00");
        assert_eq!(format_gb(500_000_000), "0.50");
        assert_eq!(format_gb(1_000_000_000), "1.00");
        assert_eq!(format_gb(1_050_000_000), "1.05");
        assert_eq!(format_gb(2_048_000_000), "2.05");
        assert_eq!(format_gb(14_907_051_520), "14.91");
    }

    #[test]
    fn capacity_badge_rounds_to_whole_gb_and_hides_when_unmeasured() {
        let device = full_device();
        assert_eq!(capacity_badge(&device).as_deref(), Some("15"));

        let mut device = full_device();
        device.total_capacity = Some(1_050_000_000);
        assert_eq!(capacity_badge(&device).as_deref(), Some("1"));
        device.total_capacity = Some(500_000_000);
        assert_eq!(capacity_badge(&device).as_deref(), Some("1"));

        device.total_capacity = None;
        assert_eq!(capacity_badge(&device), None);
    }

    #[test]
    fn the_heading_and_sidebar_name_fall_back_to_the_generic_ipod() {
        let device = full_device();
        assert_eq!(ready_heading(&device), "Video (Black)");
        assert_eq!(sidebar_name(&device), "iPod user");

        let mut bare = full_device();
        bare.model.clear();
        assert_eq!(ready_heading(&bare), msgid("iPod"));
        bare.name.clear();
        assert_eq!(sidebar_name(&bare), msgid("iPod"));
    }

    #[test]
    fn overview_rows_carry_four_rows_in_ui_order_with_full_facts() {
        let rows = overview_rows(&full_device());
        let expected = [
            (
                msgid("Capacity"),
                OverviewValue::CapacityGb("14.91".to_string()),
            ),
            (
                msgid("Serial number"),
                OverviewValue::Data("YM6312345Y5N".to_string()),
            ),
            (msgid("Format"), OverviewValue::Data("FAT".to_string())),
            (msgid("Firmware"), OverviewValue::Data("1.3".to_string())),
        ];
        assert_eq!(rows.len(), expected.len(), "the UI contract is four rows");
        for (row, (label, value)) in rows.iter().zip(expected.iter()) {
            assert_eq!(row.label, *label);
            assert_eq!(&row.value, value);
        }
    }

    #[test]
    fn overview_rows_apply_the_placeholder_rule_to_absent_facts() {
        let mut device = full_device();
        device.serial_number = None;
        device.firmware = None;
        device.total_capacity = None;
        device.filesystem = None;
        let rows = overview_rows(&device);
        assert_eq!(rows[0].value, OverviewValue::Unavailable, "capacity");
        assert_eq!(rows[1].value, OverviewValue::Unavailable, "serial");
        assert_eq!(
            rows[2].value,
            OverviewValue::Unavailable,
            "format falls back to the placeholder without a measured filesystem"
        );
        assert_eq!(rows[3].value, OverviewValue::Unavailable, "firmware");
    }

    #[test]
    fn free_bytes_is_total_minus_used_or_none() {
        assert_eq!(free_bytes(&full_device()), Some(12_859_051_520));

        let mut full = full_device();
        full.used_capacity = full.total_capacity;
        assert_eq!(free_bytes(&full), Some(0));

        let mut bare = full_device();
        bare.used_capacity = None;
        assert_eq!(free_bytes(&bare), None);
    }

    #[test]
    fn capacity_fraction_is_used_over_total_clamped() {
        let fraction = capacity_fraction(&full_device()).expect("both capacities present");
        let expected = 2_048_000_000_f64 / 14_907_051_520_f64;
        assert!((fraction - expected).abs() < 1e-9);

        let mut full = full_device();
        full.used_capacity = Some(0);
        assert_eq!(capacity_fraction(&full), Some(0.0));
        full.used_capacity = full.total_capacity;
        assert_eq!(capacity_fraction(&full), Some(1.0));

        full.total_capacity = None;
        assert_eq!(capacity_fraction(&full), None);
    }

    #[test]
    fn support_warning_follows_the_policy_verdicts() {
        assert_eq!(support_warning(&full_device()), None);

        let mut unsupported = full_device();
        unsupported.model_supported = false;
        assert_eq!(
            support_warning(&unsupported),
            Some(msgid(
                "This model is not in the supported set (Video 5/5.5G, Classic 6/7G)."
            ))
        );

        let mut no_checksum = full_device();
        no_checksum.checksum_possible = false;
        assert_eq!(
            support_warning(&no_checksum),
            Some(msgid(
                "No FireWire ID — the device database checksum cannot be generated for this Classic."
            ))
        );
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
    fn the_device_name_shows_only_while_connecting() {
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::BackendUnavailable,
            ConnectionState::Ready,
            ConnectionState::Error,
        ] {
            let decision = decide(&rich_snapshot(state));
            assert!(
                !decision.show_device,
                "{state:?} must not render the device-name label"
            );
        }
        assert!(
            decide(&rich_snapshot(ConnectionState::Connecting)).show_device,
            "Connecting renders the device name"
        );
        // An absent device (or an empty name) renders nothing even
        // while connecting.
        assert!(!decide(&snapshot_with(ConnectionState::Connecting)).show_device);
        let mut snapshot = rich_snapshot(ConnectionState::Connecting);
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
