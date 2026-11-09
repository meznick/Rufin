# ipod-sync

Mounted-device discovery for Rufin's iPod integration (B-003). Finds candidate
volumes — mount points whose root contains an `iPod_Control/` directory — and
reports them together with the availability of the libgpod backend (the
[`ipod`](../ipod/README.md) wrapper crate). Implements B-003 of
[`deltas/b003_delta.md`](../../deltas/b003_delta.md); the FSM glue that
consumes the reports lives in `gtk-library` (`ipod_discovery`).

## Why detection stops at the filesystem

Wrapper research (`deltas/b007_delta.md` §1.1) shows libgpod parse paths can
mutate live device trees — a missing `Artwork/` gets `g_mkdir`-ed. Discovery
therefore never parses a database and never opens a session: one
non-recursive stat per mount is the whole probe. Zero writes are guaranteed
structurally (mount-table read + stat + backend `dlopen`), and proven by a
tree-hash fixture (`fixture_scan_never_writes_to_candidate_tree`).

## The report

`Discovery::scan(&dyn MountSource)` returns a `ScanReport`:

- `backend` — `Loaded { abi_verified }` (the wrapper's ABI-marker verdict;
  `false` = distro build soft-accepted, consumers must surface it) or
  `Unavailable { reason }`. Loads are retried on every scan, so a backend can
  only transition unavailable → loaded at runtime.
- `candidates` — confirmed `iPod_Control/` mounts, sorted by mount point.
- `blocked` — mounts whose probe was denied: reported, never silently
  dropped, never treated as iPods.
- `appeared` / `removed` — the diff against the previous scan, so unmounts
  are observable.

Discovery keeps working without the backend: candidates are found regardless,
and there is no fallback to file copying anywhere.

## The FSM glue

`gtk-library::ipod_discovery::apply_scan` feeds one report into B-002's
`DeviceManager`: candidate + backend → `Connecting`; candidate + unavailable
backend → `BackendUnavailable` ("Sync unavailable") with the reason as the
diagnostic; removal → `Disconnected` + cleared device. The glue never calls
`set_ready_after_preflight` and the tests assert the state never reaches
`Ready` — readiness stays B-004's gate.

Multiple simultaneous devices follow the deliberate **first-wins** policy:
the report lists every candidate sorted by mount point, the FSM follows the
first. Removing the current device hands over to the next candidate;
removing a non-current one is invisible to the FSM; plugging in a device
that sorts before the current one takes the FSM over (first wins, not first
seen). All four behaviors are tested.

## Tests

```bash
# unit + integration over synthetic mount tables (no backend needed)
cargo test -p ipod-sync

# plus the env-gated gates: pinned install, restored-device snapshot, and a
# mounted device image (mount p2 of ipod/test-device-30g.img yourself; the
# test only consumes the path)
RUFIN_LIBGPOD_DIR=/tmp/opencode/libgpod-install/lib \
IPOD_TEST_TREE=/tmp/opencode/libgpod-check/fakepod \
IPOD_TEST_MOUNT=/mnt/ipod \
  cargo test -p ipod-sync
```

The synthetic tests feed a `FakeMounts` source and never touch the host's
mounts or a real device. The two real-tree gates prove detection and the
no-write guarantee against the actual device data: the restored snapshot
whole-tree-hashed, and the mounted image (`IPOD_TEST_MOUNT`) through the
real `/proc/self/mountinfo` source with the whole `iPod_Control/` subtree
hashed — on the Video layout that includes the media library (23G on the
30g image), so the gate is deliberately sized for `test-device-30g.img`,
not the 240G one. The only production mount source reads
`/proc/self/mountinfo` (Linux-only, matching the wrapper); its parser
decodes the kernel's octal escapes and follows a skip-malformed-lines
tolerance policy — malformed lines (including overflowing escapes like
`\400`) are dropped with a debug trace, never a panic, and a mount point
literally named `-` still parses as the mount point.
