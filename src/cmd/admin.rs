//! save, undo, op log, try, gc, fsck, reindex, init, projects, config, cache.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::model::{Line, LineChange, Record, StateKind};
use crate::repo::{skip_path, Repo};
use crate::revs;
use crate::storage as btrfs;
use crate::store::Project;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(super) fn operation_name(op: &str) -> String {
    let (name, suffix) = op.split_once('.').unwrap_or((op, ""));
    let name = match name {
        "commit" => "state",
        "stash" => "park",
        "cherry-pick" => "pick",
        "rebase" => "replay",
        "revert" => "invert",
        _ => name,
    };
    if suffix.is_empty() {
        name.to_string()
    } else {
        format!("{name}.{suffix}")
    }
}

pub fn save(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("-m|--message")
        .value("--note")
        .value("-F|--file")
        .flag("-a|--all")
        .flag("--amend")
        .flag("--allow-empty")
        .flag("--no-verify|-n")
        .flag("-v|--verbose")
        .flag("--no-edit")
        .flag("--allow-conflict-markers")
        .value("--author")
        .flag("-s|--signoff")
        .flag("--running")
        .flag("--force")
        .flag("--caches")
        .value("--cache")
        .removed("--layers", "--caches")
        .flag("--all-lines")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    if a.has("-m") || a.has("-F") || a.has("--amend") {
        return super::work::commit(ctx, args);
    }
    if a.has("-a") {
        return Err(exit(2, "Use --all-lines to save automatic states of all lines, or use -a with layr save -m <message>."));
    }
    if ["--allow-empty", "--no-verify", "-v", "--no-edit", "--author", "-s"].iter().any(|flag| a.has(flag)) {
        return Err(exit(2, "These options require a named state. Use 'layr save -m <message>'."));
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    let lines: Vec<Line> = if a.has("--all-lines") {
        repo.require(ctx, "line.manage")?;
        repo.lines()?.into_iter().filter(|l| repo.working(l).exists()).collect()
    } else {
        vec![h.line()?.clone()]
    };
    let msg = a.get("--note").unwrap_or("").to_string();
    // `--cache <path>` (repeatable) saves only the named cache paths. A big cache (build output)
    // costs a minute to save.
    let only: Vec<String> = a.all("--cache").iter().map(|p| p.trim_matches('/').to_string()).collect();
    if !only.is_empty() && !a.has("--caches") {
        return Err(exit(2, "--cache selects a cache path for --caches."));
    }
    // Folders that a process uses (or that changed while copied): refused, the others go on.
    let mut refusals: Vec<String> = Vec::new();
    for l in lines {
        repo.authorize_line(ctx, &l, crate::repo::LineOp::Save)?;
        if a.has("--caches") {
            // A cache copy that a later cache fails to make leaves the earlier ones in the work folder.
            repo.hold(repo.pin(&[&l.head, &repo.save_base(&l)?])?);
            if let Err(e) = prepare_caches(repo, &l, &only) {
                repo.discard_prepared();
                return Err(e);
            }
        } else if !a.has("--force") {
            repo.prepare_save(&l, false)?;
        }
        let _lock = repo.p.lock()?;
        let l = repo.line(&l.id)?;
        crate::repo::recover_cache_replacements(&repo.working(&l))?;
        let (mut caches, mut versioned) = if a.has("--caches") { repo.line_cache_paths(&l)? } else { Default::default() };
        if !only.is_empty() {
            if let Some(missing) = only.iter().find(|o| !caches.contains(o) && !versioned.contains(o)) {
                return Err(exit(1, format!("error: '{missing}' is no cache path of the line (see layr overlay list)")));
            }
            caches.retain(|p| only.contains(p));
            versioned.retain(|p| only.contains(p));
            if caches.is_empty() && versioned.is_empty() {
                return Err(exit(
                    1,
                    format!("error: '{}' is no cache path of the line (see layr overlay list)", only.join("', '")),
                ));
            }
        }
        if a.has("--caches") {
            // A folder that an interrupted prune left damaged is not saved until a prune of it succeeds.
            let damaged = repo.damaged_folders(&l)?;
            for p in caches.iter().filter(|p| damaged.contains(*p)) {
                if !a.has("-q") {
                    outln!(ctx, "{p}: skipped: damaged by an interrupted prune; run layr save --caches again to prune it");
                }
            }
            caches.retain(|p| !damaged.contains(p));
        }
        let st = if a.has("--caches") {
            // A cache folder that a tool made again as a plain folder becomes a cache first, so
            // its files stay out of the state. A folder with versioned files is no cache; one
            // that an older layr made a cache becomes a plain folder again, so the state keeps
            // its files.
            let (u, g) = repo.owner_ids(&l);
            for p in &versioned {
                match crate::repo::uncache(&repo.working(&l), p, u, g) {
                    Ok(again) => {
                        if !a.has("-q") {
                            outln!(ctx, "{p}: {}", if again { VERSIONED_AGAIN } else { VERSIONED });
                        }
                    }
                    Err(e) => refuse(e, &mut refusals)?,
                }
            }
            let mut refused = Vec::new();
            for p in &caches {
                if let Err(e) = crate::repo::convert_cache(&repo.working(&l), p, u, g) {
                    refuse(e, &mut refusals)?;
                    refused.push(p.clone());
                }
            }
            caches.retain(|p| !refused.contains(p));
            Some(repo.caches_state(ctx, &l, &msg, a.has("--running"), &caches)?)
        } else {
            repo.save(ctx, &l, &msg, a.has("--running"), a.has("--force"))?
        };
        match st {
            Some(s) => {
                if a.has("--caches") {
                    // Caches saved on a protected line may warm other users' lines.
                    let shared = repo.policy().rule_for(&l.name).is_some();
                    record_caches(ctx, repo, &s.id, &repo.working(&l), &caches, shared)?;
                    // The line's caches as cache overlays (docs/cache-overlays.md): new lines from
                    // this line take them, whatever state the line's name selects.
                    for (p, name, id) in repo.save_cache_overlays(ctx, &l, &caches, shared, &s.id)? {
                        if !a.has("-q") {
                            outln!(ctx, "{p}: overlay {name} {}", ids::short(&id));
                        }
                    }
                    // Retention when caches are made, not only in gc: no daemon has to run.
                    trash_excess_caches(repo)?;
                }
                if !a.has("-q") {
                    outln!(ctx, "{} {}", l.name, ids::display(&s.id));
                }
            }
            None => {
                if !a.has("-q") {
                    // The state that equals the working content, in the format of a new state.
                    outln!(ctx, "{} {}", l.name, ids::display(&repo.save_base(&l)?));
                }
            }
        }
        // Layers and snapshots that the save did not take (an unchanged folder, or --force).
        drop(_lock);
        repo.discard_prepared();
    }
    if !refusals.is_empty() {
        return Err(exit(1, refusals.join("\n")));
    }
    Ok(0)
}

/// Keep the message of a folder that a process uses (the other folders go on). Other errors stop the save.
fn refuse(e: anyhow::Error, refusals: &mut Vec<String>) -> Result<()> {
    if e.downcast_ref::<crate::repo::CacheRefused>().is_none() {
        return Err(e);
    }
    refusals.push(e.to_string());
    Ok(())
}

/// A cache folder that a tool made as a plain folder becomes a cache by a copy of all its files,
/// and on APFS a snapshot copies every file too: a save with caches took a minute under the
/// store lock for build output. This does both before the lock, for the selected caches (`only`:
/// `--cache`): the plain folder becomes a cache, then (APFS) one copy for the state's cache (none
/// when the head already has it) and one for the cache overlay. Under the lock a conversion finds
/// a cache already, a snapshot takes a copy with one rename, and unused copies go to the trash.
/// The files of a cache are build output: a copy from a moment before the lock is as good.
/// Prepare the copies of the line's caches before the lock; `only` (empty: all) names the paths.
fn prepare_caches(repo: &Repo, l: &Line, only: &[String]) -> Result<()> {
    let w = repo.working(l);
    let (u, g) = repo.owner_ids(l);
    for p in repo.line_cache_paths(l)?.0 {
        if !only.is_empty() && !only.contains(&p) {
            continue;
        }
        if let Err(e) = crate::repo::convert_cache(&w, &p, u, g) {
            // A refused folder is refused again under the lock, where the save reports it. It is
            // not pruned either: a process writes to it.
            if e.downcast_ref::<crate::repo::CacheRefused>().is_none() {
                return Err(e);
            }
            continue;
        }
        // Pruned before the copy and the save: the state keeps the pruned cache (docs/cache-overlays.md, section 19).
        repo.prune_cache_folder(l, &p);
        #[cfg(target_os = "macos")]
        prepare_cache_copy(repo, l, &w, &p)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn prepare_cache_copy(repo: &Repo, l: &Line, w: &Path, p: &str) -> Result<()> {
    {
        let cp = fsutil::safe_join(w, p)?;
        if !btrfs::is_subvolume(&cp) {
            return Ok(());
        }
        let name = format!("{}@{}", l.head, p.replace('/', "%2F"));
        let copies = if repo.p.cache_path(&l.head, &name).exists() { 1 } else { 2 };
        for _ in 0..copies {
            btrfs::prepare_snapshot(&cp, &repo.p.work_dir(), true)?;
        }
    }
    Ok(())
}

/// Snapshot the caches at `paths` of a working folder (`w`) as the caches of `state` and record
/// them (`save.caches`, with their subvolume UUIDs). `shared`: they may go to other users' lines.
/// A cache that an earlier save already gave the state keeps its snapshot and is recorded again,
/// so the record names every cache of `paths` that the state has.
pub fn record_caches(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    w: &Path,
    paths: &[String],
    shared: bool,
) -> Result<BTreeMap<String, String>> {
    let (mut caches, mut made) = (BTreeMap::new(), Vec::new());
    // Any error of the loop, also after an earlier snapshot of this call, removes the snapshots
    // this call made: no record names them.
    let taken = (|| -> Result<()> {
        for p in paths {
            let cp = fsutil::safe_join(w, p)?;
            let name = format!("{state}@{}", p.replace('/', "%2F"));
            if !crate::model::is_cache_name(&name, state) {
                continue;
            }
            let snap = repo.p.cache_path(state, &name);
            if snap.exists() {
                caches.insert(p.clone(), name);
            } else if btrfs::is_subvolume(&cp) {
                btrfs::snapshot(&cp, &snap, true)?;
                made.push(snap);
                caches.insert(p.clone(), name);
            }
        }
        Ok(())
    })();
    let undo = |e: anyhow::Error| {
        for snap in &made {
            let _ = btrfs::delete_tree(snap);
        }
        e
    };
    taken.map_err(undo)?;
    // Nothing to record when the state already names each of them (a repeated save). A state
    // whose record lost some (an older layr replaced its caches) gets them back.
    let known = repo.state(state).map_err(undo)?.caches;
    if made.is_empty() && caches.iter().all(|(p, n)| known.get(p) == Some(n)) {
        return Ok(caches);
    }
    let mut uuids = serde_json::Map::new();
    for snap in caches.values() {
        let info = btrfs::subvol_info(&repo.p.cache_path(state, snap)).map_err(undo)?;
        uuids.insert(snap.clone(), serde_json::Value::String(info.uuid));
    }
    let mut rec = Record::new("save.layers");
    rec.data = serde_json::json!({"state": state, "layers": caches, "uuids": uuids, "shared": shared, "platform": crate::model::cache_platform()});
    repo.commit_record(ctx, rec).map_err(undo)?;
    Ok(caches)
}

/// Undo the newest operation of the given kinds on a line.
pub fn undo_last_of(ctx: &Ctx, repo: &Repo, line: &Line, ops: &[&str]) -> Result<i32> {
    for r in repo.p.db.records(500)? {
        if r.lines.contains_key(&line.id) && ops.contains(&r.op.as_str()) && !repo.p.db.undone(&r.id)? {
            return undo_record(ctx, repo, &r.id, false);
        }
    }
    Err(exit(1, format!("error: no {} to undo", ops.join(" or "))))
}

const NOT_UNDOABLE: &[&str] = &[
    "line.adopt",
    "machine.trust",
    "save.caches",
    "save.layers",
    "check",
    "attest",
    "gc",
    "machine",
    "project",
    "export",
    "import",
    "mirror",
    "slot",
    "group.create",
    "group.add",
    "group.accept",
    "merge.candidate",
    "approve",
    "approve.withdraw",
    "rebase.step",
    "remote.ref",
    "config",
    "overlay.create",
    "overlay.use",
];

/// True when `layr undo` reverses the operation. An automatic save is one only when it names the
/// working state it changes from (`working_before`); saves made before that have none.
fn undoable(r: &Record) -> bool {
    match r.op.as_str() {
        "save" => r.data.get("working_before").is_some(),
        op => !NOT_UNDOABLE.contains(&op),
    }
}

pub fn undo(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-f|--force").flag("-n|--dry-run");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let id = match a.pos.first() {
        Some(x) => find_record(repo, x)?,
        None => {
            // The newest operation of this caller (user and session label) in the project.
            let session = ctx.session();
            let mut found = None;
            for r in repo.p.db.records(5000)? {
                let local = r.lines.keys().all(|lid| {
                    repo.p.db.line_by_id(lid).ok().flatten().map(|l| repo.working(&l).exists()).unwrap_or(false)
                        || r.op == "line.delete"
                });
                if r.actor.uid == ctx.caller.uid
                    && r.actor.session == session
                    && local
                    && undoable(&r)
                    && !repo.p.db.undone(&r.id)?
                {
                    found = Some(r.id.clone());
                    break;
                }
            }
            found.ok_or_else(|| exit(1, "error: nothing to undo"))?
        }
    };
    if a.has("-n") {
        let r = repo.p.db.record(&id)?.unwrap();
        outln!(ctx, "would undo {} {} ({})", short_op(&r.id), operation_name(&r.op), fmt_time(r.time));
        return Ok(0);
    }
    undo_record(ctx, repo, &id, a.has("-f"))
}

fn find_record(repo: &Repo, prefix: &str) -> Result<String> {
    for r in repo.p.db.records(100000)? {
        if short_op(&r.id).starts_with(prefix) || r.id.starts_with(prefix) || ids::display(&r.id).starts_with(prefix) {
            return Ok(r.id);
        }
    }
    Err(exit(128, format!("fatal: no operation {prefix}")))
}

fn short_op(id: &str) -> String {
    ids::display(id)[..12].to_string()
}

fn fmt_time(ms: i64) -> String {
    super::history::fmt_date(ms, "iso")
}

pub fn undo_record(ctx: &Ctx, repo: &Repo, id: &str, force: bool) -> Result<i32> {
    let r = repo.p.db.record(id)?.ok_or_else(|| exit(128, "fatal: unknown operation"))?;
    if r.op == "export" {
        return Err(exit(1, "error: a push cannot be undone here: the state is on the remote. Run invert ('layr invert <state>' then 'layr git push')."));
    }
    if !undoable(&r) {
        return Err(exit(1, format!("error: operation '{}' has nothing to undo", operation_name(&r.op))));
    }
    // An operation with lines is undoable while its undo entry is in the undo.ops ring of its
    // lines (docs/ownership.md). Past the ring, its states may be freed: say so, do not guess.
    let ord = crate::owners::record_ord(&r);
    let entries = crate::owners::undo_entries(&r, &ord);
    if !entries.is_empty() {
        let mut in_ring = false;
        for (holder, _) in &entries {
            in_ring |= repo.p.db.holder_exists("undo", holder)?;
        }
        if !in_ring {
            let past = if r.op == "line.delete" {
                format!(
                    "the newest {} deleted lines (undo.deleted)",
                    repo.p.config_u64("undo.deleted", crate::owners::UNDO_DELETED_DEFAULT as u64)
                )
            } else {
                format!(
                    "the newest {} operations of its line (undo.ops)",
                    repo.p.config_u64("undo.ops", crate::owners::UNDO_OPS_DEFAULT as u64)
                )
            };
            return Err(exit(1, format!("error: cannot undo: operation {} is past {past}", ids::short(&r.id))));
        }
    }
    // The states that the undo reads stay on disk while it runs (every line's entry of the record).
    let named: Vec<String> =
        crate::owners::undo_entries(&r, &crate::owners::record_ord(&r)).into_iter().flat_map(|(_, v)| v).collect();
    let named: Vec<&str> = named.iter().map(String::as_str).collect();
    repo.hold(repo.pin(&named)?);
    if r.op == "save" {
        return undo_save(ctx, repo, &r, force);
    }
    // Permissions: undo changes every line the operation changed, so the caller must be allowed
    // to change each of them. An operation without lines (tags, groups) needs the admin or the
    // user who made it.
    for (lid, ch) in &r.lines {
        let owner = repo.p.db.line_by_id(lid)?.map(|l| l.owner).or(ch.before.as_ref().map(|b| b.owner)).unwrap_or(0);
        let probe = Line {
            id: lid.clone(),
            name: ch.before.as_ref().or(ch.after.as_ref()).map(|s| s.name.clone()).unwrap_or_default(),
            head: String::new(),
            owner,
            machine: String::new(),
        };
        repo.authorize_line(ctx, &probe, crate::repo::LineOp::Rewrite)?;
    }
    if r.lines.is_empty() && !repo.is_admin(ctx) && r.actor.uid != ctx.caller.uid {
        return Err(exit(1, "permission denied: only the project admin or the user who made it can undo this operation"));
    }
    let _queue_lock = if r.data.get("queue_change").is_some() { Some(super::queue::require_lock(repo)?) } else { None };
    // Before the lock: the automatic state of each line's folder and the trees that the undo
    // reads (seconds to minutes each under the lock). Errors only lose the preparation; the
    // states are loaded again under the lock, where an error stops the undo.
    for (lid, ch) in &r.lines {
        if let Ok(Some(l)) = repo.p.db.line_by_id(lid) {
            let _ = repo.prepare_save(&l, false);
        }
        for s in [&ch.working_before, &ch.working_after].into_iter().flatten() {
            let _ = repo.state_root(s);
        }
    }
    let r2 = undo_locked(ctx, repo, &r, force);
    repo.discard_prepared();
    r2
}

/// Undo an automatic save: the working folder goes back to the state that the save changed from.
/// The head, the stage and the paths that later work changed stay. The undo record keeps the head,
/// so a later undo of it redoes the save.
fn undo_save(ctx: &Ctx, repo: &Repo, r: &Record, force: bool) -> Result<i32> {
    let lid = r.data["line"].as_str().unwrap_or("").to_string();
    let head = r.data["head"].as_str().unwrap_or("").to_string();
    let before = r.data["working_before"].as_str().unwrap_or("").to_string();
    let after = r.data["working_after"].as_str().unwrap_or("").to_string();
    let probe = repo.p.db.line_by_id(&lid)?.ok_or_else(|| exit(1, "error: the line of this save no longer exists"))?;
    repo.authorize_line(ctx, &probe, crate::repo::LineOp::Rewrite)?;
    // The line lock covers the folder; the project lock covers the save before it and the record.
    let _line_lock = repo.line_lock(&probe)?;
    if repo.p.db.line_by_name(&probe.name)?.map(|l| l.id) != Some(lid.clone()) {
        return Err(exit(1, "error: the line of this save no longer exists"));
    }
    let line = repo.line(&lid)?;
    if !force && line.head != head {
        return Err(exit(
            1,
            "error: the line changed after this operation; undo the later operations first (layr op log), or use --force",
        ));
    }
    for s in [&before, &after] {
        repo.state_src(s).map_err(|e| exit(1, format!("error: cannot undo, no file changed: {e:#}")))?;
    }
    repo.prepare_save(&line, false)?;
    let wb = save_before_undo(ctx, repo, &line, false)?;
    if let Err(e) = revert_working(repo, &line, &before, &after, force) {
        super::work::put_back(repo, &line, wb.as_deref());
        return Err(e);
    }
    let mut rec = Record::new("undo");
    rec.data = serde_json::json!({"undo": r.id, "op": r.op, "keep_stage": true});
    let now = repo.line_state(&line, &line.head);
    rec.lines.insert(
        lid.clone(),
        LineChange { before: Some(now.clone()), after: Some(now), working_before: wb.clone(), working_after: Some(before) },
    );
    let head0 = line.head.clone();
    let done = match repo.commit_line(&line, &head0, |_| repo.commit_record(ctx, rec)) {
        Ok(done) => done,
        Err(e) => {
            super::work::put_back(repo, &line, wb.as_deref());
            return Err(e);
        }
    };
    crate::gitbridge::sync_store_access(repo)?;
    outln!(
        ctx,
        "Undid {} {} ({}); undo it with 'layr undo {}'",
        short_op(&r.id),
        operation_name(&r.op),
        fmt_time(r.time),
        short_op(&done.id)
    );
    Ok(0)
}

/// Revert the working changes of an operation (from `after` to `before`). Only what the operation
/// changed goes back: a path that later work changed keeps its content.
fn revert_working(repo: &Repo, line: &Line, before: &str, after: &str, force: bool) -> Result<()> {
    let cur = repo.scan(line)?;
    let later: BTreeSet<String> = changes::file_changes(&repo.state_src(after)?, &cur.path)?
        .into_iter()
        .flat_map(|c| [Some(c.path.clone()), c.old_path.clone()])
        .flatten()
        .collect();
    repo.apply_delta(&repo.state_root(after)?, &repo.state_root(before)?, &repo.working(line), repo.own(line), &later, force)?;
    Ok(())
}

/// True when the undo of `r` only moves heads back and writes the working folders of lines that
/// exist under their names (`undo_locked`). Such an undo takes the line locks for its writes and the
/// project lock for its checks and its record. Any other undo keeps the project lock for all of it.
fn undo_in_place(repo: &Repo, r: &Record) -> bool {
    const OPS: &[&str] = &["save", "commit", "reset", "merge", "cherry-pick", "revert", "rebase", "restore", "clean"];
    if !OPS.contains(&r.op.as_str()) || r.lines.is_empty() {
        return false;
    }
    if ["ref_change", "queue_change", "overlays"].iter().any(|k| r.data.get(*k).is_some()) {
        return false;
    }
    r.lines.iter().all(|(lid, ch)| {
        let Some(before) = &ch.before else { return false };
        match repo.p.db.line_by_id(lid) {
            Ok(Some(cur)) => {
                cur.name == before.name && repo.p.db.line_by_name(&cur.name).ok().flatten().is_some_and(|x| x.id == cur.id)
            }
            _ => false,
        }
    })
}

/// The automatic save of a line before its undo. An in-place undo takes the project lock for the
/// save alone (the copy of the folder is made before it); the caller holds no project lock then.
fn save_before_undo(ctx: &Ctx, repo: &Repo, line: &Line, in_place: bool) -> Result<Option<String>> {
    if !in_place {
        return repo.working_state(ctx, line, "before undo");
    }
    repo.prepare_save(line, false)?;
    let _commit = repo.p.lock()?;
    repo.working_state(ctx, line, "before undo")
}

fn undo_locked(ctx: &Ctx, repo: &Repo, r: &Record, force: bool) -> Result<i32> {
    let in_place = undo_in_place(repo, r);
    // The lines of the undo, in id order, for their folders. An in-place undo takes the project lock
    // for its checks and its record only; any other undo keeps the project lock for all of it.
    let mut ids: Vec<&String> = r.lines.keys().collect();
    ids.sort();
    let mut _line_locks = Vec::new();
    for lid in ids {
        if let Some(l) = repo.p.db.line_by_id(lid)? {
            _line_locks.push(repo.line_lock(&l)?);
        }
    }
    let _lock = if in_place { None } else { Some(repo.p.lock()?) };
    let mut rec = Record::new("undo");
    rec.data = serde_json::json!({"undo": r.id, "op": r.op});
    // An undo of an automatic save keeps the stage, and so does its undo.
    let keep_stage = r.data["keep_stage"].as_bool() == Some(true);
    if keep_stage {
        rec.data["keep_stage"] = serde_json::json!(true);
    }
    if let Some(value) = r.data.get("ref_change") {
        let ch: crate::model::RefChange = serde_json::from_value(value.clone())?;
        let current = repo.p.db.ref_value(&ch.name, ids::now_ms())?;
        if !force && current != ch.after {
            bail!("ref '{}' changed after this operation; undo the later operations first", ch.name);
        }
        if let Some(before) = &ch.before {
            repo.state_root(&before.target)?;
        }
        if r.data.get("queue_change").is_none() {
            super::refs::authorize_update(ctx, repo, &ch.name, current.as_ref(), ch.before.as_ref(), true)?;
        }
        let inverse = crate::model::RefChange { name: ch.name, before: current, after: ch.before };
        rec.data["ref_change"] = serde_json::to_value(inverse)?;
    }
    if let Some(value) = r.data.get("queue_change") {
        let change: crate::model::QueueChange = serde_json::from_value(value.clone())?;
        let current = repo.p.db.queue_entry(&change.id)?;
        if !force && current != change.after {
            bail!("queue entry '{}' changed after this operation", change.id);
        }
        if let Some(before) = &change.before {
            repo.state_root(&before.state)?;
        }
        rec.data["queue_change"] =
            serde_json::to_value(crate::model::QueueChange { id: change.id, before: current, after: change.before })?;
    }
    // A commit that changed overlays: their lines move back and the line shows their earlier
    // states again; the changes stay in the working folder, as for the shared set. The overlay
    // lines move only after every check of the undo passed.
    let mut overlay_back: Vec<(String, Line, String)> = Vec::new();
    let changed: Vec<String> = r
        .data
        .get("overlays")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if !changed.is_empty() {
        if r.op != "commit" {
            return Err(exit(
                1,
                format!("error: undo of a {} that changed overlays is not available yet", operation_name(&r.op)),
            ));
        }
        let lid = r.data["line"].as_str().unwrap_or("").to_string();
        let line = repo.line(&lid)?;
        let after: Vec<crate::overlay::Layer> = serde_json::from_value(r.data["stack"]["layers"].clone())?;
        if repo.layers(&line)? != after && !force {
            return Err(exit(
                1,
                "error: the overlays of the line changed after this state; undo the later operations first, or use --force",
            ));
        }
        let mut before = after.clone();
        for l in before.iter_mut().filter(|l| changed.contains(&l.overlay)) {
            let o = repo.overlay_repo(&l.overlay)?;
            let parent =
                o.parents(&l.state)?.first().cloned().ok_or_else(|| exit(1, "error: the overlay state has no parent"))?;
            let info = repo.need_overlay(&l.overlay)?;
            if let Some(ol) = o.p.db.line_by_name(&Repo::overlay_line_name(&info, &line.id, line.owner))? {
                if ol.head == l.state {
                    overlay_back.push((l.overlay.clone(), ol, parent.clone()));
                }
            }
            l.state = parent;
        }
        rec.data["line"] = serde_json::json!(lid);
        rec.data["stack"] = repo.public_stack_data(&lid, &before)?;
    }
    // Tags.
    match r.op.as_str() {
        "tag" => {
            rec.op = "tag.delete".into();
            rec.data = serde_json::json!({"name": r.data["name"], "state": r.data["state"], "undo": r.id});
        }
        "tag.delete" => {
            rec.op = "tag".into();
            rec.data = serde_json::json!({"name": r.data["name"], "state": r.data["state"], "undo": r.id});
        }
        "stash.drop" => {
            bail!(
                "a dropped park entry can be applied again with 'layr park apply {}'",
                r.data["state"].as_str().map(ids::short).unwrap_or_default()
            );
        }
        _ => {}
    }
    // Every check and every read of the undo runs before the first file changes: each line must
    // still be where the operation left it, and each state must load. A failure stops the undo
    // with no file changed; a state that a store remote holds comes back from it (`state_src`).
    let mut lines = Vec::new();
    for (lid, ch) in &r.lines {
        let cur = repo.p.db.line_by_id(lid)?;
        let cur_state = cur
            .as_ref()
            .filter(|_| repo.p.db.line_by_name(&cur.as_ref().unwrap().name).ok().flatten().is_some())
            .map(|l| repo.line_state(l, &l.head));
        let alive = cur
            .as_ref()
            .map(|l| repo.p.db.line_by_name(&l.name).ok().flatten().map(|x| x.id == l.id).unwrap_or(false))
            .unwrap_or(false);
        let cur_state = if alive { cur_state } else { None };
        if !force && cur_state.as_ref().map(|s| (&s.name, &s.head)) != ch.after.as_ref().map(|s| (&s.name, &s.head)) {
            return Err(exit(
                1,
                "error: the line changed after this operation; undo the later operations first (layr op log), or use --force",
            ));
        }
        lines.push((lid, ch, cur, alive, cur_state));
    }
    let mut needed: Vec<String> = Vec::new();
    for ch in r.lines.values() {
        needed.extend(
            [ch.before.as_ref().map(|b| b.head.clone()), ch.working_before.clone(), ch.working_after.clone()]
                .into_iter()
                .flatten(),
        );
    }
    needed.extend(["final_state", "staged_state"].iter().filter_map(|k| r.data.get(*k)?.as_str().map(str::to_string)));
    for s in &needed {
        repo.state_src(s).map_err(|e| {
            exit(
                1,
                format!(
                    "error: cannot undo, no file changed: {e:#} (the newest {} operations of a line can be undone)",
                    repo.p.config_u64("undo.ops", crate::owners::UNDO_OPS_DEFAULT as u64)
                ),
            )
        })?;
    }
    // The heads the checks saw: an in-place undo checks them again under the project lock.
    let seen: Vec<(String, Option<(String, String)>)> =
        lines.iter().map(|(lid, _, _, _, cs)| ((*lid).clone(), cs.as_ref().map(|s| (s.name.clone(), s.head.clone())))).collect();
    // The lines with their restored head: their .git folders follow it after the record is made.
    let mut restored: Vec<Line> = Vec::new();
    // In place: the stage of each line is dropped after the record, and each folder that was
    // written is saved (`wb`) to put it back if the record is refused.
    let mut stage_drops: Vec<Line> = Vec::new();
    let mut written: Vec<(Line, Option<String>)> = Vec::new();
    for (lid, ch, cur, alive, cur_state) in lines {
        let mut wb = None;
        match (&ch.before, &cur) {
            (Some(b), Some(l)) if alive => {
                // Save the current content, then restore the old pointers and content.
                wb = save_before_undo(ctx, repo, l, in_place)?;
                written.push((l.clone(), wb.clone()));
                if b.name != l.name && repo.p.moved_path(&l.id).is_none() {
                    crate::fsutil::rename_tree(repo.p.line_path(&l.name), repo.p.default_line_path(&b.name))?;
                }
                let mut l2 = l.clone();
                l2.name = b.name.clone();
                match (&ch.working_before, &ch.working_after) {
                    // Revert only what the operation changed; later work stays.
                    (Some(before), Some(after)) => revert_working(repo, &l2, before, after, force)?,
                    (Some(before), _) => restore_working(repo, &l2, before)?,
                    (None, _) => {}
                }
                if !keep_stage {
                    if in_place {
                        stage_drops.push(l2.clone());
                    } else {
                        repo.drop_stage(&l2)?;
                    }
                }
                let mut back = l2.clone();
                back.head = b.head.clone();
                restored.push(back);
            }
            (Some(b), _) => {
                // The line was deleted: make it again from its last content.
                let src = ch
                    .working_before
                    .clone()
                    .or(r.data.get("final_state").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .unwrap_or(b.head.clone());
                let w = repo.p.line_path(&b.name);
                if w.exists() {
                    bail!("{} exists", w.display());
                }
                let l = Line {
                    id: lid.clone(),
                    name: b.name.clone(),
                    head: b.head.clone(),
                    owner: b.owner,
                    machine: repo.p.store.machine.id.clone(),
                };
                repo.materialize(&l, &src, &w)?;
                restored.push(l.clone());
                if let Some(staged) = r.data.get("staged_state").and_then(|v| v.as_str()) {
                    // The index of the deleted line comes back with it.
                    repo.snapshot_state(staged, &repo.p.stage_path(&l.id), false)?;
                    repo.mark_stage(&l)?;
                }
            }
            (None, Some(l)) if alive => {
                // The operation created the line: delete it (content saved first).
                wb = save_before_undo(ctx, repo, l, in_place)?;
                repo.p.trash(&repo.working(l))?;
                repo.drop_stage(l)?;
            }
            _ => {}
        }
        rec.lines.insert(
            lid.clone(),
            LineChange {
                before: cur_state,
                after: ch.before.clone(),
                working_before: wb,
                working_after: ch.working_before.clone(),
            },
        );
    }
    for (name, ol, parent) in &overlay_back {
        let o = repo.overlay_repo(name)?;
        let _l = o.p.lock()?;
        crate::overlay::move_overlay_line(&o, ctx, "undo", ol, parent, Vec::new())?;
    }
    let done = if in_place {
        let states = rec.states.clone();
        let result = (|| -> Result<Record> {
            let _commit = repo.p.lock()?;
            // The heads that the checks saw must still be there: a command that does not take the
            // line lock (line delete, follow, fetch) may have moved one.
            for (lid, before) in &seen {
                let now = repo.p.db.line_by_id(lid)?;
                match (before, now) {
                    (Some((name, from)), Some(now)) if &now.head != from => {
                        return Err(crate::repo::line_moved(name, from, &now.head));
                    }
                    (Some((name, _)), None) => return Err(crate::repo::line_gone(name)),
                    _ => {}
                }
            }
            repo.commit_record(ctx, rec)
        })();
        match result {
            Ok(done) => done,
            Err(e) => {
                crate::repo::discard_unrecorded(repo, &states);
                for (line, wb) in &written {
                    super::work::put_back(repo, line, wb.as_deref());
                }
                return Err(e);
            }
        }
    } else {
        repo.commit_record(ctx, rec)?
    };
    for line in &stage_drops {
        repo.drop_stage(line)?;
    }
    // The .git folder of each restored line points at its restored head, as after every other move
    // of a head (repo::move_head). A failure only warns: the undo is done.
    for line in &restored {
        if let Err(e) = crate::gitbridge::refresh_git_folder(repo, line) {
            eprintln!("warning: .git folder of line {} not updated: {e:#}", line.name);
        }
    }
    crate::gitbridge::sync_store_access(repo)?;
    outln!(
        ctx,
        "Undid {} {} ({}); undo it with 'layr undo {}'",
        short_op(&r.id),
        operation_name(&r.op),
        fmt_time(r.time),
        short_op(&done.id)
    );
    Ok(0)
}

/// Make the working folder equal to a state (all paths, untracked and ignored included).
pub(crate) fn restore_working(repo: &Repo, line: &Line, state: &str) -> Result<()> {
    let target = repo.state_root(state)?;
    let scan = repo.scan(line)?;
    let w = repo.working(line);
    let own = repo.own(line);
    // Automatic states leave the overlay files out: those files stay as they are.
    let private = repo.private_paths(line)?;
    let list: Vec<_> = changes::file_changes(&scan.path, &target)?
        .into_iter()
        .filter(|c| {
            !(skip_path(&c.path)
                || private.contains(&c.path)
                || c.old_path.as_ref().map(|o| private.contains(o)).unwrap_or(false))
        })
        .collect();
    let gone: Vec<&str> =
        list.iter().flat_map(|c| c.old_path.as_deref().into_iter().chain(c.new.is_none().then_some(c.path.as_str()))).collect();
    let write: Vec<&str> = list.iter().filter(|c| c.new.is_some()).map(|c| c.path.as_str()).collect();
    fsutil::replace_work_entries(&target, &w, &gone, &write, own)
}

pub fn op(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("log");
    if sub == "export" {
        let h = here(ctx)?;
        h.repo.require(ctx, "records")?;
        ctx.print(&h.repo.p.export_jsonl()?);
        return Ok(0);
    }
    if sub == "import" {
        let h = here(ctx)?;
        h.repo.require(ctx, "records")?;
        let data = super::work::read_caller_file(ctx, args.get(1).map(|s| s.as_str()).unwrap_or("-"), 512 << 20)?;
        let lines: Vec<String> =
            String::from_utf8_lossy(&data).lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
        let _lock = h.repo.p.lock()?;
        let n = h.repo.p.import_lines(&lines, crate::store::Trust::Known)?;
        outln!(ctx, "imported {n} new record(s) of {}", lines.len());
        return Ok(0);
    }
    if sub != "log" && sub != "show" {
        return Err(exit(
            128,
            "usage: layr op log [-n N] [--all] | layr op show <id> | layr op export | layr op import [<file>]",
        ));
    }
    let spec = Spec::new().value("-n").flag("--all").flag("--json");
    let a = parse(&spec, args.get(1..).unwrap_or(&[]))?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if sub == "show" {
        let id = find_record(repo, a.pos.first().ok_or_else(|| exit(128, "usage: layr op show <id>"))?)?;
        let r = repo.p.db.record(&id)?.unwrap();
        outln!(ctx, "{}", serde_json::to_string_pretty(&r)?);
        return Ok(0);
    }
    let n: usize = a.get("-n").map(|v| v.parse()).transpose()?.unwrap_or(30);
    let line = h.line.clone();
    let mut shown = 0;
    for r in repo.p.db.records(100000)? {
        if !a.has("--all") {
            if let Some(l) = &line {
                if !matches!(r.op.as_str(), "push" | "sync")
                    && !r.lines.contains_key(&l.id)
                    && r.data.get("line").and_then(|v| v.as_str()) != Some(&l.id)
                {
                    continue;
                }
            }
        }
        if a.has("--json") {
            outln!(ctx, "{}", serde_json::to_string(&r)?);
        } else {
            let undone = if repo.p.db.undone(&r.id)? { " (undone)" } else { "" };
            let what = if matches!(r.op.as_str(), "push" | "sync") {
                format!(
                    " {}: states sent {}, received {}",
                    r.data["remote"].as_str().unwrap_or(""),
                    r.data["states_sent"],
                    r.data["states_received"]
                )
            } else {
                r.states
                    .first()
                    .map(|s| format!(" {} {}", ids::short(&s.id), s.message.lines().next().unwrap_or("")))
                    .unwrap_or_default()
            };
            let session = r.actor.session.as_ref().map(|a| format!(" [{a}]")).unwrap_or_default();
            outln!(
                ctx,
                "{} {} {}{} {}{}{}",
                short_op(&r.id),
                fmt_time(r.time),
                r.actor.name,
                session,
                operation_name(&r.op),
                what,
                undone
            );
        }
        shown += 1;
        if shown >= n {
            break;
        }
    }
    Ok(0)
}

/// Run a command in a temporary writable copy of an exact state and record the result.
pub fn try_cmd(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--keep").flag("-q|--quiet").value("--check");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.require(ctx, "try")?;
    const USAGE: &str = "usage: layr try [--keep] [-q] <rev> -- <command>...\n\
        \x20      layr try [--keep] [-q] <rev> --check <name>\n\
        Runs the command in a temporary copy of the state <rev> (with a .git folder at its commit) \
        and deletes the copy afterwards; your line does not change. --check runs the command of \
        'layr config check.<name>', as protected refs, merges and the queue run it.";
    let rev = a.pos.first().ok_or_else(|| exit(128, USAGE))?;
    let argv: Vec<String> = match a.get("--check") {
        Some(name) => {
            if !a.paths.is_empty() {
                return Err(exit(128, USAGE));
            }
            let command = repo
                .p
                .config_value(&format!("check.{name}"))?
                .and_then(|v| v.as_str().map(String::from))
                .ok_or_else(|| exit(128, format!("check '{name}' is not defined (layr config check.{name} '<command>')")))?;
            vec!["sh".into(), "-c".into(), command]
        }
        None => {
            if a.paths.is_empty() {
                return Err(exit(128, USAGE));
            }
            a.paths.clone()
        }
    };
    let view = {
        let _lock = repo.p.lock()?;
        let current = h.line.as_ref().map(|line| repo.line(&line.id)).transpose()?;
        let view = revs::resolve_view(repo, current.as_ref(), rev).map_err(|e| exit(128, format!("layr try: {e:#}")))?;
        repo.check_view_read(ctx, &view)?;
        if !view.layers.is_empty() {
            repo.save_view(ctx, &view)?;
        }
        view
    };
    // `try` is the caller's own command: it gets the caller's environment, except the variables
    // that would point git or layr at the caller's own folder instead of the copy.
    const TO_COPY: &[&str] =
        &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR", "LAYR_LINE"];
    let env: Vec<(String, String)> =
        ctx.env.iter().filter(|(k, _)| !TO_COPY.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    run_check_in(
        ctx,
        repo,
        &view.shared,
        &argv,
        &argv.join(" "),
        &env,
        a.has("-q"),
        a.has("--keep"),
        false,
        (!view.layers.is_empty()).then_some(&view),
    )
}

/// Use the `try` copy and environment. JSON output sends child output to standard error.
pub fn try_state(ctx: &Ctx, repo: &Repo, state: &str, argv: &[String], quiet: bool, json: bool) -> Result<i32> {
    try_state_in(ctx, repo, state, argv, quiet, false, json)
}

fn try_state_in(ctx: &Ctx, repo: &Repo, state: &str, argv: &[String], quiet: bool, keep: bool, json: bool) -> Result<i32> {
    const TO_COPY: &[&str] =
        &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR", "LAYR_LINE"];
    let env: Vec<(String, String)> =
        ctx.env.iter().filter(|(k, _)| !TO_COPY.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    run_check_in(ctx, repo, state, argv, &argv.join(" "), &env, quiet, keep, json, None)
}

/// Run a configured check as the caller in a temporary copy of `state` and record its exit code
/// as a check of that state (`command` is the text recorded). The check runs the code of the
/// state, which may come from another user: it gets a clean environment (no tokens or keys of
/// the caller), only HOME, USER, LOGNAME and PATH.
///
/// The copy starts with the warm caches of the state, but its build output is never kept: layr
/// cannot tell whether build output is valid for a state (docs/design.md, section 5).
pub fn run_check(ctx: &Ctx, repo: &Repo, state: &str, argv: &[String], command: &str, quiet: bool) -> Result<i32> {
    run_check_in(ctx, repo, state, argv, command, &[], quiet, false, false, None)
}

/// Run a configured check in an exact view, with the same clean environment as `run_check`.
/// This function takes the project lock itself; the caller must not hold it.
pub fn run_check_view(
    ctx: &Ctx,
    repo: &Repo,
    view: &crate::overlay::View,
    argv: &[String],
    command: &str,
    quiet: bool,
) -> Result<i32> {
    {
        let _lock = repo.p.lock()?;
        repo.require(ctx, "try")?;
        repo.check_view_collected(view)?;
        repo.check_view_read(ctx, view)?;
        if !view.layers.is_empty() {
            repo.save_view(ctx, view)?;
        }
    }
    run_check_in(ctx, repo, &view.shared, argv, command, &[], quiet, false, false, (!view.layers.is_empty()).then_some(view))
}

#[allow(clippy::too_many_arguments)]
fn run_check_in(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    argv: &[String],
    command: &str,
    env: &[(String, String)],
    quiet: bool,
    keep: bool,
    json: bool,
    view: Option<&crate::overlay::View>,
) -> Result<i32> {
    // The check runs outside the store lock on this state: a free step must not remove it meanwhile.
    let _pin = repo.pin(&[state])?;
    let layers = view.map(|v| v.layers.as_slice()).unwrap_or(&[]);
    with_try_copy(ctx, repo, state, layers, keep, |dir| {
        if !quiet {
            eprintln!("layr: running '{command}' in {} (state {})", dir.display(), ids::short(state));
        }
        let started = std::time::Instant::now();
        let mut env = env.to_vec();
        env.retain(|(k, _)| k != "LAYR_TRY_VIEW");
        if let Some(view) = view {
            env.push(("LAYR_TRY_VIEW".into(), view.id()?));
        }
        let code = try_command(ctx, repo, state, argv, dir, &env, json)?;
        let _lock = repo.p.lock()?;
        let result = serde_json::json!({"command":command,"argv":argv,"exit":code,"seconds":started.elapsed().as_secs_f64()});
        let mut rec = match view {
            Some(view) => super::attest::record_view(repo, view, "check", &serde_json::to_vec(&result)?)?,
            None => super::attest::record(repo, state, "check", &serde_json::to_vec(&result)?)?,
        };
        rec.data["check"] = result;
        repo.commit_record(ctx, rec)?;
        if !quiet {
            eprintln!("layr: state {} exit {code}", ids::short(state));
        }
        Ok(code)
    })
}

/// Create a private try copy, run the callback, then remove the copy unless requested otherwise.
pub fn with_try_copy<T>(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    layers: &[crate::overlay::Layer],
    keep: bool,
    run: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let name = format!(".try-{:016x}", rand::random::<u64>());
    let dir = repo.p.line_path(&name);
    let tmp_line = Line {
        id: format!("try-{name}"),
        name,
        head: state.into(),
        owner: ctx.caller.uid,
        machine: repo.p.store.machine.id.clone(),
    };
    if layers.is_empty() {
        repo.materialize(&tmp_line, state, &dir)?;
    } else {
        let view = crate::overlay::View::new(state.to_string(), layers.to_vec());
        repo.materialize_view(ctx, &tmp_line, &view, &dir)?;
    }
    let result = run(&dir);
    if keep {
        eprintln!("layr try: kept {}", dir.display());
    } else {
        let _ = btrfs::delete_tree(&dir);
    }
    result
}

/// Use the try identity and environment rules for commands from an exact tree.
pub fn try_command(
    ctx: &Ctx,
    repo: &Repo,
    state: &str,
    argv: &[String],
    dir: &Path,
    env: &[(String, String)],
    json: bool,
) -> Result<i32> {
    if argv.is_empty() || argv[0].is_empty() {
        anyhow::bail!("a command needs a nonempty argv");
    }
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(dir);
    if json {
        command.stdout(std::process::Stdio::from(rustix::io::dup(std::io::stderr())?));
    }
    let mut env = env.to_vec();
    env.push(("LAYR_TRY_STATE".into(), ids::display(state)));
    env.retain(|(k, _)| k != "LAYR_PROJECT");
    env.push(("LAYR_PROJECT".into(), repo.p.name.clone()));
    crate::privs::command_as(&mut command, ctx.caller.uid, &env)?;
    let status = command
        .status()
        .map_err(|e| exit(127, format!("layr try: cannot run '{}' as user {}: {e}", argv[0], ctx.caller.user)))?;
    Ok(status.code().unwrap_or(128))
}

/// The owners check (docs/ownership.md): the stored table against a replay of the records, and
/// the present states that no owner names (leaks). Pins are local and not compared.
pub struct OwnersCheck {
    pub missing: Vec<crate::owners::Row>,
    pub extra: Vec<crate::owners::Row>,
    /// Undo owners of deleted lines outside the deleted ring (`owners::stale_undo_rows`).
    pub stale: Vec<crate::owners::Row>,
    /// The present states that `gc --repair` frees: no row of the replay or a live pin names them.
    pub freed: Vec<String>,
}

impl OwnersCheck {
    fn clean(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.freed.is_empty()
    }
}

fn owners_check(repo: &Repo) -> Result<OwnersCheck> {
    let want = repo.p.db.recomputed_owners(&repo.p.store.machine.id)?;
    let (missing, extra) = repo.p.db.owners_diff(&want)?;
    let stale = repo.p.db.stale_undo_rows()?;
    let freed = repo.p.db.unowned_by(&kept_rows(repo, &want)?)?;
    Ok(OwnersCheck { missing, extra, stale, freed })
}

/// The owner rows that a repair keeps: the replay plus the pins of live processes.
fn kept_rows(repo: &Repo, want: &[crate::owners::Row]) -> Result<Vec<crate::owners::Row>> {
    let mut rows = want.to_vec();
    for (state, kind, holder) in repo.p.db.owner_rows()?.into_iter().filter(|r| r.1 == "pin") {
        if crate::free::holder_alive(&holder) {
            rows.push((state, kind, holder));
        }
    }
    Ok(rows)
}

/// The bytes that only the storage of these states holds (as `layr du` counts them): clones that
/// other states or lines share are not counted, so the number is what freeing them gives back
/// (blocks shared only among these states are left out, so it is a lower bound).
fn states_bytes(repo: &Repo, states: &[String]) -> u64 {
    states
        .iter()
        .flat_map(|s| [repo.p.state_path(s), repo.p.delta_path(s)])
        .filter(|p| p.exists())
        .filter_map(|p| crate::storage::space(&p).ok().map(|s| s.unique))
        .sum()
}

/// Run a read-only check in one read transaction (a snapshot in WAL mode). It takes no store
/// lock: a lock would empty the trash at its end, which a check must not do.
fn snapshot<T>(repo: &Repo, f: impl FnOnce() -> Result<T>) -> Result<T> {
    repo.p.db.conn.execute_batch("BEGIN")?;
    let v = f();
    let _ = repo.p.db.conn.execute_batch("COMMIT");
    v
}

/// Test hook: pause after plan N until the test creates go.N in the named directory.
fn test_plan_wait(variable: &str, attempt: usize) -> Result<()> {
    if let Some(dir) = std::env::var_os(variable).map(std::path::PathBuf::from) {
        std::fs::write(dir.join(format!("planned.{attempt}")), b"")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !dir.join(format!("go.{attempt}")).exists() {
            anyhow::ensure!(std::time::Instant::now() < deadline, "test hook: no go.{attempt}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Ok(())
}

/// `layr gc` (check): exit 1 when the table and the records differ, or a state has no owner.
/// The lines name what `layr gc --repair` does.
fn check(ctx: &Ctx, repo: &Repo, quiet: bool) -> Result<i32> {
    let c = snapshot(repo, || owners_check(repo))?;
    if !quiet {
        outln!(ctx, "owners: {} missing, {} extra", c.missing.len(), c.extra.len());
        for (s, k, h) in c.missing.iter().take(5) {
            outln!(ctx, "\tmissing: {} {k} {h}", ids::short(s));
        }
        for (s, k, h) in c.extra.iter().take(5) {
            outln!(ctx, "\textra: {} {k} {h}", ids::short(s));
        }
    }
    if !c.stale.is_empty() {
        let freed: BTreeSet<&String> = c.freed.iter().collect();
        let held: Vec<String> = c.stale.iter().map(|r| r.0.clone()).filter(|s| freed.contains(s)).collect();
        outln!(
            ctx,
            "stale undo: {} owner(s) of deleted lines outside undo.deleted, holding {} that `layr gc --repair` frees",
            c.stale.len(),
            human_size(states_bytes(repo, &held))
        );
    }
    if !c.freed.is_empty() {
        let bytes = states_bytes(repo, &c.freed);
        outln!(ctx, "leaks: {} state(s), {} would be freed by `layr gc --repair`", c.freed.len(), human_size(bytes));
    }
    if c.clean() {
        if !quiet {
            outln!(ctx, "owners ok");
        }
        return Ok(0);
    }
    Ok(1)
}

/// Eviction (`gc.evict on`, docs/ownership.md, "States that a store remote holds"): a state that a
/// store remote holds loses its local tree when no strong local owner keeps it. Its record stays;
/// a read fetches the tree back. A state that a cache overlay names, a state with caches, and
/// the bases of layers that stay are kept. Off by default: nothing is evicted then.
fn evict_stored(repo: &Repo) -> Result<Vec<String>> {
    if !repo.p.gc_evicts() {
        return Ok(vec![]);
    }
    let db = &repo.p.db;
    // A cache overlay names project states through its heads and records: they stay.
    let mut named: BTreeSet<String> = BTreeSet::new();
    for name in repo.overlay_names()? {
        let overlay = repo.overlay_repo(&name)?;
        for line in overlay.p.db.lines()? {
            named.insert(line.head);
        }
        for raw in overlay.p.db.all_lines()? {
            for token in raw.split(|c: char| !c.is_ascii_hexdigit()).filter(|t| t.len() == 32) {
                named.insert(token.to_string());
            }
        }
    }
    let mut st = db.conn.prepare(
        "SELECT s.id FROM states s WHERE s.present=1 AND s.deleted=0
         AND EXISTS (SELECT 1 FROM owners o WHERE o.state=s.id AND o.kind='store')
         AND NOT EXISTS (SELECT 1 FROM owners o WHERE o.state=s.id
            AND o.kind IN ('head','auto','undo','deleted','park','overlay','pin'))",
    )?;
    let rows: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    drop(st);
    let copies = crate::store_remote::store_copies(repo)?;
    let mut evict = Vec::new();
    for id in rows {
        let plain = db.state(&id)?.is_some_and(|s| s.caches.is_empty());
        // A copy counts only on the store the remote names now and readable here (`store_copies`),
        // and only a tree that the store format can restore exactly goes (`restorable`).
        if plain && !named.contains(&id) && copies.contains_key(&id) && crate::store_remote::restorable(repo, &id)? {
            evict.push(id);
        }
    }
    let gone = repo.p.delete_states(&evict);
    for id in &evict {
        if gone.contains(id) && !repo.p.has_state(id) {
            db.set_present(id, false)?;
        }
    }
    Ok(gone)
}

/// The periodic repair of a project (`service.rs`): it prints nothing.
pub fn gc_repair_quiet(ctx: &Ctx, repo: &Repo) -> Result<usize> {
    repair_project(ctx, repo, false, true, false)
}

/// Replace the owners table with the replay of the records (live pins kept) and mark the leaked
/// states dead. Returns the leaks. The caller holds the project lock.
/// `free` frees the leaked states too. Only an explicit `gc --repair` does: a state that a command
/// has recorded but not yet given an owner looks like a leak for a moment (a periodic pass would
/// free it under the command).
/// `diff` is `owners_diff` against the replay when the caller made it with no record added
/// since (the rows that records make are the same); `None` replays here.
pub fn repair_table(
    repo: &Repo,
    free: bool,
    diff: Option<(Vec<crate::owners::Row>, Vec<crate::owners::Row>)>,
) -> Result<Vec<String>> {
    let (missing, extra) = match diff {
        Some(d) => d,
        None => repo.p.db.owners_diff(&repo.p.db.recomputed_owners(&repo.p.store.machine.id)?)?,
    };
    repo.p.db.apply_owners_diff(&missing, &extra)?;
    for holder in repo.p.db.pin_holders()? {
        if !crate::free::holder_alive(&holder) {
            repo.p.db.release_holder(&holder)?;
        }
    }
    let leaks = repo.p.db.leaked_states()?;
    if free {
        repo.p.db.free_leaks(&leaks)?;
    }
    Ok(leaks)
}

/// `layr gc --repair`: the owners table becomes the replay (plus the live pins), the leaked
/// states are freed, expired refs are released and the side caches are trimmed. The storage of
/// the freed states moves after the lock (`free::finish_project`), then the trash is emptied.
/// With `dry`, nothing changes: it prints what it would free.
fn repair_project(ctx: &Ctx, repo: &Repo, dry: bool, quiet: bool, free: bool) -> Result<usize> {
    if dry {
        let c = snapshot(repo, || owners_check(repo))?;
        let queued: Vec<String> = repo.p.db.queued_frees()?;
        let bytes = states_bytes(repo, &c.freed);
        if !quiet {
            outln!(ctx, "would delete {} state(s), {}", c.freed.len() + queued.len(), human_size(bytes));
            for id in c.freed.iter().take(50) {
                outln!(ctx, "\t{}", ids::short(id));
            }
            let caches = excess_caches(repo)?;
            outln!(ctx, "would delete {} cache(s)", caches.len());
            for (_, name) in caches.iter().take(50) {
                outln!(ctx, "\t{name}");
            }
        }
        return Ok(c.freed.len());
    }
    // Plan outside the project lock. A changed record invalidates the entire plan: retry
    // without the lock rather than replaying records while other commands wait.
    let mut attempts = 0;
    let (leaks, indexes, compares, lru) = loop {
        attempts += 1;
        let (mark, diff) = {
            let _t = crate::trace::begin("repair: plan");
            snapshot(repo, || {
                let want = repo.p.db.recomputed_owners(&repo.p.store.machine.id)?;
                Ok((repo.p.db.records_mark()?, repo.p.db.owners_diff(&want)?))
            })?
        };
        let overlays = repo.overlay_plan();
        let indexes = crate::gitbridge::plan_prune_indexes(repo)?;
        let compares = repo.plan_prune_compares()?;
        let lru = repo.plan_cache_gc_lru()?;
        // Test hook (tests/t127-repair-plan.sh): after plan N, write `planned.N` in the folder and
        // wait for `go.N`, so a test changes the project between the plan and the lock.
        test_plan_wait("LAYR_TEST_REPAIR_WAIT", attempts)?;
        let _lock = repo.p.lock()?;
        if repo.p.db.records_mark()? != mark {
            if attempts < 3 {
                continue;
            }
            anyhow::bail!("project changed during the command; run it again");
        }
        let leaks = repair_table(repo, free, Some(diff))?;
        evict_stored(repo)?;
        // Keep one record per ref: ref_history keys entries by record id, and owners
        // replay synchronizes the single ref_change in each record. Batching requires
        // a compatible record format and replay/schema migration first.
        for expired in repo.p.db.expired_refs(ids::now_ms())? {
            let mut rec = Record::new("ref.expire");
            rec.data = serde_json::json!({"ref_change": crate::model::RefChange { name:expired.name.clone(),before:Some(expired),after:None }});
            repo.commit_record(ctx, rec)?;
        }
        // Cache overlay lines of deleted lines (their states go below), and overlays with a
        // history limit (docs/cache-overlays.md).
        if let Err(e) = repo.free_removed_overlays(ctx) {
            eprintln!("warning: removed cache overlays stay until the next gc: {e:#}");
        }
        match overlays {
            Ok(plan) => repo.commit_overlay_plan(ctx, plan),
            Err(e) => eprintln!("warning: overlays: old states stay until the next gc: {e:#}"),
        }
        #[cfg(target_os = "macos")]
        {
            // Upkeep walks thin-layer dependencies and can make keyframes of current
            // heads. Moving it out of this lock needs a dependency snapshot that is
            // checked against concurrent layer creation and head changes; leaving it
            // here preserves bases until that protocol can be verified on APFS.
            let lines = repo.lines()?;
            let heads: BTreeSet<String> = lines.iter().map(|l| l.head.clone()).collect();
            let done = crate::thin::upkeep(&repo.p, &heads)?;
            // Saved statuses of lines that are gone.
            let ids: BTreeSet<String> = lines.iter().map(|l| format!("{}.cache", l.id)).collect();
            if let Ok(rd) = std::fs::read_dir(repo.p.meta_dir().join("status")) {
                for e in rd.flatten() {
                    if !ids.contains(&e.file_name().to_string_lossy().into_owned()) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
            if done.views + done.keyframes + done.deleted > 0 {
                eprintln!(
                    "thin states: {} view(s) removed, {} complete tree(s) made, {} kept base(s) deleted",
                    done.views, done.keyframes, done.deleted
                );
            }
        }
        let indexes = crate::gitbridge::check_prune_indexes(repo, indexes)?;
        let compares = repo.check_prune_compares(compares)?;
        break (leaks, indexes, compares, lru);
    };
    crate::gitbridge::finish_prune_indexes(indexes);
    compares.finish();
    if let Err(e) = repo.finish_cache_gc_lru(lru) {
        eprintln!("warning: cache size limits not applied: {e:#}");
    }
    prune_retention(ctx, repo, true)?;
    // The storage of the freed states: the leaks above and states that died before this repair.
    let freed = crate::free::finish_project(&repo.p)?;
    crate::store::empty_trash(&repo.p.trash_dir()?, None);
    // Temporary objects whose creator is gone, and old ones without a creator pid. A live
    // creator keeps its object: a copy has the times of its source, so its age says nothing.
    for d in [repo.p.scan_dir(), repo.p.work_dir()] {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let stale = crate::store::creator_gone(&name).unwrap_or_else(|| {
                    e.metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(|t| t.elapsed().map(|x| x.as_secs() > 3600).unwrap_or(false))
                        .unwrap_or(false)
                });
                if stale {
                    let _ = repo.p.trash(&e.path());
                }
            }
        }
    }
    // OCI blobs that no export names (a failed export), and saved scans of removed or changed
    // trees. Every gc path runs them: the daemon, overlay projects and the command.
    if repo.p.meta_dir().join("oci").is_dir() {
        let age = std::time::Duration::from_secs(3600);
        if let Err(e) = crate::oci::Cache::for_repo(repo).and_then(|c| crate::oci::prune_blobs(&c, age)) {
            eprintln!("warning: unnamed OCI blobs stay until the next gc: {e:#}");
        }
    }
    #[cfg(target_os = "macos")]
    crate::storage::prune_saved_scans(&repo.p.dir)?;
    if !quiet {
        outln!(ctx, "freed {freed} state(s), {} leaked", leaks.len());
    }
    Ok(freed)
}

/// Scan retention candidates without the project lock, then move unchanged candidates into
/// trash under it. A changed record invalidates the entire plan; never rescan under the lock.
fn prune_retention(ctx: &Ctx, repo: &Repo, trim_caches: bool) -> Result<()> {
    let trash = repo.p.trash_dir()?;
    std::fs::create_dir_all(&trash)?;
    for attempt in 1..=3 {
        let (mark, overlays, caches, bases) = {
            let _t = crate::trace::begin("retention: plan");
            snapshot(repo, || {
                let mark = repo.p.db.records_mark()?;
                let overlays = repo.overlay_marks()?;
                let caches = if trim_caches { excess_caches(repo)? } else { Vec::new() };
                Ok((mark, overlays, caches, repo.plan_prune_bases()?))
            })?
        };
        let states: Vec<&str> = caches.iter().map(|(id, _)| id.as_str()).collect();
        let _pin = repo.pin(&states)?;
        let paths = cache_candidates(repo, caches);
        test_plan_wait("LAYR_TEST_RETENTION_WAIT", attempt)?;
        let _lock = repo.p.lock()?;
        if repo.p.db.records_mark()? != mark {
            continue;
        }
        // Hidden overlay heads and selections can change without a project record. Lock and
        // check those projects too, so a base selected above cannot become the current base.
        let Some(_overlay_locks) = repo.lock_overlays_checked(&overlays)? else {
            continue;
        };
        let removed = apply_cache_candidates(repo, paths, Some(&trash))?;
        if !removed.is_empty() {
            let mut rec = Record::new("gc");
            rec.data = serde_json::json!({"deleted": [], "caches": removed});
            repo.commit_record(ctx, rec)?;
        }
        repo.commit_prune_bases(&bases, &trash)?;
        return Ok(());
    }
    eprintln!("warning: retention inputs changed during three attempts; cleanup deferred until the next gc");
    Ok(())
}

/// Cache snapshots (build output) are large and only the newest ones warm new lines: they stay
/// for the heads of lines and for the newest `retention.caches` (5) states with caches. The
/// states themselves stay; only their cache snapshots go. Returns `(state, snapshot name)`.
pub fn excess_caches(repo: &Repo) -> Result<Vec<(String, String)>> {
    let keep_n = repo.p.config_u64("retention.caches", 5) as usize;
    let mut heads: BTreeSet<String> = repo.p.db.lines()?.into_iter().map(|l| l.head).collect();
    heads.extend(repo.p.db.refs("", ids::now_ms())?.into_iter().map(|r| r.target));
    let mut with_caches: Vec<(i64, String, Vec<String>)> = Vec::new();
    for (id, _) in repo.p.db.present_states()? {
        if let Some(s) = repo.p.db.state(&id)? {
            let snaps: Vec<String> = s.caches.values().filter(|n| repo.p.cache_path(&id, n).exists()).cloned().collect();
            if !snaps.is_empty() {
                with_caches.push((s.time, id, snaps));
            }
        }
    }
    with_caches.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut out = Vec::new();
    for (_, id, snaps) in with_caches.into_iter().skip(keep_n) {
        if !heads.contains(&id) {
            out.extend(snaps.into_iter().map(|n| (id.clone(), n)));
        }
    }
    Ok(out)
}

struct CacheCandidate {
    path: std::path::PathBuf,
    name: String,
    identity: (u64, u64, i64, i64),
}

fn cache_candidates(repo: &Repo, caches: Vec<(String, String)>) -> Vec<CacheCandidate> {
    use std::os::unix::fs::MetadataExt;
    caches
        .into_iter()
        .filter_map(|(id, name)| {
            let path = repo.p.cache_path(&id, &name);
            let m = std::fs::symlink_metadata(&path).ok()?;
            Some(CacheCandidate { path, name, identity: (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()) })
        })
        .collect()
}

/// The GC plan uses rename-only trash; callers already holding the lock retain the ordinary
/// trash policy, including its fallback. Both paths check the planned identity.
fn apply_cache_candidates(repo: &Repo, candidates: Vec<CacheCandidate>, trash: Option<&Path>) -> Result<Vec<String>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    for candidate in candidates {
        let Ok(m) = std::fs::symlink_metadata(&candidate.path) else { continue };
        if (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()) != candidate.identity {
            continue;
        }
        if let Some(dir) = trash {
            repo.p.move_to_trash(&candidate.path, dir)?;
        } else {
            repo.p.trash(&candidate.path)?;
        }
        out.push(candidate.name);
    }
    Ok(out)
}

/// Move the cache snapshots that retention lets go into the trash. Returns their names.
pub fn trash_excess_caches(repo: &Repo) -> Result<Vec<String>> {
    apply_cache_candidates(repo, cache_candidates(repo, excess_caches(repo)?), None)
}

/// `layr gc --all-projects`: the retention of every project of the store, for a run by hand. A
/// project that fails does not stop the others; the exit code is 1 then.
fn gc_all_projects(ctx: &Ctx, dry: bool, quiet: bool) -> Result<i32> {
    let mut code = 0;
    // A project on a volume that cannot be used is skipped: nothing is deleted for it.
    for (project, volume, reason) in ctx.store.layout()?.missing() {
        if !project.contains('%') {
            eprintln!("layr gc: skipped project {project} (volume '{volume}' is not usable): {reason}");
        }
    }
    for name in ctx.store.project_names()?.into_iter().filter(|n| !n.contains('%')) {
        let result = Repo::open(ctx, &name).and_then(|repo| {
            repo.require(ctx, "gc")?;
            repair_project(ctx, &repo, dry, true, false)
        });
        match result {
            Ok(freed) if !quiet => outln!(ctx, "{name}: {freed} state(s)"),
            Ok(_) => {}
            Err(e) => {
                eprintln!("layr gc ({name}): {e:#}");
                code = 1;
            }
        }
    }
    Ok(code)
}

pub fn gc(ctx: &Ctx, args: &[String]) -> Result<i32> {
    crate::store::EMPTY_TRASH_FULLY.store(true, std::sync::atomic::Ordering::SeqCst);
    if args.iter().any(|v| v == "--remote" || v.starts_with("--remote=")) {
        return crate::store_remote::command(ctx, "gc", args);
    }
    let spec = Spec::new().flag("-n|--dry-run").flag("-q|--quiet").flag("--repair").flag("--dedup").flag("--all-projects");
    let a = parse(&spec, args)?;
    if a.has("--all-projects") {
        let code = gc_all_projects(ctx, a.has("-n"), a.has("-q"))?;
        print_trash_errors(&ctx.store);
        return Ok(code);
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "gc")?;
    let code = if a.has("--repair") {
        repair_project(ctx, &h.repo, a.has("-n"), a.has("-q"), true)?;
        // Trees of imported states that no strong owner keeps: git builds them again on read.
        let built = repair_built(ctx, &h.repo, a.has("-n"))?;
        if !a.has("-q") {
            let verb = if a.has("-n") { "would free" } else { "freed" };
            outln!(ctx, "{verb} {} built Git state(s) (rebuilt on read)", built.len());
            for id in built.iter().take(50) {
                outln!(ctx, "\t{}", ids::short(id));
            }
        }
        0
    } else {
        check(ctx, &h.repo, a.has("-q"))?
    };
    leftovers(ctx, &h.repo, a.has("--repair"), a.has("-n"), a.has("-q"))?;
    // After the project lock is released: hashing and comparing cache files takes long.
    let shared = if a.has("--dedup") { Some(crate::dedup::run(&h.repo, a.has("-n"))?) } else { None };
    if !a.has("-q") {
        if let Some(r) = shared {
            let verb = if a.has("-n") { "would share" } else { "shared" };
            outln!(ctx, "{verb} {} of disk in {} cache file(s), {:.1} s", human_size(r.bytes), r.files, r.seconds);
        }
    }
    print_trash_errors(&ctx.store);
    Ok(code)
}

/// The folders that a cache swap left (`<cache>.layr-old-<id>`, `<cache>.layr-new-<id>`): `gc`
/// reports them with their size; `gc --repair` moves them to the trash, unless a process uses them
/// or the swap is not finished (the next save finishes it).
fn leftovers(ctx: &Ctx, repo: &Repo, repair: bool, dry: bool, quiet: bool) -> Result<()> {
    for l in repo.cache_leftovers()? {
        let size = human_size(l.bytes);
        let state = match &l.kept {
            Some(why) => format!("kept ({why})"),
            None if repair && dry => "would remove".to_string(),
            None if repair => {
                repo.p.trash(&l.path)?;
                "removed".to_string()
            }
            None => "leftover (layr gc --repair removes it)".to_string(),
        };
        if !quiet {
            outln!(ctx, "{state}: {}: {} ({size})", l.line, l.rel);
        }
    }
    Ok(())
}

/// Owner kinds that keep the tree of a state (docs/ownership.md, "Storage that a live state
/// needs"). `parent`, `derived`, `ref`, `tag`, `remote-ref` and `export` keep only the record.
/// `conflict` is kept too: a conflict needs the trees of its base, ours and theirs.
const TREE_OWNERS: [&str; 8] = ["head", "auto", "undo", "deleted", "park", "overlay", "pin", "conflict"];

/// `gc --repair`: frees the trees of imported states that no owner keeps, and that git can build
/// again (`gitbridge::build_tree`). A dry run lists them and frees nothing.
fn repair_built(ctx: &Ctx, repo: &Repo, dry: bool) -> Result<Vec<String>> {
    let freed = {
        let _lock = (!dry).then(|| repo.p.lock()).transpose()?;
        drop_built_imports(repo, dry)?
    };
    if !dry && !freed.is_empty() {
        prune_retention(ctx, repo, false)?;
    }
    Ok(freed)
}

/// Rounds of freeing: a round frees the states that no owner keeps and no present layer uses as
/// its base. A base is freed in a later round, after its layers went.
fn drop_built_imports(repo: &Repo, dry: bool) -> Result<Vec<String>> {
    let mut freed = Vec::new();
    loop {
        let kept: BTreeSet<String> = repo
            .p
            .db
            .owner_rows()?
            .into_iter()
            .filter(|(_, kind, _)| TREE_OWNERS.contains(&kind.as_str()))
            .map(|(state, _, _)| state)
            .collect();
        let bases: BTreeSet<String> = repo.p.db.present_layer_bases()?.into_iter().collect();
        let candidates: Vec<String> = repo
            .p
            .db
            .present_states()?
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| !kept.contains(id) && !bases.contains(id))
            .collect();
        let round: Vec<String> = crate::gitbridge::buildable_set(repo, &candidates)?.into_iter().collect();
        if round.is_empty() {
            break;
        }
        if dry {
            freed.extend(round);
            break;
        }
        repo.p.delete_states(&round);
        let mut gone = Vec::new();
        for id in round {
            if !repo.p.has_state(&id) {
                repo.p.db.set_present(&id, false)?;
                gone.push(id);
            }
        }
        if gone.is_empty() {
            break;
        }
        freed.extend(gone);
    }
    Ok(freed)
}

/// Print the trash entries that the last emptying could not delete (`<store>/trash.errors`).
fn print_trash_errors(store: &crate::store::Store) {
    // Each volume keeps its own report beside its trash.
    let mut roots = vec![store.root.clone()];
    roots.extend(store.layout().map(|l| l.ready_volume_paths()).unwrap_or_default());
    for root in roots {
        let path = root.join("trash.errors");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if text.is_empty() {
            continue;
        }
        eprintln!("warning: {} trash entries could not be deleted ({}):", text.lines().count(), path.display());
        for line in text.lines() {
            eprintln!("\t{line}");
        }
    }
}

pub fn fsck(ctx: &Ctx, args: &[String]) -> Result<i32> {
    if args.iter().any(|v| v == "--remote" || v.starts_with("--remote=")) {
        return crate::store_remote::command(ctx, "fsck", args);
    }
    let spec = Spec::new().flag("--repair");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    repo.require(ctx, "fsck")?;
    let _lock = repo.p.lock()?;
    let mut problems = 0;
    let ic: String = repo.p.db.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if ic != "ok" {
        problems += 1;
        outln!(ctx, "database: {ic}");
    }
    match repo.p.verify_records() {
        Ok(v) => {
            for (m, n) in v {
                outln!(ctx, "records of machine {}: {n}, signatures ok", &ids::display(&m)[..8]);
            }
        }
        Err(err) => {
            problems += 1;
            outln!(ctx, "records: {err:#}");
        }
    }
    // Owners (docs/ownership.md): the table must equal the replay of the records, and every
    // present state needs an owner. The same check as `layr gc`.
    let c = owners_check(repo)?;
    if !c.clean() {
        problems += 1;
        outln!(
            ctx,
            "owners differ from the records: {} missing, {} extra, {} leaked",
            c.missing.len(),
            c.extra.len(),
            c.freed.len()
        );
        for (s, k, h) in c.missing.iter().take(5) {
            outln!(ctx, "\tmissing: {} {k} {h}", ids::short(s));
        }
        for (s, k, h) in c.extra.iter().take(5) {
            outln!(ctx, "\textra: {} {k} {h}", ids::short(s));
        }
        if a.has("--repair") {
            repair_table(repo, true, None)?;
            outln!(ctx, "owners rewritten from the records; leaked states marked for freeing");
        }
    }
    // Snapshots without a record: a save that stopped between its snapshot and its record.
    for e in std::fs::read_dir(repo.p.states_dir())?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let id = name.split('@').next().unwrap_or(&name).to_string();
        match repo.p.db.state(&id)? {
            None => {
                problems += 1;
                if a.has("--repair") {
                    btrfs::delete_tree(&e.path())?;
                    outln!(ctx, "deleted snapshot without record: {name}");
                } else {
                    outln!(ctx, "snapshot without record (leftover of an interrupted save): {name} (use --repair)");
                }
            }
            // A cache snapshot that its state does not name: a save stopped before its record.
            Some(s) if name.contains('@') && !s.caches.values().any(|n| *n == name) => {
                problems += 1;
                if a.has("--repair") {
                    btrfs::delete_tree(&e.path())?;
                    outln!(ctx, "deleted cache snapshot without record: {name}");
                } else {
                    outln!(ctx, "cache snapshot without record (leftover of an interrupted save): {name} (use --repair)");
                }
            }
            Some(_) => {}
        }
    }
    let mut st = repo.p.db.conn.prepare("SELECT id FROM states WHERE present=1")?;
    let ids_v: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    drop(st);
    for id in ids_v {
        if !repo.p.has_state(&id) {
            problems += 1;
            let deleted = repo.p.db.state(&id)?.and_then(|s| s.line).is_some_and(|l| repo.p.db.line_deleted(&l).unwrap_or(false));
            let note = if deleted { " (of a deleted line)" } else { "" };
            outln!(ctx, "state {} is marked present but missing{note}", ids::short(&id));
            if a.has("--repair") {
                repo.p.db.set_present(&id, false)?;
            }
        }
    }
    for l in repo.lines()? {
        if l.machine == repo.p.store.machine.id && !repo.working(&l).exists() {
            problems += 1;
            outln!(ctx, "line {} has no working folder (layr adopt {})", l.name, l.name);
        }
    }
    if !repo.p.is_overlay() {
        problems += fsck_overlays(ctx, repo, a.has("--repair"))?;
    }
    outln!(ctx, "{problems} problem(s)");
    Ok(if problems > 0 { 1 } else { 0 })
}

/// The overlays of a project (their records, states and snapshots) and the lines that show them
/// (each layer exists; a kept base equals its layers).
fn fsck_overlays(ctx: &Ctx, repo: &Repo, repair: bool) -> Result<usize> {
    let mut problems = 0;
    for name in repo.visible_overlay_names(ctx)? {
        // A machine that is not a reader of an overlay never receives its project.
        if !repo.p.store.project_dir(&format!("{}%{name}", repo.p.name))?.exists() {
            outln!(ctx, "overlay {name}: not on this machine (no key)");
            continue;
        }
        let o = match repo.overlay_repo(&name) {
            Ok(o) => o,
            Err(e) => {
                problems += 1;
                outln!(ctx, "overlay {name}: {e:#}");
                continue;
            }
        };
        match o.p.verify_records() {
            Ok(v) => {
                let n: u64 = v.iter().map(|x| x.1).sum();
                outln!(ctx, "overlay {name}: {n} records, signatures ok");
            }
            Err(err) => {
                problems += 1;
                outln!(ctx, "overlay {name}: records: {err:#}");
            }
        }
        for e in std::fs::read_dir(o.p.states_dir())?.flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            if o.p.db.state(&id)?.is_none() {
                problems += 1;
                if repair {
                    btrfs::delete_tree(&e.path())?;
                    outln!(ctx, "overlay {name}: deleted snapshot without record: {id}");
                } else {
                    outln!(ctx, "overlay {name}: snapshot without record: {id} (use --repair)");
                }
            }
        }
        for l in o.lines()? {
            if !o.p.has_state(&l.head) {
                problems += 1;
                let deleted = match l.name.strip_prefix("line-") {
                    Some(id) => repo.p.db.line_deleted(id)?,
                    None => false,
                };
                let note = if deleted { " (of a deleted line)" } else { "" };
                outln!(ctx, "overlay {name}: the head of line {}{note} is missing", l.name);
            }
        }
    }
    // Cache overlay lines of deleted lines go (also when their heads exist), as at gc.
    if repair {
        for (name, line) in repo.drop_orphan_cache_lines(ctx)? {
            outln!(ctx, "overlay {name}: deleted line {line} (of a deleted line)");
        }
    }
    for l in repo.lines()? {
        let layers = repo.layers(&l)?;
        if layers.iter().any(|layer| {
            repo.need_overlay(&layer.overlay).is_ok_and(|info| info.hidden)
                && !repo.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
        }) {
            continue;
        }
        if layers.is_empty() || l.machine != repo.p.store.machine.id {
            continue;
        }
        let mut ok = true;
        for x in &layers {
            if repo.overlay_repo(&x.overlay).map(|o| o.p.has_state(&x.state)).unwrap_or(false) {
                continue;
            }
            ok = false;
            problems += 1;
            outln!(ctx, "line {} shows state {} of overlay {}, which is missing", l.name, ids::short(&x.state), x.overlay);
        }
        if !ok {
            continue;
        }
        // The kept base against a new assembly of the same layers (a new line's base has empty
        // cache folders where the state may have files).
        let base = repo.head_tree(&l)?;
        let fresh = repo.assemble(&l, &l.head, &layers)?;
        let caches = repo.cache_paths(&repo.working(&l))?;
        let in_cache = |p: &str| caches.iter().any(|c| p == c || p.starts_with(&format!("{c}/")));
        let diff: Vec<String> = changes::file_changes(&base, &fresh.path)?
            .into_iter()
            .filter(|c| !skip_path(&c.path) && !in_cache(&c.path))
            .map(|c| c.path)
            .collect();
        if !diff.is_empty() {
            problems += 1;
            outln!(ctx, "line {}: its base differs from its layers in {} path(s)", l.name, diff.len());
            if repair {
                repo.prune_bases(&l, None, 0)?;
            }
        }
    }
    Ok(problems)
}

pub fn reindex(ctx: &Ctx, _args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    h.repo.require(ctx, "fsck")?;
    let _lock = h.repo.p.lock()?;
    h.repo.p.rebuild_index()?;
    // Layer bases exist on APFS only: elsewhere the table stays empty.
    #[cfg(target_os = "macos")]
    h.repo.p.db.replace_bases(&crate::thin::bases(&h.repo.p)?)?;
    let n: i64 = h.repo.p.db.conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    outln!(ctx, "index rebuilt from {n} records");
    if !h.repo.p.is_overlay() {
        for name in h.repo.visible_overlay_names(ctx)? {
            // An overlay without a key on this machine has no local project to index.
            let Ok(o) = h.repo.overlay_project(&name) else { continue };
            let _l = o.lock()?;
            o.rebuild_index()?;
            let n: i64 = o.db.conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
            outln!(ctx, "overlay {name}: index rebuilt from {n} records");
        }
    }
    Ok(0)
}

fn copy_folder(
    ctx: &Ctx,
    src: &std::path::Path,
    dst: &std::path::Path,
    include_ignored: bool,
    tracked: Option<&BTreeSet<String>>,
) -> Result<usize> {
    let rules = crate::ignore_rules::Rules::new(src);
    let mut n = 0;
    let mut entries = Vec::new();
    crate::privs::as_caller_fs(ctx, || {
        fsutil::walk(src, "", &mut |rel, m| {
            if rel == ".git" || rel.starts_with(".git/") {
                return Ok(false);
            }
            if !include_ignored && rules.ignored(rel, m.is_dir()) && !tracked.is_some_and(|paths| paths.contains(rel)) {
                return Ok(false);
            }
            entries.push((rel.to_string(), m.clone()));
            Ok(true)
        })
    })?;
    // On a store volume that ignores case, README and Readme would become one file.
    fsutil::refuse_case_collisions(dst, entries.iter().map(|(rel, _)| rel.as_str()))?;
    // Names of one file stay names of one file: the first is copied, the others link to it.
    let mut first: std::collections::HashMap<(u64, u64), String> = Default::default();
    for (rel, m) in &entries {
        if m.is_dir() {
            fsutil::make_parents(dst, &format!("{rel}/x"), None)?;
            continue;
        }
        let link = if m.is_file() {
            use std::os::unix::fs::MetadataExt;
            let lm = crate::privs::as_caller_fs(ctx, || Ok(std::fs::symlink_metadata(fsutil::safe_join(src, &rel)?)?))?;
            (lm.nlink() > 1).then(|| (lm.dev(), lm.ino()))
        } else {
            None
        };
        match link.and_then(|k| first.get(&k).map(|f| (k, f.clone()))) {
            Some((_, f)) => {
                fsutil::make_parents(dst, &rel, None)?;
                std::fs::hard_link(fsutil::safe_join(dst, &f)?, fsutil::safe_join(dst, &rel)?)?;
            }
            None => {
                copy_from_caller(ctx, src, rel, dst)?;
                if let Some(k) = link {
                    first.insert(k, rel.clone());
                }
            }
        }
        n += 1;
    }
    // Set directory modes after their children, so a restrictive mode cannot block the copy.
    for (rel, m) in entries.iter().rev().filter(|(_, m)| m.is_dir()) {
        fsutil::set_mode(dst, rel, m.mode)?;
    }
    Ok(n)
}

/// Copy one file or link of the caller's folder into the store: read with the caller's rights,
/// write with ours. The daemon runs as root, and the store folders are closed to the caller.
fn copy_from_caller(ctx: &Ctx, src: &std::path::Path, rel: &str, dst: &std::path::Path) -> Result<()> {
    let path = fsutil::safe_join(src, rel)?;
    let m = crate::privs::as_caller_fs(ctx, || Ok(std::fs::symlink_metadata(&path)?))?;
    if m.file_type().is_symlink() {
        let target = crate::privs::as_caller_fs(ctx, || Ok(std::fs::read_link(&path)?))?;
        return fsutil::put_symlink(dst, rel, &target);
    }
    let file = crate::privs::as_caller_fs(ctx, || fsutil::open_regular(&path))?;
    fsutil::copy_open_file(&file, dst, rel)
}

fn caller_git(ctx: &Ctx, dir: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    let mut command = std::process::Command::new("git");
    command.arg("-c").arg("safe.directory=*").arg("-C").arg(dir).args(args);
    crate::privs::command_as(&mut command, ctx.caller.uid, &[])?;
    Ok(command.output()?)
}

pub fn init(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .value("--from")
        .value("--git")
        .value("-b|--branch")
        .value("--line")
        .value("--owner")
        .value("--volume")
        .flag("--include-ignored")
        .value("-m|--message")
        .flag("-q|--quiet");
    let a = parse(&spec, args)?;
    let name = a
        .pos
        .first()
        .ok_or_else(|| exit(128, "usage: layr init <project> [--from <folder> | --git <url>] [--line main] [--volume <name>]"))?;
    let line_name = a.get("--line").unwrap_or("main").to_string();
    let owner = match a.get("--owner") {
        Some(o) if ctx.caller.is_root() => o.parse::<u32>().or_else(|_| -> Result<u32> {
            match crate::sys::user_by_name(o) {
                Some(u) => Ok(u.uid),
                None => {
                    bail!("unknown user {o}");
                }
            }
        })?,
        Some(_) => return Err(exit(128, "fatal: only root can create a project for another user")),
        None => ctx.caller.uid,
    };
    let actor = ctx.actor();
    // A missing source is an error, not an empty project. Checked before the project folder exists.
    if let Some(from) = a.get("--from") {
        let src = if from.starts_with('/') { std::path::PathBuf::from(from) } else { ctx.cwd.join(from) };
        if !crate::privs::as_caller_fs(ctx, || Ok(src.is_dir()))? {
            return Err(exit(128, format!("fatal: not a folder: {}", src.display())));
        }
    }
    // Project::create makes the folder or fails without changing an existing one. From here on,
    // the failure cleanup below removes the folder that this call made.
    let p = Project::create_on(
        &ctx.store,
        name,
        a.get("--volume"),
        &actor,
        serde_json::json!({"access": {format!("user:{owner}"): "admin"}, "protect": {"main": {}}}),
    )?;
    let dir = p.dir.clone();
    let repo = Repo { p, ctx: Some(ctx) };
    // The last step of every path: the line of the new state. Runs under the project lock.
    let finish = |state: String| -> Result<(Line, String)> {
        let l = repo.create_line(ctx, &line_name, &state, owner)?;
        Ok((l, state))
    };
    let res = (|| -> Result<(Line, String)> {
        let _lock = repo.p.lock()?;
        let work = repo.p.work_dir().join(crate::store::temp_name());
        btrfs::create_subvolume(&work)?;
        let cleanup = crate::repo::Temp { path: work.clone() };
        let mut parents = Vec::new();
        let mut msg = a.get("-m").unwrap_or("initial state").to_string();
        if let Some(url) = a.get("--git") {
            set_config(ctx, &repo, "remote.origin.url", serde_json::json!(url))?;
            let refs = crate::gitbridge::fetch(ctx, &repo, "origin")?;
            let branch = a.get("-b").map(|s| s.to_string()).or_else(|| default_branch(&repo)).unwrap_or_else(|| "main".into());
            let state = refs
                .iter()
                .find(|(b, _)| b == &format!("origin/{branch}") || b.ends_with(&format!("/{branch}")))
                .map(|x| x.1.clone());
            let state = state.ok_or_else(|| exit(128, format!("fatal: remote has no branch '{branch}'")))?;
            let (st, _) = crate::gitbridge::import_commit(ctx, &repo, &state, &format!("origin/{branch}"), owner)?;
            // The line starts at this commit: its tree is built now (the others are built on read).
            crate::gitbridge::build_tree(&repo, &st, None)?;
            drop(cleanup);
            return finish(st);
        }
        if let Some(from) = a.get("--from") {
            let src = if from.starts_with('/') { std::path::PathBuf::from(from) } else { ctx.cwd.join(from) };
            if src.join(".git").exists() {
                // Keep the git history: the store gets the folder's objects; the state of the
                // folder's HEAD is the parent of the folder content.
                let s = crate::gitbridge::init_store(&repo)?;
                let url = caller_git(ctx, &src, &["config", "--get", "remote.origin.url"])
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty());
                set_config(ctx, &repo, "remote.local.url", serde_json::json!(src.to_string_lossy()))?;
                if let Some(u) = &url {
                    set_config(ctx, &repo, "remote.origin.url", serde_json::json!(u))?;
                }
                let refs = crate::gitbridge::fetch(ctx, &repo, "local")?;
                let head = caller_git(ctx, &src, &["rev-parse", "HEAD"])?;
                let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
                let branch = caller_git(ctx, &src, &["rev-parse", "--abbrev-ref", "HEAD"])?;
                let branch = String::from_utf8_lossy(&branch.stdout).trim().to_string();
                let _ = (s, refs);
                if !head.is_empty() && head != "HEAD" {
                    // Keep force-added paths and their parents even when ignore rules match.
                    let tracked = caller_git(ctx, &src, &["ls-tree", "-r", "-t", "-z", "--name-only", &head])?;
                    if !tracked.status.success() {
                        bail!("cannot list source HEAD paths: {}", String::from_utf8_lossy(&tracked.stderr));
                    }
                    let tracked: BTreeSet<String> = tracked
                        .stdout
                        .split(|b| *b == 0)
                        .filter(|p| !p.is_empty())
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .collect();
                    // Copy as the caller, then freeze the bytes used by both the HEAD import
                    // and the folder content. A source edit cannot race the blob comparison.
                    let tmp = repo.p.work_dir().join(crate::store::temp_name());
                    btrfs::create_subvolume(&tmp)?;
                    let _t2 = crate::repo::Temp { path: tmp.clone() };
                    copy_folder(ctx, &src, &tmp, a.has("--include-ignored"), Some(&tracked))?;
                    let snap = repo.temp_snapshot(&tmp, true)?;
                    let (st, _) = crate::gitbridge::import_commit(ctx, &repo, &head, &format!("local/{branch}"), owner)?;
                    // The line starts at the HEAD commit: its tree is built now, from the folder copy.
                    crate::gitbridge::build_tree(&repo, &st, Some(&snap.path))?;
                    parents.push(st.clone());
                    // The folder content on top (unsaved changes, if any).
                    btrfs::delete_tree(&work)?;
                    repo.snapshot_state(&st, &work, false)?;
                    let imported = repo.state_root(&st)?;
                    let diffs = changes::file_changes(&imported, &snap.path);
                    // Different file systems give no common lineage: compare by walking instead.
                    let _ = diffs;
                    let mut changed = false;
                    let mut seen = BTreeSet::new();
                    let mut files = Vec::new();
                    fsutil::walk(&snap.path, "", &mut |rel, m| {
                        seen.insert(rel.to_string());
                        files.push((rel.to_string(), m.mode, m.is_dir()));
                        Ok(true)
                    })?;
                    let mut gone = Vec::new();
                    fsutil::walk(&imported, "", &mut |rel, _| {
                        if !skip_path(rel) && !seen.contains(rel) {
                            gone.push(rel.to_string());
                        }
                        Ok(true)
                    })?;
                    let mut copies = Vec::new();
                    let mut directories = Vec::new();
                    for (rel, mode, is_dir) in &files {
                        if !fsutil::same_entry(&imported, rel, &snap.path, rel, None)?
                            || fsutil::lmeta(&imported, rel).map(|x| x.mode != *mode).unwrap_or(true)
                        {
                            if *is_dir {
                                directories.push(rel.clone());
                            } else {
                                copies.push(rel.clone());
                            }
                        }
                    }
                    // This private tree has no user file handles to preserve. Remove replaced
                    // entries too, so APFS can clone the current bytes into new inodes.
                    let replaced_directories: Vec<&String> =
                        directories.iter().filter(|p| fsutil::lmeta(&imported, p).is_some_and(|m| !m.is_dir())).collect();
                    let gone_paths: Vec<&str> =
                        gone.iter().chain(&copies).chain(replaced_directories).map(String::as_str).collect();
                    // Apply directory modes last, from children to parents. Existing directories
                    // keep unchanged children when only their mode differs.
                    let copy_paths: Vec<&str> = copies.iter().chain(directories.iter().rev()).map(String::as_str).collect();
                    fsutil::replace_entries(&snap.path, &work, &gone_paths, &copy_paths, None)?;
                    changed |= !copies.is_empty() || !directories.is_empty() || !gone.is_empty();
                    if !changed {
                        drop(cleanup);
                        return finish(st);
                    }
                    msg = "unsaved changes of the imported folder".into();
                } else {
                    copy_folder(ctx, &src, &work, a.has("--include-ignored"), None)?;
                }
            } else {
                copy_folder(ctx, &src, &work, a.has("--include-ignored"), None)?;
            }
        }
        let st = repo.new_state(ctx, &work, StateKind::Commit, parents, &msg, None)?;
        let mut rec = Record::new("commit");
        rec.states.push(st.clone());
        repo.commit_record(ctx, rec)?;
        drop(cleanup);
        finish(st.id)
    })();
    let (l, state) = match res {
        Ok(x) => x,
        Err(e) => {
            // Close the database first: an open file keeps its overlay mount busy (OverlayFS).
            drop(repo);
            // Remove the folder this call made, with its subvolumes. The error of the step is kept.
            if let Err(c) = btrfs::delete_tree(&dir) {
                return Err(e.context(format!("cannot remove {}: {c:#}", dir.display())));
            }
            return Err(e);
        }
    };
    if !a.has("-q") {
        outln!(
            ctx,
            "Initialized project {name}: line {} at {} (state {})",
            l.name,
            repo.working(&l).display(),
            ids::short(&state)
        );
    } else {
        outln!(ctx, "{}", repo.working(&l).display());
    }
    Ok(0)
}

fn default_branch(repo: &Repo) -> Option<String> {
    let s = crate::gitbridge::store(repo);
    let out = crate::gitbridge::git_out(&s, &["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin/"]).ok()?;
    let names: Vec<&str> = out.lines().collect();
    for want in ["main", "master", "trunk"] {
        if names.contains(&want) {
            return Some(want.to_string());
        }
    }
    names.first().map(|s| s.to_string())
}

/// Settings that the owners model replaced (docs/ownership.md): the replacement is named.
pub const REMOVED_KEYS: &[(&str, &str)] = &[
    ("undo.days", "undo.ops (records per line, default 20)"),
    ("retention.hours", "retention.auto (automatic states per line, default 10)"),
];

pub fn set_config(ctx: &Ctx, repo: &Repo, key: &str, value: serde_json::Value) -> Result<()> {
    if let Some((_, new)) = REMOVED_KEYS.iter().find(|(old, _)| *old == key) {
        return Err(exit(1, format!("error: {key} was removed; use {new}")));
    }
    if key == Project::GC_EVICT_KEY && !value.is_null() && !matches!(value.as_str(), Some("on" | "off")) {
        return Err(exit(1, format!("{key} is on or off")));
    }
    if !repo.p.is_overlay() {
        if let Some(rest) = key.strip_prefix("overlay.") {
            let name = rest.split('.').next().unwrap_or("");
            if let Some(info) = repo.overlay_info(name)? {
                if rest == name || rest == format!("{name}.hidden") {
                    return Err(exit(1, "overlay visibility changes only through overlay show-to-project"));
                }
                if info.hidden {
                    let orepo = repo.overlay_repo(name)?;
                    orepo.require(ctx, "access")?;
                    let _lock = orepo.p.lock()?;
                    return set_config(ctx, &orepo, key, value);
                }
            }
        }
    }
    let mut rec = Record::new("config");
    let mut m = serde_json::Map::new();
    m.insert(key.to_string(), value);
    rec.data = serde_json::json!({"config": m});
    repo.commit_record(ctx, rec)?;
    Ok(())
}

pub fn projects(ctx: &Ctx, _args: &[String]) -> Result<i32> {
    for n in ctx.store.project_names()? {
        match Repo::open(ctx, &n) {
            Ok(r) => {
                let lines = r.lines()?.len();
                outln!(ctx, "{n}\t{lines} line(s)\t{}", r.p.dir.display());
            }
            Err(_) => continue,
        }
    }
    Ok(0)
}

pub fn config(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("-l|--list").value("--get").value("--unset").flag("--json").flag("--local");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    if a.has("--local") {
        return super::checks::local_config(ctx, repo, &a);
    }
    if a.has("-l") || (a.pos.is_empty() && !a.has("--get") && !a.has("--unset")) {
        for (k, v) in repo.p.db.config_all()? {
            outln!(ctx, "{k}={v}");
        }
        return Ok(0);
    }
    if let Some(k) = a.get("--get").or(if a.pos.len() == 1 { Some(a.pos[0].as_str()) } else { None }) {
        let mut value = repo.p.config_value(k)?;
        if let Some(rest) = k.strip_prefix("overlay.") {
            let name = rest.split('.').next().unwrap_or("");
            if repo.overlay_info(name)?.is_some_and(|i| i.hidden) {
                value = if repo.reads_overlay(name, ctx.caller.uid, ctx.caller.gid)? {
                    repo.overlay_project(name)?.db.overlay_config(k)?
                } else {
                    None
                };
            }
        }
        return match value {
            Some(v) => {
                match v.as_str() {
                    Some(s) => outln!(ctx, "{s}"),
                    None => outln!(ctx, "{v}"),
                }
                Ok(0)
            }
            None => Ok(1),
        };
    }
    let key = a.get("--unset").unwrap_or_else(|| a.pos.first().map(|s| s.as_str()).unwrap_or(""));
    if matches!(key, "checks.runner" | "checks.privileged-runner") {
        return Err(exit(128, format!("{key} is machine-local; use config --local")));
    }
    repo.require(ctx, if policy_key(key) { "access" } else { "config" })?;
    if (key == "admin" || key == "lead") && !ctx.caller.is_root() {
        return Err(exit(1, format!("permission denied: only root changes '{key}'")));
    }
    let _lock = repo.p.lock()?;
    if let Some(k) = a.get("--unset") {
        set_config(ctx, repo, k, serde_json::Value::Null)?;
        return Ok(0);
    }
    let (k, v) = (&a.pos[0], a.pos[1..].join(" "));
    let command = ["check.", "derive.command.", "merge.driver.", "merge.verify."].iter().any(|prefix| k.starts_with(prefix));
    let val =
        if command { serde_json::Value::String(v) } else { serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v)) };
    super::checks::validate_config(k, &val)?;
    if k == "queue.probes" && !val.is_null() && !val.is_boolean() {
        return Err(exit(128, "queue.probes must be true or false"));
    }
    if k == "queue.run" && !val.is_null() {
        let value = val.as_str().ok_or_else(|| exit(128, "queue.run must be a comma-separated list of line names"))?;
        for name in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            crate::store::check_name("line", name)?;
        }
    }
    set_config(ctx, repo, k, val)?;
    if POLICY_KEYS.contains(&k.as_str()) {
        crate::gitbridge::sync_store_access(repo)?;
    }
    Ok(0)
}

