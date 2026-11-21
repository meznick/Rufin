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
//! (storage + Sync). B-015 makes the Sync real: the pure
//! [`sync_controls`] gating, the Dry Run checkbox (dev builds — the
//! S-08 delta), and the [`IpodSyncService`] behind the page's sync
//! sheet (b015_plan §3.2–3.4): one morphing card above the bottom bar
//! that shows the review diff, the running progress, and the result —
//! the B-016 design that replaced the review/result dialogs.
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

use std::path::PathBuf;

use adw::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use localization::{msgid, tr, tr_with};

use crate::library::device_manager::{ConnectionState, DeviceInfo};
use crate::library::ipod_connection::{ConnectionSnapshot, IpodConnection};
use crate::library::ipod_sync_service::{IpodSyncService, SyncPhase, SyncState};

crate::composite_box!(
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
        running_spinner: gtk::Spinner,
        sync_button: gtk::Button,
        review_card: gtk::Box,
        review_heading: gtk::Label,
        review_summary: gtk::Label,
        review_body: gtk::Box,
        float_sheet: gtk::Box,
        float_running: gtk::Box,
        float_action: gtk::Label,
        float_progress_bar: gtk::ProgressBar,
        float_step_label: gtk::Label,
        float_step_fraction: gtk::Label,
        float_result: gtk::Box,
        float_heading: gtk::Label,
        float_summary: gtk::Label,
        float_details: gtk::Box,
        float_footer: gtk::Box,
        float_close: gtk::Button,
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
            "This model is not in the supported set (Video 5/5.5G).",
        ))
    } else if !device.checksum_possible {
        // Dormant while the supported set is Video-only (B-017 D-1):
        // Video never needs a checksum, and unsupported models take the
        // arm above. Becomes reachable the day a Classic re-opens the
        // set.
        Some(msgid(
            "No FireWire ID — the device database checksum cannot be generated for this Classic.",
        ))
    } else {
        None
    }
}

/// The B-015 sync gating decision (b015_plan §3.1): the Sync button is
/// enabled only on a live Ready page with a supported model and no run
/// in flight. The S-11 indication reuses the D-8 policy warning line —
/// the page already renders it; the disabled button plus the line is
/// the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncControls {
    pub sync_enabled: bool,
    pub gating_note: Option<&'static str>,
    pub disabled: bool,
}

