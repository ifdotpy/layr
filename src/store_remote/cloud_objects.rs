//! States on the Layr Cloud API v1 as trees, times tables and packs (docs/objects.md, docs/remotes.md).
//!
//! A push writes each state into the project's object store (`meta/objects`, the store of
//! `layr objects`), asks the server which of the objects that the states reach it lacks, sends those
//! in transfer packs (`layr_objects::transfer`), and publishes each state as `{"times","tree"}`. A
//! fetch reads the state bodies, downloads the packs that hold the missing trees, times tables and
//! blobs, and checks each state out of the object store.

use super::{reader, writer, Options, Report, Store};
use crate::{ctx::Ctx, model::StateRec, repo::Repo};
use anyhow::{bail, Context, Result};
use layr_objects::{transfer, Id, Keying, Kind, StateRef, Store as Objects, Temperature, WriteOptions};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// The object store of the project, as `layr objects` opens it.
fn open(repo: &Repo) -> Result<Objects> {
    #[allow(unused_mut)]
    let mut objects = Objects::open(&repo.p.meta_dir().join("objects"), Keying::Plain)?;
    // A sealed APFS state has immutable files, and a clone keeps the flag.
    #[cfg(target_os = "macos")]
    {
        objects.clone_hook = Some(crate::sys::apfs_unseal_fd);
    }
    Ok(objects)
}

/// The tree and times table of `state`, written into the object store unless it is there. When the first
/// parent is there, only the paths that changed since are read; new blobs are cold (a delta against the
/// parent's blob at the same path, else a zstd frame).
fn recorded(objects: &Objects, state: &StateRec) -> Result<Option<StateRef>> {
    let (Some(tree), Some(times)) = (state.tree.as_deref(), state.times.as_deref()) else { return Ok(None) };
    let found = StateRef { tree: Id::parse(tree)?, times: Id::parse(times)? };
    Ok((objects.has(&found.tree) && objects.has(&found.times)).then_some(found))
}

fn import(objects: &Objects, repo: &Repo, state: &StateRec) -> Result<StateRef> {
    if let Some(found) = recorded(objects, state)? {
        return Ok(found);
    }
    if let Some(found) = objects.state_ref(&state.id)? {
        return Ok(found);
    }
    let first = state.parents.first().or(state.derived_from.as_ref());
    let base = match first {
        Some(parent) if repo.p.has_state(parent) => objects.state_ref(parent)?.map(|r| (parent.clone(), r)),
        _ => None,
    };
    let _pin = repo.pin(&[state.id.as_str()])?;
    let root = repo.state_root(&state.id)?;
    let options = WriteOptions {
        temperature: Temperature::Cold,
        previous: base.as_ref().map(|(_, r)| r.tree),
        exclude: vec![b".git".to_vec()],
        similar: false,
    };
    let report = match base {
        Some((parent, r)) => {
            let _parent_pin = repo.pin(&[parent.as_str()])?;
            let (paths, dirty) = crate::changes::object_candidates(&repo.state_src(&parent)?, &repo.state_src(&state.id)?)?;
            layr_objects::write::write_changed_with_dirty(
                objects,
                &root,
                &layr_objects::write::Base { tree: r.tree, times: r.times },
                &paths,
                &dirty,
                &options,
            )?
        }
        None => layr_objects::write_tree(objects, &root, &options)?,
    };
    let found = StateRef {
        tree: report.tree.context("a write gives a tree")?,
        times: report.times.context("a write gives a times table")?,
    };
    objects.set_state(&state.id, &found)?;
    Ok(found)
}

/// Pushes the states that the server lacks (docs/remotes.md, "Trees, states and packs").
pub(super) fn push_states(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options, report: &mut Report) -> Result<()> {
    let api = store.backend.api().context("not a Layr Cloud remote")?;
    let scope = store.api_scope();
    let lines: Vec<String> = repo.p.export_jsonl()?.lines().map(str::to_string).collect();
    let required = reader::required_states(repo, &lines, opts)?;
    // Open once: loading pack indexes once per migrated state makes a large history expensive to push.
    let objects = crate::objstore::exists(&repo.p).then(|| open(repo)).transpose()?;
    let mut candidates = Vec::new();
    for (id, _) in repo.p.db.state_auxiliary()? {
        let Some((present, deleted)) = repo.p.db.state_flags(&id)? else { continue };
        if deleted {
            continue;
        }
        let state = repo.state(&id)?;
        if !present && objects.as_ref().map(|store| recorded(store, &state)).transpose()?.flatten().is_none() {
            continue;
        }
        if !reader::wanted(&state, opts) && !required.contains(&id) {
            report.omitted.push(format!("{} {}", id, serde_json::to_value(state.kind)?));
            continue;
        }
        candidates.push(state);
    }
    api.learn_states(&scope, &candidates.iter().map(|s| s.id.clone()).collect::<Vec<_>>())?;
    let mut states = Vec::new();
    for state in candidates {
        if api.state_held(&scope, &state.id)? {
            continue;
        }
        // Pinned before the trees are read: gc keeps the state until this command ends.
        match repo.pin(&[state.id.as_str()]) {
            Ok(pin) => repo.hold(pin),
            Err(e) => {
                report.omitted.push(format!("{} {}: {e:#}", state.id, serde_json::to_value(state.kind)?));
                continue;
            }
        }
        states.push(state);
    }
    if states.is_empty() {
        return Ok(());
    }
    crate::secrets::check_states(ctx, repo, states.iter().map(|s| s.id.as_str()))?;
    let objects = match objects {
        Some(objects) => objects,
        None => open(repo)?,
    };
    let states = writer::parents_first(states)?;
    let mut refs = Vec::with_capacity(states.len());
    for state in &states {
        let _stage = crate::trace::span("store objects import");
        let local = import(&objects, repo, state)?;
        let wire = layr_objects::cloud_times::from_local(&objects, &local.tree, &local.times)?;
        let times = objects.put_cloud_times(&wire)?;
        refs.push((state.id.clone(), StateRef { tree: local.tree, times }));
    }
    // Every object that the states reach, once: times tables, trees, blobs.
    let mut reach: Vec<(Id, Kind)> = Vec::new();
    let mut seen = HashSet::new();
    for (_, r) in &refs {
        // The wire table is flat; its only times object is this root.
        let times = std::iter::once((r.times, Kind::Times));
        for item in times.chain(transfer::reachable(&objects, &r.tree)?) {
            if seen.insert(item.0) {
                reach.push(item);
            }
        }
    }
    let missing: BTreeSet<String> =
        api.objects_missing(&reach.iter().map(|(id, _)| id.hex()).collect::<Vec<_>>())?.into_iter().collect();
    let send: Vec<(Id, Kind)> = reach.into_iter().filter(|(id, _)| missing.contains(&id.hex())).collect();
    report.files += send.iter().filter(|(_, kind)| *kind == Kind::Blob).count();
    for pack in transfer::build_packs(&objects, &send)? {
        let _stage = crate::trace::span("store pack publish");
        api.put_object(Kind::Opaque, &pack.data)?;
        api.put_pack_index(&pack.id.hex(), &pack.index)?;
        report.bytes += (pack.data.len() + pack.index.len()) as u64;
    }
    for (id, r) in refs {
        api.put_state(&scope, &id, &r)?;
        report.states += 1;
        report.state_ids.push(id);
    }
    Ok(())
}

