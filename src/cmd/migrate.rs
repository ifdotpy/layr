//! `layr migrate`: the one-time move of a project from snapshot storage (btrfs subvolumes, APFS
//! trees and thin layers in `states/` and `deltas/`) to the object store (docs/objects.md,
//! section 13). Every source state gets a tree and a times table, each is checked out and
//! compared with its snapshot, and a `state.tree` record adds both to the state record. State ids,
//! refs, tags, parks, queue entries, changes, attestations and the Git mapping stay as they are.

use super::{exit, here, Here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Record, StateKind, StateRec};
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use layr_objects::{Id, StateRef, Store};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

const USAGE: &str = "usage: layr migrate [<project>] [--dry-run] [--resume] [--delete-old] [--leave-missing <state>]...
Move the states of a project from snapshots to the object store (docs/objects.md, section 13):
import each state oldest first, check out and compare each one with its snapshot, record its tree
and times table, then repack. --resume continues an interrupted run. --delete-old then removes
the snapshots of the verified states (they come back from the object store on read).
--leave-missing explicitly keeps a named unavailable state record without its contents.";

/// The states of one `state.tree` record: a run records in batches of this size.
const RECORD_BATCH: usize = 64;

/// The first line of the mapping table.
const TABLE_HEADER: &str = "layr-migrate-map 1";

/// The mapping table of a run: old state id to (tree, times), one line per verified state,
/// appended and synced as the run goes (`meta/migrate/map`). An interrupted run continues from
/// it with `--resume`: a state in the table is verified and is not imported again.
#[derive(Debug)]
pub(crate) struct Table {
    path: PathBuf,
    pub entries: BTreeMap<String, StateRef>,
    /// The length of the complete lines when the file ends in a line that a crash cut.
    cut: Option<u64>,
}

impl Table {
    /// Read the table in `dir`; an absent table is empty. A last line without its newline is a
    /// write that a crash cut: it is left out, and the next append removes it.
    pub fn load(dir: &Path) -> Result<Table> {
        let path = dir.join("map");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut entries = BTreeMap::new();
        let complete = match text.rfind('\n') {
            Some(i) => &text[..=i],
            None => "",
        };
        for (n, line) in complete.lines().enumerate() {
            if n == 0 {
                if line != TABLE_HEADER {
                    bail!("{} is not a migration table of this layr ({line:?})", path.display());
                }
                continue;
            }
            let parts: Vec<&str> = line.split(' ').collect();
            let [state, tree, times] = parts[..] else {
                bail!("{} line {}: bad entry {line:?}", path.display(), n + 1);
            };
            if !crate::model::is_uuid(state) {
                bail!("{} line {}: bad state id {state:?}", path.display(), n + 1);
            }
            entries.insert(state.to_string(), StateRef { tree: Id::parse(tree)?, times: Id::parse(times)? });
        }
        let cut = (complete.len() != text.len()).then_some(complete.len() as u64);
        Ok(Table { path, entries, cut })
    }

    /// Add a verified state and sync the file before the call returns.
    pub fn append(&mut self, state: &str, r: &StateRef) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(len) = self.cut.take() {
            // The line that a crash cut goes before the next entry.
            std::fs::OpenOptions::new().write(true).open(&self.path)?.set_len(len)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        let mut line = String::new();
        if f.metadata()?.len() == 0 {
            line.push_str(TABLE_HEADER);
            line.push('\n');
        }
        line.push_str(&format!("{state} {} {}\n", r.tree, r.times));
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        self.entries.insert(state.to_string(), *r);
        Ok(())
    }
}

/// One state of the project as the planner sees it.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub id: String,
    pub parents: Vec<String>,
    pub time: i64,
    pub kind: StateKind,
    /// The record names a tree that the object store holds.
    pub recorded: bool,
    /// Freed: no line, ref, tag or undo record keeps it, and its snapshot is gone.
    pub freed: bool,
    /// No source and no owner, including after a record replay cleared `deleted`.
    pub unowned_missing: bool,
}

