// Integration test for the iTunes source read path, gated by
// `ITL_TEST_PATH` (defaults to the full production snapshot at
// `/nvme2tb/iTunesACCEPT/`; skipped when absent).
// Corpus-agnostic: expected numbers come from `itl_sync::survey` on the same
// file, so the test works against any `.itl` (snapshot, DEV, a copy).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use itl_sync::paths::{MappedLocation, MediaMapping};
use library::{Database, ReadCancellation, source_entity_uri};
use sources::{LocalImageRef, Source, SourceId, SourceImageRequest, SourceSetup};

fn test_library() -> Option<PathBuf> {
    std::env::var_os("ITL_TEST_PATH").map(PathBuf::from).or_else(|| {
        let default = PathBuf::from("/nvme2tb/iTunesACCEPT/iTunes Library.itl");
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

    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("library.sqlite");
    let database = Database::open(&database_path).await.unwrap();
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
        let album_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM albums WHERE source_key=?1 AND indexed=1",
        )
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
            max_track_count,
            max_expected as i64,
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
    let missing = database
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
        let mut catalog = database.catalog("itunes-test", None).await.unwrap();
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
        catalog.apply_catalog(&stale, true).await.unwrap();
    }
    let (_, with_stale) = database
        .source_counts(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(with_stale, report.located + 1, "stale row seeded");
    source
        .acquire_catalog(&database, &|_| {}, Arc::clone(&cancelled), true)
        .await
        .unwrap();
    let (_, after_refresh) = database
        .source_counts(source_key, &ReadCancellation::new())
        .await
        .unwrap();
    assert_eq!(
        after_refresh, report.located,
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

    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("library.sqlite");
    let database = Database::open(&database_path).await.unwrap();
    let connected = Source::connect(
        SourceId::new("itunes-art-test"),
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
            &SourceId::new("itunes-art-test"),
            sources::ITUNES_SOURCE_ID,
            "iTunes",
        )
        .await
        .unwrap();
    let source_key = database
        .source_identity_key(&SourceId::new("itunes-art-test"))
        .await
        .unwrap()
        .unwrap();

    let cancelled = Arc::new(AtomicBool::new(false));
    source
        .acquire_catalog(&database, &|_| {}, Arc::clone(&cancelled), true)
        .await
        .unwrap();

    let uri_for = |id: u32| {
        source_entity_uri(
            &SourceId::new("itunes-art-test"),
            "track",
            &format!("itunes:track:{id}"),
        )
    };
    let bindings = database
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
            "SELECT native_artwork FROM tracks WHERE source_key=?1 AND media_uri=?2",
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
    source
        .acquire_catalog(&database, &|_| {}, Arc::clone(&cancelled), true)
        .await
        .unwrap();
    let refreshed = database
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
