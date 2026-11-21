#![allow(clippy::print_stderr)] // skip messages are diagnostics, not print-debugging
// Round-trip write-back test: rates and plays one located track against a
// COPY of the library, flushes through the in-process write pipeline, and
// verifies the `.itl` facts change and stay stable across a refresh (no
// double-counting). Gated by `ITL_TEST_PATH`. Corpus-agnostic.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use itl_sync::paths::{MappedLocation, MediaMapping};
use library::{Catalog, FavoriteTarget, source_entity_uri};
use sources::{Refresh, Source, SourceId};

fn test_library() -> Option<PathBuf> {
    std::env::var_os("ITL_TEST_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            let default = PathBuf::from("/nvme2tb/iTunesACCEPT/iTunes Library.itl");
            default.exists().then_some(default)
        })
}

#[tokio::test]
async fn itunes_write_back_updates_the_itl_and_refresh_does_not_double_count() {
    let Some(library_path) = test_library() else {
        eprintln!("ITL_TEST_PATH missing — skipping iTunes write-back integration test");
        return;
    };
    // Pick a located track (its id is patchable through the write pipeline).
    let itl = itl_core::ItlFile::open(&library_path).unwrap();
    let media_root = library_path.parent().unwrap().to_path_buf();
    let mapping = MediaMapping::new(&itl, &media_root).expect("corpus maps");
    let track = itl_sync::survey::unique_tracks(&itl)
        .into_iter()
        .find(|track| matches!(mapping.to_local(track), MappedLocation::Mapped { .. }))
        .expect("a located track in the corpus");
    let track_id = track.id();
    // Rufin ratings are whole stars (0..=10); the .itl stores 0..=100.
    let new_rating = if track.rating() == 80 { 6 } else { 8 };
    let new_plays = track.play_count() + 1;
    let played_at = 1_800_000_000_i64;

    // Work on a copy: the corpus .itl must never be written.
    let directory = tempfile::tempdir().unwrap();
    let copy = directory.path().join("iTunes Library.itl");
    std::fs::copy(&library_path, &copy).unwrap();

    let catalog = Catalog::open(directory.path().join("library.sqlite"))
        .await
        .unwrap();
    let configuration = sources::itunes::connect(
        SourceId::new("itunes-test"),
        copy.clone(),
        Some(media_root),
        None,
    )
    .unwrap();
    let source = Source::open(configuration, None, None, None).unwrap();
    catalog
        .reconcile_source(
            &SourceId::new("itunes-test"),
            sources::ITUNES_SOURCE_ID,
            "iTunes",
        )
        .await
        .unwrap();
    let media_uri = source_entity_uri(
        &SourceId::new("itunes-test"),
        "track",
        &sources::itunes::catalog::track_object_id(&track),
    );

    async fn acquire(source: &Source, catalog: &Catalog) {
        let cancelled = Arc::new(AtomicBool::new(false));
        source
            .acquire_catalog(catalog, &|_| {}, cancelled, Refresh::Changed)
            .await
            .unwrap();
    }

    // Baseline listing: the native slot now holds the .itl facts and the
    // sync base agrees with the .itl.
    acquire(&source, &catalog).await;

    // User action: rate the track and play it once. The rating lands in the
    // user slot, the listen bumps the native counters.
    catalog
        .set_rating(
            &FavoriteTarget::Track(media_uri.clone()),
            Some(new_rating),
            false,
        )
        .await
        .unwrap();
    catalog
        .record_native_listen(&media_uri, false, played_at)
        .await
        .unwrap();

    // The refresh merge derives the pending deltas and flushes one atomic
    // `.itl` write.
    acquire(&source, &catalog).await;

    let after = itl_core::ItlFile::open(&copy).unwrap();
    let track_after = itl_sync::survey::unique_tracks(&after)
        .into_iter()
        .find(|track| track.id() == track_id)
        .expect("the track is still in the copy");
    assert_eq!(
        track_after.rating(),
        new_rating * 10,
        "the .itl holds the written rating"
    );
    assert_eq!(
        track_after.play_count(),
        new_plays,
        "the .itl holds the written play count"
    );

    // A second refresh derives no new deltas: the .itl facts stay put.
    acquire(&source, &catalog).await;
    let stable = itl_core::ItlFile::open(&copy).unwrap();
    let track_stable = itl_sync::survey::unique_tracks(&stable)
        .into_iter()
        .find(|track| track.id() == track_id)
        .expect("the track is still in the copy");
    assert_eq!(
        track_stable.rating(),
        new_rating * 10,
        "the rating does not drift on refresh"
    );
    assert_eq!(
        track_stable.play_count(),
        new_plays,
        "the play count does not double on refresh"
    );
}
