//! B-010: the pure per-field stat merge (b010_plan §6, owner decision
//! §11-3). No I/O, no session, no clock — owned inputs in, owned patch
//! sets out.
//!
//! Policy per field:
//!
//! - **play/skip counts — additive** (corrected 2026-11-16, not max): the
//!   `Play Counts` file is the pending-delta store; host = baseline +
//!   pending, where baseline = max(host, stored device value) guards an
//!   out-of-band device drift without double-counting (the pending delta
//!   is part of the observed device total exactly once).
//! - **play date — newest wins** in unix-local wall-clock space (host raw
//!   Apple-epoch − 2082844800 == the device's unix-local; both are the
//!   same machine's wall clock, so no tz offset participates in the
//!   comparison).
//! - **rating — host-wins push** (Video/Classic firmware cannot author
//!   ratings); pending-entry ratings are ignored with a diagnostic.
//! - volume and skip date are out of scope (itl-sync has no patch fields;
//!   owner decision §11-4).
//!
//! The device patch sets `playcount2 = playcount` — the §6 baseline rule
//! that keeps a later Windows-iTunes sync from re-importing the merged
//! delta. Patches that change nothing are omitted, so a re-run over an
//! already-merged pair is a no-op (retry idempotency by construction;
//! the journal watermarks protect the crash windows).

use crate::mapping::HostStats;
use crate::sync::import::PendingTotals;

/// Seconds between the Apple (1904) and Unix (1970) epochs.
pub const APPLE_UNIX_DELTA: i64 = 2_082_844_800;

/// One matched pair the merge consumes.
#[derive(Clone, Debug)]
pub struct StatPair {
    /// The `.itl` numeric track id — the itl-sync patch key.
    pub itl_id: u32,
    /// Track PID = device dbid.
    pub pid: u64,
    pub host: HostStats,
    /// The wrapper's observation — stored values **plus** the pending
    /// merge libgpod applies at parse time.
    pub device: ipod::TrackInfo,
    pub pending: PendingTotals,
}

/// One host-side patch (the itl-sync shape; `None` fields are untouched).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostStatPatch {
    pub itl_id: u32,
    pub play_count: Option<u32>,
    pub skip_count: Option<u32>,
    /// Unix-local wall-clock seconds; itl-sync converts with the
    /// configured offset at the write boundary.
    pub play_date_unix: Option<i64>,
}

/// One device-side patch. `playcount2` always equals `playcount` — the
/// baseline rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceStatPatch {
    pub dbid: u64,
    pub playcount: u32,
    pub playcount2: u32,
    pub skipcount: u32,
    /// Unix-local wall-clock seconds.
    pub time_played: i64,
    /// 0–100, iTunes scale — the host value (host-wins).
    pub rating: u32,
}

/// The merge outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatMerge {
    pub host_patches: Vec<HostStatPatch>,
    pub device_patches: Vec<DeviceStatPatch>,
    pub diagnostics: Vec<String>,
}

