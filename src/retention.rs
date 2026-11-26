use crate::repo::Repo;
use crate::store::Project;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The project states that a cache overlay names through its heads and records: eviction and
/// `layr migrate --delete-old` keep their trees.
pub(crate) fn overlay_named_states(repo: &Repo) -> Result<BTreeSet<String>> {
    let mut overlays = Vec::new();
    for name in repo.overlay_names()? {
        // An overlay of another platform that this machine never fetched has no project here and names no state.
        if !repo.p.store.project_dir(&format!("{}%{name}", repo.p.name))?.exists() {
            continue;
        }
        overlays.push(repo.overlay_project(&name)?);
    }
    overlay_project_named_states(overlays)
}

/// Defer native retirement if an overlay needs recovery under a busy lock.
pub(crate) fn overlay_named_states_maintenance(repo: &Repo) -> Result<Option<BTreeSet<String>>> {
    repo.maintenance_overlay_projects()?.map(overlay_project_named_states).transpose()
}

fn overlay_project_named_states(overlays: Vec<Project<'_>>) -> Result<BTreeSet<String>> {
    let mut named: BTreeSet<String> = BTreeSet::new();
    for overlay in overlays {
        for line in overlay.db.lines()? {
            named.insert(line.head);
        }
        for raw in overlay.db.all_lines()? {
            for token in raw.split(|c: char| !c.is_ascii_hexdigit()).filter(|t| t.len() == 32) {
                named.insert(token.to_string());
            }
        }
    }
    Ok(named)
}

/// Remove the trees of states whose records stay (eviction, imports that git builds again) and
/// mark them absent. The states are reserved first, in one transaction with the check for pins
/// (`Db::reserve_trees`): a state that a reader pinned stays, and a reader that pins later is
/// refused until the tree is gone. Reservations of a crashed command are finished first. Returns
/// the states whose trees went.
pub(crate) fn drop_trees(repo: &Repo, ids: &[String]) -> Result<Vec<String>> {
    let mut left: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (state, holder) in repo.p.db.drop_rows()? {
        if !crate::free::holder_alive(&holder) {
            left.entry(holder).or_default().push(state);
        }
    }
    for (holder, states) in left {
        let gone = absent_after_delete(repo, &states);
        repo.p.db.release_trees(&states, &gone, &holder)?;
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let holder = crate::free::self_holder();
    let reserved = repo.p.db.reserve_trees(ids, &holder)?;
    let gone = absent_after_delete(repo, &reserved);
    repo.p.db.release_trees(&reserved, &gone, &holder)?;
    Ok(gone)
}

/// Delete the trees of `ids`; the states whose trees are gone after it (a state kept as the base
/// of a thin layer stays).
fn absent_after_delete(repo: &Repo, ids: &[String]) -> Vec<String> {
    repo.p.delete_states(ids).into_iter().filter(|id| !repo.p.has_state(id)).collect()
}
