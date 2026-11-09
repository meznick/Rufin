# Delta Spec: B-004 (device readiness preflight)

## Context

B-004 is the single path from `Connecting` to `Ready`: non-mutating
readiness checks fed through B-002's
`DeviceManager::set_ready_after_preflight` callback. It consumes the B-005
retrieval (`DeviceFacts` in the manager's `DeviceInfo`), the B-003 discovery
report (backend ABI verdict), and the B-007 wrapper (`DeviceSession`,
`ModelInfo`, `ArtworkStatus`). The glue never marks Ready itself — B-004
wires the real callback.

Depends on B-005 (merged, `33d4039` + checkoff `6953c3d`). B-006
(observer/UI binding) builds on top. `s03_b004.md` already corrected the
spec wording (no Trust step; retry → Connecting → rerun preflight; removal
from any state → Disconnected).

## R2 gate: artwork errors and apparent success, revisited for readiness

B-007 closed R2 for the *wrapper surface* (explicit artwork verdicts, §1.2
of `deltas/b007_delta.md`). This gate decides what those verdicts and the
other device facts mean for *readiness*. Evidence, all against the pinned
libgpod 0.8.3 source (`/tmp/opencode/libgpod-src/libgpod-0.8.3/src/`):

### 2.1 Checksum prerequisites (which models need the FireWire GUID)

1. `itdb_device.c:1907` `itdb_device_get_checksum_type`: with
   `sysinfo_extended` present, the type comes from the parsed `db_version`
   (0/1/2 → NONE, 3 → HASH58, 4 → HASH72, 5 → HASHAB). Without it, a
   generation switch: CLASSIC_1/2/3 + NANO_3/4 → HASH58 (lines 1939–1944),
   NANO_5/Touch 1–3/iPhone 1–3 → HASH72, and VIDEO_1/VIDEO_2 → **NONE**
   (lines 1975–1976).
2. `itdb_device.c:1989` `itdb_device_write_checksum` dispatches HASH58 to
   `itdb_hash58_write_hash`, and `itdb_hash58.c:249` calls
   `itdb_device_get_hex_uuid(device, firewire_id)`; failure is a hard error:
   **"Couldn't find the iPod firewire ID"** (line 250). HASH58 checksums
   require the FireWire GUID.
3. `itdb_device.c:1846` `itdb_device_get_firewire_id` reads
   `sysinfo_extended` properties or the plain sysinfo hash entry
   `FirewireGuid`; `itdb_device.c:993–995` shows `itdb_device_read_sysinfo`
   merging `FireWireGUID` from `SysInfoExtended` into that hash entry at
   parse time — exactly the value the wrapper's `DeviceInfo::firewire_id()`
   returns (added in B-005).

**Conclusion:** Classic 1/2/3 require a present FireWire GUID before any
write that regenerates the database checksum; Video 1/2 do not. B-004
verifies this *prerequisite*; libgpod owns the generation itself (B-008+
write path). The generation-based rule mirrors libgpod's own fallback
branch and stays correct when `SysInfoExtended` exists: a real Classic's
`db_version` ≥ 3 → HASH58 → GUID required anyway.

### 2.2 Artwork verdict → readiness

Wrapper `classify()` (`crates/ipod/src/artwork.rs:451`): model unknown →
`SkippedUnknownModel`; `declared == None` → `Missing` ("ArtworkDB missing
or unreadable"); `declared == 0 && parsed == 0` → `Ok` (legitimately
artless); `parsed < declared` → `Missing`; failed proof-read → `Missing`.

**Conclusion (owner decision):** `Missing` **fails readiness** — declared
entries not parsed, a missing/corrupt ArtworkDB, or a failed proof-read is
corruption, and the write path would compound it. `Ok` passes, including a
legitimately artless device. `SkippedUnknownModel` is a defensive Model
failure (unreachable after the model gate, but never silently accepted).
B-010's R2 revisit may relax `Missing` later; B-004 is strict.

### 2.3 Unknown-model signal

`itdb_device.c:1206` `itdb_device_get_ipod_info`: when `sysinfo_extended`
exists it prefers `itdb_ipod_info_from_serial` (last 3 chars of the serial
against `serial_to_model_mapping`, `itdb_device.c:633`, hash built in
`init_ipod_model_table` at 2064); otherwise `ModelNumStr` through
`get_ipod_info_from_model_number` (leading alpha char skipped,
`itdb_device.c:2116–2118`); otherwise the INVALID/UNKNOWN table entries.
The wrapper maps INVALID/UNKNOWN to `DeviceInfo.model = None`
(`crates/ipod/src/session.rs` `device_info_inner`), and B-005 maps that to
empty model/generation strings. B-004 turns `None` into a Model failure —
the empty-`SysInfo` case (which is what the real device snapshot has) is
never a silent artless parse.

### 2.4 Read-only mount detection

`statvfs` `f_flag & ST_RDONLY` (libc v0.2.189 in the workspace; `ST_RDONLY
= 1`). Live check 2026-11-09 against the ro-mounted 2Kn loop image
(`ipod/test-device-30g.img` p2, `losetup -b 2048`, `mount -o ro`):
`f_flag = 0x1001`, bit 0 set. A ro mount can never accept a sync write, so
it fails readiness. No mount-options parsing needed.

### 2.5 Fixture model numbers for the Classic tests

Model table (`itdb_device.c:48`, `ipod_info_table`): `B029`/`B147` (80 GB,
Classic 1), `B145`/`B150` (160 GB, Classic 1), `B562`/`B565` (120 GB,
Classic 2), `C293`/`C297` (160 GB, Classic 3) — lines 207–218.
Serial suffixes mapping to Classic (`serial_to_model_mapping`, lines
775–782): `Y5N → B029`, `YMV → B147`, `YMU → B145`, `YMX → B150`,
`2C5 → B562`, `2C7 → B565`, `9ZS → C293`, `9ZU → C297`.

**Fixture consequence:** `seed_sysinfo(mount, "MB029")` resolves to Classic
1 Silver (leading `M` skipped). But `testkit::seed_sysinfo_extended`
hardcodes serial `YM999999V9K` → `V9K` → A444 (Video 2G), and the serial
path wins over `ModelNumStr` — so the Classic-with-FireWire fixture writes
its own `SysInfoExtended` plist with a Classic serial (e.g.
`YM6312345Y5N` → `Y5N` → B029) plus `FireWireGUID`. The minimal plist has
no `DBVersion` key, so the extended-path `db_version` is 0/absent →
CHECKSUM_NONE — irrelevant for B-004, which checks the generation-based
prerequisite, not the actual checksum type.

## Design decisions (owner-approved, 2026-11-09)

1. **Preflight opens its own `DeviceSession`** (second `itdb_parse` per
   device appearance, after B-005's retrieval parse). `artwork_status()`
   and the raw model enums are not in `DeviceFacts`; folding them into
   retrieval would blur its display-facts boundary. Cost on the 3373-track
   snapshot is seconds.
2. **abi_verified=false lands in `BackendUnavailable`.** B-002 defines that
   state for "missing, disabled, or incompatible backend"; a soft-accepted
   distro build is incompatible. The glue consumes the verdict itself (it
   owns the report): retrieval still runs so the device info is visible,
   then BackendUnavailable with the ABI diagnostic — preflight is never
   called.
3. **Artwork `Missing` fails readiness** (§2.2).
4. **Supported set = Video 1/2 + Classic 1/2/3** (generations 16, 17, 18,
   19, 26 — the B-001 scope). Everything else → "unsupported model" error.
5. **No FSM-level retry helper in B-004.** Retry = replug (removal →
   reappearance → fresh retrieval + preflight) or backend recovery
   (Unavailable → Loaded on a later scan). The S-05 "retry option" UI
   button is B-006's job.
6. **Read-only mount fails readiness** via `statvfs` (§2.4).
7. **Capacity is not re-checked by preflight.** B-002's
   `set_ready_after_preflight` validates present/valid/`used <= total`
   after the callback; the callback returns Ok. Preflight's own `statvfs`
   serves only the read-only flag.
8. **Check order** (first failure wins): session open → model resolved →
   supported set → Classic checksum prerequisite → artwork verdict →
   read-only flag.
9. **Error mapping:** ipod-sync preflight returns its own `ReadinessError`
   (ipod-sync cannot depend on gtk-library's `PreflightError`);
   `BackendAbi` → `PreflightError::BackendUnavailable`, `Check` →
   `PreflightError::Readiness`.
10. **`device_manager.rs` is untouched.** The B-002 contract
    (`permits_preflight` = Connecting, capacity validation, `set_state(Ready)`
    no-op) already covers everything.

## Wording alignment (this delta's commits)

- `plan.md`'s B-004 bullet already requires the preflight to consume
  `abi_verified` (amended in the B-005 review-fix commit). No change.
- `crates/gtk-library/src/ipod_discovery.rs` module docs: the "Two
  deliberate simplifications" paragraph (abi_verified ignored) is rewritten
  during implementation — the glue now consumes the verdict.
- `tasks.md` checkoff stays the owner's commit, as B-005.

## Verification sketch (implementation phase; plan:
`~/.opencode/plan/b004-preflight-implementation.md`)

tasks.md bullet → fixture:

| tasks.md coverage | Where |
|---|---|
| missing Classic FireWire ID | unit `evaluate`: Classic 1/2/3 + None → `ChecksumPrerequisite`; integration: `MB029`-seeded temp copy without SysInfoExtended |
| Video without that requirement | unit: Video + None → Ok; snapshot gate (A446) → Ok |
| missing firmware | glue: `firmware: None` facts + Ok preflight → Ready |
| unsupported models | unit: NANO_1/SHUFFLE_3/TOUCH_1/UNKNOWN → Model error; unknown model (empty SysInfo) → Model error |
| read-only mounts | unit `read_only=true` → ReadOnly; mounted gate: ro loop → `ReadOnly` error |
| corruption | temp copy: corrupt iTunesDB → Database error; corrupt ArtworkDB → Artwork error |
| unavailable backend | glue: `Unavailable` → BackendUnavailable, preflight never called; `abi_verified=false` → BackendUnavailable |
| retries | glue: replug (Error → removal → reappearance → fresh preflight → Ready) and backend recovery (Unavailable → Loaded scan → Ready) |
| removal | glue: removal from Ready → Disconnected + cleared |
| backend/access/capacity/parse | session open failure → Database error (access/corruption detail); capacity validated by B-002 after the callback; no-write tree-hash proof on every gate |

Real-model write acceptance is B-017, not claimed here.

## Results

(to be filled after implementation: what was built, what the tests pinned,
review findings and their fixes.)
