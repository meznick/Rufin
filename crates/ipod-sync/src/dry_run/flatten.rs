//! B-009: `SLst` flattening normalization (`deltas/b009_plan.md` §4).
//!
//! iTunes flattens nested smart-rule groups when it writes the device:
//! the host `.itl` stores nested `SLst` trees, the device iTunesDB carries
//! the flat form, and libgpod 0.8.3 mis-evaluates nested blobs (B-008 §7).
//! Comparing host criteria against device criteria therefore needs the
//! exact observed mapping, reproduced here:
//!
//! 1. **Singleton groups unwrap.** iTunes wraps every UI criterion in a
//!    group (one leaf, any operator); the device carries the leaf directly.
//!    Observed on «Хай рейт» / «Последние добавленные» / «Самые популярные».
//! 2. **OR of `Is` leaves on one field merges into one `BinaryAnd` leaf.**
//!    Observed: the media-kind group `(video kind Is 1) OR (video kind
//!    Is 0x20)` becomes `video kind BinaryAnd 0x21` (33) — the OR of the
//!    from-values; from/to dates and units are copied from the first leaf.
//!    Verified byte-exact against the device blobs of the three surfaced
//!    smart playlists on the snapshot pair.
//!
//! Anything else (multi-leaf AND groups, OR groups of non-`Is` leaves,
//! deeper nesting) has no observed device form and yields
//! [`FlattenError::Unflattenable`] — the caller reports it as a diagnostic,
//! never as a silent criteria match.

use crate::mapping::smart::{SmartLeaf, SmartRule, SmartRules};

/// Why a rule set cannot be normalized to the observed device form.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FlattenError {
    /// A group node has no observed flattened equivalent (multi-leaf AND,
    /// OR of non-`Is` leaves, nested non-singleton groups).
    #[error("smart-rule group has no observed device form")]
    Unflattenable,
}

/// Action constants (libgpod `itdb.h`, corpus-verified).
const ACTION_IS: u32 = 0x0000_0001;
const ACTION_BINARY_AND: u32 = 0x0000_0400;

/// Normalize host rules to the flat form iTunes writes to the device.
///
/// Leaves pass through; groups are unwrapped (singleton) or merged
/// (OR-of-`Is`, see the module docs). The result has no [`SmartRule::Group`]
/// nodes on success.
pub fn flatten(rules: &SmartRules) -> Result<SmartRules, FlattenError> {
    let mut out = SmartRules {
        unk004: rules.unk004,
        match_operator: rules.match_operator,
        rules: Vec::with_capacity(rules.rules.len()),
    };
    for rule in &rules.rules {
        out.rules.push(match rule {
            SmartRule::Leaf(leaf) => SmartRule::Leaf(leaf.clone()),
            SmartRule::Group(group) => flatten_group(group)?,
        });
    }
    Ok(out)
}

