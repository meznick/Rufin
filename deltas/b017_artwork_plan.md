# B-017 A3: the production artwork provider

Status: plan (2026-11-21), owner-approved direction — the follow-up
named in `b017_plan.md` §8 after the step-0 finding that a `None`
provider drops planned add-covers.

## 1. What and why

A3 (artwork lands for added tracks) is the last open piece of the write
path. The executor's artwork loop (`sync/exec.rs`, `apply_device_ops`)
calls `ctx.artwork(add.pid)` for every planned `ArtworkAdd`; the plan
carries only pids, so the boundary must resolve pid → host file →
embedded cover bytes → `ipod::ArtworkInput`. Today
`ShellSyncBoundary.artwork` returns `None` unconditionally — every add
lands cover-less and a second dry run re-surfaces the covers.

The engine side is already proven: the pair gate's
`embedded_cover_provider` (`ipod-sync/tests/b010_sync.rs`) does exactly
this shape against `HostInputs` with lofty, and the step-0 run's gate
asserts the post-sync dry run empties. The production boundary lacks
only its own pid → path map.

## 2. Design

- `ShellSyncBoundary` gains a lazy `OnceLock<Option<HashMap<u64,
  PathBuf>>>`: pid → local path of every located track in the
  configured `.itl`.
- A pure helper `resolve_host_paths(library, media_root)` builds it:
  `itl_core::ItlFile::open` → `itl_sync::paths::MediaMapping::new(&itl,
  &media_root)` → `itl_sync::survey::unique_tracks(&itl)` →
  `mapping.to_local(track)`, inserting `track.persistent_id()` → path
  for every `MappedLocation::Mapped { path, .. }`.
  This is the same media-folder machinery the sources crate's iTunes
  catalog uses (guest UNC/URI → local path, URI form wins); the sync's
  `BoundarySource` already carries `library` + `media_root`, so the
  resolution reads exactly the file the source reads — a disposable
  copy during fixture runs, the live `.itl` otherwise (read-only,
  never written by this path).
- `artwork(pid)`: `source()` → `get_or_init` the map → path →
  `sources::inspect_embedded_cover(path)` →
  `ArtworkInput { encoded: bytes, rotation }` (rotation passthrough —
  embedded covers carry 0, but the field is honest).
- Lazy because the `.itl` parse (decrypt + inflate of ~1 MB) must not
  run at shell build time. The first call lands on the sync thread
  (the executor calls it per add); `OnceLock` makes it `Sync` and
  one-time.
- Dependencies: `gtk-shell` adds the registry crates `itl-core 0.1.3`
  and `itl-sync 0.1.0` — both already in the lock via `sources`, no
  version change.

## 3. Failure semantics

- `None` from `artwork` = the executor's existing "artwork unavailable
  at the provider" diagnostic + notice; the run continues. Artwork is
  never a failure of the run (engine contract, unchanged).
- Map-resolution failures (no or several sources — the service already
  refuses those at prepare; an unopenable `.itl`; no media mapping)
  make the map `None`: every pid misses, diagnostics surface, nothing
  partial is written.

## 4. Test

- Env-gated unit test in `ipod_sync.rs` (`#[cfg(test)]`):
  `ITL_TEST_PATH`, defaulting to the full production snapshot at
  `/nvme2tb/iTunesACCEPT/`, skipped when absent (the sources crate's
  pattern). Asserts `resolve_host_paths` is non-empty and every mapped
  path exists on disk — the casefix guarantee (all 3377 located tracks
  resolve) becomes the A3 pin.
- Existing gates stay green: `cargo test -p gtk-shell`,
  `-p gtk-library`, `-p ipod-sync`.

## 5. Verification

1. `cargo build -p rufin -F gtk-shell/sync-writes` (the feature build —
   the only build that can write, and the one the owner runs).
2. Clippy on `gtk-shell`, rustfmt.
3. The unit test above against the snapshot.
4. Fixture run (owner's click, step-0 flow): loop-mounted image +
   disposable `.itl` copy. The review must show the planned covers;
   the completed run must report artwork added and the post-sync dry
   run must be empty (the pair-gate invariant, engine-side).
5. Real pod: no writes beyond the accepted gates — the provider changes
   nothing about the gate policy. The remaining A3 evidence is
   collected with the rest of phase A.

## 6. Out of scope (follow-ups, in order)

- The build-flag footgun guard (build-script assertion that release
  builds carry `sync-writes`, or an iPod-page "writes compiled out"
  notice) — rides with the D-3 flip.
- Phase B in the VM (owner-driven), then D-3 (`sync-writes`
  default-on).
- B-016 deferred polish: Cancel wiring, real covers in the diff rows,
  step-name translation, pulse for indeterminate phases.

## 7. State note

The implementation was drafted before this plan was written
(`gtk-shell/Cargo.toml` deps + `ipod_sync.rs` changes); this document
fixed the design and the verification before the build and commit
landed. Landed with the plan itself: clippy clean, the env-gated test
green against the snapshot, `gtk-library` lib tests green, the
feature-enabled binary rebuilt (17:46, `testkit` symbols present).
