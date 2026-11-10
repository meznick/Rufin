//! B-006 env-gated observer gates (see `deltas/b006_delta.md`): the full
//! pipeline — scan → retrieval → preflight → observer snapshots — against
//! the restored-device snapshot (`IPOD_TEST_TREE`), the mounted read-only
//! device image (`IPOD_TEST_MOUNT`), and the mounted read-write image
//! (`IPOD_TEST_MOUNT_RW`, the Ready path on a genuine vfat mount with the
//! 2026-11-10 model-policy verdicts), on the pinned libgpod install
//! (`RUFIN_LIBGPOD_DIR`). The fast unit matrix lives in
//! `src/ipod_connection.rs` and `src/ipod_monitor.rs`; these gates prove
//! the Ready/Error verdicts arrive on the subscriber channel end-to-end
//! with the real backend, and that the service scan path stays write-free.
//!
//! Skip conventions as in `ipod-sync/tests/preflight.rs`: an unset env var
//! skips with an explanation; a set-but-broken backend dir panics.
//!
//! The skip messages are the only `eprintln!` here and are diagnostics, not
//! print-debugging (the same convention as the other suites).
#![allow(clippy::print_stderr)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gtk_library::device_manager::ConnectionState;
use gtk_library::ipod_connection::{ConnectionSnapshot, IpodConnection};
use ipod::BackendSearch;
use ipod_sync::{MountEntry, MountError, MountSource};

/// The service, built on an explicitly pinned backend dir. The backend is
/// pre-loaded once so a set-but-broken `RUFIN_LIBGPOD_DIR` panics here with
/// a clear message instead of surfacing as a confusing
/// `BackendUnavailable` snapshot mid-gate.
fn connection_or_skip() -> Option<IpodConnection> {
    let Some(dir) = std::env::var_os("RUFIN_LIBGPOD_DIR") else {
        eprintln!("skipping: RUFIN_LIBGPOD_DIR unset (the pinned libgpod install)");
        return None;
    };
    let dir = PathBuf::from(dir);
    ipod::IpodBackend::load_with(BackendSearch::Explicit(dir.clone()))
        .expect("RUFIN_LIBGPOD_DIR is set but the backend did not load");
    Some(IpodConnection::with_backend_search(
        BackendSearch::Explicit(dir),
    ))
}

/// A mount source with a single mount point — the gates feed the snapshot
/// directory or the mounted image as a synthetic mount, so the host's real
/// mount table never leaks in.
struct SingleMount(PathBuf);

impl MountSource for SingleMount {
    fn mounts(&self) -> Result<Vec<MountEntry>, MountError> {
        Ok(vec![MountEntry {
            mount_point: self.0.clone(),
            fstype: "vfat".to_string(),
            source: "/dev/gate".to_string(),
        }])
    }
}

fn expect(snapshots: &async_channel::Receiver<ConnectionSnapshot>) -> ConnectionSnapshot {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match snapshots.try_recv() {
            Ok(snapshot) => return snapshot,
            Err(async_channel::TryRecvError::Empty) => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for a snapshot"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(async_channel::TryRecvError::Closed) => panic!("snapshot channel closed"),
        }
    }
}

/// The restored-device snapshot (`SysInfo` seeded MA446, rw host
/// filesystem): the full Ready path through the observer channel, plus the
/// no-write proof — the tree hash after the Ready emission matches the
/// before hash (the emission is the last step of the scan, so everything
/// the worker ran is covered).
#[test]
fn env_gated_snapshot_scan_reaches_ready_through_the_observer_path() {
    let Some(connection) = connection_or_skip() else {
        return;
    };
    let Some(tree) = std::env::var_os("IPOD_TEST_TREE") else {
        eprintln!("skipping: IPOD_TEST_TREE unset (the restored-device snapshot)");
        return;
    };
    let tree = PathBuf::from(tree);
    assert!(
        tree.join(ipod_sync::IPOD_CONTROL_DIR).is_dir(),
        "IPOD_TEST_TREE must be a mount root containing iPod_Control/ (got {})",
        tree.display()
    );

    let before = ipod::tree_hash(&tree).expect("hash the snapshot tree");
    let snapshots = connection.subscribe();
    connection.scan_from(Box::new(SingleMount(tree.clone())));

    let connecting = expect(&snapshots);
    assert_eq!(connecting.state, ConnectionState::Connecting);
    assert_eq!(connecting.device.as_ref().unwrap().mount_point, tree);
    assert_eq!(
        connecting.device.as_ref().unwrap().name,
        "",
        "the detection emission carries the placeholder, not facts"
    );

    let connecting = expect(&snapshots);
    assert_eq!(connecting.state, ConnectionState::Connecting);
    assert!(
        !connecting.device.as_ref().unwrap().name.is_empty(),
        "the retrieval emission carries the master-playlist name"
    );

    let ready = expect(&snapshots);
    assert_eq!(ready.state, ConnectionState::Ready);
    assert!(
        ready.diagnostics.is_none(),
        "Ready must carry no diagnostics: {:?}",
        ready.diagnostics
    );
    assert_eq!(
        ready.device.as_ref().unwrap().mount_point,
        tree,
        "the Ready snapshot keeps the candidate's mount point"
    );
    assert!(
        ready.device.as_ref().unwrap().model_supported,
        "the seeded MA446 snapshot is inside the supported set"
    );
    assert!(
        ready.device.as_ref().unwrap().checksum_possible,
        "Video needs no checksum"
    );
    assert_eq!(
        ipod::tree_hash(&tree).expect("hash the snapshot tree"),
        before,
        "the service scan path must not modify the snapshot tree"
    );
}

