//! B-004 env-gated preflight gates (see `deltas/b004_delta.md`): the
//! restored-device snapshot (`IPOD_TEST_TREE`), a mounted device image
//! (`IPOD_TEST_MOUNT`, read-only), and fresh `itdb_init_ipod` fixtures in
//! tempdirs — all against the pinned libgpod install (`RUFIN_LIBGPOD_DIR`).
//! The fast unit matrix lives in `src/preflight.rs`; these gates pin the
//! research numbers against real trees and prove the no-write discipline.
//!
//! The skip messages (and the transient-read-artifact warning in
//! `support::stable_tree_hash`) are the only `eprintln!` here and are
//! diagnostics, not print-debugging (the same convention as the wrapper's
//! integration suite).
#![allow(clippy::print_stderr)]

mod support;

use std::path::{Path, PathBuf};

use ipod::{BackendSearch, IpodBackend};
use ipod_sync::{ReadinessCheck, ReadinessError, preflight};

/// Load the backend via the production search, or skip with an explanation.
/// A set-but-broken `RUFIN_LIBGPOD_DIR` is a test-environment failure and
/// panics rather than silently skipping (the discovery-suite convention).
fn backend_or_skip() -> Option<ipod::IpodBackend> {
    let Some(dir) = std::env::var_os("RUFIN_LIBGPOD_DIR") else {
        eprintln!("skipping: RUFIN_LIBGPOD_DIR unset (the pinned libgpod install)");
        return None;
    };
    let backend = ipod::IpodBackend::load_with(BackendSearch::Explicit(PathBuf::from(dir)))
        .expect("RUFIN_LIBGPOD_DIR is set but the backend did not load");
    Some(backend)
}

/// A fresh device tree in a tempdir, built with libgpod's own
/// `itdb_init_ipod` (`ipod::testkit::init_device`): SysInfo carries the
/// `ModelNumStr`, the iTunesDB is empty but valid, and the ArtworkDB exists
/// with zero declared entries — the legitimately-artless `Ok` verdict.
///
/// Classic fixtures cannot be built with `init_device` directly:
/// `itdb_init_ipod` writes the fresh database through `itdb_write`, and the
/// Classic checksum path refuses with "Couldn't find the iPod firewire ID"
/// (itdb_itunesdb.c:8192 → itdb_device.c:1989 → itdb_hash58.c:249) — a live
/// confirmation of the prerequisite this task checks. So the Classic
/// fixtures start as a Video init (`MA446`) and rewrite the SysInfo
/// afterwards (`MB029`: the leading `M` is skipped by libgpod's
/// model-number lookup, delta §2.5).
fn fixture(backend: &IpodBackend, model_number: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    ipod::testkit::init_device(backend, dir.path(), "MA446", "iPod user")
        .expect("itdb_init_ipod fixture");
    if model_number != "MA446" {
        ipod::testkit::seed_sysinfo(dir.path(), model_number).expect("SysInfo seed");
    }
    dir
}

/// The Classic-with-FireWire fixture: `testkit::seed_sysinfo_extended`
/// hardcodes a Video serial (`V9K` → A444), and the serial path wins over
/// `ModelNumStr` — so this test writes its own plist with a Classic serial
/// plus the `FireWireGUID` (delta §2.5). The serial and the GUID are
/// parameters so the plist contents stay visible at the call site.
fn seed_classic_sysinfo_extended(mount: &Path, serial: &str, firewire_guid: &str) {
    let dir = mount.join("iPod_Control").join("Device");
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>FireWireGUID</key><string>{firewire_guid}</string>\n\
         <key>SerialNumber</key><string>{serial}</string>\n\
         </dict></plist>\n"
    );
    std::fs::write(dir.join("SysInfoExtended"), plist).unwrap();
}

fn expect_check(result: Result<(), ReadinessError>, kind: ReadinessCheck) -> ReadinessError {
    match result {
        Ok(()) => panic!("expected a {kind} failure, got Ok"),
        Err(ReadinessError::Check {
            kind: actual,
            detail,
        }) => {
            assert_eq!(actual, kind, "unexpected check kind (detail: {detail})");
            assert!(!detail.is_empty());
            ReadinessError::Check { kind, detail }
        }
        Err(other) => panic!("expected a device-level Check, got {other}"),
    }
}

/// The restored-device snapshot (the fakepod, `SysInfo` seeded MA446): the
/// full Ready path on a real tree — model Video 2nd Gen. (supported, no
/// checksum requirement), artwork `Ok` with proof-read, host filesystem
/// rw. The tree hash before/after is the no-write proof.
#[test]
fn env_gated_snapshot_preflight_passes() {
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
    let Some(backend) = backend_or_skip() else {
        return;
    };

    // Plain tree_hash: the snapshot lives on the host filesystem (no loop
    // layer), so the transient read artifact cannot occur here.
    let before = ipod::tree_hash(&tree).unwrap();
    preflight(&backend, &tree).unwrap();
    assert_eq!(
        ipod::tree_hash(&tree).unwrap(),
        before,
        "preflight must not modify the snapshot tree"
    );
}

