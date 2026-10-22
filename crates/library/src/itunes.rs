//! iTunes source support: native listen bumps and the rating sync base that
//! make `.itl` ↔ player merges lossless.
//!
//! Counts and last-played never need a base row: the runtime write-back
//! writes the native slot verbatim, so at refresh the pending player delta
//! is exactly `native - itl` (clamped at zero). Ratings do: the base row is
//! the last rating agreed with the `.itl`, and the merge treats
//! `itl != base` as "iTunes moved, iTunes wins".

use sqlx::Connection;
use std::collections::HashMap;

use crate::{Database, LibraryError, LibraryResult, ReadCancellation, SourceKey};

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

impl Database {
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
        let mut writer = self.writer().await?;
        let connection = writer.as_mut().ok_or(LibraryError::WriterUnavailable)?;
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
        Ok(())
    }

    /// The values the iTunes write-back flushes: the full-precision user
    /// rating and the native stats row.
    pub async fn itunes_player_state(
        &self,
        media_uri: &str,
        cancellation: &ReadCancellation,
    ) -> LibraryResult<ItunesPlayerState> {
        let (_permit, mut connection) = self.acquire_general(cancellation).await?;
        let (user_rating, native_rating, play_count, skip_count, last_played): (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT (SELECT rating FROM user_media_state WHERE media_uri=?1),
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
        write_user_rating(media_uri, rating, self).await
    }

    /// Flush success: the native rating becomes the written value, so a
    /// later user "clear rating" falls back to the value the `.itl` holds.
    pub async fn adopt_native_rating(&self, media_uri: &str, rating: i64) -> LibraryResult<()> {
        let mut writer = self.writer().await?;
        let connection = writer.as_mut().ok_or(LibraryError::WriterUnavailable)?;
        sqlx::query(
            "INSERT INTO native_media_state(media_uri,rating) VALUES(?1,?2)
             ON CONFLICT(media_uri) DO UPDATE SET rating=excluded.rating",
        )
        .bind(media_uri)
        .bind(rating)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    /// Rating sync base rows of one source, keyed by media URI.
    pub async fn itunes_base(
        &self,
        source_key: SourceKey,
        cancellation: &ReadCancellation,
    ) -> LibraryResult<HashMap<String, Option<i64>>> {
        let (_permit, mut connection) = self.acquire_general(cancellation).await?;
        let rows: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT media_uri,rating FROM itunes_sync_base WHERE source_key=?1",
        )
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
        let mut writer = self.writer().await?;
        let connection = writer.as_mut().ok_or(LibraryError::WriterUnavailable)?;
        sqlx::query(
            "INSERT INTO itunes_sync_base(source_key,media_uri,rating) VALUES(?1,?2,?3)
             ON CONFLICT(media_uri) DO UPDATE SET rating=excluded.rating",
        )
        .bind(source_key.raw())
        .bind(media_uri)
        .bind(rating)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    /// Drop base rows of the source that are no longer in the `.itl`.
    pub async fn prune_itunes_base(
        &self,
        source_key: SourceKey,
        keep: &[String],
    ) -> LibraryResult<()> {
        let mut writer = self.writer().await?;
        let connection = writer.as_mut().ok_or(LibraryError::WriterUnavailable)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM itunes_sync_base WHERE source_key=?1 AND media_uri NOT IN(
                 SELECT value FROM json_each(?2))"
        )))
        .bind(source_key.raw())
        .bind(serde_json::to_string(keep)?)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }
}

async fn write_user_rating(
    media_uri: &str,
    rating: i64,
    database: &Database,
) -> LibraryResult<()> {
    let mut writer = database.writer().await?;
    let connection = writer.as_mut().ok_or(LibraryError::WriterUnavailable)?;
    let mut transaction = connection.begin().await?;
    sqlx::query(
        "INSERT INTO user_media_state(media_uri,rating) VALUES(?1,?2)
         ON CONFLICT(media_uri) DO UPDATE SET rating=excluded.rating",
    )
    .bind(media_uri)
    .bind(rating)
    .execute(&mut *transaction)
    .await?;
    crate::favorites::cleanup_empty_state(&mut transaction, media_uri).await?;
    transaction.commit().await?;
    Ok(())
}