/// The mounted device image (p2 of `ipod/test-device-30g.img`, ro): the
/// read-only check wins over everything. Retrieval still succeeds (the
/// real image parses fine read-only, with an unknown model — empty
/// `SysInfo`), then preflight reports the ro mount — the observer channel
/// carries `Connecting` twice and then `Error` with the ro diagnostic.
#[test]
fn env_gated_mounted_readonly_lands_error_through_the_observer_path() {
    let Some(connection) = connection_or_skip() else {
        return;
    };
    let Some(mount) = std::env::var_os("IPOD_TEST_MOUNT") else {
        eprintln!("skipping: IPOD_TEST_MOUNT unset (mount p2 of ipod/test-device-30g.img)");
        return;
    };
    let mount = PathBuf::from(mount);
    assert!(
        mount.join(ipod_sync::IPOD_CONTROL_DIR).is_dir(),
        "IPOD_TEST_MOUNT must point at a mounted iPod volume (no iPod_Control under {})",
        mount.display()
    );

    let snapshots = connection.subscribe();
    connection.scan_from(Box::new(SingleMount(mount.clone())));

    let connecting = expect(&snapshots);
    assert_eq!(connecting.state, ConnectionState::Connecting);
    let connecting = expect(&snapshots);
    assert_eq!(connecting.state, ConnectionState::Connecting);

    let error = expect(&snapshots);
    assert_eq!(error.state, ConnectionState::Error);
    assert!(
        error
            .diagnostics
            .as_deref()
            .is_some_and(|detail| detail.contains("read-only")),
        "the read-only diagnostic must arrive on the channel: {:?}",
        error.diagnostics
    );
}

/// The mounted device image (p2 of `ipod/test-device-30g.img`), mounted
/// **read-write**: the full Ready path on a genuine vfat mount, plus the
/// no-write tree hash. The real image has an empty `SysInfo` — under the
/// 2026-11-10 model policy (b006 delta decision 12) that is a limited
/// device, not a refusal, so the Ready snapshot must carry
/// `model_supported == false` and `checksum_possible == false`: the gate
/// proves the verdict plumbing end-to-end on real hardware.
///
/// Env-gated on `IPOD_TEST_MOUNT_RW` — deliberately distinct from the
/// read-only `IPOD_TEST_MOUNT` so the two scenarios never collide in one
/// `cargo test` invocation.
#[test]
fn env_gated_mounted_rw_reaches_ready_with_limited_verdicts() {
    let Some(connection) = connection_or_skip() else {
        return;
    };
    let Some(mount) = std::env::var_os("IPOD_TEST_MOUNT_RW") else {
        eprintln!(
            "skipping: IPOD_TEST_MOUNT_RW unset (mount p2 of ipod/test-device-30g.img read-write)"
        );
        return;
    };
    let mount = PathBuf::from(mount);
    let control = mount.join(ipod_sync::IPOD_CONTROL_DIR);
    assert!(
        control.is_dir(),
        "IPOD_TEST_MOUNT_RW must point at a mounted iPod volume (no iPod_Control under {})",
        mount.display()
    );

    let before = ipod::tree_hash(&control).expect("hash the mounted tree");
    let snapshots = connection.subscribe();
    connection.scan_from(Box::new(SingleMount(mount.clone())));

    let connecting = expect(&snapshots);
    assert_eq!(connecting.state, ConnectionState::Connecting);
    assert_eq!(
        connecting.device.as_ref().unwrap().name,
        "",
        "the detection emission carries the placeholder"
    );

    let connecting = expect(&snapshots);
    assert_eq!(connecting.state, ConnectionState::Connecting);
    assert!(
        !connecting.device.as_ref().unwrap().name.is_empty(),
        "the retrieval emission carries the master-playlist name"
    );

    let ready = expect(&snapshots);
    assert_eq!(ready.state, ConnectionState::Ready);
    assert!(
        ready.diagnostics.is_none(),
        "Ready must carry no diagnostics: {:?}",
        ready.diagnostics
    );
    let device = ready.device.as_ref().unwrap();
    assert_eq!(device.mount_point, mount);
    assert!(
        !device.model_supported,
        "the real image has an empty SysInfo: the model verdict must be limited"
    );
    assert!(
        !device.checksum_possible,
        "an unresolved model keeps the checksum verdict conservative"
    );
    assert_eq!(
        ipod::tree_hash(&control).expect("hash the mounted tree"),
        before,
        "the service scan path must not modify a mounted device"
    );
}
