# B-010a: AAC-256 transcoding for device sync — implementation plan

Status: **approved by the owner (2026-11-16); implemented** — results in
§6. Task branch:
`feature/ipod-page-task-B-010a`, based on `feature/ipod-page` at `52b7b28`.
B-010a is the transcode sub-task the owner split out of B-010 ("first of all
implement transcoding to lower bitrate, after this one is ready and tested
continue to main B-010"). The trigger rule is the owner-approved one from
`deltas/b010_plan.md` §11-2; this delta is its own detailed plan.

## 1. Goal and boundaries

Deliver the file-writing half of the sync's transcode rule, tested end to
end on disposable fixtures, **before** the main B-010 executor exists:

1. A pure, unit-tested **transcode decision** (which files get what
   treatment) in `crates/ipod-sync`.
2. A GStreamer **AAC-256 m4a encoder** in `crates/audio-processing` (the
   crate that already owns the transcode pipeline) — the existing AAC arm
   emits ADTS, the pod firmware wants an MP4 container.
3. A **temp-file lifecycle** (host-side staging, deterministic names,
   cleanup on success/abort, orphan sweep) in `crates/ipod-sync`.
4. An **env-gated acceptance**: encode a generated high-nominal-bitrate
   source, copy it onto a disposable device-tree copy through the existing
   gated wrapper surface, re-parse, and pin the result.

Owner rule being implemented (b010_plan §11-2): **non-MP3 tracks with a
bitrate above 256 kbps are transcoded to AAC 256**; MP3 files are never
transcoded; non-MP3 at or below 256 kbps are copied verbatim; PDF Digital
Booklets are never copied at all (b010_plan §11-5).

This sub-task does **not**: run inside the sync executor (B-010 wires the
provider), write to a live pod, enable `sync-writes` on any production
path, implement the capacity re-check against the real encoded size (B-010
uses the measured size), or bind UI. Device/firmware acceptance of the m4a
output stays B-017; this task proves the fixture-level contract.

## 2. What already exists (verified 2026-11-16)

- `crates/audio-processing/src/transcode.rs`: `transcode_audio(stream,
  options, output, cancelled)` — a proven encode loop (startup hold probe,
  seek window, bus polling, cancellation, error mapping to `io::Error`).
  Its `AudioCodec::Aac` arm is `avenc_aac bitrate=N ! aacparse !
  audio/mpeg,mpegversion=4,stream-format=adts` — **raw ADTS, not m4a**.
  The lossy path resamples to 48 kHz and preserves channels.
- Plugins present on the owner's host: `avenc_aac`, `aacparse`, `mp4mux`
  (faststart-capable), `avmux_ipod` (classic iPod muxer), `flacenc`,
  `lamemp3enc`. `avenc_mp3` is absent — irrelevant, MP3 is never encoded.
- `crates/ipod` gated write surface (W-7): `create_track(&TrackMetadata)`,
  `copy_track(dbid, source)` (→ `itdb_cp_track_to_ipod`, the F-name
  layout), `write_library`. `TrackMetadata` carries title/album/…,
  `filetype` (extension string), `size`, `tracklen` (ms), `bitrate`,
  `samplerate`, `rating`.
- `crates/ipod-sync` dev-deps already include `ipod` with the
  `sync-writes` feature and a sources-crate probe; adding
  `audio-processing` as a dev-dependency for the acceptance follows the
  same pattern (production deps stay read-side and GStreamer-free).
- Ground truth: the owner's device already shows what iTunes writes for
  the same rule — 2536 host-ALAC tracks appear on the pod as AAC with
  bitrate 256 (b008_plan §2a(c)).

## 3. Design

### 3.1 Transcode decision (pure, `crates/ipod-sync`)

New module `src/sync/transcode.rs` (no GStreamer, no I/O — the decision is
data in, data out; B-010's executor and the UI consume it):

```
enum FileWriteMode {
    NeverCopy,          // PDF booklets (b010_plan §11-5)
    CopyVerbatim,       // mp3, or non-mp3 <= 256 kbps
    TranscodeToAac256,  // non-mp3 above 256 kbps
}

fn decide(filetype: &str, bitrate_kbps: Option<u32>) -> FileWriteMode
```

Rules (owner-approved, b010_plan §11-2):
- PDF / booklet filetypes → `NeverCopy`.
- MP3 → `CopyVerbatim`, always (owner rule: MP3 is never transcoded).
- Lossless codecs (ALAC, FLAC, WAV, AIFF) → `TranscodeToAac256` — their
  nominal bitrate is above 256 by definition, so the rule's literal form
  and the codec-family form agree.
- Other non-MP3 with `bitrate > 256` → `TranscodeToAac256`.
- Other non-MP3 with `bitrate <= 256` → `CopyVerbatim` (no lossy→lossy
  re-encode).
- Unknown bitrate on a non-MP3 lossy → `TranscodeToAac256` with a
  diagnostic attached (conservative default, mirrors iTunes converting
  high-bitrate files; the rare mis-transcode of a low-bitrate file is
  harmless compared to copying an oversized one).

Unit tests pin the full matrix including unknown-bitrate and PDF arms.

### 3.2 AAC-256 m4a encode (`crates/audio-processing`)

New function `transcode_to_m4a(stream, bitrate_kbps, output, cancelled)`
next to `transcode_audio`, reusing the same primitives
(`ensure_gstreamer_initialized`, the parse-launch + bus/cancel loop shape,
error mapping). Pipeline:

```
uridecodebin ! audioconvert ! audioresample ! audio/x-raw,rate=48000
  ! avenc_aac bitrate=256000
  ! aacparse
  ! mp4mux faststart=true
  ! filesink
```

- `mp4mux faststart=true` puts the moov atom first — the layout the
  classic iPod firmware expects; `avmux_ipod` is the recorded fallback if
  the acceptance shows an issue (both plugins verified present).
- 48 kHz resample and channel preservation follow the crate's existing
  lossy-path convention (the pod plays 48 kHz fine).
- Fixed bitrate 256000 (the crate's existing AAC arm uses the same
  encoder element); the B-009 capacity bound of 32000 bytes/s remains the
  conservative upper bound for the encoded size.
- No tagging: the device DB carries all metadata, the pod firmware reads
  nothing from the file.
- A standalone function rather than generalizing `TranscodePipeline`:
  its encoder-string construction and effects wiring are shaped around
  the crate's other use cases; a focused sibling function is the
  lower-risk change and reviewable in isolation.

Unit test (crate-local, no env gates): generate a WAV with a high nominal
bitrate (44.1 kHz stereo 16-bit — nominal 1411 kbps, the record-trigger
shape), encode to m4a, then decode-verify: file parses via
`uridecodebin`, non-trivial size, and the encoded size stays under
`duration_ms / 1000 × 32000`.

### 3.3 Temp lifecycle (pure, `crates/ipod-sync`)

`src/sync/transcode.rs` (same module as 3.1): deterministic staging —
`<temp_dir>/rufin-ipod-sync/<device-id>/<track-dbid>.m4a` — with
`create_staging_dir`, `staging_path_for`, and `cleanup_staging`
(success + abort) and an orphan sweep the executor runs at sync start.
`device-id` is the identity scheme from b010_plan §5 (FireWire ID when
present, else master-pid+model hash). Pure directory/path logic; the
encoder writes through a caller-supplied closure so the module stays
GStreamer-free (b010_plan §7's caller-supplied-provider pattern).

### 3.4 Env-gated acceptance (`crates/ipod-sync`, dev-deps)

One integration test, gated on `RUFIN_LIBGPOD_DIR` only (the pinned
libgpod for the backend load). No snapshot corpus is involved at all —
the device tree is built fresh by `ipod::testkit::init_device`, which is
what makes a before/after tree-hash proof unnecessary (there are no
originals to protect):

1. Generate the hi-nominal-bitrate WAV (the 3.2 fixture generator,
   duplicated in the dev test).
2. `decide()` says `TranscodeToAac256` for it; a fake MP3 record and a
   ≤256 AAC record say `CopyVerbatim` (pure, no env).
3. Encode via `audio-processing` (dev-dep) → assert the m4a parses and
   the size bound holds.
4. On the **freshly initialized** device tree (`ipod::testkit`
   conventions): `create_track` with the host record's metadata and
   `filetype = "m4a"`, `copy_track(dbid, encoded_m4a)`, `write_library`;
   then a fresh session re-parses: the track exists, `ipod_path` resolves,
   the file is byte-identical to the encoded m4a, and the DB record says
   m4a/AAC with sane bitrate.

The acceptance proves the whole chain the main task will call: decide →
encode → gated copy → parseable device state. Skip messages point at
`scripts/build-libgpod.sh` per the B-008 §8 convention.

## 4. Work sequence

1. `src/sync/transcode.rs` in ipod-sync: the decision + staging lifecycle
   with unit tests (matrix + tempdir fixtures).
2. `transcode_to_m4a` in audio-processing with the crate-local
   encode→decode-verify test.
3. Env-gated acceptance in ipod-sync (dev-deps: `audio-processing`; the
   `ipod` sync-writes dev-dep already exists).
4. Full battery: `cargo test -p ipod-sync`, `cargo test -p
   audio-processing`, the env-gated acceptance, wrapper suite,
   `scripts/ipod-dev lint`, `git diff --check`. Record results in this
   delta; request review.

After merge (on trunk): add the B-010a entry to `tasks.md` (convention:
tasks.md moves on the feature branch), and amend `deltas/b010_plan.md`
§11-2/§4/§7/§9/§10 with a one-liner that the transcode provider is
delivered by B-010a and consumed by B-010.

## 5. Decisions (proposed, for owner review)

1. **Muxer**: `mp4mux faststart=true` primary, `avmux_ipod` fallback
   (§3.2). Both plugins verified present.
2. **Unknown-bitrate non-MP3 lossy**: transcode with a diagnostic
   (§3.1) — conservative default.
3. **48 kHz resample**: keep the crate's existing lossy-path convention
   (§3.2); 44.1 passthrough is not worth a pipeline fork.
4. **No tagging of the encoded file** (§3.2) — the DB is the metadata
   carrier on the pod.

Non-goals restated: executor integration, live-pod writes, capacity
re-check math, UI — all B-010/B-011+.

## 6. Implementation results (2026-11-16)

- Decision + staging module landed in `crates/ipod-sync/src/sync/transcode.rs`
  (3 unit tests: the full decision matrix, deterministic scoped staging
  paths, lifecycle + device-scoped orphan sweep).
- `transcode_to_m4a` landed in `crates/audio-processing` with the shared
  `map_pipeline_error` refactor (the same error mapping now serves both
  entry points). Crate-local tests pin `ftyp`, moov-before-mdat (the
  faststart layout the pod needs), the 32000 B/s capacity bound and
  decode-back verification, plus the two failure paths: a pre-cancelled
  encode returns `Interrupted` without leaving a file, and an unencodable
  source returns the mapped pipeline error (bounded by a test-only
  cancellation deadline).
- Env-gated acceptance `crates/ipod-sync/tests/b010a_transcode.rs` passes
  against the pinned libgpod (fresh MA446 tree): decide → encode → gated
  create/copy/write → re-parse pins filetype `m4a`, bitrate 256, on-pod
  path ending `.m4a`, and a byte-identical on-disk copy.
- Battery: ipod-sync 74 unit + the full env-gated suite (pair gate,
  mapping, transcode gate) green; audio-processing 13/13; test-synthetic;
  test-wrapper; lint exit 0; `git diff --check` clean.
- §5 decisions as implemented: `mp4mux faststart=true` (no fallback needed
  on the fixtures); the unknown-bitrate arm transcodes with a diagnostic;
  48 kHz resample kept; encoded files carry no tags.
