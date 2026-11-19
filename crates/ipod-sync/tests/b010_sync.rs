//! B-010: the sync gates — env-gated, disposable corpora only
//! (`deltas/b010_plan.md` §9). Env: `ITL_TEST_PATH` (a disposable `.itl`
//! copy), `IPOD_TEST_TREE` (the restored-device snapshot), `ITL_TEST_MEDIA_ROOT`
//! (the real media root the host mapping resolves against),
//! `RUFIN_LIBGPOD_DIR` (the pinned libgpod). Everything runs on fresh
//! copies; the originals must stay byte-identical.
//!
//! `sync_pair_gate`: one full sync on the in-sync pair — the known adds
//! minus the policy exclusions, zero pending imports, a completed
//! journal, and a byte-identical original `.itl` copy.
//!
//! `sync_failure_battery`: the S-13/§9 checklist on a seeded pair —
//! fail-after-every-step with recovery, no double import across the
//! host-commit crash window, host-refusal abort before any device change,
//! and competing-writer refusal.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ipod_sync::dry_run::{
    ArtworkProbe, CoverEvidence, DeviceOnlyPolicy, HostInputs, SourceKey, SyncOptions,
    collect_device, collect_host, compute,
};
use ipod_sync::sync::exec::{
    STEP_BACKUP, STEP_DB_WRITE, STEP_FILE_OPS, STEP_HOST_WRITE, STEP_INTENT, STEP_VERIFY, STEPS,
    SyncContext, SyncStatus, run_sync,
};
use ipod_sync::sync::host::HostWriteContext;
use ipod_sync::sync::journal::{Journal, LAST_SUCCESS_FILE};
use ipod_sync::sync::merge::StatMerge;
use ipod_sync::sync::plan::ExecutionPlan;
use ipod_sync::sync::report::{ProgressEvent, RecoveryState};
use itl_core::ItlFile;

struct LoftyProbe;

impl ArtworkProbe for LoftyProbe {
    fn cover(&self, path: &Path) -> CoverEvidence {
        use lofty::file::TaggedFileExt;
        let Ok(file) = lofty::read_from_path(path) else {
            return CoverEvidence::Unknown;
        };
        let tag = file
            .tag(file.primary_tag_type())
            .or_else(|| file.tags().first());
        match tag.and_then(|tag| tag.pictures().first()) {
            Some(picture) => CoverEvidence::Present {
                bytes: picture.data().len() as u64,
            },
            None => CoverEvidence::Absent,
        }
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &target);
        } else {
            std::fs::copy(&source, &target).unwrap();
        }
    }
}

fn hash_tree(root: &Path) -> Vec<(String, String)> {
    use std::collections::BTreeMap;
    fn walk(root: &Path, prefix: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path
                .strip_prefix(prefix)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(relative, "<dir>".into());
                walk(&path, prefix, out);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::Hasher;
                hasher.write(&bytes);
                out.insert(relative, format!("{:016x}", hasher.finish()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out.into_iter().collect()
}

/// Seed empty media files at every on-pod path the snapshot's DB records.
/// The restored-device snapshot carries no `Music/` (the B-009 gate clears
/// the probe map for this); the executor's revalidation re-collects the
/// device itself, so the fixture must make the files exist — otherwise
/// every matched track becomes a FileRepair and the space check refuses.
fn seed_media_files(device_root: &Path, backend: &ipod::IpodBackend) {
    let session = ipod::DeviceSession::open(backend, device_root).unwrap();
    for track in session.tracks().unwrap() {
        let Some(path) = track.ipod_path else {
            continue;
        };
        let on_disk = device_root.join(path.trim_start_matches(':').replace(':', "/"));
        if let Some(parent) = on_disk.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        if !on_disk.exists() {
            std::fs::write(&on_disk, b"seeded media placeholder").unwrap();
        }
    }
}

fn consistency_hash(mount: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let ipod_control = mount.join("iPod_Control");
    for relative in ipod_sync::sync::backup::CONSISTENCY_SET {
        let path = ipod_control.join(relative);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::Hasher;
                hasher.write(&bytes);
                out.push((relative.to_string(), format!("{:016x}", hasher.finish())));
            }
            Err(_) => out.push((relative.to_string(), "<absent>".into())),
        }
    }
    if let Ok(entries) = std::fs::read_dir(ipod_control.join("Artwork")) {
        let mut banks: Vec<(String, String)> = entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".ithmb"))
            .map(|entry| {
                let bytes = std::fs::read(entry.path()).unwrap();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::Hasher;
                hasher.write(&bytes);
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    format!("{:016x}", hasher.finish()),
                )
            })
            .collect();
        banks.sort();
        out.extend(banks);
    }
    out.sort();
    out
}

