//! B-010: the executor — the §5 transaction protocol end to end
//! (b010_plan §7). Revalidation → journal intent → backup → import-first
//! host write → file/playlist/artwork ops → staged DB write → verification
//! → completion, with recovery from the journal at every entry point and
//! a fault hook the failure-injection battery drives.
//!
//! The approved plan (hash + action lists) is the caller's — the dry run
//! the user reviewed. A freshly re-run dry run must hash identically
//! before the first write. The stat merge is computed from the *same*
//! inputs at the host step and the device step (the pending store is
//! untouched until the DB write consumes it, so both calls agree), with
//! the journal watermarks as the dedup memory.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dry_run::{CriteriaState, snapshot};
use crate::dry_run::{HostInputs, PlaylistKind, SyncOptions, compute};
use crate::mapping::map_tracks;
use crate::sync::backup::{backup_consistency_set, restore_consistency_set};
use crate::sync::exclusion::ExclusionGuard;
use crate::sync::host::{HostWriteContext, write_host_patches};
use crate::sync::import::{align_pending, parse_play_counts};
use crate::sync::journal::{Journal, OperationRecord, Watermark};
use crate::sync::merge::{APPLE_UNIX_DELTA, StatMerge, StatPair, merge_stats};
use crate::sync::plan::{ExecutionPlan, plan_hash};
use crate::sync::recover::{RecoveryPlan, plan_recovery};
use crate::sync::transcode::{FileWriteMode, decide_file_write};

/// The ordered steps; the journal records completion by these names.
pub const STEP_INTENT: &str = "Intent";
pub const STEP_BACKUP: &str = "Backup";
pub const STEP_HOST_WRITE: &str = "HostWrite";
pub const STEP_FILE_OPS: &str = "FileOps";
pub const STEP_DB_WRITE: &str = "DbWrite";
pub const STEP_VERIFY: &str = "Verify";

/// Every step, in execution order (the failure battery iterates this).
pub const STEPS: [&str; 6] = [
    STEP_INTENT,
    STEP_BACKUP,
    STEP_HOST_WRITE,
    STEP_FILE_OPS,
    STEP_DB_WRITE,
    STEP_VERIFY,
];

/// What the run reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Completed,
    Partial {
        failed_step: &'static str,
        diagnostic: String,
    },
}

/// The run's outcome (S-10: only a full completion advances last-success).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncOutcome {
    pub status: SyncStatus,
    pub host_patches_written: usize,
    pub device_patches_written: usize,
    pub files_added: usize,
    pub files_removed: usize,
    pub playlists_written: usize,
    pub artwork_added: usize,
    pub diagnostics: Vec<String>,
}

/// A step failed — the journal is retained and the operation is resumable.
#[derive(Debug)]
pub struct SyncError {
    pub step: &'static str,
    pub diagnostic: String,
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sync failed at {}: {}", self.step, self.diagnostic)
    }
}

impl std::error::Error for SyncError {}

/// Everything the executor needs; all caller-supplied (the B-015 sync
/// boundary wires it from the selected source and the approved dry run).
pub struct SyncContext<'a> {
    pub mount: &'a Path,
    pub backend: &'a ipod::IpodBackend,
    pub host: &'a HostInputs,
    pub options: &'a SyncOptions,
    pub host_write: &'a HostWriteContext,
    pub device_identity: &'a str,
    /// The approved plan — the dry run the user reviewed (hash + lists).
    pub plan: &'a ExecutionPlan,
    /// Transcode a host file to an AAC-256 m4a at `destination`
    /// (the B-010a provider — `audio_processing::transcode_to_m4a`).
    pub transcode: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
    /// Encoded artwork for a pid, or `None` when the cover is unavailable
    /// (reported, never a failure of the run).
    pub artwork: &'a dyn Fn(u64) -> Option<ipod::ArtworkInput>,
    pub cancelled: &'a dyn Fn() -> bool,
    /// Test fault injection: return true to simulate a crash right after
    /// the named step's journal write. Production passes `|_| false`.
    pub fail_after: &'a dyn Fn(&str) -> bool,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn sync_err(step: &'static str, message: impl Into<String>) -> SyncError {
    SyncError {
        step,
        diagnostic: message.into(),
    }
}

