//! Read-only walk of the raw device `iTunesDB` for the verbatim smart-rule
//! blobs — `mhod` type 51 payloads keyed by playlist id (B-009,
//! `deltas/b009_plan.md` §3 gap A).
//!
//! libgpod 0.8.3 parses smart rules into its own flat model and never
//! exposes the original blob, so the byte-exact payload (needed for the
//! host-vs-device criteria comparison) is read straight from the file. The
//! walk is pure `std::fs::read` + offset arithmetic: no libgpod call, no
//! writes, no allocations beyond the returned map.
//!
//! Layout evidence (2026-11-13, restored-device snapshot): the root record
//! is `mhbd` (header length at +4); children are `mhsd` containers whose
//! inner section magic sits at +header length; `mhlp` sections list `mhyp`
//! playlists (header length +4, total length +8, mhod count +12, id u64
//! little-endian at +28); each `mhod` record (total length +8, type +12)
//! carries the 384-byte `SLst` payload at +24 when its type is 51.
//!
//! All `mhlp` sections are walked and the map deduplicates by playlist id:
//! the snapshot carries the same playlist (same id) in several lists, and
//! blobs for playlists libgpod does not surface never join with a parsed
//! playlist — they stay inert map entries. Every offset is bounds-checked;
//! truncated or corrupt data yields an error, never a panic.

use std::collections::HashMap;
use std::path::Path;

use crate::types::IpodError;

/// Read the verbatim smart-rule blobs from an `iTunesDB` file.
///
/// Returns playlist id → raw `SLst` payload (`mhod` type 51 body, i.e. the
/// bytes after the 24-byte `mhod` header).
pub fn read_smart_blobs(itunesdb: &Path) -> Result<HashMap<u64, Vec<u8>>, IpodError> {
    let data = std::fs::read(itunesdb)
        .map_err(|e| IpodError::NotFound(format!("cannot read iTunesDB: {e}")))?;
    let mut out = HashMap::new();
    walk(&data, &mut out)?;
    Ok(out)
}

fn corrupt() -> IpodError {
    IpodError::Internal("corrupt iTunesDB: unexpected end of file".to_string())
}

fn u32_at(data: &[u8], off: usize) -> Option<usize> {
    let raw = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().expect("4 bytes")) as usize)
}

fn u64_at(data: &[u8], off: usize) -> Option<u64> {
    let raw = data.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().expect("8 bytes")))
}

fn walk(data: &[u8], out: &mut HashMap<u64, Vec<u8>>) -> Result<(), IpodError> {
    if data.len() < 16 || &data[0..4] != b"mhbd" {
        return Err(IpodError::Internal(
            "corrupt iTunesDB: missing mhbd header".to_string(),
        ));
    }
    let mut pos = u32_at(data, 4).ok_or_else(corrupt)?;
    while pos.checked_add(16).is_some_and(|end| end <= data.len()) {
        if &data[pos..pos + 4] != b"mhsd" {
            // End of the section chain.
            break;
        }
        let hlen = u32_at(data, pos + 4).ok_or_else(corrupt)?;
        let total = u32_at(data, pos + 8).ok_or_else(corrupt)?;
        let inner = pos.checked_add(hlen).ok_or_else(corrupt)?;
        if data
            .get(inner..inner.checked_add(4).ok_or_else(corrupt)?)
            .is_some_and(|magic| magic == b"mhlp")
        {
            parse_mhlp(data, inner, out)?;
        }
        pos = pos.checked_add(total).ok_or_else(corrupt)?;
    }
    Ok(())
}

