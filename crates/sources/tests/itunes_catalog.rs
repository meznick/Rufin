// Integration test for the iTunes source read path, gated by
// `ITL_TEST_PATH` (defaults to the known-good DEV library; skipped when
// the file is absent). This is the same corpus the itl-player golden tests
// use — the test asserts the catalog mirrors the `.itl` facts.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use library::{Database, ReadCancellation, source_entity_uri};
use sources::{Source, SourceId, SourceSetup};

fn test_library() -> Option<PathBuf> {
    std::env::var_os("ITL_TEST_PATH").map(PathBuf::from).or_else(|| {
        let default = PathBuf::from("/nvme2tb/iTunes DEV/iTunes Library.itl");
        default.exists().then_some(default)
    })
}

#[tokio::test]
async fn itunes_catalog_reads_tracks_albums_and_stats() {
    let Some(library) = test_library() else {
        eprintln!("ITL_TEST_PATH missing — skipping iTunes source integration test");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("library.sqlite"))
        .await
        .unwrap();
    let connected = Source::connect(
        SourceId::new("itunes-test"),
        SourceSetup::Itunes {
            name: None,
            library,
            media_folder: None,
            tz_offset_seconds: None,
        },
        "",
    )
    .await
    .unwrap();
    let (_configuration, source, _credential) = connected.into_parts();
    database
        .reconcile_source(
            &SourceId::new("itunes-test"),
            sources::ITUNES_SOURCE_ID,
            "iTunes",
        )
        .await
        .unwrap();

    let cancelled = Arc::new(AtomicBool::new(false));
    let (complete, _) = source
        .acquire_catalog(&database, &|_| {}, Arc::clone(&cancelled), true)
        .await
        .unwrap();
    assert!(complete);

    // Catalog facts: the DEV library holds 180 unique tracks; the second
    // mlth duplicates (2 records) must merge into the same objects.
    let source_key = database
        .source_identity_key(&SourceId::new("itunes-test"))
        .await
        .unwrap()
        .unwrap();
    let (albums, count) = database
        .source_counts(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(count, 180, "unique .itl tracks in the catalog");
    assert!(albums > 0, "albums observed");

    // A known rated track from the DEV corpus (#507 = 80): the rating
    // arrives in the native slot and is recorded as the sync base.
    let media_uri = source_entity_uri(&SourceId::new("itunes-test"), "track", "itunes:track:507");
    let state = database
        .itunes_player_state(&media_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(state.native_rating, Some(80), "rating imported from the .itl");

    let base = database
        .itunes_base(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        base.get(&media_uri).copied().flatten(),
        Some(80),
        "the .itl rating is the sync base"
    );

    // A known played track (#515 = 3 plays).
    let played_uri = source_entity_uri(&SourceId::new("itunes-test"), "track", "itunes:track:515");
    let played = database
        .itunes_player_state(&played_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(played.play_count, Some(3), "play count imported from the .itl");
}
