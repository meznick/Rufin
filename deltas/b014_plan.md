# B-014: Device Overview view (iTunes-style Ready shell)

Status: owner-approved (2026-11-17, demo + Q&A; D-8..D-11 defaults
accepted at "go"); implemented as written with two recorded deviations
(§11).

## 1. What this task is

B-014 replaces the minimal B-013 Ready arm with the real device
overview, shaped as the owner's demo: the classic iTunes three-region
device window — left sidebar (device identity + nav sections), main
content (the Обзор page), bottom bar (storage + Sync). Spec reference:
R-03 (Device Info), S-06 (Info display). Plan reference: Phase 3, Data
Models & Interfaces. Task verification: the Ready view displays actual
name/model, firmware or unavailable, and measured capacity without
fabricated defaults or nominal-capacity substitution.

The demo is a structure reference, not a pixel spec: Rufin renders it
in its own adwaita styling (no Windows chrome, no title/menu bars —
the route shell already provides those). Only the `ready` stack page
rebuilds; the other five state pages stay exactly as B-013 shipped
them, and the interrupted-notice banner stays above the whole stack.

Honesty rules (S-06, unchanged from the first draft):

- **Measured capacity only.** The nominal model-table capacity
  (`ipod::ModelInfo::capacity_gb`) deliberately never reaches
  `gtk-library`; B-014 must not re-introduce it.
- **Owner's placeholder rule (2026-11-17):** anything our model does
  not provide renders with a simple "Unavailable" placeholder — never
  invented. Applies to firmware (always `None` on real pods today —
  the pinned build exports no `SysInfoExtended` parser symbols), to
  `Формат` (the DB-format marker we do not track — a model gap, not a
  device gap), and to every absent optional field.

Scope is the Ready shell + overview. No sync controls beyond the
insensitive placeholders (B-015), no progress/results (B-016), no
notice dismissal (B-015/016). Nothing writes to the device or the
`.itl`.

## 2. What already exists (measured, not guessed)

- `ipod_page.rs` Ready arm (B-013): a centered `empty-state` box
  (icon, `ready_heading` = "Ready", `ready_device` = the name label),
  marked as B-014's takeover point. `decide()`'s Ready arm sets
  `show_device`.
- `DeviceInfo` (B-002, `device_manager.rs:56`): `name`/`model`/
  `generation` `String` (empty = "not retrieved"), `model_number`/
  `firmware`/`total_capacity`/`used_capacity`/`firewire_id` `Option`,
  `mount_point`, the policy verdicts `model_supported`/
  `checksum_possible` (informational, b006 delta decision 12). **No
  serial number field yet** — but the value exists: the `ipod` crate's
  `DeviceInfo.sysinfo` carries the raw `SysInfo` pairs, and the
  documented key set includes `"SerialNumber"` (`ipod/types.rs:85`).
  Adding it is a small, honest data-layer change (§3.6).
- **Ready guarantees** (`set_ready_after_preflight`,
  `device_manager.rs:169`): both capacities `Some`, total > 0, used ≤
  total. Ready-reachable absent values: `firmware` (always today),
  `model_number`/`firewire_id`/`serial` (device-dependent), empty
  `name`/`model`/`generation` (unresolved models reach Ready per the
  model policy).
- `name` provenance (B-005): master-playlist name, `""` when absent —
  no volume-label fallback. The demo's sidebar name may therefore be
  empty; the fallback is the generic "iPod".
- No byte-formatting helper exists in the workspace (searched); B-014
  adds a small pure one locally. No row-list widget infra in
  `gtk-library` — the overview uses plain grids/labels with the
  existing `muted`/`dim-label` classes.
- i18n: `msgid`/`tr`, pot via `xtask generate i18n-template`. Widget
  construction is untestable in this repo — the pure-layer policy
  stands: every semantic lives in display-free functions.

## 3. The design

### 3.1 The Ready shell (D-1)

The `ready` stack page rebuilds from the centered empty-state into a
horizontal `GtkBox`:

- **Sidebar** (fixed ~240 px, adwaita list styling): device header +
  two nav sections (§3.2).
