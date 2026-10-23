//! The `.itl` read path: dedupe, path mapping, observation building, and the
//! three-way stats merge (`.itl` / player / sync base).

use std::collections::BTreeMap;

use itl_sync::paths::{MappedLocation, MediaMapping};
use itl_sync::survey::unique_tracks;
use library::{
    AlbumMetadata, ArtistCredit, ArtistMetadata, ArtistObservation, AlbumObservation,
    CatalogBatch, GenreObservation, NativeUserState, ReadCancellation, SourceId, TagMetadata,
    TagReference, TrackMetadata, TrackObservation,
};

use crate::source::{SourceReadProgress, SourceReadStage};
use crate::{SourceError, SourceResult};
use super::write::ItunesPatch;
use super::ItunesSource;

pub(crate) async fn acquire(
    source: &ItunesSource,
    catalog: &mut library::Catalog<'_>,
    progress: &(dyn Fn(SourceReadProgress) + Send + Sync),
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> SourceResult<()> {
    let config = source.config();
    let library_path = config.library.clone();
    let itl = tokio::task::spawn_blocking(move || itl_core::ItlFile::open(&library_path))
        .await
        .map_err(|error| SourceError::Other(error.to_string()))?
        .map_err(|error| {
            SourceError::InvalidConfig(format!(
                "cannot open iTunes library {}: {error}",
                config.library.display()
            ))
        })?;
    if cancelled() {
        return Err(SourceError::Cancelled);
    }
    let tz = config.tz_offset(&itl);
    let mapping = MediaMapping::new(&itl, &config.local_root()).ok_or_else(|| {
        SourceError::InvalidConfig(
            "no media folder mapping: the library has no media folder field and no locations"
                .to_string(),
        )
    })?;

    // Tracks with no location at all (purchased but not downloaded — the
    // cloud items) have no file to play: they are not published into the
    // catalog. They reappear automatically on the first refresh after iTunes
    // downloads them; `itl-sync survey` keeps reporting them as
    // `no location` / `no_location_cloud`.
    let located = unique_tracks(&itl)
        .into_iter()
        .filter_map(|track| {
            let mapped = mapping.to_local(track);
            (!matches!(mapped, MappedLocation::NoLocation)).then_some((track, mapped))
        })
        .collect::<Vec<_>>();
    let total = located.len();
    progress(SourceReadProgress {
        stage: SourceReadStage::Albums,
        completed: 0,
        total: None,
    });

    let source_id = SourceId::new(catalog.source_id.to_string());
    let track_object_id = |id: u32| format!("itunes:track:{id}");
    let media_uri_for = |object_id: &str| {
        library::source_entity_uri(&source_id, "track", object_id)
    };
    // Albums: iTunes groups tracks by (Album, Album Artist). The mith album
    // id (@0x9C) is absent on most tracks (zeros), and `album_persistent_id`
    // reads the wrong header range (the date-added bytes) — so the identity
    // is the normalized (name, album-artist) pair, mirroring the local
    // source (`file/media.rs::album_id`). Tracks without an album name
    // share one "Unknown Album" per album artist (remote-source behavior;
    // iTunes itself shows them only in Songs).
    const UNKNOWN_ALBUM: &str = "Unknown Album";
    let album_identity = |name: &str, artist: &str| {
        (artist.to_lowercase(), name.to_lowercase())
    };
    let album_object_id = |identity: &(String, String)| {
        format!("itunes:album:{}:{}", identity.0, identity.1)
    };
    let album_of = |track: &itl_core::Track| {
        let name = track.album().unwrap_or("").to_string();
        let artist = track
            .album_artist()
            .or_else(|| track.artist())
            .unwrap_or("")
            .to_string();
        let (title, identity) = if name.is_empty() {
            (UNKNOWN_ALBUM.to_string(), album_identity(UNKNOWN_ALBUM, &artist))
        } else {
            (name.clone(), album_identity(&name, &artist))
        };
        (title, artist, identity)
    };

    // Albums referenced by tracks; title/artist/year from the first track.
    let mut albums: BTreeMap<(String, String), (String, String, Option<i64>)> = BTreeMap::new();
    for (track, _) in &located {
        let (title, artist, identity) = album_of(track);
        albums
            .entry(identity)
            .or_insert_with(|| (title, artist, track.year().map(i64::from)));
    }

    // Artists, album artists, genres: name-keyed entities shared with the
    // track credits (the catalog joins credits on object_id).
    let artist_id = |name: &str| format!("itunes:artist:{name}");
    let genre_id = |name: &str| format!("itunes:genre:{name}");
    let mut artists: BTreeMap<String, String> = BTreeMap::new();
    let mut genres: BTreeMap<String, String> = BTreeMap::new();
    for (track, _) in &located {
        if let Some(name) = track.artist().filter(|name| !name.is_empty()) {
            artists.entry(name.to_string()).or_insert_with(|| name.to_string());
        }
        if let Some(name) = track
            .album_artist()
            .or_else(|| track.artist())
            .filter(|name| !name.is_empty())
        {
            artists.entry(name.to_string()).or_insert_with(|| name.to_string());
        }
        if let Some(name) = track.genre().filter(|name| !name.is_empty()) {
            genres
                .entry(name.to_string())
                .or_insert_with(|| name.to_string());
        }
    }

    let mut batch = CatalogBatch::default();
    batch.albums = albums
        .iter()
        .map(|(identity, (title, artist, year))| AlbumObservation {
            object_id: album_object_id(identity),
            metadata: AlbumMetadata {
                title: title.clone(),
                artist: artist.clone(),
                sort_text: title.to_lowercase(),
                year: *year,
                release_date: None,
                date_added: None,
                musicbrainz_release_id: None,
                musicbrainz_release_group_id: None,
                is_compilation: None,
                album_artists: (!artist.is_empty())
                    .then(|| ArtistCredit {
                        object_id: artist_id(artist),
                        name: artist.clone(),
                        sort_text: Some(artist.to_lowercase()),
                        musicbrainz_artist_id: None,
                    })
                    .into_iter()
                    .collect(),
                genres: Vec::new(),
                release_types: Vec::new(),
            },
            artwork_binding: None,
            user_data: NativeUserState::default(),
            first_seen_at: None,
        })
        .collect();
    batch.artists = artists
        .iter()
        .map(|(name, _)| ArtistObservation {
            object_id: artist_id(name),
            metadata: ArtistMetadata {
                name: name.clone(),
                sort_text: Some(name.to_lowercase()),
                musicbrainz_artist_id: None,
            },
            artwork_binding: None,
            user_data: NativeUserState::default(),
        })
        .collect();
    batch.genres = genres
        .iter()
        .map(|(name, _)| GenreObservation {
            object_id: genre_id(name),
            metadata: TagMetadata {
                name: name.clone(),
                sort_text: Some(name.to_lowercase()),
            },
            artwork_binding: None,
        })
        .collect();

    let base = catalog
        .database
        .itunes_base(catalog.source, &ReadCancellation::new())
        .await?;
    let mut observed_uris = Vec::with_capacity(located.len());
    let mut patches: Vec<(String, ItunesPatch)> = Vec::new();

    for (index, (track, mapped)) in located.iter().enumerate() {
        if index % 128 == 0 && cancelled() {
            return Err(SourceError::Cancelled);
        }
        progress(SourceReadProgress {
            stage: SourceReadStage::Tracks,
            completed: index,
            total: Some(total),
        });

        let object_id = track_object_id(track.id());
        let media_uri = media_uri_for(&object_id);
        observed_uris.push(media_uri.clone());

        let player = catalog
            .database
            .itunes_player_state(&media_uri, &ReadCancellation::new())
            .await?;
        let old_base = base.get(&media_uri).copied().flatten();

        let itl_rating = Some(i64::from(track.rating()));
        let itl_play = i64::from(track.play_count());
        let itl_skip = i64::from(track.skip_count());
        let itl_played = (track.play_date_raw() != 0).then(|| {
            itl_core::wallclock_to_unix(itl_core::apple_to_unix(track.play_date_raw()), tz)
        });

        // Rating: iTunes wins whenever the .itl moved from the last agreed
        // value; otherwise a pending user override is pushed. The user slot
        // is adopted only when it actually holds an override — an absent
        // slot already falls back to the source value.
        let mut final_rating = itl_rating;
        let mut patch_rating: Option<u8> = None;
        if old_base != itl_rating {
            if let Some(rating) = itl_rating {
                if player.user_rating.is_some() && player.user_rating != itl_rating {
                    catalog
                        .database
                        .adopt_user_rating(&media_uri, rating)
                        .await?;
                }
            }
            catalog
                .database
                .write_itunes_base(catalog.source, &media_uri, itl_rating)
                .await?;
        } else if let Some(user) = player.user_rating {
            if Some(user) != itl_rating {
                final_rating = Some(user);
                patch_rating = Some(user.clamp(0, 100) as u8);
            }
        }

        // Counts: itl-relative deltas (the runtime flush writes the native
        // slot verbatim, so `native - itl` is exactly the pending growth).
        let native_play = player.play_count.unwrap_or(0);
        let native_skip = player.skip_count.unwrap_or(0);
        let final_play = itl_play + (native_play - itl_play).max(0);
        let final_skip = itl_skip + (native_skip - itl_skip).max(0);
        let final_played = match (itl_played, player.last_played) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, other) => other,
        };

        let patch = ItunesPatch {
            track_id: track.id(),
            rating: patch_rating,
            play_count: (final_play != itl_play).then_some(final_play as u32),
            skip_count: (final_skip != itl_skip).then_some(final_skip as u32),
            play_date_unix: final_played
                .filter(|played| itl_played.map_or(true, |itl| *played > itl)),
        };
        if !patch.is_empty() {
            patches.push((media_uri.clone(), patch));
        }

        let local_path = match mapped {
            MappedLocation::Mapped { path, .. } => Some(path.to_string_lossy().into_owned()),
            MappedLocation::NoLocation | MappedLocation::OutsideMediaFolder { .. } => None,
        };
        let title = track.title().unwrap_or("").to_string();
        let artist = track.artist().unwrap_or("").to_string();
        let album_artist_name = track
            .album_artist()
            .or_else(|| track.artist())
            .unwrap_or("")
            .to_string();
        let (album_title, _, album_identity) = album_of(track);

        batch.tracks.push(TrackObservation {
            object_id: object_id.clone(),
            media_uri: Some(media_uri.clone()),
            album_object_id: Some(album_object_id(&album_identity)),
            metadata: TrackMetadata {
                title: title.clone(),
                artist: artist.clone(),
                album: album_title,
                album_display_artist: Some(album_artist_name.clone()),
                sort_text: title.to_lowercase(),
                duration_millis: i64::from(track.duration_ms()),
                disc_number: i64::from(track.disc_number().unwrap_or(1)),
                track_number: i64::from(track.track_number().unwrap_or(0)),
                year: track.year().map(i64::from),
                release_date: None,
                date_added: format_date(track.date_added_unix()),
                source_format: track.kind().and_then(source_format),
                audio_properties: library::AudioProperties {
                    bitrate_kbps: (track.bit_rate() > 0).then(|| u32::from(track.bit_rate()) / 1000),
                    sample_rate_hz: (track.sample_rate() > 0).then_some(track.sample_rate()),
                    bit_depth: None,
                    channels: None,
                },
                comment: None,
                bpm: track.bpm().map(i64::from),
                musicbrainz_recording_id: None,
                musicbrainz_release_track_id: None,
                musicbrainz_album_id: None,
                musicbrainz_release_group_id: None,
                primary_artist_musicbrainz_id: None,
                artists: {
                    let sort_text = (!artist.is_empty()).then(|| artist.to_lowercase());
                    (!artist.is_empty())
                        .then(|| ArtistCredit {
                            object_id: artist_id(&artist),
                            name: artist,
                            sort_text,
                            musicbrainz_artist_id: None,
                        })
                        .into_iter()
                        .collect()
                },
                album_artists: (!album_artist_name.is_empty())
                    .then(|| ArtistCredit {
                        object_id: artist_id(&album_artist_name),
                        name: album_artist_name.clone(),
                        sort_text: Some(album_artist_name.to_lowercase()),
                        musicbrainz_artist_id: None,
                    })
                    .into_iter()
                    .collect(),
                genres: track
                    .genre()
                    .filter(|name| !name.is_empty())
                    .map(|name| TagReference {
                        object_id: genre_id(name),
                        name: name.to_string(),
                        sort_text: Some(name.to_lowercase()),
                    })
                    .into_iter()
                    .collect(),
                moods: Vec::new(),
                album_tags: None,
                source_loudness: None,
                album_loudness: None,
            },
            source_path: local_path,
            cue_path: None,
            cue_start_millis: None,
            cue_end_millis: None,
            first_seen_at: None,
            loudness_analysis_key: [0; 32],
            artwork_binding: None,
            user_data: NativeUserState {
                favorite: None,
                rating: Some(final_rating),
                play_count: Some(final_play),
                skip_count: Some(final_skip),
                last_played: final_played,
            },
            folders: None,
        });
    }
    if cancelled() {
        return Err(SourceError::Cancelled);
    }
    progress(SourceReadProgress {
        stage: SourceReadStage::Artists,
        completed: 0,
        total: Some(batch.artists.len()),
    });

    catalog.apply_catalog(&batch, true).await?;

    // Unindex everything this pass did not observe (tracks removed from the
    // .itl, tracks that lost their location, albums/artists that became
    // empty). The generic wrapper calls this with an empty kind list, which
    // only records completeness — the actual absent-row sweep is per kind.
    if let Some(stamp) = catalog.observed {
        catalog
            .complete_source_observation(stamp, &["track", "album", "artist", "genre"], None)
            .await?;
    }

    // Base rows for tracks no longer in the .itl.
    catalog
        .database
        .prune_itunes_base(catalog.source, &observed_uris)
        .await?;

    // Push the pending deltas into the .itl (single atomic write per batch).
    if !patches.is_empty() {
        let write_patches = patches
            .iter()
            .map(|(_, patch)| patch.clone())
            .collect::<Vec<_>>();
        match super::write::write_patches(&config, &write_patches).await {
            Ok(()) => {
                for (media_uri, patch) in &patches {
                    if let Some(rating) = patch.rating {
                        let _ = catalog
                            .database
                            .write_itunes_base(
                                catalog.source,
                                media_uri,
                                Some(i64::from(rating)),
                            )
                            .await;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "iTunes write-back failed; pending deltas stay in the player state");
            }
        }
    }
    Ok(())
}

fn source_format(kind: &str) -> Option<String> {
    match kind {
        "MPEG audio file" => Some("MP3".to_string()),
        "AAC audio file" => Some("AAC".to_string()),
        "Apple Lossless audio file" => Some("ALAC".to_string()),
        "AIFF audio file" => Some("AIFF".to_string()),
        "WAV audio file" => Some("WAV".to_string()),
        _ => None,
    }
}

/// `YYYY-MM-DD` from unix seconds (UTC); None for zero/negative.
fn format_date(unix: i64) -> Option<String> {
    if unix <= 0 {
        return None;
    }
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = y + i64::from(month <= 2);
    (year, month, day)
}
