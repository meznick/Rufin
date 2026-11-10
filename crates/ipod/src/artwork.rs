//! Artwork evidence without gdk-pixbuf (W-4).
//!
//! libgpod's own thumbnail decode (`itdb_thumb_to_pixbufs`) pulls in
//! gdk-pixbuf; this crate deliberately does not. Instead it parses the
//! device's `ArtworkDB` (mhfd/mhsd/mhli/mhii container) to count declared
//! entries independently of libgpod and to locate raw pixels, then decodes
//! the RGB565 payload straight out of the `.ithmb` bank for the proof-read.
//!
//! The container layout and the RGB565 packing were reverse-engineered in
//! itl-player `tools/ipod_inspect.py` and cross-verified against libgpod's
//! own decoded output (pixel-identical; `NOTES.md` §20). This port is
//! bounds-checked at every step: device data can never make it panic, a
//! malformed file yields an `Err` with the offset-level reason.
//!
//! # Format recap (verified on the restored device, 2026-11-08)
//!
//! - `ArtworkDB` = `mhfd` (0x84-byte header) + `mhsd` blocks. The image list
//!   has `mhsd` index 1 (`u16` at +0x0C — ArtworkDB uses a u16 here, unlike
//!   iTunesDB), containing `mhli` → `mhii` entries, one per artwork-bearing
//!   track. Album list (index 2) was empty on the real pod (3356 image
//!   entries, 0 album entries); this parser counts the image list only.
//! - `mhii` children `mhod` of type 2 hold `mhni` thumbnail records:
//!   `format_id` (i32 +0x10), `ithmb_offset` (u32 +0x14), `image_size`
//!   (u32 +0x18), `vpad`/`hpad` (i16 +0x1C/+0x1E), `height`/`width`
//!   (i16 +0x20/+0x22). The `mhni`'s own `mhod` (type 3) names the bank
//!   file, e.g. `:F1028_1.ithmb` — the leading `:` is iTunes' path marker,
//!   the on-disk name is the part after it.
//! - The bank stores fixed-size blocks of RGB565 little-endian: stride =
//!   `(width + hpad) * 2` bytes, `height + vpad` rows, the real content in
//!   the top-left `width × height`. The real pod uses 1028 = 100×100 and
//!   1029 = 200×200 with zero pads (F1028_1 = 3356 × 20000 B, F1029_1 =
//!   3356 × 80000 B).

use std::path::Path;

use crate::types::{ArtworkVerdict, ProofReadFailure, ProofReadReport};

/// mhfd header length; also the offset of the first `mhsd`.
const MHFD_HEADER_LEN: usize = 0x84;

/// `mhsd` index of the image list (track artwork).
const MHSD_IMAGE_LIST: u16 = 1;
/// `mhsd` index of the album list (album artwork, `song_id == 0`). Not
/// counted — see the module docs for the real-device evidence.
const MHSD_ALBUM_LIST: u16 = 2;

/// `mhod` type of a thumbnail container.
const MHOD_TYPE_THUMBNAIL: u16 = 2;
/// `mhod` type of a file-name string.
const MHOD_TYPE_FILE_NAME: u16 = 3;

/// One parsed ArtworkDB: the declared-entry count and the first thumbnail
/// record of every distinct format (that is all the proof-read needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtworkDbParse {
    /// Number of `mhii` entries in the image list — the "declared entries"
    /// of [`crate::types::ArtworkStatus`].
    pub declared: u64,
    /// First thumbnail per distinct format id, in file order.
    pub first_per_format: Vec<FormatThumb>,
}

/// A thumbnail record inside an `mhii` (one per artwork format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormatThumb {
    pub format_id: i32,
    /// Byte offset of the block inside the bank file.
    pub offset: u64,
    /// Bytes of the block (incl. padding).
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub hpad: u32,
    pub vpad: u32,
    /// Bank file name, sanitized (leading `:` stripped).
    pub ithmb_file: String,
}

/// Parse an ArtworkDB image in memory. Bounds-checked; returns an
/// offset-level reason on any structural deviation.
pub(crate) fn parse_artwork_db(data: &[u8]) -> Result<ArtworkDbParse, String> {
    if data.len() < 8 || &data[..4] != b"mhfd" {
        return Err("not an ArtworkDB (mhfd magic missing)".to_string());
    }
    let header_len = le_u32(data, 4)? as usize;
    let total_len = le_u32(data, 8)? as usize;
    if header_len != MHFD_HEADER_LEN {
        return Err(format!(
            "unexpected mhfd header length {header_len} (expected {MHFD_HEADER_LEN})"
        ));
    }
    let end = total_len.min(data.len());
    let mut declared: u64 = 0;
    let mut first_per_format: Vec<FormatThumb> = Vec::new();
    let mut off = MHFD_HEADER_LEN;
    while off + 8 <= end {
        if &data[off..off + 4] != b"mhsd" {
            // Tolerate trailing junk after the last block; a wrong magic
            // *inside* the declared length is the caller's parse failure.
            break;
        }
        let hlen = le_u32(data, off + 4)? as usize;
        let tlen = le_u32(data, off + 8)? as usize;
        if tlen == 0 {
            // Zero-advance guard: `off += tlen` must always move forward, or
            // a malformed file could spin this loop forever.
            return Err(format!("mhsd total length is zero at {off:#x}"));
        }
        let index = le_u16(data, off + 0x0C)?;
        if index == MHSD_IMAGE_LIST {
            let list_end = (off + tlen).min(end);
            walk_image_list(
                data,
                (off + hlen).min(list_end),
                list_end,
                &mut declared,
                &mut first_per_format,
            )?;
        } else if index != MHSD_ALBUM_LIST {
            // Other list kinds (file list, ...) are skipped, not errors.
            tracing::debug!(index, "skipping non-image ArtworkDB list");
        }
        off += tlen;
    }
    Ok(ArtworkDbParse {
        declared,
        first_per_format,
    })
}

