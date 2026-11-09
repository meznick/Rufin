//! Integration tests against a real pinned libgpod install.
//!
//! The backend is found like in production — `RUFIN_LIBGPOD_DIR` or the
//! loader's default paths — and every test **skips** when it is unavailable
//! (the repo's established pattern: `ITL_TEST_PATH`-style env gating, no CI
//! here). The write-path tests additionally need the `sync-writes` feature:
//!
//! ```text
//! RUFIN_LIBGPOD_DIR=/tmp/opencode/libgpod-install/lib \
//!   cargo test -p ipod --features sync-writes --test integration
//! ```
//!
//! Fixtures are built with libgpod's own `itdb_init_ipod` in tempdirs
//! ([`ipod::testkit`]) — the B-007 §1.1 methodology, now in the suite. No
//! test touches a live pod or any tree outside its own tempdir.
//!
//! The skip messages are the only `eprintln!` in the crate and are
//! diagnostics for why a gated test did not run, not print-debugging.
#![allow(clippy::print_stderr)]

use std::path::PathBuf;

/// Load the backend, or skip the test with an explanation.
fn backend_or_skip() -> Option<ipod::IpodBackend> {
    match ipod::IpodBackend::load() {
        Ok(backend) => Some(backend),
        Err(e) => {
            eprintln!(
                "skipping: backend unavailable ({e}); set RUFIN_LIBGPOD_DIR to a pinned install"
            );
            None
        }
    }
}

/// W-1: a parse failure is a structured error, never a panic or a fallback.
#[test]
fn parse_failure_is_structured() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let err = ipod::DeviceSession::open(&backend, dir.path()).unwrap_err();
    eprintln!("empty-tree parse error: {err}");
    match err {
        ipod::IpodError::GError { domain, .. } => {
            assert!(domain.contains("itdb"), "unexpected domain {domain}");
        }
        other => panic!("expected a structured GError, got {other}"),
    }
}

/// A fresh fixture: model resolved (MA446 → A446 / VIDEO_2), zero tracks,
/// the master playlist exists, and the artwork verdict is `Ok` (legitimately
/// artless — an ArtworkDB with zero declared entries).
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_fresh_device_reads() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();

    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let info = session.device_info().unwrap();
    let model = info.model.expect("model must resolve with seeded SysInfo");
    assert_eq!(model.model_number, "A446");
    assert_eq!(model.generation, 17, "VIDEO_2");
    assert_eq!(session.tracks().unwrap().len(), 0);

    let playlists = session.playlists().unwrap();
    assert!(
        playlists.iter().any(|p| p.is_master),
        "a fresh device has a master playlist"
    );

    let status = session.artwork_status().unwrap();
    assert_eq!(
        status.verdict,
        ipod::ArtworkVerdict::Ok,
        "fresh device is legitimately artless; diagnostic: {:?}",
        status.diagnostic
    );
    assert_eq!(status.declared_entries, Some(0));
}

/// W-3/W-4: with an empty `SysInfo` the model is unknown and the verdict is
/// `SkippedUnknownModel` even though parsing succeeds.
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_unknown_model_gates_artwork() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    // Empty SysInfo — exactly the restored-pod situation (iTunes 12.13
    // leaves it empty).
    std::fs::write(dir.path().join("iPod_Control/Device/SysInfo"), b"").unwrap();

    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let info = session.device_info().unwrap();
    assert!(
        info.model.is_none(),
        "empty SysInfo must not invent a model"
    );
    let status = session.artwork_status().unwrap();
    assert_eq!(status.verdict, ipod::ArtworkVerdict::SkippedUnknownModel);
    assert!(status.diagnostic.as_deref().unwrap().contains("model"));
    // The declared count is still reported: our parser does not need the
    // model gate.
    assert_eq!(status.declared_entries, Some(0));
}

/// B-005 plumbing: a seeded `SysInfoExtended` (`FireWireGUID` key) must
/// surface through the sysinfo hash as `FirewireGuid` — libgpod merges the
/// extended properties into the plain sysinfo at parse time, and the wrapper
/// surfaces the merged key via [`ipod::DeviceInfo::firewire_id`].
#[cfg(feature = "sync-writes")]
#[test]
fn firewire_guid_from_sysinfo_extended_surfaces() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();
    ipod::testkit::seed_sysinfo_extended(dir.path(), "0x000A270012345678").unwrap();

    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let info = session.device_info().unwrap();
    assert_eq!(info.firewire_id(), Some("0x000A270012345678"));
    // `itdb_device_get_uuid` quirk: it returns the sysinfo hash's
    // FirewireGuid entry (borrowed) — so uuid and firewire_id agree here.
    assert_eq!(info.uuid.as_deref(), Some("0x000A270012345678"));
}

