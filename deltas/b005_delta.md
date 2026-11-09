# Delta Spec: B-005 (DeviceInfo retrieval)

## Context

B-005 fills the `DeviceInfo` the connection FSM already carries: name, model,
generation, model number, firmware, capacities, FireWire ID. It consumes the
B-003 discovery (`Discovery::backend()`) and the B-007 wrapper (`ipod`
crate), and feeds `DeviceManager::set_device` — replacing the placeholder
empty name/model/generation the B-003 glue currently installs.

Depends on B-003 (merged). B-004 (preflight) and B-006 (observer/UI binding)
build on top.

## R3 gate: device-name provenance (owner-reviewed, 2026-11-09)

The gate question: where does the display name actually come from?
`tasks.md` claimed metadata under `iPod_Control/Device/`; `plan.md` claimed
master-playlist name with a volume-name fallback. Evidence:

1. **libgpod has no volume-label code.** A source-wide search of
   `libgpod-0.8.3/src` for label/volname/volume-name reading finds nothing.
   The only "device name" libgpod knows is the master playlist's name:
   `itdb_init_ipod()` writes the `ipod_name` argument into the MPL and its
   doc comment says "the name to give to the iPod. Will be displayed in
   gtkpod or itunes". The `mhbd` header struct has no name field.
2. **`Device/SysInfo` has no name key, and the real one is empty.** The
   SysInfo parser stores any `Key: Value` line generically, but libgpod
   itself only ever queries `ModelNumStr` (grep over the sources: the only
   key literal queried). The snapshot's real `SysInfo` is 0 bytes — the
   `MA446` seed in the fakepod fixture is a research aid, not device truth.
3. **The volume label is a derived, lossy copy.** On the device snapshot
   (30g image, p2 mounted ro) `blkid` reports the FAT32 label `IPOD USER`,
   while the iTunesDB master playlist (type-1 mhyp) is named `iPod user`
   (harness `tools/libgpod_check.c` against the rebuilt fakepod). FAT32
   labels are uppercase-only; the user's mixed case survives only in the
   iTunesDB. The playlist is authoritative, the label is not.

**Conclusion (owner-approved):** the display name is the iTunesDB
master-playlist name. There is **no volume-name fallback** — it would be our
own FAT-label reading (libgpod cannot provide it), it is lossy (uppercase),
and it is effectively dead code: whenever `itdb_parse` succeeds, the MPL
exists and carries a name. A missing/empty MPL name stays an explicit
absence.

## Design decisions (owner-approved, 2026-11-09)

1. **Name = master-playlist name.** The B-007 wrapper already exposes it:
   `PlaylistInfo { name: Option<String>, is_master }` from
   `session.playlists()`. No new FFI.
2. **No volume-name fallback.** Missing name → explicit absence; the UI
   layer owns any display placeholder ("iPod" or similar never appears in
   the data layer).
3. **Model/generation from `SysInfo` `ModelNumStr`** via libgpod's model
   table, exactly as the wrapper's `DeviceInfo::model: Option<ModelInfo>`
   already resolves it. An empty/absent SysInfo yields `None` and must never
   be replaced by a default model — on the real snapshot SysInfo is empty,
   so model-unknown is the expected live-device outcome and is tested.
4. **Firmware and FireWire ID come only from `SysInfoExtended`**
   (`itdb_sysinfo_extended_parse`, `itdb_sysinfo_properties_get_firewire_id`).
   The snapshot has no `SysInfoExtended`, so both are `None` there.
5. **Capacity from filesystem statistics** (statvfs on the mount point),
   never `Itdb_IpodInfo.capacity` — that is the model table's nominal value
   and the wrapper marks it display-only.
6. **No SysInfo reload on a live parsed/artwork context.** `itdb_device_read_sysinfo()`
   is documented upstream (itdb_device.c, warning above the function) as
   invalidating all artwork of the parsed database: "Trying to access this
   artwork will result in memory corruption." B-005 reads everything from
   the session's already-parsed structures.
7. **Zero writes.** Same discipline as B-003: read-only mounts, no
   initialization, no helper files. A treehash before/after gate proves it.

## Wording alignment (this delta's commits)

- `tasks.md` B-005: research gate marked resolved with the evidence summary;
  Verification rewritten — name from the master playlist, model/generation
  from `ModelNumStr`, firmware/FireWire from `SysInfoExtended` only, capacity
  from fs statistics, missing values explicit.
- `plan.md` metadata-provenance paragraph: master-playlist name, no
  volume-name fallback, no volume-label mention.
- `deltas/b001_research.md` R3: resolution addendum with the evidence
  pointer.

## Open point for the implementation plan (owner decision)

The B-002 contract in `gtk-library` has `name/model/generation: String`
(absence = empty string, UI decides display). "Missing name stays explicit"
is satisfied by the empty string, but the cleaner shape is
`name: Option<String>` (and possibly `model`/`generation` likewise). Changing
the type ripples through B-002's merged tests and the B-003 glue. Default:
keep the `String` contract as is; the implementation plan will either
confirm that or propose the type change as a spec edit.

## Verification sketch (implementation phase, to be expanded in the plan)

- Fixture iPod trees under `iPod_Control/Device/` variants: full SysInfo
  (`ModelNumStr: MA446`), empty SysInfo, SysInfoExtended present/absent,
  MPL with name / without name.
- Env-gated snapshot + mounted gates (`IPOD_TEST_TREE`, `IPOD_TEST_MOUNT`)
  assert the research numbers: name `iPod user`, model resolved from the
  seeded `MA446` (fixture) / `None` on the unseeded real tree, capacities
  measured, firmware/FireWire `None`.
- No-write treehash proof on retrieval; no `itdb_device_read_sysinfo` call
  anywhere in the read path.

## Results

Pending implementation.
