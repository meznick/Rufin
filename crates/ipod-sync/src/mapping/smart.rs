//! B-008: the `.itl` smart-playlist rules parser (see
//! `deltas/b008_plan.md` §7).
//!
//! The `.itl` `mhoh` 0x65 payload and the device iTunesDB `mhod` type 51
//! carry the same big-endian `SLst` blob. Structure (verified on the
//! iTunes 12.13 reference library, 2026-11-12):
//!
//! - 136-byte header: magic `SLst`, `unk004` (observed 0x00010001),
//!   rule count @8, match operator @12 (0 = AND, 1 = OR).
//! - Rules, each: field @0, action @4, length @52 (the value-block size),
//!   then the value block @56 (6 × BE i64: from/to value/date/units).
//! - **Nesting**: a rule with field 0x00 and action `Is` whose value block
//!   starts with the bytes `SLst` is a group node — its value block is a
//!   complete nested ruleset. iTunes encodes "media kind is (A or B …)" as
//!   a group; when writing the device it flattens the groups into concrete
//!   `VideoKind` bitmask rules (e.g. `Is 1 OR Is 0x20` → `BinaryAnd 0x21`).
//!   libgpod 0.8.3 parses the flat form only and mis-evaluates nested blobs.
//!
//! Known leaf fields/actions (libgpod `itdb.h`, extended by the corpus):
//!
//! | field | meaning |
//! |---|---|
//! | 0x10 | Date added (`InTheLast`, units in `from_units`, seconds) |
//! | 0x16 | Play count (`GreaterThan`) |
//! | 0x19 | Rating (`GreaterThan`; iTunes stores the boundary minus 1) |
//! | 0x3C | Video kind bitmask (`BinaryAnd` / `NotBinaryAnd`) |
//! | 0x85 | Purchase/iCloud status bitmask (bit 0 = downloaded; hypothesis, backlog) |
//! | 0x00 | Group node (nested `SLst` value) |
//!
//! Unknown fields/actions are carried as raw `u32`s — never rejected, never
//! guessed. This module only parses and represents; evaluation and the
//! device-side flattening are later tasks (B-010).

/// The `SLst` blob magic (big-endian header section, ASCII).
pub const SLST_MAGIC: &[u8; 4] = b"SLst";

/// Fixed `SLst` header length: magic + unknown + count + operator and the
/// 120 undocumented bytes to the first rule.
const HEADER_LEN: usize = 136;
/// Fixed rule-header length: field, action, 44 bytes of unknowns, length.
const RULE_HEADER_LEN: usize = 56;
/// The standard leaf value-block length (6 × BE i64 + trailing unknowns).
const LEAF_VALUE_LEN: usize = 68;
/// Nested-rule depth guard: real corpora nest one level; anything deeper is
/// hostile or corrupt.
const MAX_DEPTH: usize = 16;

/// A parsed smart-playlist rule set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartRules {
    /// The header's unknown field (observed 0x00010001).
    pub unk004: u32,
    /// 0 = AND, 1 = OR (the only values observed).
    pub match_operator: u32,
    pub rules: Vec<SmartRule>,
}

/// One rule: a leaf criterion or a nested group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmartRule {
    Leaf(SmartLeaf),
    /// Field 0x00, action `Is`, value = a complete nested `SLst` ruleset.
    Group(SmartRules),
}

/// A concrete criterion (field/action over a value range).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartLeaf {
    pub field: u32,
    pub action: u32,
    pub from_value: i64,
    pub from_date: i64,
    pub from_units: i64,
    pub to_value: i64,
    pub to_date: i64,
    pub to_units: i64,
}

/// Why an `SLst` blob could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SmartRulesError {
    #[error("not an SLst blob at offset {at:#x}")]
    BadMagic { at: usize },
    #[error("SLst header truncated at {at:#x}: need 136 bytes, blob has {have}")]
    HeaderTruncated { at: usize, have: usize },
    #[error("rule count {count} cannot fit in a {blob}-byte blob")]
    RuleCount { count: u32, blob: usize },
    #[error("rule {index} truncated at {at:#x}: need {need} bytes, blob has {have}")]
    RuleTruncated {
        index: usize,
        at: usize,
        need: usize,
        have: usize,
    },
    #[error("group nesting deeper than {MAX_DEPTH} levels")]
    TooDeep,
}

/// Parse a smart-playlist `SLst` blob into owned rules. Pure, allocation is
/// bounded by the blob, recursion is depth-guarded.
pub fn parse_smart_rules(blob: &[u8]) -> Result<SmartRules, SmartRulesError> {
    parse_smart_rules_at(blob, 0, 0)
}

