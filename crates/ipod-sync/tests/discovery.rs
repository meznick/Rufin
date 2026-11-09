//! B-003 fixture matrix (see `deltas/b003_delta.md`): detection, unrelated
//! volumes, permissions, removal, missing backend, no-write guarantee, and
//! the env-gated backend-status check against the pinned install.
//!
//! Everything runs against synthetic mount tables over temp dirs — never a
//! real device, never the host's mounts.

use std::path::{Path, PathBuf};

use ipod::{BackendSearch, MARKER_FILE_NAME};
use ipod_sync::{BackendStatus, Discovery, IPOD_CONTROL_DIR, MountEntry, MountError, MountSource};

/// A mount source fed by a plain vector; mutated between scans to simulate
/// mounts appearing and disappearing.
#[derive(Debug, Default)]
struct FakeMounts {
    entries: Vec<MountEntry>,
}

impl MountSource for FakeMounts {
    fn mounts(&self) -> Result<Vec<MountEntry>, MountError> {
        Ok(self.entries.clone())
    }
}

fn mount(dir: &Path) -> MountEntry {
    MountEntry {
        mount_point: dir.to_path_buf(),
        fstype: "vfat".to_string(),
        source: "/dev/fake".to_string(),
    }
}

/// A disposable fixture whose root looks like an iPod: `iPod_Control/`
/// (with a `Device/SysInfo` file for hash content) plus a dummy `iTunesDB`.
fn pod_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(IPOD_CONTROL_DIR)).unwrap();
    std::fs::create_dir(dir.path().join(IPOD_CONTROL_DIR).join("Device")).unwrap();
    std::fs::write(
        dir.path()
            .join(IPOD_CONTROL_DIR)
            .join("Device")
            .join("SysInfo"),
        b"fake sysinfo",
    )
    .unwrap();
    std::fs::write(dir.path().join("iTunesDB"), b"fake itunesdb").unwrap();
    dir
}

/// `chmod 000` does not stop root, so permission fixtures must self-skip
/// when running as root (no `libc` dep: `id -u` is enough for a test).
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|out| out.stdout == b"0\n")
        .unwrap_or(false)
}

#[test]
fn fixture_detection_finds_ipod_control() {
    let pod = pod_fixture();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let source = FakeMounts {
        entries: vec![mount(pod.path())],
    };

    let first = discovery.scan(&source).unwrap();
    assert_eq!(first.candidates.len(), 1);
    let candidate = &first.candidates[0];
    assert_eq!(candidate.mount_point, pod.path());
    assert_eq!(candidate.fstype, "vfat");
    assert_eq!(first.blocked, vec![]);
    // First scan: everything found has appeared; nothing was removed.
    assert_eq!(first.appeared, vec![pod.path().to_path_buf()]);
    assert_eq!(first.removed, Vec::<PathBuf>::new());

    // Second scan with the same table: idempotent, no diff.
    let second = discovery.scan(&source).unwrap();
    assert_eq!(second.candidates.len(), 1);
    assert_eq!(second.appeared, Vec::<PathBuf>::new());
    assert_eq!(second.removed, Vec::<PathBuf>::new());
}

#[test]
fn fixture_unrelated_volumes_are_ignored() {
    let plain = tempfile::tempdir().unwrap();
    let with_files = tempfile::tempdir().unwrap();
    std::fs::write(with_files.path().join("random.dat"), b"x").unwrap();
    std::fs::create_dir(with_files.path().join("sub")).unwrap();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let source = FakeMounts {
        entries: vec![mount(plain.path()), mount(with_files.path())],
    };

    let report = discovery.scan(&source).unwrap();
    assert_eq!(report.candidates, vec![]);
    assert_eq!(report.blocked, vec![]);
    assert_eq!(report.appeared, Vec::<PathBuf>::new());
}

#[test]
fn fixture_ipod_control_file_is_not_a_candidate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(IPOD_CONTROL_DIR), b"a file, not a dir").unwrap();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let source = FakeMounts {
        entries: vec![mount(dir.path())],
    };

    let report = discovery.scan(&source).unwrap();
    assert_eq!(report.candidates, vec![]);
    assert_eq!(report.blocked, vec![]);
}

#[cfg(unix)]
#[test]
fn fixture_permission_denied_mount_is_blocked() {
    use std::os::unix::fs::PermissionsExt;
    if running_as_root() {
        // chmod 000 does not block root; the fixture cannot run.
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(IPOD_CONTROL_DIR)).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    // Restore permissions before the tempdir drops, or removal fails.
    struct Restore<'a>(&'a Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = Restore(dir.path());

    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let source = FakeMounts {
        entries: vec![mount(dir.path())],
    };

    let report = discovery.scan(&source).unwrap();
    assert_eq!(
        report.candidates,
        vec![],
        "an unprobeable mount is not a candidate"
    );
    assert_eq!(report.blocked.len(), 1);
    assert_eq!(report.blocked[0].mount_point, dir.path());
    assert!(!report.blocked[0].error.is_empty());
}

