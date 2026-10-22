// Integration test for the iTunes source read path, gated by
// `ITL_TEST_PATH` (defaults to the DEV library; skipped when absent).
// Corpus-agnostic: expected numbers come from `itl_sync::survey` on the same
// file, so the test works against any `.itl` (real library, DEV, a copy).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use itl_sync::paths::{MappedLocation, MediaMapping};
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
    let Some(library_path) = test_library() else {
        eprintln!("ITL_TEST_PATH missing — skipping iTunes source integration test");
        return;
    };
    let itl = itl_core::ItlFile::open(&library_path).unwrap();
    let media_root = library_path.parent().unwrap().to_path_buf();
    let report = itl_sync::survey::survey(&itl, &media_root);
    assert!(
        report.no_location > 0,
        "corpus must contain no-location tracks for the filter check"
    );
    let mapping = MediaMapping::new(&itl, &media_root).expect("corpus maps under the media folder");

    let directory = tempfile::tempdir().unwrap();
    let database = Database::open(directory.path().join("library.sqlite"))
        .await
        .unwrap();
    let connected = Source::connect(
        SourceId::new("itunes-test"),
        SourceSetup::Itunes {
            name: None,
            library: library_path.clone(),
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

    // Tracks, albums and stats mirror the .itl — except no-location tracks,
    // which have no file to play and are not published.
    let source_key = database
        .source_identity_key(&SourceId::new("itunes-test"))
        .await
        .unwrap()
        .unwrap();
    let (albums, count) = database
        .source_counts(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        count, report.located,
        "catalog holds located unique .itl tracks only"
    );
    assert!(albums > 0, "albums observed");

    // A no-location track is absent from the catalog: no base rows and no
    // native stats were written for it.
    let no_location = unique_tracks(&itl)
        .into_iter()
        .find(|track| matches!(mapping.to_local(track), MappedLocation::NoLocation))
        .expect("a no-location track in the corpus");
    let missing_uri = source_entity_uri(
        &SourceId::new("itunes-test"),
        "track",
        &format!("itunes:track:{}", no_location.id()),
    );
    let missing = database
        .itunes_player_state(&missing_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        missing,
        Default::default(),
        "no-location track is not imported"
    );

    // A rated located track: the rating lands in the native slot and is
    // recorded as the sync base.
    let rated = unique_tracks(&itl)
        .into_iter()
        .filter(|track| track.rating() > 0)
        .find(|track| matches!(mapping.to_local(track), MappedLocation::Mapped { .. }))
        .expect("a rated located track in the corpus");
    let rated_uri = source_entity_uri(
        &SourceId::new("itunes-test"),
        "track",
        &format!("itunes:track:{}", rated.id()),
    );
    let state = database
        .itunes_player_state(&rated_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        state.native_rating,
        Some(i64::from(rated.rating())),
        "rating imported from the .itl"
    );
    let base = database
        .itunes_base(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        base.get(&rated_uri).copied().flatten(),
        Some(i64::from(rated.rating())),
        "the .itl rating is the sync base"
    );

    // A played located track: the play count is imported as-is.
    let played = unique_tracks(&itl)
        .into_iter()
        .filter(|track| track.play_count() > 0)
        .find(|track| matches!(mapping.to_local(track), MappedLocation::Mapped { .. }))
        .expect("a played located track in the corpus");
    let played_uri = source_entity_uri(
        &SourceId::new("itunes-test"),
        "track",
        &format!("itunes:track:{}", played.id()),
    );
    let played_state = database
        .itunes_player_state(&played_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        played_state.play_count,
        Some(i64::from(played.play_count())),
        "play count imported from the .itl"
    );
}

fn unique_tracks(itl: &itl_core::ItlFile) -> Vec<&itl_core::Track> {
    itl_sync::survey::unique_tracks(itl)
}