- **Separator**, then a vertical content column:
  - the **content stack** — today exactly one page, "Обзор" (§3.3);
    B-015 adds the "Музыка" page behind the same stack;
  - the **bottom bar** (§3.5).

Top-aligned, filling the route content. All other page chrome (topbar
button, notice banner) is untouched.

### 3.2 The sidebar (D-2)

1. **Device header**: iPod icon (28 px) + a column — the device name
   (fallback "iPod" via the pure `sidebar_name`) and a **capacity
   badge** (measured total, whole GB — §3.7) — plus a **Scan-again
   icon button** (refresh icon, accessible label "Scan for connected
   iPods") at the header's end (D-11). The manual-scan policy (B-012)
   leaves Ready without an on-page refresh otherwise.
2. **"Настройки" section**: Обзор (the single active row, click is a
   no-op — it is already the visible page), Музыка, Фильмы, Телешоу,
   Фото — all but Обзор insensitive. Structural furniture for the
   sync-options pages B-015+ will add.
3. **"На моем устройстве" section**: insensitive category rows
   (Музыка, Фильмы, Телешоу, Аудиокниги, Покупки) — the demo's
   structural items without the demo's fake playlist names (those are
   real pod data; on-device browsing is B-015+).

Both nav sections are static, greyed, and documented as furniture —
no navigation, no data reads.

### 3.3 The Обзор page (D-3, D-4)

- **Heading** (`h2` in the demo): the model name via the pure
  `ready_heading` — `model` → generic "iPod". The demo shows "iPod";
  there is no "Ready" state label anywhere on the page — the page
  itself is the state (the B-013 `ready_heading`/subtitle idea is
  dropped).