fn parse_mhlp(data: &[u8], at: usize, out: &mut HashMap<u64, Vec<u8>>) -> Result<(), IpodError> {
    // `mhlp` layout: magic + header length @+4 + **playlist count** @+8
    // (no total-length field — the count is the iteration bound, verified
    // against the snapshot: 9 and 13 for the two populated lists).
    let hlen = u32_at(data, at + 4).ok_or_else(corrupt)?;
    let count = u32_at(data, at + 8).ok_or_else(corrupt)?;
    let mut cur = at.checked_add(hlen).ok_or_else(corrupt)?;
    for _ in 0..count {
        if cur.checked_add(16).is_none_or(|end| end > data.len()) {
            return Err(corrupt());
        }
        if &data[cur..cur + 4] != b"mhyp" {
            return Err(corrupt());
        }
        let phlen = u32_at(data, cur + 4).ok_or_else(corrupt)?;
        let ptotal = u32_at(data, cur + 8).ok_or_else(corrupt)?;
        let mhod_count = u32_at(data, cur + 12).ok_or_else(corrupt)?;
        let pid = u64_at(data, cur + 28).ok_or_else(corrupt)?;
        let mend = cur.checked_add(ptotal).ok_or_else(corrupt)?;
        if mend > data.len() {
            return Err(corrupt());
        }
        let mut m = cur.checked_add(phlen).ok_or_else(corrupt)?;
        for _ in 0..mhod_count {
            if m.checked_add(24).is_none_or(|end| end > mend) {
                return Err(corrupt());
            }
            if &data[m..m + 4] != b"mhod" {
                return Err(corrupt());
            }
            let mtotal = u32_at(data, m + 8).ok_or_else(corrupt)?;
            let mtype = u32_at(data, m + 12).ok_or_else(corrupt)?;
            if mtype == 51 {
                let payload_end = m.checked_add(mtotal).ok_or_else(corrupt)?;
                if payload_end > mend || payload_end > data.len() {
                    return Err(corrupt());
                }
                let payload = data.get(m + 24..payload_end).ok_or_else(corrupt)?;
                if !payload.is_empty() {
                    out.entry(pid).or_insert_with(|| payload.to_vec());
                }
            }
            m = m.checked_add(mtotal).ok_or_else(corrupt)?;
        }
        cur = mend;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal synthetic tree: one `mhbd` root, one `mhsd` container, one
    /// `mhlp` with one `mhyp` carrying an mhod-51 blob and a name mhod.
    /// `mhod` records have a 24-byte header (magic, header length, total,
    /// type, 8 reserved bytes) and the payload at +24 — the layout the
    /// snapshot shows (string mhods then carry enc/len inside the payload).
    fn synthetic_db(pid: u64, blob: &[u8]) -> Vec<u8> {
        let name = b"\x00\x00";
        let mhod_name = {
            let total = 24 + name.len();
            let mut m = vec![0u8; 24];
            m[0..4].copy_from_slice(b"mhod");
            m[4..8].copy_from_slice(&24u32.to_le_bytes());
            m[8..12].copy_from_slice(&(total as u32).to_le_bytes());
            m[12..16].copy_from_slice(&1u32.to_le_bytes()); // type 1 = title
            m.extend_from_slice(name);
            m
        };
        let mhod_slst = {
            let total = 24 + blob.len();
            let mut m = vec![0u8; 24];
            m[0..4].copy_from_slice(b"mhod");
            m[4..8].copy_from_slice(&24u32.to_le_bytes());
            m[8..12].copy_from_slice(&(total as u32).to_le_bytes());
            m[12..16].copy_from_slice(&51u32.to_le_bytes()); // type 51 = rules
            m.extend_from_slice(blob);
            m
        };
        let mhyp = {
            let hlen = 40;
            let mut p = vec![0u8; hlen];
            p[0..4].copy_from_slice(b"mhyp");
            p[4..8].copy_from_slice(&(hlen as u32).to_le_bytes());
            p[12..16].copy_from_slice(&2u32.to_le_bytes()); // two mhods
            p[28..36].copy_from_slice(&pid.to_le_bytes());
            p.extend_from_slice(&mhod_name);
            p.extend_from_slice(&mhod_slst);
            let total = p.len() as u32;
            p[8..12].copy_from_slice(&total.to_le_bytes());
            p
        };
        let mhlp = {
            let hlen = 16;
            let mut l = vec![0u8; hlen];
            l[0..4].copy_from_slice(b"mhlp");
            l[4..8].copy_from_slice(&(hlen as u32).to_le_bytes());
            l[8..12].copy_from_slice(&1u32.to_le_bytes()); // one mhyp (count @+8)
            l.extend_from_slice(&mhyp);
            l
        };
        let mhsd = {
            let hlen = 16;
            let mut s = Vec::with_capacity(hlen + mhlp.len());
            s.extend_from_slice(b"mhsd");
            s.extend_from_slice(&(hlen as u32).to_le_bytes());
            let total = (hlen + mhlp.len()) as u32;
            s.extend_from_slice(&total.to_le_bytes());
            s.extend_from_slice(&3u32.to_le_bytes()); // section type
            s.extend_from_slice(&mhlp);
            s
        };
        let hlen = 16u32;
        let mut root = Vec::with_capacity(16 + mhsd.len());
        root.extend_from_slice(b"mhbd");
        root.extend_from_slice(&hlen.to_le_bytes());
        root.extend_from_slice(&((16 + mhsd.len()) as u32).to_le_bytes());
        root.extend_from_slice(&1u32.to_le_bytes());
        root.extend_from_slice(&mhsd);
        root
    }

    #[test]
    fn extracts_slst_payload_keyed_by_playlist_id() {
        let blob = b"SLst\x00\x00\x00\x00rest";
        let db = synthetic_db(0xDEAD_BEEF_CAFE, blob);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iTunesDB");
        std::fs::write(&path, &db).unwrap();
        let map = read_smart_blobs(&path).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map[&0xDEAD_BEEF_CAFE], blob);
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let err = read_smart_blobs(std::path::Path::new("/nonexistent/iTunesDB"))
            .expect_err("missing file must error");
        assert!(matches!(err, IpodError::NotFound(_)));
    }

    #[test]
    fn truncated_blob_is_an_error_not_a_panic() {
        let db = synthetic_db(1, b"SLstpayload");
        let truncated = &db[..db.len() - 3];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iTunesDB");
        std::fs::write(&path, truncated).unwrap();
        assert!(read_smart_blobs(&path).is_err());
    }
}
