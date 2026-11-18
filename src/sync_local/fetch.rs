//! Local fetch policy, import authorization and state publication.
use super::Local;
use crate::{model::StateRec, oplog, repo::Repo, store_remote::Options};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_store::sync::{FetchSelection, Imported};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

impl Local<'_, '_> {
    pub(super) fn import(&mut self, lines: &[String], keys: &BTreeMap<String, VerifyingKey>) -> Result<Imported> {
        let repo = self.repo();
        let bootstrap = repo.p.id.is_empty();
        if !bootstrap {
            for (machine, key) in keys {
                if repo.p.db.trusted(machine)?.as_deref().is_some_and(|trusted| trusted != hex::encode(key.to_bytes())) {
                    bail!("remote machine {machine}: first-record key does not match the trusted public key");
                }
            }
        }
        let count = {
            let lock = repo.p.lock_maintenance()?;
            let trust = if bootstrap { crate::store::Trust::All } else { crate::store::Trust::Known };
            lock.import_lines(lines, trust)?
        };
        if bootstrap {
            self.repo_mut()?.p.reload_id()?;
        }
        self.bootstrap = bootstrap;
        Ok(Imported { count, bootstrap, project: self.repo().p.id.clone() })
    }
    pub(super) fn finish(&mut self, keys: &BTreeMap<String, VerifyingKey>) -> Result<()> {
        anyhow::ensure!(self.bootstrap, "bootstrap trust requires an initial local import");
        let repo = self.repo();
        for (machine, key) in keys {
            if repo.p.db.trusted(machine)?.is_none() {
                let mut record = crate::model::DraftRecord::new("machine.trust");
                record.data = serde_json::json!({"machine":machine,"public_key":hex::encode(key.to_bytes())});
                repo.commit_record(&repo.p.lock_maintenance()?, self.ctx, record)?;
            }
        }
        self.bootstrap = false;
        Ok(())
    }
    pub(crate) fn select_fetch(
        &mut self,
        records: &[String],
        opts: &Options,
        available: &[String],
        clone_lines: Option<Vec<crate::model::Line>>,
    ) -> Result<FetchSelection<StateRec, crate::model::Line>> {
        let repo = self.repo();
        let ctx = self.ctx;
        let parent = self.parent;
        let mut selected = opts.clone();
        if let Some(parent) = parent {
            if let Some(state) = opts.caches_of_state.as_deref() {
                selected.only_caches = Some(caches_of_state(parent, repo, state)?);
            } else if !opts.caches_of.is_empty() {
                selected.only_caches = Some(caches_of_lines(parent, repo, &opts.caches_of)?);
            } else if !opts.lines.is_empty() {
                // `--line`: the caches that these lines start from, and the history of their own overlay lines.
                selected.only_caches = Some(caches_of_lines(parent, repo, &opts.lines)?);
            }
            if !opts.lines.is_empty() {
                selected.only_states = Some(states_of_lines(parent, repo, &opts.lines, opts.heads)?);
            }
        }
        let opts = &selected;
        let mut omitted = omitted_cache_refs(repo, opts)?;
        let mut required = required_states(repo, records, opts)?;
        if let Some(lines) = &clone_lines {
            required.extend(state_ancestries(repo, lines.iter().map(|line| line.head.clone()))?);
        }
        // `--heads`: the heads of the lines and what an undo of a line delete needs; the other states stay on the remote.
        let lines_keep = base_selection(repo, records, opts)?;
        let heads: Option<BTreeSet<String>> = opts
            .heads
            .then(|| -> Result<BTreeSet<String>> {
                let lines = match &clone_lines {
                    Some(lines) => lines.clone(),
                    None => repo.p.db.lines()?,
                };
                if let Some(name) = opts.only_line.as_ref().filter(|_| !repo.p.is_overlay()) {
                    let line = lines
                        .iter()
                        .find(|line| line.name == *name)
                        .with_context(|| format!("line '{name}' is not in the store"))?;
                    return Ok(BTreeSet::from([line.head.clone()]));
                }
                let mut keep: BTreeSet<String> = lines.into_iter().map(|l| l.head).collect();
                keep.extend(deletion_dependencies(records)?);
                Ok(keep)
            })
            .transpose()?;
        // `--line` and `--overlay` on the base: the other states stay on the remote, as with `--heads`.
        let heads = lines_keep.or(heads);
        let mut left = Vec::new();
        let mut missing = Vec::new();
        // A plain fetch of a cache overlay: its states stay on the remote, counted.
        let plain_cache = opts.plain_cache_fetch && repo.p.is_overlay();
        let mut stay = 0;
        for id in available {
            // A tree whose record this fetch did not read: a push published both after this fetch read the records
            // (the record first, then the tree), or an overlay left out the records of a signer that is not trusted
            // here yet. The next fetch takes it; nothing is imported without its record.
            let Some(state) = repo.p.db.state(id)? else {
                omitted.push(format!("state {}: its record is not taken here yet", crate::ids::short(id)));
                continue;
            };
            if !wanted(&state, opts) && !required.contains(id) {
                continue;
            }
            if heads.as_ref().is_some_and(|keep| !repo.p.is_overlay() && !keep.contains(id)) {
                if !repo.p.has_state(id) {
                    left.push(id.clone());
                }
                continue;
            }
            // A cache overlay state of another platform stays on the remote (docs/cache-overlays.md).
            if repo.p.is_overlay() && crate::cache_overlay::other_platform(state.cache_platform.as_deref()) {
                omitted.push(format!(
                    "cache state {}: platform {}",
                    crate::ids::short(id),
                    state.cache_platform.as_deref().unwrap_or("")
                ));
                continue;
            }
            // `--caches-of`: only the caches that a new line from those lines takes.
            // A cache state is the head of its overlay line, so `required` names it, but a cache that is not here is only
            // a cache that no line takes here: nothing else needs its files.
            if repo.p.is_overlay() && state.cache_of.is_some() && opts.only_caches.as_ref().is_some_and(|only| !only.contains(id))
            {
                if plain_cache {
                    stay += usize::from(!repo.p.has_state(id));
                } else {
                    omitted.push(format!("cache state {}: not for --line", crate::ids::short(id)));
                }
                continue;
            }
            // `--line`: a state of an overlay that no chosen line shows stays on the remote.
            if repo.p.is_overlay()
                && state.cache_of.is_none()
                && opts.only_states.as_ref().is_some_and(|only| !only.contains(id))
                && !repo.p.has_state(id)
            {
                if plain_cache {
                    stay += 1;
                } else {
                    omitted.push(format!("state {}: not for --line", crate::ids::short(id)));
                }
                continue;
            }
            // A cache of another user's unprotected line is never taken here (`usable_cache`): its data stays on the
            // remote, so a clone downloads only the caches that its lines can start from.
            if repo.p.is_overlay()
                && state.cache_of.is_some()
                && !(ctx.caller.is_root() || state.cache_shared || state.cache_by == Some(ctx.caller.uid))
            {
                omitted.push(format!("cache state {}: of another user's line", crate::ids::short(id)));
                continue;
            }
            if repo.p.has_state(id) {
                repo.p.lock_or_borrow()?.write(|tx| tx.set_present(id, true))?;
            } else {
                missing.push(state);
            }
        }
        if stay > 0 {
            let name = repo.p.name.rsplit_once('%').map_or(repo.p.name.as_str(), |(_, name)| name);
            omitted.push(format!(
            "{stay} state(s) stay on the remote; a new line fetches the cache it starts from, fetch --overlay {name} takes them all"
        ));
        }
        Ok(FetchSelection { missing, left, omitted, clone_lines, restricted: heads.is_some() })
    }
    pub(super) fn stage(&mut self, parent: Option<&str>) -> Result<std::path::PathBuf> {
        anyhow::ensure!(self.stage.is_none(), "a restore is already staged");
        let path = self.repo().p.work_dir().join(crate::store::temp_name());
        self.stage = Some(match parent {
            Some(parent) => {
                crate::repo::Temp::source(&self.repo().state_src(parent)?, path.clone(), crate::fs::Protection::Writable)?
            }
            None => crate::repo::Temp::empty(path.clone())?,
        });
        Ok(path)
    }
    pub(super) fn install(&mut self, state: &StateRec, transid: u64, files: usize) -> Result<usize> {
        let stage = self.stage.take().context("no state restore is staged")?;
        // The transfer has completed before publication acquires the project lock.
        match publish_restored(self.repo(), state, &stage.path, transid) {
            Ok(published) => {
                stage.discard()?;
                Ok(if published { files } else { 0 })
            }
            Err(error) => Err(stage.abort(error)),
        }
    }
}