- **Info box** (two-column grid, left/right as in the demo) with four
  rows, all always rendered, absent → translated "Unavailable"
  (owner's placeholder rule):
  - **Емкость**: measured total capacity, decimal GB, two decimals
    (the demo's precision: "58,14 ГБ") — §3.7;
  - **Серийный номер**: the serial from `SysInfo` — the new
    `serial_number` field (§3.6);
  - **Формат**: always the "Unavailable" placeholder until the model
    tracks the DB-format marker — documented as a model gap, not a
    device fact;
  - **Версия ПО**: firmware, "Unavailable" on every real pod today
    (the S-06 literal).
- The demo's update-check sentence ("У Вас самое новое ПО…") is
  omitted — an iTunes update check does not exist here; inventing the
  text would be fabrication.
- **Обновить / Восстановить iPod… buttons**: rendered insensitive
  (D-6) — furniture, documented as permanently out of scope (restore
  wipes the pod; Rufin never offers it).

### 3.4 The Параметры box (D-6)

The demo's five checkbox rows — "Открывать iTunes при подключении",
"Синхронизировать только отмеченные песни и видео", the bitrate
select row, "Обрабатывать музыку и видео вручную", "Использовать как
внешний диск" — plus "Сбросить предупреждения": all rendered
insensitive, exactly the demo's structure. Translatable copy; the
rows are furniture until B-015 defines real sync options (the labels
stay as the demo wrote them — the owner chose fidelity to the demo).

### 3.5 The bottom bar (D-7)

- **Storage bar**: a `GtkProgressBar`-style used/free two-segment bar
  (used accent-colored, free muted) — **no** per-kind Аудио/Другое
  split: that needs per-kind byte accounting the sync layer does not
  have yet; the split is deferred to B-015/16.
- **Free text** centered over the bar: "Свободно: 36,61 ГБ" —
  `format_gb(total − used)`, the demo's copy shape.
- **Синхронизировать**: a single insensitive button, the B-015
  takeover point. **No Готово** — Rufin's iPod page is a route, there
  is no window to close.

### 3.6 Data layer: the serial number (D-9)

Add `serial_number: Option<String>` honestly from `SysInfo`'s
`"SerialNumber"` key:

1. `ipod-sync/src/retrieval.rs`: `DeviceFacts.serial_number`,
   populated in `collect()` from `info.sysinfo` (the key is already
   documented in `ipod/types.rs:85`);
2. `gtk-library/src/device_manager.rs`: `DeviceInfo.serial_number`,
   plus the doc line;
3. `gtk-library/src/ipod_discovery.rs`: `detected_device` → `None`,
   `device_info_from_facts` maps it through;
4. every literal `DeviceInfo`/`DeviceFacts` construction in the test
   fixtures gains the field (the compiler finds them — a mechanical
   sweep in `device_manager`, `ipod_discovery`, `ipod_page`,
   `ipod_connection`, and the retrieval tests).

No other data-layer changes.

### 3.7 The pure layer (all semantics, display-free)

Free functions in `ipod_page.rs`, all GTK-free and gate-tested:

```rust
pub struct OverviewRow {
    pub label: &'static str,      // msgid, tr() at application
    pub value: OverviewValue,
}

pub enum OverviewValue {
    Data(String),      // verbatim device content, plain text only
    Unavailable,       // the placeholder, translated at application
}

pub fn overview_rows(device: &DeviceInfo) -> Vec<OverviewRow>;
pub fn ready_heading(device: &DeviceInfo) -> String;   // model → "iPod"
pub fn sidebar_name(device: &DeviceInfo) -> String;    // name → "iPod"
pub fn capacity_badge(device: &DeviceInfo) -> Option<String>; // whole GB, None when unmeasured
pub fn free_bytes(device: &DeviceInfo) -> Option<u64>; // total − used
pub fn capacity_fraction(device: &DeviceInfo) -> Option<f64>;
pub fn support_warning(device: &DeviceInfo) -> Option<&'static str>; // D-8
pub fn format_gb(bytes: u64) -> String;                // two decimals
```

`overview_rows` returns exactly four rows in UI order — Capacity,
Serial, Format, Firmware — `Data` for present values, `Unavailable`
for `None`/empty, Format always `Unavailable` (§3.3).

`format_gb`: decimal GB (bytes / 1e9, the iTunes convention), exactly
two decimals, round half-up: `2_048_000_000` → `"2.05 GB"`,
`14_907_051_520` → `"14.91 GB"`, `0` → `"0.00 GB"`. Hardcoded `.`
separator (no locale machinery in the repo) — D-10.

`capacity_badge`: measured total rounded to whole GB, half-up
(`14_907_051_520` → `"15 GB"`); `None` when unmeasured (the badge
hides). A summary, not a fabrication — the precise value lives in the
info box.

### 3.8 The policy warning (D-8, new — not in the demo)

`support_warning` maps the b006 policy verdicts to one muted warning
line under the info box: `!model_supported` → "This model is not in
the supported set (Video 5/5.5G, Classic 6/7G).";
`model_supported && !checksum_possible` → "No FireWire ID — the
device database checksum cannot be generated for this Classic.";
both fine → no line. The demo has no such element, but an
unsupported/unresolved model reaches Ready by the 2026-11-10 policy
and must not look identical to a supported one; the actual sync
gating (S-11) stays B-015. Default: keep — see D-8.

### 3.9 Wiring

One new action: the sidebar Scan-again icon button →
`connection.scan()` (clone the facade, the B-013 pattern). The nav
rows, the greyed buttons, and the greyed checkboxes carry no handlers.

`apply_snapshot`'s Ready arm: apply the heading, the sidebar name, the
badge, the four rows, the warning line, the bar fraction, the free
text. `decide()` itself is unchanged except that the Ready arm stops
using `show_device` (the name/model move to the sidebar/heading), so
the B-013 gate `the_device_name_shows_only_on_connecting_and_ready`
shrinks to Connecting (§5.7).

### 3.10 Copy (proposed, translatable)

- Sidebar sections: "Настройки" ("Settings"), "На моем устройстве"
  ("On my device"); nav items: "Обзор", "Музыка", "Фильмы",
  "Телешоу", "Фото", "Аудиокниги", "Покупки".
- Rows: "Емкость" ("Capacity"), "Серийный номер" ("Serial number"),
  "Формат" ("Format"), "Версия ПО" ("Software version").
- Placeholder: "Unavailable".
- Options box: the demo's five labels + "Сбросить предупреждения"
  ("Reset warnings"); buttons: "Обновить" ("Update"),
  "Восстановить iPod…" ("Restore iPod…").
- Bottom bar: "Свободно: {value}" ("Free: {value}"),
  "Синхронизировать" ("Sync").
- Warnings (§3.8); scan button accessible label "Scan for connected
  iPods".

## 4. Integration points

1. `crates/ipod-sync/src/retrieval.rs` (+ its tests) — the
   `serial_number` field.
2. `crates/gtk-library/src/device_manager.rs` (+ tests) — the
   `serial_number` field, doc line, fixture sweep.
3. `crates/gtk-library/src/ipod_discovery.rs` — the two construction
   sites.
4. `crates/gtk-library/src/ipod_page.rs` — the pure layer (§3.7),
   the Ready arm application, the scan wiring, the module-doc
   takeover markers, the gate updates.
5. `crates/gtk-library/resources/routes/ipod_page.ui` — the `ready`
   page rebuild, new `composite_box!` fields.
6. `crates/gtk-preferences/resources/style.css` — minimal additions
   (sidebar rows, badge, bar segments, warning line); reuse
   `muted`/`dim-label`/`section-heading`/`.ipod-notice` elsewhere.
7. `locales/rufin.pot` regeneration.
8. No spec change → no delta file. tasks.md checkoff + docs §9 entry
   land with the merge hygiene (post-FF), not on the task branch.

## 5. Verification: gates (unit tests, display-free)

In `ipod_page.rs`:

1. `format_gb` two-decimal matrix: `0` → `"0.00 GB"`; `1` →
   `"0.00 GB"`; `500_000_000` → `"0.50 GB"`;
   `1_000_000_000` → `"1.00 GB"`; `1_050_000_000` → `"1.05 GB"`;
   `2_048_000_000` → `"2.05 GB"`; `14_907_051_520` → `"14.91 GB"`.
2. `capacity_badge`: `14_907_051_520` → `"15 GB"`;
   `1_050_000_000` → `"1 GB"`; `500_000_000` → `"1 GB"` (half-up);
   `None` → `None` (badge hides).
3. `ready_heading`: model → model; empty model → "iPod".
   `sidebar_name`: name → name; empty name → "iPod".
4. `overview_rows` matrix: exact four-row sequence (labels + kinds —
   the order is the UI contract); full facts → `Data` for capacity
   and serial, `Unavailable` for Format even with everything present;
   `serial: None`/`firmware: None`/capacity `None` → `Unavailable`
   (the owner's placeholder rule).
5. `free_bytes`: `(14_907_051_520, 2_048_000_000)` →
   `12_859_051_520`; used == total → `0`; `None` arms → `None`.
6. `capacity_fraction`: ≈ 0.1374 (approx assert); used `0` → `0.0`;
   used == total → `1.0`; `None` arms → `None`.
7. Update the B-013 gate: `show_device` is Connecting-only (the Ready
   arm of the matrix drops).
8. `support_warning`: `(false, _)` → model msgid; `(true, false)` →
   checksum msgid; `(true, true)` → `None`.

In `ipod-sync`: the retrieval tests gain the serial assertion (present
in the seeded snapshot's SysInfo, absent otherwise).

Housekeeping: clippy 0 on the touched crates, workspace check, i18n
check, `git diff --check`, fmt on touched files, and the B-013
composite cross-check (every `composite_box!` field id exists in the
.ui) before commit.

Manual acceptance: real pod Ready walk — sidebar name + badge, model
heading, capacity/free numbers against the pod's real size, firmware
and Format "Unavailable", serial from SysInfo, the greyed furniture
(the demo's picture), the Scan-again button, the warning line via the
`IPOD_TEST_TREE` fake tree with an out-of-set model.

## 6. Scope boundaries (not this task)

Sync button behavior / dry run / real options (B-015); progress and
results (B-016); notice dismissal and recovery (B-015/016); hardware
acceptance (B-017); smart criteria (B-023); on-device browsing (the
"На моем устройстве" data); per-kind storage accounting; firmware
update / restore (permanently out of scope — the greyed buttons are
furniture). S-11's gating surface stays B-015 — the D-8 warning line
is informational, not a gate.

## 7. Risks

- **Stale facts**: no event-driven rescan by design (B-012 decision) —
  the sidebar Scan-again (D-11) is the manual remedy. Accepted.
- **Untrusted content**: every device string is device-controlled —
  plain text only, never markup mode; long values wrap/ellipsize.
- **Hardcoded decimal separator**: no locale machinery in the repo;
  `.` documented in `format_gb` (D-10).
- **Furniture decay**: the greyed rows/buttons encode the demo's
  structure, not promises; several (Восстановить, Формат) are
  permanently out of scope — the plan documents each so later tasks
  don't treat them as backlog.
- **The Ready look changes** what B-013 accepted — the manual
  acceptance re-walks the Ready page entirely.

## 8. Decisions for the owner

Owner-answered via the demo + Q&A (2026-11-17), encoded above:

- D-1: the Ready state rebuilds as the three-region iTunes shell;
  the other five state pages stay untouched (§3.1).
- D-2: sidebar nav sections render as greyed furniture, both
  sections, no fake playlist names (§3.2).
- D-3: heading = model → "iPod"; sidebar = name → "iPod"; no "Ready"
  state label (the demo has none) (§3.3).
- D-4: every overview row always renders; absent → "Unavailable"
  (the owner's placeholder rule); Формат is a permanent placeholder
  until the model tracks it (§3.3).
- D-5: measured capacity only, two decimals, whole-GB badge, free =
  total − used (§3.3/§3.5/§3.7).
- D-6: the Параметры box and Обновить/Восстановить render greyed,
  exactly the demo's structure (§3.3/§3.4).
- D-7: the bottom bar shows used/free + free text + a greyed
  Синхронизировать, no Готово; the per-kind segment split is
  deferred (§3.5).

New, pending the owner's sign-off:

- D-8: the model-policy warning line under the info box — not in the
  demo, but an unsupported-model pod must not look identical to a
  supported one. Default: keep (§3.8).
- D-9: add `serial_number` to the data model (SysInfo is already
  parsed; this is plumbing, not new parsing). Default: yes (§3.6).
- D-10: hardcoded `.` decimal separator in `format_gb` (the demo uses
  `,`; locale-aware formatting is not worth machinery now). Default:
  `.` (§3.7).
- D-11: the Scan-again icon button lives in the sidebar device header
  (the demo has no scan control; the manual-scan policy needs one on
  Ready). Default: sidebar header (§3.2).

## 9. Documentation follow-ups (done with the task)

- Remove the "B-014 takes over" markers (`ipod_page.rs` module doc +
  the Ready arm comment); document the furniture rows/buttons as such
  (a comment, not user-facing).
- `locales/rufin.pot` regen; tasks.md checkoff + docs §9 entry land
  with the merge hygiene (post-FF), not on the task branch.

## 10. Implementation steps

1. Data layer: `serial_number` through `ipod-sync` →
  `device_manager` → `ipod_discovery` + the fixture sweep; run the
  suites.
2. Pure layer + gates (§3.7, §5.1–5.6, 5.8); update the `show_device`
  gate (§5.7).
3. `ipod_page.ui`: rebuild the `ready` page (§3.1–3.5); update the
  `composite_box!` fields.
4. `apply_snapshot` Ready arm (heading, sidebar, rows, warning, bar,
  free text); scan wiring (§3.9).
5. CSS additions; pot regen.
6. Housekeeping battery (clippy/fmt/workspace/i18n/diff-check) +
  composite cross-check.
7. Manual acceptance (owner machine: real pod + fake-tree warning).

## 11. Implementation results

**Done 2026-11-17** on `feature/ipod-page-task-B-014`, per §10, with
all owner decisions as written:

1. Data layer (D-9): `serial_number: Option<String>` honestly from
   `SysInfo`'s `"SerialNumber"` key — the `ipod` crate gained a
   `serial_number()` helper next to `firewire_id()` (with the
   SysInfoExtended-key distinction documented),
   `ipod-sync/retrieval.rs` gained the `DeviceFacts` field + the
   provenance doc line, `device_manager.rs`/`ipod_discovery.rs` the
   `DeviceInfo` field and the two construction sites, and the
   compiler-driven fixture sweep filled every test literal. New
   gates: retrieval serial pass-through/absence; the device-manager
   optional-fields test carries a serial arm.
2. `ipod_page.rs`: the pure layer — `overview_rows` (four rows in UI
   order: Capacity, Serial, Format, Firmware; absent → the
   `Unavailable` placeholder, Format permanent), `ready_heading`
   (model → "iPod"), `sidebar_name` (name → "iPod"), `capacity_badge`
   (whole GB, `None` hides), `free_bytes`, `capacity_fraction`
   (clamped), `support_warning` (the D-8 verdict mapping),
   `format_gb`. The Ready arm of `decide()` stops using `show_device`
   (the show_device gate is now Connecting-only); `apply_snapshot`
   applies the shell; `apply_overview` renders the rows;
   `ipod_route_view` wires the sidebar refresh → `connection.scan()`
   (D-11).
3. `ipod_page.ui`: the `ready` page rebuilt as the three-region
   shell (D-1) — sidebar with device header (icon, name, badge,
   refresh button) + both greyed nav sections (D-2), the Обзор
   content (heading, info-box grid, D-8 warning label, the greyed
   Параметры box with the demo's five checkboxes + reset button and
   the greyed Обновить/Восстановить, D-6), and the bottom bar
   (used/free progress bar with the "Free:" text, greyed Sync, no
   Готово — D-7). Furniture sites carry XML comments marking them as
   placeholders.
4. `style.css`: `.ipod-sidebar`, `.ipod-badge`, `.ipod-nav-title`,
   `.ipod-nav-list`, `.ipod-nav-active`, `.ipod-info-box`,
   `.ipod-warning`, `.ipod-bottom-bar`. `locales/rufin.pot`
   regenerated (76 new strings).
5. Gates (§5): 8 new display-free matrices in `ipod_page.rs` —
   `format_gb` (seven cases), `capacity_badge` (rounding + hide),
   heading/name fallbacks, `overview_rows` order + full facts,
   placeholder rule, `free_bytes`, `capacity_fraction`, policy
   warnings — plus the renamed Connecting-only `show_device` gate
   and the retrieval serial gate.

Battery: gtk-library 129 lib tests (121 + 8) + 3 env-gated
connection gates green; ipod-sync 117 (116 + 1 serial gate); ipod
48; clippy 0 on gtk-library/ipod-sync/ipod; workspace + i18n checks
green; touched files fmt-clean (the pre-existing toolchain drift
stays untouched); the composite cross-check passes 28 bound fields
against the .ui.

**Deviations from the plan (both recorded, neither owner-visible):**

- §5.1/§3.7: `format_gb` returns the number only ("2.05"), not
  "2.05 GB" — the unit is the thin layer's `tr_with("{value} GB")`,
  so the unit translates ("ГБ") instead of hardcoding English. The
  gates assert the number strings; the composition stays untested by
  the widget-free policy.
- §3.4: the demo's bitrate select renders as static text inside the
  checkbox label ("…to 256 kbps AAC") — a DropDown needs a model and
  the row is greyed furniture.

**Gate-forced correction (found by the owner's launch, 2026-11-17):**

- The first template build failed at runtime: "Can't use <layout>
  here". Cause: in GTK4's GtkBuilder, GtkGrid's `<layout>` element
  belongs **inside** the `<object>` element (the GTK3-style
  outside-placement was applied to the firmware cell). Caught by
  `gtk4-builder-tool validate` — which the composite cross-check does
  not cover (ids only, no XML semantics). **The validation tool is now
  part of the housekeeping loop for every `.ui` change**: the minimal
  reproduce (layout in/out) pinned the GTK4 rule before the fix, and
  `gtk4-builder-tool validate ipod_page.ui` passes clean.

Manual acceptance (owner machine, env-gated backend) remains: real
pod Ready walk (name, badge, model heading, capacity/free numbers,
firmware + Format "Unavailable", serial), the greyed furniture
picture, the refresh button, the warning line via the fake tree.