/// Settings that decide who may do what: changing them is the `access` action.
const POLICY_KEYS: &[&str] = &["access", "grants", "deny", "protect", "members", "admin", "lead"];

/// A named check's command belongs to the protection rules that name it (`check.<name>`).
fn policy_key(key: &str) -> bool {
    POLICY_KEYS.contains(&key) || key.starts_with("check.") || key.starts_with("checks.") || key == "registry.tokens"
}

/// `layr access`: roles and deny rules (docs/design.md, section 3).
pub fn access(ctx: &Ctx, args: &[String]) -> Result<i32> {
    if args.first().map(String::as_str) == Some("token") {
        return super::oci::token(ctx, &args[1..]);
    }
    use crate::access::{action_role, parse_principal, show_principal, Role, ACTIONS};
    let h = here(ctx)?;
    let repo = &h.repo;
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = args.get(1..).unwrap_or(&[]);
    let principal = |s: Option<&String>| -> Result<String> {
        let s =
            s.ok_or_else(|| exit(128, "usage: layr access (grant|revoke|deny|allow|check) <user|@group|uid:N|gid:N|*> ..."))?;
        parse_principal(s).ok_or_else(|| exit(128, format!("fatal: unknown user or group '{s}'")))
    };
    let pol = repo.policy();
    match sub {
        "person-key" if rest.len() == 2 => {
            repo.require(ctx, "access")?;
            let login = &rest[0];
            if !layr_store::person::login_name(login) {
                bail!("invalid person login");
            }
            layr_store::person::verifying_key(&rest[1])?;
            let _lock = repo.p.lock()?;
            let mut pins = repo.p.config_value("person.keys")?.unwrap_or_else(|| serde_json::json!({}));
            pins[login] = serde_json::json!(rest[1]);
            set_config(ctx, repo, "person.keys", pins)?;
            outln!(ctx, "pinned person {login}");
            Ok(0)
        }
        "list" => {
            for (k, r) in &pol.access {
                outln!(ctx, "{}\t{}", show_principal(k), r.name());
            }
            for (k, (r, end)) in &pol.grants {
                outln!(ctx, "{}\t{}\tuntil {}", show_principal(k), r.name(), fmt_time(*end));
            }
            for (k, v) in &pol.deny {
                outln!(ctx, "{}	deny {}", show_principal(k), v.join(" "));
            }
            Ok(0)
        }
        "check" => {
            // Explain a decision: `layr access check <principal> <action>`.
            let k = principal(rest.first())?;
            let action = rest.get(1).ok_or_else(|| exit(128, "usage: layr access check <principal> <action>"))?;
            if action_role(action).is_none() {
                return Err(exit(128, format!("fatal: unknown action '{action}'")));
            }
            let who = match k.strip_prefix("user:").and_then(|u| u.parse::<u32>().ok()) {
                Some(uid) => crate::access::Who::of(uid, crate::ctx::Caller::from_uid(uid).gid),
                None => return Err(exit(128, "fatal: check takes a user")),
            };
            match pol.check(&who, action) {
                Ok(()) => outln!(ctx, "allowed: {} is {}", show_principal(&k), pol.role(&who).name()),
                Err(e) => outln!(ctx, "refused: {e}"),
            }
            Ok(0)
        }
        "grant" | "revoke" | "deny" | "allow" => {
            repo.require(ctx, "access")?;
            let k = principal(rest.first())?;
            let _lock = repo.p.lock()?;
            let pol = repo.policy();
            match sub {
                "grant" => {
                    let role = rest.get(1).and_then(|r| Role::parse(r)).ok_or_else(|| {
                        exit(128, "usage: layr access grant <principal> (reader|writer|maintainer|admin) [--until <30m|2h|1d>]")
                    })?;
                    let mut pol = pol.clone();
                    match rest.iter().position(|x| x == "--until") {
                        Some(i) => {
                            // A role for a time adds to the principal's role and ends by itself.
                            let d = rest.get(i + 1).ok_or_else(|| exit(128, "usage: --until <30m|2h|1d>"))?;
                            pol.grants.insert(k, (role, ids::now_ms() + duration_ms(d)?));
                            set_config(ctx, repo, "grants", pol.grants_json())?;
                        }
                        None => {
                            pol.access.insert(k, role);
                            set_config(ctx, repo, "access", pol.access_json())?;
                        }
                    }
                }
                "revoke" => {
                    let mut pol = pol.clone();
                    if pol.grants.remove(&k).is_some() {
                        set_config(ctx, repo, "grants", pol.grants_json())?;
                    }
                    pol.access.remove(&k);
                    set_config(ctx, repo, "access", pol.access_json())?;
                }
                _ => {
                    let actions: Vec<String> = rest[1..].to_vec();
                    if actions.is_empty() {
                        return Err(exit(128, format!("usage: layr access {sub} <principal> <action>...")));
                    }
                    for x in &actions {
                        if x != "*" && action_role(x).is_none() {
                            let known: Vec<&str> = ACTIONS.iter().map(|(a, _)| *a).collect();
                            return Err(exit(128, format!("fatal: unknown action '{x}' (known: {})", known.join(", "))));
                        }
                    }
                    let mut d = pol.deny.clone();
                    let e = d.entry(k.clone()).or_default();
                    if sub == "deny" {
                        for x in actions {
                            if !e.contains(&x) {
                                e.push(x);
                            }
                        }
                    } else {
                        e.retain(|x| !actions.contains(x));
                    }
                    if e.is_empty() {
                        d.remove(&k);
                    }
                    set_config(ctx, repo, "deny", serde_json::to_value(d)?)?;
                }
            }
            crate::gitbridge::sync_store_access(repo)?;
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr access [list|grant|revoke|deny|allow|check] ...")),
    }
}

/// `layr approve [<rev>] [--withdraw]`: approve an exact state (or withdraw the approval). A
/// protection rule can require approvals of the state a merge takes (`--approvals`).
pub fn approve(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--withdraw").flag("-q|--quiet").value("--declassify").flag("--json");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let state = revs::resolve(repo, h.line.as_ref(), a.pos.first().map(|s| s.as_str()).unwrap_or("head"))?;
    if a.pos.len() > 1 {
        bail!("usage: layr approve [--declassify <path>@<id>] <rev>");
    }
    if a.has("--withdraw") && a.has("--declassify") {
        bail!("--withdraw and --declassify cannot be combined");
    }
    let s = repo.state(&state)?;
    let _lock = repo.p.lock()?;
    let withdraw = a.has("--withdraw");
    let mut rec = match a.get("--declassify") {
        Some(scope) => crate::secrets::declassify(ctx, repo, scope, &state)?,
        None => super::attest::record(repo, &state, if withdraw { "revoke" } else { "approval" }, b"")?,
    };
    if withdraw {
        rec.data["revokes"] =
            serde_json::json!(repo.p.db.own_attestations(&state, &repo.p.store.machine.id, ctx.caller.uid, Some("approval"))?);
    }
    let rec = repo.commit_record(ctx, rec)?;
    if a.has("--json") {
        outln!(ctx, "{}", rec.data);
    } else if !a.has("-q") {
        let what = if withdraw { "Withdrew the approval of" } else { "Approved" };
        outln!(ctx, "{what} state {} {}", ids::short(&state), s.message.lines().next().unwrap_or(""));
    }
    Ok(0)
}

/// A duration: `90s`, `30m`, `2h`, `1d`.
pub(crate) fn duration_ms(s: &str) -> Result<i64> {
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| exit(128, format!("fatal: bad duration '{s}' (for example 30m, 2h, 1d)")))?;
    let k = match unit {
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(exit(128, format!("fatal: bad duration '{s}' (for example 30m, 2h, 1d)"))),
    };
    Ok(n.saturating_mul(k))
}

