# B-016 delta: the sync sheet (progress + review UI)

## What shipped (2026-11-20)

The review and result `adw::AlertDialog`s are gone. The sync flow is now
one **morphing sheet**: a card pinned between the ready page's content
scroll area and the bottom bar (`.ipod-sync-sheet`), revealed with a
slide-down when a session starts, hidden when idle. The page never
reflows — the sheet takes height from the scroll area.

States, in one component:

- **Review (dev builds only)**: heading + summary line (counts + the
  capacity verdict, error-colored when it does not fit). The body holds
  grouped expandable sections built from `ReviewDetails`: adds (title ·
  artist · album, size, transcode chip), stat updates (one old→new chip
  per field), playlists (delta chip + nested +/− member names), creates,
  removes, and the uncapped findings. Footer: Cancel + Apply. A dry run
  (the S-08 deliverable) and a nothing-to-do review offer Close only.
- **Running (identical in dev and release)**: the diff body is replaced
  by the action line (the engine's current item label), the accent
  progress bar (per-step fraction; flat while indeterminate) and the
  "Step x of y — name" / "n / total" labels. Driven by the monotone
  `SyncProgress` the service folds from every engine event (reset on
  each `Start`). No buttons.
- **Result**: completed/failed heading (error class on failure), the
  changes summary line, the first diagnostic in error color, the resume
  hint **only when the engine reports pending recovery**, the remaining
  diagnostics in a Details expander, Close.

## Design decisions

- One component, never two: the first HTML mockup round split review
  (inline) from running/result (floating sheet); the owner settled on a
  single morphing sheet — the review was below the fold in the inline
  variant and the component swap reflowed the page.
- Release builds never show the diff: the policy machine already routes
  release clicks straight to Running; the sheet renders it identically
  to dev. The review surface is dev-only.
- Names, not ids: `PreparedSync` gained `device_tracks`, and the service
  resolves every diff row against the host catalog and the device parse
  (`ReviewDetails`). Findings are uncapped — the sheet has room.
- The resume hint is honest: `SyncOutcomeSummary` carries the engine's
  `RecoveryState`, so a preflight refusal (no recovery) shows no hint.

## Deferred (in order)

- The Cancel button — the engine's `cancelled` hook is wired but the
  service never sends it (B-016's original scope). The cancelled run is
  a resumable journal state, so the wire-up is small.
- Real covers in diff rows (the sheet shows initials tiles).
- Translating the engine's step names ("FileOps" renders raw).
- Pulse animation for indeterminate phases (currently a flat bar).

The mockup spec for the HTML round lives at
`~/.opencode/plan/ipod-sync-panel-ui.md` (Kate's local plan dir), not in
this repo — the design tokens and sample data are there for the next
mockup round.