fn env_optional(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// Load the common env (the `.itl` copy, its media root, the device
/// snapshot, the backend); skip with a message when anything is missing.
fn gate_env() -> Option<(PathBuf, PathBuf, PathBuf, ipod::IpodBackend)> {
    let itl = env_optional("ITL_TEST_PATH")?;
    if !itl.is_file() {
        eprintln!("skipping: ITL_TEST_PATH is not a file ({})", itl.display());
        return None;
    }
    let media_root = env_optional("ITL_TEST_MEDIA_ROOT")
        .unwrap_or_else(|| itl.parent().expect("itl parent").to_path_buf());
    let tree = env_optional("IPOD_TEST_TREE")?;
    if !tree.join("iPod_Control").is_dir() {
        eprintln!("skipping: IPOD_TEST_TREE has no iPod_Control/");
        return None;
    }
    match ipod::IpodBackend::load() {
        Ok(backend) => Some((itl, media_root, tree, backend)),
        Err(error) => {
            eprintln!("skipping: backend unavailable ({error}); see scripts/build-libgpod.sh");
            None
        }
    }
}

/// Build the approved plan from the current host + device copies.
fn approved_plan(
    itl: &Path,
    media_root: &Path,
    device_root: &Path,
    backend: &ipod::IpodBackend,
) -> (HostInputs, ExecutionPlan) {
    let itl_file = ItlFile::open(itl).expect("open .itl copy");
    let host = collect_host(&itl_file, media_root, &LoftyProbe);
    let session = ipod::DeviceSession::open(backend, device_root).unwrap();
    let facts = ipod_sync::retrieval::collect(&session, device_root).unwrap();
    let device = collect_device(&session, device_root, &facts).unwrap();
    let options = SyncOptions {
        source_key: SourceKey("gate".into()),
        transcode_aac: true,
        device_only: DeviceOnlyPolicy::Remove,
    };
    let report = compute(&host, &device, &options);
    let plan = ExecutionPlan::build(&report, StatMerge::default());
    (host, plan)
}

/// Seed a pending store: the first track gets +2 plays / +1 skip (the
/// R1-measured format: entries align to the mhlt order, no dbids).
fn seed_pending_store(device_root: &Path) {
    let mut bytes = vec![0_u8; 0x60];
    bytes[0..4].copy_from_slice(b"mhdp");
    bytes[4..8].copy_from_slice(&0x60_u32.to_le_bytes());
    bytes[8..12].copy_from_slice(&0x1c_u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&1_u32.to_le_bytes());
    let entry = [2_u32, 0xE7150000, 0, 0xFFFF_FFFF, 0, 1, 0];
    for value in entry {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(device_root.join("iPod_Control/iTunes/Play Counts"), &bytes).unwrap();
}

#[allow(clippy::too_many_arguments)]
fn context<'a>(
    device_root: &'a Path,
    identity: &'a str,
    backend: &'a ipod::IpodBackend,
    host: &'a HostInputs,
    options: &'a SyncOptions,
    host_write: &'a HostWriteContext,
    plan: &'a ExecutionPlan,
    fail_after: &'a dyn Fn(&str) -> bool,
    reporter: &'a (dyn Fn(ProgressEvent) + Send + Sync),
    artwork: &'a dyn Fn(u64) -> Option<ipod::ArtworkInput>,
) -> SyncContext<'a> {
    context_with(
        device_root,
        identity,
        backend,
        host,
        options,
        host_write,
        plan,
        fail_after,
        reporter,
        &|| false,
        artwork,
    )
}

/// The full context builder — `cancelled` is caller-supplied for the
/// cancellation gate; the plain [`context`] passes `false` forever.
/// `identity` is per-test and must be stable across one test's runs: the
/// transcode staging dir keys on it, so two parallel tests sharing an
/// identity would race on each other's staged m4a files.
#[allow(clippy::too_many_arguments)]
fn context_with<'a>(
    device_root: &'a Path,
    identity: &'a str,
    backend: &'a ipod::IpodBackend,
    host: &'a HostInputs,
    options: &'a SyncOptions,
    host_write: &'a HostWriteContext,
    plan: &'a ExecutionPlan,
    fail_after: &'a dyn Fn(&str) -> bool,
    reporter: &'a (dyn Fn(ProgressEvent) + Send + Sync),
    cancelled: &'a dyn Fn() -> bool,
    artwork: &'a dyn Fn(u64) -> Option<ipod::ArtworkInput>,
) -> SyncContext<'a> {
    SyncContext {
        mount: device_root,
        backend,
        host,
        options,
        host_write,
        device_identity: identity,
        plan,
        transcode: &|source, destination| {
            let uri = gstreamer::glib::filename_to_uri(source, None).unwrap();
            audio_processing::transcode_to_m4a(
                &audio_processing::ResolvedStream::new(uri.as_str()),
                256,
                destination,
                || false,
            )
        },
        artwork,
        cancelled,
        fail_after,
        reporter,
    }
}

