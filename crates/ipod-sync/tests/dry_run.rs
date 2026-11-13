//! B-009: the dry-run gate — env-gated, disposable corpora only
//! (`deltas/b009_plan.md` §6).
//!
//! Env: `ITL_TEST_PATH` (a disposable `.itl` copy), `IPOD_TEST_TREE` (the
//! restored-device snapshot tree), `RUFIN_LIBGPOD_DIR` (the pinned
//! libgpod), and optionally `ITL_TEST_MEDIA_ROOT` (the local root the
//! `.itl` media-folder mapping resolves against; defaults to the parent of
//! `ITL_TEST_PATH`). The gate copies the device tree to a temp dir and
//! runs the full dry run against the copies, then proves zero mutation:
//! before/after tree hash of the device copy and before/after byte hash
//! of the `.itl` copy.
//!
//! Pair expectation (accuracy smoke): the snapshot pair is an in-sync
//! iTunes dump, so the report is a near no-op — zero re-copies, zero
//! repairs (the snapshot tree holds no `Music/` media, so the probe map
//! is cleared for this check — documented below), zero orphan removals,
//! zero criteria differences after flattening, zero member diffs on the
//! flat playlists, and only the known benign divergences.

use std::path::{Path, PathBuf};

use ipod_sync::dry_run::{ArtworkProbe, CoverEvidence, DeviceOnlyPolicy, SourceKey, SyncOptions};

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