/// Run the sync transaction. Resumable: a `SyncError` leaves the journal
/// holding every completed step; the next call recovers from it.
pub fn run_sync(ctx: &SyncContext<'_>) -> Result<SyncOutcome, SyncError> {
    let journal = Journal::open(ctx.mount)
        .map_err(|error| sync_err(STEP_INTENT, format!("journal unavailable: {error}")))?;
    // Competing-writer exclusion — the refusal the spec wants before any
    // destructive write (S-12). Held for the whole operation.
    let _guard = ExclusionGuard::acquire(ctx.mount)
        .map_err(|error| sync_err(STEP_INTENT, error.to_string()))?;

    // Recovery from an interrupted operation.
    let active = journal
        .load_active()
        .map_err(|error| sync_err(STEP_INTENT, format!("journal unreadable: {error}")))?;
    let db_parses = ipod::DeviceSession::open(ctx.backend, ctx.mount).is_ok();
    let mut outcome = SyncOutcome {
        status: SyncStatus::Completed,
        host_patches_written: 0,
        device_patches_written: 0,
        files_added: 0,
        files_removed: 0,
        playlists_written: 0,
        artwork_added: 0,
        diagnostics: Vec::new(),
    };
    let mut record = match plan_recovery(active, db_parses) {
        RecoveryPlan::None => None,
        RecoveryPlan::Resume(record) => {
            outcome.diagnostics.push(format!(
                "resuming an interrupted operation ({}/6 steps completed)",
                record.completed_steps.len()
            ));
            Some(record)
        }
        RecoveryPlan::VerifyAndAdvance(_record) => {
            verify_device(ctx)?;
            // The DB write already consumed the pending store — the
            // completion housekeeping belongs to this path too.
            remove_consumed_pending(ctx.mount)?;
            journal
                .complete(now_unix())
                .map_err(|error| sync_err(STEP_VERIFY, error.to_string()))?;
            outcome.status = SyncStatus::Completed;
            outcome
                .diagnostics
                .push("recovered: verified and advanced".into());
            return Ok(outcome);
        }
        RecoveryPlan::Restore(record) => {
            let backup = record
                .backup_dir
                .as_deref()
                .ok_or_else(|| sync_err(STEP_INTENT, "journal records no backup to restore"))?;
            restore_consistency_set(ctx.mount, &journal.root().join(backup))
                .map_err(|error| sync_err(STEP_INTENT, format!("restore failed: {error}")))?;
            // The backup restores the pre-write pending store; a stray
            // `.bak` from the interrupted write is its consumed duplicate.
            remove_consumed_pending(ctx.mount)?;
            journal.mark_failed().map_err(|error| {
                sync_err(STEP_INTENT, format!("journal update failed: {error}"))
            })?;
            outcome.status = SyncStatus::Partial {
                failed_step: STEP_INTENT,
                diagnostic: "recovered: consistency set restored from the backup; the interrupted \
                     operation is recorded as failed"
                    .into(),
            };
            return Ok(outcome);
        }
    };
    let done: HashSet<String> = record
        .as_ref()
        .map(|record| record.completed_steps.iter().cloned().collect())
        .unwrap_or_default();
    let record = record.get_or_insert_with(|| {
        OperationRecord::new(
            ctx.device_identity.to_string(),
            ctx.plan.hash.clone(),
            now_unix(),
        )
    });
    let step_done = |record: &mut OperationRecord,
                     journal: &Journal,
                     step: &'static str|
     -> Result<(), SyncError> {
        record.step_done(step);
        journal
            .write(record)
            .map_err(|error| sync_err(step, format!("journal write failed: {error}")))?;
        if (ctx.fail_after)(step) {
            return Err(sync_err(step, "injected failure after the step committed"));
        }
        Ok(())
    };

    // 1. Revalidation (fresh plan only).
    if !done.contains(STEP_INTENT) {
        revalidate(ctx)?;
        step_done(record, &journal, STEP_INTENT)?;
    }
    // 2. Consistency-set backup.
    if !done.contains(STEP_BACKUP) {
        let backup = backup_consistency_set(ctx.mount, journal.root())
            .map_err(|error| sync_err(STEP_BACKUP, format!("backup failed: {error}")))?;
        record.backup_dir = Some(
            backup
                .strip_prefix(journal.root())
                .unwrap_or(&backup)
                .to_string_lossy()
                .into_owned(),
        );
        step_done(record, &journal, STEP_BACKUP)?;
    }
    // 3. Stat import and host write (import-first ordering).
    if !done.contains(STEP_HOST_WRITE) {
        let merge = compute_merge(ctx, record)?;
        write_host_patches(ctx.host_write, &merge.host_patches)
            .map_err(|error| sync_err(STEP_HOST_WRITE, error.to_string()))?;
        outcome.host_patches_written = merge.host_patches.len();
        outcome
            .diagnostics
            .extend(merge.diagnostics.iter().cloned());
        step_done(record, &journal, STEP_HOST_WRITE)?;
    }
    // 4-5. Device mutations and the staged DB write share ONE session —
    // the in-memory model mutated by the ops is exactly what gets
    // serialized (a fresh session would re-parse the unmodified DB).
    // FileOps re-runs on every resume until the DB write commits: a
    // crashed session loses its in-memory mutations, while the DB write
    // itself is an idempotent re-serialization of the parsed state.
    let mut write_session = ipod::DeviceSession::open(ctx.backend, ctx.mount)
        .map_err(|error| sync_err(STEP_FILE_OPS, format!("write session failed: {error}")))?;
    if !done.contains(STEP_DB_WRITE) {
        apply_device_ops(ctx, &mut write_session, record, &mut outcome)?;
        step_done(record, &journal, STEP_FILE_OPS)?;
    }
    write_db_staged(ctx, &mut write_session)?;
    step_done(record, &journal, STEP_DB_WRITE)?;
    drop(write_session);
    // 6. Post-write verification.
    if !done.contains(STEP_VERIFY) {
        verify_device(ctx)?;
        step_done(record, &journal, STEP_VERIFY)?;
    }
    // 7. Completion — the only place last-success advances (S-10).
    remove_consumed_pending(ctx.mount)?;
    journal
        .complete(now_unix())
        .map_err(|error| sync_err(STEP_VERIFY, format!("journal completion failed: {error}")))?;
    outcome.status = SyncStatus::Completed;
    Ok(outcome)
}