/// Walk `mhli` → `mhii` entries inside one image-list `mhsd`.
fn walk_image_list(
    data: &[u8],
    start: usize,
    end: usize,
    declared: &mut u64,
    first_per_format: &mut Vec<FormatThumb>,
) -> Result<(), String> {
    let off = start;
    if off + 8 > end {
        return Ok(());
    }
    if &data[off..off + 4] != b"mhli" {
        return Err(format!("expected mhli at {off:#x}"));
    }
    let hlen = le_u32(data, off + 4)? as usize;
    let children = le_u32(data, off + 8)? as u64;
    let mut child_off = (off + hlen).min(end);
    for _ in 0..children {
        if child_off + 8 > end || &data[child_off..child_off + 4] != b"mhii" {
            return Err(format!(
                "mhli declared {children} children, truncated at {child_off:#x}"
            ));
        }
        let tlen = le_u32(data, child_off + 8)? as usize;
        if tlen == 0 {
            // Zero-advance guard: a declared children count combined with a
            // zero-length child would re-read the same bytes up to u32::MAX
            // times.
            return Err(format!("mhii total length is zero at {child_off:#x}"));
        }
        parse_mhii(data, child_off, end, first_per_format)?;
        *declared += 1;
        child_off = (child_off + tlen).min(end);
    }
    Ok(())
}

/// Parse one `mhii`: walk its `mhod` children for thumbnail records.
fn parse_mhii(
    data: &[u8],
    off: usize,
    end: usize,
    first_per_format: &mut Vec<FormatThumb>,
) -> Result<(), String> {
    let hlen = le_u32(data, off + 4)? as usize;
    let tlen = le_u32(data, off + 8)? as usize;
    let children = le_u32(data, off + 0x0C)? as u64;
    let mut child_off = (off + hlen).min(end);
    let entry_end = (off + tlen).min(end);
    for _ in 0..children {
        if child_off + 8 > entry_end || &data[child_off..child_off + 4] != b"mhod" {
            return Err(format!("mhii children truncated at {child_off:#x}"));
        }
        let mhod_hlen = le_u32(data, child_off + 4)? as usize;
        let mhod_tlen = le_u32(data, child_off + 8)? as usize;
        if mhod_tlen == 0 {
            // Zero-advance guard (same hazard as the mhii walk).
            return Err(format!("mhod total length is zero at {child_off:#x}"));
        }
        let mhod_type = le_u16(data, child_off + 0x0C)?;
        if mhod_type == MHOD_TYPE_THUMBNAIL {
            let inner_end = (child_off + mhod_tlen).min(entry_end);
            walk_mhni(
                data,
                (child_off + mhod_hlen).min(inner_end),
                inner_end,
                first_per_format,
            )?;
        }
        child_off = (child_off + mhod_tlen).min(entry_end);
    }
    Ok(())
}

/// Walk `mhni` records inside a thumbnail `mhod`; keep the first per format.
fn walk_mhni(
    data: &[u8],
    mut off: usize,
    end: usize,
    first_per_format: &mut Vec<FormatThumb>,
) -> Result<(), String> {
    while off + 8 <= end {
        if &data[off..off + 4] != b"mhni" {
            break;
        }
        let hlen = le_u32(data, off + 4)? as usize;
        let tlen = le_u32(data, off + 8)? as usize;
        if tlen == 0 {
            // Zero-advance guard: this `while` is unbounded, so a zero-length
            // mhni would spin forever.
            return Err(format!("mhni total length is zero at {off:#x}"));
        }
        let format_id = le_i32(data, off + 0x10)?;
        let offset = le_u32(data, off + 0x14)? as u64;
        let size = le_u32(data, off + 0x18)? as u64;
        let vpad = le_i16(data, off + 0x1C)?;
        let hpad = le_i16(data, off + 0x1E)?;
        let height = le_i16(data, off + 0x20)?;
        let width = le_i16(data, off + 0x22)?;

        // The bank file name lives in the mhni's own mhod (type 3).
        let ithmb_file = mhni_filename(data, (off + hlen).min(end), (off + tlen).min(end));

        let known_format = first_per_format
            .iter()
            .any(|thumb| thumb.format_id == format_id);
        if !known_format && width > 0 && height > 0 {
            first_per_format.push(FormatThumb {
                format_id,
                offset,
                size,
                width: width as u32,
                height: height as u32,
                hpad: hpad.max(0) as u32,
                vpad: vpad.max(0) as u32,
                ithmb_file,
            });
        }
        off += tlen;
    }
    Ok(())
}

