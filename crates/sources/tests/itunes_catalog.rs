#![allow(clippy::print_stderr)] // skip messages are diagnostics, not print-debugging
// Integration test for the iTunes source read path, gated by
// `ITL_TEST_PATH` (defaults to the full production snapshot at
// `/nvme2tb/iTunesACCEPT/`; skipped when absent).
// Corpus-agnostic: expected numbers come from `itl_sync::survey` on the same
// file, so the test works against any `.itl` (snapshot, DEV, a copy).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use itl_sync::paths::{MappedLocation, MediaMapping};
use library::{Catalog, ReadCancellation, source_entity_uri};
use sources::{LocalImageRef, Refresh, Source, SourceId, SourceImageRequest};

fn test_library() -> Option<PathBuf> {
    std::env::var_os("ITL_TEST_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            let default = PathBuf::from("/nvme2tb/iTunesACCEPT/iTunes Library.itl");
            default.exists().then_some(default)
        })
}

/// Opens a fresh Catalog, connects the iTunes source, and reconciles it.
async fn connect(
    source_id: &str,
    library: PathBuf,
    media_folder: Option<PathBuf>,
) -> (Catalog, Source, library::SourceKey, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("library.sqlite");
    let catalog = Catalog::open(&database_path).await.unwrap();
    let configuration =
        sources::itunes::connect(SourceId::new(source_id), library, media_folder, None).unwrap();
    let source = Source::open(configuration, None, None, None).unwrap();
    catalog
        .reconcile_source(
            &SourceId::new(source_id),
            sources::ITUNES_SOURCE_ID,
            "iTunes",
        )
        .await
        .unwrap();
    let source_key = catalog.source_key(&SourceId::new(source_id)).unwrap();
    (catalog, source, source_key, database_path)
}

async fn acquire(source: &Source, catalog: &Catalog) {
    let cancelled = Arc::new(AtomicBool::new(false));
    source
        .acquire_catalog(catalog, &|_| {}, cancelled, Refresh::Changed)
        .await
        .unwrap();
}