/// Fetches `states` from the server into the project: their objects into the object store, then each
/// state checked out with its times. A state that the server lacks is named in the report.
pub(super) fn fetch_states(store: &Store, repo: &Repo, states: Vec<StateRec>, report: &mut Report) -> Result<()> {
    if states.is_empty() {
        return Ok(());
    }
    let api = store.backend.api().context("not a Layr Cloud remote")?;
    let scope = store.api_scope();
    let mut objects = open(repo)?;
    let mut refs = Vec::new();
    for state in writer::parents_first(states)? {
        match api.get_state(&scope, &state.id)? {
            Some(r) => refs.push((state, r)),
            None => report.omitted.push(format!("state {}: not on Layr Cloud", crate::ids::short(&state.id))),
        }
    }
    // Rounds: the missing trees and times tables first (a tree names its subtrees and blobs), then the blobs.
    // A pack usually holds all of one push, so one or two rounds do.
    let mut packs_read = BTreeSet::new();
    for round in 0.. {
        if round == 64 {
            bail!("Layr Cloud: the objects of the states did not come in 64 rounds");
        }
        let mut need = BTreeSet::new();
        for (_, r) in &refs {
            need.extend(transfer::missing_trees(&objects, &r.tree)?.into_iter().map(|id| id.hex()));
            need.extend(transfer::missing_cloud_times(&objects, &r.times)?.into_iter().map(|id| id.hex()));
        }
        if need.is_empty() {
            for (_, r) in &refs {
                for (id, kind) in transfer::reachable(&objects, &r.tree)? {
                    if kind == Kind::Blob && !objects.has(&id) {
                        need.insert(id.hex());
                    }
                }
            }
        }
        if need.is_empty() {
            break;
        }
        let need: Vec<String> = need.into_iter().collect();
        let located: BTreeMap<String, String> = api.locate(&need)?;
        if let Some(id) = need.iter().find(|id| !located.contains_key(*id)) {
            bail!("Layr Cloud has no pack with object {id}");
        }
        let packs: BTreeSet<String> = located.into_values().collect();
        let fresh: Vec<&String> = packs.iter().filter(|p| !packs_read.contains(*p)).collect();
        if fresh.is_empty() {
            bail!(
                "Layr Cloud packs {} do not hold the objects they are said to hold",
                packs.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        for pack in fresh {
            let _stage = crate::trace::span("store pack fetch");
            let data = api.get_object(Kind::Opaque, pack)?.with_context(|| format!("Layr Cloud lacks pack {pack}"))?;
            let index = api.get_pack_index(pack)?;
            transfer::add_cloud_pack(&objects, &data, &index).with_context(|| format!("pack {pack} from Layr Cloud"))?;
            report.bytes += (data.len() + index.len()) as u64;
            packs_read.insert(pack.clone());
        }
        // Open again: the store reads the new pack indexes.
        objects = open(repo)?;
    }
    for (state, r) in refs {
        if repo.p.has_state(&state.id) {
            repo.p.db.set_present(&state.id, true)?;
            continue;
        }
        let times = transfer::cloud_times_to_folder(&objects, &r.tree, &r.times)?;
        let local = StateRef { tree: r.tree, times };
        objects.set_state(&state.id, &local)?;
        let stage = repo.p.work_dir().join(crate::store::temp_name());
        crate::storage::create_subvolume(&stage)?;
        let result = (|| -> Result<u64> {
            let made = layr_objects::checkout_with(&objects, &local.tree, Some(&local.times), &stage)?;
            reader::publish_restored(repo, &state, &stage, 0)?;
            Ok(made.files)
        })();
        if stage.exists() {
            let _ = crate::storage::delete_tree(&stage);
        }
        report.files += result.with_context(|| format!("state {}", crate::ids::short(&state.id)))? as usize;
        report.states += 1;
        report.state_ids.push(state.id.clone());
    }
    Ok(())
}
