//! B-010: the pure `Play Counts` reader (b010_plan §6/§7).
//!
//! The pending-delta store on the pod: an `mhdp` file whose entries carry
//! **no dbid** — they align to the `mhlt` track order (measured in the R1
//! inventory, `deltas/b001_research.md`). The caller supplies the ordered
//! dbid list and pairs the entries up via [`align_pending`]. Entry layout
//! by `entry_length`: playcount @+0, time_played @+4 (Mac epoch), bookmark
//! @+8, rating @+12 (`0xFFFFFFFF` = none, field exists from ≥0x10),
//! unknown u32 @+16 (≥0x14), skipcount @+20 and last_skipped @+24 (≥0x1c).
//!
//! Both `Play Counts` and `Play Counts.bak` are read; the journal
//! watermarks dedupe entries our own interrupted sync already imported.

use std::collections::HashMap;

use thiserror::Error;

/// One pending entry in file order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendingEntry {
    pub playcount: u32,
    /// Mac-epoch local wall-clock seconds; 0 = absent.
    pub time_played: u32,
    pub bookmark_time: u32,
    /// `NO_RATING` = the entry carries no rating.
    pub rating: u32,
    pub skipcount: u32,
    /// Mac-epoch; 0 = absent.
    pub last_skipped: u32,
}

/// The "no rating" sentinel of the pending-store format.
pub const NO_RATING: u32 = 0xFFFF_FFFF;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ImportError {
    #[error("not a Play Counts file (missing mhdp header)")]
    NotPlayCounts,
    #[error("Play Counts header too short ({0} bytes)")]
    HeaderTooShort(usize),
    #[error("Play Counts entry length {0} is smaller than 12")]
    EntryTooShort(u32),
    #[error("Play Counts declares {declared} entries but only {available} fit in the file")]
    Truncated { declared: u32, available: usize },
}

/// Parse a raw `Play Counts`/`Play Counts.bak` file.
pub fn parse_play_counts(bytes: &[u8]) -> Result<Vec<PendingEntry>, ImportError> {
    if bytes.len() < 4 || &bytes[..4] != b"mhdp" {
        return Err(ImportError::NotPlayCounts);
    }
    if bytes.len() < 16 {
        return Err(ImportError::HeaderTooShort(bytes.len()));
    }
    let u32_at = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    let header_length = u32_at(4) as usize;
    if header_length < 0x60 {
        return Err(ImportError::HeaderTooShort(header_length));
    }
    let entry_length = u32_at(8) as usize;
    if entry_length < 0x0c {
        return Err(ImportError::EntryTooShort(entry_length as u32));
    }
    let entry_num = u32_at(12) as usize;
    let available = bytes.len().saturating_sub(header_length) / entry_length;
    if entry_num > available {
        return Err(ImportError::Truncated {
            declared: entry_num as u32,
            available,
        });
    }
    let mut entries = Vec::with_capacity(entry_num);
    for index in 0..entry_num {
        let seek = header_length + index * entry_length;
        entries.push(PendingEntry {
            playcount: u32_at(seek),
            time_played: u32_at(seek + 4),
            bookmark_time: u32_at(seek + 8),
            rating: if entry_length >= 0x10 {
                u32_at(seek + 12)
            } else {
                NO_RATING
            },
            skipcount: if entry_length >= 0x1c {
                u32_at(seek + 20)
            } else {
                0
            },
            last_skipped: if entry_length >= 0x1c {
                u32_at(seek + 24)
            } else {
                0
            },
        });
    }
    Ok(entries)
}

/// Per-dbid pending totals the merge consumes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PendingTotals {
    pub plays: u32,
    pub skips: u32,
    /// Newest pending play, Mac-epoch local wall-clock.
    pub played_at_mac: Option<u32>,
    /// Newest pending skip, Mac-epoch local wall-clock.
    pub skipped_at_mac: Option<u32>,
    /// Entries carrying a rating — ignored on Video/Classic (diagnostic).
    pub rating_entries: u32,
}

