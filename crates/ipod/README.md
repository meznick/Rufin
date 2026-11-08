# ipod

Safe, runtime-loaded Rust wrapper around **libgpod 0.8.3** — the C library that
reads and writes the iPod `iTunesDB` / `ArtworkDB` databases for the Video
5/5.5g and Classic generations. Implements R-06 / W-1..W-7 of
[`deltas/b007_delta.md`](../../deltas/b007_delta.md); the research evidence and
the pinned build recipe live in the itl-player repo (`docs/LIBGPOD.md`,
`NOTES.md` §20, `tools/libgpod_check.c`).

The full API and safety docs are in the rustdoc (`src/lib.rs`). This file is
the short version plus the operational notes.

## Why a wrapper

Three verified libgpod properties force one (B-007 §1): `itdb_write_file`
side effects are not confined by the output path; parse success never implies
artwork success (an unknown model silently skips artwork); and device dbids
are not a stable cross-sync identity. The wrapper makes all three explicit:
reads are provably non-mutating on well-formed trees, artwork carries an
explicit verdict with an independent ArtworkDB count and an ithmb
proof-read, and the write surface is defined but gated.

## Module map

| File | Role |
|---|---|
| `backend.rs` | `IpodBackend::load`, search order, SONAME 4, ABI marker policy (W-1/W-6) |
| `session.rs` | `DeviceSession`: open/drop, owned read surface (W-2/W-3/W-4) |
| `ffi/symbols.rs` | typed `extern "C"` declarations + symbol tables + GLib helpers |
| `ffi/layouts.rs` | bindgen-generated struct mirrors of the pinned headers |
| `ffi/layouts_sha.rs` | SHA-256 of `layouts.rs` — the ABI marker tie-in |
| `artwork.rs` | own ArtworkDB (mhii) parser + RGB565 decoder + proof-read |
| `treehash.rs` | content+structure tree hash (W-5) |
| `write.rs` | gated write APIs (W-7) |
| `testkit.rs` | fixture builder (`itdb_init_ipod`), `sync-writes` only |

## Loading and the ABI marker

`RUFIN_LIBGPOD_DIR` (or the loader paths) → `libgpod.so.4` → resolve the
read-side symbols (a wrong library fails here) → `dladdr(itdb_parse)` for the
real path → look for `rufin-libgpod.abi` next to it:

```text
VERSION=0.8.3
LAYOUTS=sha256:cd93ed97aa1ab7cda96a13d94b9f0df45ced5ab5d6f60d6792b0a8a27bad4816
```

Marker present and mismatched → `BackendUnavailable` (hard). Absent (distro
build) → SONAME 4 trusted with a warning (soft policy; strictness is the
packaging follow-up, delta §5). Write symbols are resolved eagerly but their
absence only fails writes.

## The write gate

`write.rs` defines `create_track`, `copy_track`, `update_track`,
`remove_track`, `update_playlists`, `set_artwork`, `write_library` — all
refuse at runtime unless the crate is compiled with `--features sync-writes`.
No sync path may enable that feature before B-010.

## Tests

```text
# unit + integration against the pinned install (env-gated skips otherwise)
RUFIN_LIBGPOD_DIR=/tmp/opencode/libgpod-install/lib \
IPOD_TEST_TREE=/tmp/opencode/libgpod-check/fakepod \
  cargo test -p ipod --features sync-writes
```

- Unit (40): marker parser + ABI policy table (matching/missing/mismatch/
  unreadable), loader refusals (missing/garbage/wrong library), `with_native`
  serialization (8 threads against an overlap/nesting assertion), synthetic +
  real ArtworkDB parsing, RGB565 decoder, tree hash, verdict table, feature
  gate.
- Integration (9): structured parse failure; fresh fixture reads; empty-SysInfo
  model gate; full create/copy/artwork/write/reparse round-trip with the
  independent artwork verdict `Ok`; read non-mutation via tree hash; ArtworkDB
  removed and corrupt (both `Missing` while tracks still parse); removed
  `.ithmb` banks (proof-read forces `Missing`); concurrent sessions across 4
  threads sharing one backend; the restored-device snapshot (3373 tracks /
  9 playlists / ArtworkDB 3356 entries, verdict `Ok` with 3356 parsed
  thumbnails when `SysInfo` is seeded).

## Facts discovered during implementation (recorded in delta §8)

`itdb_device_get_ipod_info` never returns null for a valid device — an
unknown model is the INVALID/UNKNOWN table entry, so the W-3 gate checks the
model *value*; libgpod writes ArtworkDB bank names as UTF-16LE (enc byte 2);
`itdb_track_add` assigns the unique dbid; `itdb_init_ipod` seeds `SysInfo`
with `ModelNumStr`; the real ArtworkDB has 3356 image-list entries and 0
album-list entries; libgpod exports no version symbol (hence the marker).