/// The pair gate's artwork provider: read the embedded cover straight
/// from the host file (the production boundary returns `None` until the
/// runtime-catalog mapping lands, so the gate owns the engine-side
/// proof — the B-017 step-0 finding: without a provider, planned covers
/// for adds are skipped and the re-run never empties).
fn embedded_cover_provider(host: &HostInputs) -> impl Fn(u64) -> Option<ipod::ArtworkInput> + '_ {
    move |pid| {
        let path = host
            .tracks
            .iter()
            .find(|track| track.track_pid == pid)
            .and_then(|track| match &track.location {
                ipod_sync::mapping::HostLocation::Mapped(path) => Some(path.clone()),
                _ => None,
            })?;
        use lofty::file::TaggedFileExt;
        let file = lofty::read_from_path(&path).ok()?;
        let tag = file
            .tag(file.primary_tag_type())
            .or_else(|| file.tags().first())?;
        let picture = tag.pictures().first()?;
        Some(ipod::ArtworkInput {
            encoded: picture.data().to_vec(),
            rotation: 0,
        })
    }
}

fn gate_options() -> SyncOptions {
    SyncOptions {
        source_key: SourceKey("gate".into()),
        transcode_aac: true,
        device_only: DeviceOnlyPolicy::Remove,
    }
}

