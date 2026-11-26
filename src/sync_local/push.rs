//! Local push selection, secrets policy and pinned state reads.
use super::{fetch, Local};
use crate::{model::StateRec, store::Project, store_remote::Options};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};

impl Local<'_, '_> {
    pub(super) fn select_push(
        &self,
        record_lines: &[String],
        opts: &Options,
        present: &BTreeSet<String>,
        omitted: &mut Vec<String>,
    ) -> Result<Vec<StateRec>> {
        let repo = self.repo();
        // State rows, refs and line heads describe one catalog version: a push holds no project lock (`plan_fetch`).
        repo.p.db.with_read_snapshot(|| {
            let catalog = fetch::state_catalog(repo)?;
            let required = fetch::required_states(repo, record_lines, opts, &catalog)?;
            let selection = fetch::push_selection(repo, record_lines, opts, &catalog)?;
            let mut states = Vec::new();
            // A state that is here only as objects (its native copy retired after `layr migrate`) goes too: reading its
            // tree builds the copy again for this command (`objstore::build_state`). Before, such a state never left this
            // store, and a remote line could name a head whose tree no remote had.
            let mut candidates = repo.p.db.present_states()?;
            let unsent = repo.p.db.object_only_states()?.into_iter().filter(|id| !present.contains(id)).collect();
            candidates.extend(in_object_store(&repo.p, &catalog, unsent)?.into_iter().map(|id| (id, false)));
            for (id, deleted) in candidates {
                if deleted || present.contains(&id) {
                    continue;
                }
                let state = crate::db::need(catalog.get(&id), &format!("state {}", crate::ids::short(&id)))?;
                if !fetch::wanted(state, opts) && !required.contains(&id) {
                    omitted.push(format!("{} {}", id, serde_json::to_value(state.kind)?));
                    continue;
                }
                if selection.as_ref().is_some_and(|keep| !keep.contains(&id)) {
                    continue;
                }
                states.push(state.clone());
            }
            Ok(states)
        })
    }
    pub(super) fn open(&self, id: &str) -> Result<Box<dyn layr_store::sync::StateRead + '_>> {
        let repo = self.repo();
        // The caller holds command-duration pins. Preserve local recovery without lazy transport recursion.
        if !repo.p.has_state(id) {
            // As in Repo::check_state, freed states skip object rebuilding but may rebuild from local Git.
            let rebuilt = !matches!(repo.p.db.state_flags(id)?, Some((_, true))) && crate::objstore::build_state(repo, id)?;
            if !rebuilt {
                if crate::gitbridge::buildable(repo, id) {
                    crate::gitbridge::build_tree(repo, id, None, None)?;
                } else {
                    match repo.p.db.state_flags(id)? {
                        Some((_, true)) => {
                            bail!("state {} was freed: no line, ref, tag or undo record keeps it", crate::ids::short(id))
                        }
                        None => bail!("unknown state {}", crate::ids::short(id)),
                        _ => bail!("state {} is not available locally", crate::ids::short(id)),
                    }
                }
            }
        }
        let root = crate::source::storage::complete(&repo.p, id)?;
        Ok(Box::new(Read { root }))
    }
}
/// The states of `ids` whose tree and times table the local object store holds, in their order. One open serves all:
/// an open lists the packs and reads the dictionary again, and loads each pack index again at its first lookup. A
/// repack since the open can have moved a loose object into a pack that it has not listed: the objects that it misses
/// are looked up once more after one more listing.
fn in_object_store(p: &Project, catalog: &BTreeMap<String, StateRec>, ids: Vec<String>) -> Result<Vec<String>> {
    let mut named = Vec::new();
    for id in ids {
        if let Some(found) = catalog.get(&id).map(crate::objstore::named).transpose()?.flatten() {
            named.push((id, found));
        }
    }
    if named.is_empty() || !crate::objstore::exists(p) {
        return Ok(Vec::new());
    }
    let objects = crate::objstore::shared(p)?;
    let mut missing: HashSet<_> =
        named.iter().flat_map(|(_, found)| [found.tree, found.times]).filter(|object| !objects.has(object)).collect();
    if !missing.is_empty() {
        objects.refresh_packs()?;
        missing.retain(|object| !objects.has(object));
    }
    Ok(named
        .into_iter()
        .filter(|(_, found)| ![found.tree, found.times].iter().any(|o| missing.contains(o)))
        .map(|(id, _)| id)
        .collect())
}

struct Read {
    root: std::path::PathBuf,
}
impl layr_store::sync::StateRead for Read {
    fn legacy_root(&self) -> &std::path::Path {
        &self.root
    }
}