/// What a run does with each state.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The states to import and verify, parents before children, older first.
    pub todo: Vec<String>,
    /// Verified in the table of an earlier run, not yet recorded.
    pub unrecorded: Vec<String>,
    /// Recorded already.
    pub done: usize,
    /// Cache and derived states: outside source history, they keep their snapshots.
    pub auxiliary: usize,
    /// Freed states: no tree to move.
    pub freed: usize,
    /// Absent, unowned source records. Their records stay unchanged.
    pub unowned_missing: usize,
}

/// Order the states of a project for a run: each state after its parents, ready states by time
/// and then id. Parents outside the set do not hold a state back.
pub(crate) fn plan(states: &[Candidate], table: &BTreeMap<String, StateRef>) -> Plan {
    let mut p = Plan::default();
    let mut todo: BTreeMap<&str, &Candidate> = BTreeMap::new();
    for c in states {
        if c.kind.auxiliary() {
            p.auxiliary += 1;
        } else if c.recorded {
            p.done += 1;
        } else if table.contains_key(&c.id) {
            p.unrecorded.push(c.id.clone());
        } else if c.freed {
            p.freed += 1;
        } else if c.unowned_missing {
            p.unowned_missing += 1;
        } else {
            todo.insert(&c.id, c);
        }
    }
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, c) in &todo {
        let mut parents: Vec<&str> = c.parents.iter().map(String::as_str).filter(|p| todo.contains_key(p)).collect();
        parents.sort();
        parents.dedup();
        waiting.insert(id, parents.len());
        for parent in parents {
            children.entry(parent).or_default().push(id);
        }
    }
    let mut ready: BinaryHeap<std::cmp::Reverse<(i64, &str)>> =
        waiting.iter().filter(|(_, n)| **n == 0).map(|(id, _)| std::cmp::Reverse((todo[id].time, *id))).collect();
    while let Some(std::cmp::Reverse((_, id))) = ready.pop() {
        p.todo.push(id.to_string());
        for child in children.get(id).into_iter().flatten() {
            let n = waiting.get_mut(child).expect("a known child");
            *n -= 1;
            if *n == 0 {
                ready.push(std::cmp::Reverse((todo[child].time, child)));
            }
        }
    }
    // A parent cycle cannot come from signed records; the states in one are not left out silently.
    for (id, n) in &waiting {
        if *n > 0 {
            p.todo.push(id.to_string());
        }
    }
    p
}

/// The differences that count for `id`. A state imported from Git is built again on each read, with the times of
/// that build (Git has no file times): only its times may differ; content, modes and links must match.
fn counted_differences<'d>(repo: &Repo, id: &str, differences: &'d [String]) -> Vec<&'d String> {
    let git_built = crate::gitbridge::buildable(repo, id);
    differences.iter().filter(|d| !(git_built && d.starts_with("time differs"))).collect()
}

/// How many migrated states pass between two releases of trees that Git built.
const BUILT_RELEASE_EVERY: usize = 100;

fn table_dir(repo: &Repo) -> PathBuf {
    repo.p.meta_dir().join("migrate")
}

/// The marker of a run that has not finished: `--resume` continues it.
fn running_marker(repo: &Repo) -> PathBuf {
    table_dir(repo).join("running")
}