/// Compute the merge over the matched pairs.
pub fn merge_stats(pairs: &[StatPair]) -> StatMerge {
    let mut merge = StatMerge::default();
    for pair in pairs {
        if pair.pending.rating_entries > 0 {
            merge.diagnostics.push(format!(
                "track {}: {} Play Counts rating entries ignored (Video/Classic firmware never writes them)",
                pair.pid, pair.pending.rating_entries
            ));
        }

        // Counts: additive with the out-of-band drift guard. The observed
        // device total already includes the pending delta (libgpod merges
        // at parse), so the stored value is observed − pending.
        let stored_plays = pair.device.playcount.saturating_sub(pair.pending.plays);
        let stored_skips = pair.device.skipcount.saturating_sub(pair.pending.skips);
        let baseline_plays = pair.host.play_count.max(stored_plays);
        let baseline_skips = pair.host.skip_count.max(stored_skips);
        let merged_plays = baseline_plays.saturating_add(pair.pending.plays);
        let merged_skips = baseline_skips.saturating_add(pair.pending.skips);

        // Play date: newest in unix-local space; the pending entries are
        // Mac-epoch, the same wall clock minus the fixed epoch delta.
        let host_played_unix = i64::from(pair.host.play_date_raw) - APPLE_UNIX_DELTA;
        let mut newest = host_played_unix.max(pair.device.time_played);
        if let Some(mac) = pair.pending.played_at_mac {
            newest = newest.max(i64::from(mac) - APPLE_UNIX_DELTA);
        }

        let host_patch = HostStatPatch {
            itl_id: pair.itl_id,
            play_count: (merged_plays > pair.host.play_count).then_some(merged_plays),
            skip_count: (merged_skips > pair.host.skip_count).then_some(merged_skips),
            play_date_unix: (newest > host_played_unix).then_some(newest),
        };
        let device_patch = DeviceStatPatch {
            dbid: pair.pid,
            playcount: merged_plays,
            playcount2: merged_plays,
            skipcount: merged_skips,
            time_played: newest,
            rating: u32::from(pair.host.rating),
        };
        merge.host_patches.push(host_patch);
        // Only emit device patches that change something: a re-run over an
        // already-merged pair stays a no-op.
        let device_changed = device_patch.playcount != pair.device.playcount
            || device_patch.playcount2 != pair.device.playcount2
            || device_patch.skipcount != pair.device.skipcount
            || device_patch.time_played != pair.device.time_played
            || device_patch.rating != pair.device.rating;
        if device_changed {
            merge.device_patches.push(device_patch);
        }
    }
    merge
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(
        itl_id: u32,
        pid: u64,
        host: HostStats,
        device: ipod::TrackInfo,
        pending: PendingTotals,
    ) -> StatPair {
        StatPair {
            itl_id,
            pid,
            host,
            device,
            pending,
        }
    }

    fn host_stats(rating: u8, plays: u32, skips: u32, date_raw: u32) -> HostStats {
        HostStats {
            rating,
            play_count: plays,
            skip_count: skips,
            play_date_raw: date_raw,
            volume_adjustment: 0,
        }
    }

    fn device_track(
        plays: u32,
        playcount2: u32,
        skips: u32,
        played: i64,
        rating: u32,
    ) -> ipod::TrackInfo {
        ipod::TrackInfo {
            dbid: 0,
            playcount: plays,
            playcount2,
            skipcount: skips,
            time_played: played,
            rating,
            ..Default::default()
        }
    }

    #[test]
    fn pending_plays_are_added_and_baseline_reset() {
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 100, 10, 0),
            device_track(102, 100, 10, 0, 80),
            PendingTotals {
                plays: 2,
                ..Default::default()
            },
        )]);
        assert_eq!(merge.diagnostics.len(), 0);
        assert_eq!(merge.host_patches.len(), 1);
        assert_eq!(merge.host_patches[0].play_count, Some(102));
        assert_eq!(merge.host_patches[0].skip_count, None);
        assert_eq!(merge.device_patches.len(), 1);
        assert_eq!(merge.device_patches[0].playcount, 102);
        assert_eq!(
            merge.device_patches[0].playcount2, 102,
            "the baseline rule: playcount2 == playcount"
        );
    }

    #[test]
    fn host_plays_and_pending_plays_sum() {
        // Host advanced 100→103 in Rufin; two pending device plays.
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 103, 0, 0),
            device_track(102, 100, 0, 0, 80),
            PendingTotals {
                plays: 2,
                ..Default::default()
            },
        )]);
        assert_eq!(merge.host_patches[0].play_count, Some(105));
        assert_eq!(merge.device_patches[0].playcount, 105);
    }

    #[test]
    fn out_of_band_device_drift_is_guarded_not_lost() {
        // The device holds more than the host even without pending plays
        // (e.g. an iTunes sync the host never saw): the device total is
        // truth and the host catches up; the device itself needs no write.
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 90, 0, 0),
            device_track(100, 100, 0, 0, 80),
            PendingTotals::default(),
        )]);
        assert_eq!(merge.host_patches[0].play_count, Some(100));
        assert!(
            merge.device_patches.is_empty(),
            "the device already holds the merged total"
        );
    }

    #[test]
    fn drift_and_pending_never_double_count() {
        // stored 105 (drifted), pending 3: merged = 105 + 3, the pending
        // delta participates exactly once even though the observed total
        // (108) already includes it — and the device already holds 108,
        // so only the host moves.
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 100, 0, 0),
            device_track(108, 105, 0, 0, 80),
            PendingTotals {
                plays: 3,
                ..Default::default()
            },
        )]);
        assert_eq!(merge.host_patches[0].play_count, Some(108));
        // The device already holds the merged playcount, but its baseline
        // (playcount2 = 105) must reset to the merged total — that IS the
        // change the device patch carries.
        assert_eq!(merge.device_patches.len(), 1);
        assert_eq!(merge.device_patches[0].playcount, 108);
        assert_eq!(merge.device_patches[0].playcount2, 108);
    }

    #[test]
    fn skips_are_additive_too() {
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 10, 5, 0),
            device_track(10, 10, 8, 0, 80),
            PendingTotals {
                skips: 3,
                ..Default::default()
            },
        )]);
        assert_eq!(merge.host_patches[0].skip_count, Some(8));
        assert!(
            merge.device_patches.is_empty(),
            "the device already holds the merged skip total"
        );
    }

    #[test]
    fn play_date_newest_wins_in_all_directions() {
        let raw = |unix: i64| (unix + APPLE_UNIX_DELTA) as u32;
        let host_newer = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 0, 0, raw(500)),
            device_track(0, 0, 0, 400, 80),
            PendingTotals::default(),
        )]);
        assert_eq!(host_newer.host_patches[0].play_date_unix, None);
        assert_eq!(
            host_newer.device_patches[0].time_played, 500,
            "device gets the host date"
        );

        let device_newer = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 0, 0, 0),
            device_track(0, 0, 0, 600, 80),
            PendingTotals::default(),
        )]);
        assert_eq!(device_newer.host_patches[0].play_date_unix, Some(600));
        assert!(
            device_newer.device_patches.is_empty(),
            "the device already holds the newest date"
        );

        let pending_newest = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 0, 0, 0),
            device_track(0, 0, 0, 0, 80),
            PendingTotals {
                plays: 1,
                played_at_mac: Some(raw(700)),
                ..Default::default()
            },
        )]);
        assert_eq!(pending_newest.host_patches[0].play_date_unix, Some(700));
        assert_eq!(pending_newest.device_patches[0].time_played, 700);
    }

    #[test]
    fn rating_is_host_wins_and_pending_ratings_are_diagnosed() {
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(40, 0, 0, 0),
            device_track(0, 0, 0, 0, 100),
            PendingTotals {
                rating_entries: 2,
                ..Default::default()
            },
        )]);
        assert_eq!(
            merge.host_patches[0].play_count, None,
            "rating never patches the host"
        );
        assert_eq!(merge.device_patches[0].rating, 40, "the downgrade survives");
        assert_eq!(merge.diagnostics.len(), 1);
        assert!(merge.diagnostics[0].contains("rating entries ignored"));
    }

    #[test]
    fn an_already_merged_pair_is_a_no_op() {
        let raw = |unix: i64| (unix + APPLE_UNIX_DELTA) as u32;
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, 100, 10, raw(100)),
            device_track(100, 100, 10, 100, 80),
            PendingTotals::default(),
        )]);
        assert!(merge.host_patches[0].play_count.is_none());
        assert!(merge.host_patches[0].skip_count.is_none());
        assert!(merge.host_patches[0].play_date_unix.is_none());
        assert!(
            merge.device_patches.is_empty(),
            "no device patch for a no-op pair"
        );
        assert!(merge.diagnostics.is_empty());
    }

    #[test]
    fn saturation_never_wraps() {
        let merge = merge_stats(&[pair(
            1,
            0xA1,
            host_stats(80, u32::MAX, 0, 0),
            device_track(u32::MAX - 1, u32::MAX - 1, 0, 0, 80),
            PendingTotals {
                plays: 5,
                ..Default::default()
            },
        )]);
        assert_eq!(
            merge.host_patches[0].play_count, None,
            "the host is already at the saturated maximum"
        );
        assert_eq!(merge.device_patches[0].playcount, u32::MAX);
        assert_eq!(merge.device_patches[0].playcount2, u32::MAX);
    }
}