async fn track_counts(
    database_path: &std::path::Path,
    source_key: library::SourceKey,
) -> (i64, i64) {
    use sqlx::Connection as _;
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(database_path),
    )
    .await
    .unwrap();
    let tracks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tracks WHERE source_key=?1 AND indexed=1")
            .bind(source_key.raw())
            .fetch_one(&mut connection)
            .await
            .unwrap();
    let albums: i64 =
        sqlx::query_scalar("SELECT count(*) FROM albums WHERE source_key=?1 AND indexed=1")
            .bind(source_key.raw())
            .fetch_one(&mut connection)
            .await
            .unwrap();
    (tracks, albums)
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

    // Expected album grouping: the source keys albums by the normalized
    // (album name, album artist) pair — the same rule as iTunes' display.
    // No-location tracks are not published, so they don't contribute.
    let mut expected_albums: BTreeMap<(String, String), usize> = BTreeMap::new();
    for track in unique_tracks(&itl) {
        if matches!(mapping.to_local(track), MappedLocation::NoLocation) {
            continue;
        }
        let name = track.album().unwrap_or("").to_string();
        let artist = track
            .album_artist()
            .or_else(|| track.artist())
            .unwrap_or("")
            .to_string();
        let key = if name.is_empty() {
            (artist.to_lowercase(), "unknown album".to_string())
        } else {
            (artist.to_lowercase(), name.to_lowercase())
        };
        *expected_albums.entry(key).or_insert(0) += 1;
    }
    let max_expected = expected_albums.values().copied().max().unwrap_or(0);

    let (catalog, source, source_key, database_path) =
        connect("itunes-test", library_path.clone(), None).await;
    acquire(&source, &catalog).await;

    // Tracks, albums and stats mirror the .itl — except no-location tracks,
    // which have no file to play and are not published.
    let (count, albums) = track_counts(&database_path, source_key).await;
    assert_eq!(
        count, report.located as i64,
        "catalog holds located unique .itl tracks only"
    );
    assert!(albums > 0, "albums observed");

    // Album grouping regression: tracks of one album must land in ONE album
    // object (keyed by normalized name+artist), never in per-track clones
    // with the same title. Checks: exact album count, no duplicate
    // (title, artist) pairs, and the largest album holds all its tracks.
    {
        use sqlx::Connection as _;
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&database_path),
        )
        .await
        .unwrap();
        let album_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM albums WHERE source_key=?1 AND indexed=1")
                .bind(source_key.raw())
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(
            album_rows,
            expected_albums.len() as i64,
            "one album object per (name, album artist) pair"
        );
        let duplicate_pairs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (
                 SELECT json_extract(metadata,'$.title'), json_extract(metadata,'$.artist')
                 FROM albums WHERE source_key=?1 AND indexed=1
                 GROUP BY 1,2 HAVING count(*)>1)",
        )
        .bind(source_key.raw())
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(
            duplicate_pairs, 0,
            "no two album objects with the same (title, artist)"
        );
        let max_track_count: i64 = sqlx::query_scalar(
            "SELECT max(track_count) FROM albums WHERE source_key=?1 AND indexed=1",
        )
        .bind(source_key.raw())
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert_eq!(
            max_track_count, max_expected as i64,
            "the largest album holds all of its tracks (play album works)"
        );
    }

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
    let missing = catalog
        .itunes_player_state(&missing_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        missing,
        Default::default(),
        "no-location track is not imported"
    );

    // Refresh sweeps rows the new observation did not include: a stale
    // catalog from an older build must not keep no-location tracks visible.
    {
        let mut observation = catalog.observe("itunes-test").await.unwrap();
        let mut stale = library::CatalogBatch::default();
        stale.tracks.push(library::TrackObservation {
            object_id: format!("itunes:track:{}", no_location.id()),
            media_uri: Some(missing_uri.clone()),
            metadata: library::TrackMetadata {
                title: "stale cloud track".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        observation.apply_catalog(&stale, true).await.unwrap();
    }
    let (with_stale, _) = track_counts(&database_path, source_key).await;
    assert_eq!(with_stale, report.located as i64 + 1, "stale row seeded");
    acquire(&source, &catalog).await;
    let (after_refresh, _) = track_counts(&database_path, source_key).await;
    assert_eq!(
        after_refresh, report.located as i64,
        "refresh unindexes tracks absent from the .itl observation"
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
    let state = catalog
        .itunes_player_state(&rated_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        state.native_rating,
        Some(i64::from(rated.rating())),
        "rating imported from the .itl"
    );
    let base = catalog
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
    let played_state = catalog
        .itunes_player_state(&played_uri, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        played_state.play_count,
        Some(i64::from(played.play_count())),
        "play count imported from the .itl"
    );

    // The explicit flag (mith @0xA6, store or manual bit) lands in the track
    // metadata; clean and unflagged tracks are both `false`. Each half is
    // guarded so the corpus decides what can be checked.
    {
        use sqlx::Connection as _;
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&database_path),
        )
        .await
        .unwrap();
        let located_tracks = unique_tracks(&itl)
            .into_iter()
            .filter(|track| matches!(mapping.to_local(track), MappedLocation::Mapped { .. }))
            .collect::<Vec<_>>();
        if let Some(explicit) = located_tracks.iter().find(|track| track.is_explicit()) {
            let uri = source_entity_uri(
                &SourceId::new("itunes-test"),
                "track",
                &format!("itunes:track:{}", explicit.id()),
            );
            let flagged: i64 = sqlx::query_scalar(
                "SELECT json_extract(details.metadata, '$.explicit') FROM track_details details
                 WHERE details.track_key=(SELECT track_key FROM tracks WHERE media_uri=?1)",
            )
            .bind(&uri)
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert_eq!(flagged, 1, "explicit flag imported from the .itl");
            let stored: String = sqlx::query_scalar(
                "SELECT json(details.metadata) FROM track_details details
                 WHERE details.track_key=(SELECT track_key FROM tracks WHERE media_uri=?1)",
            )
            .bind(&uri)
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert!(
                stored.contains("\"explicit\":true"),
                "the stored JSON carries a real boolean, not the JSONB integer ({stored})"
            );
        }
        if let Some(clean) = located_tracks.iter().find(|track| !track.is_explicit()) {
            let uri = source_entity_uri(
                &SourceId::new("itunes-test"),
                "track",
                &format!("itunes:track:{}", clean.id()),
            );
            let flagged: i64 = sqlx::query_scalar(
                "SELECT json_extract(details.metadata, '$.explicit') FROM track_details details
                 WHERE details.track_key=(SELECT track_key FROM tracks WHERE media_uri=?1)",
            )
            .bind(uri)
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert_eq!(flagged, 0, "clean tracks are not explicit");
        }
    }
}

fn unique_tracks(itl: &itl_core::ItlFile) -> Vec<&itl_core::Track> {
    itl_sync::survey::unique_tracks(itl)
}

/// Whether a readable file carries an embedded picture, judged exactly like
/// the source's inspector: a successful lofty read without pictures means
/// the source publishes no binding for the track.
fn has_embedded_picture(path: &std::path::Path) -> Option<bool> {
    use lofty::config::ParseOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    let file = std::fs::File::open(path).ok()?;
    let tagged = Probe::new(std::io::BufReader::new(file))
        .options(ParseOptions::new().read_cover_art(true))
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    Some(!tag.pictures().is_empty())
}

/// The `Local` candidate of a published binding, as the artwork pipeline
/// hands it to `Source::image`.
fn local_reference(binding: &[u8]) -> LocalImageRef {
    let candidates: Vec<library::ArtworkCandidate> = serde_json::from_slice(binding).unwrap();
    let candidate = candidates
        .iter()
        .find(|candidate| matches!(candidate.image, library::ArtworkImage::Local { .. }))
        .expect("binding holds a Local candidate");
    match &candidate.image {
        library::ArtworkImage::Local {
            source_id,
            path,
            revision,
            picture_index,
        } => match picture_index {
            Some(picture_index) => LocalImageRef::Embedded {
                source_id: source_id.clone(),
                path: path.clone(),
                revision: revision.clone(),
                picture_index: *picture_index,
            },
            None => LocalImageRef::File {
                source_id: source_id.clone(),
                path: path.clone(),
                revision: revision.clone(),
            },
        },
        _ => unreachable!("matched Local above"),
    }
}

#[tokio::test]
async fn itunes_embedded_artwork_publishes_bindings_and_serves_images() {
    let Some(library_path) = test_library() else {
        eprintln!("ITL_TEST_PATH missing — skipping iTunes source integration test");
        return;
    };
    let itl = itl_core::ItlFile::open(&library_path).unwrap();
    let media_root = library_path.parent().unwrap().to_path_buf();
    let mapping = MediaMapping::new(&itl, &media_root).expect("corpus maps under the media folder");

    // Classify located tracks by their embedded pictures.
    let mut with_picture: Option<u32> = None;
    let mut without_picture: Option<u32> = None;
    for track in unique_tracks(&itl) {
        let MappedLocation::Mapped { path, .. } = mapping.to_local(track) else {
            continue;
        };
        match has_embedded_picture(&path) {
            Some(true) => with_picture.get_or_insert(track.id()),
            Some(false) => without_picture.get_or_insert(track.id()),
            None => continue,
        };
        if with_picture.is_some() && without_picture.is_some() {
            break;
        }
    }

    let (catalog, source, source_key, database_path) =
        connect("itunes-art-test", library_path.clone(), None).await;
    acquire(&source, &catalog).await;

    let uri_for = |id: u32| {
        source_entity_uri(
            &SourceId::new("itunes-art-test"),
            "track",
            &format!("itunes:track:{id}"),
        )
    };
    let bindings = catalog
        .itunes_artwork_bindings(source_key, &ReadCancellation::new())
        .await
        .unwrap();

    // A track with an embedded picture publishes a Local binding, and
    // `Source::image` serves the picture bytes.
    let picture_id = with_picture.expect("corpus has a located track with an embedded picture");
    let picture_uri = uri_for(picture_id);
    let binding = bindings
        .get(&picture_uri)
        .expect("artwork binding published for the picture track");
    let image = source
        .image(SourceImageRequest::Local(local_reference(binding)))
        .await
        .unwrap();
    assert!(!image.bytes.is_empty(), "served picture is not empty");
    assert!(
        image
            .content_type
            .as_deref()
            .is_some_and(|content_type| content_type.starts_with("image/")),
        "picture content type: {:?}",
        image.content_type
    );
    assert!(
        image.bytes.starts_with(b"\xFF\xD8") || image.bytes.starts_with(b"\x89PNG"),
        "picture bytes carry a JPEG/PNG magic"
    );

    // A pictureless located track publishes no binding, and the
    // authoritative observation leaves no native artwork behind.
    if let Some(id) = without_picture {
        let uri = uri_for(id);
        assert!(
            !bindings.contains_key(&uri),
            "no binding for the pictureless track"
        );
        use sqlx::Connection as _;
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&database_path),
        )
        .await
        .unwrap();
        let native_artwork: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT details.native_artwork FROM track_details details
             WHERE details.track_key=(SELECT track_key FROM tracks WHERE source_key=?1 AND media_uri=?2)",
        )
        .bind(source_key.raw())
        .bind(&uri)
        .fetch_one(&mut connection)
        .await
        .unwrap();
        assert!(
            native_artwork.is_none(),
            "pictureless track has no native artwork"
        );
    }

    // A refresh reuses the cached binding verbatim (the file revision is
    // unchanged), and the picture is still served.
    acquire(&source, &catalog).await;
    let refreshed = catalog
        .itunes_artwork_bindings(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        refreshed.get(&picture_uri),
        bindings.get(&picture_uri),
        "cached binding reused across refreshes"
    );
    let image = source
        .image(SourceImageRequest::Local(local_reference(
            &refreshed[&picture_uri],
        )))
        .await
        .unwrap();
    assert!(!image.bytes.is_empty(), "cached picture still served");
}