/// Find the bank file name (an `mhod` of type 3) among an `mhni`'s children.
fn mhni_filename(data: &[u8], mut off: usize, end: usize) -> String {
    while off + 8 <= end {
        if &data[off..off + 4] != b"mhod" {
            break;
        }
        let mhod_tlen = le_u32(data, off + 8).unwrap_or(0) as usize;
        if mhod_tlen == 0 {
            // Zero-advance guard (this walk is unbounded); an empty name
            // makes the proof-read fail with "no bank file name" later.
            break;
        }
        let mhod_type = le_u16(data, off + 0x0C).unwrap_or(0);
        if mhod_type == MHOD_TYPE_FILE_NAME {
            let s_len = le_i32(data, off + 0x18).unwrap_or(0).max(0) as usize;
            let enc = data.get(off + 0x1C).copied().unwrap_or(0);
            let string_off = (off + 0x24).min(end);
            let string_end = (string_off + s_len).min(end);
            return sanitize_bank_name(&decode_string(&data[string_off..string_end], enc));
        }
        off = (off + mhod_tlen).min(end);
    }
    String::new()
}

/// Decode an mhod string per its encoding byte — mirrors itl-player
/// `tools/ipod_inspect.py` `cstr()`: 0/1 = UTF-8, 2 = UTF-16LE. The fixture
/// round-trip proved libgpod writes bank names as UTF-16 (enc 2), so this
/// is not a theoretical path.
fn decode_string(raw: &[u8], enc: u8) -> String {
    if enc == 2 {
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

/// iTunes writes bank paths with a leading `:` (`:F1028_1.ithmb`); the
/// on-disk name is the part after it.
fn sanitize_bank_name(text: &str) -> String {
    text.rsplit(':').next().unwrap_or_default().to_string()
}

/// Decode one RGB565 little-endian bank block into tightly packed RGBA8
/// (`width * height * 4` bytes). The color expansion matches
/// itl-player `tools/ipod_inspect.py` (`r << 3`, `g << 2`, `b << 3`), the
/// decoder whose output was verified pixel-identical against libgpod's.
///
/// A truncated block (fewer rows than declared) is an error — for a
/// proof-read, silently clamping would mask corruption.
pub(crate) fn decode_rgb565_le(
    raw: &[u8],
    width: u32,
    height: u32,
    hpad: u32,
    vpad: u32,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(format!("implausible dimensions {width}x{height}"));
    }
    let stride = (width + hpad)
        .checked_mul(2)
        .ok_or_else(|| format!("stride overflow for width {width}"))? as usize;
    if stride == 0 {
        return Err("zero stride".to_string());
    }
    let available = raw.len() / stride;
    if (height + vpad) as usize > available {
        return Err(format!(
            "block holds {available} rows, {} declared",
            height + vpad
        ));
    }
    let mut out = vec![0u8; (width as usize) * (height as usize) * 4];
    for row in 0..height as usize {
        let base = row * stride;
        for x in 0..width as usize {
            let pixel = le_u16(raw, base + x * 2)?;
            let r5 = ((pixel >> 11) & 0x1F) as u8;
            let g6 = ((pixel >> 5) & 0x3F) as u8;
            let b5 = (pixel & 0x1F) as u8;
            let out_off = (row * width as usize + x) * 4;
            out[out_off] = r5 << 3;
            out[out_off + 1] = g6 << 2;
            out[out_off + 2] = b5 << 3;
            out[out_off + 3] = 0xFF;
        }
    }
    Ok(out)
}

/// True when a bank file name is safe to join onto the artwork directory:
/// non-empty, no path separators, and not a `..`/`.` traversal component.
/// Bank names are flat (`F1028_1.ithmb`); anything else in device data is
/// corrupt or hostile and the proof-read must refuse it, not follow it.
fn bank_name_is_safe(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// Run the one-thumbnail-per-format proof-read against the bank files in
/// `artwork_dir`. Never fails structurally: every failure is recorded in the
/// returned report (the caller turns a failed proof into a `Missing`
/// verdict).
pub(crate) fn proof_read(artwork_dir: &Path, first_per_format: &[FormatThumb]) -> ProofReadReport {
    let mut report = ProofReadReport {
        formats_checked: Vec::new(),
        failed: None,
    };
    for thumb in first_per_format {
        if thumb.ithmb_file.is_empty() {
            report.failed = Some(ProofReadFailure {
                format_id: thumb.format_id as u32,
                reason: "no bank file name in mhni".to_string(),
            });
            break;
        }
        if !bank_name_is_safe(&thumb.ithmb_file) {
            report.failed = Some(ProofReadFailure {
                format_id: thumb.format_id as u32,
                reason: format!("unsafe bank file name {:?}", thumb.ithmb_file),
            });
            break;
        }
        let file = artwork_dir.join(&thumb.ithmb_file);
        let result = read_thumb_block(&file, thumb.offset, thumb.size).and_then(|block| {
            decode_rgb565_le(&block, thumb.width, thumb.height, thumb.hpad, thumb.vpad)
        });
        match result {
            Ok(_pixels) => report.formats_checked.push(thumb.format_id as u32),
            Err(reason) => {
                report.failed = Some(ProofReadFailure {
                    format_id: thumb.format_id as u32,
                    reason,
                });
                break;
            }
        }
    }
    report
}

/// Read one declared thumbnail block from an ithmb bank: open + seek +
/// bounded read of exactly the declared bytes. Deliberately NOT
/// `std::fs::read` — real bank files are hundreds of MB (F1029_1.ithmb is
/// ~268 MB on the test device) and the block is a few KB, so slurping the
/// whole bank would hold a plugged-in device busy and spike memory on
/// every scan.
fn read_thumb_block(file: &Path, offset: u64, size: u64) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut bank =
        std::fs::File::open(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let len = bank
        .metadata()
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?
        .len();
    let start = offset;
    let end = start
        .checked_add(size)
        .ok_or_else(|| format!("block {start}+{size} overflows in {}", file.display()))?;
    if end > len {
        return Err(format!(
            "block {start}..{end} outside {} ({} bytes)",
            file.display(),
            len
        ));
    }
    bank.seek(SeekFrom::Start(start))
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let mut block = vec![0u8; size as usize];
    bank.read_exact(&mut block)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    Ok(block)
}

// ---------------------------------------------------------------------------
// Bounds-checked little-endian readers: every access is validated against the
// slice, so malformed device data can never panic the parser.
// ---------------------------------------------------------------------------

fn le_u16(data: &[u8], off: usize) -> Result<u16, String> {
    let bytes: [u8; 2] = data
        .get(off..off + 2)
        .ok_or_else(|| format!("u16 read out of bounds at {off:#x}"))?
        .try_into()
        .expect("slice of 2");
    Ok(u16::from_le_bytes(bytes))
}

fn le_i16(data: &[u8], off: usize) -> Result<i16, String> {
    Ok(le_u16(data, off)? as i16)
}

fn le_u32(data: &[u8], off: usize) -> Result<u32, String> {
    let bytes: [u8; 4] = data
        .get(off..off + 4)
        .ok_or_else(|| format!("u32 read out of bounds at {off:#x}"))?
        .try_into()
        .expect("slice of 4");
    Ok(u32::from_le_bytes(bytes))
}

fn le_i32(data: &[u8], off: usize) -> Result<i32, String> {
    Ok(le_u32(data, off)? as i32)
}

/// Pure verdict computation (W-4), shared by the session and the tests.
///
/// Rules (B-007 §1.2 / W-4):
/// - model gate closed → `SkippedUnknownModel` regardless of counts;
/// - ArtworkDB absent or unreadable (`declared == None`) → `Missing`;
/// - `declared == 0 && parsed == 0` → `Ok` (legitimately artless);
/// - `parsed < declared` → `Missing`;
/// - a failed proof-read → `Missing` even when counts match;
/// - otherwise `Ok`.
pub(crate) fn classify(
    model_known: bool,
    declared: Option<u64>,
    parsed: u64,
    proof_failed: Option<&ProofReadFailure>,
) -> (ArtworkVerdict, Option<String>) {
    if !model_known {
        return (
            ArtworkVerdict::SkippedUnknownModel,
            Some("model unknown (empty SysInfo): libgpod skipped artwork parsing".to_string()),
        );
    }
    let Some(declared) = declared else {
        return (
            ArtworkVerdict::Missing,
            Some("ArtworkDB missing or unreadable".to_string()),
        );
    };
    if declared == 0 && parsed == 0 {
        return (ArtworkVerdict::Ok, None);
    }
    if parsed < declared {
        return (
            ArtworkVerdict::Missing,
            Some(format!(
                "ArtworkDB declares {declared} entries, {parsed} tracks carry thumbnails"
            )),
        );
    }
    if let Some(failure) = proof_failed {
        return (
            ArtworkVerdict::Missing,
            Some(format!(
                "proof-read failed for format {}: {}",
                failure.format_id, failure.reason
            )),
        );
    }
    (ArtworkVerdict::Ok, None)
}

#[cfg(test)]
mod tests {
    // Skip diagnostics only (the real-ArtworkDB-copy test is env-gated on a
    // local research artifact) — not print-debugging.
    #![allow(clippy::print_stderr)]

    use super::*;
    use crate::types::ArtworkVerdict;

    /// Build a minimal synthetic ArtworkDB with one image-list `mhii` that
    /// declares one 2×2 thumbnail (format 1028, block of 8 bytes at offset
    /// 16) in bank `:F1028_1.ithmb`. Header lengths are the format's own
    /// (mhfd 0x84, mhsd 0x18, mhli 0x10, mhii 0x30, mhod 0x18/0x24,
    /// mhni 0x24); the parser walks by declared hlen/tlen, so the sizes are
    /// self-consistent by construction.
    fn synthetic_artwork_db() -> Vec<u8> {
        fn put_u16(data: &mut [u8], off: usize, v: u16) {
            data[off..off + 2].copy_from_slice(&v.to_le_bytes())
        }
        fn put_u32(data: &mut [u8], off: usize, v: u32) {
            data[off..off + 4].copy_from_slice(&v.to_le_bytes())
        }
        fn put_i32(data: &mut [u8], off: usize, v: i32) {
            data[off..off + 4].copy_from_slice(&v.to_le_bytes())
        }

        let name = b":F1028_1.ithmb";
        let name_mhod_len = 0x24 + name.len();
        let mhni_len = 0x24 + name_mhod_len;
        let thumb_mhod_len = 0x18 + mhni_len;
        let mhii_len = 0x30 + thumb_mhod_len;
        let mhli_len = 0x10 + mhii_len;
        let mhsd_len = 0x18 + mhli_len;

        let mut data = vec![0u8; MHFD_HEADER_LEN + mhsd_len];

        // mhfd
        data[0..4].copy_from_slice(b"mhfd");
        put_u32(&mut data, 4, MHFD_HEADER_LEN as u32);
        put_u32(&mut data, 8, (MHFD_HEADER_LEN + mhsd_len) as u32);
        put_u32(&mut data, 0x14, 1); // num_children

        // mhsd (image list)
        let mhsd = MHFD_HEADER_LEN;
        data[mhsd..mhsd + 4].copy_from_slice(b"mhsd");
        put_u32(&mut data, mhsd + 4, 0x18);
        put_u32(&mut data, mhsd + 8, mhsd_len as u32);
        put_u16(&mut data, mhsd + 0x0C, MHSD_IMAGE_LIST);

        // mhli
        let mhli = mhsd + 0x18;
        data[mhli..mhli + 4].copy_from_slice(b"mhli");
        put_u32(&mut data, mhli + 4, 0x10);
        put_u32(&mut data, mhli + 8, 1);

        // mhii
        let mhii = mhli + 0x10;
        data[mhii..mhii + 4].copy_from_slice(b"mhii");
        put_u32(&mut data, mhii + 4, 0x30);
        put_u32(&mut data, mhii + 8, mhii_len as u32);
        put_u32(&mut data, mhii + 0x0C, 1);
        put_i32(&mut data, mhii + 0x10, 42); // image_id
        put_u32(&mut data, mhii + 0x14, 7); // song_id (lo)

        // mhod (type 2, thumbnail container)
        let thumb_mhod = mhii + 0x30;
        data[thumb_mhod..thumb_mhod + 4].copy_from_slice(b"mhod");
        put_u32(&mut data, thumb_mhod + 4, 0x18);
        put_u32(&mut data, thumb_mhod + 8, thumb_mhod_len as u32);
        put_u16(&mut data, thumb_mhod + 0x0C, MHOD_TYPE_THUMBNAIL);

        // mhni
        let mhni = thumb_mhod + 0x18;
        data[mhni..mhni + 4].copy_from_slice(b"mhni");
        put_u32(&mut data, mhni + 4, 0x24);
        put_u32(&mut data, mhni + 8, mhni_len as u32);
        put_i32(&mut data, mhni + 0x10, 1028); // format_id
        put_u32(&mut data, mhni + 0x14, 16); // ithmb_offset
        put_u32(&mut data, mhni + 0x18, 8); // image_size (2x2x2)
        put_u16(&mut data, mhni + 0x1E, 0); // hpad
        put_u16(&mut data, mhni + 0x20, 2); // height
        put_u16(&mut data, mhni + 0x22, 2); // width

        // mhod (type 3, bank file name)
        let name_mhod = mhni + 0x24;
        data[name_mhod..name_mhod + 4].copy_from_slice(b"mhod");
        put_u32(&mut data, name_mhod + 4, 0x24);
        put_u32(&mut data, name_mhod + 8, name_mhod_len as u32);
        put_u16(&mut data, name_mhod + 0x0C, MHOD_TYPE_FILE_NAME);
        put_i32(&mut data, name_mhod + 0x18, name.len() as i32);
        data[name_mhod + 0x24..name_mhod + 0x24 + name.len()].copy_from_slice(name);
        data
    }

    #[test]
    fn parser_handles_synthetic_happy_path() {
        let parsed = parse_artwork_db(&synthetic_artwork_db()).unwrap();
        assert_eq!(parsed.declared, 1);
        assert_eq!(parsed.first_per_format.len(), 1);
        let thumb = &parsed.first_per_format[0];
        assert_eq!(thumb.format_id, 1028);
        assert_eq!(thumb.offset, 16);
        assert_eq!(thumb.size, 8);
        assert_eq!((thumb.width, thumb.height), (2, 2));
        assert_eq!(thumb.ithmb_file, "F1028_1.ithmb");
    }

    /// Parses the real restored-device ArtworkDB copy when it exists (the
    /// same file the B-007 cross-checks used): 3356 image-list entries,
    /// formats 1028 (100×100) and 1029 (200×200), zero pads.
    #[test]
    fn parser_matches_real_artworkdb_copy() {
        let path = std::path::Path::new("/tmp/opencode/libgpod-check/copies/ArtworkDB");
        let Ok(data) = std::fs::read(path) else {
            eprintln!("skipping: {} not present", path.display());
            return;
        };
        let parsed = parse_artwork_db(&data).unwrap();
        assert_eq!(parsed.declared, 3356);
        let formats: Vec<i32> = parsed
            .first_per_format
            .iter()
            .map(|t| t.format_id)
            .collect();
        assert_eq!(formats, vec![1028, 1029]);
        assert_eq!(
            (
                parsed.first_per_format[0].width,
                parsed.first_per_format[0].height
            ),
            (100, 100)
        );
        assert_eq!(parsed.first_per_format[0].size, 20000);
    }

    #[test]
    fn proof_read_decodes_first_thumb_per_format() {
        let dir = tempfile::tempdir().unwrap();
        // A 2x2 RGB565 block at offset 16 in the bank.
        let mut bank = vec![0u8; 16];
        bank.extend_from_slice(&[0x00, 0xF8, 0xE0, 0x07, 0x1F, 0x00, 0xFF, 0xFF]);
        std::fs::write(dir.path().join("F1028_1.ithmb"), &bank).unwrap();
        let first = vec![FormatThumb {
            format_id: 1028,
            offset: 16,
            size: 8,
            width: 2,
            height: 2,
            hpad: 0,
            vpad: 0,
            ithmb_file: "F1028_1.ithmb".to_string(),
        }];
        let report = proof_read(dir.path(), &first);
        assert_eq!(report.formats_checked, vec![1028]);
        assert!(report.failed.is_none());
    }

    #[test]
    fn proof_read_reports_missing_bank() {
        let dir = tempfile::tempdir().unwrap();
        let first = vec![FormatThumb {
            format_id: 1028,
            offset: 0,
            size: 8,
            width: 2,
            height: 2,
            hpad: 0,
            vpad: 0,
            ithmb_file: "F1028_1.ithmb".to_string(),
        }];
        let report = proof_read(dir.path(), &first);
        let failure = report.failed.expect("proof-read must fail");
        assert_eq!(failure.format_id, 1028);
        assert!(failure.reason.contains("cannot read"), "{}", failure.reason);
    }

    #[test]
    fn proof_read_reports_truncated_block() {
        let dir = tempfile::tempdir().unwrap();
        // 4 bytes: not enough for the declared 2x2 block of 8.
        std::fs::write(dir.path().join("F1028_1.ithmb"), b"1234").unwrap();
        let first = vec![FormatThumb {
            format_id: 1028,
            offset: 0,
            size: 8,
            width: 2,
            height: 2,
            hpad: 0,
            vpad: 0,
            ithmb_file: "F1028_1.ithmb".to_string(),
        }];
        let report = proof_read(dir.path(), &first);
        let failure = report.failed.expect("proof-read must fail");
        assert!(failure.reason.contains("outside"), "{}", failure.reason);
    }

    #[test]
    fn decoder_matches_python_scaling() {
        // 2x2, no pads: 0xF800 (red), 0x07E0 (green), 0x001F (blue),
        // 0xFFFF (white).
        let raw = [0x00, 0xF8, 0xE0, 0x07, 0x1F, 0x00, 0xFF, 0xFF];
        let pixels = decode_rgb565_le(&raw, 2, 2, 0, 0).unwrap();
        assert_eq!(
            pixels,
            vec![
                0xF8, 0x00, 0x00, 0xFF, // red
                0x00, 0xFC, 0x00, 0xFF, // green
                0x00, 0x00, 0xF8, 0xFF, // blue
                0xF8, 0xFC, 0xF8, 0xFF, // white
            ]
        );
    }

    #[test]
    fn decoder_honors_stride_padding() {
        // width 1, hpad 1 -> stride 4; two rows of 4 bytes, second pixel row
        // is green.
        let raw = [
            0x00, 0xF8, 0xAA, 0xAA, // row 0: red + pad
            0xE0, 0x07, 0xBB, 0xBB, // row 1: green + pad
        ];
        let pixels = decode_rgb565_le(&raw, 1, 2, 1, 0).unwrap();
        assert_eq!(pixels, vec![0xF8, 0, 0, 0xFF, 0, 0xFC, 0, 0xFF]);
    }

    #[test]
    fn decoder_rejects_truncated_blocks() {
        let err = decode_rgb565_le(&[0x00, 0xF8], 2, 2, 0, 0).unwrap_err();
        assert!(err.contains("holds"), "{err}");
    }

    #[test]
    fn decoder_rejects_implausible_dimensions() {
        assert!(decode_rgb565_le(&[], 0, 2, 0, 0).is_err());
        assert!(decode_rgb565_le(&[], 8192, 2, 0, 0).is_err());
    }

    #[test]
    fn parser_rejects_non_artworkdb() {
        assert!(parse_artwork_db(b"mhbd....").unwrap_err().contains("mhfd"));
        assert!(parse_artwork_db(&[]).is_err());
    }

    #[test]
    fn parser_rejects_garbage_in_list_region() {
        // mhfd → mhsd (image list) whose content region is not an mhli.
        let mhsd_len = 0x20usize;
        let mut data = vec![0u8; MHFD_HEADER_LEN + mhsd_len];
        data[0..4].copy_from_slice(b"mhfd");
        data[4..8].copy_from_slice(&(MHFD_HEADER_LEN as u32).to_le_bytes());
        let total_len = data.len() as u32;
        data[8..12].copy_from_slice(&total_len.to_le_bytes());
        data[MHFD_HEADER_LEN..MHFD_HEADER_LEN + 4].copy_from_slice(b"mhsd");
        data[MHFD_HEADER_LEN + 4..MHFD_HEADER_LEN + 8].copy_from_slice(&0x18u32.to_le_bytes());
        data[MHFD_HEADER_LEN + 8..MHFD_HEADER_LEN + 12]
            .copy_from_slice(&(mhsd_len as u32).to_le_bytes());
        data[MHFD_HEADER_LEN + 0x0C..MHFD_HEADER_LEN + 0x0E]
            .copy_from_slice(&MHSD_IMAGE_LIST.to_le_bytes());
        // content region: not an mhli
        data[MHFD_HEADER_LEN + 0x18..MHFD_HEADER_LEN + 0x1C].copy_from_slice(b"XXXX");
        let err = parse_artwork_db(&data).unwrap_err();
        assert!(err.contains("mhli"), "{err}");
    }

    #[test]
    fn parser_rejects_short_mhii_children() {
        // mhfd -> mhsd -> mhli declaring 3 children with none present.
        let mut data = vec![0u8; MHFD_HEADER_LEN + 0x20 + 0x10];
        data[0..4].copy_from_slice(b"mhfd");
        data[4..8].copy_from_slice(&(MHFD_HEADER_LEN as u32).to_le_bytes());
        let total_len = data.len() as u32;
        data[8..12].copy_from_slice(&total_len.to_le_bytes());
        let off = MHFD_HEADER_LEN;
        data[off..off + 4].copy_from_slice(b"mhsd");
        data[off + 4..off + 8].copy_from_slice(&0x18u32.to_le_bytes());
        data[off + 8..off + 12].copy_from_slice(&0x20u32.to_le_bytes());
        data[off + 0x0C..off + 0x0E].copy_from_slice(&MHSD_IMAGE_LIST.to_le_bytes());
        let li = off + 0x18;
        data[li..li + 4].copy_from_slice(b"mhli");
        data[li + 4..li + 8].copy_from_slice(&0x10u32.to_le_bytes());
        data[li + 8..li + 12].copy_from_slice(&3u32.to_le_bytes());
        let err = parse_artwork_db(&data).unwrap_err();
        assert!(err.contains("truncated"), "{err}");
    }

    /// Zero-length block guards: without them the walks either spin forever
    /// (the unbounded `while` loops) or re-read the same bytes up to the
    /// declared child count (u32::MAX). Each guard turns the case into an
    /// `Err` instead of a hang.
    #[test]
    fn parser_rejects_zero_length_mhsd() {
        let mut data = vec![0u8; MHFD_HEADER_LEN + 0x10];
        data[0..4].copy_from_slice(b"mhfd");
        data[4..8].copy_from_slice(&(MHFD_HEADER_LEN as u32).to_le_bytes());
        let total_len = data.len() as u32;
        data[8..12].copy_from_slice(&total_len.to_le_bytes());
        let off = MHFD_HEADER_LEN;
        data[off..off + 4].copy_from_slice(b"mhsd");
        data[off + 4..off + 8].copy_from_slice(&0x18u32.to_le_bytes());
        // tlen left zero on purpose.
        data[off + 0x0C..off + 0x0E].copy_from_slice(&MHSD_IMAGE_LIST.to_le_bytes());
        let err = parse_artwork_db(&data).unwrap_err();
        assert!(err.contains("zero"), "{err}");
    }

    #[test]
    fn parser_rejects_zero_length_mhii() {
        // mhfd -> mhsd -> mhli declaring 1 child whose mhii has tlen 0.
        let mut data = vec![0u8; MHFD_HEADER_LEN + 0x20 + 0x18];
        data[0..4].copy_from_slice(b"mhfd");
        data[4..8].copy_from_slice(&(MHFD_HEADER_LEN as u32).to_le_bytes());
        let total_len = data.len() as u32;
        data[8..12].copy_from_slice(&total_len.to_le_bytes());
        let off = MHFD_HEADER_LEN;
        data[off..off + 4].copy_from_slice(b"mhsd");
        data[off + 4..off + 8].copy_from_slice(&0x18u32.to_le_bytes());
        data[off + 8..off + 12].copy_from_slice(&(0x20 + 0x18u32).to_le_bytes());
        data[off + 0x0C..off + 0x0E].copy_from_slice(&MHSD_IMAGE_LIST.to_le_bytes());
        let li = off + 0x18;
        data[li..li + 4].copy_from_slice(b"mhli");
        data[li + 4..li + 8].copy_from_slice(&0x10u32.to_le_bytes());
        data[li + 8..li + 12].copy_from_slice(&1u32.to_le_bytes());
        let mhii = li + 0x10;
        data[mhii..mhii + 4].copy_from_slice(b"mhii");
        data[mhii + 4..mhii + 8].copy_from_slice(&0x30u32.to_le_bytes());
        // mhii tlen left zero on purpose.
        let err = parse_artwork_db(&data).unwrap_err();
        assert!(err.contains("zero"), "{err}");
    }

    /// `mhni_filename` is an unbounded walk too; a zero-length mhod must make
    /// it give up (empty name → proof-read failure), not spin.
    #[test]
    fn filename_walk_survives_zero_length_mhod() {
        let mut data = vec![0u8; 0x24 + 8];
        let off = 0x24;
        data[off..off + 4].copy_from_slice(b"mhod");
        // mhod_tlen left zero on purpose.
        assert_eq!(mhni_filename(&data, off, data.len()), "");
    }

    /// A hostile bank name must never redirect the (read-only) proof-read
    /// outside the artwork directory.
    #[test]
    fn proof_read_rejects_path_traversal_name() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../outside", "a/b", "a\\b", ".."] {
            let first = vec![FormatThumb {
                format_id: 1028,
                offset: 0,
                size: 8,
                width: 2,
                height: 2,
                hpad: 0,
                vpad: 0,
                ithmb_file: name.to_string(),
            }];
            let report = proof_read(dir.path(), &first);
            let failure = report.failed.expect("proof-read must refuse");
            assert!(failure.reason.contains("unsafe"), "{}", failure.reason);
        }
        // The leading-`:` form sanitizes down to a safe flat name.
        assert!(bank_name_is_safe("F1028_1.ithmb"));
        assert!(!bank_name_is_safe(""));
    }

    #[test]
    fn bank_names_are_sanitized() {
        assert_eq!(sanitize_bank_name(":F1028_1.ithmb"), "F1028_1.ithmb");
        assert_eq!(sanitize_bank_name("F1028_1.ithmb"), "F1028_1.ithmb");
        assert_eq!(sanitize_bank_name(""), "");
    }

    #[test]
    fn strings_decode_per_encoding_byte() {
        // UTF-8 (enc 0/1).
        assert_eq!(decode_string(b"F1028_1.ithmb", 0), "F1028_1.ithmb");
        assert_eq!(decode_string(b"F1028_1.ithmb", 1), "F1028_1.ithmb");
        // UTF-16LE (enc 2) — the encoding libgpod writes bank names in.
        let utf16: Vec<u8> = "F1028_1.ithmb"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_string(&utf16, 2), "F1028_1.ithmb");
    }

    #[test]
    fn verdict_classification_table() {
        // Unknown model gates everything (declared count still reported).
        let (verdict, diag) = classify(false, Some(3356), 0, None);
        assert_eq!(verdict, ArtworkVerdict::SkippedUnknownModel);
        assert!(diag.is_some());

        // ArtworkDB missing.
        let (verdict, _) = classify(true, None, 0, None);
        assert_eq!(verdict, ArtworkVerdict::Missing);

        // Legitimately artless: 0 declared, 0 parsed.
        let (verdict, diag) = classify(true, Some(0), 0, None);
        assert_eq!(verdict, ArtworkVerdict::Ok);
        assert!(diag.is_none());

        // Healthy device: declared == parsed.
        let (verdict, diag) = classify(true, Some(3356), 3356, None);
        assert_eq!(verdict, ArtworkVerdict::Ok);
        assert!(diag.is_none());

        // Declared but not parsed.
        let (verdict, diag) = classify(true, Some(3356), 10, None);
        assert_eq!(verdict, ArtworkVerdict::Missing);
        assert!(diag.unwrap().contains("10"));

        // Proof-read failure is Missing even when counts match.
        let failure = ProofReadFailure {
            format_id: 1028,
            reason: "cannot read".to_string(),
        };
        let (verdict, diag) = classify(true, Some(1), 1, Some(&failure));
        assert_eq!(verdict, ArtworkVerdict::Missing);
        assert!(diag.unwrap().contains("1028"));
    }
}
