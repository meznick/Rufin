//! B-010: the consistency-set backup and restore (b010_plan §5 steps 3/7,
//! §7). Lives on the device, inside the journal directory (owner decision
//! §11-8); the pre-write space check guarantees room for it.
//!
//! The set is exactly what a crash between the artwork write and the DB
//! rename could leave half-updated: `iTunesDB`, `ArtworkDB` + the ithmb
//! banks, the pending stores, the OTG files and the device identity files.
//! Media files are not backed up (re-copyable from the host; their removal
//! is deferred until the DB write commits). Restoring the DBs does not
//! roll back media files — orphans and missing files are exactly the
//! states the next sync's delta engine repairs, so the pair returns to
//! consistency by construction.

use std::io;
use std::path::{Path, PathBuf};

/// Files/directories under `iPod_Control/` that form the consistency set.
/// Missing entries are skipped — `Play Counts`/`OTGPlaylistInfo` are
/// legitimately absent on an idle device.
pub const CONSISTENCY_SET: &[&str] = &[
    "iTunes/iTunesDB",
    "iTunes/Play Counts",
    "iTunes/Play Counts.bak",
    "iTunes/iTunesStats",
    "iTunes/OTGPlaylistInfo",
    "Artwork/ArtworkDB",
    "Device/SysInfo",
    "Device/SysInfoExtended",
];

/// The ithmb banks: every `*.ithmb` file under `iPod_Control/Artwork/`
/// (the list is discovered at backup time — the bank count varies by
/// model and artwork churn).
pub const ITHMB_DIR: &str = "Artwork";