#[test]
fn sync_pair_gate() {
    let Some((itl, media_root, tree, backend)) = gate_env() else {
        return;
    };
    let workdir = tempfile::tempdir().unwrap();
    let device_root = workdir.path().join("device");
    copy_tree(&tree, &device_root);
    let itl_copy = workdir.path().join("library.itl");
    std::fs::copy(&itl, &itl_copy).unwrap();
    let itl_original = std::fs::read(&itl).unwrap();
    seed_media_files(&device_root, &backend);
    let tree_before = hash_tree(&tree);

    let (host, plan) = approved_plan(&itl_copy, &media_root, &device_root, &backend);
    // The dry run excludes PDF booklets from `file_adds` entirely
    // (b015_fixes F4) — the executor copies every planned add.
    let expected_adds = plan.file_adds.len();
    let options = gate_options();
    let host_write = HostWriteContext {
        library_path: itl_copy.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };

    let events = Mutex::new(Vec::<ProgressEvent>::new());
    let reporter = |event: ProgressEvent| events.lock().unwrap().push(event);
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let artwork = embedded_cover_provider(&host);
    let ctx = context(
        &device_root,
        "gate-pair",
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &|_| false,
        &reporter,
        &artwork,
    );
    let result = run_sync(&ctx);
    let finished = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(result.status, SyncStatus::Completed);
    assert_eq!(
        result.changes.files_added, expected_adds,
        "all adds minus the PDF booklets: {:?}",
        result.diagnostics
    );
    assert_eq!(
        result.recovery, None,
        "a completed run holds no pending recovery"
    );
    let last = result
        .last_success
        .expect("the completion advances last-success");
    assert!(
        last.completed_unix >= started && last.completed_unix <= finished,
        "the marker carries the completion time ({:?} not in {started}..={finished})",
        last.completed_unix
    );

    // B-011: the emitted step sequence — one began+ended per step, in
    // protocol order, on a x/6 bar.
    let events = events.into_inner().unwrap();
    let steps: Vec<(&'static str, u8, bool)> = events
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::Step {
                name,
                index,
                total,
                began,
            } => {
                assert_eq!(*total, 6, "the step bar is x/6");
                Some((*name, *index, *began))
            }
            _ => None,
        })
        .collect();
    let mut expected_steps = Vec::new();
    for (position, name) in STEPS.iter().enumerate() {
        expected_steps.push((*name, position as u8 + 1, true));
        expected_steps.push((*name, position as u8 + 1, false));
    }
    assert_eq!(steps, expected_steps, "one began+ended per step, in order");

    // B-011: the FileOps item stream — determinate items run 1..=T with
    // one constant total, and every item everywhere stays within it.
    for event in &events {
        if let ProgressEvent::Item { current, total, .. } = event {
            assert!(
                *total == 0 || *current <= *total,
                "no determinate item exceeds its total: {current}/{total}"
            );
        }
    }
    let mut phase: Option<&str> = None;
    let mut file_ops_items: Vec<(u32, u32)> = Vec::new();
    for event in &events {
        match event {
            ProgressEvent::Step { name, began, .. } => {
                if *began {
                    phase = Some(name);
                }
            }
            ProgressEvent::Item { current, total, .. }
                if phase == Some(STEP_FILE_OPS) && *total > 0 =>
            {
                file_ops_items.push((*current, *total));
            }
            _ => {}
        }
    }
    assert!(
        !file_ops_items.is_empty(),
        "the pair sync does determinate file work"
    );
    let file_ops_total = file_ops_items[0].1;
    for (position, (current, item_total)) in file_ops_items.iter().enumerate() {
        assert_eq!(*item_total, file_ops_total, "one constant FileOps total");
        assert_eq!(
            *current,
            position as u32 + 1,
            "current runs 1..=total without gaps"
        );
    }
    let plan_sum = (plan.file_adds.len()
        + plan.file_repairs.len()
        + plan.file_removes.len()
        + plan.playlist_creates.len()
        + plan.playlist_updates.len()
        + plan.playlist_removes.len()
        + plan.artwork_adds.len()) as u32;
    assert!(
        file_ops_total >= plan_sum,
        "the phase total covers the plan plus the stat patches ({file_ops_total} vs {plan_sum})"
    );

    // The post-sync device parses and carries the added tracks.
    let session = ipod::DeviceSession::open(&backend, &device_root).unwrap();
    let tracks = session.tracks().unwrap();
    assert_eq!(tracks.len(), 3373 + expected_adds, "track count after adds");
    let status = session.artwork_status().unwrap();
    assert_eq!(
        status.verdict,
        ipod::ArtworkVerdict::Ok,
        "artwork proof-read"
    );
    drop(session);

    // B-017 step-0 hardening: a second dry run over the synced pair must
    // be empty — a run that reports success while the re-read still shows
    // deltas is a lie (the step-0 app run left 3384 shifted play dates,
    // two unapplied smart-member updates, and new-track covers behind).
    let itl_file = ItlFile::open(&itl_copy).unwrap();
    let host_after = collect_host(&itl_file, &media_root, &LoftyProbe);
    let session = ipod::DeviceSession::open(&backend, &device_root).unwrap();
    let facts = ipod_sync::retrieval::collect(&session, &device_root).unwrap();
    let device_after = collect_device(&session, &device_root, &facts).unwrap();
    drop(session);
    let report_after = compute(&host_after, &device_after, &options);
    assert_eq!(report_after.file_adds.len(), 0, "no adds remain");
    assert_eq!(report_after.file_repairs.len(), 0, "no repairs remain");
    assert_eq!(report_after.file_removes.len(), 0, "no removes remain");
    assert_eq!(report_after.playlist_creates.len(), 0);
    assert_eq!(report_after.playlist_removes.len(), 0);
    assert_eq!(report_after.artwork_adds.len(), 0, "no covers remain");
    assert_eq!(
        report_after.stat_counts.rating, 0,
        "rating diffs gone: {report_after:?}"
    );
    assert_eq!(
        report_after.stat_counts.play_count, 0,
        "play-count diffs gone: {report_after:?}"
    );
    assert_eq!(
        report_after.stat_counts.play_date, 0,
        "play-date diffs gone (the tz double-shift regression): {report_after:?}"
    );
    for update in &report_after.playlist_updates {
        assert!(
            update.is_spl && update.criteria == ipod_sync::dry_run::CriteriaState::Differ,
            "only deferred smart-criteria updates may survive: {update:?}"
        );
    }

    // The journal holds the completion marker, no active record.
    let journal = Journal::open(&device_root).unwrap();
    assert!(journal.load_active().unwrap().is_none());
    assert!(
        journal.root().join(LAST_SUCCESS_FILE).is_file(),
        "last-success marker exists"
    );

    // The originals are untouched: the corpus tree and the `.itl` copy
    // source byte-identical.
    assert_eq!(hash_tree(&tree), tree_before, "device corpus untouched");
    assert_eq!(
        std::fs::read(&itl).unwrap(),
        itl_original,
        ".itl copy source untouched"
    );
}

