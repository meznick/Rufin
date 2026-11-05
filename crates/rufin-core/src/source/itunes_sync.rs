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

use async_channel::Sender;
use library::{Database, ReadCancellation, SourceKey};
use sources::itunes::write::{ItunesPatch, write_patches};
use sources::itunes::{ITUNES_SOURCE_ID, ItunesSourceConfig};
use sources::SourceId;

use crate::runtime::{SourceEvent, SourceNotice, SourceNoticeKind};
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
    /// Source ids already notified about the current busy period (the latch
    /// clears when a flush eventually succeeds, so the next busy period
    /// notifies again instead of spamming a toast per action).
    busy_notified: Mutex<BTreeMap<String, bool>>,
}

/// Owned by the runtime shared state; created once per app start.
#[derive(Clone)]
pub(crate) struct ItunesSync {
    inner: Arc<Inner>,
    database: Arc<Database>,
    settings: SettingsFile,
    runtime: tokio::runtime::Handle,
    events: Sender<SourceEvent>,
}

impl ItunesSync {
    pub(crate) fn new(
        database: Arc<Database>,
        settings: SettingsFile,
        runtime: tokio::runtime::Handle,
        events: Sender<SourceEvent>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner::default()),
            database,
            settings,
            runtime,
            events,
        }
    }

    /// One pass at app start: if any configured iTunes library is currently
    /// open in iTunes, tell the user (writes stay pending until it closes).
    pub(crate) fn startup_check(&self) {
        let service = self.clone();
        self.runtime.spawn(async move {
            let stored = service.settings.load();
            for item in &stored.sources.configured {
                if item.configuration.kind != ITUNES_SOURCE_ID {
                    continue;
                }
                let Ok(config) = ItunesSourceConfig::from_configuration(&item.configuration) else {
                    continue;
                };
                if !config.library_busy() {
                    continue;
                }
                let source_id = item.configuration.source_id.clone();
                let Some(source_key) = service
                    .database
                    .source_identity_key(&source_id)
                    .await
                    .ok()
                    .flatten()
                else {
                    continue;
                };
                service.notify_busy(&source_id, source_key).await;
            }
        });
    }

    /// Tell the user the library is open in iTunes, once per busy period.
    async fn notify_busy(&self, source_id: &SourceId, source_key: SourceKey) {
        let fresh = {
            let mut notified = self
                .inner
                .busy_notified
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            notified.insert(source_id.as_str().to_string(), true).is_none()
        };
        if fresh {
            tracing::warn!(
                %source_id,
                %source_key,
                "the iTunes library is open in iTunes — write-back stays pending until it closes",
            );
            let _ = self
                .events
                .send(SourceEvent::Notice(SourceNotice {
                    source_key,
                    kind: SourceNoticeKind::ItunesLibraryBusy,
                }))
                .await;
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

        if config.library_busy() {
            // iTunes has the library open: keep the batch dirty and tell the
            // user once per busy period. The authoritative refusal happens in
            // itl-sync anyway; this skips spawning the binary just to fail.
            {
                let mut pending = self
                    .inner
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let entry = pending.by_source.entry(source_id.as_str().to_string()).or_default();
                for (media_uri, flags) in &dirty {
                    entry.entry(media_uri.clone()).or_default().merge(*flags);
                }
            }
            self.notify_busy(source_id, source_key.clone()).await;
            return;
        }

        let mut patches = Vec::new();
        let mut base_ratings: Vec<(String, i64)> = Vec::new();
        for (media_uri, flags) in &dirty {
            // The `.itl` patch targets the numeric track id carried in the
            // media URI's object id (`itunes:track:<id>`, built by the
            // catalog through `source_entity_uri`). A patch without an id
            // would silently no-op against track 0 and — worse — still mark
            // the sync base as written, so the next refresh wipes the user
            // rating as "iTunes wins". Derive the id up front and drop the
            // entry (the refresh merge re-derives it anyway) when it cannot
            // be parsed.
            let Some(track_id) = itunes_track_id(media_uri) else {
                tracing::warn!(
                    %media_uri,
                    "cannot derive the iTunes track id from the media uri — dropping the patch",
                );
                continue;
            };
            let Ok(state) = self
                .database
                .itunes_player_state(media_uri, &ReadCancellation::new())
                .await
            else {
                continue;
            };
            let mut patch = ItunesPatch::default();
            patch.track_id = track_id;
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
                // the library was writable: reset the busy-notice latch so a
                // future busy period notifies again
                self.inner
                    .busy_notified
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(source_id.as_str());
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
                {
                    let mut pending = self
                        .inner
                        .pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let entry = pending.by_source.entry(source_id.as_str().to_string()).or_default();
                    for (media_uri, flags) in dirty {
                        entry.entry(media_uri).or_default().merge(flags);
                    }
                }
                // a sentinel appearing between the pre-check and the binary
                // run is a busy refusal too — tell the user about it
                if config.library_busy() {
                    self.notify_busy(source_id, source_key.clone()).await;
                }
                tracing::warn!(
                    %error,
                    %source_id,
                    "iTunes write-back failed; the batch stays pending",
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

/// The numeric `.itl` track id carried in an iTunes media URI
/// (`rufin:source/track/<source>/itunes:track:<id>`; `source_entity_parts`
/// decodes the percent-encoding the catalog applied).
fn itunes_track_id(media_uri: &str) -> Option<u32> {
    let (_, kind, object_id) = library::source_entity_parts(media_uri)?;
    if kind != "track" {
        return None;
    }
    object_id
        .strip_prefix("itunes:track:")?
        .parse::<u32>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::itunes_track_id;

    #[test]
    fn parses_the_track_id_from_an_itunes_media_uri() {
        assert_eq!(
            itunes_track_id(
                "rufin:source/track/rufin%2Dsource%2D52882a9a6993fc026780b247dab7704f/itunes%3Atrack%3A4035"
            ),
            Some(4035)
        );
    }

    #[test]
    fn rejects_uris_that_do_not_carry_an_itunes_track() {
        // album kind
        assert_eq!(
            itunes_track_id("rufin:source/album/x/itunes%3Atrack%3A4035"),
            None
        );
        // wrong object-id prefix
        assert_eq!(
            itunes_track_id("rufin:source/track/x/other%3Aobject%3A4035"),
            None
        );
        // non-numeric id
        assert_eq!(
            itunes_track_id("rufin:source/track/x/itunes%3Atrack%3Anan"),
            None
        );
        // not a source entity uri at all
        assert_eq!(itunes_track_id("garbage"), None);
    }
}
