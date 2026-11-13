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

/// The track facts smart rules evaluate against — the libgpod
/// `itdb_splr_eval` field mapping (itdb_playlist.c), expressed as owned
/// values. `None` fields are unknown on the evaluating side and make any
/// rule that needs them `Undecided` instead of guessing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackFacts {
    /// **Effective** rating 0–100 — the caller applies the album
    /// fill-down, because iTunes evaluates the effective value (the raw
    /// `mith` byte is not what the criteria see).
    pub rating: u8,
    pub play_count: u32,
    pub skip_count: u32,
    /// Apple-epoch local wall-clock seconds; 0 = unset.
    pub date_added: u32,
    /// Apple-epoch local wall-clock seconds; 0 = never played.
    pub last_played: u32,
    /// `mhit` mediatype bits — the value `VideoKind` rules binary-AND
    /// (libgpod maps `ITDB_SPLFIELD_VIDEO_KIND` to `track->mediatype`).
    /// `None` where the evaluating side has no verified source (the host
    /// `.itl` mediatype offset is not yet verified — b008 plan §7).
    pub media_kind_bits: Option<u32>,
    /// The 0x85 purchase/iCloud status bits (hypothesis: bit 0 =
    /// downloaded). `None` = unknown.
    pub purchase_status: Option<u32>,
    /// "Now" in the same Apple-epoch local wall-clock space, for
    /// in-the-last rules. Caller-supplied on purpose: the evaluator
    /// never reads the clock, so tests and dry runs stay deterministic.
    pub now: u32,
}

/// Three-valued rule match: an unknown fact or an unknown rule is
/// `Undecided`, never silently `False`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleMatch {
    True,
    False,
    Undecided,
}

/// Evaluate a rule set against one track's facts.
///
/// Group semantics mirror the corpus and libgpod: match operator 0 =
/// AND, 1 = OR. Leaf semantics mirror `itdb_splr_eval` plus the corpus
/// findings: `VideoKind` `Is` behaves as a bit test (the observed
/// flattening `Is 1 OR Is 0x20` → `BinaryAnd 0x21` requires it), and
/// rating/date values follow iTunes' boundary-minus-1 encoding.
pub fn eval_rules(rules: &SmartRules, facts: &TrackFacts) -> RuleMatch {
    let results: Vec<RuleMatch> = rules.rules.iter().map(|r| eval_rule(r, facts)).collect();
    match rules.match_operator {
        0 => {
            if results.contains(&RuleMatch::False) {
                RuleMatch::False
            } else if results.contains(&RuleMatch::Undecided) {
                RuleMatch::Undecided
            } else {
                RuleMatch::True
            }
        }
        1 => {
            if results.contains(&RuleMatch::True) {
                RuleMatch::True
            } else if results.contains(&RuleMatch::Undecided) {
                RuleMatch::Undecided
            } else {
                RuleMatch::False
            }
        }
        // Unknown operators are not guessed: agree only when every leaf
        // agrees, otherwise stay undecided.
        _ => {
            let first = results.first().copied().unwrap_or(RuleMatch::Undecided);
            if results.iter().all(|r| *r == first) {
                first
            } else {
                RuleMatch::Undecided
            }
        }
    }
}

fn eval_rule(rule: &SmartRule, facts: &TrackFacts) -> RuleMatch {
    match rule {
        SmartRule::Group(group) => eval_rules(group, facts),
        SmartRule::Leaf(leaf) => eval_leaf(leaf, facts),
    }
}

/// Leaf evaluation over the observed and libgpod-known fields. Everything
/// else — string rules, playlist rules, unknown fields — is `Undecided`.
fn eval_leaf(leaf: &SmartLeaf, facts: &TrackFacts) -> RuleMatch {
    match leaf.field {
        0x19 => int_cmp(i64::from(facts.rating), leaf),
        0x16 => int_cmp(i64::from(facts.play_count), leaf),
        0x44 => int_cmp(i64::from(facts.skip_count), leaf),
        0x3C => match leaf.action {
            // `Is` on VideoKind is a bit test, not equality — the
            // flattening evidence (OR of single-bit `Is` rules becomes
            // one `BinaryAnd` mask) requires it.
            0x0000_0001 | 0x0000_0400 => bits(facts.media_kind_bits, leaf.from_value, false),
            0x0200_0400 => bits(facts.media_kind_bits, leaf.from_value, true),
            _ => RuleMatch::Undecided,
        },
        0x85 => match leaf.action {
            0x0000_0400 => bits(facts.purchase_status, leaf.from_value, false),
            _ => RuleMatch::Undecided,
        },
        0x10 => date_cmp(i64::from(facts.date_added), leaf, facts),
        0x17 => date_cmp(i64::from(facts.last_played), leaf, facts),
        _ => RuleMatch::Undecided,
    }
}

fn int_cmp(value: i64, leaf: &SmartLeaf) -> RuleMatch {
    match leaf.action {
        0x0000_0001 => (value == leaf.from_value).into(),
        0x0200_0001 => (value != leaf.from_value).into(),
        0x0000_0010 => (value > leaf.from_value).into(),
        0x0000_0040 => (value < leaf.from_value).into(),
        _ => RuleMatch::Undecided,
    }
}

fn date_cmp(value: i64, leaf: &SmartLeaf, facts: &TrackFacts) -> RuleMatch {
    match leaf.action {
        0x0000_0001 => (value == leaf.from_value).into(),
        0x0200_0001 => (value != leaf.from_value).into(),
        // libgpod: date > now + fromdate * fromunits (fromdate = -1,
        // fromunits = seconds → "in the last N seconds").
        0x0000_0200 => {
            (value > i64::from(facts.now) + leaf.from_date.saturating_mul(leaf.from_units)).into()
        }
        _ => RuleMatch::Undecided,
    }
}

