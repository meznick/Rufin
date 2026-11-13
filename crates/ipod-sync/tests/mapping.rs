//! B-008 env-gated mapping tests: the host adapter over a disposable `.itl`
//! copy (`ITL_TEST_PATH`, the repo's established pattern — never the live
//! library), and the full host↔device mapping over the restored-device
//! snapshot (`IPOD_TEST_TREE`) with the pinned backend
//! (`RUFIN_LIBGPOD_DIR`). Unset env vars skip with an explanation; a
//! set-but-broken backend panics (the suite convention).

use std::path::{Path, PathBuf};

use ipod_sync::mapping::{
    HostLibrary, host_library, host_playlists, host_tracks, map_playlists, map_tracks,
};

/// Load a disposable `.itl` copy, or skip.
fn itl_or_skip() -> Option<itl_core::ItlFile> {
    let Some(path) = std::env::var_os("ITL_TEST_PATH") else {
        eprintln!("skipping: ITL_TEST_PATH unset (a disposable .itl copy)");
        return None;
    };
    let path = PathBuf::from(path);
    let itl = itl_core::ItlFile::open(&path).unwrap_or_else(|e| {
        panic!("ITL_TEST_PATH is set but cannot be opened: {e}");
    });
    Some(itl)
}

/// The host adapter must build owned records with sane identity and stats:
/// library PID nonzero, track PIDs mostly nonzero (the verified @0x80
/// chain), locations mapped, and modern smart playlists detected with
/// parseable rules (the 0x65/0x66 form).
#[test]
fn env_gated_host_records_carry_verified_identity_and_smart_rules() {
    let Some(itl) = itl_or_skip() else {
        return;
    };
    let root = std::env::var_os("ITL_TEST_PATH")
        .map(PathBuf::from)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let library: HostLibrary =
        host_library(&itl, &root).expect("the corpus must have a media-folder mapping");
    assert_ne!(library.library_pid, 0);

    let mapping = itl_sync::paths::MediaMapping::new(&itl, &root).unwrap();
    let tracks = host_tracks(&itl, &mapping, library.library_pid);
    assert!(!tracks.is_empty(), "the corpus has no tracks");
    let with_pid = tracks.iter().filter(|t| t.track_pid != 0).count();
    assert!(
        with_pid * 100 / tracks.len() >= 95,
        "track PIDs mostly missing: {with_pid}/{}",
        tracks.len()
    );
    let mapped = tracks
        .iter()
        .filter(|t| matches!(t.location, ipod_sync::mapping::HostLocation::Mapped(_)))
        .count();
    assert!(
        mapped * 100 / tracks.len() >= 90,
        "locations mostly unmapped: {mapped}/{}",
        tracks.len()
    );
    assert!(
        tracks
            .iter()
            .any(|t| t.stats.rating != 0 || t.stats.play_count != 0),
        "the corpus must carry some statistics"
    );

    let playlists = host_playlists(&itl);
    assert!(playlists.first().is_some_and(|p| p.is_master_candidate));
    let smart: Vec<_> = playlists.iter().filter(|p| p.smart.is_some()).collect();
    eprintln!(
        "note: {} playlists, {} smart, {} with parsed rules",
        playlists.len(),
        smart.len(),
        smart
            .iter()
            .filter(|p| p
                .smart
                .as_ref()
                .and_then(|s| s.rules.as_ref())
                .is_some_and(|r| r.is_ok()))
            .count()
    );
    for playlist in &smart {
        let rules = playlist
            .smart
            .as_ref()
            .and_then(|s| s.rules.as_ref())
            .expect("a smart playlist detected by rules/info must carry the rules field");
        rules
            .as_ref()
            .unwrap_or_else(|e| panic!("smart rules of {:?} must parse: {e}", playlist.name));
    }
}