/// Every state of the project with what the planner needs.
fn candidates(repo: &Repo, store: Option<&Store>) -> Result<Vec<Candidate>> {
    let mut st = repo.p.db.conn.prepare("SELECT rec, deleted FROM states")?;
    let rows: Vec<(String, bool)> =
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?.collect::<Result<_, _>>()?;
    drop(st);
    // A replay fills owners without freeing states, so `deleted` alone cannot
    // identify absent dead states. Keep both recorded and replayed owners:
    // a stale or incomplete owners table must not hide a live missing source.
    let mut owned: BTreeSet<String> = repo.p.db.owner_rows()?.into_iter().map(|r| r.0).collect();
    owned.extend(repo.p.recomputed_owners()?.into_iter().map(|r| r.0));
    // A state in the free queue whose storage is not removed yet: readers are refused (`pins_add`).
    let queued: BTreeSet<String> = {
        let mut q = repo.p.db.conn.prepare("SELECT state FROM free_queue")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
        rows
    };
    let mut out = Vec::with_capacity(rows.len());
    for (raw, deleted) in rows {
        let s: StateRec = serde_json::from_str(&raw)?;
        let recorded = match (store, &s.tree, &s.times) {
            (Some(store), Some(tree), Some(times)) => store.has(&Id::parse(tree)?) && store.has(&Id::parse(times)?),
            _ => false,
        };
        let absent = !repo.p.has_state(&s.id);
        let unowned = !owned.contains(&s.id);
        let freed = unowned && ((deleted && absent) || queued.contains(&s.id));
        let unowned_missing = !deleted && absent && unowned && !crate::gitbridge::buildable(repo, &s.id);
        out.push(Candidate { id: s.id, parents: s.parents, time: s.time, kind: s.kind, recorded, freed, unowned_missing });
    }
    Ok(out)
}

