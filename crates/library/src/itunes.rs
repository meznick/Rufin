//! iTunes source support: native listen bumps and the rating sync base that
//! make `.itl` ↔ player merges lossless.
//!
//! Counts and last-played never need a base row: the runtime write-back
//! writes the native slot verbatim, so at refresh the pending player delta
//! is exactly `native - itl` (clamped at zero). Ratings do: the base row is
//! the last rating agreed with the `.itl`, and the merge treats
//! `itl != base` as "iTunes moved, iTunes wins".

use std::collections::HashMap;

use crate::{LibraryResult, ReadCancellation, SourceKey};

/// Player-side values the iTunes write-back flushes into the `.itl`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItunesPlayerState {
    /// Full-precision user rating (0..=100), when the user set one.
    pub user_rating: Option<i64>,
    /// Source rating from the last observation (0..=100).
    pub native_rating: Option<i64>,
    pub play_count: Option<i64>,
    pub skip_count: Option<i64>,
    pub last_played: Option<i64>,
}

impl crate::Catalog {
    /// A local listen on an iTunes-source track: bump the native counters so
    /// the displayed stats move immediately (the `catalog_index_native_*`
    /// triggers refresh them) and the delta reaches the `.itl` on the next
    /// write-back. Skipped listens bump skip_count; played ones bump
    /// play_count and last_played (iTunes semantics: Last Played moves only
    /// on real plays).
    pub async fn record_native_listen(
        &self,
        media_uri: &str,
        skipped: bool,
        started_at: i64,
    ) -> LibraryResult<()> {
        let mut write = self.write().await?;
        let connection = write.connection().await?;
        if skipped {
            sqlx::query(
                "INSERT INTO native_media_state(media_uri,skip_count) VALUES(?1,1)
                 ON CONFLICT(media_uri) DO UPDATE SET
                     skip_count=coalesce(native_media_state.skip_count,0)+1",
            )
            .bind(media_uri)
            .execute(&mut *connection)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO native_media_state(media_uri,play_count,last_played) VALUES(?1,1,?2)
                 ON CONFLICT(media_uri) DO UPDATE SET
                     play_count=coalesce(native_media_state.play_count,0)+1,
                     last_played=max(coalesce(native_media_state.last_played,0),excluded.last_played)",
            )
            .bind(media_uri)
            .bind(started_at)
            .execute(&mut *connection)
            .await?;
        }
        write.commit().await?;
        Ok(())
    }

    /// The values the iTunes write-back flushes: the full-precision user
    /// rating and the native stats row.
    pub async fn itunes_player_state(
        &self,
        media_uri: &str,
        _cancellation: &ReadCancellation,
    ) -> LibraryResult<ItunesPlayerState> {
        let mut connection = self.acquire_reader().await?;
        let (user_rating, native_rating, play_count, skip_count, last_played): (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT (SELECT rating FROM user_state WHERE media_uri=?1),
                    (SELECT rating FROM native_media_state WHERE media_uri=?1),
                    (SELECT play_count FROM native_media_state WHERE media_uri=?1),
                    (SELECT skip_count FROM native_media_state WHERE media_uri=?1),
                    (SELECT last_played FROM native_media_state WHERE media_uri=?1)",
        )
        .bind(media_uri)
        .fetch_one(&mut *connection)
        .await?;
        Ok(ItunesPlayerState {
            user_rating,
            native_rating,
            play_count,
            skip_count,
            last_played,
        })
    }

    /// iTunes wins: adopt the `.itl` rating into the user slot, so the
    /// visible rating follows iTunes even when an older override lingers.
    pub async fn adopt_user_rating(&self, media_uri: &str, rating: i64) -> LibraryResult<()> {
        let mut write = self.write().await?;
        crate::favorites::write_user_media_state(
            &mut write,
            &crate::favorites::UserMediaStateWrite {
                media_uri: media_uri.to_owned(),
                favorite: None,
                rating: Some(rating),
                changed_at: None,
            },
        )
        .await?;
        write.commit().await?;
        Ok(())
    }

    /// Flush success: the native rating becomes the written value, so a
    /// later user "clear rating" falls back to the value the `.itl` holds.
    pub async fn adopt_native_rating(&self, media_uri: &str, rating: i64) -> LibraryResult<()> {
        let mut write = self.write().await?;
        let connection = write.connection().await?;
        sqlx::query(
            "INSERT INTO native_media_state(media_uri,rating) VALUES(?1,?2)
             ON CONFLICT(media_uri) DO UPDATE SET rating=excluded.rating",
        )
        .bind(media_uri)
        .bind(rating)
        .execute(&mut *connection)
        .await?;
        write.commit().await?;
        Ok(())
    }

    /// Embedded-artwork bindings the last observation published for one
    /// source's tracks, keyed by media URI. The iTunes catalog reuses a
    /// binding verbatim while the track file revision (size + mtime) is
    /// unchanged, so refreshes only `stat` already-inspected files.
    pub async fn itunes_artwork_bindings(
        &self,
        source_key: SourceKey,
        _cancellation: &ReadCancellation,
    ) -> LibraryResult<HashMap<String, Vec<u8>>> {
        let mut connection = self.acquire_reader().await?;
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT track.media_uri,details.native_artwork
             FROM tracks track JOIN track_details details USING(track_key)
             WHERE track.source_key=?1 AND details.native_artwork IS NOT NULL",
        )
        .bind(source_key.raw())
        .fetch_all(&mut *connection)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Rating sync base rows of one source, keyed by media URI.
    pub async fn itunes_base(
        &self,
        source_key: SourceKey,
        _cancellation: &ReadCancellation,
    ) -> LibraryResult<HashMap<String, Option<i64>>> {
        let mut connection = self.acquire_reader().await?;
        let rows: Vec<(String, Option<i64>)> =
            sqlx::query_as("SELECT media_uri,rating FROM itunes_sync_base WHERE source_key=?1")
                .bind(source_key.raw())
                .fetch_all(&mut *connection)
                .await?;
        Ok(rows.into_iter().collect())
    }

    /// Upsert one base row (rating = the value last agreed with the `.itl`).
    pub async fn write_itunes_base(
        &self,
        source_key: SourceKey,
        media_uri: &str,
        rating: Option<i64>,
    ) -> LibraryResult<()> {
        let mut write = self.write().await?;
        let connection = write.connection().await?;
        sqlx::query(
            "INSERT INTO itunes_sync_base(source_key,media_uri,rating) VALUES(?1,?2,?3)
             ON CONFLICT(media_uri) DO UPDATE SET rating=excluded.rating",
        )
        .bind(source_key.raw())
        .bind(media_uri)
        .bind(rating)
        .execute(&mut *connection)
        .await?;
        write.commit().await?;
        Ok(())
    }

    /// Drop base rows of the source that are no longer in the `.itl`.
    pub async fn prune_itunes_base(
        &self,
        source_key: SourceKey,
        keep: &[String],
    ) -> LibraryResult<()> {
        let mut write = self.write().await?;
        let connection = write.connection().await?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM itunes_sync_base WHERE source_key=?1 AND media_uri NOT IN(
                 SELECT value FROM json_each(?2))"
        )))
        .bind(source_key.raw())
        .bind(serde_json::to_string(keep)?)
        .execute(&mut *connection)
        .await?;
        write.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CatalogBatch, SourceId, TrackMetadata, TrackObservation};

    /// The artwork blob lives in `track_details`, not `tracks` — the query
    /// must join. This guard exists because the corpus-gated iTunes
    /// integration tests skip on hosts without the corpus.
    #[tokio::test]
    #[allow(clippy::panic_in_result_fn)] // assertions are the test's point
    async fn itunes_artwork_bindings_read_the_details_blob() -> LibraryResult<()> {
        let directory = tempfile::tempdir()?;
        let catalog = crate::Catalog::open(directory.path().join("library.sqlite")).await?;
        catalog
            .reconcile_source(&SourceId::new("itunes-test"), "itunes", "iTunes")
            .await?;
        let source_key = catalog.source_key(&SourceId::new("itunes-test")).unwrap();
        let mut observation = catalog.observe("itunes-test").await?;
        let mut batch = CatalogBatch::default();
        batch.tracks.push(TrackObservation {
            object_id: "t1".into(),
            media_uri: Some("rufin:source/track/itunes-test/t1".into()),
            metadata: TrackMetadata {
                title: "One".into(),
                ..Default::default()
            },
            artwork_binding: Some(br#"[]"#.to_vec()),
            ..Default::default()
        });
        observation.apply_catalog(&batch, true).await?;
        let bindings = catalog
            .itunes_artwork_bindings(source_key, &ReadCancellation::new())
            .await?;
        assert_eq!(
            bindings
                .get("rufin:source/track/itunes-test/t1")
                .map(Vec::as_slice),
            Some(br#"[]"#.as_slice()),
            "the published binding round-trips through track_details"
        );
        Ok(())
    }
}
