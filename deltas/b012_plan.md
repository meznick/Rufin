# B-012: iPod page base layout

Status: owner-approved (2026-11-17); D-3 reworked per the owner's
topbar edit; delta `b012_delta.md` covers the spec change; everything not
overridden there is implemented as written.

## 1. What this task is

B-012 is the first UI task: it makes the iPod page exist. Spec reference:
R-01 (Page Visibility, as modified by `deltas/b012_delta.md`), S-01 (No
device connected). Plan reference: Phase 3, first bullet ("Keep the iPod
page visible when disconnected"). Task verification: the page is always
accessible in Rufin, including its No iPod view when disconnected.

Scope is deliberately the skeleton: the topbar entry point, the route, the
page container, the connection-service wiring, and exactly one state view
— the "No iPod detected" view for `Disconnected`. The other four
connection-state views are B-013's contract (S-02..S-05); the device
overview is B-014; the sync controls and the `ProgressPipe` subscription
are B-015/B-016. This task builds the container those views land in and
proves the plumbing they ride on, headless and display-free where
possible.

## 2. What already exists (measured, not guessed)

- The connection service is done and shipped (B-006), but **nothing in
  the app instantiates it yet** — `grep IpodConnection crates/` outside
  gtk-library returns nothing. `gtk_library::ipod_connection::IpodConnection`
  is a cheap cloneable facade: a detached worker thread owns discovery,
  the loaded backend, and the FSM; `snapshot()`, `subscribe()`
  (async-channel, no replay), `scan()`, `retry()`, interrupted-notice
  commands. Production constructor is `IpodConnection::new()`
  (`BackendSearch::FromEnv`).
  `gtk_library::ipod_monitor::IpodMountMonitor::start(connection)` takes
  ownership of the facade, attaches gio volume-monitor handlers with a
  300 ms debounced scan, runs the startup scan, and exposes
  `connection()` for clones.
- `ConnectionSnapshot { state, device, diagnostics, interrupted_operation }`
  with the five-state `ConnectionState` (`Disconnected`, `Connecting`,
  `BackendUnavailable`, `Ready`, `Error`) and per-state `ui_label()`s —
  the B-013 surface, not this task's rendering scope.
- Routing: `Route` (rufin-core `route.rs`, serde + uniffi under
  `bindings`) is in-memory only — nothing persists it, so a new variant
  has no storage impact. The shell renders routes in
  `gtk-shell/src/shell/route.rs` (`render_current_route_content_inner`),
  which special-cases `Route::Search` and falls through to the
  catalog-backed collection path; static non-catalog pages exist as
  precedent (`MountedRoute::static_widget` +
  `route_layout::route_empty_view`). Navigation is
  `shell.navigate(route)` (`RouteStack`), already used by the sidebar
  rail buttons.
- The topbar (`gtk-shell/resources/shell/topbar.ui`, struct in
  `gtk-shell/src/shell/topbar.rs`): the header bar's start group is the
  `navigation` box — sidebar, back, forward, controller buttons, then
  the `casting_host` box. `casting_host` holds the playback-output
  ("this device") button (`shell/build.rs:239`). The search field is the
  title widget. Buttons bind in `Topbar::bind(&self, shell)`; `Topbar`
  fields are instantiated from the `.ui` in `Topbar::new()` — a
  display-free widget construction, testable without a window.
- Icons: `resources/icons/hicolor/scalable/actions/*-symbolic.svg`,
  compiled through the gtk-widgets gresource manifest
  (`crates/gtk-widgets/resources/rufin.gresource.xml`, `include_icons`).
  `build_support::verify_symbolic_icons` forbids raster embeds in symbolic
  SVGs; `ui_resource::verify_icons` has a fixed whitelist of important
  paths checked at startup.
- UI pages: `gtk_widgets::composite_box!` templates in
  `crates/gtk-library/resources/routes/*.ui` (manifest +
  `INTERFACE_RESOURCE_PATHS` test). The `empty-state` / `section-heading`
  / `muted` classes already exist (`placeholder.ui`, global
  `gtk-preferences/resources/style.css`). **Correction (gate-forced, see
  §11):** no test in this repo constructs GTK widgets — class init asserts
  the main thread — so the page's state logic lives in a pure `decide()`
  and the widget application is the thin untested layer every other view
  has. CI excludes the `gtk-*` crates from its test runs (they only
  typecheck there), so these unit tests run locally.
- Localization: `msgid`/`tr`; `locales/rufin.pot` is generated and CI
  checks it (`xtask generate i18n-template --check`).
- The env-gated `ipod_connection_gate` already proves scan → snapshot
  end-to-end headless; B-012 adds no device I/O, so it needs no new
  env-gated gate — unit tests are the gates here.

## 3. The page design

New module `crates/gtk-library/src/ipod_page.rs`:

- `IpodPageView` — a `composite_box!` over a new
  `resources/routes/ipod_page.ui` (pattern of `folders.ui`), containing a
  `gtk::Stack` with exactly two children for now:
  1. the **No-iPod view** (S-01): the `empty-state` composition — a large
     muted `rufin-ipod-symbolic` icon, heading "No iPod detected", body
     copy (proposed: "Connect a supported iPod (Video or Classic) to sync
     music, playlists, and statistics."), and a **status indicator** row
     (a small state-colored dot + the state's `ui_label()` text, with an
     accessible label on the row);
  2. a **stub arm** for every non-`Disconnected` state: a minimal
     neutral placeholder labelled with the state name. It exists so the
     page never renders a wrong state while B-013 builds the real views;
     B-013 replaces it. The stub is visibly temporary (module doc +
     `// B-013 replaces this arm` marker).
- `apply_snapshot(&self, snapshot: &ConnectionSnapshot)` — the single
  rendering entry: switches the stack by `snapshot.state` and updates the
  indicator. Pure and testable; the subscription calls it on the main
  thread.
- Subscription wiring: `subscribe_to(connection: &IpodConnection)` (or a
  constructor taking it) — the page keeps the receiver, spawns the
  channel on the thread-default main context, applies the initial
  `connection.snapshot()` (the channel has no replay), and re-applies on
  each event. This is the seam B-013..B-016 ride on: snapshots drive the
  state views, the interrupted-notice surface, and the overview; the
  `SyncResult`/`ProgressPipe` subscription in B-016 uses the same
  receiver pattern on the same main context.
- Free function `ipod_route_view(connection: &IpodConnection) ->
  gtk::Widget` returning the built+subscribed page — the shell dispatch
  calls it and wraps it in `MountedRoute::static_widget` (no catalog, no
  search provider, no source dependency).

## 4. Integration points

1. `crates/rufin-core/src/route.rs` — `Route::Ipod` variant. Deliberately
   **not** added to `requires_selected_source()` /
   `uses_selected_source()` — the page must work with no configured
   library source (that is the "always accessible" half of R-01). No
   `SidebarRouteItem` variant and no sidebar descriptor: the page is a
   topbar destination (D-3), so the sidebar machinery stays untouched.
2. New icon `resources/icons/hicolor/scalable/actions/rufin-ipod-symbolic.svg`
   — an iPod silhouette drawn in the existing symbolic style (fill
   `#241f31`, 16×16 viewBox, no raster embeds); manifest entry in
   `crates/gtk-widgets/resources/rufin.gresource.xml`; add the path to the
   `verify_icons` whitelist (topbar button = important).
3. `crates/gtk-library/src/ipod_page.rs` + `resources/routes/ipod_page.ui`
   + manifest entry + `INTERFACE_RESOURCE_PATHS` + `pub mod ipod_page;`
   in `lib.rs`.
4. Shell ownership: `crates/gtk-shell/src/shell/mod.rs` gains
   `ipod_monitor: IpodMountMonitor` (owns the facade; the page grabs
   `monitor.connection()` clones) — constructed in
   `crates/gtk-shell/src/shell/build.rs` with `IpodConnection::new()` +
   `IpodMountMonitor::start(...)` on the GTK main thread. The monitor
   lives for the app's lifetime; the worker thread exits when the last
   facade drops.
5. `crates/gtk-shell/src/shell/route.rs` — a `Route::Ipod` arm in
   `render_current_route_content_inner`, right after the `Search` arm and
   before the catalog path:
   `MountedRoute::static_widget(gtk_library::ipod_page::ipod_route_view(&shell.ipod_monitor.connection()))`.
   The page therefore renders even at startup with no source configured
   and survives source switches (no catalog to rebuild).
6. The topbar entry point (D-3):
   - `crates/gtk-shell/resources/shell/topbar.ui`: a `GtkButton id="ipod"`
     appended inside the start `navigation` box, **after `casting_host`**
     (the "this device" picker) and before the search title widget:
     icon `rufin-ipod-symbolic`, tooltip and accessible label "iPod"
     (translatable), classes `flat` + `topbar-button` — the same shape as
     the controller button. It is always visible; no pressed/active state
     (like Controller/Source — the page is a destination, not a toggle).
   - `crates/gtk-shell/src/shell/topbar.rs`: `ipod: gtk::Button` field in
     `Topbar`, instantiated in `Topbar::new()`, bound in `Topbar::bind`
     via the weak-clone pattern → `shell.navigate(Route::Ipod)`.
   - No sidebar-route bookkeeping anywhere: `update_navigation_selection`
     simply finds no matching item for the Ipod route (nothing breaks);
     back/forward history works normally through `RouteStack`.
7. Styling: reuse `empty-state`/`section-heading`/`muted`; add one small
   class (e.g. `.ipod-status-dot` + state modifiers) to
   `crates/gtk-preferences/resources/style.css` for the indicator dot.
8. `locales/rufin.pot` regeneration (`cargo run -p xtask -- generate
   i18n-template`) so the i18n check stays green.

Known non-goals (explicit, not forgotten): the topbar search entry keeps
its default behaviour on the iPod page; the interrupted-operation notice
surface and the retry button are B-013; no dry-run/progress UI; no
active-state styling on the topbar button.

## 5. Verification: gates (unit tests, display-free)

- rufin-core (`route.rs` tests): `Route::Ipod` serializes/deserializes;
  it is not source-scoped (`requires_selected_source` / `uses_selected_source`
  both false); `RouteStack` navigates to and back from `Route::Ipod`
  (extends `route_stack_tests`).
- gtk-library (`ipod_page.rs` tests): the pure `decide()` — a
  `Disconnected` snapshot decides the No-iPod page with the
  "Disconnected" label and the muted dot class; each of `Connecting` /
  `BackendUnavailable` / `Ready` / `Error` decides the stub arm (the
  B-013 takeover point), never the No-iPod arm; the subscription
  roundtrip — `IpodConnection::with_backend_search` with an unreachable
  `Explicit` dir (no device I/O, worker thread real), `subscribe()`,
  initial snapshot, then `set_interrupted_operation(...)` → the notice
  arrives on the channel and does not change the decided state. No GTK
  widgets constructed (see §2 correction); the resource test picks up
  `ipod_page.ui` via `INTERFACE_RESOURCE_PATHS` (automatic once
  registered).
- gtk-shell (`topbar.rs` / `route.rs` tests): the `RouteStack` test
  navigates to and back from `Route::Ipod` (extends `route_stack_tests`).
  The `Topbar` gains the `ipod` button; no widget-level test constructs
  it (see §2 correction) — the button's presence rides on the
  `INTERFACE_RESOURCE_PATHS` test for `topbar.ui`, and the three-line
  `bind()` → `shell.navigate(Route::Ipod)` wiring rides on the tested
  `RouteStack` (the only non-automated line, called out in the results).
- Full battery at the end: `cargo test` (workspace minus the env-gated
  suites), clippy 0, fmt, `cargo check --workspace` (includes
  `rufin-android` — the `Route` uniffi enum gains a variant; the Kotlin
  side is generated at gradle time, not checked by cargo), and the
  i18n-template check.

## 6. Scope boundaries (not this task)

B-013 (five-state views + retry + interrupted-notice surface + the B-006
review debts), B-014 (device overview), B-015 (sync execution UI),
B-016 (progress/results subscription), B-017 hardware acceptance. The
page must not grow their UI now — the stack and the subscription are the
seams, nothing more.

## 7. Risks

- Exhaustive matches over `Route` elsewhere (topbar, route keyboard,
  events) will fail the workspace check; the fallout is mechanical
  match-arm extensions.
- The android crate must still typecheck (uniffi enum extension); if the
  generated Kotlin is checked in anywhere, it stays out of cargo's sight
  — flag at implementation if gradle owns it.
- The subscription tests spawn the real worker thread; they must not
  depend on timing beyond the channel delivery (async-channel recv with a
  generous timeout, per the existing connection tests' style).
- Topbar real estate: one more compact button in the start group. If it
  crowds narrow windows, hiding it in compact mode is a later polish, not
  part of this task.

## 8. Decisions for the owner

D-1 (page location) — the view lives in gtk-library (`ipod_page.rs`),
like every other route view; the shell owns a thin dispatch arm. The
connection service stays a gtk-library type.

D-2 (service ownership) — the `Shell` owns `IpodConnection` +
`IpodMountMonitor` (constructed in `shell/build.rs`); the page and later
consumers get facade clones. Production backend search is
`BackendSearch::FromEnv` (the shipped `IpodConnection::new()`).

D-3 (access point) — **not the sidebar**: the pages there lead to
different library sorts, not devices. The iPod page opens from a button
in the top bar, left-aligned next to the "this device" (playback output)
button, before the search bar — inside the header's start group, right
after `casting_host`. No sidebar item, no preferences entry, no active
state on the button. This changes spec R-01's "accessible in the
sidebar" — `deltas/b012_delta.md` records the modification.

D-4 (icon) — a new `rufin-ipod-symbolic` drawn in the existing symbolic
style, added to the gtk-widgets manifest and the verify_icons whitelist.

D-5 (non-Disconnected states) — B-012 renders a clearly-temporary stub
arm for the four other states; B-013 replaces it. The page never shows a
wrong state in between.

D-6 (gates) — no new env-gated gate: B-012 performs no device I/O; the
unit matrix of §5 plus the existing `ipod_connection_gate` cover it.

## 9. Documentation follow-ups (done with the task)

- tasks.md: B-012 checkoff + Result line at merge (per convention).
- docs/itunes-ipod.md §9: B-012 → done, queue starts at B-013, one line
  on the page/topbar seams (at the merge-hygiene commit).
- `deltas/b012_delta.md` (the R-01 MODIFIED) ships with this task and is
  merged into `spec.md` at feature completion, per SDD.
- plan.md Phase 3 stays as the phase reference — no edit needed for a
  single task.

## 10. Implementation steps

1. `deltas/b012_delta.md`: the R-01 access-point modification with its
   scenario.
2. rufin-core: `Route::Ipod` + the §5 route tests.
3. Icon SVG + gtk-widgets manifest + verify_icons entry.
4. gtk-library: `ipod_page.ui`, `ipod_page.rs` (view, `apply_snapshot`,
   subscription, `ipod_route_view`), manifest/INTERFACE_RESOURCE_PATHS,
   the §5 gtk-library tests.
5. gtk-shell: `Shell` fields, build.rs construction, the dispatch arm,
   the topbar button + field + bind, shell tests; fix any
   exhaustive-match fallout across the workspace.
6. `style.css` dot class; regenerate `locales/rufin.pot`.
7. Full battery (§5 last bullet) and fixes it forces.
8. Documentation (§9).

## 11. Implementation results

**Done 2026-11-17** on `feature/ipod-page-task-B-012`, per §10:

1. `deltas/b012_delta.md`: the R-01 access-point MODIFIED (topbar button,
   no sidebar) with its scenario.
2. rufin-core: `Route::Ipod` (serde/uniffi, not source-scoped) + 2 tests
   (serialization roundtrip, never source-scoped). No `SidebarRouteItem`
   variant — the sidebar machinery stayed untouched per D-3.
3. Icon: `rufin-ipod-symbolic.svg` (iPod silhouette with screen and click
   wheel, `currentColor`, no raster embeds) + gtk-widgets manifest entry +
   `verify_icons` whitelist entry.
4. gtk-library: `ipod_page.rs` — `IpodPageView` composite over
   `routes/ipod_page.ui` (stack: No-iPod view per S-01 with icon, heading,
   body copy, status dot + label; the temporary stub arm), the pure
   `decide()` carrying every semantic, `apply_snapshot`, the snapshot
   subscription (`glib::spawn_future_local`), `ipod_route_view()`.
   3 tests: the two `decide` matrices + the subscription roundtrip on a
   real worker thread with an unreachable backend.
5. gtk-shell: `Shell.ipod_monitor` (`IpodMountMonitor::start(IpodConnection::new())`
   in `build.rs` — the first-ever instantiation of the B-006 service in
   the app), the `Route::Ipod` dispatch arm (static widget, before the
   catalog path), the topbar `ipod` button (start group, after
   `casting_host`, before the search title widget) with the
   `bind()` → `navigate(Route::Ipod)` wiring. 1 new RouteStack test.
6. `gtk-preferences/resources/style.css`: `.ipod-status-dot` +
   `.ipod-status-disconnected`. `locales/rufin.pot` regenerated — the
   i18n-template check passes.

Battery: rufin-core 154 lib tests, gtk-widgets 56, gtk-library 115 +
3 env-gated connection gates, gtk-shell 54 — all green; clippy 0 on
rufin-core/gtk-widgets/gtk-library; `cargo check --workspace` clean
(includes the `rufin-android` uniffi typecheck); i18n check green.

**Gate-forced corrections:**

- **The widget-test claim was wrong.** No test in this repo constructs
  GTK widgets — `GtkWidget` class init asserts the main thread, so
  `IpodPageView::new()` in a unit test SIGABRTs. The plan's §2 claim
  ("gtk-widgets tests construct GtkLabel etc.") described module bodies,
  not tests. Fix: the page's state semantics moved into the pure
  `decide()` (page arm, label, dot class) and the gates test that; the
  widget application in `apply_snapshot` is the thin untested layer every
  other view in this app has, covered by manual acceptance. §2 and §5 are
  corrected above.
- **The topbar button is not widget-tested** for the same reason; its
  presence rides on the compiled-resource test and the click wiring rides
  on the tested `RouteStack` (the single manual-acceptance line).
- **Local-toolchain debt, not this task's:** the workspace carries
  pre-existing fmt drift (the local rustfmt is newer than CI's pinned
  one — `cargo fmt --all` rewrites a dozen untouched files, all reverted;
  my files are fmt-clean) and pre-existing clippy debt in
  `gtk-preferences/.../itunes.rs` (4× `into_owned`) and
  `sources/tests/itunes_*.rs` (skip-message `eprintln!`) — both from the
  earlier iTunes work, untouched here. Owner decision needed on whether
  to fix those in a dedicated hygiene branch.