#[test]
fn sync_failure_battery() {
    let Some((itl, media_root, tree, backend)) = gate_env() else {
        return;
    };
    let workdir = tempfile::tempdir().unwrap();
    let device_root = workdir.path().join("device");
    copy_tree(&tree, &device_root);
    let itl_copy = workdir.path().join("library.itl");
    std::fs::copy(&itl, &itl_copy).unwrap();
    seed_media_files(&device_root, &backend);

    // Seed a pending store: the first track gets +2 plays / +1 skip (the
    // R1-measured format: entries align to the mhlt order, no dbids).
    let first_dbid = {
        let session = ipod::DeviceSession::open(&backend, &device_root).unwrap();
        session.tracks().unwrap()[0].dbid
    };
    let host_playcount_of = |dbid: u64| {
        let itl_file = ItlFile::open(&itl_copy).unwrap();
        let host = collect_host(&itl_file, &media_root, &LoftyProbe);
        host.tracks
            .iter()
            .find(|track| track.track_pid == dbid)
            .map(|track| track.stats.play_count)
            .unwrap_or(0)
    };
    // The device's stored baseline for the seeded track (before the
    // pending store exists — the pair legitimately drifts on stats).
    let device_stored = {
        let session = ipod::DeviceSession::open(&backend, &device_root).unwrap();
        session.tracks().unwrap()[0].playcount
    };
    let first_track_before = host_playcount_of(first_dbid);

    // --- Host refusal: a broken library path must abort BEFORE any
    // device change (the import-first ordering invariant).
    seed_pending_store(&device_root);
    // A pre-existing last-success marker: a refused run must never touch
    // it (S-10 — only full completion advances).
    let seeded_marker = br#"{"device":"gate-device","plan_hash":"seed","completed_unix":12345}"#;
    std::fs::create_dir_all(device_root.join("iPod_Control/Rufin")).unwrap();
    std::fs::write(
        device_root.join("iPod_Control/Rufin/last-success.json"),
        seeded_marker,
    )
    .unwrap();
    // The refusal must leave the *consistency set* untouched (the journal
    // and backup legitimately land before the host write — write ordering).
    let device_before = consistency_hash(&device_root);
    let itl_before = std::fs::read(&itl_copy).unwrap();
    let (host, plan) = approved_plan(&itl_copy, &media_root, &device_root, &backend);
    let options = gate_options();
    let good_host_write = HostWriteContext {
        library_path: itl_copy.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };
    let missing_library = workdir.path().join("missing.itl");
    let bad_host_write = HostWriteContext {
        library_path: missing_library.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };
    let bad_context = context(
        &device_root,
        "gate-battery",
        &backend,
        &host,
        &options,
        &bad_host_write,
        &plan,
        &|_| false,
        &|_| {},
        &|_| None,
    );
    let result = run_sync(&bad_context);
    let SyncStatus::Failed { step, diagnostic } = &result.status else {
        panic!("host refusal must fail: {:?}", result.status);
    };
    assert_eq!(
        *step, STEP_HOST_WRITE,
        "abort at the host write: {diagnostic}"
    );
    assert_eq!(
        result.recovery,
        Some(RecoveryState::Resumable {
            completed_steps: vec![STEP_INTENT, STEP_BACKUP],
        }),
        "the refused run leaves its committed prefix resumable"
    );
    assert_eq!(
        std::fs::read(device_root.join("iPod_Control/Rufin/last-success.json")).unwrap(),
        seeded_marker,
        "a failed run never advances last-success"
    );
    assert_eq!(
        result
            .last_success
            .as_ref()
            .map(|marker| marker.completed_unix),
        Some(12345),
        "the result carries the previous marker, not a fresh one"
    );
    assert_eq!(
        consistency_hash(&device_root),
        device_before,
        "no consistency-set change before the host write"
    );
    assert_eq!(
        std::fs::read(&itl_copy).unwrap(),
        itl_before,
        "no host write happened"
    );
    // The pending store is untouched — the retry starts clean.
    assert!(
        device_root
            .join("iPod_Control/iTunes/Play Counts")
            .is_file(),
        "Play Counts not consumed by the refused run"
    );
    // Clear the refused run's journal (intent + backup) so the step
    // injections below start from a clean operation.
    std::fs::remove_dir_all(device_root.join("iPod_Control/Rufin")).unwrap();

    // --- Fail after every step boundary, then recover: the pending delta
    // lands exactly once (no double import).
    for (position, injected) in STEPS.iter().enumerate() {
        let injection = |step: &str| step == *injected;
        let events = Mutex::new(Vec::<ProgressEvent>::new());
        let reporter = |event: ProgressEvent| events.lock().unwrap().push(event);
        let ctx = context(
            &device_root,
            "gate-battery",
            &backend,
            &host,
            &options,
            &good_host_write,
            &plan,
            &injection,
            &reporter,
            &|_| None,
        );
        let result = run_sync(&ctx);
        if *injected == STEP_VERIFY {
            // A crash after the committed DB write is recovered by the
            // planner's verify-and-advance route — that IS the designed
            // completion, and it finishes the operation.
            assert_eq!(result.status, SyncStatus::Completed);
            assert_eq!(result.recovery, None);
            assert!(
                result.diagnostics.iter().any(|d| d.contains("recovered")),
                "the verify-and-advance route reports the recovery: {:?}",
                result.diagnostics
            );
        } else {
            let SyncStatus::Failed { step, .. } = &result.status else {
                panic!(
                    "injection after {injected} must stop the run: {:?}",
                    result.status
                );
            };
            assert_eq!(*step, *injected, "the failure names the injected step");
            // B-011: only the executed steps emit on a resume — the
            // committed prefix resumes silently, and the journal holds
            // exactly the committed prefix as the recovery reference.
            // The DbWrite injection re-executes FileOps too (its commit
            // is gated on DbWrite, and the crashed session's mutations
            // are gone), so both steps emit.
            let began: Vec<&'static str> = events
                .into_inner()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    ProgressEvent::Step {
                        name, began: true, ..
                    } => Some(*name),
                    _ => None,
                })
                .collect();
            let expected_began: Vec<&'static str> = if *injected == STEP_DB_WRITE {
                vec![STEP_FILE_OPS, STEP_DB_WRITE]
            } else {
                vec![*injected]
            };
            assert_eq!(
                began, expected_began,
                "only the executed steps emit at position {position}"
            );
            assert_eq!(
                result.recovery,
                Some(RecoveryState::Resumable {
                    completed_steps: STEPS[..=position].to_vec(),
                }),
                "the committed prefix is the recovery reference"
            );
            // The device must still parse (or be restorable next run).
            assert!(
                ipod::DeviceSession::open(&backend, &device_root).is_ok(),
                "device parses after the {injected} crash"
            );
        }
    }
    // The pending plays landed exactly once: the merge's drift guard
    // gives max(host, device stored) + pending — a second import would
    // add the pending again on top.
    let expected = first_track_before.max(device_stored) + 2;
    assert_eq!(
        host_playcount_of(first_dbid),
        expected,
        "the seeded plays are imported exactly once across the crashes"
    );
    // And the pending store is fully consumed.
    let itunes_dir = device_root.join("iPod_Control/iTunes");
    assert!(
        !itunes_dir.join("Play Counts").is_file() && !itunes_dir.join("Play Counts.bak").is_file(),
        "the pending store is fully consumed at completion"
    );
    let journal = Journal::open(&device_root).unwrap();
    assert!(journal.load_active().unwrap().is_none());
    assert!(journal.root().join(LAST_SUCCESS_FILE).is_file());

    // --- Competing writer: a held device lock refuses a new operation
    // before anything happens (the /proc scan skips self, so the lock
    // layer is the check the gate exercises).
    let lock_path = device_root.join("iPod_Control/Rufin/sync.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    // SAFETY: a valid fd for the duration; the lock dies with the file.
    unsafe {
        assert_eq!(
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&lock_file),
                libc::LOCK_EX | libc::LOCK_NB
            ),
            0,
            "the gate's own lock acquisition"
        );
    }
    let (host, plan) = approved_plan(&itl_copy, &media_root, &device_root, &backend);
    let ctx = context(
        &device_root,
        "gate-battery",
        &backend,
        &host,
        &options,
        &good_host_write,
        &plan,
        &|_| false,
        &|_| {},
        &|_| None,
    );
    let result = run_sync(&ctx);
    let SyncStatus::Failed { step, diagnostic } = &result.status else {
        panic!("a held device lock must refuse: {:?}", result.status);
    };
    assert_eq!(*step, "Intent");
    assert!(diagnostic.contains("lock"), "{diagnostic}");
    assert_eq!(
        result.recovery, None,
        "a clean journal holds no recovery reference"
    );
    drop(lock_file);
}