/// The pure gating decision for one connection snapshot plus the sync
/// service's phase-derived flag: `disabled` while a run is in flight
/// **or** while the diff is still being computed — the Sync click must
/// never race the live preview (the B-016 feedback).
pub fn sync_controls(snapshot: &ConnectionSnapshot, disabled: bool) -> SyncControls {
    let ready = snapshot.state == ConnectionState::Ready;
    let alive = snapshot.worker_alive;
    let supported = snapshot
        .device
        .as_ref()
        .is_some_and(|device| device.model_supported);
    let gating_note = snapshot
        .device
        .as_ref()
        .filter(|_| ready && alive && !supported)
        .and_then(|device| support_warning(device));
    SyncControls {
        sync_enabled: ready && alive && supported && !disabled,
        gating_note,
        disabled,
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
    /// `sync_disabled` is the sync service's phase-derived flag —
    /// true while a run is in flight or the diff is being computed.
    pub fn apply_snapshot(&self, snapshot: &ConnectionSnapshot, sync_disabled: bool) {
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
                    // The B-015 gating (S-11): the Sync button follows
                    // the snapshot + the phase-derived flag.
                    let controls = sync_controls(snapshot, sync_disabled);
                    imp.sync_button.set_sensitive(controls.sync_enabled);
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
/// The review sheet's one-line summary — the counts joined with the
/// capacity verdict (the widget layer translates; the pure counts live
/// in the service).
fn review_summary_line(summary: &crate::library::ipod_sync_service::ReviewSummary) -> String {
    if summary.nothing_to_do {
        return tr(msgid(
            "Nothing to sync — the device already matches the library.",
        ));
    }
    let mut parts = Vec::new();
    let mut push = |template: &str, count: usize| {
        if count > 0 {
            parts.push(tr_with(template, &[("count", &count.to_string())]));
        }
    };
    push("{count} tracks to add", summary.files_added);
    push("{count} tracks to update", summary.stat_updates as usize);
    push("{count} playlists to update", summary.playlists_updated);
    push("{count} playlists to create", summary.playlists_created);
    push("{count} playlists to remove", summary.playlists_removed);
    push("{count} tracks to remove", summary.files_removed);
    let verdict = if summary.would_fit {
        tr_with(
            "Fits — uses ~{estimate} of {free} free",
            &[
                (
                    "estimate",
                    &unit_gb_text(&format_gb(summary.estimate_bytes)),
                ),
                ("free", &unit_gb_text(&format_gb(summary.free_bytes))),
            ],
        )
    } else {
        tr_with(
            "Does not fit — needs {estimate}, only {free} free",
            &[
                (
                    "estimate",
                    &unit_gb_text(&format_gb(summary.estimate_bytes)),
                ),
                ("free", &unit_gb_text(&format_gb(summary.free_bytes))),
            ],
        )
    };
    parts.push(verdict);
    parts.join(" · ")
}

/// Clear every child of a dynamically-filled box.
fn clear_box(box_: &gtk::Box) {
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
}

/// The initials tile's two glyphs from a title.
fn initials(title: &str) -> String {
    title
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

fn subtitle_line(artist: &str, album: &str) -> String {
    match (artist.is_empty(), album.is_empty()) {
        (false, false) => format!("{artist} · {album}"),
        (false, true) => artist.to_string(),
        (true, false) => album.to_string(),
        (true, true) => String::new(),
    }
}

/// One decimal megabyte, the diff rows' size unit.
fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// One diff row: initials tile, title over artist · album, the
/// right-aligned size or transcode chip.
fn diff_track_row(track: &crate::library::ipod_sync_service::DiffTrack) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("ipod-diff-row");

    let tile = gtk::Label::new(Some(&initials(&track.title)));
    tile.add_css_class("ipod-diff-tile");
    row.append(&tile);

    let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info.set_hexpand(true);
    let title = gtk::Label::new(Some(&track.title));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_single_line_mode(true);
    info.append(&title);
    let subtitle = subtitle_line(&track.artist, &track.album);
    if !subtitle.is_empty() {
        let label = gtk::Label::new(Some(&subtitle));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_single_line_mode(true);
        label.add_css_class("muted");
        info.append(&label);
    }
    row.append(&info);

    if let Some(size) = track.size_bytes {
        let size_label = gtk::Label::new(Some(&format_mb(size)));
        size_label.add_css_class("muted");
        row.append(&size_label);
    } else if track.transcodes {
        let chip = gtk::Label::new(Some(&tr(msgid("AAC 256"))));
        chip.add_css_class("ipod-delta-chip");
        row.append(&chip);
    }
    row.upcast()
}

/// The arrow direction one stat chip renders: the pair the sync
/// actually moves from and to (the B-016 review feedback — a play-count
/// chip that showed higher → lower implied a loss that never happens).
///
/// Counts merge additively: both sides only ever move **up** to the
/// converged total (host catches up with the device's pending plays,
/// the device catches up with a drifted host), so the chip shows
/// min → max. Every other field is host-wins device-ward: the device
/// gets the host's value, so the chip shows device → host — including
/// a rating downgrade, which is real.
fn stat_delta_direction(delta: &crate::library::ipod_sync_service::StatDelta) -> (i64, i64) {
    match delta.field {
        ipod_sync::dry_run::StatField::PlayCount | ipod_sync::dry_run::StatField::SkipCount => {
            (delta.host.min(delta.device), delta.host.max(delta.device))
        }
        _ => (delta.device, delta.host),
    }
}

/// One stat-update row: title/artist and a chip per changed field.
fn stat_track_row(track: &crate::library::ipod_sync_service::StatTrack) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("ipod-diff-row");

    let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info.set_hexpand(true);
    let title = gtk::Label::new(Some(&track.title));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_single_line_mode(true);
    info.append(&title);
    if !track.artist.is_empty() {
        let artist = gtk::Label::new(Some(&track.artist));
        artist.set_xalign(0.0);
        artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
        artist.set_single_line_mode(true);
        artist.add_css_class("muted");
        info.append(&artist);
    }
    row.append(&info);

    let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    chips.set_halign(gtk::Align::End);
    for delta in &track.deltas {
        let (old, new) = stat_delta_direction(delta);
        let old = old.to_string();
        let new = new.to_string();
        let text = match delta.field {
            ipod_sync::dry_run::StatField::Rating => {
                tr_with("rating {old} → {new}", &[("old", &old), ("new", &new)])
            }
            ipod_sync::dry_run::StatField::PlayCount => {
                tr_with("plays {old} → {new}", &[("old", &old), ("new", &new)])
            }
            ipod_sync::dry_run::StatField::SkipCount => {
                tr_with("skips {old} → {new}", &[("old", &old), ("new", &new)])
            }
            ipod_sync::dry_run::StatField::PlayDate => tr(msgid("played date → newer")),
            ipod_sync::dry_run::StatField::VolumeAdjustment => {
                tr_with("volume {old} → {new}", &[("old", &old), ("new", &new)])
            }
        };
        let chip = gtk::Label::new(Some(&text));
        chip.add_css_class("ipod-delta-chip");
        chips.append(&chip);
    }
    row.append(&chips);
    row.upcast()
}

/// A diff section: an expander with a count chip and its rows.
fn diff_section(title: &str, count: usize, rows: Vec<gtk::Widget>, collapsed: bool) -> gtk::Widget {
    let expander = adw::ExpanderRow::builder()
        .title(title)
        .expanded(!collapsed)
        .build();
    let chip = gtk::Label::new(Some(&count.to_string()));
    chip.add_css_class("ipod-count-chip");
    expander.add_suffix(&chip);
    for row in rows {
        expander.add_row(&row);
    }
    expander.upcast()
}

/// A nested member line: a colored +/− prefix over the track name.
fn member_row(
    prefix: &str,
    track: &crate::library::ipod_sync_service::DiffTrack,
    class: &str,
) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ipod-sheet-member");
    let sign = gtk::Label::new(Some(prefix));
    sign.add_css_class(class);
    row.append(&sign);
    let name = gtk::Label::new(Some(&format!(
        "{}{}",
        track.title,
        if track.artist.is_empty() {
            String::new()
        } else {
            format!(" — {}", track.artist)
        }
    )));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_single_line_mode(true);
    name.set_hexpand(true);
    row.append(&name);
    row.upcast()
}

/// One playlist diff row: the name, a delta chip, and the expandable
/// member list. Deferred smart lists render a plain row with a chip.
fn playlist_diff_row(playlist: &crate::library::ipod_sync_service::DiffPlaylist) -> gtk::Widget {
    if playlist.deferred {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("ipod-diff-row");
        let name = gtk::Label::new(Some(&playlist.name));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_single_line_mode(true);
        row.append(&name);
        let chip = gtk::Label::new(Some(&tr(msgid("deferred"))));
        chip.add_css_class("ipod-delta-chip");
        row.append(&chip);
        return row.upcast();
    }

    let expander = adw::ExpanderRow::builder()
        .title(&playlist.name)
        .expanded(true)
        .build();
    let mut delta = String::new();
    if !playlist.adds.is_empty() {
        delta.push_str(&tr_with(
            "+{count}",
            &[("count", &playlist.adds.len().to_string())],
        ));
    }
    if !playlist.removes.is_empty() {
        if !delta.is_empty() {
            delta.push(' ');
        }
        delta.push_str(&tr_with(
            "−{count}",
            &[("count", &playlist.removes.len().to_string())],
        ));
    }
    if playlist.reorder {
        delta.push_str(&format!(" · {}", tr(msgid("order changed"))));
    }
    if !delta.is_empty() {
        let chip = gtk::Label::new(Some(&delta));
        chip.add_css_class("ipod-delta-chip");
        expander.add_suffix(&chip);
    }
    for add in &playlist.adds {
        expander.add_row(&member_row("+", add, "ipod-delta-add"));
    }
    for remove in &playlist.removes {
        expander.add_row(&member_row("−", remove, "ipod-delta-rem"));
    }
    if playlist.reorder {
        let note = gtk::Label::new(Some(&tr(msgid("Member order follows the library."))));
        note.add_css_class("muted");
        note.set_xalign(0.0);
        expander.add_row(&note);
    }
    expander.upcast()
}

/// The always-visible inline review card: the heading, the summary
/// line and the grouped diff sections. No footer — the Sync button is
/// the single action.
fn render_review_card(
    view: &IpodPageView,
    summary: &crate::library::ipod_sync_service::ReviewSummary,
) {
    let imp = view.imp();
    imp.review_heading.set_label(&tr(msgid("Sync review")));
    imp.review_summary.set_label(&review_summary_line(summary));
    if summary.would_fit {
        imp.review_summary.remove_css_class("error-text");
    } else {
        imp.review_summary.add_css_class("error-text");
    }

    clear_box(&imp.review_body);
    if summary.nothing_to_do {
        imp.review_body.set_visible(false);
    } else {
        imp.review_body.set_visible(true);
        let details = &summary.details;
        let mut sections: Vec<gtk::Widget> = Vec::new();
        if !details.adds.is_empty() {
            let rows = details.adds.iter().map(diff_track_row).collect();
            sections.push(diff_section(
                &tr(msgid("Add to device")),
                details.adds.len(),
                rows,
                false,
            ));
        }
        if !details.updates.is_empty() {
            let rows = details.updates.iter().map(stat_track_row).collect();
            sections.push(diff_section(
                &tr(msgid("Update on device")),
                details.updates.len(),
                rows,
                false,
            ));
        }
        if !details.playlists.is_empty() {
            let rows = details.playlists.iter().map(playlist_diff_row).collect();
            sections.push(diff_section(
                &tr(msgid("Playlists")),
                details.playlists.len(),
                rows,
                false,
            ));
        }
        if !details.playlist_creates.is_empty() {
            let rows = details
                .playlist_creates
                .iter()
                .map(playlist_diff_row)
                .collect();
            sections.push(diff_section(
                &tr(msgid("Playlists to create")),
                details.playlist_creates.len(),
                rows,
                false,
            ));
        }
        if !details.playlist_removes.is_empty() {
            let rows = details
                .playlist_removes
                .iter()
                .map(|name| {
                    let label = gtk::Label::new(Some(name));
                    label.set_xalign(0.0);
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    row.add_css_class("ipod-diff-row");
                    row.append(&label);
                    row.upcast()
                })
                .collect();
            sections.push(diff_section(
                &tr(msgid("Playlists to remove")),
                details.playlist_removes.len(),
                rows,
                false,
            ));
        }
        if !details.removals.is_empty() {
            let rows = details.removals.iter().map(diff_track_row).collect();
            sections.push(diff_section(
                &tr(msgid("Remove from device")),
                details.removals.len(),
                rows,
                false,
            ));
        }
        if !details.findings.is_empty() {
            let rows = details
                .findings
                .iter()
                .map(|finding| {
                    let label = gtk::Label::new(Some(finding));
                    label.set_xalign(0.0);
                    label.set_wrap(true);
                    label.add_css_class("ipod-finding");
                    label.upcast()
                })
                .collect();
            sections.push(diff_section(
                &tr(msgid("Findings")),
                details.findings.len(),
                rows,
                details.findings.len() > 3,
            ));
        }
        for section in sections {
            imp.review_body.append(&section);
        }
    }
    imp.review_card.set_visible(true);
}

/// The floating running panel: the discreet single action line, the
/// bar and the step labels — no heading (the prototype round found it
/// collides with the bar), identical in dev and release.
fn render_running_float(view: &IpodPageView, progress: &ipod_sync::sync::report::SyncProgress) {
    let imp = view.imp();
    imp.float_result.set_visible(false);
    clear_box(&imp.float_details);
    imp.float_details.set_visible(false);
    imp.float_footer.set_visible(false);
    imp.float_running.set_visible(true);

    match &progress.item_label {
        Some(label) => {
            imp.float_action.set_label(label);
            imp.float_action.set_visible(true);
        }
        None => imp.float_action.set_visible(false),
    }
    if progress.item_total > 0 {
        imp.float_progress_bar
            .set_fraction(f64::from(progress.item_current) / f64::from(progress.item_total));
        imp.float_step_fraction.set_label(&format!(
            "{} / {}",
            progress.item_current, progress.item_total
        ));
    } else {
        imp.float_progress_bar.set_fraction(0.0);
        imp.float_step_fraction.set_label("");
    }
    if progress.step_total > 0 {
        imp.float_step_label.set_label(&format!(
            "{} — {}",
            tr_with(
                "Step {index} of {total}",
                &[
                    ("index", &progress.step_index.to_string()),
                    ("total", &progress.step_total.to_string()),
                ],
            ),
            progress.step.unwrap_or(""),
        ));
    } else {
        imp.float_step_label.set_label("");
    }
}

/// The floating result panel: the outcome, the failure diagnostic with
/// the resume hint and the collapsed Details expander, and Close.
fn render_result_float(
    view: &IpodPageView,
    summary: &crate::library::ipod_sync_service::SyncOutcomeSummary,
) {
    let imp = view.imp();
    imp.float_running.set_visible(false);
    imp.float_result.set_visible(true);

    let failed = !matches!(
        &summary.status,
        ipod_sync::sync::report::SyncStatus::Completed
    );
    let heading = if failed {
        tr(msgid("Sync failed"))
    } else {
        tr(msgid("Sync completed"))
    };
    imp.float_heading.set_label(&heading);
    if failed {
        imp.float_heading.add_css_class("error-text");
    } else {
        imp.float_heading.remove_css_class("error-text");
    }

    let changes = &summary.changes;
    let mut parts = Vec::new();
    let mut push = |template: &str, count: usize| {
        if count > 0 {
            parts.push(tr_with(template, &[("count", &count.to_string())]));
        }
    };
    push(
        "{count} files written",
        changes.files_added + changes.files_removed,
    );
    push("{count} playlists updated", changes.playlists_written);
    push("{count} covers added", changes.artwork_added);
    push(
        "{count} stats merged",
        changes.host_patches_written + changes.device_patches_written,
    );
    imp.float_summary.set_label(&parts.join(" · "));

    clear_box(&imp.float_details);
    if failed {
        imp.float_details.set_visible(true);
        if let Some(diagnostic) =
            crate::library::ipod_sync_service::outcome_diagnostic(&summary.status)
        {
            let label = gtk::Label::new(Some(&diagnostic));
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.add_css_class("error-text");
            imp.float_details.append(&label);
        }
        if summary.recovery.is_some() {
            let hint = gtk::Label::new(Some(&tr(msgid(
                "The sync can be resumed — reconnect the iPod and sync again.",
            ))));
            hint.set_xalign(0.0);
            hint.set_wrap(true);
            hint.add_css_class("muted");
            imp.float_details.append(&hint);
        }
        if !summary.diagnostics.is_empty() {
            let details = adw::ExpanderRow::builder()
                .title(&tr(msgid("Details")))
                .expanded(false)
                .build();
            for diagnostic in &summary.diagnostics {
                let label = gtk::Label::new(Some(diagnostic));
                label.set_xalign(0.0);
                label.set_wrap(true);
                label.add_css_class("ipod-finding");
                details.add_row(&label);
            }
            imp.float_details.append(&details);
        }
    } else {
        imp.float_details.set_visible(false);
    }
    imp.float_footer.set_visible(true);
}

/// Apply one sync-service state to the page: the in-flight spinner, the
/// running lock on the Sync button, the inline review card and the
/// floating panel — the thin widget layer over the service's pure
/// machine.
fn apply_sync_state(view: &IpodPageView, state: &SyncState) {
    let imp = view.imp();
    let running = matches!(state.phase, SyncPhase::Collecting | SyncPhase::Running);
    imp.running_spinner.set_visible(running);
    imp.running_spinner.set_spinning(running);
    match &state.phase {
        SyncPhase::Review { summary } => {
            render_review_card(view, summary);
            imp.float_sheet.set_visible(false);
        }
        SyncPhase::Running => {
            render_running_float(view, &state.progress);
            imp.float_sheet.set_visible(true);
        }
        SyncPhase::Done(summary) => {
            render_result_float(view, summary);
            imp.float_sheet.set_visible(true);
        }
        // Collecting: the diff is being computed — the card shows
        // a placeholder line so the in-progress state is visible (a
        // bare spinner read as nothing happening, the B-016 feedback).
        SyncPhase::Collecting => {
            imp.review_card.set_visible(true);
            imp.review_heading.set_label(&tr(msgid("Sync review")));
            imp.review_summary
                .set_label(&tr(msgid("Comparing the device and the library…")));
            clear_box(&imp.review_body);
            imp.review_body.set_visible(false);
        }
        SyncPhase::Idle => {
            imp.review_card.set_visible(false);
            imp.float_sheet.set_visible(false);
        }
    }
}

/// The shell's entry point for `Route::Ipod`: the built and subscribed
/// page with the action wiring. Call on the GTK main thread (the
/// subscription attaches to the thread-default main context).
pub fn ipod_route_view(connection: &IpodConnection, sync: &IpodSyncService) -> gtk::Widget {
    let view = IpodPageView::new();
    let imp = view.imp();

    // Scan again (No-iPod / Sync unavailable / Error): a fresh
    // mount-table scan — `retry()` re-applies the stale last report, so
    // a replugged device needs this, not retry. A manual scan also
    // refreshes the live preview (the prepare re-reads the device
    // itself, so the trigger is immediate).
    {
        let connection = connection.clone();
        let sync = sync.clone();
        imp.scan_button.connect_clicked(move |_| {
            connection.scan();
            if let Some(device) = connection.snapshot().device {
                sync.start(true, &device.mount_point);
            }
        });
    }
    {
        let connection = connection.clone();
        let sync = sync.clone();
        imp.unavailable_scan_button.connect_clicked(move |_| {
            connection.scan();
            if let Some(device) = connection.snapshot().device {
                sync.start(true, &device.mount_point);
            }
        });
    }
    {
        let connection = connection.clone();
        let sync = sync.clone();
        imp.error_scan_button.connect_clicked(move |_| {
            connection.scan();
            if let Some(device) = connection.snapshot().device {
                sync.start(true, &device.mount_point);
            }
        });
    }
    // Retry (Error): same-device transient failures — a synthetic replug
    // re-running retrieval + preflight against the last report.
    {
        let connection = connection.clone();
        imp.error_retry_button
            .connect_clicked(move |_| connection.retry());
    }
    // The Ready sidebar's refresh icon (B-014): scanning is manual, so
    // the overview needs its own on-page trigger — and the preview
    // refreshes with it.
    {
        let connection = connection.clone();
        let sync = sync.clone();
        imp.sidebar_scan_button.connect_clicked(move |_| {
            connection.scan();
            if let Some(device) = connection.snapshot().device {
                sync.start(true, &device.mount_point);
            }
        });
    }
    // Sync: the real run. The live preview is the review, so the click
    // is the approval. The service ignores a start while collecting or
    // running — the click cannot double-run.
    {
        let connection = connection.clone();
        let sync = sync.clone();
        imp.sync_button.connect_clicked(move |_| {
            if let Some(device) = connection.snapshot().device {
                sync.start(false, &device.mount_point);
            }
        });
    }
    // The floating result panel's Close.
    {
        let sync = sync.clone();
        imp.float_close.connect_clicked(move |_| sync.dismiss());
    }

    // The connection snapshot + the sync phase drive the Sync button
    // together: both subscriptions re-apply the controls. The button is
    // muted while the diff is being computed, in flight, or the result
    // is showing — the Sync click must never race the live preview.
    let last_snapshot =
        std::rc::Rc::new(std::cell::RefCell::new(Option::<ConnectionSnapshot>::None));
    let sync_disabled = std::rc::Rc::new(std::cell::Cell::new(false));
    // The initial flag follows the service's current phase (the channel
    // carries no replay — a page mounted mid-run must not flash the
    // button enabled).
    sync_disabled.set(!matches!(
        sync.snapshot().phase,
        SyncPhase::Review { .. } | SyncPhase::Idle
    ));

    // The live preview: re-run when the device appears or changes. A
    // failed preview is not re-run automatically (the user closes the
    // result and decides) — the gate against an error panel that
    // reappears forever with no source configured.
    let last_preview_mount = std::rc::Rc::new(std::cell::RefCell::new(Option::<PathBuf>::None));
    let preview_ok = std::rc::Rc::new(std::cell::Cell::new(true));
    let preview_for = {
        let connection = connection.clone();
        let sync = sync.clone();
        let last_preview_mount = last_preview_mount.clone();
        let preview_ok = preview_ok.clone();
        move |phase: &SyncPhase| {
            if !matches!(phase, SyncPhase::Idle) || !preview_ok.get() {
                return;
            }
            let Some(device) = connection.snapshot().device else {
                return;
            };
            let mount = device.mount_point;
            if last_preview_mount.borrow().as_deref() == Some(mount.as_path()) {
                return;
            }
            last_preview_mount.borrow_mut().replace(mount.clone());
            sync.start(true, &mount);
        }
    };

    let connection_receiver = connection.subscribe();
    *last_snapshot.borrow_mut() = Some(connection.snapshot());
    view.apply_snapshot(&connection.snapshot(), sync_disabled.get());
    // The B-016 feedback fix: the device may already be Ready here (a
    // previous route visit scanned it) — the subscription channel
    // carries no replay, so the preview must be triggered against the
    // initial snapshot too, not only on future events.
    preview_for(&sync.snapshot().phase);
    {
        let view = view.clone();
        let last_snapshot = last_snapshot.clone();
        let sync_disabled = sync_disabled.clone();
        let sync = sync.clone();
        let preview_for = preview_for.clone();
        glib::spawn_future_local(async move {
            while let Ok(snapshot) = connection_receiver.recv().await {
                *last_snapshot.borrow_mut() = Some(snapshot.clone());
                view.apply_snapshot(&snapshot, sync_disabled.get());
                preview_for(&sync.snapshot().phase);
            }
        });
    }
    let sync_receiver = sync.subscribe();
    apply_sync_state(&view, &sync.snapshot());
    {
        let view = view.clone();
        let last_snapshot = last_snapshot.clone();
        let sync_disabled = sync_disabled.clone();
        let preview_for = preview_for.clone();
        let preview_ok = preview_ok.clone();
        let last_preview_mount = last_preview_mount.clone();
        glib::spawn_future_local(async move {
            while let Ok(state) = sync_receiver.recv().await {
                let disabled = !matches!(state.phase, SyncPhase::Review { .. } | SyncPhase::Idle);
                sync_disabled.set(disabled);
                if let Some(snapshot) = last_snapshot.borrow().as_ref() {
                    view.apply_snapshot(snapshot, disabled);
                }
                apply_sync_state(&view, &state);
                if let SyncPhase::Done(summary) = &state.phase {
                    preview_ok.set(matches!(
                        summary.status,
                        ipod_sync::sync::report::SyncStatus::Completed
                    ));
                    // The post-run refresh: clear the mount key so the
                    // Idle trigger (after Close) re-previews.
                    last_preview_mount.borrow_mut().take();
                }
                preview_for(&state.phase);
            }
        });
    }

    view.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use ipod::BackendSearch;

    use crate::library::ipod_connection::OperationNotice;

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
            device: Some(crate::library::device_manager::DeviceInfo {
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
    fn full_device() -> crate::library::device_manager::DeviceInfo {
        crate::library::device_manager::DeviceInfo {
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
                "This model is not in the supported set (Video 5/5.5G)."
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

    /// A Ready snapshot over a supported device — the only shape that
    /// enables the Sync button.
    fn ready_snapshot() -> ConnectionSnapshot {
        let mut snapshot = rich_snapshot(ConnectionState::Ready);
        snapshot.device.as_mut().unwrap().model_supported = true;
        snapshot
    }

    #[test]
    fn stat_delta_direction_points_the_way_the_sync_moves() {
        use crate::library::ipod_sync_service::StatDelta;
        use ipod_sync::dry_run::StatField;

        // Counts merge additively: the chip shows min → max, never a
        // loss. Device ahead (pending plays) = the host catches up.
        let pending_plays = StatDelta {
            field: StatField::PlayCount,
            host: 24,
            device: 27,
        };
        assert_eq!(stat_delta_direction(&pending_plays), (24, 27));

        // Host ahead (drift) = the device catches up.
        let drifted_host = StatDelta {
            field: StatField::PlayCount,
            host: 30,
            device: 27,
        };
        assert_eq!(stat_delta_direction(&drifted_host), (27, 30));

        // A rating is host-wins device-ward — the downgrade is real.
        let downgrade = StatDelta {
            field: StatField::Rating,
            host: 60,
            device: 80,
        };
        assert_eq!(stat_delta_direction(&downgrade), (80, 60));
    }

    #[test]
    fn sync_controls_gate_the_button() {
        let controls = sync_controls(&ready_snapshot(), false);
        assert!(
            controls.sync_enabled,
            "Ready + supported + idle enables sync"
        );
        assert_eq!(controls.gating_note, None);

        // Every non-Ready state disables sync.
        for state in [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::BackendUnavailable,
            ConnectionState::Error,
        ] {
            let controls = sync_controls(&rich_snapshot(state), false);
            assert!(!controls.sync_enabled, "{state:?} disables sync");
        }

        // An unsupported model on Ready disables sync with the note.
        let mut unsupported = ready_snapshot();
        unsupported.device.as_mut().unwrap().model_supported = false;
        let controls = sync_controls(&unsupported, false);
        assert!(!controls.sync_enabled);
        assert_eq!(
            controls.gating_note,
            support_warning(unsupported.device.as_ref().unwrap())
        );

        // A disabled flag (run in flight, diff computing) mutes
        // the button regardless.
        let controls = sync_controls(&ready_snapshot(), true);
        assert!(!controls.sync_enabled);
        assert!(controls.disabled);

        // A dead worker disables the button.
        let mut dead = ready_snapshot();
        dead.worker_alive = false;
        assert!(!sync_controls(&dead, false).sync_enabled);

        // No device on Ready (defensive) disables the button.
        let controls = sync_controls(&snapshot_with(ConnectionState::Ready), false);
        assert!(!controls.sync_enabled);
        assert_eq!(controls.gating_note, None);
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
