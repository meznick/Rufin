//! Fixture builder for integration tests — compiled only with the
//! `sync-writes` feature (see the crate root docs on the W-7 gate).
//!
//! Produces disposable device trees with libgpod's own writer
//! (`itdb_init_ipod`) — the same primitive the B-007 §1.1 experiments used
//! to build their fixtures, now inside the test suite. Tests create a
//! `tempdir`, initialize, seed `SysInfo`, open a session, add tracks and
//! artwork through the gated write surface, `write_library`, and reparse.
//! Nothing here ever runs against a live pod.

use std::ffi::{CString, c_char};
use std::path::Path;

use crate::IpodBackend;
use crate::with_native;
use crate::write::{OperationFailure, SYNC_WRITES_ENABLED};

/// A minimal valid 1×1 transparent PNG, embedded so the fixture tests do not
/// depend on external test assets. libgpod's gdk-pixbuf loader decodes it;
/// `set_artwork` fails the round-trip test if this ever stops being valid.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x60, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Initialize an empty device tree at `mount` with libgpod's own
/// `itdb_init_ipod`. `model_number` selects the model (use `MA446` for the
/// Video 5.5g — the same fixture model the research used).
pub fn init_device(
    backend: &IpodBackend,
    mount: &Path,
    model_number: &str,
    ipod_name: &str,
) -> Result<(), OperationFailure> {
    if !SYNC_WRITES_ENABLED {
        return Err(OperationFailure::WritesGated);
    }
    with_native(|| {
        let write = backend
            .write()
            .ok_or(OperationFailure::BackendSymbolMissing {
                symbol: "itdb_init_ipod",
            })?;
        let mp = CString::new(mount.as_os_str().as_encoded_bytes()).map_err(|_| {
            OperationFailure::NotFound("mount point contains a NUL byte".to_string())
        })?;
        let model = CString::new(model_number).map_err(|_| {
            OperationFailure::InvalidState("model number contains a NUL byte".to_string())
        })?;
        let name = CString::new(ipod_name).map_err(|_| {
            OperationFailure::InvalidState("device name contains a NUL byte".to_string())
        })?;
        let mut error = std::ptr::null_mut();
        // SAFETY: all inputs are valid NUL-terminated strings for the call;
        // `error` is a valid GError** out-param. `itdb_init_ipod` creates
        // the device tree at `mount` — test fixtures only.
        let ok = unsafe {
            (write.itdb_init_ipod)(mp.as_ptr(), model.as_ptr(), name.as_ptr(), &mut error)
        };
        if ok == 0 {
            return Err(
                match unsafe { crate::ffi::take_gerror(&mut error, backend.glib()) } {
                    Some((domain, code, message)) => OperationFailure::GError {
                        domain,
                        code,
                        message,
                    },
                    None => OperationFailure::InvalidState(
                        "itdb_init_ipod failed without a GError".to_string(),
                    ),
                },
            );
        }
        Ok(())
    })
}

/// Seed the device `SysInfo` with a `ModelNumStr` line — the model gate
/// (§1.2) depends on it. The file format is the text key/value form libgpod
/// itself parses (`itdb_device.c`: `fgets` + split at the first `:`); an
/// empty `SysInfo` is exactly the "model unknown" case the wrapper reports.
pub fn seed_sysinfo(mount: &Path, model_num_str: &str) -> std::io::Result<()> {
    let dir = mount.join("iPod_Control").join("Device");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("SysInfo"),
        format!("ModelNumStr: {model_num_str}\n"),
    )
}

/// Allocate a GLib-owned copy of a string, for tests that set track fields
/// directly (the string fields of tracks added via `create_track` are set
/// through the write surface; this helper covers the raw paths).
///
/// # Safety
///
/// `value` must be valid for the call. The result must be freed with
/// `g_free` by its owner (the track/database).
pub unsafe fn g_strdup(backend: &IpodBackend, value: &str) -> Option<*mut c_char> {
    let cvalue = CString::new(value).ok()?;
    unsafe { backend.glib().g_strdup(cvalue.as_ptr()) }
}
