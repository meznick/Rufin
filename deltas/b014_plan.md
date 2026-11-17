# B-014: Device Overview view

Status: draft — awaiting owner approval (2026-11-17).

## 1. What this task is

B-014 replaces the minimal B-013 Ready arm with the real device
overview. Spec reference: R-03 (Device Info), S-06 (Info display). Plan
reference: Phase 3, Data Models & Interfaces. Task verification: the
Ready view displays actual name/model, firmware or unavailable, and
measured capacity without fabricated defaults or nominal-capacity
substitution.

Two S-06 honesty rules drive the design:

- **Measured capacity only.** The nominal model-table capacity
  (`ipod::ModelInfo::capacity_gb`, display-only per `ipod/types.rs`)
  deliberately never reaches `gtk-library`: `DeviceFacts` and
  `DeviceInfo` carry only the `statvfs` measurements, so nominal
  substitution is already impossible at the type level. B-014 must not
  re-introduce it (e.g. by mapping from the model name).
- **"Unavailable", never invented.** The pinned libgpod build does not
  export the `SysInfoExtended` parser symbols, so `firmware` is `None`
  on every real pod today; S-06 explicitly says the overview shows the
  firmware as unavailable rather than inventing a value. The same rule
  applies to every other absent field.

Scope is the Ready page content only. No sync controls (B-015), no
progress/results (B-016), no notice dismissal (B-015/016). Nothing
writes to the device or the `.itl`.

## 2. What already exists (measured, not guessed)

- `ipod_page.rs` Ready arm (B-013): `ready_view` — a centered
  `empty-state` box (icon 64 px, `ready_heading` = "Ready" via
  `apply_snapshot`, `ready_device` = the name label). `decide()`'s
  Ready arm sets `show_device`; the arm is marked as B-014's takeover
  point.
- `DeviceInfo` (B-002 contract, `device_manager.rs:56`): `name`/
  `model`/`generation` are `String` (empty = "not retrieved", never
  fabricated — `ipod_discovery.rs` `detected_device`), `model_number`/
  `firmware`/`total_capacity`/`used_capacity`/`firewire_id` are
  `Option`, plus `mount_point` (always known) and the policy verdicts
  `model_supported`/`checksum_possible` (informational since the
  2026-11-10 model policy, b006 delta decision 12).
- **Ready guarantees** (`set_ready_after_preflight`,
  `device_manager.rs:169`): both capacities are `Some`, total > 0,
  used ≤ total. The only Ready-reachable absent values are `firmware`
  (always, today), `model_number`/`firewire_id` (device-dependent),
  and empty `name`/`model`/`generation` strings (an unresolved model
  still reaches Ready per the model policy — `model_supported` is
  informational).
- `name` provenance (B-005, `ipod-sync/retrieval.rs`): master-playlist
  name, `""` when absent — no volume-label fallback. So even Ready can
  carry an empty name.
- No byte-formatting helper exists anywhere in the workspace (searched:
  `format_bytes`/`human_bytes`/`pretty_bytes`). B-014 adds a small pure
  one locally.
- No row-list widget infra in `gtk-library`: the `.album-detail-row`
  pattern is a composite row widget (`album_detail_row.ui`) with
  buttons/gestures — too heavy to lift. The overview uses a plain
  `GtkGrid` with two label columns, styled with the existing
  `muted`/`dim-label` classes.
- i18n: `msgid`/`tr` pattern (`ipod_page.rs`), pot via
  `cargo run -p xtask -- generate i18n-template`.
- Widget construction is untestable in this repo (GtkWidget class init
  asserts the main thread) — the pure-layer policy from B-012/B-013
  stands: every semantic lives in display-free functions, the widget
  application stays thin.

## 3. The design

### 3.1 The pure layer (all semantics, display-free)

New free functions in `ipod_page.rs`, all GTK-free and gate-tested:

```rust
/// One overview row: a translatable label msgid and its value.
pub struct OverviewRow {
    /// Row label (msgid, `tr()` at application time).
    pub label: &'static str,
    pub value: OverviewValue,
}

pub enum OverviewValue {
    /// Verbatim device data — device-controlled content, always set as
    /// plain text at application (never markup mode).
    Data(String),
    /// The S-06 "unavailable" placeholder — translated at application.
    Unavailable,
    /// Capacity: composed at application from the measured bytes via
    /// `format_gb` and the translated "used of" connector (see §3.1.3).
    Capacity { used: Option<u64>, total: Option<u64> },
}

pub fn overview_rows(device: &DeviceInfo) -> Vec<OverviewRow>;
pub fn ready_heading(device: &DeviceInfo) -> String;
pub fn capacity_fraction(device: &DeviceInfo) -> Option<f64>;
pub fn support_warning(device: &DeviceInfo) -> Option<&'static str>;
pub fn format_gb(bytes: u64) -> String;
```

#### 3.1.1 Row set and placeholder rules

Three groups, in order (group labels are section headers, not rows):

- **Device**: Model (`model`), Generation (`generation`), Model number
  (`model_number` — **row hidden when `None`**), Firmware (`firmware` —
  **row always rendered**, `Unavailable` when `None`, the S-06 literal).
- **Storage**: Capacity (the `Capacity` variant; `Unavailable` when
  either measurement is missing — defensive, Ready guarantees both,
  see §2).
- **Connection**: Mount point (`mount_point` display string, always),
  FireWire ID (`firewire_id` — **row hidden when `None`**).

Empty `model`/`generation` strings render as `Unavailable` (an
unresolved model is Ready-legitimate per the model policy) — same rule
as firmware, one honest placeholder everywhere. `format_gb` output is
`Data`, not markup.

#### 3.1.2 Heading

The heading is the device name with an honest fallback chain, never an
invented name: `name` → `model` → `generation` → the translated
generic "iPod" (a product label, not a fabricated device name). The
"Ready" state label moves to a muted subtitle under the heading — the
state is implicit in the page itself (see D-2).

#### 3.1.3 Capacity formatting

`format_gb`: decimal GB (bytes / 1e9 — the iTunes convention), exactly
one decimal place, round half-up: `2_048_000_000` → `"2.0 GB"`,
`14_907_051_520` → `"14.9 GB"`, `1_050_000_000` → `"1.1 GB"`, `0` →
`"0.0 GB"`, sub-GB stays one decimal (`"0.5 GB"`). Hardcoded `.`
separator: the repo has no locale machinery; documented.

Composition: `"2.0 GB used of 14.9 GB"` — `format_gb(used)` +
translated connector (msgid "used of") + `format_gb(total)`. Where the
composition lives depends on `tr()`'s test-safety: if `tr` is callable
in tests (no GTK dependency, falls back to the msgid — expected), the
composition is a free `capacity_text(used, total) -> String` and is
gate-tested; if not, the `Capacity` variant composition moves into
`apply_snapshot` (the thin untested layer) and the gates cover
`format_gb` + variant construction only. Fallback is acceptable —
`format_gb` carries the numeric logic either way (see §7).

`capacity_fraction`: `used / total` as `f64`, `None` when either
measurement is `None`; drives the optional capacity bar (D-5).

#### 3.1.4 Support warnings

`support_warning` maps the policy verdicts (b006 delta decision 12) to
an informational banner under the header, display-only:

- `!model_supported` → model msgid (unsupported **or** unresolved —
  the verdict covers both);
- `model_supported && !checksum_possible` → checksum msgid;
- both fine → `None`.

The actual sync gating (S-11, disabled button + reason) stays B-015 —
this banner is overview honesty, not gating.

### 3.2 The Ready page

