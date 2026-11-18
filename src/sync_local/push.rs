//! Local push selection, secrets policy and pinned state reads.
use super::{fetch, Local};
use crate::{model::StateRec, store_remote::Options};
use anyhow::{bail, Result};
use std::collections::BTreeSet;

impl Local<'_, '_> {
    pub(super) fn select_push(
        &self,
        record_lines: &[String],
        opts: &Options,
        present: &BTreeSet<String>,
        omitted: &mut Vec<String>,
    ) -> Result<Vec<StateRec>> {
        let repo = self.repo();
        let required = fetch::required_states(repo, record_lines, opts)?;
        let selection = fetch::push_selection(repo, record_lines, opts)?;
        let mut states = Vec::new();
        // A state that is here only as objects (its native copy retired after `layr migrate`) goes too: reading its tree
        // builds the copy again for this command (`objstore::build_state`). Before, such a state never left this store,
        // and a remote line could name a head whose tree no remote had.
        let mut candidates = repo.p.db.present_states()?;
        for id in repo.p.db.object_only_states()? {
            if !present.contains(&id) && crate::objstore::verified(&repo.p, &id)?.is_some() {
                candidates.push((id, false));
            }
        }
        for (id, deleted) in candidates {
            if deleted || present.contains(&id) {
                continue;
            }
            let state = repo.state(&id)?;
            if !fetch::wanted(&state, opts) && !required.contains(&id) {
                omitted.push(format!("{} {}", id, serde_json::to_value(state.kind)?));
                continue;
            }
            if selection.as_ref().is_some_and(|keep| !keep.contains(&id)) {
                continue;
            }
            states.push(state);
        }
        Ok(states)
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
struct Read {
    root: std::path::PathBuf,
}
impl layr_store::sync::StateRead for Read {
    fn legacy_root(&self) -> &std::path::Path {
        &self.root
    }
}