/// Re-run the dry run against a fresh session and refuse on any mismatch
/// with the approved hash, plus the free-space check that counts the
/// on-device overhead (backup + staged DB + journal, owner decision
/// §11-8).
fn revalidate(ctx: &SyncContext<'_>) -> Result<(), SyncError> {
    if (ctx.cancelled)() {
        return Err(sync_err(STEP_INTENT, "cancelled"));
    }
    let session = ipod::DeviceSession::open(ctx.backend, ctx.mount)
        .map_err(|error| sync_err(STEP_INTENT, format!("device re-parse failed: {error}")))?;
    let facts = crate::retrieval::collect(&session, ctx.mount)
        .map_err(|error| sync_err(STEP_INTENT, format!("device facts failed: {error}")))?;
    let fresh_device = snapshot::collect_device(&session, ctx.mount, &facts)
        .map_err(|error| sync_err(STEP_INTENT, format!("device collection failed: {error}")))?;
    let fresh = compute(ctx.host, &fresh_device, ctx.options);
    if plan_hash(&fresh) != ctx.plan.hash {
        return Err(sync_err(
            STEP_INTENT,
            "the device changed since the approved dry run — re-run the dry run",
        ));
    }
    let overhead = consistency_overhead(ctx.mount)
        .map_err(|error| sync_err(STEP_INTENT, error.to_string()))?;
    let free = fresh_device
        .free_bytes
        .ok_or_else(|| sync_err(STEP_INTENT, "device capacity unknown"))?;
    if fresh.capacity.estimate_bytes.saturating_add(overhead) > free {
        return Err(sync_err(
            STEP_INTENT,
            format!(
                "not enough space: {} needed ({} adds + {} overhead), {} free",
                fresh.capacity.estimate_bytes + overhead,
                fresh.capacity.estimate_bytes,
                overhead,
                free
            ),
        ));
    }
    Ok(())
}

