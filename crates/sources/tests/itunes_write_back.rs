// Round-trip write-back test: rates and plays one located track against a
// COPY of the library, flushes through `itl-sync`, and verifies the `.itl`
// facts change and stay stable across a refresh (no double-counting).
// Gated by `ITL_TEST_PATH` and the `itl-sync` binary (env `ITL_SYNC_BIN`,
// else the itl-player debug build). Corpus-agnostic.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use itl_sync::paths::{MappedLocation, MediaMapping};
use library::{Database, FavoriteTarget, ReadCancellation, source_entity_uri};
use sources::{Source, SourceId, SourceSetup};

fn test_library() -> Option<PathBuf> {
    std::env::var_os("ITL_TEST_PATH").map(PathBuf::from).or_else(|| {
        let default = PathBuf::from("/nvme2tb/iTunesACCEPT/iTunes Library.itl");
        default.exists().then_some(default)
    })
}

fn sync_binary() -> Option<PathBuf> {
    if let Some(binary) = std::env::var_os("ITL_SYNC_BIN") {
        return Some(PathBuf::from(binary));
    }
    // The fork sits next to itl-player (crates/sources/…/tests → ../../..).
    let relative = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../itl-player/target/debug/itl_sync");
    if relative.exists() {
        return Some(relative);
    }
    let home = PathBuf::from("/home/meznick/Documents/Projects/itl-player/target/debug/itl_sync");
    home.exists().then_some(home)
}

#[tokio::test]
async fn itunes_write_back_updates_the_itl_and_refresh_does_not_double_count() {
    let Some(library_path) = test_library() else {
        eprintln!("ITL_TEST_PATH missing — skipping iTunes write-back integration test");
        return;
    };
    let Some(binary) = sync_binary() else {
        eprintln!("itl-sync binary not found — skipping iTunes write-back integration test");
        return;
    };
    // Pick a located track (its id is patchable through itl-sync).
    let itl = itl_core::ItlFile::open(&library_path).unwrap();
    let media_root = library_path.parent().unwrap().to_path_buf();
    let mapping = MediaMapping::new(&itl, &media_root).expect("corpus maps");
    let track = itl_sync::survey::unique_tracks(&itl)
        .into_iter()
        .find(|track| matches!(mapping.to_local(track), MappedLocation::Mapped { .. }))
        .expect("a located track in the corpus");
    let track_id = track.id();
    let new_rating = if track.rating() == 80 { 60 } else { 80 };
    let new_plays = track.play_count() + 1;
    let played_at = 1_800_000_000_i64;

    // Work on a copy: the corpus .itl must never be written.
    let directory = tempfile::tempdir().unwrap();
    let copy = directory.path().join("iTunes Library.itl");
    std::fs::copy(&library_path, &copy).unwrap();

    let database = Database::open(directory.path().join("library.sqlite"))
        .await
        .unwrap();
    let connected = Source::connect(
        SourceId::new("itunes-test"),
        SourceSetup::Itunes {
            name: None,
            library: copy,
            media_folder: Some(media_root),
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

    let media_uri = source_entity_uri(
        &SourceId::new("itunes-test"),
        "track",
        &format!("itunes:track:{track_id}"),
    );
    let state = database
        .itunes_player_state(&media_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(state.play_count, Some(i64::from(track.play_count())));

    // User action: rate, record a listen.
    database
        .set_rating(
            &FavoriteTarget::Track(media_uri.clone()),
            Some((new_rating / 10) as u8),
        )
        .await
        .unwrap();
    database
        .record_native_listen(&media_uri, false, played_at)
        .await
        .unwrap();
    let state = database
        .itunes_player_state(&media_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(state.play_count, Some(i64::from(new_plays)));
    assert_eq!(state.user_rating, Some(i64::from(new_rating)));

    // Flush through itl-sync (the runtime service path).
    let config = sources::itunes::ItunesSourceConfig::from_configuration(&configuration).unwrap();
    let patch = sources::itunes::write::ItunesPatch {
        track_id,
        rating: Some(new_rating as u8),
        play_count: Some(new_plays),
        skip_count: None,
        play_date_unix: Some(played_at),
    };
    sources::itunes::write::write_patches_with(&config, &[patch], &binary)
        .await
        .unwrap();

    // The copy now holds the new values.
    let written = itl_core::ItlFile::open(&config.library).unwrap();
    let written_track = written.track_by_id(track_id).unwrap();
    assert_eq!(written_track.play_count(), new_plays);
    assert_eq!(written_track.rating(), new_rating);

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
    assert_eq!(
        state.play_count,
        Some(i64::from(new_plays)),
        "no double-count after refresh"
    );
    assert_eq!(state.native_rating, Some(i64::from(new_rating)));
    assert_eq!(state.user_rating, Some(i64::from(new_rating)));
}