pub(crate) fn source_lines(repo: &Repo, canonical_main: bool) -> Result<Vec<crate::model::Line>> {
    if repo.p.is_overlay() {
        return repo.p.db.lines();
    }
    let authorized = repo.p.authorized_records()?;
    // These proof keys authorize canonical-main selection only. Reader and cache
    // writer identities cannot act as merge executors or manifest authorities.
    let trusted = authorized
        .iter()
        .filter(|r| {
            r.scope()
                .terms()
                .is_none_or(|terms| matches!(terms.role, layr_store::trust::Role::Full | layr_store::trust::Role::Merge))
        })
        .map(|r| (r.record().machine.clone(), *r.signed().key()))
        .collect();
    let records = authorized.iter().map(|r| r.record().clone()).collect::<Vec<_>>();
    let creator = layr_model::project_perspective(&repo.p.id, &records)?;
    let mut model = layr_model::Project::from_authorized_records(&repo.p.id, authorized, &creator)?;
    if canonical_main && model.config.contains_key("queue.merge-machine") {
        layr_model::main_head::select(&mut model, &records, &trusted)?;
    }
    Ok(model.line_list())
}

pub(crate) fn omitted_cache_refs(repo: &Repo, opts: &Options) -> Result<Vec<String>> {
    let mut omitted = Vec::new();
    if !opts.caches {
        for reference in repo.p.db.refs("", crate::ids::now_ms())? {
            if repo.state(&reference.target)?.kind == crate::model::StateKind::Cache {
                omitted.push(format!(
                    "cache state {} of ref {}: fetch --overlay <overlay> takes it",
                    reference.target, reference.name
                ));
            }
        }
    }
    Ok(omitted)
}

