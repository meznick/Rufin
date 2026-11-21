# B-016 delta: the sync sheet (progress + review UI)

## What shipped

The review and result `adw::AlertDialog`s are gone. Two rounds of HTML
mockups (the owner's design loop with an external HTML/CSS model) settled
the shape; the second round deviated from the original spec and the
owner adopted the deviations (2026-11-21).

**The inline review card** (both builds — release included, owner
decision 2026-11-21): a card in the page's scroll flow under the Options
box, always visible whenever the device is ready. The page auto-runs a
dry-run preview on entry, on every manual scan, and after each run
(`ipod_route_view` triggers `sync.start(dry_run=true)`; the machine's
`Start` is allowed from `Idle`/`Done`/`Review`, so the preview is never
a gate). The card shows the summary line (counts + capacity verdict,
error-colored when it does not fit) and the name-resolved diff sections:
adds (title · artist · album, size, transcode chip), stat updates (one
old→new chip per field), playlists (delta chip + nested +/− member
names), creates, removes, uncapped findings. A failed preview lands in
`Done(failed)` — shown once; the page does not auto-retry it (an error
panel that reappears forever with no source configured is worse than
silence).

**The Sync click is the approval** (owner decision 2021-11-21, replacing
the S-08 review gate): the diff is already on the page, so there is no
Apply step. `ReviewPolicy` is deleted; a prepared real run goes straight
to `Running`. The Dry Run checkbox is gone — the live preview replaced
its purpose.

**The floating run/result panel**: a full-page-width overlay anchored to
the page bottom (spans the device sidebar + content column, 35px
margins, top-rounded corners, upward shadow), shown during `Running` and
`Done`. Running: no heading (it collided with the bar in the mockup) —
just the discreet action line (the engine's own item labels, e.g.
"Adding I Fink U Freeky"), the accent progress bar with the per-step
fraction, and the "Step x of y — name" / "n / total" labels, driven by
the monotone `SyncProgress` the service folds from every engine event
(reset per run). Result: completed/failed heading (error class), the
changes summary line, on failure the first diagnostic in error color,
the resume hint **only when the engine reports pending recovery**, the
remaining diagnostics in a collapsed Details expander, and Close.

## Design decisions

- Review is a live surface, not a gate: the machine's `Review` phase
  only ever holds a preview; there is no approval event, no pending
  stash, no policy parameter.
- Names, not ids: `PreparedSync` gained `device_tracks`, and the service
  resolves every diff row against the host catalog and the device parse
  (`ReviewDetails`). Findings are uncapped.
- The resume hint is honest: `SyncOutcomeSummary` carries the engine's
  `RecoveryState`, so a preflight refusal (no recovery) shows no hint.
- The floating panel overlays the page (no reflow); the inline card
  stays visible behind it rather than collapsing the layout.
- The mockup's custom iPod SVG is a prototype artifact — the app keeps
  its `rufin-ipod-symbolic` icon.

## Deferred (in order)

- The Cancel button — the engine's `cancelled` hook is wired but the
  service never sends it. The cancelled run is a resumable journal
  state, so the wire-up is small.
- Real covers in diff rows (the sheet shows initials tiles).
- Translating the engine's step names ("FileOps" renders raw).
- Pulse animation for indeterminate phases (currently a flat bar).

The mockup spec lives at `~/.opencode/plan/ipod-sync-panel-ui.md`
(Kate's local plan dir), not in this repo — the design tokens and sample
data are there for the next mockup round.