fn bits(known: Option<u32>, mask: i64, negate: bool) -> RuleMatch {
    let Some(bits) = known else {
        return RuleMatch::Undecided;
    };
    let hit = i64::from(bits) & mask != 0;
    (if negate { !hit } else { hit }).into()
}

impl From<bool> for RuleMatch {
    fn from(value: bool) -> Self {
        if value {
            RuleMatch::True
        } else {
            RuleMatch::False
        }
    }
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

    // ------------------------------------------------------------------
    // Evaluation
    // ------------------------------------------------------------------

    fn facts(rating: u8, play_count: u32, date_added: u32) -> TrackFacts {
        TrackFacts {
            rating,
            play_count,
            date_added,
            media_kind_bits: Some(0x21),
            purchase_status: Some(1),
            now: 1_000_000,
            ..Default::default()
        }
    }

    fn flat(field: u32, action: u32, values: [i64; 6]) -> SmartRules {
        parse_smart_rules(&build_slst(
            0x0001_0001,
            1,
            0,
            &[build_leaf(field, action, values)],
        ))
        .unwrap()
    }

    #[test]
    fn evaluates_rating_with_boundary_minus_one() {
        let rules = flat(0x19, 0x10, [69, 0, 1, 69, 0, 1]);
        assert_eq!(eval_rules(&rules, &facts(100, 0, 0)), RuleMatch::True);
        assert_eq!(eval_rules(&rules, &facts(80, 0, 0)), RuleMatch::True);
        assert_eq!(eval_rules(&rules, &facts(70, 0, 0)), RuleMatch::True);
        assert_eq!(eval_rules(&rules, &facts(60, 0, 0)), RuleMatch::False);
        assert_eq!(eval_rules(&rules, &facts(0, 0, 0)), RuleMatch::False);
    }

    #[test]
    fn evaluates_playcount_greater_than() {
        let rules = flat(0x16, 0x10, [2, 0, 1, 2, 0, 1]);
        assert_eq!(eval_rules(&rules, &facts(0, 3, 0)), RuleMatch::True);
        assert_eq!(eval_rules(&rules, &facts(0, 2, 0)), RuleMatch::False);
    }

    #[test]
    fn evaluates_date_added_in_the_last() {
        // fromdate = -1, fromunits = 604800 → in the last 7 days.
        let rules = flat(0x10, 0x200, [0x2DAE2DAE2DAE2DAE, -1, 604800, 0, 0, 0]);
        let inside = TrackFacts {
            date_added: 1_000_000 - 3 * 86_400,
            ..facts(0, 0, 0)
        };
        let outside = TrackFacts {
            date_added: 1_000_000 - 8 * 86_400,
            ..facts(0, 0, 0)
        };
        assert_eq!(eval_rules(&rules, &inside), RuleMatch::True);
        assert_eq!(eval_rules(&rules, &outside), RuleMatch::False);
    }

    #[test]
    fn evaluates_video_kind_bits_and_unknown_facts() {
        let mask = flat(0x3C, 0x400, [0x21, 0, 1, 0x21, 0, 1]);
        assert_eq!(eval_rules(&mask, &facts(0, 0, 0)), RuleMatch::True);
        let without_bits = TrackFacts {
            media_kind_bits: None,
            ..facts(0, 0, 0)
        };
        assert_eq!(eval_rules(&mask, &without_bits), RuleMatch::Undecided);
        let no_bits = TrackFacts {
            media_kind_bits: Some(0x40),
            ..facts(0, 0, 0)
        };
        assert_eq!(eval_rules(&mask, &no_bits), RuleMatch::False);
    }

    #[test]
    fn evaluates_groups_with_and_or_and_undecided_propagation() {
        // OR(group of two VideoKind Is rules) reproduces the corpus form.
        let inner = build_slst(
            0x0001_0001,
            2,
            1,
            &[
                build_leaf(0x3C, 0x1, [1, 0, 1, 1, 0, 1]),
                build_leaf(0x3C, 0x1, [0x20, 0, 1, 0x20, 0, 1]),
            ],
        );
        let outer = build_slst(
            0x0001_0001,
            2,
            0,
            &[
                build_group(&inner),
                build_leaf(0x19, 0x10, [69, 0, 1, 69, 0, 1]),
            ],
        );
        let rules = parse_smart_rules(&outer).unwrap();
        assert_eq!(eval_rules(&rules, &facts(100, 0, 0)), RuleMatch::True);
        assert_eq!(eval_rules(&rules, &facts(60, 0, 0)), RuleMatch::False);
        // Unknown media bits → the group is undecided, AND with a true
        // rating rule stays undecided (never silently false).
        let unknown_bits = TrackFacts {
            media_kind_bits: None,
            ..facts(100, 0, 0)
        };
        assert_eq!(eval_rules(&rules, &unknown_bits), RuleMatch::Undecided);
    }

    #[test]
    fn purchase_status_and_unknown_fields_are_undecided_not_false() {
        let purchase = flat(0x85, 0x400, [1, 0, 1, 1, 0, 1]);
        assert_eq!(eval_rules(&purchase, &facts(0, 0, 0)), RuleMatch::True);
        let unknown_status = TrackFacts {
            purchase_status: None,
            ..facts(0, 0, 0)
        };
        assert_eq!(eval_rules(&purchase, &unknown_status), RuleMatch::Undecided);
        let unknown_field = flat(0xDEAD, 0x1, [0; 6]);
        assert_eq!(
            eval_rules(&unknown_field, &facts(0, 0, 0)),
            RuleMatch::Undecided
        );
    }
}