/// The on-device overhead the space check must count: the consistency-set
/// sizes (the backup), the staged DB copy, and journal slack.
fn consistency_overhead(mount: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    let ipod_control = mount.join("iPod_Control");
    for relative in crate::sync::backup::CONSISTENCY_SET {
        if let Ok(metadata) = std::fs::metadata(ipod_control.join(relative)) {
            total = total.saturating_add(metadata.len());
        }
    }
    if let Ok(entries) = std::fs::read_dir(ipod_control.join("Artwork")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".ithmb")
                && let Ok(metadata) = entry.metadata()
            {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    // The staged DB is roughly the iTunesDB size again; journal slack.
    if let Ok(metadata) = std::fs::metadata(ipod_control.join("iTunes/iTunesDB")) {
        total = total.saturating_add(metadata.len());
    }
    Ok(total.saturating_add(1024 * 1024))
}

/// Parse both pending files, align to the fresh track order, merge over
/// the matched pairs (deduping against the journal watermarks) and record
/// the applied-delta watermarks in the journal **before** the host write.
/// The same inputs are available at the device step (the pending store is
/// consumed only by the DB write), so both calls agree by construction.
fn compute_merge(
    ctx: &SyncContext<'_>,
    record: &mut OperationRecord,
) -> Result<StatMerge, SyncError> {
    let session = ipod::DeviceSession::open(ctx.backend, ctx.mount)
        .map_err(|error| sync_err(STEP_HOST_WRITE, format!("device re-parse failed: {error}")))?;
    let tracks = session
        .tracks()
        .map_err(|error| sync_err(STEP_HOST_WRITE, format!("track read failed: {error}")))?;
    let ordered_dbids: Vec<u64> = tracks.iter().map(|track| track.dbid).collect();
    let mapping = map_tracks(&ctx.host.tracks, &tracks);
    let itunes = ctx.mount.join("iPod_Control/iTunes");
    let mut entries = Vec::new();
    for name in ["Play Counts", "Play Counts.bak"] {
        let path = itunes.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => match parse_play_counts(&bytes) {
                Ok(parsed) => entries.extend(parsed),
                Err(error) => {
                    return Err(sync_err(
                        STEP_HOST_WRITE,
                        format!("{name} is unreadable: {error}"),
                    ));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(sync_err(
                    STEP_HOST_WRITE,
                    format!("{name} read failed: {error}"),
                ));
            }
        }
    }
    let (mut pending, _align_diagnostics) = align_pending(&entries, &ordered_dbids);
    let previous_watermarks: HashMap<u64, Watermark> = record
        .watermarks
        .iter()
        .map(|watermark| (watermark.dbid, *watermark))
        .collect();
    let host_by_itl: HashMap<u32, &crate::mapping::HostTrack> = ctx
        .host
        .tracks
        .iter()
        .map(|track| (track.itl_id, track))
        .collect();
    let mut pairs = Vec::new();
    let mut watermarks = Vec::new();
    for matched in mapping.matched() {
        let crate::mapping::TrackOutcome::Matched { dbid } = matched.outcome else {
            continue;
        };
        let Some(host_track) = host_by_itl.get(&matched.itl_id) else {
            continue;
        };
        let Some(device_track) = tracks.iter().find(|track| track.dbid == dbid) else {
            continue;
        };
        let mut totals = pending.remove(&dbid).unwrap_or_default();
        if let Some(watermark) = previous_watermarks.get(&dbid) {
            // Entries already applied by an interrupted run: subtract them
            // so only newer entries land this time.
            totals.plays = totals.plays.saturating_sub(watermark.plays);
            totals.skips = totals.skips.saturating_sub(watermark.skips);
        }
        watermarks.push(Watermark {
            dbid,
            plays: totals.plays,
            skips: totals.skips,
        });
        pairs.push(StatPair {
            itl_id: host_track.itl_id,
            pid: host_track.track_pid,
            host: host_track.stats.clone(),
            device: device_track.clone(),
            pending: totals,
        });
    }
    record.watermarks = watermarks;
    Ok(merge_stats(&pairs))
}

/// Step 4: all device-side mutations in one write session — transcode and
/// copy adds, repair, remove, playlists, artwork, stat patches.
fn apply_device_ops(
    ctx: &SyncContext<'_>,
    session: &mut ipod::DeviceSession<'_>,
    record: &mut OperationRecord,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError> {
    let host_by_pid: HashMap<u64, &crate::mapping::HostTrack> = ctx
        .host
        .tracks
        .iter()
        .filter(|track| track.track_pid != 0)
        .map(|track| (track.track_pid, track))
        .collect();

    // File adds — the owner's transcode rule decides the mode per file.
    for add in &ctx.plan.file_adds {
        if (ctx.cancelled)() {
            return Err(sync_err(STEP_FILE_OPS, "cancelled"));
        }
        let Some(host_track) = host_by_pid.get(&add.pid) else {
            outcome
                .diagnostics
                .push(format!("add {}: host record vanished", add.pid));
            continue;
        };
        let decision = decide_file_write(
            host_track.metadata.file_extension.as_deref(),
            Some(u32::from(host_track.metadata.bit_rate)),
        );
        if decision.mode == FileWriteMode::NeverCopy {
            outcome.diagnostics.push(format!(
                "add {}: not copied (PDF booklet, iTunes never copies it)",
                add.pid
            ));
            continue;
        }
        let dbid = add.pid;
        session
            .create_track(&ipod::TrackMetadata {
                title: host_track.metadata.title.clone(),
                artist: host_track.metadata.artist.clone(),
                album: host_track.metadata.album.clone(),
                albumartist: host_track.metadata.album_artist.clone(),
                composer: host_track.metadata.composer.clone(),
                genre: host_track.metadata.genre.clone(),
                filetype: host_track.metadata.file_extension.clone(),
                size: host_track.metadata.size_bytes,
                tracklen: host_track.metadata.duration_ms as i32,
                cd_nr: host_track.metadata.disc_number.map(i32::from).unwrap_or(0),
                cds: host_track.metadata.disc_count.map(i32::from).unwrap_or(0),
                track_nr: host_track.metadata.track_number.map(i32::from).unwrap_or(0),
                tracks: host_track.metadata.track_count.map(i32::from).unwrap_or(0),
                year: host_track.metadata.year.map(i32::from).unwrap_or(0),
                bitrate: i32::from(host_track.metadata.bit_rate),
                samplerate: host_track.metadata.sample_rate as u16,
                rating: u32::from(host_track.stats.rating),
                dbid: Some(dbid),
                playcount: Some(host_track.stats.play_count),
                playcount2: Some(host_track.stats.play_count),
                skipcount: Some(host_track.stats.skip_count),
                time_played: Some(i64::from(host_track.stats.play_date_raw) - APPLE_UNIX_DELTA),
                volume: Some(host_track.stats.volume_adjustment),
            })
            .map_err(|error| {
                sync_err(STEP_FILE_OPS, format!("create_track {}: {error}", add.pid))
            })?;
        let source: PathBuf = if decision.mode == FileWriteMode::TranscodeToAac256 {
            let staging_root =
                crate::sync::transcode::staging_root(&std::env::temp_dir(), ctx.device_identity);
            crate::sync::transcode::create_staging_dir(&staging_root)
                .map_err(|error| sync_err(STEP_FILE_OPS, format!("staging: {error}")))?;
            let staged = crate::sync::transcode::staging_path_for(&staging_root, add.pid);
            (ctx.transcode)(&add.host_path, &staged).map_err(|error| {
                sync_err(STEP_FILE_OPS, format!("transcode {}: {error}", add.pid))
            })?;
            staged
        } else {
            add.host_path.clone()
        };
        session
            .copy_track(dbid, &source)
            .map_err(|error| sync_err(STEP_FILE_OPS, format!("copy {}: {error}", add.pid)))?;
        let _ = std::fs::remove_file(&source);
        outcome.files_added += 1;
    }

    // Repairs: the DB record exists, the media file is gone.
    for repair in &ctx.plan.file_repairs {
        session
            .copy_track(repair.dbid, &repair.host_path)
            .map_err(|error| sync_err(STEP_FILE_OPS, format!("repair {}: {error}", repair.dbid)))?;
        outcome.files_added += 1;
    }

    // Removals: the DB record and the media file.
    for remove in &ctx.plan.file_removes {
        session
            .remove_track(remove.dbid)
            .map_err(|error| sync_err(STEP_FILE_OPS, format!("remove {}: {error}", remove.dbid)))?;
        let on_disk = ctx.mount.join(
            remove
                .device_path
                .to_string_lossy()
                .trim_start_matches(':')
                .replace(':', "/"),
        );
        match std::fs::remove_file(&on_disk) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(sync_err(
                    STEP_FILE_OPS,
                    format!("media delete {}: {error}", on_disk.display()),
                ));
            }
        }
        outcome.files_removed += 1;
    }

    // Device stat patches — the same merge the host step applied (the
    // pending store is still untouched, so the inputs agree).
    let merge = compute_merge(ctx, record)?;
    for patch in &merge.device_patches {
        session
            .update_track(
                patch.dbid,
                &ipod::TrackMetadata {
                    playcount: Some(patch.playcount),
                    playcount2: Some(patch.playcount2),
                    skipcount: Some(patch.skipcount),
                    time_played: Some(patch.time_played),
                    rating: patch.rating,
                    ..Default::default()
                },
            )
            .map_err(|error| {
                sync_err(STEP_FILE_OPS, format!("stat patch {}: {error}", patch.dbid))
            })?;
        outcome.device_patches_written += 1;
    }

    // Playlists: static creates, updates, removes. Smart criteria writes
    // are the §8 translation — deferred with a diagnostic, never silently
    // converted to static (b010_plan §8).
    for create in &ctx.plan.playlist_creates {
        match create.kind {
            PlaylistKind::Static => {
                let members: Vec<u64> = create
                    .member_pids
                    .iter()
                    .filter_map(|member| *member)
                    .collect();
                session
                    .create_playlist(
                        create.name.as_deref().unwrap_or(""),
                        create.host_pid,
                        &members,
                        false,
                    )
                    .map_err(|error| {
                        sync_err(STEP_FILE_OPS, format!("create playlist: {error}"))
                    })?;
                outcome.playlists_written += 1;
            }
            PlaylistKind::Smart => {
                outcome.diagnostics.push(format!(
                    "smart playlist {:?} ({}) criteria write deferred (§8 translation)",
                    create.name, create.host_pid
                ));
            }
        }
    }
    for update in &ctx.plan.playlist_updates {
        if update.is_spl {
            if !matches!(update.criteria, CriteriaState::Equal) {
                outcome.diagnostics.push(format!(
                    "smart playlist {} criteria update deferred (§8 translation)",
                    update.device_id
                ));
            }
            continue;
        }
        let current: Vec<u64> = session
            .playlists()
            .map_err(|error| sync_err(STEP_FILE_OPS, format!("playlist read failed: {error}")))?
            .into_iter()
            .find(|playlist| playlist.id == update.device_id)
            .map(|playlist| playlist.member_dbids)
            .unwrap_or_default();
        let mut ordered: Vec<u64> = current
            .into_iter()
            .filter(|dbid| !update.members_remove.contains(dbid))
            .collect();
        ordered.extend(update.members_add.iter().copied());
        session
            .update_playlist_members(update.device_id, &ordered)
            .map_err(|error| {
                sync_err(
                    STEP_FILE_OPS,
                    format!("playlist update {}: {error}", update.device_id),
                )
            })?;
        outcome.playlists_written += 1;
    }
    for remove in &ctx.plan.playlist_removes {
        session.remove_playlist(remove.device_id).map_err(|error| {
            sync_err(
                STEP_FILE_OPS,
                format!("playlist remove {}: {error}", remove.device_id),
            )
        })?;
        outcome.playlists_written += 1;
    }

    // Artwork adds (no removals, ever — owner decision §11-6).
    for add in &ctx.plan.artwork_adds {
        match (ctx.artwork)(add.pid) {
            Some(input) => {
                session.set_artwork(add.pid, &input).map_err(|error| {
                    sync_err(STEP_FILE_OPS, format!("artwork {}: {error}", add.pid))
                })?;
                outcome.artwork_added += 1;
            }
            None => outcome
                .diagnostics
                .push(format!("artwork {} unavailable at the provider", add.pid)),
        }
    }
    Ok(())
}

