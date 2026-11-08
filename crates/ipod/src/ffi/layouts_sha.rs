//! SHA-256 of `ffi/layouts.rs` (the committed file, exactly as checked in).
//! This is the value the `rufin-libgpod.abi` marker must carry; the loader
//! compares the marker against this constant, so a library built against
//! different struct layouts can never be used with these mirrors.
//!
//! Regenerate together with `layouts.rs` (see its header comment):
//!
//! ```text
//! sha256sum crates/ipod/src/ffi/layouts.rs
//! ```

/// `LAYOUTS=<this value>` line of the ABI marker, including the `sha256:`
/// prefix.
pub const LAYOUTS_SHA256: &str =
    "sha256:cd93ed97aa1ab7cda96a13d94b9f0df45ced5ab5d6f60d6792b0a8a27bad4816";