`ready_view` rebuilds from the centered empty-state into a content
layout (top-aligned, matching the page's `.route-content` style):

1. **Header row**: iPod icon (32 px) + a vertical column — heading
   (`ready_heading`, `section-heading` class, device name via
   `ready_heading()`) with the muted "Ready" state label under it —
   plus a **"Scan again" icon button** at the row end (D-7,
   accessible label "Scan for connected iPods", same as No-iPod's).
2. **Warning banner** (a single label, hidden when `support_warning`
   is `None`): warning icon + the msgid text, styled like the
   existing `.ipod-notice` banner.
3. **Group sections**: "Device" / "Storage" / "Connection" group
   labels (small, muted) above a `GtkGrid` — left column the row
   label (muted), right column the value (wrapped, selectable —
   mount point and FireWire ID benefit from copyable text).
4. **Capacity bar**: a thin `GtkProgressBar` under the Capacity row,
   fraction from `capacity_fraction`, hidden when `None` (D-5).
5. **Bottom of the page**: nothing — documented as B-015's takeover
   point (sync button / dry run), no placeholder widget.

`apply_snapshot`'s Ready arm stops using `show_device` (the name is
the heading now); `decide()` itself is unchanged. `show_device`
remains Connecting-only, so the B-013 gate
`the_device_name_shows_only_on_connecting_and_ready` shrinks to
Connecting (see §5).

### 3.3 Wiring

One new action: the Ready header's Scan-again button →
`connection.scan()` (clone the facade, same pattern as the three B-013
buttons). Rationale: scanning is manual (B-012 decision), the gio
monitor stays unwired, so without this button a replugged pod would
leave the overview stale with no on-page remedy — and Ready is exactly
where the user looks at the facts.

### 3.4 Copy (proposed, translatable)

- Group labels: "Device", "Storage", "Connection".
- Row labels: "Model", "Generation", "Model number", "Firmware",
  "Capacity", "Mount point", "FireWire ID".
- Placeholder: "Unavailable" (matches the S-06 wording).
- Connector: "used of".
- Fallback heading: "iPod".
- Model warning: "This model is not in the supported set (Video
  5/5.5G, Classic 6/7G)."
- Checksum warning: "No FireWire ID — the device database checksum
  cannot be generated for this Classic."
- Scan button: "Scan again" (accessible "Scan for connected iPods").

## 4. Integration points

1. `crates/gtk-library/src/ipod_page.rs` — the pure layer (§3.1), the
   Ready arm application, the scan wiring, the module-doc takeover
   markers, the gate updates.
2. `crates/gtk-library/resources/routes/ipod_page.ui` — the `ready`
   page rebuild, new `composite_box!` fields.
3. `crates/gtk-preferences/resources/style.css` — minimal additions
   (group label, row-grid spacing, warning row); reuse
   `muted`/`dim-label`/`section-heading`/`.ipod-notice` everywhere
   else.
4. `locales/rufin.pot` regeneration (i18n check).
5. **No** `device_manager`/`ipod`/`ipod-sync` changes — the facts all
   exist (B-002/B-005); this is presentation-only.
6. No spec change → no delta file. tasks.md checkoff + docs §9 entry
   land with the merge hygiene (post-FF), not on the task branch.

## 5. Verification: gates (unit tests, display-free)

In `ipod_page.rs`:

1. `format_gb` matrix: `0` → `"0.0 GB"`; `1` → `"0.0 GB"`;
   `500_000_000` → `"0.5 GB"`; `1_000_000_000` → `"1.0 GB"`;
   `1_050_000_000` → `"1.1 GB"` (round half-up);
   `2_048_000_000` → `"2.0 GB"`; `14_907_051_520` → `"14.9 GB"`.
2. `ready_heading` fallback chain: populated name → name; empty name +
   model → model; empty name/model + generation → generation; all
   empty → "iPod".
3. `overview_rows` full-facts matrix: exact row sequence (labels,
   kinds — order is the UI contract), Model number + FireWire ID
   present, firmware `Some`, both capacities `Some`.
4. Sparse matrix: empty `model`/`generation` → `Unavailable`;
   `firmware: None` → `Unavailable` (the S-06 literal);
   `model_number: None`/`firewire_id: None` → rows absent; capacity
   `None` arms → `Unavailable` value + `capacity_fraction: None`.
5. `support_warning`: `(false, _)` → model msgid; `(true, false)` →
   checksum msgid; `(true, true)` → `None`.
6. `capacity_fraction`: `(2_048_000_000, 14_907_051_520)` ≈ 0.1374
   (approx assert); used `0` → `0.0`; used == total → `1.0`; `None`
   arms → `None`.
7. Update the B-013 gate: `show_device` is Connecting-only now (the
   Ready arm of the matrix drops).
