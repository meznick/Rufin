//! B-005 env-gated real-tree gates (see `deltas/b005_delta.md`): the
//! restored-device snapshot (`IPOD_TEST_TREE`) and a mounted device image
//! (`IPOD_TEST_MOUNT`), both with the pinned libgpod install
//! (`RUFIN_LIBGPOD_DIR`). The fast unit fixtures live in
//! `src/retrieval.rs`; these gates pin the research numbers from the
//! B-007/B-005 cross-checks against real trees.
//!
//! The skip messages (and the transient-read-artifact warning in
//! `support::stable_tree_hash`) are the only `eprintln!` here and are
//! diagnostics, not print-debugging (the same convention as the wrapper's
//! integration suite).
#![allow(clippy::print_stderr)]

mod support;

use std::path::PathBuf;

use ipod::{BackendSearch, DeviceSession};
use ipod_sync::{IPOD_CONTROL_DIR, collect};

/// Load the backend via the production search, or skip with an explanation.
/// A set-but-broken `RUFIN_LIBGPOD_DIR` is a test-environment failure and
/// panics rather than silently skipping (the discovery-suite convention).
fn backend_or_skip() -> Option<ipod::IpodBackend> {
    let Some(dir) = std::env::var_os("RUFIN_LIBGPOD_DIR") else {
        eprintln!(
            "skipping: RUFIN_LIBGPOD_DIR unset — the pinned libgpod install is missing; \
             build it with scripts/build-libgpod.sh"
        );
        return None;
    };
    let backend = ipod::IpodBackend::load_with(BackendSearch::Explicit(PathBuf::from(dir)))
        .expect("RUFIN_LIBGPOD_DIR is set but the backend did not load");
    Some(backend)
}

/// The restored-device snapshot (the fakepod): `SysInfo` seeded with
/// `ModelNumStr: MA446`, no `SysInfoExtended`. The research numbers from the
/// B-007 cross-checks and the R3 gate: name = master playlist "iPod user",
/// model resolved via the seeded `MA446`, no FireWire ID, capacities are the
/// host filesystem's (invariants only — the snapshot is not a device mount).
#[test]
fn env_gated_snapshot_retrieval_matches_research_numbers() {
    let Some(tree) = std::env::var_os("IPOD_TEST_TREE") else {
        eprintln!("skipping: IPOD_TEST_TREE unset (the restored-device snapshot)");
        return;
    };
    let tree = PathBuf::from(tree);
    assert!(
        tree.join(IPOD_CONTROL_DIR).is_dir(),
        "IPOD_TEST_TREE must be a mount root containing iPod_Control/ (got {})",
        tree.display()
    );
    let Some(backend) = backend_or_skip() else {
        return;
    };

    // Plain tree_hash, not stable_tree_hash: the snapshot lives on the host
    // filesystem (no loop layer), so the transient read artifact cannot
    // occur here — only the mounted gates need the stabilization.
    let before = ipod::tree_hash(&tree).unwrap();
    let session = DeviceSession::open(&backend, &tree).unwrap();
    let facts = collect(&session, &tree).unwrap();

    assert_eq!(facts.name, "iPod user", "master-playlist name (R3)");
    assert_eq!(
        facts.model_number.as_deref(),
        Some("A446"),
        "seeded ModelNumStr MA446 must resolve to A446"
    );
    assert_eq!(facts.model, "Video (Black)", "libgpod model table, MA446");
    assert_eq!(
        facts.generation, "Video (2nd Gen.)",
        "libgpod generation string"
    );
    assert_eq!(facts.firmware, None, "no SysInfoExtended in the snapshot");
    assert_eq!(
        facts.firewire_id, None,
        "no SysInfoExtended in the snapshot"
    );
    let total = facts.total_capacity.expect("statvfs must succeed");
    let used = facts.used_capacity.expect("statvfs must succeed");
    assert!(total > 0);
    assert!(used <= total);
    assert_eq!(
        ipod::tree_hash(&tree).unwrap(),
        before,
        "metadata retrieval must not modify the snapshot tree"
    );
}

/// The mounted device image (p2 of `ipod/test-device-30g.img`): the REAL
/// device metadata — an **empty** `SysInfo` (the iTunes 12.13 behavior) and
/// no `SysInfoExtended`. The expected live-device outcome is pinned here:
/// name still comes from the master playlist, but model/generation/model
/// number are explicitly absent, never defaulted. Capacities are measured
/// on the FAT32 data partition (the 30g image: assert a device-realistic
/// total). The tree hash covers `iPod_Control/` including the 23G media
/// library — the strongest no-write proof, same sizing as the discovery
/// mounted gate.
#[test]
fn env_gated_mounted_retrieval_pins_unknown_model_outcome() {
    let Some(mount) = std::env::var_os("IPOD_TEST_MOUNT") else {
        eprintln!("skipping: IPOD_TEST_MOUNT unset (mount p2 of ipod/test-device-30g.img)");
        return;
    };
    let mount = PathBuf::from(mount);
    let control = mount.join(IPOD_CONTROL_DIR);
    assert!(
        control.is_dir(),
        "IPOD_TEST_MOUNT must point at a mounted iPod volume (no iPod_Control under {})",
        mount.display()
    );
    let Some(backend) = backend_or_skip() else {
        return;
    };

    let before = support::stable_tree_hash(&control);
    let session = DeviceSession::open(&backend, &mount).unwrap();
    let facts = collect(&session, &mount).unwrap();

    assert_eq!(facts.name, "iPod user", "master-playlist name (R3)");
    assert_eq!(
        facts.model_number, None,
        "the real device has an empty SysInfo — the model must stay unknown"
    );
    assert_eq!(facts.model, "", "model unknown is an explicit empty string");
    assert_eq!(
        facts.generation, "",
        "generation unknown is an explicit empty string"
    );
    assert_eq!(facts.firmware, None);
    assert_eq!(
        facts.firewire_id, None,
        "no SysInfoExtended on the Video snapshot"
    );
    let total = facts
        .total_capacity
        .expect("statvfs must succeed on the mounted volume");
    let used = facts
        .used_capacity
        .expect("statvfs must succeed on the mounted volume");
    const GIB: u64 = 1024 * 1024 * 1024;
    assert!(
        (20 * GIB..=40 * GIB).contains(&total),
        "expected a device-realistic total on the 30g image's data partition, got {total}"
    );
    assert!(used <= total);
    assert_eq!(
        support::stable_tree_hash(&control),
        before,
        "metadata retrieval must not modify the mounted device tree"
    );
}
