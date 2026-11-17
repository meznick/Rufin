# B-012: iPod page base layout

Status: proposal, awaiting owner review (decisions in §8; everything not
overridden there is implemented as written).

## 1. What this task is

B-012 is the first UI task: it makes the iPod page exist. Spec reference:
R-01 (Page Visibility), S-01 (No device connected). Plan reference: Phase 3,
first bullet ("Keep the iPod page visible when disconnected"). Task
verification: the page is always accessible in Rufin, including its No iPod
view when disconnected.

Scope is deliberately the skeleton: the sidebar entry, the route, the page
container, the connection-service wiring, and exactly one state view — the
"No iPod detected" view for `Disconnected`. The other four connection-state
views are B-013's contract (S-02..S-05); the device overview is B-014; the
sync controls and the `ProgressPipe` subscription are B-015/B-016. This
task builds the container those views land in and proves the plumbing they
ride on, headless and display-free where possible.

## 2. What already exists (measured, not guessed)

- The connection service is done and shipped (B-006), but **nothing in the
  app instantiates it yet** — `grep IpodConnection crates/` outside
  gtk-library returns nothing. `gtk_library::ipod_connection::IpodConnection`
  is a cheap cloneable facade: a detached worker thread owns discovery, the
  loaded backend, and the FSM; `snapshot()`, `subscribe()` (async-channel,
  no replay), `scan()`, `retry()`, interrupted-notice commands. Production
  constructor is `IpodConnection::new()` (`BackendSearch::FromEnv`).
  `gtk_library::ipod_monitor::IpodMountMonitor::start(connection)` takes
  ownership of the facade, attaches gio volume-monitor handlers with a
  300 ms debounced scan, runs the startup scan, and exposes
  `connection()` for clones.
- `ConnectionSnapshot { state, device, diagnostics, interrupted_operation }`
  with the five-state `ConnectionState` (`Disconnected`, `Connecting`,
  `BackendUnavailable`, `Ready`, `Error`) and per-state `ui_label()`s —
  the B-013 surface, not this task's rendering scope.
- Routing: `Route` (rufin-core `route.rs`, serde + uniffi under
  `bindings`), `SidebarRouteItem` (rufin-core `settings/sidebar.rs`, serde +
  uniffi), `sidebar_route_descriptor()` (stable id, title, icon name, root
  route). The shell renders routes in
  `gtk-shell/src/shell/route.rs` (`render_current_route_content_inner`),
  which special-cases `Route::Search` and falls through to the
  catalog-backed collection path; static non-catalog pages exist as
  precedent (`MountedRoute::static_widget` +
  `route_layout::route_empty_view`).
- The sidebar is built from `settings.sidebar.route_items`
  (`navigation.rs` `build_normal_navigation`/`build_compact_navigation`);
  `SidebarSettings::sanitize()` **re-adds missing items in default order**
  — persisted settings self-heal, no migration code needed. The
  preferences "starting pages" list iterates `SidebarRouteItem::all()`
  (gtk-preferences `preferences/mod.rs:911`), so the new item appears
  there automatically. Keyboard route cycling iterates visible items —
  generic. Rail buttons get their CSS class from
  `gtk_widgets::route::sidebar_route_css_class` (exhaustive match over the
  item enum).
- Icons: `resources/icons/hicolor/scalable/actions/*-symbolic.svg`,
  compiled through the gtk-widgets gresource manifest
  (`crates/gtk-widgets/resources/rufin.gresource.xml`, `include_icons`).
  `build_support::verify_symbolic_icons` forbids raster embeds in symbolic
  SVGs; `ui_resource::verify_icons` has a fixed whitelist of important
  paths checked at startup.
- UI pages: `gtk_widgets::composite_box!` templates in
  `crates/gtk-library/resources/routes/*.ui` (manifest +
  `INTERFACE_RESOURCE_PATHS` test). The `empty-state` / `section-heading` /
  `muted` classes already exist (`placeholder.ui`, global
  `gtk-preferences/resources/style.css`). Widget construction works in
  unit tests without a display (gtk-widgets tests construct `GtkLabel`
  etc. today); CI excludes the `gtk-*` crates from its test runs (they
  only typecheck there), so these unit tests run locally — they must stay
  display-free by construction.
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

