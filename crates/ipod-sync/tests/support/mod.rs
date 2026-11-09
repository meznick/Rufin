//! Shared helpers for the ipod-sync env-gated test suites.
//!
//! Both test binaries (`discovery.rs`, `retrieval.rs`) hash real trees, and
//! [`stable_tree_hash`] lives here so the loop-artifact handling does not
//! drift between them. The eprintln! convention of the crate applies to
//! this module too: the transient-read-artifact warning is a diagnostic of
//! a real phenomenon, never print-debugging.

use std::path::Path;

/// A tree hash robust against the loop device's transient read artifact.
///
/// The 2Kn loop mount has been observed serving intermittent single-bit-
/// corrupted reads under IO load (one captured instance: byte `0xDD → 0xCD`
/// in a music file; the image file itself never changed — see
/// `deltas/b005_delta.md`). A flip can hit ANY single read, so the mounted
/// gates take two consecutive equal hashes as the stable value: two
/// corrupted reads in a row agreeing is beyond plausible, while a real
/// (persistent) change stays visible in every read. The snapshot gates hash
/// a plain host filesystem and deliberately do not use this helper.
pub fn stable_tree_hash(root: &Path) -> String {
    for attempt in 0..5 {
        let first = ipod::tree_hash(root).unwrap();
        let second = ipod::tree_hash(root).unwrap();
        if first == second {
            return first;
        }
        eprintln!("transient read artifact (attempt {attempt}); re-reading");
    }
    panic!(
        "tree hash did not stabilize after 5 attempts — the device tree is changing or the read path is badly corrupted"
    );
}