/// Why the project cannot be migrated now: a command holds the project lock or a line lock, or
/// the daemon of this user serves the store (its follow passes, syncs and gc write). None when
/// nothing runs.
fn busy(repo: &Repo) -> Result<Option<String>> {
    use rustix::fs::{flock, FlockOperation};
    let meta = repo.p.meta_dir();
    let free = |path: &Path| -> Result<bool> {
        let f = match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e.into()),
        };
        Ok(match flock(&f, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => true,
            Err(rustix::io::Errno::WOULDBLOCK) => false,
            Err(e) => return Err(e.into()),
        })
    };
    if !free(&meta.join("lock"))? {
        let holder = std::fs::read_to_string(meta.join("lock.holder"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .map(|v| format!(" (pid {}: {})", v["pid"], v["command"].as_str().unwrap_or("?")))
            .unwrap_or_default();
        return Ok(Some(format!("a layr command holds the project lock{holder}")));
    }
    if let Ok(rd) = std::fs::read_dir(meta.join("line-locks")) {
        for e in rd.flatten() {
            if !free(&e.path())? {
                let id = e.file_name().to_string_lossy().into_owned();
                let name = repo.p.db.line_by_id(&id)?.map(|l| l.name).unwrap_or(id);
                return Ok(Some(format!("a layr command or agent works in line '{name}' (it holds the line lock)")));
            }
        }
    }
    // On Linux the daemon socket can be the root service that runs this command: only the
    // locks count there. On macOS the user daemon is its own process.
    #[cfg(target_os = "macos")]
    if let crate::service::Hello::Running { root: Some(served), .. } =
        crate::service::hello(&crate::service::user_daemon_socket())
    {
        let same = match (served.canonicalize(), repo.p.store.root.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => served == repo.p.store.root,
        };
        if same {
            return Ok(Some(
                "the layr daemon of this user serves this store (follow passes, syncs and gc write states); stop it with `launchctl bootout gui/$(id -u)/ws.layr.daemon`, migrate, then start it again (docs/follow.md)".into(),
            ));
        }
    }
    Ok(None)
}

fn human(bytes: u64) -> String {
    super::admin::human_size(bytes)
}

pub fn migrate(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("-n|--dry-run").flag("--resume").flag("--delete-old").value("--leave-missing"), args)?;
    if a.pos.len() > 1 {
        return Err(exit(128, USAGE));
    }
    let h = match a.pos.first() {
        Some(name) => Here {
            repo: Repo::open(ctx, name)?,
            line: None,
            loc: crate::ctx::Loc { project: name.clone(), line: None, sub: String::new() },
        },
        None => here(ctx)?,
    };
    let repo = &h.repo;
    if !repo.is_admin(ctx) {
        bail!("permission denied: layr migrate needs the admin role of project {}", repo.p.name);
    }
    if repo.p.is_overlay() {
        bail!("{} is an overlay: migrate its project; overlays keep their snapshots in this version", repo.p.name);
    }
    let leave_missing: BTreeSet<String> =
        a.all("--leave-missing").iter().map(|rev| crate::revs::resolve(repo, h.line.as_ref(), rev)).collect::<Result<_>>()?;
    let dry = a.has("-n");
    let marker = running_marker(repo);
    if marker.exists() && !a.has("--resume") && !dry {
        return Err(exit(
            1,
            format!(
                "an earlier `layr migrate` of project {} did not finish ({}); continue it with `layr migrate --resume`",
                repo.p.name,
                marker.display()
            ),
        ));
    }
    if let Some(why) = busy(repo)? {
        if dry {
            outln!(ctx, "would refuse now: {why}");
        } else {
            return Err(exit(1, format!("layr migrate refused: {why}. Wait for it to finish, then run the migration again.")));
        }
    }
    if dry {
        return dry_run(ctx, &h, a.has("--delete-old"), &leave_missing);
    }
    outln!(ctx, "target storage: object store v{} (snapshot storage is v1)", layr_objects::store::FORMAT_VERSION);

    // 1. Unsaved work of each line folder of this machine becomes an automatic state, by the
    // same steps as any layr command that saves the working folder first.
    let me = repo.p.store.machine.id.clone();
    for line in repo.lines()? {
        if line.machine != me || !repo.working(&line).exists() {
            continue;
        }
        let _line_lock = repo.line_lock(&line)?;
        let line = repo.line(&line.id)?;
        repo.prepare_save(&line)?;
        let _lock = repo.p.lock()?;
        let before = line.head.clone();
        if let Some(st) = repo.working_state(ctx, &line, "before layr migrate")? {
            if st != before {
                outln!(ctx, "line {}: unsaved work saved as automatic state {}", line.name, ids::short(&st));
            }
        }
    }

    // 2. States that retention evicted come back from a store remote first; without the lock,
    // as any read does. A state that no remote gives stops the run before anything changes.
    let mut table = Table::load(&table_dir(repo))?;
    let store = crate::objstore::open(&repo.p)?;
    let states = candidates(repo, Some(&store))?;
    let p = plan(&states, &table.entries);
    let mut missing = Vec::new();
    let mut omitted = BTreeSet::new();
    for id in &p.todo {
        if repo.p.has_state(id) || crate::gitbridge::buildable(repo, id) {
            continue;
        }
        if let Err(e) = repo.state_src(id) {
            if leave_missing.contains(id) {
                omitted.insert(id.clone());
            } else {
                missing.push(format!("{} ({e:#})", ids::short(id)));
            }
        }
    }
    if !missing.is_empty() {
        return Err(exit(
            1,
            format!(
                "layr migrate refused: {} state(s) are not on this machine and no store remote gave them back:\n\t{}\nFetch them (`layr fetch <remote>`), or add the remote that holds them, then run the migration again.",
                missing.len(),
                missing.join("\n\t")
            ),
        ));
    }

    // 3. Under the project lock for the rest of the run: other commands wait until it ends.
    let _lock = repo.p.lock()?;
    std::fs::create_dir_all(table_dir(repo))?;
    std::fs::write(&marker, serde_json::json!({"pid": std::process::id(), "since_ms": ids::now_ms()}).to_string())?;
    // The plan again under the lock: a command may have saved since.
    let states = candidates(repo, Some(&store))?;
    let mut p = plan(&states, &table.entries);
    omitted.retain(|id| !repo.p.has_state(id) && !crate::gitbridge::buildable(repo, id));
    p.todo.retain(|id| !omitted.contains(id));
    std::fs::write(table_dir(repo).join("unavailable.json"), serde_json::to_vec_pretty(&omitted)?)?;
    for id in &omitted {
        outln!(ctx, "left unavailable by request: {id}; its record stays unchanged");
    }
    let hot = super::objects::hot_states(&h)?;
    let stop_after: Option<usize> = ctx.env("LAYR_TEST_MIGRATE_STOP_AFTER").and_then(|v| v.parse().ok());
    let started = Instant::now();
    let mut pending: Vec<String> = p.unrecorded.clone();
    let record = |pending: &mut Vec<String>, table: &Table| -> Result<()> {
        for chunk in pending.chunks(RECORD_BATCH) {
            let mut map = serde_json::Map::new();
            for id in chunk {
                let r = table.entries.get(id).expect("a verified state is in the table");
                // A resumed run must not trust a journal whose objects were lost or changed.
                if p.unrecorded.contains(id) {
                    let (_, comparison, _, _) = super::objects::compare_checkout(&h, &store, id, r)?;
                    let differences = counted_differences(repo, id, &comparison.differences);
                    if !differences.is_empty() {
                        bail!(
                            "state {} no longer matches its migration journal: {}",
                            ids::short(id),
                            differences.iter().map(|d| d.as_str()).collect::<Vec<_>>().join("; ")
                        );
                    }
                }
                map.insert(id.clone(), serde_json::json!({"times": r.times.hex(), "tree": r.tree.hex()}));
            }
            let mut rec = Record::new(layr_model::validation::STATE_TREE_OP);
            rec.data = serde_json::json!({ "states": map });
            repo.commit_record(ctx, rec)?;
        }
        pending.clear();
        Ok(())
    };
    let total = p.todo.len();
    let (mut files, mut bytes) = (0u64, 0u64);
    for (n, id) in p.todo.iter().enumerate() {
        let parent = repo.state(id)?.parents.first().cloned();
        // An incremental import reads the paths that changed since the first parent; it needs
        // the parent's snapshot. Without it every path is read (the result is the same tree).
        let full = parent.as_ref().is_none_or(|p| !repo.p.has_state(p) || store.state_ref(p).ok().flatten().is_none());
        let cold = !hot.contains(id);
        let i = super::objects::import_state(&h, &store, id, cold, full, None)
            .with_context(|| format!("import state {}", ids::short(id)))?;
        let r = StateRef { tree: i.tree, times: i.times };
        let (_, cmp, checkout_s, compare_s) =
            super::objects::compare_checkout(&h, &store, id, &r).with_context(|| format!("verify state {}", ids::short(id)))?;
        let differences = counted_differences(repo, id, &cmp.differences);
        if !differences.is_empty() {
            outln!(ctx, "state {}: the checkout of tree {} differs from the snapshot:", ids::short(id), r.tree);
            for d in differences.iter().take(20) {
                outln!(ctx, "\t{d}");
            }
            record(&mut pending, &table)?;
            return Err(exit(
                1,
                format!(
                    "layr migrate stopped at state {}: {} difference(s). Nothing was deleted; {} state(s) before it are verified. Report this, then continue with `layr migrate --resume`.",
                    ids::short(id),
                    differences.len(),
                    n
                ),
            ));
        }
        table.append(id, &r)?;
        files += i.report.files;
        bytes += i.report.bytes;
        outln!(
            ctx,
            "{}/{} state {} {}: {} files, import {:.2} s, checkout {:.2} s, compare {:.2} s",
            n + 1,
            total,
            ids::short(id),
            if cold { "cold" } else { "hot" },
            i.report.files,
            i.seconds,
            checkout_s,
            compare_s
        );
        pending.push(id.clone());
        if stop_after == Some(n + 1) {
            // Test hook (tests/t202-migrate.sh): an interruption after the table write and before
            // the record, the worst point for `--resume`.
            let _ = ctx.out.borrow_mut().flush();
            std::process::exit(86);
        }
        if pending.len() >= RECORD_BATCH {
            record(&mut pending, &table)?;
        }
        // A state imported from Git is read as a tree that Git builds; each stays until something frees it. A
        // long run over Git history would fill the disk with these trees: free them as the run goes.
        if (n + 1) % BUILT_RELEASE_EVERY == 0 {
            let freed = super::admin::drop_built_imports(repo, false)?;
            if !freed.is_empty() {
                repo.prune_all_bases()?;
                let _ = crate::store::empty_trash(&repo.p.trash_dir()?, None);
            }
        }
    }
    record(&mut pending, &table)?;
    let imported = started.elapsed().as_secs_f64();
    let (e, packed, repack_s) = super::objects::repack_store(&h)?;
    let stat = crate::objstore::open(&repo.p)?.stat()?;
    outln!(
        ctx,
        "migrated {} state(s) ({} files, {} bytes read) in {:.1} s; repack {:.1} s ({} hot blobs turned cold, {} packs)",
        total,
        files,
        bytes,
        imported,
        repack_s,
        e.blobs,
        packed.packs
    );
    outln!(
        ctx,
        "states: {} migrated now, {} recorded from an earlier run, {} migrated before, {} cache or derived (kept as snapshots), {} freed",
        total,
        p.unrecorded.len(),
        p.done,
        p.auxiliary,
        p.freed
    );
    outln!(ctx, "object store: {} ({} bytes)", human(stat.total_bytes()), stat.total_bytes());
    outln!(ctx, "{} absent source state(s) have no owner; their records stay unchanged", p.unowned_missing);
    super::admin::set_config(ctx, repo, crate::objstore::ENABLED, serde_json::json!(true))?;
    delete_old(ctx, &h, a.has("--delete-old"), false)?;
    std::fs::remove_file(&marker)?;
    Ok(0)
}