1. `crates/rufin-core/src/route.rs` — `Route::Ipod` variant plus the
   `sidebar_route_descriptor` arm: stable id `"Ipod"`, title
   `msgid("iPod")`, icon `"rufin-ipod-symbolic"`, root `Route::Ipod`.
   Deliberately **not** added to `requires_selected_source()` /
   `uses_selected_source()` — the page must work with no configured
   library source (that is the "always accessible" half of R-01).
2. `crates/rufin-core/src/settings/sidebar.rs` —
   `SidebarRouteItem::Ipod`; `all()` grows to 14, the new item **last**
   (after `SmartPlaylists`); `default_visible()` true. Existing
   installations self-heal through `sanitize()` (measured in §2).
3. `crates/gtk-widgets/src/route.rs` —
   `sidebar_route_css_class(SidebarRouteItem::Ipod) => "nav-route-ipod"`
   (exhaustiveness; no in-repo CSS targets these classes, so no CSS file
   change).
4. New icon `resources/icons/hicolor/scalable/actions/rufin-ipod-symbolic.svg`
   — an iPod silhouette drawn in the existing symbolic style (fill
   `#241f31`, 16×16 viewBox, no raster embeds); manifest entry in
   `crates/gtk-widgets/resources/rufin.gresource.xml`; add the path to the
   `verify_icons` whitelist (nav icon = important).
5. `crates/gtk-library/src/ipod_page.rs` + `resources/routes/ipod_page.ui`
   + manifest entry + `INTERFACE_RESOURCE_PATHS` + `pub mod ipod_page;`
   in `lib.rs`.
6. Shell ownership: `crates/gtk-shell/src/shell/mod.rs` gains
   `ipod_monitor: IpodMountMonitor` (owns the facade; the page grabs
   `monitor.connection()` clones) — constructed in
   `crates/gtk-shell/src/shell/build.rs` with `IpodConnection::new()` +
   `IpodMountMonitor::start(...)` on the GTK main thread. The monitor
   lives for the app's lifetime; the worker thread exits when the last
   facade drops.
7. `crates/gtk-shell/src/shell/route.rs` — a `Route::Ipod` arm in
   `render_current_route_content_inner`, right after the `Search` arm and
   before the catalog path:
   `MountedRoute::static_widget(gtk_library::ipod_page::ipod_route_view(&shell.ipod_monitor.connection()))`.
   The page therefore renders even at startup with no source configured
   and survives source switches (no catalog to rebuild).
8. Styling: reuse `empty-state`/`section-heading`/`muted`; add one small
   class (e.g. `.ipod-status-dot` + state modifiers) to
   `crates/gtk-preferences/resources/style.css` for the indicator dot.
9. `locales/rufin.pot` regeneration (`cargo run -p xtask -- generate
   i18n-template`) so the i18n check stays green.

Known non-goals (explicit, not forgotten): the topbar search entry keeps
its default behaviour on the iPod page; the interrupted-operation notice
surface and the retry button are B-013; no dry-run/progress UI.

## 5. Verification: gates (unit tests, display-free)

- rufin-core (`settings/sidebar.rs`, `route.rs` tests):
  `SidebarRouteItem::all()` stays order-stable (first `Home`, last `Ipod`,
  14 items); the descriptor's stable id serializes/deserializes
  (`"Ipod"`), its root route is `Route::Ipod`; `sanitize()` adds `Ipod`
  (visible) to a stored list missing it, at the default position;
  `Route::Ipod` is not source-scoped.
- gtk-widgets: the `nav-route-ipod` css-class arm (extend the existing
  route-class test if present, else a small one).