pub(crate) fn walk_ancestry(
    tips: impl IntoIterator<Item = String>,
    mut parents: impl FnMut(&str) -> Result<Vec<String>>,
) -> Result<Vec<String>> {
    let mut queue: std::collections::VecDeque<String> = tips.into_iter().collect();
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        queue.extend(parents(&id)?);
        order.push(id);
    }
    Ok(order)
}

pub(crate) fn state_ancestries(repo: &Repo, tips: impl IntoIterator<Item = String>) -> Result<Vec<String>> {
    walk_ancestry(tips, |id| {
        Ok(repo.p.db.state(id)?.map(|state| state.parents.into_iter().chain(state.derived_from).collect()).unwrap_or_default())
    })
}

pub(crate) fn state_ancestry(repo: &Repo, id: &str) -> Result<Vec<String>> {
    state_ancestries(repo, [id.to_string()])
}

pub(crate) fn required_states(repo: &Repo, lines: &[String], opts: &Options) -> Result<BTreeSet<String>> {
    let mut required = deletion_dependencies(lines)?;
    let mut tips: Vec<String> = repo.p.db.lines()?.into_iter().map(|line| line.head).collect();
    for reference in repo.p.db.refs("", crate::ids::now_ms())? {
        let state = repo.state(&reference.target)?;
        if state.kind == crate::model::StateKind::Cache && !opts.caches {
            continue;
        }
        tips.push(reference.target);
    }
    required.extend(state_ancestries(repo, tips)?);
    Ok(required)
}

/// The base states that `fetch --line` and `--overlay` take, or None for every state. Without `base` in `--overlay`
/// no base state comes; with `--line`, the history of those lines (only their heads with `--heads`) and what an undo of
/// a line delete needs.
pub(crate) fn base_selection(repo: &Repo, records: &[String], opts: &Options) -> Result<Option<BTreeSet<String>>> {
    if repo.p.is_overlay() {
        return Ok(None);
    }
    let mut tips = Vec::new();
    for name in &opts.lines {
        let line = repo.p.db.line_by_name(name)?.with_context(|| format!("--line: line '{name}' is not in the store"))?;
        tips.push(line.head.clone());
        // Every machine's head of the line: a server queue can move it ahead of this machine's head.
        for (_, head) in repo.p.db.line_heads(&line.id)? {
            tips.push(head);
        }
    }
    if let Some(only) = &opts.overlays {
        for name in only.iter().filter(|name| *name != crate::overlay::MAIN) {
            if repo.overlay_info(name)?.is_none() {
                bail!("--overlay: no overlay '{name}' in project {}", repo.p.name);
            }
        }
    }
    if !opts.takes_layer(crate::overlay::MAIN) {
        return Ok(Some(BTreeSet::new()));
    }
    if opts.lines.is_empty() {
        return Ok(None);
    }
    let mut keep: BTreeSet<String> =
        if opts.heads { tips.into_iter().collect() } else { state_ancestries(repo, tips)?.into_iter().collect() };
    keep.extend(deletion_dependencies(records)?);
    Ok(Some(keep))
}