fn flatten_group(group: &SmartRules) -> Result<SmartRule, FlattenError> {
    if group.rules.len() == 1 {
        // Singleton criterion group: unwrap recursively — the device
        // carries the leaf directly.
        return match &group.rules[0] {
            SmartRule::Leaf(leaf) => Ok(SmartRule::Leaf(leaf.clone())),
            SmartRule::Group(inner) => flatten_group(inner),
        };
    }
    if group.match_operator == 1 {
        // OR of `Is` leaves on one field → one `BinaryAnd` leaf with the
        // OR-ed mask (the media-kind evidence: Is 1 OR Is 0x20 → 0x21).
        let leaves: Vec<&SmartLeaf> = group
            .rules
            .iter()
            .map(|rule| match rule {
                SmartRule::Leaf(leaf) => Some(leaf),
                SmartRule::Group(_) => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(FlattenError::Unflattenable)?;
        let first = &leaves[0];
        let same_field = leaves.iter().all(|leaf| leaf.field == first.field);
        let is_is = leaves
            .iter()
            .all(|leaf| leaf.action == ACTION_IS && leaf.from_value == leaf.to_value);
        if same_field && is_is {
            let mask = leaves.iter().fold(0i64, |acc, leaf| acc | leaf.from_value);
            return Ok(SmartRule::Leaf(SmartLeaf {
                field: first.field,
                action: ACTION_BINARY_AND,
                from_value: mask,
                from_date: first.from_date,
                from_units: first.from_units,
                to_value: mask,
                to_date: first.to_date,
                to_units: first.to_units,
            }));
        }
    }
    Err(FlattenError::Unflattenable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(field: u32, action: u32, value: i64) -> SmartLeaf {
        SmartLeaf {
            field,
            action,
            from_value: value,
            from_date: 0,
            from_units: 1,
            to_value: value,
            to_date: 0,
            to_units: 1,
        }
    }

    fn group(operator: u32, rules: Vec<SmartRule>) -> SmartRules {
        SmartRules {
            unk004: 0x0001_0001,
            match_operator: operator,
            rules,
        }
    }

    #[test]
    fn singleton_groups_unwrap() {
        // «Хай рейт»[1]: a one-leaf AND group (rating > 69) → the leaf.
        let host = group(
            0,
            vec![
                SmartRule::Group(group(
                    1,
                    vec![
                        SmartRule::Leaf(leaf(0x3C, ACTION_IS, 1)),
                        SmartRule::Leaf(leaf(0x3C, ACTION_IS, 0x20)),
                    ],
                )),
                SmartRule::Group(group(0, vec![SmartRule::Leaf(leaf(0x19, 0x10, 69))])),
            ],
        );
        let flat = flatten(&host).unwrap();
        assert_eq!(flat.rules.len(), 2);
        assert!(flat.rules.iter().all(|r| matches!(r, SmartRule::Leaf(_))));
        let SmartRule::Leaf(second) = &flat.rules[1] else {
            panic!("expected leaf");
        };
        assert_eq!(
            (second.field, second.action, second.from_value),
            (0x19, 0x10, 69)
        );
    }

    #[test]
    fn or_of_is_leaves_merges_to_binary_and() {
        // The media-kind group: (video kind Is 1) OR (video kind Is 0x20)
        // → video kind BinaryAnd 0x21 (33) — the device blob form.
        let host = group(
            0,
            vec![SmartRule::Group(group(
                1,
                vec![
                    SmartRule::Leaf(leaf(0x3C, ACTION_IS, 1)),
                    SmartRule::Leaf(leaf(0x3C, ACTION_IS, 0x20)),
                ],
            ))],
        );
        let flat = flatten(&host).unwrap();
        let SmartRule::Leaf(merged) = &flat.rules[0] else {
            panic!("expected leaf");
        };
        assert_eq!(
            (
                merged.field,
                merged.action,
                merged.from_value,
                merged.to_value,
                merged.from_units
            ),
            (0x3C, ACTION_BINARY_AND, 0x21, 0x21, 1)
        );
    }

    #[test]
    fn already_flat_rules_pass_through() {
        // «Музыка» / «Аудиокниги» / … host form: flat leaves already.
        let host = group(
            0,
            vec![
                SmartRule::Leaf(leaf(0x3C, ACTION_BINARY_AND, 0x102001)),
                SmartRule::Leaf(leaf(0x3C, 0x0200_0400, 0x208004)),
            ],
        );
        assert_eq!(flatten(&host).unwrap(), host);
    }

    #[test]
    fn unobserved_group_shapes_are_unflattenable() {
        // Multi-leaf AND group — no observed device form.
        let and_two = group(
            0,
            vec![SmartRule::Group(group(
                0,
                vec![
                    SmartRule::Leaf(leaf(0x19, 0x10, 69)),
                    SmartRule::Leaf(leaf(0x16, 0x10, 2)),
                ],
            ))],
        );
        assert_eq!(flatten(&and_two), Err(FlattenError::Unflattenable));

        // OR of GreaterThan leaves — not the observed Is pattern.
        let or_gt = group(
            0,
            vec![SmartRule::Group(group(
                1,
                vec![
                    SmartRule::Leaf(leaf(0x19, 0x10, 69)),
                    SmartRule::Leaf(leaf(0x16, 0x10, 2)),
                ],
            ))],
        );
        assert_eq!(flatten(&or_gt), Err(FlattenError::Unflattenable));
    }
}