- gtk-library (`ipod_page.rs` tests):
  - constructing the view and applying a `Disconnected` snapshot shows
    the No-iPod arm (visible) and hides the stub; the heading is
    "No iPod detected" and the indicator shows "Disconnected";
  - each of `Connecting` / `BackendUnavailable` / `Ready` / `Error`
    snapshots shows the stub arm, never the No-iPod arm, never a panic —
    the B-013 takeover point;
  - the subscription roundtrip: `IpodConnection::with_backend_search`
    with an unreachable `Explicit` dir (no device I/O, worker thread
    real), `subscribe()`, apply the initial snapshot, then
    `set_interrupted_operation(...)` → the page's applied snapshot
    carries the notice — proving the channel plumbing the later views
    ride on, without a display;
  - the resource test picks up `ipod_page.ui` via
    `INTERFACE_RESOURCE_PATHS` (automatic once registered).
- gtk-shell: `route_uses_selected_source(Route::Ipod) == false`
  (extends the route.rs tests); the navigation tests' generic descriptor
  loops cover the new item automatically — extend the position test only
  if the sidebar order test needs the new tail.
- Full battery at the end: `cargo test` (workspace minus the env-gated
  suites), clippy 0, fmt, `cargo check --workspace` (includes
  `rufin-android` — the uniffi enums gain variants; the Kotlin side is
  generated at gradle time, not checked by cargo), and the i18n-template
  check.

## 6. Scope boundaries (not this task)

B-013 (five-state views + retry + interrupted-notice surface + the B-006
review debts), B-014 (device overview), B-015 (sync execution UI),
B-016 (progress/results subscription), B-017 hardware acceptance. The
page must not grow their UI now — the stack and the subscription are the
seams, nothing more.

## 7. Risks

- Exhaustive matches over `Route` / `SidebarRouteItem` elsewhere (topbar,
  route keyboard, events) will fail the workspace check; the fallout is
  mechanical match-arm extensions, listed in §4 implicitly.
- The android crate must still typecheck (uniffi enum extension); if the
  generated Kotlin is checked in anywhere, it stays out of cargo's sight —
  flag at implementation if gradle owns it.
- The subscription tests spawn the real worker thread; they must not
  depend on timing beyond the channel delivery (async-channel recv with a
  generous timeout, per the existing connection tests' style).

## 8. Decisions for the owner

D-1 (page location) — the view lives in gtk-library (`ipod_page.rs`),
like every other route view; the shell owns a thin dispatch arm. The
connection service stays a gtk-library type.

D-2 (service ownership) — the `Shell` owns `IpodConnection` +
`IpodMountMonitor` (constructed in `shell/build.rs`); the page and later
consumers get facade clones. Production backend search is
`BackendSearch::FromEnv` (the shipped `IpodConnection::new()`).

D-3 (sidebar position) — `Ipod` appended last in `all()` (after
SmartPlaylists), visible by default; existing configs self-heal through
`sanitize()`. (Alternative rejected silently: a separate device section —
more shell surgery for one item.)

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
  on the page/seams (at the merge-hygiene commit).
- plan.md Phase 3 stays as the phase reference — no edit needed for a
  single task.

## 10. Implementation steps

1. rufin-core: `Route::Ipod`, `SidebarRouteItem::Ipod`, descriptors,
   order + sanitize tests (§5 first block).
2. Icon SVG + gtk-widgets manifest + css-class arm + verify_icons entry.
3. gtk-library: `ipod_page.ui`, `ipod_page.rs` (view, `apply_snapshot`,
   subscription, `ipod_route_view`), manifest/INTERFACE_RESOURCE_PATHS,
   the §5 gtk-library tests.
4. gtk-shell: `Shell` fields, build.rs construction, the dispatch arm,
   shell tests; fix any exhaustive-match fallout across the workspace.
5. `style.css` dot class; regenerate `locales/rufin.pot`.
6. Full battery (§5 last bullet) and fixes it forces.
7. Documentation (§9).

## 11. Implementation results

(filled on completion — date, what each step produced, gate outcomes,
gate-forced fixes)