8. `capacity_text` composition (if §3.1.3's `tr` fallback holds in
   tests): `"2.0 GB used of 14.9 GB"`.

Housekeeping: `cargo clippy` 0 on the touched crates, `cargo check
--workspace`, i18n check, `git diff --check`, fmt on touched files,
and the B-013 composite cross-check (every `composite_box!` field id
exists in the .ui) before commit.

Manual acceptance: real pod Ready walk — name heading, model/
generation rows, firmware "Unavailable" (pinned build), capacity ≈
the pod's real size, FireWire row absent (the Video case), Scan
again re-runs the scan, warnings via the `IPOD_TEST_TREE` fake tree
with an out-of-set model and a Classic-without-FireWire variant.

## 6. Scope boundaries (not this task)

Sync button / dry run / readiness recheck (B-015); progress and result
rendering (B-016); notice dismissal and recovery actions (B-015/B-016);
hardware acceptance (B-017); smart criteria (B-023). S-11's gating
surface ("sync disabled with a message") stays B-015 — the D-4 banner
is informational, not a gate.

## 7. Risks

- **`tr()` test-safety**: if the `localization::tr` call is not
  test-safe (no catalog in tests → does it return the msgid?), the
  capacity composition moves to `apply_snapshot` and the gate covers
  `format_gb` + variant construction only — a documented, acceptable
  fallback (§3.1.3). Decided at implementation step 1.
- **Untrusted content**: every device string (name, model, mount
  point, FireWire ID) is device-controlled — always plain text, never
  markup mode; long values wrap (max-width on the value column).
- **Stale facts**: no event-driven rescan by design (B-012 decision) —
  the Ready Scan-again button (D-7) is the manual remedy; the
  overview can show pre-replug facts until then. Accepted.
- **Hardcoded decimal separator**: no locale machinery in the repo;
  `.` documented in `format_gb`.
- **The Ready look changes** what B-013 accepted — the manual
  acceptance re-walks the Ready page (name heading + subtitle instead
  of the centered "Ready" empty-state).

## 8. Decisions for the owner

- D-1: layout — content page (header + three groups + scan button),
  replacing the centered empty-state; the page bottom stays empty as
  B-015's takeover point (§3.2).
- D-2: heading — device name with the fallback chain
  name → model → generation → "iPod"; "Ready" becomes the muted
  subtitle (§3.1.2).
- D-3: placeholders — Model/Generation/Firmware/Capacity always
  render, absent → "Unavailable" (S-06); Model number and FireWire ID
  rows are hidden when absent; capacity is measured-only, decimal GB
  with one decimal (§3.1.1/§3.1.3).
- D-4: informational warning banner for `!model_supported` /
  `!checksum_possible`, display-only, copy as proposed; sync gating
  stays B-015 (§3.1.4).
- D-5: thin capacity bar under the Capacity row, hidden when the
  fraction is unavailable (§3.2.4).
- D-6: Scan-again icon button in the Ready header — the manual-scan
  policy leaves Ready without an on-page refresh otherwise (§3.3).

## 9. Documentation follow-ups (done with the task)

- Remove the "B-014 takes over" markers (`ipod_page.rs` module doc +
  the Ready arm comment).
- `locales/rufin.pot` regen; tasks.md checkoff + docs §9 entry land
  with the merge hygiene (post-FF), not on the task branch.

## 10. Implementation steps

1. Pure layer + gates (§3.1, §5.1–5.6, 5.8); decide the `tr` question
   (§7); run the lib suite.
2. Update the `show_device` gate (§5.7).
3. `ipod_page.ui`: rebuild the `ready` page (§3.2); update the
   `composite_box!` fields.
4. `apply_snapshot` Ready arm (rows, warning, bar, heading +
   subtitle); scan wiring in `ipod_route_view` (§3.3).
5. CSS additions; pot regen.
6. Housekeeping battery (clippy/fmt/workspace/i18n/diff-check) +
   composite cross-check.
7. Manual acceptance (owner machine: real pod + fake-tree warnings).

## 11. Implementation results

(filled after implementation — gates, corrections, battery)