fn parse_smart_rules_at(
    blob: &[u8],
    at: usize,
    depth: usize,
) -> Result<SmartRules, SmartRulesError> {
    if depth > MAX_DEPTH {
        return Err(SmartRulesError::TooDeep);
    }
    if blob.len().saturating_sub(at) < 4 || &blob[at..at + 4] != SLST_MAGIC {
        return Err(SmartRulesError::BadMagic { at });
    }
    if blob.len().saturating_sub(at) < HEADER_LEN {
        return Err(SmartRulesError::HeaderTruncated {
            at,
            have: blob.len().saturating_sub(at),
        });
    }
    let unk004 = read_be_u32(blob, at + 4);
    let count = read_be_u32(blob, at + 8) as usize;
    let match_operator = read_be_u32(blob, at + 12);

    let mut rules =
        Vec::with_capacity(count.min(blob.len() / (RULE_HEADER_LEN + LEAF_VALUE_LEN) + 1));
    let mut cursor = at + HEADER_LEN;
    for index in 0..count {
        let available = blob.len().saturating_sub(cursor);
        if available < RULE_HEADER_LEN {
            return Err(SmartRulesError::RuleTruncated {
                index,
                at: cursor,
                need: RULE_HEADER_LEN,
                have: available,
            });
        }
        let field = read_be_u32(blob, cursor);
        let action = read_be_u32(blob, cursor + 4);
        let value_len = read_be_u32(blob, cursor + 52) as usize;
        let value_at = cursor + RULE_HEADER_LEN;
        let need = RULE_HEADER_LEN + value_len;
        let available = blob.len().saturating_sub(cursor);
        if available < need {
            return Err(SmartRulesError::RuleTruncated {
                index,
                at: cursor,
                need,
                have: available,
            });
        }
        let value = &blob[value_at..value_at + value_len];
        // Group nodes are detected by their value: a complete nested `SLst`
        // blob. The corpus marks them field 0x00 / action `Is`, but the
        // value magic is the authoritative signal — a leaf whose value
        // starts with the magic would be a malformed rule anyway.
        let rule = if value.len() >= 4 && &value[..4] == SLST_MAGIC {
            SmartRule::Group(parse_smart_rules_at(value, 0, depth + 1)?)
        } else {
            SmartRule::Leaf(SmartLeaf {
                field,
                action,
                from_value: read_be_i64(value, 0),
                from_date: read_be_i64(value, 8),
                from_units: read_be_i64(value, 16),
                to_value: read_be_i64(value, 24),
                to_date: read_be_i64(value, 32),
                to_units: read_be_i64(value, 40),
            })
        };
        rules.push(rule);
        cursor += need;
    }
    Ok(SmartRules {
        unk004,
        match_operator,
        rules,
    })
}

fn read_be_u32(blob: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(blob[at..at + 4].try_into().expect("bounds-checked slice"))
}

fn read_be_i64(blob: &[u8], at: usize) -> i64 {
    let available = blob.len().saturating_sub(at).min(8);
    if available == 0 {
        return 0;
    }
    let mut bytes = [0u8; 8];
    bytes[8 - available..].copy_from_slice(&blob[at..at + available]);
    i64::from_be_bytes(bytes)
}

/// Human name for a known smart-rule field.
pub fn smart_field_name(field: u32) -> Option<&'static str> {
    Some(match field {
        0x00 => "Group",
        0x10 => "DateAdded",
        0x16 => "PlayCount",
        0x19 => "Rating",
        0x3C => "VideoKind",
        0x85 => "PurchaseStatus",
        _ => return None,
    })
}