/// `layr protect`: protection rules of line name patterns.
pub fn protect(ctx: &Ctx, args: &[String]) -> Result<i32> {
    use crate::access::{Role, RuleSpec};
    let spec = Spec::new()
        .value("--update")
        .flag("--no-expect")
        .flag("--expect")
        .value("--check")
        .flag("--direct")
        .flag("--no-direct")
        .value("--rewrite")
        .value("--path")
        .value("--approvals")
        .value("--approvers")
        .flag("--remove");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let pol = repo.policy();
    let pattern = match a.pos.first() {
        None => {
            for (p, r) in &pol.protect {
                let rule = pol.rule_for(p);
                let checks = r.check.join(",");
                match rule {
                    Some(x) => {
                        if crate::model::ref_namespace(p) {
                            let from =
                                x.approvers.iter().map(|key| crate::access::show_principal(key)).collect::<Vec<_>>().join(",");
                            outln!(
                                ctx,
                                "{p}\tupdate {}\texpect {}\tcheck {}\tapprovals {} from {}",
                                x.update.name(),
                                x.expect,
                                if checks.is_empty() { "-" } else { &checks },
                                x.approvals,
                                if from.is_empty() { "maintainers" } else { &from }
                            );
                            continue;
                        }
                        let paths: Vec<String> = x.paths.iter().map(|(p, r)| format!("{p}={}", r.name())).collect();
                        outln!(
                            ctx,
                            "{p}\tupdate {}\texpect {}\tcheck {}\tdirect {}\trewrite {}{}{}",
                            x.update.name(),
                            x.expect,
                            if checks.is_empty() { "-".into() } else { checks },
                            x.direct,
                            x.rewrite.name(),
                            if paths.is_empty() { String::new() } else { format!("\tpaths {}", paths.join(",")) },
                            if x.approvals == 0 {
                                String::new()
                            } else {
                                let from: Vec<String> = x.approvers.iter().map(|k| crate::access::show_principal(k)).collect();
                                format!(
                                    "\tapprovals {} from {}",
                                    x.approvals,
                                    if from.is_empty() { "maintainers".to_string() } else { from.join(",") }
                                )
                            }
                        )
                    }
                    None => outln!(ctx, "{p}"),
                }
            }
            return Ok(0);
        }
        Some(p) => p.clone(),
    };
    if crate::model::ref_namespace(&pattern) && ["--direct", "--no-direct", "--rewrite", "--path"].iter().any(|flag| a.has(flag))
    {
        bail!("ref protection does not accept --direct, --no-direct, --rewrite or --path");
    }
    repo.require(ctx, "access")?;
    let _lock = repo.p.lock()?;
    let mut all = repo.policy().protect;
    if a.has("--remove") {
        all.remove(&pattern);
    } else {
        let mut r: RuleSpec = all.get(&pattern).cloned().unwrap_or_default();
        let role = |s: &str| -> Result<String> {
            Role::parse(s).map(|r| r.name().to_string()).ok_or_else(|| exit(128, format!("fatal: unknown role '{s}'")))
        };
        if let Some(u) = a.get("--update") {
            r.update = Some(role(u)?);
        }
        if let Some(u) = a.get("--rewrite") {
            r.rewrite = Some(role(u)?);
        }
        if a.has("--no-expect") {
            r.expect = Some(false);
        }
        if a.has("--expect") {
            r.expect = Some(true);
        }
        if a.has("--direct") {
            r.direct = Some(true);
        }
        if a.has("--no-direct") {
            r.direct = Some(false);
        }
        if let Some(c) = a.get("--check") {
            r.check = c.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
        }
        if let Some(n) = a.get("--approvals") {
            r.approvals = Some(n.parse().map_err(|_| exit(128, format!("fatal: --approvals takes a number, not '{n}'")))?);
        }
        if let Some(who) = a.get("--approvers") {
            r.approvers = Vec::new();
            for w in who.split(',').filter(|x| !x.is_empty()) {
                r.approvers.push(
                    crate::access::parse_principal(w).ok_or_else(|| exit(128, format!("fatal: unknown user or group '{w}'")))?,
                );
            }
        }
        // `--path <path>=<role>` (repeatable); an empty role removes the path rule.
        for pr in a.all("--path") {
            let (path, rl) = pr.split_once('=').ok_or_else(|| exit(128, "usage: --path <path>=<role>"))?;
            fsutil::check_rel(path.trim_end_matches('/')).map_err(|_| exit(128, format!("fatal: bad path '{path}'")))?;
            if rl.is_empty() {
                r.paths.remove(path);
            } else {
                r.paths.insert(path.to_string(), role(rl)?);
            }
        }
        all.insert(pattern, r);
    }
    set_config(ctx, repo, "protect", serde_json::to_value(all)?)?;
    Ok(0)
}