/// B-011 gate: the resume bar. A crash after FileOps leaves the committed
/// prefix in the journal; the next run's first event is the resume
/// notice, step events begin at the re-executed FileOps step (4/6), and
/// the resumed run completes and advances last-success.
#[test]
fn sync_reporting_resumes_from_the_committed_prefix() {
    let Some((itl, media_root, tree, backend)) = gate_env() else {
        return;
    };
    let workdir = tempfile::tempdir().unwrap();
    let device_root = workdir.path().join("device");
    copy_tree(&tree, &device_root);
    let itl_copy = workdir.path().join("library.itl");
    std::fs::copy(&itl, &itl_copy).unwrap();
    seed_media_files(&device_root, &backend);

    let (host, plan) = approved_plan(&itl_copy, &media_root, &device_root, &backend);
    let options = gate_options();
    let host_write = HostWriteContext {
        library_path: itl_copy.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };

    // Crash after FileOps committed.
    let crash = |step: &str| step == STEP_FILE_OPS;
    let ctx = context(
        &device_root,
        "gate-resume",
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &crash,
        &|_| {},
        &|_| None,
    );
    let result = run_sync(&ctx);
    assert_eq!(
        result.status,
        SyncStatus::Failed {
            step: STEP_FILE_OPS,
            diagnostic: "injected failure after the step committed".into(),
        },
        "the injected crash stops the run at FileOps"
    );
    assert_eq!(
        result.recovery,
        Some(RecoveryState::Resumable {
            completed_steps: vec![STEP_INTENT, STEP_BACKUP, STEP_HOST_WRITE, STEP_FILE_OPS],
        })
    );
    assert_eq!(
        result.last_success, None,
        "the crashed run never advanced last-success"
    );

    // Resume with a recording reporter.
    let events = Mutex::new(Vec::<ProgressEvent>::new());
    let reporter = |event: ProgressEvent| events.lock().unwrap().push(event);
    let ctx = context(
        &device_root,
        "gate-resume",
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &|_| false,
        &reporter,
        &|_| None,
    );
    let result = run_sync(&ctx);
    assert_eq!(result.status, SyncStatus::Completed);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("resuming")),
        "the resume is reported: {:?}",
        result.diagnostics
    );
    let events = events.into_inner().unwrap();
    assert!(
        matches!(&events[0], ProgressEvent::Notice(message)
            if message.contains("resuming an interrupted operation (4/6")),
        "the resume notice comes first: {:?}",
        events[0]
    );
    let first_step = events
        .iter()
        .find_map(|event| match event {
            ProgressEvent::Step {
                name,
                index,
                began: true,
                ..
            } => Some((*name, *index)),
            _ => None,
        })
        .expect("a step event");
    assert_eq!(
        first_step,
        (STEP_FILE_OPS, 4),
        "the bar resumes at the re-executed step, never at zero"
    );
    let began_steps: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ProgressEvent::Step {
                name, began: true, ..
            } => Some(*name),
            _ => None,
        })
        .collect();
    assert_eq!(
        began_steps,
        vec![STEP_FILE_OPS, STEP_DB_WRITE, STEP_VERIFY],
        "the committed prefix emits no step events"
    );
    assert!(
        result.last_success.is_some(),
        "the resumed run completes and advances last-success"
    );
}

