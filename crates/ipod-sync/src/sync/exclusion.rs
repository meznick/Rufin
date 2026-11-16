//! B-010: competing-writer exclusion (b010_plan §5, S-12). Three layers,
//! checked in order before the first destructive write and held for the
//! whole operation:
//!
//! 1. A Rufin-owned `flock` on a lock file inside the journal directory —
//!    a second Rufin process (or another app honoring the lock) cannot
//!    start while an operation runs.
//! 2. A `/proc` handle scan — any *other* process holding open files under
//!    `iPod_Control/` refuses the write (the pattern already used for the
//!    host `.itl`). The device is attached to one OS at a time in the
//!    owner's environment, so the realistic competitors are local
//!    processes; this covers them.
//! 3. A writability probe (create + remove a file in the journal
//!    directory) — a read-only mount or a vanished volume fails here.

use std::io;
use std::path::Path;

use crate::sync::journal::Journal;

/// Why the write is refused.
#[derive(Debug)]
pub enum ExclusionError {
    /// Another process holds files open under the mount.
    CompetingHandles(String),
    /// The mount is not writable.
    NotWritable(io::Error),
    /// Another Rufin operation holds the lock.
    Locked(io::Error),
}

impl std::fmt::Display for ExclusionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompetingHandles(message) => write!(f, "competing writer: {message}"),
            Self::NotWritable(error) => write!(f, "device is not writable: {error}"),
            Self::Locked(error) => write!(f, "another sync holds the device lock: {error}"),
        }
    }
}

impl std::error::Error for ExclusionError {}

/// The held device lock — drop it when the operation ends.
pub struct ExclusionGuard {
    _file: std::fs::File,
}

impl ExclusionGuard {
    /// Run every exclusion layer and hold the lock.
    pub fn acquire(mount: &Path) -> Result<Self, ExclusionError> {
        no_other_handles(mount)?;
        probe_writable(mount)?;
        let journal = Journal::open(mount).map_err(ExclusionError::NotWritable)?;
        let lock_path = journal.root().join("sync.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(ExclusionError::NotWritable)?;
        // SAFETY: `file` is a valid fd for the duration of the guard; the
        // lock is advisory and released by drop/close.
        let result = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if result != 0 {
            return Err(ExclusionError::Locked(io::Error::last_os_error()));
        }
        Ok(Self { _file: file })
    }
}

/// Scan `/proc` for other processes holding open files under `mount`
/// (including the `iPod_Control/` subtree). Self and unavailable
/// directories are skipped; a handle that cannot be attributed is
/// reported, never silently accepted.
pub fn no_other_handles(mount: &Path) -> Result<(), ExclusionError> {
    let self_pid = std::process::id();
    let mount_canonical = std::fs::canonicalize(mount).unwrap_or_else(|_| mount.to_path_buf());
    let mut offenders = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return Ok(()), // no /proc — the layer is unavailable, not failed
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue; // exited or inaccessible
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if target.starts_with(&mount_canonical) || target.starts_with(mount) {
                let exe = std::fs::read_link(entry.path().join("exe"))
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| format!("pid {pid}"));
                offenders.push(exe);
                break;
            }
        }
    }
    if offenders.is_empty() {
        Ok(())
    } else {
        Err(ExclusionError::CompetingHandles(format!(
            "open handles under {} from: {}",
            mount.display(),
            offenders.join(", ")
        )))
    }
}

/// Create and remove a probe file inside the journal directory — the
/// mount must actually accept writes.
pub fn probe_writable(mount: &Path) -> Result<(), ExclusionError> {
    let journal = Journal::open(mount).map_err(ExclusionError::NotWritable)?;
    let probe = journal.root().join(".write-probe");
    std::fs::write(&probe, b"probe").map_err(ExclusionError::NotWritable)?;
    std::fs::remove_file(&probe).map_err(ExclusionError::NotWritable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writable_tree_passes_and_holds_the_lock() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("pod");
        std::fs::create_dir_all(mount.join(crate::sync::journal::JOURNAL_DIR)).unwrap();
        let guard = ExclusionGuard::acquire(&mount).expect("clean tree acquires");
        // A second acquire must fail while the guard lives.
        assert!(ExclusionGuard::acquire(&mount).is_err());
        drop(guard);
        assert!(ExclusionGuard::acquire(&mount).is_ok());
    }

    #[test]
    fn a_read_only_mount_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("pod");
        std::fs::create_dir_all(&mount).unwrap();
        // Simulate a read-only mount: no permission to create anything.
        let mut permissions = std::fs::metadata(&mount).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&mount, permissions).unwrap();
        assert!(ExclusionGuard::acquire(&mount).is_err());
        // Restore permissions (tempdir cleanup needs the directory
        // writable).
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut permissions = std::fs::metadata(&mount).unwrap().permissions();
            permissions.set_readonly(false);
            std::fs::set_permissions(&mount, permissions).unwrap();
        }
    }

    #[test]
    fn own_handles_are_never_offenders() {
        let temp = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(temp.path().join("held")).unwrap();
        // The handle is ours; the scan must not flag the process.
        let _held = file;
        assert!(no_other_handles(temp.path()).is_ok());
    }
}