/// The states that `push --line` and `--overlay` send, or None for every state: on the base as for a fetch, in an
/// overlay the selection of its layer (`only_states`).
pub(crate) fn push_selection(repo: &Repo, records: &[String], opts: &Options) -> Result<Option<BTreeSet<String>>> {
    if repo.p.is_overlay() {
        return Ok(opts.only_states.clone());
    }
    let mut base = opts.clone();
    base.heads = false;
    base_selection(repo, records, &base)
}

pub(crate) fn wanted(state: &crate::model::StateRec, opts: &Options) -> bool {
    let kind = serde_json::to_value(state.kind).unwrap_or_default();
    match kind.as_str() {
        Some("auto") => opts.auto,
        Some("cache") => opts.caches,
        _ => true,
    }
}

/// Freeze the restored folder `stage` into the state `state` under the project
/// lock, with the identity of its signed state (`transid` names the btrfs send it came from, else 0). False
/// when the state was there already (`stage` stays for the caller to delete).
pub(crate) fn publish_restored(repo: &Repo, state: &crate::model::StateRec, stage: &Path, transid: u64) -> Result<bool> {
    let id = &state.id;
    let _lock = repo.p.lock_or_borrow()?;
    let destination = repo.p.state_path(id);
    if destination.exists() {
        repo.p.lock_or_borrow()?.write(|tx| tx.set_present(id, true))?;
        return Ok(false);
    }
    let captured = crate::fs::freeze_path_owned(stage, &destination, crate::fs::CapturePurpose::State)?;
    // A tree built from git has no signed UUID and receives a fresh one here.
    let received = if state.subvol.is_empty() { crate::ids::new_id() } else { state.subvol.clone() };
    let owner = captured.received_identity(&received, transid)?;
    match repo.p.lock_or_borrow()?.write(|tx| tx.set_present(id, true)) {
        Ok(()) => owner.release(),
        Err(crate::db::CommitResolution::NotCommitted(error)) => return Err(owner.abort(error)),
        Err(error) => {
            owner.release();
            return Err(error.into());
        }
    }
    Ok(true)
}

pub(crate) fn materialize_clone_lines(
    ctx: &crate::ctx::Ctx,
    repo: &mut Repo,
    only: Option<&BTreeSet<String>>,
    source_lines: Option<&[crate::model::Line]>,
    dir: &Path,
    heads: bool,
) -> Result<Vec<String>> {
    // The working folders are written with no project lock: each under its line's lock, with the
    // head pinned. Only the adopt record is written under the project lock, after the folder.
    let lines = match source_lines {
        Some(lines) => lines.to_vec(),
        None => repo.p.db.lines()?,
    };
    let mut pending = Vec::new();
    let mut missing = Vec::new();
    for line in lines {
        if only.is_some_and(|ids| !ids.contains(&line.id)) {
            continue;
        }
        let local = crate::model::Line { machine: repo.p.store.machine.id.clone(), owner: ctx.caller.uid, ..line.clone() };
        if repo.working(&local).exists() {
            continue;
        }
        if !repo.p.has_state(&line.head) {
            pending.push(line.id.clone());
            missing.push(format!(
                "line {}: head state {} is not in the store yet; run `layr fetch <remote>` later",
                line.name, line.head
            ));
            continue;
        }
        repo.hold(repo.pin(&[line.head.as_str()])?);
        let mut created = false;
        {
            let _line = repo.line_lock(&local)?;
            if !repo.working(&local).exists() {
                // Git export walks ancestors: a heads-only fetch must leave their trees
                // on the remote. Restore the normal read policy even on failure.
                let restore = repo.restore_from_remote;
                repo.restore_from_remote = restore && !heads;
                let result = repo.materialize(&local, &local.head, &repo.working(&local));
                repo.restore_from_remote = restore;
                result?;
                created = true;
            }
        }
        if created {
            let _lock = repo.p.lock_maintenance()?;
            let mut record = crate::model::DraftRecord::new("line.adopt");
            record.lines.insert(
                local.id.clone(),
                crate::model::LineChange {
                    transition: crate::model::LineTransition::from_endpoints(
                        repo.p.db.line_by_id(&line.id)?.map(|before| repo.line_state(&before, &before.head)),
                        Some(repo.line_state(&local, &local.head)),
                    ),
                    working_before: None,
                    working_after: None,
                },
            );
            let shown = repo.readable_layers(&local)?;
            record.data = serde_json::json!({
                "folder_view": repo.public_view_data(&local.id, &shown)?,
            });
            repo.commit_record(&_lock, ctx, record)?;
        }
    }
    crate::fsutil::atomic_write(&dir.join("pending-lines.json"), &serde_json::to_vec(&pending)?, 0o600)?;
    Ok(missing)
}