/// Step 5: the staged DB write (never `itdb_write`) from the mutation
/// session, verification of the staged file on a disposable copy, atomic
/// rename, flush.
fn write_db_staged(
    ctx: &SyncContext<'_>,
    session: &mut ipod::DeviceSession<'_>,
) -> Result<(), SyncError> {
    let itunes = ctx.mount.join("iPod_Control/iTunes");
    let staged = itunes.join("iTunesDB.staged");
    session
        .write_library_to(&staged)
        .map_err(|error| sync_err(STEP_DB_WRITE, error.to_string()))?;

    // Verify the staged file parses on a disposable copy of the DB's own
    // dependencies (iTunes/, Artwork/, Device/ — never the media folder).
    let verify_root =
        crate::sync::transcode::staging_root(&std::env::temp_dir(), ctx.device_identity)
            .join("db-verify");
    let _ = std::fs::remove_dir_all(&verify_root);
    for dir in ["iTunes", "Artwork", "Device"] {
        let source = ctx.mount.join("iPod_Control").join(dir);
        if source.is_dir() {
            copy_tree(&source, &verify_root.join("iPod_Control").join(dir)).map_err(|error| {
                sync_err(
                    STEP_DB_WRITE,
                    format!("verification staging failed: {error}"),
                )
            })?;
        }
    }
    std::fs::copy(&staged, verify_root.join("iPod_Control/iTunes/iTunesDB"))
        .map_err(|error| sync_err(STEP_DB_WRITE, format!("staged copy failed: {error}")))?;
    let verify_session = ipod::DeviceSession::open(ctx.backend, &verify_root).map_err(|error| {
        sync_err(
            STEP_DB_WRITE,
            format!("the staged database does not parse: {error}"),
        )
    })?;
    let tracks = verify_session
        .tracks()
        .map_err(|error| sync_err(STEP_DB_WRITE, format!("staged track read failed: {error}")))?;
    if tracks.is_empty() {
        return Err(sync_err(
            STEP_DB_WRITE,
            "the staged database parses to an empty library",
        ));
    }
    drop(verify_session);
    let _ = std::fs::remove_dir_all(&verify_root);

    // Atomic rename into place + flush.
    std::fs::rename(&staged, itunes.join("iTunesDB"))
        .map_err(|error| sync_err(STEP_DB_WRITE, format!("rename into place failed: {error}")))?;
    if let Ok(file) = std::fs::File::open(itunes.join("iTunesDB")) {
        let _ = file.sync_all();
    }
    if let Ok(dir) = std::fs::File::open(&itunes) {
        let _ = dir.sync_all();
    }
    // syncfs the mount: everything libgpod wrote (artwork first) flushes.
    if let Ok(mount_fd) = std::fs::File::open(ctx.mount) {
        // SAFETY: a valid fd for the duration of the call; best-effort.
        unsafe {
            let _ = libc::syncfs(std::os::unix::io::AsRawFd::as_raw_fd(&mount_fd));
        }
    }
    Ok(())
}

