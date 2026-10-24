//! Native repository adapters for the shared overlay authority checks.
use crate::{oplog, repo::Repo};
use anyhow::{Context, Result};
pub use layr_model::authority::{digest, from_lines, protected, Authority, Change};
use std::collections::{BTreeMap, BTreeSet};

pub fn of(repo: &Repo, name: &str) -> Result<Authority> {
    let lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    from_lines(&repo.p.id, &lines, &crate::store_remote::overlays::trusted_keys(repo)?)?
        .remove(name)
        .context("overlay has no creator authority")
}

pub fn validate_import(project: &crate::store::Project, incoming: &[String]) -> Result<()> {
    let Some((parent, name)) = project.name.split_once('%') else { return Ok(()) };
    let repo = Repo { p: crate::store::Project::open(project.store, parent)? };
    let authority = of(&repo, name)?;
    let mut lines: BTreeSet<String> = project.export_jsonl()?.lines().map(str::to_owned).collect();
    lines.extend(incoming.iter().cloned());
    let id = if project.id.is_empty() {
        incoming
            .first()
            .map(|line| oplog::parse_line(line, None).map(|signed| signed.record.project))
            .transpose()?
            .unwrap_or_default()
    } else {
        project.id.clone()
    };
    authority.validate(&lines.into_iter().collect::<Vec<_>>(), &id, &crate::store_remote::overlays::trusted_keys(&repo)?)
}

pub fn retained(authority: &Authority, repo: &Repo) -> Result<BTreeMap<String, String>> {
    let lines = repo.p.export_jsonl()?.lines().map(str::to_owned).collect::<Vec<_>>();
    layr_model::authority::retained(authority, &lines)
}