/// Why a cache path is no cache (`save --caches`, `overlay list`, `overlay repair`): its folder holds
/// files of the head state, and a cache would hide them from the states that follow.
pub(crate) const VERSIONED: &str = "not a cache: has versioned files; remove them from history first";
/// A folder with versioned files that an older layr made a cache, after it became plain again.
pub(crate) const VERSIONED_AGAIN: &str = "not a cache: has versioned files; a plain folder again";

/// The `layr cache` forms that the cache overlays replaced (docs/cache-overlays.md, section 4).
/// Each fails and names its replacement; no repository is opened.
pub fn removed_cache(args: &[String]) -> Result<i32> {
    let (old, new) = match args.first().map(|s| s.as_str()) {
        Some("add") => ("layr cache add", "'regenerable' in .layrattributes, then 'layr save --caches'"),
        Some("remove") => {
            ("layr cache remove", "'-regenerable' in .layrattributes, or 'layr config regenerable' without the path")
        }
        Some("pull") => ("layr cache pull", "'layr line new <name> <line>' or 'layr merge <line>'"),
        Some("repair") => ("layr cache repair", "'layr overlay repair <overlay>'"),
        Some("list") => ("layr cache list", "'layr overlay list'"),
        _ => ("layr cache", "'layr overlay list'"),
    };
    Err(exit(2, format!("{old} was removed. Use {new}.")))
}

