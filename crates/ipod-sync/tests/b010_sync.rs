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

use ipod_sync::dry_run::{
    ArtworkProbe, CoverEvidence, DeviceOnlyPolicy, HostInputs, SourceKey, SyncOptions,
    collect_device, collect_host, compute,
};
use ipod_sync::sync::exec::{SyncContext, SyncStatus, run_sync};
use ipod_sync::sync::host::HostWriteContext;
use ipod_sync::sync::journal::{Journal, LAST_SUCCESS_FILE};
use ipod_sync::sync::merge::StatMerge;
use ipod_sync::sync::plan::ExecutionPlan;
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

fn context<'a>(
    device_root: &'a Path,
    backend: &'a ipod::IpodBackend,
    host: &'a HostInputs,
    options: &'a SyncOptions,
    host_write: &'a HostWriteContext,
    plan: &'a ExecutionPlan,
    fail_after: &'a dyn Fn(&str) -> bool,
) -> SyncContext<'a> {
    SyncContext {
        mount: device_root,
        backend,
        host,
        options,
        host_write,
        device_identity: "gate-device",
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
        artwork: &|_| None,
        cancelled: &|| false,
        fail_after,
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
    let pdfs = host
        .tracks
        .iter()
        .filter(|track| track.metadata.file_extension.as_deref() == Some("pdf"))
        .count();
    let expected_adds = plan.file_adds.len().saturating_sub(pdfs);
    let options = gate_options();
    let host_write = HostWriteContext {
        library_path: itl_copy.clone(),
        media_root: media_root.clone(),
        tz_offset_seconds: None,
    };

    let ctx = context(
        &device_root,
        &backend,
        &host,
        &options,
        &host_write,
        &plan,
        &|_| false,
    );
    let outcome = run_sync(&ctx).expect("the pair sync completes");
    assert_eq!(outcome.status, SyncStatus::Completed);
    assert_eq!(
        outcome.files_added, expected_adds,
        "all adds minus the PDF booklets: {:?}",
        outcome.diagnostics
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
    let seed = || {
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
    };
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
    seed();
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
        &backend,
        &host,
        &options,
        &bad_host_write,
        &plan,
        &|_| false,
    );
    let error = run_sync(&bad_context).expect_err("host refusal aborts");
    assert_eq!(error.step, "HostWrite", "abort at the host write: {error}");
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
    for injected in ipod_sync::sync::exec::STEPS {
        let injection = |step: &str| step == injected;
        let ctx = context(
            &device_root,
            &backend,
            &host,
            &options,
            &good_host_write,
            &plan,
            &injection,
        );
        let result = run_sync(&ctx);
        if injected == ipod_sync::sync::exec::STEP_VERIFY {
            // A crash after the committed DB write is recovered by the
            // planner's verify-and-advance route — that IS the designed
            // completion, and it finishes the operation.
            let outcome = result.expect("the post-DbWrite crash verify-advances to completion");
            assert_eq!(outcome.status, SyncStatus::Completed);
            assert!(
                outcome.diagnostics.iter().any(|d| d.contains("recovered")),
                "the verify-and-advance route reports the recovery: {:?}",
                outcome.diagnostics
            );
        } else {
            assert!(
                result.is_err(),
                "injection after {injected} must stop the run"
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
        &backend,
        &host,
        &options,
        &good_host_write,
        &plan,
        &|_| false,
    );
    let error = run_sync(&ctx).expect_err("a held device lock refuses");
    assert_eq!(error.step, "Intent");
    assert!(error.diagnostic.contains("lock"), "{error}");
    drop(lock_file);
}