/// The full write round-trip through libgpod's own writer, then a reparse
/// that must see exactly what was written — including the artwork status
/// `Ok` with our independent ArtworkDB count and the ithmb proof-read.
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_track_artwork_round_trip() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();

    let source = dir.path().join("song.mp3");
    std::fs::write(&source, b"fake audio bytes").unwrap();

    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let dbid = session
        .create_track(&ipod::TrackMetadata {
            title: Some("Тест трек".to_string()),
            artist: Some("Тест артист".to_string()),
            album: Some("Тест альбом".to_string()),
            filetype: Some("mp3".to_string()),
            size: 16,
            tracklen: 1000,
            rating: 80,
            ..Default::default()
        })
        .unwrap();
    assert_ne!(dbid, 0, "itdb_track_add assigns a unique dbid");
    session.copy_track(dbid, &source).unwrap();
    session
        .set_artwork(
            dbid,
            &ipod::ArtworkInput {
                encoded: ipod::testkit::TINY_PNG.to_vec(),
                rotation: 0,
            },
        )
        .unwrap();
    session.write_library().unwrap();
    drop(session);

    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let tracks = session.tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    let track = &tracks[0];
    assert_eq!(track.dbid, dbid, "dbid is stable across the write");
    assert_eq!(track.title.as_deref(), Some("Тест трек"), "UTF-8 survives");
    assert_eq!(track.rating, 80);
    assert_eq!(track.size, 16);
    assert!(track.has_thumbnail, "artwork must attach on reparse");

    let stats = session.playback_stats().unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].rating, 80);

    let status = session.artwork_status().unwrap();
    assert_eq!(
        status.verdict,
        ipod::ArtworkVerdict::Ok,
        "diagnostic: {:?}",
        status.diagnostic
    );
    assert_eq!(status.declared_entries, Some(1));
    assert_eq!(status.tracks_with_thumbnails, 1);
    let proof = status.proof_read.expect("proof-read must run");
    assert!(
        !proof.formats_checked.is_empty(),
        "at least one format must be decoded"
    );
    assert!(proof.failed.is_none(), "{:?}", proof.failed);
    drop(session);

    // update + remove on the same fixture.
    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    session
        .update_track(
            dbid,
            &ipod::TrackMetadata {
                title: Some("Переименован".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    session.write_library().unwrap();
    let tracks = session.tracks().unwrap();
    assert_eq!(tracks[0].title.as_deref(), Some("Переименован"));
    session.remove_track(dbid).unwrap();
    session.write_library().unwrap();
    assert_eq!(session.tracks().unwrap().len(), 0);
    // The media file is deliberately left on the pod by remove_track (see
    // the method docs) — exactly one file under iPod_Control/Music.
    let music_files: Vec<_> = walkdir::WalkDir::new(dir.path().join("iPod_Control/Music"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();
    assert_eq!(
        music_files.len(),
        1,
        "copied media file must survive remove_track"
    );
}

/// The `remove_track` detach guarantee: a track that IS a member of the
/// master playlist must be removed from every playlist before it is freed,
/// or the next `write_library` dereferences a dangling `pl->members` entry
/// (upstream `itdb_track_unlink` deliberately leaves memberships alone).
/// Regression test for the review finding — the round-trip above passes
/// even with the bug because its fixture track is in no playlist.
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_remove_track_detaches_from_playlists() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();

    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let dbid = session
        .create_track(&ipod::TrackMetadata {
            title: Some("mpl member".to_string()),
            ..Default::default()
        })
        .unwrap();
    let master_id = session
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.is_master)
        .expect("a fresh device has a master playlist")
        .id;
    session
        .update_playlists(&ipod::PlaylistChanges {
            additions: vec![(master_id, vec![dbid])],
        })
        .unwrap();
    session.write_library().unwrap();
    drop(session);

    // The track must actually be an MPL member before the removal (this is
    // what a real device looks like — the MPL lists every track).
    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let master = session
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.is_master)
        .unwrap();
    assert_eq!(
        master.member_dbids,
        vec![dbid],
        "fixture must seat the track in the MPL"
    );
    drop(session);

    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    session.remove_track(dbid).unwrap();
    session.write_library().unwrap();
    drop(session);

    // Reparse: no tracks, and no playlist (incl. the MPL) may carry the
    // removed dbid — with the bug, the writer would have serialized a
    // dangling member and the parse would fail or show a bogus entry.
    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    assert_eq!(session.tracks().unwrap().len(), 0);
    for playlist in session.playlists().unwrap() {
        assert!(
            !playlist.member_dbids.contains(&dbid),
            "playlist {:?} still references the removed track",
            playlist.name
        );
    }
}

/// W-5: every read-side call leaves the device tree content+structure
/// identical (tree hash before/after).
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_reads_are_non_mutating() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();
    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let dbid = session
        .create_track(&ipod::TrackMetadata {
            title: Some("t".to_string()),
            ..Default::default()
        })
        .unwrap();
    session
        .set_artwork(
            dbid,
            &ipod::ArtworkInput {
                encoded: ipod::testkit::TINY_PNG.to_vec(),
                rotation: 0,
            },
        )
        .unwrap();
    session.write_library().unwrap();
    drop(session);

    let before = ipod::tree_hash(dir.path()).unwrap();
    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    session.device_info().unwrap();
    session.tracks().unwrap();
    session.playlists().unwrap();
    session.playback_stats().unwrap();
    session.artwork_status().unwrap();
    drop(session);
    let after = ipod::tree_hash(dir.path()).unwrap();
    assert_eq!(before, after, "read calls must not mutate the tree");
}

