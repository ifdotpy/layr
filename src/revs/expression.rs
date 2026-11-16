//! Revision syntax, independent of catalog lookup and ancestry traversal.
use anyhow::{bail, Context, Result};
use std::num::NonZeroUsize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSelector<'a> {
    Head,
    Park,
    Change {
        prefix: &'a str,
        version: Option<usize>,
        spelling: &'a str,
    },
    View(layr_types::ViewId),
    Attestation(layr_types::RecordId),
    /// A catalog name whose line/tag/ref/id interpretation depends on lookup.
    Name(&'a str),
    Position {
        name: &'a str,
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestorStep {
    FirstParent(usize),
    Parent(NonZeroUsize),
    /// `^0`, including its existing zero-padded spellings.
    Identity,
}

#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub struct RevisionExpr<'a> {
    pub(super) base: BaseSelector<'a>,
    pub(super) ancestors: Vec<AncestorStep>,
}

impl<'a> RevisionExpr<'a> {
    pub fn parse(rev: &'a str) -> Result<Self> {
        super::reject_old(rev)?;
        // Literal braces can occur in Git ref names. Preserve their old suffix
        // shielding without recursion, an unbounded stack, or unchecked depth.
        let mut depth = 0isize;
        let mut split = rev.len();
        for (index, byte) in rev.bytes().enumerate() {
            match byte {
                b'{' => depth = depth.checked_add(1).with_context(|| format!("bad revision {rev}"))?,
                b'}' => depth = depth.checked_sub(1).with_context(|| format!("bad revision {rev}"))?,
                b'~' | b'^' if depth == 0 => {
                    split = index;
                    break;
                }
                _ => {}
            }
        }
        let (base, mut suffix) = rev.split_at(split);
        let base = match base.find("@{") {
            Some(index) if base.ends_with('}') => BaseSelector::Position {
                name: &base[..index],
                index: base[index + 2..base.len() - 1].parse().map_err(|_| anyhow::anyhow!("bad revision {rev}"))?,
            },
            _ => match base {
                "" | "head" | "@" => BaseSelector::Head,
                "park" => BaseSelector::Park,
                _ if base.starts_with("changes/") => {
                    let (prefix, version) = match base[8..].split_once('/') {
                        Some((prefix, version)) => (prefix, Some(super::change::version(version)?)),
                        None => (&base[8..], None),
                    };
                    BaseSelector::Change { prefix, version, spelling: base }
                }
                _ if base.starts_with("views/") => BaseSelector::View(
                    layr_types::ViewId::parse(&base[6..]).map_err(|_| anyhow::anyhow!("invalid view id '{}'", &base[6..]))?,
                ),
                _ if base.starts_with("attestations/") => {
                    let id = &base[13..];
                    if !crate::model::is_uuid(id) {
                        bail!("unknown attestation '{id}'");
                    }
                    BaseSelector::Attestation(layr_types::RecordId::parse(id)?)
                }
                _ => BaseSelector::Name(base),
            },
        };
        let mut ancestors = Vec::new();
        while !suffix.is_empty() {
            let operator = suffix.as_bytes()[0];
            if !matches!(operator, b'~' | b'^') {
                bail!("bad revision {rev}");
            }
            // The checked operator is one ASCII byte; the digit prefix likewise
            // ends on a UTF-8 boundary. No input-controlled byte slicing occurs.
            let tail = &suffix[1..];
            let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
            let (number, rest) = tail.split_at(digits);
            let count: usize = if number.is_empty() { 1 } else { number.parse()? };
            ancestors.push(match operator {
                b'~' => AncestorStep::FirstParent(count),
                _ => match NonZeroUsize::new(count) {
                    Some(parent) => AncestorStep::Parent(parent),
                    None => AncestorStep::Identity,
                },
            });
            suffix = rest;
        }
        Ok(Self { base, ancestors })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bases_positions_and_ordered_ancestor_steps() {
        let expr = RevisionExpr::parse("origin/topic@{+01}~2^^0~0^3").unwrap();
        assert_eq!(expr.base, BaseSelector::Position { name: "origin/topic", index: 1 });
        assert_eq!(
            expr.ancestors,
            vec![
                AncestorStep::FirstParent(2),
                AncestorStep::Parent(NonZeroUsize::new(1).unwrap()),
                AncestorStep::Identity,
                AncestorStep::FirstParent(0),
                AncestorStep::Parent(NonZeroUsize::new(3).unwrap())
            ]
        );
        for name in ["feature@laptop", "origin/{nested{ref}}", "origin/unbalanced}"] {
            assert_eq!(RevisionExpr::parse(name).unwrap().base, BaseSelector::Name(name));
        }
        for name in ["", "head", "@"] {
            assert_eq!(RevisionExpr::parse(name).unwrap().base, BaseSelector::Head);
        }
        assert_eq!(RevisionExpr::parse("park").unwrap().base, BaseSelector::Park);
        assert_eq!(
            RevisionExpr::parse("changes/name/v02").unwrap().base,
            BaseSelector::Change { prefix: "name", version: Some(1), spelling: "changes/name/v02" }
        );
        assert!(matches!(RevisionExpr::parse(&format!("views/{}", "a".repeat(64))).unwrap().base, BaseSelector::View(_)));
        assert!(matches!(
            RevisionExpr::parse("attestations/01a12290-0000-7000-8000-000000000001").unwrap().base,
            BaseSelector::Attestation(_)
        ));
    }

    #[test]
    fn numeric_bounds_and_errors_match_existing_syntax() {
        let max = usize::MAX.to_string();
        assert_eq!(RevisionExpr::parse(&format!("head~{max}")).unwrap().ancestors, vec![AncestorStep::FirstParent(usize::MAX)]);
        let overflow = format!("{max}0");
        assert_eq!(
            RevisionExpr::parse(&format!("head^{overflow}")).unwrap_err().to_string(),
            overflow.parse::<usize>().unwrap_err().to_string()
        );
        for rev in [format!("head@{{{overflow}}}"), "head@{nope}".into(), "head~x".into(), "head^+1".into(), "head^1é".into()] {
            assert_eq!(RevisionExpr::parse(&rev).unwrap_err().to_string(), format!("bad revision {rev}"));
        }
    }

    #[test]
    fn malformed_nesting_is_iterative_and_deprecated_tokens_stay_friendly() {
        let rev = format!("head@{}1{}", "{".repeat(10_000), "}".repeat(10_000));
        assert_eq!(RevisionExpr::parse(&rev).unwrap_err().to_string(), format!("bad revision {rev}"));
        assert_eq!(RevisionExpr::parse("HEAD~1").unwrap_err().to_string(), "HEAD was removed. Use head.");
        assert_eq!(RevisionExpr::parse("^stash@{0}").unwrap_err().to_string(), "stash revisions were removed. Use park@{n}.");
    }
}
