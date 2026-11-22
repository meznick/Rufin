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
//! - **play date — newest wins** in unix-local wall-clock space. The host
//!   raw value is Apple-epoch **plus** the `.itl` envelope tz offset
//!   (wall-clock local), so the conversion is raw − 2082844800 −
//!   `tz_offset_seconds` — the exact inverse of the dry-run comparison
//!   (`compute.rs`). The device value is libgpod's already-converted
//!   unix-local; pending-store mac times are the device firmware's
//!   wall-clock local, the same conversion. Writing the offset-stripped
//!   value matters: libgpod adds the pod's own timezone shift at the
//!   write boundary, so a value that still carries the host offset comes
//!   back shifted (the B-017 step-0 finding: 3384 uniform +3h play-date
//!   diffs after the first real sync).
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

use crate::mapping::{HostStats, HostTrackMetadata};
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
    /// The host's metadata, for the device-side repair: a field that
    /// differs from the parsed device value is patched back (the device
    /// DB can hold wiped metadata from builds before the 2026-11-22
    /// `update_track` fix).
    pub host_metadata: HostTrackMetadata,
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
/// baseline rule. `repair` carries the metadata fields the device DB
/// holds wrong (empty strings, zeroed scalars): `Some` = write the host
/// value, `None` = leave the parsed value alone.
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
    pub repair: MetadataRepair,
}

/// The metadata fields `update_track` writes when they differ from what
/// the device DB holds. `None` = untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataRepair {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub albumartist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub filetype: Option<String>,
    pub size: Option<u32>,
    pub tracklen: Option<i32>,
    pub cd_nr: Option<i32>,
    pub cds: Option<i32>,
    pub track_nr: Option<i32>,
    pub tracks: Option<i32>,
    pub year: Option<i32>,
    pub bitrate: Option<i32>,
    pub samplerate: Option<u16>,
}

impl MetadataRepair {
    /// Whether any field carries a repair.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The merge outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatMerge {
    pub host_patches: Vec<HostStatPatch>,
    pub device_patches: Vec<DeviceStatPatch>,
    pub diagnostics: Vec<String>,
}

/// The fields whose device DB value differs from the host's — `Some(host)`
/// where a repair must write. An empty device string counts as missing:
/// stat patches of builds before the 2026-11-22 `update_track` fix wiped
/// the parsed strings to null, and the repair heals those rows. A repair
/// is only ever emitted for a *meaningful* host value (non-empty string,
/// non-zero scalar): an absent host field never overwrites the device.
/// With `full` (the rebuild mode) the device comparison is skipped and
/// every meaningful host field is emitted, rewriting the row wholesale.
fn metadata_repair(
    host: &HostTrackMetadata,
    device: &ipod::TrackInfo,
    full: bool,
) -> MetadataRepair {
    let changed = |host: &Option<String>, device: &Option<String>| {
        let host = host.as_deref().unwrap_or("");
        let device = device.as_deref().unwrap_or("");
        (!host.is_empty() && (full || host != device)).then(|| host.to_string())
    };
    let optional_int = |host: Option<i32>, device: i32| {
        (host.is_some_and(|host| host != 0) && (full || host != Some(device)))
            .then_some(host)
            .flatten()
    };
    MetadataRepair {
        title: changed(&host.title, &device.title),
        artist: changed(&host.artist, &device.artist),
        album: changed(&host.album, &device.album),
        albumartist: changed(&host.album_artist, &device.albumartist),
        composer: changed(&host.composer, &device.composer),
        genre: changed(&host.genre, &device.genre),
        filetype: changed(&host.file_extension, &device.filetype),
        size: (host.size_bytes != 0 && (full || device.size != host.size_bytes))
            .then_some(host.size_bytes),
        tracklen: (host.duration_ms != 0 && (full || device.tracklen != host.duration_ms as i32))
            .then_some(host.duration_ms as i32),
        cd_nr: optional_int(host.disc_number.map(i32::from), device.cd_nr),
        cds: optional_int(host.disc_count.map(i32::from), device.cds),
        track_nr: optional_int(host.track_number.map(i32::from), device.track_nr),
        tracks: optional_int(host.track_count.map(i32::from), device.tracks),
        year: optional_int(host.year.map(i32::from), device.year),
        bitrate: (host.bit_rate != 0 && (full || device.bitrate != i32::from(host.bit_rate)))
            .then_some(i32::from(host.bit_rate)),
        samplerate: (host.sample_rate != 0
            && (full || device.samplerate != host.sample_rate as u16))
            .then_some(host.sample_rate as u16),
    }
}