/// The full pair: every device track must resolve to a host PID, no device
/// dbid may be orphaned, and the device master must be reported as
/// device-owned. Env-gated on the restored-device snapshot.
#[test]
fn env_gated_snapshot_pair_maps_tracks_and_playlists() {
    let Some(itl) = itl_or_skip() else {
        return;
    };
    let Some(root) = std::env::var_os("IPOD_TEST_TREE") else {
        eprintln!("skipping: IPOD_TEST_TREE unset (the restored-device snapshot)");
        return;
    };
    let root = PathBuf::from(root);
    if !root.join("iPod_Control").is_dir() {
        eprintln!("skipping: {} has no iPod_Control/", root.display());
        return;
    }
    let Some(dir) = std::env::var_os("RUFIN_LIBGPOD_DIR") else {
        eprintln!(
            "skipping: RUFIN_LIBGPOD_DIR unset — the pinned libgpod install is missing; \
             build it with scripts/build-libgpod.sh"
        );
        return;
    };
    let backend = ipod::IpodBackend::load_with(ipod::BackendSearch::Explicit(PathBuf::from(dir)))
        .expect("RUFIN_LIBGPOD_DIR is set but the backend did not load");

    let local_root = std::env::var_os("ITL_TEST_PATH")
        .map(PathBuf::from)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let library =
        host_library(&itl, &local_root).expect("the corpus must have a media-folder mapping");
    let media = itl_sync::paths::MediaMapping::new(&itl, &local_root).unwrap();
    let host = host_tracks(&itl, &media, library.library_pid);
    let host_pids: std::collections::HashSet<u64> = host
        .iter()
        .map(|t| t.track_pid)
        .filter(|p| *p != 0)
        .collect();

    let session = ipod::DeviceSession::open(&backend, &root).unwrap();
    let device_tracks = session.tracks().unwrap();
    let track_map = map_tracks(&host, &device_tracks);
    assert!(
        track_map.device_only_dbids.is_empty(),
        "every device dbid must resolve to a host PID; orphans: {:?}",
        track_map.device_only_dbids
    );
    let matched = track_map.matched().count();
    assert!(
        matched as u64 == device_tracks.len() as u64 && !device_tracks.is_empty(),
        "matched {matched} of {} device tracks",
        device_tracks.len()
    );
    let device_pids: std::collections::HashSet<u64> =
        device_tracks.iter().map(|t| t.dbid).collect();
    assert_eq!(
        device_pids.len(),
        device_tracks.len(),
        "dbids must be unique"
    );
    let missing = host
        .iter()
        .filter(|t| t.track_pid != 0 && !device_pids.contains(&t.track_pid))
        .count();
    eprintln!("note: {missing} host tracks have no device counterpart (cloud/podcasts)");
    assert!(
        missing < host_pids.len() / 2,
        "implausibly many host tracks missing"
    );

    let device_playlists = session.playlists().unwrap();
    let playlist_map = map_playlists(&host_playlists(&itl), &device_playlists);
    assert!(
        playlist_map.device_master.is_some(),
        "the snapshot must report its device master playlist"
    );

    // Artwork evidence on the device side: the seeded snapshot declares
    // 3356 ArtworkDB entries and the wrapper parses exactly that many
    // thumbnails (verdict Ok). The host-side artwork reference is the
    // mapped media file itself (embedded covers — the sources layer's
    // extraction); this asserts the device half of the artwork mapping.
    let artwork = session.artwork_status().unwrap();
    assert_eq!(
        artwork.verdict,
        ipod::ArtworkVerdict::Ok,
        "diagnostic: {:?}",
        artwork.diagnostic
    );
    assert_eq!(artwork.declared_entries, Some(3356));
    assert_eq!(artwork.tracks_with_thumbnails, 3356);
    let with_thumbnail = device_tracks.iter().filter(|t| t.has_thumbnail).count();
    assert_eq!(
        with_thumbnail, 3356,
        "per-track thumbnail flags must agree with the artwork verdict"
    );

    // Smart evaluation cross-check: the criteria are the truth and must
    // reproduce the device's materialized member lists on in-sync data.
    // The device facts (rating/playcount/mediatype) come from the parsed
    // iTunesDB; the criteria come from the host .itl (byte-identical to
    // the device rules for the flat playlists). «Хай рейт» (rating > 3.5★)
    // and «Самые популярные» (plays > 2) are time-independent.
    let host_playlists = host_playlists(&itl);
    let by_name: std::collections::HashMap<&str, &ipod_sync::mapping::HostPlaylist> =
        host_playlists
            .iter()
            .filter_map(|p| p.name.as_deref().map(|name| (name, p)))
            .collect();
    for (name, _) in [("Хай рейт", 0), ("Самые популярные", 0)] {
        let Some(host_pl) = by_name.get(name).copied() else {
            eprintln!("note: playlist {name} not in the corpus");
            continue;
        };
        let Some(device_pl) = device_playlists
            .iter()
            .find(|d| d.name.as_deref() == Some(name))
        else {
            eprintln!("note: playlist {name} not on the device");
            continue;
        };
        let Some(rules) = host_pl
            .smart
            .as_ref()
            .and_then(|s| s.rules.as_ref())
            .and_then(|r| r.as_ref().ok())
        else {
            panic!("{name} must carry parseable rules");
        };
        let member_set: std::collections::HashSet<u64> =
            device_pl.member_dbids.iter().copied().collect();
        let facts_by_dbid: std::collections::HashMap<u64, ipod_sync::mapping::TrackFacts> =
            device_tracks
                .iter()
                .map(|t| {
                    (
                        t.dbid,
                        ipod_sync::mapping::TrackFacts {
                            rating: t.rating.min(100) as u8,
                            play_count: t.playcount,
                            skip_count: t.skipcount,
                            date_added: t.time_added.max(0) as u32,
                            last_played: t.time_played.max(0) as u32,
                            media_kind_bits: Some(t.mediatype),
                            purchase_status: None,
                            now: 0,
                        },
                    )
                })
                .collect();
        let evaluated: std::collections::HashSet<u64> = facts_by_dbid
            .iter()
            .filter(|(_, facts)| {
                ipod_sync::mapping::eval_rules(rules, facts) == ipod_sync::mapping::RuleMatch::True
            })
            .map(|(dbid, _)| *dbid)
            .collect();
        assert!(
            !member_set.is_empty(),
            "{name}: the device playlist has no members"
        );
        assert!(
            !evaluated.is_empty(),
            "{name}: the evaluation matched nothing"
        );
        let only_members = member_set.difference(&evaluated).count();
        let only_evaluated = evaluated.difference(&member_set).count();
        // Directional, because materialization is a cache: the stored
        // members are what the criteria matched when iTunes last computed
        // them, and the same snapshot's facts are frozen at that write, so
        // (almost) every stored member must satisfy the criteria — the
        // small exception is iTunes' raw-rating fill-down writes. A wrong
        // evaluator would flag whole categories here. The reverse gap
        // (matching tracks missing from the list) is unbounded by design:
        // it is exactly the staleness the B-021 backlog records — criteria
        // edited after the last materialization, e.g. «Самые популярные»
        // changed from plays>15 to plays>2 but the stored list still has
        // the old 25; measured 1910 tracks missing on this snapshot.
        assert!(
            only_members <= 20,
            "{name}: {only_members} stored members fail the criteria on their own \
             snapshot's facts — expected near zero (raw-rating fill-down tolerance)",
        );
        eprintln!(
            "note: {name} — members {members}, evaluated {evaluated}, members failing \
             criteria {only_members}, criteria matches missing from the list {only_evaluated}",
            members = member_set.len(),
            evaluated = evaluated.len(),
        );
    }
    eprintln!(
        "note: {} playlist matches, {} device-only, {} host missing",
        playlist_map.matches.len(),
        playlist_map.device_only.len(),
        playlist_map
            .matches
            .iter()
            .filter(|m| m.outcome == ipod_sync::mapping::PlaylistOutcome::MissingOnDevice)
            .count()
    );
}