#[test]
fn dry_run_pair_gate() {
    let Some(itl_path) = std::env::var_os("ITL_TEST_PATH").map(PathBuf::from) else {
        eprintln!(
            "skipping: ITL_TEST_PATH unset (point it at a disposable .itl copy; \
             see scripts/build-libgpod.sh for the backend)"
        );
        return;
    };
    let Some(tree) = std::env::var_os("IPOD_TEST_TREE").map(PathBuf::from) else {
        eprintln!("skipping: IPOD_TEST_TREE unset (the restored-device snapshot tree)");
        return;
    };
    if !tree.join("iPod_Control").is_dir() {
        eprintln!("skipping: {} has no iPod_Control/", tree.display());
        return;
    }
    let Ok(backend) = ipod::IpodBackend::load() else {
        eprintln!("skipping: backend unavailable (see scripts/build-libgpod.sh)");
        return;
    };

    // Disposable device copy — the session opens against it, never the
    // corpus tree itself.
    let workdir = tempfile::tempdir().unwrap();
    let device_root = workdir.path().join("device");
    copy_tree(&tree, &device_root);
    let device_before = ipod::treehash::tree_hash(&device_root).unwrap();
    let itl_before = std::fs::read(&itl_path).unwrap();

    // Host inputs. The media root defaults to the .itl copy's parent;
    // ITL_TEST_MEDIA_ROOT points the mapping at the real snapshot media
    // files so the artwork probe and file-existence checks see real data.
    let media_root = std::env::var_os("ITL_TEST_MEDIA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| itl_path.parent().unwrap().to_path_buf());
    let itl = itl_core::ItlFile::open(&itl_path).unwrap();
    let host = ipod_sync::dry_run::snapshot::collect_host(&itl, &media_root, &LoftyProbe);

    // Device inputs from the disposable copy.
    let session = ipod::DeviceSession::open(&backend, &device_root).unwrap();
    let facts = ipod_sync::retrieval::collect(&session, &device_root).unwrap();
    let mut device =
        ipod_sync::dry_run::snapshot::collect_device(&session, &device_root, &facts).unwrap();
    // The snapshot tree holds the databases and artwork but no `Music/`
    // media (the 30 GB image was extracted without the audio files), so
    // every on-pod file probe would report "missing" and flood repairs.
    // The probe itself is unit-tested with temp trees; for the pair
    // expectation the device files are treated as present.
    device.device_file_present.clear();

    let options = SyncOptions {
        source_key: SourceKey("itunes".to_string()),
        transcode_aac: true,
        device_only: DeviceOnlyPolicy::Remove,
    };
    let report = ipod_sync::dry_run::compute(&host, &device, &options);

    // ---- Zero-mutation proof -------------------------------------------------
    assert_eq!(
        ipod::treehash::tree_hash(&device_root).unwrap(),
        device_before,
        "the device tree must be byte-identical after a dry run"
    );
    assert_eq!(
        std::fs::read(&itl_path).unwrap(),
        itl_before,
        "the host .itl must be byte-identical after a dry run"
    );

    // ---- Pair expectations (accuracy smoke) ----------------------------------
    // The pair is in sync on *identity* (B-008: 3373/3373 resolve, 0 orphan
    // dbids) but not perfectly on files: 11 host tracks are legitimately
    // missing from the device dump, measured and pinned:
    //  - 8 added 2026-11-10 08:59 (#20454..#20477), after the device dump
    //    was written — genuine same-day staleness of the pair;
    //  - 2 Digital Booklets (#6877, #7365) — PDFs, which iTunes never
    //    copies to the pod (a sync-policy exception for B-010, like the
    //    playlist selection);
    //  - 2 Die Antwoord tracks (#7233, #7237) — local files with plays,
    //    absent from the dump; same evidence gap as the 8 above.
    let expected_adds: [u32; 11] = [
        6877, 7233, 7237, 7365, 20454, 20459, 20462, 20465, 20471, 20474, 20477,
    ];
    let adds: Vec<u32> = report.file_adds.iter().map(|a| a.itl_id).collect();
    assert_eq!(
        adds, expected_adds,
        "the pinned 11 host tracks missing from the device dump"
    );
    assert_eq!(report.file_repairs.len(), 0, "in-sync pair: no repairs");
    assert_eq!(
        report.file_removes.len(),
        0,
        "in-sync pair: no orphan dbids to remove (B-008: 0 device-only)"
    );

    // Criteria: the three surfaced smart playlists must compare Equal after
    // flattening (the flattening ground-truth assertion).
    let criteria_states: Vec<_> = report
        .playlist_updates
        .iter()
        .map(|u| &u.criteria)
        .collect();
    assert!(
        criteria_states
            .iter()
            .all(|c| **c != ipod_sync::dry_run::CriteriaState::Differ),
        "no criteria differences on an in-sync pair: {criteria_states:?}"
    );
    assert!(
        criteria_states
            .iter()
            .all(|c| **c != ipod_sync::dry_run::CriteriaState::Unknown),
        "no unknown criteria on the flat playlists: {criteria_states:?}"
    );

    // Stat diffs: the engine compares every matched pair (a few more than
    // B-008's 3227 "unambiguous" set), so the gate pins the pair's own
    // measured counts (2026-11-13 run: rating 235, play_count 77,
    // skip_count 934, play_date 83, volume 0) with ~5% headroom. A wrong
    // comparison (e.g. every track diffing) lights up thousands and fails.
    let counts = report.stat_counts;
    assert!(
        counts.rating <= 250,
        "rating diffs {counts:?} exceed the pinned range"
    );
    assert!(
        counts.play_count <= 90,
        "play-count diffs {counts:?} exceed the pinned range"
    );
    assert!(
        counts.skip_count <= 950,
        "skip-count diffs {counts:?} exceed the pinned range"
    );
    assert!(
        counts.play_date <= 100,
        "play-date diffs {counts:?} exceed the pinned range"
    );
    assert_eq!(
        counts.volume_adjustment, 0,
        "volume adjustment matches on this pair"
    );

    // Device-owned playlists: the master is exempt in every policy. The
    // active OTG queue would be too (libgpod-injected `OTG Playlist N`),
    // but this dump carries no `OTGPlaylistInfo` file. `On-The-Go 1` is a
    // *saved* OTG playlist — a regular playlist iTunes treats like any
    // other — so the host mirrors it and it matches by name+kind; nothing
    // device-only remains, zero removals, zero device-local entries.
    assert!(report.context.device_master.is_some(), "master present");
    assert_eq!(
        report.context.device_local_playlists.len(),
        0,
        "no active OTG queue on this pair: {:?}",
        report.context.device_local_playlists
    );
    assert_eq!(
        report.playlist_removes.len(),
        0,
        "an in-sync iTunes dump has no non-local device-only playlists: {:?}",
        report.playlist_removes
    );

    // Member diffs: static (user) playlists are the truth — membership
    // must be equal on an in-sync pair. Smart playlists are caches of a
    // criteria evaluation (B-021's domain): «Последние добавленные»
    // ("in the last 7 days") and «Хай рейт» (rating-based) materialized
    // their lists at different moments on each side, so small member
    // diffs there are expected staleness — the dry run reporting them is
    // correct. The gate bounds the drift to the measured scale.
    for update in &report.playlist_updates {
        let drift = update.members_add.len() + update.members_remove.len();
        eprintln!(
            "gate note: playlist update {:?}: +{} -{} drift {drift} criteria {:?}",
            update.name,
            update.members_add.len(),
            update.members_remove.len(),
            update.criteria
        );
        if update.is_spl {
            assert!(
                drift <= 50,
                "playlist {:?}: smart-list staleness drift {drift} exceeds the measured scale",
                update.name
            );
        } else {
            assert_eq!(
                drift, 0,
                "playlist {:?}: unexpected member diff on the in-sync pair",
                update.name
            );
        }
    }

    // Artwork: adds only where the host carries an embedded cover and the
    // device lacks a thumbnail; removals only where the host is artless
    // and the device carries a thumbnail (the iTunes art-cache caveat).
    let device_tracks: std::collections::HashMap<u64, bool> = device
        .tracks
        .iter()
        .map(|t| (t.dbid, t.has_thumbnail))
        .collect();
    let mut expected_adds = 0u32;
    let mut expected_removes = 0u32;
    for track in &host.tracks {
        let Some(evidence) = host.artwork.get(&track.track_pid) else {
            continue;
        };
        let Some(has_thumb) = device_tracks.get(&track.track_pid) else {
            continue;
        };
        match evidence {
            CoverEvidence::Present { .. } if !has_thumb => expected_adds += 1,
            CoverEvidence::Absent if *has_thumb => expected_removes += 1,
            _ => {}
        }
    }
    assert_eq!(
        report.artwork_adds.len() as u32,
        expected_adds,
        "artwork adds must equal the computed host-cover ∩ no-thumbnail set"
    );
    assert_eq!(
        report.artwork_removes.len() as u32,
        expected_removes,
        "artwork removals must equal the computed host-artless ∩ thumbnail set"
    );

    // Capacity: the estimate must be exactly the sum of the 11 pinned
    // adds (transcode formula, unit-tested) plus repairs (none), and the
    // verdict must follow the comparison — a consistency check on the
    // summary fields.
    let expected_estimate: u64 = report
        .file_adds
        .iter()
        .map(|a| a.size_estimate)
        .chain(report.file_repairs.iter().map(|r| r.size_estimate))
        .sum();
    assert_eq!(report.capacity.estimate_bytes, expected_estimate);
    assert_eq!(
        report.capacity.would_fit,
        report.capacity.estimate_bytes <= report.capacity.free_bytes
    );
    eprintln!(
        "gate note: capacity estimate {} of {} free",
        report.capacity.estimate_bytes, report.capacity.free_bytes
    );

    eprintln!(
        "gate note: library {} — {} stat diffs ({:?}), {} creates, {} updates, \
         {} artwork adds, {} artwork removals, {} artwork unknown, {} diagnostics",
        report.context.library_pid,
        report.stat_diffs.len(),
        counts,
        report.playlist_creates.len(),
        report.playlist_updates.len(),
        report.artwork_adds.len(),
        report.artwork_removes.len(),
        report.artwork_unknown,
        report.diagnostics.len()
    );
    for create in &report.playlist_creates {
        eprintln!(
            "gate note: planned playlist create {:?} (kind {:?})",
            create.name, create.kind
        );
    }
}
