//! Parked-line history lookup.

use crate::model::Line;
use crate::repo::Repo;
use anyhow::Result;

/// The number of a parked line of `line`, from its name.
pub(crate) fn parked_number(line: &str, name: &str) -> Option<u64> {
    let n = name.strip_prefix("parked.")?.strip_prefix(line)?.strip_prefix('.')?;
    n.chars().all(|c| c.is_ascii_digit()).then(|| n.parse().ok()).flatten()
}

/// The parked lines of `line` on this machine, newest first: `park@{0}` is the first.
pub fn parked_lines(repo: &Repo, line: &Line) -> Result<Vec<Line>> {
    let mut v: Vec<(u64, Line)> = repo
        .p
        .db
        .lines()?
        .into_iter()
        .filter(|l| l.machine == repo.p.store.machine.id)
        .filter_map(|l| parked_number(&line.name, &l.name).map(|n| (n, l)))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(v.into_iter().map(|(_, l)| l).collect())
}