/// B-011 gate: the restore arm. A crash after the committed DB write plus
/// a corrupted device database must restore from the backup and report
/// partial success with the `Restored` reference — never a silent
/// completion, never a fresh last-success.
#[test]
fn sync_reporting_restores_a_broken_database() {
    let Some((itl, media_root, tree, backend)) = gate_env() else {
        return;
    };
    let workdir = tempfile::tempdir().unwrap();
    let device_root = workdir.path().join("device");
    copy_tree(&tree, &device_root);
    let itl_copy = workdir.path().join("library.itl");
    std::fs::copy(&itl, &itl_copy).unwrap();
    seed_media_files(&device_root, &backend);

    let (host, plan) = approved_plan(&itl_copy, &media_root, &device_root, &backend);
    let options = gate_options();
    let host_write = HostWriteContext {
        library_path: itl_copy.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };

    // Crash after the DB write committed.
    let crash = |step: &str| step == STEP_DB_WRITE;
    let ctx = context(
        &device_root,
        "gate-restore",
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &crash,
        &|_| {},
        &|_| None,
    );
    let result = run_sync(&ctx);
    assert_eq!(
        result.status,
        SyncStatus::Failed {
            step: STEP_DB_WRITE,
            diagnostic: "injected failure after the step committed".into(),
        }
    );

    // Corrupt the device database — the next run must restore, not resume.
    std::fs::write(
        device_root.join("iPod_Control/iTunes/iTunesDB"),
        b"garbage database",
    )
    .unwrap();
    let events = Mutex::new(Vec::<ProgressEvent>::new());
    let reporter = |event: ProgressEvent| events.lock().unwrap().push(event);
    let ctx = context(
        &device_root,
        "gate-restore",
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &|_| false,
        &reporter,
        &|_| None,
    );
    let result = run_sync(&ctx);
    let SyncStatus::Partial {
        failed_step,
        diagnostic,
    } = &result.status
    else {
        panic!(
            "a broken DB must restore to partial success: {:?}",
            result.status
        );
    };
    assert_eq!(*failed_step, STEP_INTENT);
    assert!(diagnostic.contains("restored"), "{diagnostic}");
    assert_eq!(
        result.recovery,
        Some(RecoveryState::Restored),
        "the partial result references the restore"
    );
    assert_eq!(
        result.last_success, None,
        "a restore never advances last-success"
    );
    assert!(
        ipod::DeviceSession::open(&backend, &device_root).is_ok(),
        "the restored database parses"
    );
    let events = events.into_inner().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProgressEvent::Item { label, .. } if label.contains("restoring the consistency set"))),
        "the restore emits its indeterminate item: {events:?}"
    );
}