/// The states whose snapshots `--delete-old` removes: every state with a recorded tree that has
/// a snapshot here, except states that an active view or cache overlay needs.
fn deletable(repo: &Repo) -> Result<Vec<String>> {
    let store = crate::objstore::open_existing(&repo.p)?;
    #[allow(unused_mut)]
    let mut keep: BTreeSet<String> = super::admin::overlay_named_states(repo)?;
    #[cfg(target_os = "macos")]
    keep.extend(crate::views::protected_states(&repo.p));
    let mut out = Vec::new();
    for c in candidates(repo, Some(&store))? {
        if !c.recorded || keep.contains(&c.id) || !repo.p.has_state(&c.id) {
            continue;
        }
        out.push(c.id);
    }
    Ok(out)
}

/// Remove the old snapshots of the verified states (`--delete-old`), or print what that frees.
/// The records stay; a read makes a snapshot again from the object store (`objstore::build_state`).
fn delete_old(ctx: &Ctx, h: &Here, delete: bool, dry: bool) -> Result<()> {
    let repo = &h.repo;
    if !crate::objstore::exists(&repo.p) {
        outln!(ctx, "old snapshots: nothing to delete (no state is migrated yet)");
        return Ok(());
    }
    let ids = deletable(repo)?;
    let size = super::admin::states_bytes(repo, &ids);
    if !delete || dry {
        outln!(
            ctx,
            "old snapshots: `layr migrate --delete-old` {} {} in {} snapshot(s)",
            if dry { "would free" } else { "frees" },
            human(size),
            ids.len()
        );
        return Ok(());
    }
    let left = plan(&candidates(repo, Some(&crate::objstore::open(&repo.p)?))?, &BTreeMap::new());
    if !left.todo.is_empty() {
        bail!("{} state(s) are not migrated yet: the old snapshots stay", left.todo.len());
    }
    // Verify every replacement before deleting any snapshot. Root objects alone do not prove
    // that descendant times tables, blobs and delta bases are still present and readable.
    let store = crate::objstore::open(&repo.p)?;
    for id in &ids {
        let state = repo.state(id)?;
        let r = StateRef {
            tree: Id::parse(state.tree.as_deref().context("missing migrated tree")?)?,
            times: Id::parse(state.times.as_deref().context("missing migrated times")?)?,
        };
        let (_, comparison, _, _) = super::objects::compare_checkout(&h, &store, id, &r)?;
        if !comparison.differences.is_empty() {
            bail!("state {} cannot replace its old snapshot: {}", ids::short(id), comparison.differences.join("; "));
        }
    }
    let gone = super::admin::drop_trees(repo, &ids)?;
    outln!(
        ctx,
        "old snapshots: deleted {} snapshot(s), {} freed; their states remain in the object store",
        gone.len(),
        human(size)
    );
    Ok(())
}