pub(crate) fn deletion_dependencies(lines: &[String]) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for line in lines {
        let record = oplog::parse_line(line)?.record;
        if record.op != "line.delete" {
            continue;
        }
        if let Some(state) = record.data["final_state"].as_str() {
            ids.insert(state.to_string());
        }
        if let Some(state) = record.data["staged_state"].as_str() {
            ids.insert(state.to_string());
        }
        for change in record.lines.values() {
            if let Some(state) = &change.working_before {
                ids.insert(state.clone());
            }
        }
    }
    Ok(ids)
}

/// The states of the overlay `orepo` that the project lines `lines` show: the history of each line on the overlay
/// (the same name and id, or a name of layr before 0.44: `line-<line id>`, with any platform suffix) and of the
/// shared lines (`user-<uid>`), only the heads with `heads`. It adds the part that each line shows and the parts
/// that the states of the line name, with the history of each part.
pub(crate) fn states_of_lines(repo: &Repo, orepo: &Repo, lines: &[String], heads: bool) -> Result<BTreeSet<String>> {
    let overlay = orepo.p.name.rsplit_once('%').map(|(_, g)| g.to_string());
    let mut ids = Vec::new();
    let mut line_ids = Vec::new();
    let mut parts = Vec::new();
    for name in lines {
        let line = repo.p.db.line_by_name(name)?.with_context(|| format!("--line: line '{name}' is not in the store"))?;
        // The line on the overlay: its own name, or a name of layr before 0.44 (`line-<id>`, `line-<id>.<platform>`).
        line_ids.push(line.id.clone());
        ids.push(line.name.clone());
        ids.push(format!("line-{}", line.id));
        let Some(overlay) = &overlay else { continue };
        // Every machine's head of the line (a remote moves the line ahead of this machine).
        let mut tips = vec![line.head.clone()];
        parts.extend(repo.layers(&line).unwrap_or_default().into_iter().filter(|l| &l.overlay == overlay).map(|l| l.state));
        for (_machine, head) in repo.p.db.line_heads(&line.id)? {
            tips.push(head);
        }
        let states = if heads { tips } else { state_ancestries(repo, tips)? };
        for id in states {
            if let Some(part) = repo.p.db.state(&id)?.and_then(|st| st.parts.get(overlay).cloned()) {
                parts.push(part);
            }
        }
    }
    let tips: Vec<String> = orepo
        .p
        .db
        .lines()?
        .into_iter()
        .filter(|l| {
            l.name.starts_with("user-")
                || line_ids.contains(&l.id)
                || ids.iter().any(|id| l.name == *id || (id.starts_with("line-") && l.name.starts_with(&format!("{id}."))))
        })
        .map(|l| l.head)
        .chain(parts)
        .collect();
    if heads {
        return Ok(tips.into_iter().collect());
    }
    Ok(state_ancestries(orepo, tips)?.into_iter().collect())
}

/// The cache states of the cache overlay `orepo` that `line new X <line>` takes for each of `lines`, from the records
/// alone (the states are not here yet): the newest cache of the line's own overlay line, and the newest cache of this
/// platform saved with its head or else with its nearest ancestor that has one (`cache_source`).
pub(crate) fn caches_of_lines(repo: &Repo, orepo: &Repo, lines: &[String]) -> Result<BTreeSet<String>> {
    caches_of_lines_for(repo, orepo, lines, None)
}