/// Backup the consistency set into `<journal_root>/backup/`. Returns the
/// backup directory. Existing files in the destination are overwritten —
/// the executor uses a fresh directory per operation.
pub fn backup_consistency_set(mount: &Path, journal_root: &Path) -> io::Result<PathBuf> {
    let backup = journal_root.join("backup");
    std::fs::create_dir_all(&backup)?;
    let ipod_control = mount.join("iPod_Control");
    for relative in CONSISTENCY_SET {
        let source = ipod_control.join(relative);
        match std::fs::metadata(&source) {
            Ok(metadata) if metadata.is_file() => {
                copy_verified(&source, &backup.join(relative))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let artwork = ipod_control.join(ITHMB_DIR);
    if let Ok(entries) = std::fs::read_dir(&artwork) {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".ithmb") {
                copy_verified(&entry.path(), &backup.join(ITHMB_DIR).join(name.as_ref()))?;
            }
        }
    }
    Ok(backup)
}

/// Restore the consistency set from a backup directory produced by
/// [`backup_consistency_set`]. Only files present in the backup are
/// written back; the backup is verified byte-for-byte as it restores.
pub fn restore_consistency_set(mount: &Path, backup: &Path) -> io::Result<()> {
    let ipod_control = mount.join("iPod_Control");
    for relative in CONSISTENCY_SET {
        let source = backup.join(relative);
        match std::fs::metadata(&source) {
            Ok(metadata) if metadata.is_file() => {
                copy_verified(&source, &ipod_control.join(relative))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let banks = backup.join(ITHMB_DIR);
    if let Ok(entries) = std::fs::read_dir(&banks) {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".ithmb") {
                copy_verified(
                    &entry.path(),
                    &ipod_control.join(ITHMB_DIR).join(name.as_ref()),
                )?;
            }
        }
    }
    Ok(())
}

/// Copy with a read-back verification: the destination must equal the
/// source byte-for-byte before the call returns.
fn copy_verified(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    let (source_bytes, destination_bytes) = (std::fs::read(source)?, std::fs::read(destination)?);
    if source_bytes != destination_bytes {
        return Err(io::Error::other(format!(
            "backup verification failed: {} != {}",
            source.display(),
            destination.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("pod");
        let ipod_control = mount.join("iPod_Control");
        std::fs::create_dir_all(ipod_control.join("iTunes")).unwrap();
        std::fs::create_dir_all(ipod_control.join("Artwork")).unwrap();
        std::fs::create_dir_all(ipod_control.join("Device")).unwrap();
        std::fs::write(ipod_control.join("iTunes/iTunesDB"), b"the db").unwrap();
        std::fs::write(ipod_control.join("Artwork/ArtworkDB"), b"art db").unwrap();
        std::fs::write(ipod_control.join("Artwork/F1028_1.ithmb"), b"bank one").unwrap();
        std::fs::write(ipod_control.join("Artwork/F1029_1.ithmb"), b"bank two").unwrap();
        std::fs::write(ipod_control.join("Device/SysInfo"), b"ModelNumStr: MA446\n").unwrap();
        // A pending store present on a live device.
        std::fs::write(ipod_control.join("iTunes/Play Counts"), b"mhdp pending").unwrap();
        (temp, mount, ipod_control)
    }

    #[test]
    fn backup_and_restore_round_trip() {
        let (temp, mount, ipod_control) = pod();
        let journal = temp.path().join("Rufin");
        let backup = backup_consistency_set(&mount, &journal).unwrap();

        // Corrupt the device tree, then restore.
        std::fs::write(ipod_control.join("iTunes/iTunesDB"), b"corrupted").unwrap();
        std::fs::write(ipod_control.join("Artwork/ArtworkDB"), b"corrupted").unwrap();
        std::fs::remove_file(ipod_control.join("Artwork/F1029_1.ithmb")).unwrap();
        std::fs::remove_file(ipod_control.join("iTunes/Play Counts")).unwrap();

        restore_consistency_set(&mount, &backup).unwrap();
        assert_eq!(
            std::fs::read(ipod_control.join("iTunes/iTunesDB")).unwrap(),
            b"the db"
        );
        assert_eq!(
            std::fs::read(ipod_control.join("Artwork/ArtworkDB")).unwrap(),
            b"art db"
        );
        assert_eq!(
            std::fs::read(ipod_control.join("Artwork/F1029_1.ithmb")).unwrap(),
            b"bank two"
        );
        assert_eq!(
            std::fs::read(ipod_control.join("iTunes/Play Counts")).unwrap(),
            b"mhdp pending"
        );
    }

    #[test]
    fn absent_files_are_skipped() {
        let (temp, mount, ipod_control) = pod();
        // No OTGPlaylistInfo on this pod — the backup must not fail.
        std::fs::remove_file(ipod_control.join("iTunes/Play Counts")).unwrap();
        let journal = temp.path().join("Rufin");
        let backup = backup_consistency_set(&mount, &journal).unwrap();
        assert!(backup.is_dir());
        assert!(
            !backup.join("iTunes/Play Counts").exists(),
            "absent files are not materialized"
        );
        restore_consistency_set(&mount, &backup).unwrap();
        assert!(
            !ipod_control.join("iTunes/Play Counts").exists(),
            "restore does not fabricate absent files"
        );
    }

    #[test]
    fn restore_errors_propagate() {
        // The read-back verification guards the copy operation itself (a
        // torn copy can't slip through); it cannot detect a pre-corrupted
        // backup — the backup is the trust anchor, verified at backup
        // time. A failed copy must surface, though.
        let (temp, mount, ipod_control) = pod();
        let journal = temp.path().join("Rufin");
        let backup = backup_consistency_set(&mount, &journal).unwrap();
        // A directory blocking the destination makes the copy fail.
        std::fs::remove_file(ipod_control.join("iTunes/iTunesDB")).unwrap();
        std::fs::create_dir(ipod_control.join("iTunes/iTunesDB")).unwrap();
        let error = restore_consistency_set(&mount, &backup).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn non_ithmb_artwork_files_are_not_backed_up() {
        let (temp, mount, ipod_control) = pod();
        std::fs::write(ipod_control.join("Artwork/not-a-bank.bin"), b"x").unwrap();
        let journal = temp.path().join("Rufin");
        let backup = backup_consistency_set(&mount, &journal).unwrap();
        assert!(
            !backup.join("Artwork/not-a-bank.bin").exists(),
            "only ithmb banks are in the set"
        );
    }
}