/// The restored-device snapshot (env `IPOD_TEST_TREE` = mount root with
/// `iPod_Control/`): the research numbers from the B-007 cross-checks.
/// 3373 tracks, 9 playlists, ArtworkDB declares 3356 entries; with a seeded
/// `SysInfo` the verdict is `Ok` with 3356 parsed thumbnails.
#[test]
fn snapshot_reads_match_research_numbers() {
    let Some(root) = std::env::var_os("IPOD_TEST_TREE") else {
        eprintln!("skipping: IPOD_TEST_TREE unset");
        return;
    };
    let root = PathBuf::from(root);
    if !root.join("iPod_Control").is_dir() {
        eprintln!("skipping: {} has no iPod_Control/", root.display());
        return;
    }
    let Some(backend) = backend_or_skip() else {
        return;
    };

    let session = ipod::DeviceSession::open(&backend, &root).unwrap();
    assert_eq!(session.tracks().unwrap().len(), 3373);
    assert_eq!(session.playlists().unwrap().len(), 9);
    let status = session.artwork_status().unwrap();
    assert_eq!(status.declared_entries, Some(3356));
    if status.verdict == ipod::ArtworkVerdict::Ok {
        assert_eq!(
            status.tracks_with_thumbnails, 3356,
            "diagnostic: {:?}",
            status.diagnostic
        );
    } else {
        // Unseeded snapshot: model unknown, artwork skipped by libgpod —
        // exactly the §1.2 situation the verdict exists for.
        assert_eq!(
            status.verdict,
            ipod::ArtworkVerdict::SkippedUnknownModel,
            "diagnostic: {:?}",
            status.diagnostic
        );
        assert_eq!(status.tracks_with_thumbnails, 0);
    }
}

/// W-4 acceptance: with the ArtworkDB removed, `artwork_status()` reports
/// `Missing` while track parsing still succeeds. Then, with a corrupt
/// ArtworkDB in its place, again `Missing` while tracks still parse — the
/// §6 "missing/corrupt ArtworkDB" fixture coverage.
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_artworkdb_missing_or_corrupt_reports_missing() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();
    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let dbid = session
        .create_track(&ipod::TrackMetadata {
            title: Some("track with art".to_string()),
            ..Default::default()
        })
        .unwrap();
    session
        .set_artwork(
            dbid,
            &ipod::ArtworkInput {
                encoded: ipod::testkit::TINY_PNG.to_vec(),
                rotation: 0,
            },
        )
        .unwrap();
    session.write_library().unwrap();
    drop(session);
    let artdb = dir.path().join("iPod_Control/Artwork/ArtworkDB");
    assert!(artdb.exists(), "the write must have produced an ArtworkDB");

    // Phase 1: ArtworkDB removed.
    std::fs::remove_file(&artdb).unwrap();
    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    assert_eq!(
        session.tracks().unwrap().len(),
        1,
        "track parsing must still succeed without ArtworkDB"
    );
    let status = session.artwork_status().unwrap();
    assert_eq!(
        status.verdict,
        ipod::ArtworkVerdict::Missing,
        "diagnostic: {:?}",
        status.diagnostic
    );
    assert_eq!(status.declared_entries, None);
    drop(session);

    // Phase 2: corrupt ArtworkDB in its place (libgpod ignores the artwork
    // parser's failure, so `itdb_parse` succeeds — the verdict must not).
    std::fs::write(&artdb, b"not an mhfd container").unwrap();
    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    assert_eq!(session.tracks().unwrap().len(), 1, "tracks still parse");
    let status = session.artwork_status().unwrap();
    assert_eq!(
        status.verdict,
        ipod::ArtworkVerdict::Missing,
        "diagnostic: {:?}",
        status.diagnostic
    );
    assert!(status.diagnostic.as_deref().unwrap().contains("parse"));
}