/// `caches_of_lines`, with `uid`: only the caches that a line of that user can take (`usable_cache`).
pub(crate) fn caches_of_lines_for(repo: &Repo, orepo: &Repo, lines: &[String], uid: Option<u32>) -> Result<BTreeSet<String>> {
    let saved_with = saved_with(repo, orepo, uid)?;
    let mut out = BTreeSet::new();
    for name in lines {
        let line = repo.line(name).with_context(|| format!("--line: no line {name}"))?;
        // The line on the overlay: its own name or id, or a name of layr before 0.44 (`line-<id>.<platform>`).
        if let Some(own) = crate::overlay::find_follow_line(&orepo.p.db, &line)? {
            if !orepo.p.db.state(&own.head)?.is_some_and(|st| crate::cache_overlay::other_platform(st.cache_platform.as_deref()))
                && usable_by(repo, orepo, &own.head, uid)?
            {
                out.insert(own.head);
            }
        }
        // Every machine's head of the line: a check runner's own main can be an old head while a server
        // queue moved main on its merge machine, and the runner checks states made from that newer head.
        let mut heads = vec![line.head.clone()];
        for (_, head) in repo.p.db.line_heads(&line.id)? {
            if !heads.contains(&head) {
                heads.push(head);
            }
        }
        for head in heads {
            out.extend(nearest_cache(repo, &saved_with, head)?);
        }
    }
    Ok(out)
}

/// The cache state of the cache overlay `orepo` that `line new X <state>` takes for a line of `uid`, from the records
/// alone: the newest usable cache of this platform saved with `state` or else with its nearest ancestor that has one
/// (`cache_source`).
pub(crate) fn caches_from_state(repo: &Repo, orepo: &Repo, state: &str, uid: u32) -> Result<BTreeSet<String>> {
    Ok(nearest_cache(repo, &saved_with(repo, orepo, Some(uid))?, state.to_string())?.into_iter().collect())
}

/// Whether a line of `uid` may take the cache state `id` (`Repo::usable_cache` without the key, which needs the
/// working folder): made by `uid`, shared, or the overlay takes caches of other users (`trust=readers`). None: any.
fn usable_by(repo: &Repo, orepo: &Repo, id: &str, uid: Option<u32>) -> Result<bool> {
    let Some(uid) = uid else { return Ok(true) };
    let Some(st) = orepo.p.db.state(id)? else { return Ok(false) };
    let name = orepo.p.name.rsplit_once('%').map_or(orepo.p.name.as_str(), |(_, name)| name);
    Ok(uid == 0 || st.cache_shared || st.cache_by == Some(uid) || !repo.trust_protected(name)?)
}

/// The newest cache state of this platform saved with each project state, from the records of `orepo`; with `uid`,
/// only the caches that a line of that user can take, as `cache_source` filters them before it picks the newest.
fn saved_with(repo: &Repo, orepo: &Repo, uid: Option<u32>) -> Result<BTreeMap<String, String>> {
    let mut saved_with: BTreeMap<String, String> = BTreeMap::new();
    for (id, of) in orepo.p.db.cache_states_known()? {
        // A cache of another platform stays on the remote: a newer one (an x86 runner's after an arm runner's) must
        // not hide this platform's cache of the same state.
        if orepo.p.db.state(&id)?.is_some_and(|st| crate::cache_overlay::other_platform(st.cache_platform.as_deref())) {
            continue;
        }
        if !usable_by(repo, orepo, &id, uid)? {
            continue;
        }
        // A save with no named change saves with an automatic state over the head: it counts for the head.
        if let Some(st) = repo.p.db.state(&of)? {
            if st.kind == crate::model::StateKind::Auto {
                if let Some(parent) = st.parents.first() {
                    saved_with.entry(parent.clone()).or_insert_with(|| id.clone());
                }
            }
        }
        saved_with.entry(of).or_insert(id);
    }
    Ok(saved_with)
}

/// The cache saved with `head` or else with its nearest ancestor that has one (at most 200 states back).
fn nearest_cache(repo: &Repo, saved_with: &BTreeMap<String, String>, head: String) -> Result<Option<String>> {
    let mut seen = BTreeSet::new();
    let mut queue = std::collections::VecDeque::from([head]);
    while let Some(id) = queue.pop_front() {
        if seen.len() >= 200 || !seen.insert(id.clone()) {
            continue;
        }
        if let Some(cache) = saved_with.get(&id) {
            return Ok(Some(cache.clone()));
        }
        if let Some(state) = repo.p.db.state(&id)? {
            queue.extend(state.parents);
        }
    }
    Ok(None)
}

pub(crate) fn caches_of_state(repo: &Repo, orepo: &Repo, state: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for (id, of) in orepo.p.db.cache_states_known()? {
        let auto_of_state = repo
            .p
            .db
            .state(&of)?
            .is_some_and(|s| s.kind == crate::model::StateKind::Auto && s.parents.first().is_some_and(|p| p == state));
        if of == state || auto_of_state {
            out.insert(id);
        }
    }
    Ok(out)
}