/// B-011 gate: cancellation is a resumable failure. The pending store
/// stays untouched, the last-success marker stays byte-identical, and the
/// recovery reference carries the committed prefix.
#[test]
fn sync_reporting_cancellation_is_a_resumable_failure() {
    let Some((itl, media_root, tree, backend)) = gate_env() else {
        return;
    };
    let workdir = tempfile::tempdir().unwrap();
    let device_root = workdir.path().join("device");
    copy_tree(&tree, &device_root);
    let itl_copy = workdir.path().join("library.itl");
    std::fs::copy(&itl, &itl_copy).unwrap();
    seed_media_files(&device_root, &backend);
    seed_pending_store(&device_root);

    let seeded_marker = br#"{"device":"gate-device","plan_hash":"seed","completed_unix":777}"#;
    std::fs::create_dir_all(device_root.join("iPod_Control/Rufin")).unwrap();
    std::fs::write(
        device_root.join("iPod_Control/Rufin/last-success.json"),
        seeded_marker,
    )
    .unwrap();

    let (host, plan) = approved_plan(&itl_copy, &media_root, &device_root, &backend);
    let options = gate_options();
    let host_write = HostWriteContext {
        library_path: itl_copy.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };

    // The reporter flips the cancel flag at the first determinate item;
    // the executor checks it at the next FileOps checkpoint.
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let events = Mutex::new(Vec::<ProgressEvent>::new());
    let reporter = {
        let cancelled = std::sync::Arc::clone(&cancelled);
        move |event: ProgressEvent| {
            if matches!(&event, ProgressEvent::Item { total, .. } if *total > 0) {
                cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            events.lock().unwrap().push(event);
        }
    };
    let cancelled_flag = {
        let cancelled = std::sync::Arc::clone(&cancelled);
        move || cancelled.load(std::sync::atomic::Ordering::SeqCst)
    };
    let ctx = context_with(
        &device_root,
        "gate-cancel",
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &|_| false,
        &reporter,
        &cancelled_flag,
        &|_| None,
    );
    let result = run_sync(&ctx);
    let SyncStatus::Failed { step, diagnostic } = &result.status else {
        panic!("cancellation must fail the run: {:?}", result.status);
    };
    assert_eq!(*step, STEP_FILE_OPS);
    assert!(diagnostic.contains("cancelled"), "{diagnostic}");
    assert_eq!(
        result.recovery,
        Some(RecoveryState::Resumable {
            completed_steps: vec![STEP_INTENT, STEP_BACKUP, STEP_HOST_WRITE],
        }),
        "the cancelled run keeps its committed prefix resumable"
    );
    assert!(
        device_root
            .join("iPod_Control/iTunes/Play Counts")
            .is_file(),
        "the pending store is untouched by the cancelled run"
    );
    assert_eq!(
        std::fs::read(device_root.join("iPod_Control/Rufin/last-success.json")).unwrap(),
        seeded_marker,
        "cancellation never advances last-success"
    );
    assert!(
        ipod::DeviceSession::open(&backend, &device_root).is_ok(),
        "the device still parses after the cancellation"
    );
}