/// Human name for a known smart-rule action.
pub fn smart_action_name(action: u32) -> Option<&'static str> {
    Some(match action {
        0x0000_0001 => "Is",
        0x0000_0010 => "GreaterThan",
        0x0000_0200 => "InTheLast",
        0x0000_0400 => "BinaryAnd",
        0x0200_0400 => "NotBinaryAnd",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a flat leaf rule set with the observed 136-byte header layout.
    fn build_slst(unk: u32, count: u32, operator: u32, rules: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SLST_MAGIC);
        out.extend_from_slice(&unk.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&operator.to_be_bytes());
        out.resize(HEADER_LEN, 0);
        for rule in rules {
            out.extend_from_slice(rule);
        }
        out
    }

    /// Build a leaf rule: field, action, and a full 68-byte value block with
    /// the given from/to triples.
    fn build_leaf(field: u32, action: u32, values: [i64; 6]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&field.to_be_bytes());
        out.extend_from_slice(&action.to_be_bytes());
        out.resize(RULE_HEADER_LEN - 4, 0);
        out.extend_from_slice(&(LEAF_VALUE_LEN as u32).to_be_bytes());
        for v in values {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.resize(RULE_HEADER_LEN + LEAF_VALUE_LEN, 0);
        out
    }

    /// Build a group rule whose value block is a nested `SLst` blob.
    fn build_group(inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_be_bytes()); // field 0x00
        out.extend_from_slice(&1u32.to_be_bytes()); // action Is
        out.resize(RULE_HEADER_LEN - 4, 0);
        out.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        out.extend_from_slice(inner);
        out
    }

    #[test]
    fn parses_flat_rules() {
        let leaf = build_leaf(0x19, 0x10, [69, 0, 1, 69, 0, 1]);
        let blob = build_slst(0x0001_0001, 1, 0, &[leaf]);
        let parsed = parse_smart_rules(&blob).unwrap();
        assert_eq!(parsed.unk004, 0x0001_0001);
        assert_eq!(parsed.match_operator, 0);
        let SmartRule::Leaf(l) = &parsed.rules[0] else {
            panic!("expected leaf");
        };
        assert_eq!(l.field, 0x19);
        assert_eq!(l.action, 0x10);
        assert_eq!(l.from_value, 69);
        assert_eq!(smart_field_name(l.field), Some("Rating"));
        assert_eq!(smart_action_name(l.action), Some("GreaterThan"));
    }

    #[test]
    fn parses_nested_groups_like_the_reference_corpus() {
        // The observed encoding of "Media Kind is (Music OR Videos)":
        // a 0x00/Is rule whose value is an OR group of two VideoKind leaves
        // (Is 1, Is 0x20) — flattened by iTunes to BinaryAnd 0x21 on write.
        let inner = build_slst(
            0x0001_0001,
            2,
            1,
            &[
                build_leaf(0x3C, 0x1, [1, 0, 1, 1, 0, 1]),
                build_leaf(0x3C, 0x1, [0x20, 0, 1, 0x20, 0, 1]),
            ],
        );
        let outer = build_slst(0x0001_0001, 1, 0, &[build_group(&inner)]);
        let parsed = parse_smart_rules(&outer).unwrap();
        let SmartRule::Group(group) = &parsed.rules[0] else {
            panic!("expected group");
        };
        assert_eq!(group.match_operator, 1);
        assert_eq!(group.rules.len(), 2);
        for rule in &group.rules {
            let SmartRule::Leaf(l) = rule else {
                panic!("expected leaf inside the group");
            };
            assert_eq!(l.field, 0x3C);
            assert_eq!(l.action, 0x1);
        }
    }

    #[test]
    fn rejects_bad_magic_short_header_and_truncated_rule() {
        let bad = build_slst(0, 0, 0, &[]);
        assert!(matches!(
            parse_smart_rules(&bad[1..]),
            Err(SmartRulesError::BadMagic { .. })
        ));
        let short = vec![0u8; 100];
        assert!(matches!(
            parse_smart_rules(&short),
            Err(SmartRulesError::BadMagic { .. })
        ));
        let mut header_only = build_slst(0x0001_0001, 1, 0, &[]);
        header_only.truncate(HEADER_LEN + 10);
        assert!(matches!(
            parse_smart_rules(&header_only),
            Err(SmartRulesError::RuleTruncated { .. })
        ));
    }

    #[test]
    fn rejects_impossible_rule_count_and_deep_nesting() {
        let blob = build_slst(0x0001_0001, 1000, 0, &[]);
        assert!(matches!(
            parse_smart_rules(&blob),
            Err(SmartRulesError::RuleTruncated { .. })
        ));
        // A group containing itself, hand-built: 17 levels of nesting must
        // trip the depth guard instead of the stack.
        let mut inner = build_slst(
            0x0001_0001,
            1,
            0,
            &[build_leaf(0x3C, 0x1, [1, 0, 1, 1, 0, 1])],
        );
        for _ in 0..MAX_DEPTH + 1 {
            inner = build_slst(0x0001_0001, 1, 0, &[build_group(&inner)]);
        }
        assert!(matches!(
            parse_smart_rules(&inner),
            Err(SmartRulesError::TooDeep)
        ));
    }

    #[test]
    fn unknown_fields_and_actions_are_carried_not_rejected() {
        let leaf = build_leaf(0xDEAD, 0xBEEF, [0; 6]);
        let blob = build_slst(0x0001_0001, 1, 0, &[leaf]);
        let parsed = parse_smart_rules(&blob).unwrap();
        let SmartRule::Leaf(l) = &parsed.rules[0] else {
            panic!("expected leaf");
        };
        assert_eq!(l.field, 0xDEAD);
        assert_eq!(l.action, 0xBEEF);
        assert_eq!(smart_field_name(l.field), None);
        assert_eq!(smart_action_name(l.action), None);
    }
}
