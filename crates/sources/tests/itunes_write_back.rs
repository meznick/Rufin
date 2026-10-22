// Round-trip write-back test: plays a listen + rating against a COPY of
// the DEV library, flushes through `itl-sync`, and verifies the `.itl`
// facts change and stay stable across a refresh (no double-counting).
// Gated by `ITL_TEST_PATH` and the `itl-sync` binary (env `ITL_SYNC_BIN`,
// else the itl-player debug build).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use library::{Database, FavoriteTarget, ReadCancellation, source_entity_uri};
use sources::{Source, SourceId, SourceSetup};

fn test_library() -> Option<PathBuf> {
    std::env::var_os("ITL_TEST_PATH").map(PathBuf::from).or_else(|| {
        let default = PathBuf::from("/nvme2tb/iTunes DEV/iTunes Library.itl");
        default.exists().then_some(default)
    })
}

fn sync_binary() -> Option<PathBuf> {
    if let Some(binary) = std::env::var_os("ITL_SYNC_BIN") {
        return Some(PathBuf::from(binary));
    }
    let candidate = PathBuf::from(
        "/home/meznick/Documents/Projects/itl-player/target/debug/itl_sync",
    );
    candidate.exists().then_some(candidate)
}

#[tokio::test]
async fn itunes_write_back_updates_the_itl_and_refresh_does_not_double_count() {
    let Some(library) = test_library() else {
        eprintln!("ITL_TEST_PATH missing — skipping iTunes write-back integration test");
        return;
    };
    let Some(binary) = sync_binary() else {
        eprintln!("itl-sync binary not found — skipping iTunes write-back integration test");
        return;
    };
    // Work on a copy: the corpus .itl must never be written.
    let directory = tempfile::tempdir().unwrap();
    let library_dir = directory.path().join("iTunes DEV");
    std::fs::create_dir(&library_dir).unwrap();
    let copy = library_dir.join("iTunes Library.itl");
    std::fs::copy(&library, &copy).unwrap();

    let database = Database::open(directory.path().join("library.sqlite"))
        .await
        .unwrap();
    let connected = Source::connect(
        SourceId::new("itunes-test"),
        SourceSetup::Itunes {
            name: None,
            library: copy,
            media_folder: None,
            tz_offset_seconds: None,
        },
        "",
    )
    .await
    .unwrap();
    let (configuration, source, _credential) = connected.into_parts();
    database
        .reconcile_source(
            &SourceId::new("itunes-test"),
            sources::ITUNES_SOURCE_ID,
            "iTunes",
        )
        .await
        .unwrap();

    let cancelled = Arc::new(AtomicBool::new(false));
    source
        .acquire_catalog(&database, &|_| {}, Arc::clone(&cancelled), true)
        .await
        .unwrap();

    // The known corpus track: #515 (rating 100, 3 plays).
    let media_uri = source_entity_uri(&SourceId::new("itunes-test"), "track", "itunes:track:515");
    let state = database
        .itunes_player_state(&media_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(state.play_count, Some(3));
    assert_eq!(state.native_rating, Some(100));

    // User action: rate 8 stars, record a listen.
    database
        .set_rating(&FavoriteTarget::Track(media_uri.clone()), Some(8))
        .await
        .unwrap();
    database
        .record_native_listen(&media_uri, false, 1_800_000_000)
        .await
        .unwrap();
    let state = database
        .itunes_player_state(&media_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(state.play_count, Some(4), "native counter bumped");
    assert_eq!(state.user_rating, Some(80), "user rating stored");

    // Flush through itl-sync (the runtime service path).
    let config = sources::itunes::ItunesSourceConfig::from_configuration(&configuration).unwrap();
    let patch = sources::itunes::write::ItunesPatch {
        track_id: 515,
        rating: Some(80),
        play_count: Some(4),
        skip_count: None,
        play_date_unix: Some(1_800_000_000),
    };
    // The write path resolves the binary from ITL_SYNC_BIN; the test passes
    // it explicitly instead.
    sources::itunes::write::write_patches_with(&config, &[patch], &binary)
        .await
        .unwrap();

    // The .itl now holds the new values.
    let itl = itl_core::ItlFile::open(&config.library).unwrap();
    let track = itl.track_by_id(515).unwrap();
    assert_eq!(track.play_count(), 4);
    assert_eq!(track.rating(), 80);

    // Refresh: no double-counting — the itl value is authoritative and the
    // native slot matches it.
    source
        .acquire_catalog(&database, &|_| {}, Arc::clone(&cancelled), true)
        .await
        .unwrap();
    let state = database
        .itunes_player_state(&media_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(state.play_count, Some(4), "no double-count after refresh");
    assert_eq!(state.native_rating, Some(80));
    assert_eq!(state.user_rating, Some(80));
}
