//! Logical change and version lookup.

use crate::repo::Repo;
use anyhow::{bail, Result};

/// The change that `prefix` names: `#12` (a number), a bare number such as `12`, a full change id, or
/// at least four characters of its displayed id. A bare number that is also the start of a change id is
/// ambiguous.
pub fn find(repo: &Repo, prefix: &str) -> Result<String> {
    let (digits, explicit) = match prefix.strip_prefix('#') {
        Some(n) => (n, true),
        None => (prefix, false),
    };
    let number = (!digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<u64>().ok())
        .flatten()
        .filter(|n| *n > 0 && !digits.starts_with('0'));
    if explicit && number.is_none() {
        bail!("bad change number '{prefix}': use #1, #2, ...");
    }
    let by_number = match number {
        Some(n) => repo.read_model()?.numbering().change(n).map(String::from),
        None => None,
    };
    if explicit {
        return by_number.ok_or_else(|| anyhow::anyhow!("unknown change '{prefix}'"));
    }
    let ids: Vec<String> = repo
        .p
        .db
        .changes()?
        .into_iter()
        .filter(|id| id == prefix || (prefix.len() >= 4 && crate::ids::display(id).starts_with(prefix)))
        .collect();
    match (by_number, ids.as_slice()) {
        (Some(id), []) => Ok(id),
        (Some(id), [other]) if *other == id => Ok(id),
        (Some(_), _) => {
            bail!("'{prefix}' is change #{prefix} and also the start of a change id; use #{prefix} or more of the id")
        }
        (None, [id]) => Ok(id.clone()),
        (None, []) => bail!("unknown change '{prefix}'"),
        (None, _) => bail!("ambiguous change '{prefix}'"),
    }
}

pub fn version(value: &str) -> Result<usize> {
    let n = value
        .strip_prefix('v')
        .ok_or_else(|| anyhow::anyhow!("bad change version '{value}': use v1, v2, ..."))?
        .parse::<usize>()?;
    if n == 0 {
        bail!("change versions start at v1");
    }
    Ok(n - 1)
}
