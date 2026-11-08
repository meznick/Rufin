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