/// `--dry-run`: what a run would do, with nothing changed (no object store, table or record is
/// made, and no line folder is saved).
fn dry_run(ctx: &Ctx, h: &Here, delete: bool, leave_missing: &BTreeSet<String>) -> Result<i32> {
    let repo = &h.repo;
    let table = Table::load(&table_dir(repo))?;
    let store = if crate::objstore::exists(&repo.p) { Some(crate::objstore::open_existing(&repo.p)?) } else { None };
    let states = candidates(repo, store.as_ref())?;
    let mut p = plan(&states, &table.entries);
    let hot = super::objects::hot_states(h)?;
    let copies = crate::store_remote::store_copies(repo).unwrap_or_default();
    let (mut fetch, mut lost) = (0, Vec::new());
    let mut omitted = BTreeSet::new();
    for id in &p.todo {
        if repo.p.has_state(id) || crate::gitbridge::buildable(repo, id) {
            continue;
        }
        if copies.contains_key(id) {
            fetch += 1;
        } else if leave_missing.contains(id) {
            omitted.insert(id.clone());
        } else {
            lost.push(ids::short(id));
        }
    }
    p.todo.retain(|id| !omitted.contains(id));
    let n_hot = p.todo.iter().filter(|id| hot.contains(*id)).count();
    let me = &repo.p.store.machine.id;
    let folders = repo.lines()?.iter().filter(|l| &l.machine == me && repo.working(l).exists()).count();
    outln!(ctx, "dry run: nothing changes");
    outln!(ctx, "target storage: object store v{} (snapshot storage is v1)", layr_objects::store::FORMAT_VERSION);
    outln!(ctx, "{} line folder(s): unsaved work in them is saved as automatic states first", folders);
    outln!(
        ctx,
        "{} state(s) to migrate: {} hot (line heads and the states before them), {} cold",
        p.todo.len(),
        n_hot,
        p.todo.len() - n_hot
    );
    outln!(
        ctx,
        "{} verified by an earlier run and not recorded, {} migrated before, {} cache or derived (kept as snapshots), {} freed",
        p.unrecorded.len(),
        p.done,
        p.auxiliary,
        p.freed
    );
    if fetch > 0 {
        outln!(ctx, "{fetch} state(s) are evicted and come back from a store remote first");
    }
    outln!(ctx, "{} absent source state(s) have no owner; their records stay unchanged", p.unowned_missing);
    for id in &omitted {
        outln!(ctx, "left unavailable by request: {id}; its record stays unchanged");
    }
    if !lost.is_empty() {
        outln!(ctx, "would refuse: {} state(s) are on no store remote known here: {}", lost.len(), lost.join(", "));
    }
    let present: Vec<String> =
        states.iter().filter(|c| !c.kind.auxiliary() && repo.p.has_state(&c.id)).map(|c| c.id.clone()).collect();
    outln!(
        ctx,
        "snapshots of source states here: {} in {} state(s)",
        human(super::admin::states_bytes(repo, &present)),
        present.len()
    );
    delete_old(ctx, h, delete, true)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> String {
        format!("{:08x}-0000-4000-8000-{:012x}", n, n)
    }

    fn sref(n: u8) -> StateRef {
        StateRef { tree: Id([n; 32]), times: Id([n.wrapping_add(1); 32]) }
    }

    fn cand(n: u8, parents: &[u8], time: i64) -> Candidate {
        Candidate {
            id: id(n),
            parents: parents.iter().map(|p| id(*p)).collect(),
            time,
            kind: StateKind::Commit,
            recorded: false,
            freed: false,
            unowned_missing: false,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("layr-migrate-{name}-{}", ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_table_keeps_entries_across_loads() -> Result<()> {
        let dir = scratch("table");
        let mut t = Table::load(&dir)?;
        assert!(t.entries.is_empty());
        t.append(&id(1), &sref(1))?;
        t.append(&id(2), &sref(2))?;
        let again = Table::load(&dir)?;
        assert_eq!(again.entries.len(), 2);
        assert_eq!(again.entries[&id(2)], sref(2));
        assert!(std::fs::read_to_string(dir.join("map"))?.starts_with(TABLE_HEADER));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_line_cut_by_a_crash_is_left_out_and_removed_by_the_next_append() -> Result<()> {
        let dir = scratch("cut");
        let mut t = Table::load(&dir)?;
        t.append(&id(1), &sref(1))?;
        // A crash in the middle of the second write.
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("map"))?;
        f.write_all(format!("{} {}", id(2), sref(2).tree).as_bytes())?;
        drop(f);
        let mut t = Table::load(&dir)?;
        assert_eq!(t.entries.keys().cloned().collect::<Vec<_>>(), vec![id(1)]);
        t.append(&id(3), &sref(3))?;
        let t = Table::load(&dir)?;
        assert_eq!(t.entries.keys().cloned().collect::<Vec<_>>(), vec![id(1), id(3)]);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_bad_table_is_refused() -> Result<()> {
        let dir = scratch("bad");
        std::fs::write(dir.join("map"), format!("{TABLE_HEADER}\nnot-a-state x y\n"))?;
        assert!(Table::load(&dir).is_err());
        std::fs::write(dir.join("map"), "something else\n")?;
        assert!(Table::load(&dir).is_err());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn parents_come_before_children_and_older_states_first() {
        // 1 <- 2 <- 4 (merge of 2 and 3), 1 <- 3; 3 is older than 2.
        let states = vec![cand(4, &[2, 3], 40), cand(2, &[1], 30), cand(3, &[1], 20), cand(1, &[], 10)];
        let p = plan(&states, &BTreeMap::new());
        assert_eq!(p.todo, vec![id(1), id(3), id(2), id(4)]);
        // A child with an older clock than its parent still comes after it.
        let states = vec![cand(2, &[1], 5), cand(1, &[], 10)];
        assert_eq!(plan(&states, &BTreeMap::new()).todo, vec![id(1), id(2)]);
    }

    #[test]
    fn resume_skips_verified_and_recorded_states() {
        let mut states = vec![cand(1, &[], 10), cand(2, &[1], 20), cand(3, &[2], 30), cand(4, &[3], 40)];
        states[0].recorded = true;
        let mut table = BTreeMap::new();
        table.insert(id(2), sref(2));
        let p = plan(&states, &table);
        assert_eq!(p.done, 1);
        assert_eq!(p.unrecorded, vec![id(2)]);
        assert_eq!(p.todo, vec![id(3), id(4)]);
        // A finished run leaves nothing to do: a second run is a no-op.
        for s in &mut states {
            s.recorded = true;
        }
        let p = plan(&states, &table);
        assert!(p.todo.is_empty() && p.unrecorded.is_empty());
        assert_eq!(p.done, 4);
    }

    #[test]
    fn cache_derived_and_freed_states_are_not_moved() {
        let mut states = vec![cand(1, &[], 10), cand(2, &[], 20), cand(3, &[1], 30)];
        states[1].kind = StateKind::Cache;
        states[0].freed = true;
        let p = plan(&states, &BTreeMap::new());
        assert_eq!(p.auxiliary, 1);
        assert_eq!(p.freed, 1);
        // The parent of 3 is freed: 3 is imported with a full read.
        assert_eq!(p.todo, vec![id(3)]);
    }
}