/// Corpus-free regression guard: the stream arm must play the host path the
/// observation stored in `tracks.source_path`. After the 0.19 port the arm
/// read `playable.file` instead — a `files` row, which only the file walker
/// writes — so every iTunes track failed with NotFound, surfaced as "Track
/// unavailable" at playback. Runs on any host, no `.itl` needed.
#[tokio::test]
async fn stream_resolves_the_observation_source_path() {
    let configuration = sources::itunes::connect(
        SourceId::new("itunes-stream-test"),
        "/nonexistent/iTunes Library.itl".into(),
        None,
        None,
    )
    .unwrap();
    let source = Source::open(configuration, None, None, None).unwrap();
    let media_uri = "rufin:source/track/itunes-stream-test/itunes:track:7";
    let playable = library::Playable {
        track: library::TrackKey::from_raw(1),
        source: Some(library::SourceKey::from_raw(1)),
        source_id: Some(SourceId::new("itunes-stream-test")),
        source_kind: Some("itunes".to_string()),
        media_uri: media_uri.to_string(),
        object_id: Some("itunes:track:7".to_string()),
        format: Some("M4A".to_string()),
        source_path: Some("/music/Artist/Album/Track.m4a".to_string()),
        cue_path: None,
        download: None,
        mapped: None,
        locator: None,
        file: None,
        window: None,
        audio_revision: None,
        audio_hash: None,
        channels: None,
        bitrate_kbps: None,
        bit_depth: None,
        gains: library::Gains::default(),
        names: library::PlayableNames::default(),
    };
    let stream = source
        .stream(&playable, playback::StreamRequest::original(media_uri))
        .await
        .unwrap();
    assert_eq!(
        stream.uri(),
        url::Url::from_file_path("/music/Artist/Album/Track.m4a")
            .unwrap()
            .as_str(),
        "the stream plays the host path the observation stored, not a files row"
    );
}
