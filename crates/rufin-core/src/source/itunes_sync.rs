//! Debounced `.itl` write-back for iTunes sources.
//!
//! The player state (user rating slot, native stats slot) is committed by
//! the ordinary Rufin paths; this service watches the two iTunes touch
//! points, waits out a short debounce window to batch rapid edits, and then
//! flushes one `itl-sync sync --json -` invocation per dirty batch. Failed
//! writes stay dirty and are retried on the next user action or refresh —
//! the refresh merge re-derives the pending deltas from the player state,
//! so nothing is lost.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use library::{Database, ReadCancellation};
use sources::itunes::write::{ItunesPatch, write_patches};
use sources::itunes::{ITUNES_SOURCE_ID, ItunesSourceConfig};
use sources::SourceId;

use crate::settings::SettingsFile;
use crate::source::configured_source;

const DEBOUNCE: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Dirty {
    rating: bool,
    activity: bool,
}

impl Dirty {
    fn merge(&mut self, other: Self) {
        self.rating |= other.rating;
        self.activity |= other.activity;
    }
}

#[derive(Default)]
struct Pending {
    by_source: BTreeMap<String, BTreeMap<String, Dirty>>,
    scheduled: bool,
}

#[derive(Default)]
struct Inner {
    pending: Mutex<Pending>,
    flushing: AtomicBool,
}

/// Owned by the runtime shared state; created once per app start.
#[derive(Clone)]
pub(crate) struct ItunesSync {
    inner: Arc<Inner>,
    database: Arc<Database>,
    settings: SettingsFile,
    runtime: tokio::runtime::Handle,
}

impl ItunesSync {
    pub(crate) fn new(
        database: Arc<Database>,
        settings: SettingsFile,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner::default()),
            database,
            settings,
            runtime,
        }
    }

    /// A rating was set/cleared on an iTunes-source track.
    pub(crate) fn mark_rating(&self, source_id: &SourceId, media_uri: &str) {
        self.mark(source_id, media_uri, Dirty {
            rating: true,
            activity: false,
        });
    }

    /// A listen was recorded on an iTunes-source track (native slots already
    /// bumped by the caller).
    pub(crate) fn mark_activity(&self, source_id: &SourceId, media_uri: &str) {
        self.mark(source_id, media_uri, Dirty {
            rating: false,
            activity: true,
        });
    }

    fn mark(&self, source_id: &SourceId, media_uri: &str, dirty: Dirty) {
        let mut pending = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending
            .by_source
            .entry(source_id.as_str().to_string())
            .or_default()
            .entry(media_uri.to_string())
            .or_default()
            .merge(dirty);
        if pending.scheduled {
            return;
        }
        pending.scheduled = true;
        drop(pending);
        let inner = Arc::clone(&self.inner);
        let service = self.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(DEBOUNCE).await;
            {
                let mut pending = inner
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                pending.scheduled = false;
            }
            service.flush().await;
        });
    }

    /// One write-back pass over everything dirty. Serialized: overlapping
    /// triggers simply enqueue while a flush is in flight.
    async fn flush(&self) {
        if self.inner.flushing.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut pending.by_source)
        };
        for (source_id, dirty) in pending {
            self.flush_source(&SourceId::new(source_id), dirty).await;
        }
        self.inner.flushing.store(false, Ordering::Release);
    }

    async fn flush_source(
        &self,
        source_id: &SourceId,
        dirty: BTreeMap<String, Dirty>,
    ) {
        let Some(configuration) = ({
            let stored = self.settings.load();
            configured_source(&stored.sources, source_id)
                .ok()
                .map(|configured| configured.configuration)
        }) else {
            return;
        };
        if configuration.kind != ITUNES_SOURCE_ID {
            return;
        }
        let Ok(config) = ItunesSourceConfig::from_configuration(&configuration) else {
            tracing::warn!(%source_id, "could not decode the iTunes source configuration");
            return;
        };
        let Some(source_key) = self
            .database
            .source_identity_key(source_id)
            .await
            .ok()
            .flatten()
        else {
            return;
        };

        let mut patches = Vec::new();
        let mut base_ratings: Vec<(String, i64)> = Vec::new();
        for (media_uri, flags) in &dirty {
            let Ok(state) = self
                .database
                .itunes_player_state(media_uri, &ReadCancellation::new())
                .await
            else {
                continue;
            };
            let mut patch = ItunesPatch::default();
            if flags.rating {
                // The user override wins over the native slot; clearing the
                // override falls back to the source value (= last agreed
                // .itl rating), never to zero.
                if let Some(rating) = state.user_rating.or(state.native_rating) {
                    patch.rating = Some(rating.clamp(0, 100) as u8);
                }
            }
            if flags.activity {
                if let Some(play_count) = state.play_count {
                    patch.play_count = Some(play_count.clamp(0, u32::MAX as i64) as u32);
                }
                if let Some(skip_count) = state.skip_count {
                    patch.skip_count = Some(skip_count.clamp(0, u32::MAX as i64) as u32);
                }
                if let Some(last_played) = state.last_played {
                    patch.play_date_unix = Some(last_played);
                }
            }
            if !patch.is_empty() {
                if let Some(rating) = patch.rating {
                    base_ratings.push((media_uri.clone(), i64::from(rating)));
                }
                patches.push(patch);
            }
        }
        if patches.is_empty() {
            return;
        }
        match write_patches(&config, &patches).await {
            Ok(()) => {
                for (media_uri, rating) in &base_ratings {
                    if let Err(error) = self
                        .database
                        .write_itunes_base(source_key, media_uri, Some(*rating))
                        .await
                    {
                        tracing::warn!(%error, media_uri, "could not update the iTunes sync base");
                    }
                    if let Err(error) = self
                        .database
                        .adopt_native_rating(media_uri, *rating)
                        .await
                    {
                        tracing::warn!(%error, media_uri, "could not adopt the native rating");
                    }
                }
            }
            Err(error) => {
                // Keep the batch dirty: the next user action or refresh
                // re-derives the same deltas from the player state.
                let mut pending = self
                    .inner
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let entry = pending.by_source.entry(source_id.as_str().to_string()).or_default();
                for (media_uri, flags) in dirty {
                    entry.entry(media_uri).or_default().merge(flags);
                }
                tracing::warn!(
                    %error,
                    %source_id,
                    "iTunes write-back failed; {} tracks stay pending",
                    entry.len()
                );
            }
        }
    }
}

/// The listen hook for iTunes sources: bump the native counters (the
/// catalog index triggers refresh the displayed stats) and mark the track
/// dirty for write-back.
pub(crate) async fn record_itunes_listen(
    owner: &crate::source::SourceOwner,
    media_uri: &str,
    skipped: bool,
    started_at: i64,
) {
    let Some(source_id) = owner
        .shared
        .database
        .track_source_by_uri(media_uri, &ReadCancellation::new())
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    if owner
        .configuration(&source_id)
        .is_none_or(|configuration| configuration.kind != ITUNES_SOURCE_ID)
    {
        return;
    }
    if let Err(error) = owner
        .shared
        .database
        .record_native_listen(media_uri, skipped, started_at)
        .await
    {
        tracing::warn!(%error, media_uri, "could not record the iTunes native listen");
        return;
    }
    owner
        .shared
        .itunes_sync
        .mark_activity(&source_id, media_uri);
}