/// Compute the merge over the matched pairs.
///
/// `tz_offset_seconds` is the `.itl` envelope offset (the value the dry
/// run compares with): the host raw play date is wall-clock local, so it
/// converts with the offset; the pending-store mac times are the device
/// firmware's wall-clock local and share the conversion. Everything the
/// merge emits is offset-stripped unix-local — the shape libgpod's write
/// boundary and itl-sync's host write both expect.
pub fn merge_stats(pairs: &[StatPair], tz_offset_seconds: i32, full: bool) -> StatMerge {
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

        // Play date: newest in unix-local space; the host raw value and
        // the pending-store mac times are wall-clock local (Apple epoch +
        // the envelope offset), the device value is libgpod's converted
        // unix-local — the offset strips the first two to the same space.
        let host_played_unix =
            i64::from(pair.host.play_date_raw) - APPLE_UNIX_DELTA - i64::from(tz_offset_seconds);
        let mut newest = host_played_unix.max(pair.device.time_played);
        if let Some(mac) = pair.pending.played_at_mac {
            newest = newest.max(i64::from(mac) - APPLE_UNIX_DELTA - i64::from(tz_offset_seconds));
        }

        let host_patch = HostStatPatch {
            itl_id: pair.itl_id,
            play_count: (merged_plays > pair.host.play_count).then_some(merged_plays),
            skip_count: (merged_skips > pair.host.skip_count).then_some(merged_skips),
            play_date_unix: (newest > host_played_unix).then_some(newest),
        };
        let repair = metadata_repair(&pair.host_metadata, &pair.device, full);
        let device_patch = DeviceStatPatch {
            dbid: pair.pid,
            playcount: merged_plays,
            playcount2: merged_plays,
            skipcount: merged_skips,
            time_played: newest,
            rating: u32::from(pair.host.rating),
            repair,
        };
        merge.host_patches.push(host_patch);
        // Only emit device patches that change something: a re-run over an
        // already-merged pair stays a no-op.
        let device_changed = device_patch.playcount != pair.device.playcount
            || device_patch.playcount2 != pair.device.playcount2
            || device_patch.skipcount != pair.device.skipcount
            || device_patch.time_played != pair.device.time_played
            || device_patch.rating != pair.device.rating
            || !device_patch.repair.is_empty();
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
            host_metadata: HostTrackMetadata::default(),
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
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 100, 10, 0),
                device_track(102, 100, 10, 0, 80),
                PendingTotals {
                    plays: 2,
                    ..Default::default()
                },
            )],
            0,
            false,
        );
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
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 103, 0, 0),
                device_track(102, 100, 0, 0, 80),
                PendingTotals {
                    plays: 2,
                    ..Default::default()
                },
            )],
            0,
            false,
        );
        assert_eq!(merge.host_patches[0].play_count, Some(105));
        assert_eq!(merge.device_patches[0].playcount, 105);
    }

    #[test]
    fn out_of_band_device_drift_is_guarded_not_lost() {
        // The device holds more than the host even without pending plays
        // (e.g. an iTunes sync the host never saw): the device total is
        // truth and the host catches up; the device itself needs no write.
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 90, 0, 0),
                device_track(100, 100, 0, 0, 80),
                PendingTotals::default(),
            )],
            0,
            false,
        );
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
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 100, 0, 0),
                device_track(108, 105, 0, 0, 80),
                PendingTotals {
                    plays: 3,
                    ..Default::default()
                },
            )],
            0,
            false,
        );
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
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 10, 5, 0),
                device_track(10, 10, 8, 0, 80),
                PendingTotals {
                    skips: 3,
                    ..Default::default()
                },
            )],
            0,
            false,
        );
        assert_eq!(merge.host_patches[0].skip_count, Some(8));
        assert!(
            merge.device_patches.is_empty(),
            "the device already holds the merged skip total"
        );
    }

    #[test]
    fn play_date_newest_wins_in_all_directions() {
        let raw = |unix: i64| (unix + APPLE_UNIX_DELTA) as u32;
        let host_newer = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 0, 0, raw(500)),
                device_track(0, 0, 0, 400, 80),
                PendingTotals::default(),
            )],
            0,
            false,
        );
        assert_eq!(host_newer.host_patches[0].play_date_unix, None);
        assert_eq!(
            host_newer.device_patches[0].time_played, 500,
            "device gets the host date"
        );

        let device_newer = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 0, 0, 0),
                device_track(0, 0, 0, 600, 80),
                PendingTotals::default(),
            )],
            0,
            false,
        );
        assert_eq!(device_newer.host_patches[0].play_date_unix, Some(600));
        assert!(
            device_newer.device_patches.is_empty(),
            "the device already holds the newest date"
        );

        let pending_newest = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 0, 0, 0),
                device_track(0, 0, 0, 0, 80),
                PendingTotals {
                    plays: 1,
                    played_at_mac: Some(raw(700)),
                    ..Default::default()
                },
            )],
            0,
            false,
        );
        assert_eq!(pending_newest.host_patches[0].play_date_unix, Some(700));
        assert_eq!(pending_newest.device_patches[0].time_played, 700);
    }

    #[test]
    fn rating_is_host_wins_and_pending_ratings_are_diagnosed() {
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(40, 0, 0, 0),
                device_track(0, 0, 0, 0, 100),
                PendingTotals {
                    rating_entries: 2,
                    ..Default::default()
                },
            )],
            0,
            false,
        );
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
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 100, 10, raw(100)),
                device_track(100, 100, 10, 100, 80),
                PendingTotals::default(),
            )],
            0,
            false,
        );
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
    fn the_envelope_tz_offset_strips_from_the_host_play_date() {
        // The B-017 step-0 finding: the raw `.itl` play date is wall-clock
        // local (Apple epoch + the envelope offset). Without stripping the
        // offset, the value handed to libgpod carries +3h, libgpod adds
        // the pod's own +3h at the write boundary, and every re-read
        // lands 3 hours ahead.
        let tz = 3 * 3600;
        let raw = |unix: i64| (unix + APPLE_UNIX_DELTA + i64::from(tz)) as u32;
        let host_newer = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 0, 0, raw(500)),
                device_track(0, 0, 0, 400, 80),
                PendingTotals::default(),
            )],
            tz,
            false,
        );
        assert_eq!(
            host_newer.device_patches[0].time_played, 500,
            "the device patch is offset-stripped unix, not local wall clock"
        );

        // An in-sync pair under the offset is a no-op on both sides.
        let merged = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 100, 10, raw(100)),
                device_track(100, 100, 10, 100, 80),
                PendingTotals::default(),
            )],
            tz,
            false,
        );
        assert!(merged.host_patches[0].play_date_unix.is_none());
        assert!(merged.device_patches.is_empty());

        // Pending-store mac times share the conversion.
        let pending = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, 0, 0, 0),
                device_track(0, 0, 0, 0, 80),
                PendingTotals {
                    plays: 1,
                    played_at_mac: Some(raw(700)),
                    ..Default::default()
                },
            )],
            tz,
            false,
        );
        assert_eq!(pending.device_patches[0].time_played, 700);
        assert_eq!(pending.host_patches[0].play_date_unix, Some(700));
    }

    #[test]
    fn saturation_never_wraps() {
        let merge = merge_stats(
            &[pair(
                1,
                0xA1,
                host_stats(80, u32::MAX, 0, 0),
                device_track(u32::MAX - 1, u32::MAX - 1, 0, 0, 80),
                PendingTotals {
                    plays: 5,
                    ..Default::default()
                },
            )],
            0,
            false,
        );
        assert_eq!(
            merge.host_patches[0].play_count, None,
            "the host is already at the saturated maximum"
        );
        assert_eq!(merge.device_patches[0].playcount, u32::MAX);
        assert_eq!(merge.device_patches[0].playcount2, u32::MAX);
    }

    fn host_with_metadata(title: &str, artist: &str) -> HostTrackMetadata {
        HostTrackMetadata {
            title: Some(title.to_string()),
            artist: Some(artist.to_string()),
            album: Some("Album".to_string()),
            album_artist: Some("Album Artist".to_string()),
            composer: Some("Composer".to_string()),
            genre: Some("Genre".to_string()),
            file_extension: Some("m4a".to_string()),
            size_bytes: 4096,
            duration_ms: 201_000,
            disc_number: Some(1),
            disc_count: Some(1),
            track_number: Some(3),
            track_count: Some(12),
            year: Some(2024),
            bit_rate: 256,
            sample_rate: 44_100,
            bpm: None,
        }
    }

    fn wiped_device_track() -> ipod::TrackInfo {
        ipod::TrackInfo {
            dbid: 0xA1,
            title: None,
            artist: None,
            album: None,
            albumartist: None,
            composer: None,
            genre: None,
            filetype: None,
            size: 0,
            tracklen: 0,
            cd_nr: 0,
            cds: 0,
            track_nr: 0,
            tracks: 0,
            year: 0,
            bitrate: 0,
            samplerate: 0,
            ..Default::default()
        }
    }

    #[test]
    fn wiped_device_metadata_is_repaired_from_the_host() {
        // The pre-fix update_track wiped strings and scalars on every stat
        // patch; the merge must heal them from the host .itl facts.
        let mut pair = pair(
            1,
            0xA1,
            host_stats(80, 0, 0, 0),
            wiped_device_track(),
            PendingTotals::default(),
        );
        pair.host_metadata = host_with_metadata("Title", "Artist");
        let merge = merge_stats(&[pair], 0, false);
        assert_eq!(merge.device_patches.len(), 1, "the repair is emitted");
        let repair = &merge.device_patches[0].repair;
        assert_eq!(repair.title.as_deref(), Some("Title"));
        assert_eq!(repair.artist.as_deref(), Some("Artist"));
        assert_eq!(repair.album.as_deref(), Some("Album"));
        assert_eq!(repair.albumartist.as_deref(), Some("Album Artist"));
        assert_eq!(repair.composer.as_deref(), Some("Composer"));
        assert_eq!(repair.genre.as_deref(), Some("Genre"));
        assert_eq!(repair.filetype.as_deref(), Some("m4a"));
        assert_eq!(repair.size, Some(4096));
        assert_eq!(repair.tracklen, Some(201_000));
        assert_eq!(repair.track_nr, Some(3));
        assert_eq!(repair.year, Some(2024));
        assert_eq!(repair.bitrate, Some(256));
        assert_eq!(repair.samplerate, Some(44_100));
    }

    #[test]
    fn matching_device_metadata_needs_no_repair() {
        // A healthy device row agrees with the host on every field: no
        // repair, and with equal stats no patch at all.
        let mut device = wiped_device_track();
        device.title = Some("Title".to_string());
        device.artist = Some("Artist".to_string());
        device.album = Some("Album".to_string());
        device.albumartist = Some("Album Artist".to_string());
        device.composer = Some("Composer".to_string());
        device.genre = Some("Genre".to_string());
        device.filetype = Some("m4a".to_string());
        device.size = 4096;
        device.tracklen = 201_000;
        device.cd_nr = 1;
        device.cds = 1;
        device.track_nr = 3;
        device.tracks = 12;
        device.year = 2024;
        device.bitrate = 256;
        device.samplerate = 44_100;
        device.rating = 80;
        let mut pair = pair(
            1,
            0xA1,
            host_stats(80, 0, 0, 0),
            device,
            PendingTotals::default(),
        );
        pair.host_metadata = host_with_metadata("Title", "Artist");
        let merge = merge_stats(&[pair], 0, false);
        assert!(
            merge.device_patches.is_empty(),
            "an agreeing row produces no patch"
        );
    }

    #[test]
    fn rebuild_rewrites_matching_metadata_wholesale() {
        // A healthy pair (device agrees on every field) is a no-op in the
        // normal merge — but the rebuild mode must still emit the full
        // rewrite, so a re-serialized DB is trustworthy regardless of the
        // parsed state.
        let host_metadata = host_with_metadata("Title", "Artist");
        let mut device = wiped_device_track();
        device.title = Some("Title".to_string());
        device.artist = Some("Artist".to_string());
        device.album = Some("Album".to_string());
        device.albumartist = Some("Album Artist".to_string());
        device.composer = Some("Composer".to_string());
        device.genre = Some("Genre".to_string());
        device.filetype = Some("m4a".to_string());
        device.size = 4096;
        device.tracklen = 201_000;
        device.cd_nr = 1;
        device.cds = 1;
        device.track_nr = 3;
        device.tracks = 12;
        device.year = 2024;
        device.bitrate = 256;
        device.samplerate = 44_100;
        device.rating = 80;
        let mut pair = pair(
            1,
            0xA1,
            host_stats(80, 0, 0, 0),
            device,
            PendingTotals::default(),
        );
        pair.host_metadata = host_metadata;
        assert!(
            merge_stats(&[pair.clone()], 0, false)
                .device_patches
                .is_empty(),
            "an agreeing pair is a no-op in the normal merge"
        );
        let rebuild = merge_stats(&[pair], 0, true);
        assert_eq!(rebuild.device_patches.len(), 1, "the rebuild always writes");
        let repair = &rebuild.device_patches[0].repair;
        assert_eq!(repair.title.as_deref(), Some("Title"));
        assert_eq!(repair.artist.as_deref(), Some("Artist"));
        assert_eq!(repair.tracklen, Some(201_000));
        assert_eq!(repair.year, Some(2024));
    }
}