/// §6 "removed `.ithmb`": the ArtworkDB stays intact, the banks vanish. Per
/// §1.2 the parse can still report an attached thumbnail, so the counts may
/// match — the proof-read is what must fail and force `Missing`.
#[cfg(feature = "sync-writes")]
#[test]
fn fixture_ithmb_removed_reports_missing() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
    ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();
    let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    let dbid = session
        .create_track(&ipod::TrackMetadata {
            title: Some("track with art".to_string()),
            ..Default::default()
        })
        .unwrap();
    session
        .set_artwork(
            dbid,
            &ipod::ArtworkInput {
                encoded: ipod::testkit::TINY_PNG.to_vec(),
                rotation: 0,
            },
        )
        .unwrap();
    session.write_library().unwrap();
    drop(session);

    let artwork_dir = dir.path().join("iPod_Control/Artwork");
    let banks: Vec<_> = std::fs::read_dir(&artwork_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ithmb"))
        .collect();
    assert!(
        !banks.is_empty(),
        "the write must have produced ithmb banks"
    );
    for bank in &banks {
        std::fs::remove_file(bank).unwrap();
    }

    let session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
    assert_eq!(session.tracks().unwrap().len(), 1, "tracks still parse");
    let status = session.artwork_status().unwrap();
    assert_eq!(
        status.verdict,
        ipod::ArtworkVerdict::Missing,
        "diagnostic: {:?}",
        status.diagnostic
    );
    assert_eq!(status.declared_entries, Some(1));
    if status.tracks_with_thumbnails == 1 {
        // Counts match, so the proof-read must be the failing step.
        let proof = status.proof_read.expect("proof-read must have run");
        assert!(proof.failed.is_some(), "{:?}", proof);
    }
}

/// W-6: concurrent sessions across threads run serialized through the native
/// lock. Each thread builds its own disposable fixture and opens its own
/// session (sessions are `!Send` by design), sharing only the backend. If the
/// lock were missing or the session leaked native state across threads, this
/// test would fail or crash; a deadlock would hang it.
#[cfg(feature = "sync-writes")]
#[test]
fn concurrent_sessions_are_serialized() {
    // The backend must be shareable across threads (W-6's contract for the
    // B-003 consumer that will hand it to `spawn_blocking`).
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ipod::IpodBackend>();

    let Some(backend) = backend_or_skip() else {
        return;
    };
    let backend = std::sync::Arc::new(backend);
    let mut workers = Vec::new();
    for i in 0..4 {
        let backend = backend.clone();
        workers.push(std::thread::spawn(move || {
            let dir = tempfile::tempdir().unwrap();
            ipod::testkit::init_device(&backend, dir.path(), "MA446", "fixture-pod").unwrap();
            ipod::testkit::seed_sysinfo(dir.path(), "MA446").unwrap();
            for round in 0..3 {
                let mut session = ipod::DeviceSession::open(&backend, dir.path()).unwrap();
                if round == 0 {
                    let dbid = session
                        .create_track(&ipod::TrackMetadata {
                            title: Some(format!("worker {i}")),
                            ..Default::default()
                        })
                        .unwrap();
                    assert_ne!(dbid, 0);
                    session.write_library().unwrap();
                }
                session.device_info().unwrap();
                assert_eq!(session.tracks().unwrap().len(), 1);
                session.playlists().unwrap();
                session.playback_stats().unwrap();
                session.artwork_status().unwrap();
            }
        }));
    }
    for worker in workers {
        worker.join().expect("worker thread panicked");
    }
}