/// Hidden helpers for tests: `layr debug changes <a> <b>` prints the raw change list.
pub fn debug(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let h = here(ctx)?;
    match args.first().map(|s| s.as_str()) {
        Some("changes") if args.len() >= 3 => {
            let a = h.repo.state_src(&revs::resolve(&h.repo, h.line.as_ref(), &args[1])?)?;
            let b = h.repo.state_src(&revs::resolve(&h.repo, h.line.as_ref(), &args[2])?)?;
            for c in changes::candidates(&a, &b)? {
                let k = |m: &Option<fsutil::Meta>| m.as_ref().map(|x| format!("{:?}", x.ftype)).unwrap_or("-".into());
                outln!(
                    ctx,
                    "{}\t{}\t{}\t{}\t{:?}",
                    c.old_path.clone().unwrap_or_default(),
                    c.path,
                    k(&c.old),
                    k(&c.new),
                    c.ranges
                );
            }
            Ok(0)
        }
        // A checked receive of a stream from stdin into the project's states (for tests of
        // the checks in `recv`): root only.
        Some("receive") if args.len() >= 3 => {
            if !ctx.caller.is_root() {
                return Err(exit(1, "error: debug receive needs root"));
            }
            let p = &h.repo.p;
            let parent = |u: &str| -> Option<std::path::PathBuf> {
                let id = p.db.state_by_subvol(u).ok().flatten()?;
                let path = p.state_path(&id);
                path.exists().then_some(path)
            };
            let e = crate::recv::Expect { name: &args[1], uuid: &args[2], parent: &parent, clones: &[] };
            crate::recv::receive(&mut std::io::stdin(), &p.states_dir(), &e)?;
            outln!(ctx, "received {}", args[1]);
            Ok(0)
        }
        _ => Err(exit(128, "usage: layr debug changes <a> <b> | layr debug receive <name> <uuid> < stream")),
    }
}