#[test]
fn fixture_removal_is_reported_on_rescan() {
    let pod = pod_fixture();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let mut source = FakeMounts {
        entries: vec![mount(pod.path())],
    };

    let first = discovery.scan(&source).unwrap();
    assert_eq!(first.candidates.len(), 1);

    // The volume unmounts: the mount leaves the table.
    source.entries.clear();
    let second = discovery.scan(&source).unwrap();
    assert_eq!(second.candidates, vec![]);
    assert_eq!(second.removed, vec![pod.path().to_path_buf()]);
    assert_eq!(second.appeared, Vec::<PathBuf>::new());
}

#[test]
fn fixture_reappearance_is_reported() {
    let pod = pod_fixture();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let mut source = FakeMounts {
        entries: vec![mount(pod.path())],
    };

    discovery.scan(&source).unwrap();
    source.entries.clear();
    let gone = discovery.scan(&source).unwrap();
    assert_eq!(gone.removed, vec![pod.path().to_path_buf()]);

    source.entries.push(mount(pod.path()));
    let back = discovery.scan(&source).unwrap();
    assert_eq!(back.candidates.len(), 1);
    assert_eq!(back.appeared, vec![pod.path().to_path_buf()]);
    assert_eq!(back.removed, Vec::<PathBuf>::new());
}

#[test]
fn fixture_backend_missing_reports_unavailable_but_finds_candidates() {
    let pod = pod_fixture();
    // Explicit directory that holds no libgpod: the load must fail
    // deterministically, without touching the process environment.
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let source = FakeMounts {
        entries: vec![mount(pod.path())],
    };

    let report = discovery.scan(&source).unwrap();
    // Discovery works without the backend: candidates are found regardless.
    assert_eq!(report.candidates.len(), 1);
    match &report.backend {
        BackendStatus::Unavailable { reason } => assert!(!reason.is_empty()),
        BackendStatus::Loaded { .. } => panic!("an empty Explicit dir must not load"),
    }
    assert!(discovery.backend().is_none());
}

#[test]
fn fixture_scan_never_writes_to_candidate_tree() {
    let pod = pod_fixture();
    let before = ipod::tree_hash(pod.path()).unwrap();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    let source = FakeMounts {
        entries: vec![mount(pod.path())],
    };

    discovery.scan(&source).unwrap();
    let after_first = ipod::tree_hash(pod.path()).unwrap();
    assert_eq!(
        before, after_first,
        "a scan must not modify the device tree"
    );

    discovery.scan(&source).unwrap();
    let after_second = ipod::tree_hash(pod.path()).unwrap();
    assert_eq!(
        before, after_second,
        "rescan must not modify the device tree"
    );
}

/// Env-gated (like the wrapper's integration suite): with `RUFIN_LIBGPOD_DIR`
/// pointing at a libgpod install, the scan must report `Loaded` with the ABI
/// verdict matching the marker file next to the library, and even a loaded
/// backend (dlopen) must not write into the candidate tree.
#[test]
fn env_gated_backend_loaded_reports_abi_status() {
    let Some(dir) = std::env::var_os("RUFIN_LIBGPOD_DIR") else {
        return;
    };
    let lib_dir = PathBuf::from(dir);
    let expected_abi = lib_dir.join(MARKER_FILE_NAME).exists();

    let pod = pod_fixture();
    let tree_before = ipod::tree_hash(pod.path()).unwrap();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(lib_dir));
    let source = FakeMounts {
        entries: vec![mount(pod.path())],
    };

    let report = discovery.scan(&source).unwrap();
    assert_eq!(report.candidates.len(), 1);
    match &report.backend {
        BackendStatus::Loaded { abi_verified } => {
            assert_eq!(
                *abi_verified, expected_abi,
                "the report must mirror the marker verdict"
            );
        }
        BackendStatus::Unavailable { reason } => {
            panic!("RUFIN_LIBGPOD_DIR is set but the backend did not load: {reason}");
        }
    }
    assert!(discovery.backend().is_some());
    assert_eq!(
        ipod::tree_hash(pod.path()).unwrap(),
        tree_before,
        "a scan with a loaded backend must not modify the device tree"
    );
}

#[test]
fn fixture_multiple_candidates_are_sorted_and_deduped() {
    let pod_a = pod_fixture();
    let pod_b = pod_fixture();
    let unrelated = tempfile::tempdir().unwrap();
    let mut discovery = Discovery::with_backend_search(BackendSearch::Explicit(
        tempfile::tempdir().unwrap().keep(),
    ));
    // The same mount point twice (bind-mount-like duplicate) must dedupe.
    let source = FakeMounts {
        entries: vec![
            mount(pod_a.path()),
            mount(unrelated.path()),
            mount(pod_a.path()),
            mount(pod_b.path()),
        ],
    };

    let report = discovery.scan(&source).unwrap();
    let mut points: Vec<PathBuf> = report
        .candidates
        .iter()
        .map(|c| c.mount_point.clone())
        .collect();
    points.sort();
    assert_eq!(points.len(), 2);
    assert_eq!(
        report
            .candidates
            .iter()
            .map(|c| c.mount_point.clone())
            .collect::<Vec<_>>(),
        points
    );
    assert!(report.candidates.iter().all(|c| c.fstype == "vfat"));
}