/// Sum the pending entries per dbid. `ordered_dbids` = the session's track
/// order (parse builds the mhlt in exactly the file's index order).
/// Entries beyond the track list cannot be attributed and are reported —
/// the raw file is journaled/backed up, so nothing is silently dropped.
pub fn align_pending(
    entries: &[PendingEntry],
    ordered_dbids: &[u64],
) -> (HashMap<u64, PendingTotals>, Vec<String>) {
    let mut totals: HashMap<u64, PendingTotals> = HashMap::new();
    let mut diagnostics = Vec::new();
    if entries.len() > ordered_dbids.len() {
        diagnostics.push(format!(
            "Play Counts carries {} entries for {} tracks; the {} un-attributable tail entries are preserved in the journal, not applied",
            entries.len(),
            ordered_dbids.len(),
            entries.len() - ordered_dbids.len()
        ));
    }
    for (index, entry) in entries.iter().enumerate() {
        let Some(dbid) = ordered_dbids.get(index) else {
            break;
        };
        let total = totals.entry(*dbid).or_default();
        total.plays = total.plays.saturating_add(entry.playcount);
        total.skips = total.skips.saturating_add(entry.skipcount);
        if entry.time_played != 0 {
            total.played_at_mac = Some(
                total
                    .played_at_mac
                    .map_or(entry.time_played, |t| t.max(entry.time_played)),
            );
        }
        if entry.last_skipped != 0 {
            total.skipped_at_mac = Some(
                total
                    .skipped_at_mac
                    .map_or(entry.last_skipped, |t| t.max(entry.last_skipped)),
            );
        }
        if entry.rating != NO_RATING {
            total.rating_entries += 1;
        }
    }
    (totals, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(entry_length: u32, entries: &[&[u32]]) -> Vec<u8> {
        let mut bytes = vec![0_u8; 0x60];
        bytes[0..4].copy_from_slice(b"mhdp");
        bytes[4..8].copy_from_slice(&0x60_u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&entry_length.to_le_bytes());
        bytes[12..16].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for entry in entries {
            let mut raw = vec![0_u8; entry_length as usize];
            for (index, value) in entry.iter().enumerate() {
                raw[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
            bytes.extend_from_slice(&raw);
        }
        bytes
    }

    #[test]
    fn parses_the_full_entry_layout() {
        let bytes = file(0x1c, &[&[3, 0xE7150000, 0, NO_RATING, 0, 1, 0xE7150100]]);
        let entries = parse_play_counts(&bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].playcount, 3);
        assert_eq!(entries[0].time_played, 0xE7150000);
        assert_eq!(entries[0].rating, NO_RATING);
        assert_eq!(entries[0].skipcount, 1);
        assert_eq!(entries[0].last_skipped, 0xE7150100);
    }

    #[test]
    fn shorter_entries_treat_missing_fields_as_absent() {
        // A 0x10 entry has no skip fields.
        let bytes = file(0x10, &[&[5, 1, 2, NO_RATING]]);
        let entries = parse_play_counts(&bytes).unwrap();
        assert_eq!(entries[0].skipcount, 0);
        assert_eq!(entries[0].last_skipped, 0);
        // A 0x0c entry has no rating either.
        let bytes = file(0x0c, &[&[5, 1, 2]]);
        let entries = parse_play_counts(&bytes).unwrap();
        assert_eq!(entries[0].rating, NO_RATING);
    }

    #[test]
    fn rejects_malformed_files() {
        assert_eq!(
            parse_play_counts(b"not it"),
            Err(ImportError::NotPlayCounts)
        );
        let short_header = {
            let mut bytes = vec![0_u8; 16];
            bytes[0..4].copy_from_slice(b"mhdp");
            bytes
        };
        // The header length field is zero → below the 96-byte minimum.
        assert_eq!(
            parse_play_counts(&short_header),
            Err(ImportError::HeaderTooShort(0))
        );
        let small_entry = file(0x08, &[&[1, 2]]);
        assert_eq!(
            parse_play_counts(&small_entry),
            Err(ImportError::EntryTooShort(8))
        );
        // Declares 2 entries but only one fits.
        let mut bytes = file(0x0c, &[&[1, 2, 3]]);
        let declared = 2_u32.to_le_bytes();
        bytes[12..16].copy_from_slice(&declared);
        assert_eq!(
            parse_play_counts(&bytes),
            Err(ImportError::Truncated {
                declared: 2,
                available: 1
            })
        );
    }

    #[test]
    fn aligns_entries_to_the_track_order() {
        let entries = vec![
            PendingEntry {
                playcount: 2,
                time_played: 100,
                skipcount: 1,
                ..Default::default()
            },
            PendingEntry {
                playcount: 3,
                time_played: 200,
                rating: 60,
                ..Default::default()
            },
        ];
        let (totals, diagnostics) = align_pending(&entries, &[0xA1, 0xA2]);
        assert!(diagnostics.is_empty());
        assert_eq!(totals[&0xA1].plays, 2);
        assert_eq!(totals[&0xA1].skips, 1);
        assert_eq!(totals[&0xA1].played_at_mac, Some(100));
        assert_eq!(totals[&0xA2].plays, 3);
        assert_eq!(totals[&0xA2].played_at_mac, Some(200));
        assert_eq!(totals[&0xA2].rating_entries, 1);
    }

    #[test]
    fn tail_entries_are_reported_not_dropped() {
        let entries = vec![PendingEntry::default(), PendingEntry::default()];
        let (totals, diagnostics) = align_pending(&entries, &[0xA1]);
        assert_eq!(totals.len(), 1, "only the attributed entry lands");
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("un-attributable"));
    }
}