/// A byte count for people: `812 MiB`.
pub(crate) fn human_size(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

#[cfg(test)]
mod record_caches_tests {
    use super::*;
    use crate::ctx::Caller;
    use crate::model::Actor;

    /// A cache snapshot that a later failure of the same call leaves unnamed is removed.
    #[test]
    fn a_failed_call_removes_the_snapshots_it_made() -> Result<()> {
        // Snapshots need the subvolume file system: the APFS (or btrfs) test root, as in storage tests.
        let Some(base) = std::env::var_os("LAYR_APFS_TEST_ROOT").or_else(|| std::env::var_os("LAYR_OVERLAYFS_TEST_ROOT")) else {
            eprintln!("skipped: set LAYR_APFS_TEST_ROOT to a subvolume file system");
            return Ok(());
        };
        let dir = std::path::PathBuf::from(base).join(format!("layr-record-caches-{}", ids::new_id()));
        let ctx = Ctx {
            store: crate::store::Store::open(&dir)?,
            caller: Caller::from_uid(0),
            cwd: dir.clone(),
            env: std::collections::HashMap::from([("LAYR_PROJECT".into(), "p".into())]),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let project_dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(project_dir.join("meta/db"))?;
        std::fs::create_dir(project_dir.join("states"))?;
        let id = ids::new_id();
        let p = Project {
            store: &ctx.store,
            name: "p".into(),
            dir: project_dir.clone(),
            id: id.clone(),
            db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite"))?,
        };
        let mut rec = crate::model::Record::new("project");
        rec.data = serde_json::json!({"id": id, "name": "p", "config": {}});
        p.append(rec, &Actor { uid: 0, name: "root".into(), session: None })?;
        let repo = Repo { p, ctx: None };
        let work = dir.join("work");
        std::fs::create_dir_all(&work)?;
        crate::storage::create_subvolume(&work.join("a"))?;
        let state = ids::new_id();
        // The first cache is snapshotted; the second path escapes the working folder and fails.
        let paths = ["a".to_string(), "../escape".to_string()];
        assert!(record_caches(&ctx, &repo, &state, &work, &paths, false).is_err());
        assert_eq!(std::fs::read_dir(repo.p.states_dir())?.count(), 0, "the failed call leaves no snapshot");
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }
}