/// The mounted device image (p2 of `ipod/test-device-30g.img`, ro): the
/// read-only check wins over everything — the mount state is the most
/// fundamental blocker, so preflight reports it without even parsing.
#[test]
fn env_gated_mounted_readonly_fails() {
    let Some(mount) = std::env::var_os("IPOD_TEST_MOUNT") else {
        eprintln!("skipping: IPOD_TEST_MOUNT unset (mount p2 of ipod/test-device-30g.img)");
        return;
    };
    let mount = PathBuf::from(mount);
    let control = mount.join(ipod_sync::IPOD_CONTROL_DIR);
    assert!(
        control.is_dir(),
        "IPOD_TEST_MOUNT must point at a mounted iPod volume (no iPod_Control under {})",
        mount.display()
    );
    let Some(backend) = backend_or_skip() else {
        return;
    };

    let before = support::stable_tree_hash(&control);
    let error = expect_check(preflight(&backend, &mount), ReadinessCheck::ReadOnly);
    assert!(
        error.to_string().contains("read-only"),
        "diagnostic must name the mount state: {error}"
    );
    assert_eq!(
        support::stable_tree_hash(&control),
        before,
        "preflight must not modify the mounted device tree"
    );
}

/// A fresh Video fixture: the full pass — supported model, no checksum
/// requirement, legitimately artless artwork, rw host filesystem.
#[test]
fn fresh_video_fixture_passes() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MA446");
    preflight(&backend, dir.path()).unwrap();
}

/// A fresh Classic fixture without SysInfoExtended: the checksum
/// prerequisite fails with an actionable FireWire diagnostic.
#[test]
fn classic_without_firewire_fails_checksum_prerequisite() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MB029");
    let error = expect_check(
        preflight(&backend, dir.path()),
        ReadinessCheck::ChecksumPrerequisite,
    );
    assert!(
        error.to_string().contains("FireWire"),
        "diagnostic must name the FireWire ID: {error}"
    );
}

/// The same Classic with a SysInfoExtended plist (Classic serial + FireWire
/// GUID): the prerequisite passes and the fixture is ready-capable.
#[test]
fn classic_with_firewire_passes() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MB029");
    seed_classic_sysinfo_extended(dir.path(), "YM6312345Y5N", "000A27001DB3C2D6");
    preflight(&backend, dir.path()).unwrap();
}

/// An empty SysInfo is the unknown-model case (the real device snapshot's
/// state): Model failure, never a silent artless device.
#[test]
fn unknown_model_fails() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MA446");
    std::fs::write(dir.path().join("iPod_Control/Device/SysInfo"), "").unwrap();
    let error = expect_check(preflight(&backend, dir.path()), ReadinessCheck::Model);
    assert!(
        error.to_string().contains("could not be resolved"),
        "diagnostic must explain the unknown model: {error}"
    );
}

/// A Nano fixture (MA350 → Nano 1G): outside the Video/Classic scope —
/// Model failure naming the unsupported device.
#[test]
fn unsupported_model_fails() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MA350");
    let error = expect_check(preflight(&backend, dir.path()), ReadinessCheck::Model);
    assert!(
        error.to_string().contains("unsupported model"),
        "diagnostic must say unsupported: {error}"
    );
}

/// A corrupt iTunesDB fails the parse: Database failure.
#[test]
fn corrupt_itunesdb_fails_database() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MA446");
    let db = dir.path().join("iPod_Control/iTunes/iTunesDB");
    let mut bytes = std::fs::read(&db).unwrap();
    assert!(!bytes.is_empty(), "itdb_init_ipod must write an iTunesDB");
    // Break the mhbd magic — the parse must reject the database.
    for byte in bytes.iter_mut().take(4) {
        *byte = !*byte;
    }
    std::fs::write(&db, bytes).unwrap();
    let error = expect_check(preflight(&backend, dir.path()), ReadinessCheck::Database);
    assert!(
        error.to_string().contains("cannot be opened or parsed"),
        "diagnostic must explain the parse failure: {error}"
    );
}

/// Denied access: an unreadable iTunesDB fails the parse with the access
/// error in the diagnostic. Skipped when running as root (EACCES does not
/// apply).
#[test]
fn unreadable_itunesdb_fails_database() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, chmod 000 cannot deny access");
        return;
    }
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MA446");
    let db = dir.path().join("iPod_Control/iTunes/iTunesDB");
    let mut permissions = std::fs::metadata(&db).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o000);
    std::fs::set_permissions(&db, permissions).unwrap();
    let error = expect_check(preflight(&backend, dir.path()), ReadinessCheck::Database);
    eprintln!("denied-access diagnostic: {error}");
}

/// A corrupt ArtworkDB is the `Missing` verdict: Artwork failure carrying
/// the wrapper's diagnostic.
#[test]
fn corrupt_artworkdb_fails_artwork() {
    let Some(backend) = backend_or_skip() else {
        return;
    };
    let dir = fixture(&backend, "MA446");
    let db = dir.path().join("iPod_Control/Artwork/ArtworkDB");
    assert!(
        db.is_file(),
        "itdb_init_ipod must write an ArtworkDB ({} missing)",
        db.display()
    );
    std::fs::write(&db, b"not an artwork database").unwrap();
    let error = expect_check(preflight(&backend, dir.path()), ReadinessCheck::Artwork);
    assert!(
        error.to_string().contains("ArtworkDB"),
        "diagnostic must name the ArtworkDB: {error}"
    );
}