/// Step 6: the post-write verification — fresh parse and the artwork
/// proof-read (R2: the only detector of the silently non-fatal artwork
/// write failures).
fn verify_device(ctx: &SyncContext<'_>) -> Result<(), SyncError> {
    let session = ipod::DeviceSession::open(ctx.backend, ctx.mount)
        .map_err(|error| sync_err(STEP_VERIFY, format!("post-write re-parse failed: {error}")))?;
    let _tracks = session.tracks().map_err(|error| {
        sync_err(
            STEP_VERIFY,
            format!("post-write track read failed: {error}"),
        )
    })?;
    let status = session.artwork_status().map_err(|error| {
        sync_err(
            STEP_VERIFY,
            format!("post-write artwork check failed: {error}"),
        )
    })?;
    if status.verdict != ipod::ArtworkVerdict::Ok {
        return Err(sync_err(
            STEP_VERIFY,
            format!(
                "artwork verdict after the write is {:?}: {}",
                status.verdict,
                status.diagnostic.as_deref().unwrap_or("no diagnostic")
            ),
        ));
    }
    Ok(())
}

/// Completion housekeeping: the pending store is consumed **by us**, not
/// by the staged write — measured (R1): `itdb_write_file`'s
/// `playcounts_reset` resolves the pending names against the mount root
/// instead of the iTunes dir and is a no-op, while `itdb_write`'s
/// `rename_files` (which we never call) is the only upstream consumer.
/// Removing the files here keeps a later Windows iTunes from re-adding
/// the already-merged delta (b010_plan §5 step 8; B-017 validates).
fn remove_consumed_pending(mount: &Path) -> Result<(), SyncError> {
    for name in [
        "Play Counts",
        "Play Counts.bak",
        "iTunesStats",
        "PlayCounts.plist",
    ] {
        let path = mount.join("iPod_Control/iTunes").join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(sync_err(
                    STEP_VERIFY,
                    format!("{name} removal failed: {error}"),
                ));
            }
        }
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}
