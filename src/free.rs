//! The free step (docs/ownership.md). `owners.rs` marks a state dead in the database when its last
//! owner goes, and queues it in `free_queue`. This module moves the storage of queued states to
//! the trash. It runs at the end of every command, after the commit and after the store lock is
//! released. A crash before it leaves the queue: the next command finishes it.
//!
//! Pins keep a state for a reader that works outside the store lock (`pin`).

use crate::store::{Project, Store};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Projects this process opened: `(store root, project name)`. `finish` frees their queues when
/// the command ends and forgets them.
static OPENED: Mutex<BTreeSet<(PathBuf, String)>> = Mutex::new(BTreeSet::new());

/// The queue that a free pass of this process left for a project (`(store root, project name)`): the states that
/// wait as the base of a live layer. The free step at the end of the command skips a project whose queue is still
/// the same, so a command that ran the pass itself (`gc --repair`) does not walk the layers again.
static LEFT_QUEUED: Mutex<BTreeMap<(PathBuf, String), Vec<String>>> = Mutex::new(BTreeMap::new());

/// Line heads that records of this command moved to (`Db::derive`): at the end of the command a thin head with a
/// long chain gets a complete tree, in the background (`keyframe_heads`).
static MOVED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// States that a line head left, and lines that a record deleted, in this command (`Db::derive`): their derived
/// data goes at the end of the command (`drop_derived`).
static LEFT: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static DELETED_LINES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// A line head left `state`.
pub fn head_left(state: &str) {
    if let Ok(mut left) = LEFT.lock() {
        left.insert(state.to_string());
    }
}

/// A record deleted the line `id`.
pub fn line_deleted(id: &str) {
    if let Ok(mut deleted) = DELETED_LINES.lock() {
        deleted.insert(id.to_string());
    }
}

/// Final states of deleted lines that get a complete tree after the command (`keyframe_later`).
static FINALS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The thin final state of a deleted line gets a complete tree in the background after this
/// command (`layr __keyframe ... final`), not in the delete: its copy of the whole tree and the
/// seal of every entry took seconds under the project lock. `layr gc` makes the ones that a stopped
/// process left.
pub fn keyframe_later(state: &str) {
    if let Ok(mut finals) = FINALS.lock() {
        finals.insert(state.to_string());
    }
}

/// A record moved a line head to `state`.
pub fn head_moved(state: &str) {
    if let Ok(mut moved) = MOVED.lock() {
        moved.insert(state.to_string());
    }
}

/// Called when a project opens: its queue is checked at the end of the command.
pub fn register(store: &Store, name: &str) {
    if let Ok(mut opened) = OPENED.lock() {
        opened.insert((store.root.clone(), name.to_string()));
    }
}

/// The free step of the projects that this command opened. Errors wait for the next command
/// (the queue keeps the states). Test only: `LAYR_TEST_EXIT_AFTER_COMMIT` exits with status 86
/// here, before any storage moves, when a queue is not empty (tests/t99-ownership-free.sh).
pub fn finish(ctx: &crate::ctx::Ctx) {
    let store = &ctx.store;
    release_held();
    let names: Vec<String> = match OPENED.lock() {
        Ok(mut opened) => {
            let names = opened.iter().filter(|(root, _)| *root == store.root).map(|(_, n)| n.clone()).collect();
            opened.retain(|(root, _)| *root != store.root);
            names
        }
        Err(_) => return,
    };
    let moved: BTreeSet<String> = MOVED.lock().map(|mut m| std::mem::take(&mut *m)).unwrap_or_default();
    let left: BTreeSet<String> = LEFT.lock().map(|mut m| std::mem::take(&mut *m)).unwrap_or_default();
    let deleted: BTreeSet<String> = DELETED_LINES.lock().map(|mut m| std::mem::take(&mut *m)).unwrap_or_default();
    let finals: BTreeSet<String> = FINALS.lock().map(|mut m| std::mem::take(&mut *m)).unwrap_or_default();
    for name in names {
        if store.project_dir(&name).is_ok_and(|dir| dir.join("meta/object-native").is_dir()) {
            if let Ok(p) = Project::open(store, &name) {
                let repo = crate::repo::Repo { p, ctx: Some(ctx) };
                if let Err(error) = crate::objstore::retire_native(&repo) {
                    eprintln!("layr: native copies of {name} remain for retry: {error:#}");
                }
            }
        }
        if !moved.is_empty() {
            keyframe_heads(store, &name, &moved);
        }
        #[cfg(target_os = "macos")]
        if let Ok(dir) = store.project_dir(&name) {
            for id in finals.iter().filter(|id| dir.join("deltas").join(id).is_dir()) {
                spawn_keyframe(&store.root, &name, id, true);
            }
        }
        if !left.is_empty() || !deleted.is_empty() {
            if let Err(e) = drop_derived(store, &name, &left, &deleted) {
                eprintln!("layr: derived data of {name} stays until the next command that changes it: {e:#}");
            }
        }
        let Ok(dir) = store.project_dir(&name) else { continue };
        let Ok(conn) =
            rusqlite::Connection::open_with_flags(crate::store::db_file(&dir), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        // Each inspection finishes its autocommit read before expiration can write owners.
        // A ref whose time ran out releases its owners now: the free step below frees what it kept.
        if expired_refs(&conn) {
            if let Err(e) = expire_refs(ctx, &name) {
                eprintln!("layr: expired refs of {name} stay until the next command: {e:#}");
            }
        }
        // A project without pending work is not opened: an open prints its trace (LAYR_TRACE).
        if !pending(&conn) || unchanged_queue(store, &name, &conn) {
            continue;
        }
        let Ok(p) = Project::open(store, &name) else { continue };
        if std::env::var_os("LAYR_TEST_EXIT_AFTER_COMMIT").is_some() && !p.db.queued_frees().unwrap_or_default().is_empty() {
            eprintln!("test: exit after the commit, before the free step of {name}");
            std::process::exit(86);
        }
        let _t = crate::trace::span("finish project");
        if let Err(e) = finish_project(&p) {
            eprintln!("layr: states of {name} stay queued until the next command: {e:#}");
        }
    }
    // Opening the projects above registered them again: they are done.
    if let Ok(mut opened) = OPENED.lock() {
        opened.retain(|(root, _)| *root != store.root);
    }
}

/// The heads in `moved` that are thin states of project `name` with a chain longer than `thin.chain`: each gets a
/// complete tree in a background process (`layr __keyframe`), so that no chain grows without end and the bases
/// below it can go. A layer walk of at most `thin.chain` steps per moved head; no walk of the project.
fn keyframe_heads(store: &Store, name: &str, moved: &BTreeSet<String>) {
    #[cfg(target_os = "macos")]
    {
        let Ok(dir) = store.project_dir(name) else { return };
        let delta = |id: &str| dir.join("deltas").join(id);
        if !moved.iter().any(|id| delta(id).is_dir()) {
            return;
        }
        let Ok(p) = Project::open(store, name) else { return };
        let max = p.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize;
        for id in moved.iter().filter(|id| p.delta_path(id).is_dir()) {
            if crate::thin::chain_len(&p, id) > max {
                spawn_keyframe(&store.root, name, id, false);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (store, name, moved);
}

/// Start `layr __keyframe <store> <project> <state> [final]` in the background; without it the next command (or
/// `gc`, for a final state) tries again.
#[cfg(target_os = "macos")]
fn spawn_keyframe(root: &std::path::Path, name: &str, id: &str, final_state: bool) {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut args: Vec<&std::ffi::OsStr> = vec!["__keyframe".as_ref(), root.as_os_str(), name.as_ref(), id.as_ref()];
    if final_state {
        args.push("final".as_ref());
    }
    let _ = std::process::Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The background keyframe (`layr __keyframe`): under the project lock, a head that still is a thin state with a
/// long chain gets a complete tree. A command that opened the layers of the state before reads them without the
/// lock, so the layer stays for `thin.view_seconds`, as a view that a command used. Then the layer goes and the
/// free step moves the bases that nothing uses now. One process per project makes complete trees at a time; another
/// one finds the work done or leaves it to the next save. The final state of a deleted line (`final_state`) gets its
/// complete tree whatever its chain, and waits for a process of another state.
#[cfg(target_os = "macos")]
pub fn keyframe_process(root: &std::path::Path, name: &str, id: &str, final_state: bool) -> Result<()> {
    let store = Store::open(root)?;
    let p = Project::open(&store, name)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.meta_dir().join("keyframe.lock"))?;
    let take = if final_state {
        rustix::fs::FlockOperation::LockExclusive
    } else {
        rustix::fs::FlockOperation::NonBlockingLockExclusive
    };
    if rustix::fs::flock(&lock, take).is_err() {
        return Ok(());
    }
    {
        let _lock = p.lock()?;
        let max = p.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize;
        if !p.delta_path(id).is_dir() || (!final_state && crate::thin::chain_len(&p, id) <= max) {
            return Ok(());
        }
        crate::thin::complete_tree(&p, id)?;
    }
    // The wait holds no lock: the keyframe of a newer head must not wait for it.
    drop(lock);
    let wait = p.config_u64("thin.view_seconds", crate::thin::VIEW_IN_USE_SECONDS);
    std::thread::sleep(std::time::Duration::from_secs(wait));
    {
        let _lock = p.lock()?;
        // A command can remove the view in the wait: then the state stays thin and the next save tries again.
        if p.state_path(id).is_dir() && p.delta_path(id).is_dir() {
            crate::thin::make_keyframe(&p, id)?;
        }
    }
    finish_project(&p)?;
    Ok(())
}

/// The derived data of lines that a record deleted (the saved status and live compare, the overlay bases) and the
/// git index of a state that no line head names any more and that has no Git commit. Only the named items are
/// looked at; nothing walks the project.
fn drop_derived(store: &Store, name: &str, left: &BTreeSet<String>, deleted: &BTreeSet<String>) -> Result<()> {
    let p = Project::open(store, name)?;
    for id in deleted {
        let _ = std::fs::remove_file(p.meta_dir().join("status").join(format!("{id}.cache")));
        let _ = std::fs::remove_file(p.meta_dir().join("overlap-live").join(format!("{id}.json")));
        if let Ok(rd) = std::fs::read_dir(p.base_dir()) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().split('@').next() == Some(id.as_str()) {
                    let _ = crate::storage::delete_tree(&e.path());
                }
            }
        }
    }
    if !left.is_empty() {
        let heads: BTreeSet<String> = p.db.lines()?.into_iter().map(|l| l.head).collect();
        for state in left.iter().filter(|s| !heads.contains(*s) && crate::model::is_uuid(s)) {
            if p.db.git_commit_of(state)?.is_none() {
                let _ = std::fs::remove_file(p.git_dir().join("layr-index").join(state));
            }
        }
    }
    Ok(())
}

/// True when a ref of project `name` has expired (a read-only query; no project open).
fn expired_refs(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM refs WHERE deleted=0 AND expires_at IS NOT NULL AND expires_at<=?1)",
        [crate::ids::now_ms()],
        |r| r.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

/// The `ref.expire` record of each expired ref of project `name`, under its lock.
fn expire_refs(ctx: &crate::ctx::Ctx, name: &str) -> Result<()> {
    let repo = crate::repo::Repo::open(ctx, name)?;
    let _lock = repo.p.lock()?;
    for expired in repo.p.db.expired_refs(crate::ids::now_ms())? {
        let mut rec = crate::model::Record::new("ref.expire");
        rec.data = serde_json::json!({"ref_change": crate::model::RefChange { name: expired.name.clone(), before: Some(expired), after: None }});
        repo.commit_record(ctx, rec)?;
    }
    Ok(())
}

/// True when a project has queued states or pins to release. Reads the database directly.
fn pending(conn: &rusqlite::Connection) -> bool {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM free_queue) OR EXISTS(SELECT 1 FROM owners WHERE kind='pin')", [], |r| {
        r.get::<_, bool>(0)
    })
    .unwrap_or(false)
}

/// True when a free pass of this process left the queue of project `name` as it is now and no pin is held: a new
/// pass would find the same waiting bases.
fn unchanged_queue(store: &Store, name: &str, conn: &rusqlite::Connection) -> bool {
    let Some(left) = LEFT_QUEUED.lock().ok().and_then(|m| m.get(&(store.root.clone(), name.to_string())).cloned()) else {
        return false;
    };
    let pins =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM owners WHERE kind='pin')", [], |r| r.get::<_, bool>(0)).unwrap_or(true);
    let Ok(mut st) = conn.prepare("SELECT state FROM free_queue ORDER BY state") else { return false };
    let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) else { return false };
    let now: Vec<String> = rows.flatten().collect();
    !pins && now == left
}

/// Move the storage of every dead queued state of a project to the trash. A state that a live
/// thin layer still uses as its base stays queued: it goes when that layer goes (APFS). Returns
/// the number of states whose storage went.
pub fn finish_project(p: &Project) -> Result<usize> {
    hold_before_free();
    // The check and the move run under the project lock: a command that re-owns a queued state
    // (an undo, a fetch, a ref) either finishes before the check, or waits and finds the state
    // here. Only the trash emptying runs after the lock.
    let gone = {
        // A caller that already holds the lock (it is not taken twice: a second lock would wait).
        let _lock = if p.holds_lock() { None } else { Some(p.lock()?) };
        release_dead_pins(p)?;
        let gone = free_queued(p)?;
        let mut left = p.db.queued_frees()?;
        left.sort();
        if let Ok(mut m) = LEFT_QUEUED.lock() {
            m.insert((p.store.root.clone(), p.name.clone()), left);
        }
        gone
    };
    if !gone.is_empty() {
        crate::store::kick_trash(&p.trash_dir()?);
    }
    Ok(gone.len())
}

/// The check and the move of the queued states. The caller holds the project lock.
fn free_queued(p: &Project) -> Result<Vec<String>> {
    let queued = p.db.queued_frees()?;
    let mut dead = Vec::new();
    let mut treeless = Vec::new();
    for state in queued {
        if p.db.is_owned(&state)? {
            // Only record owners keep it (it lost its last tree owner): an imported state can lose its tree, git
            // builds it again on read. The others keep their tree.
            if !p.db.has_tree_owner(&state)? && p.has_state(&state) {
                treeless.push(state);
                continue;
            }
            // Owned again (a record named it after its death): it is live, if its storage is here.
            if p.has_state(&state) {
                p.db.set_present(&state, true)?;
            }
            p.db.dequeue_frees(std::slice::from_ref(&state))?;
        } else {
            dead.push(state);
        }
    }
    let mut gone_trees = drop_trees(p, treeless)?;
    if dead.is_empty() {
        return Ok(std::mem::take(&mut gone_trees));
    }
    for state in &dead {
        if let Some(s) = p.db.state(state)? {
            for snap in s.caches.values() {
                p.trash(&p.cache_path(state, snap))?;
            }
        }
    }
    let mut gone = p.delete_states(&dead);
    p.db.dequeue_frees(&gone)?;
    gone.extend(gone_trees);
    Ok(gone)
}

/// The trees of the owned states `states` that only record owners keep: an imported state that git can build
/// loses its tree and stays as a record (`present=0`). A base of a present thin layer stays queued until its layers
/// go. Any other state keeps its tree and leaves the queue. Returns the states whose tree went.
fn drop_trees(p: &Project, states: Vec<String>) -> Result<Vec<String>> {
    if states.is_empty() {
        return Ok(Vec::new());
    }
    let bases: BTreeSet<String> = p.db.present_layer_bases()?.into_iter().collect();
    // A tree that git built lately stays (`gitbridge::keep_newest_built` frees it when newer ones push it out).
    let (recent, states): (Vec<String>, Vec<String>) = states.into_iter().partition(|s| crate::gitbridge::recently_built(p, s));
    for state in &recent {
        p.db.set_present(state, true)?;
    }
    p.db.dequeue_frees(&recent)?;
    let (waiting, candidates): (Vec<String>, Vec<String>) = states.into_iter().partition(|s| bases.contains(s));
    let buildable = crate::gitbridge::buildable_states(p, &candidates)?;
    // Owned again, and not a tree that git can build: it is live, as in `free_queued`.
    let keep: Vec<String> = candidates.iter().filter(|s| !buildable.contains(*s)).cloned().collect();
    for state in &keep {
        p.db.set_present(state, true)?;
    }
    p.db.dequeue_frees(&keep)?;
    let drop: Vec<String> = buildable.into_iter().collect();
    let gone = p.delete_states(&drop);
    for state in &gone {
        if !p.has_state(state) {
            p.db.set_present(state, false)?;
        }
    }
    p.db.dequeue_frees(&gone)?;
    let _ = waiting;
    Ok(gone)
}

/// Test only (tests/t109-free-recheck.sh): `LAYR_TEST_FREE_PAUSE_MS` pauses the free step before
/// its lock, so another command can re-own a queued state first.
fn hold_before_free() {
    let Some(ms) = std::env::var("LAYR_TEST_FREE_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) else { return };
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Release the pins of processes that are gone (a crash inside a pinned read).
fn release_dead_pins(p: &Project) -> Result<()> {
    let holders = p.db.pin_holders()?;
    let processes = HolderProcesses::new(&holders);
    for holder in holders {
        if !processes.alive(&holder) {
            p.db.release_holder(&holder)?;
        }
    }
    Ok(())
}

/// The holder of this process's pins: `pid:start time`.
pub fn self_holder() -> String {
    static HOLDER: OnceLock<String> = OnceLock::new();
    HOLDER
        .get_or_init(|| {
            let pid = std::process::id();
            let start = process_start(pid).or_else(|| {
                HolderProcesses::new(&[format!("{pid}:")]).start(pid).filter(|&time| time != 0).and_then(format_start)
            });
            format!("{pid}:{}", start.unwrap_or_default())
        })
        .clone()
}

/// True when the process of a holder runs. An unknown start time counts as the same process,
/// so a failed process lookup never releases a live pin.
pub fn holder_alive(holder: &str) -> bool {
    HolderProcesses::new(&[holder.to_string()]).alive(holder)
}

struct HolderProcesses(sysinfo::System);

impl HolderProcesses {
    fn new(holders: &[String]) -> Self {
        let pids: Vec<_> =
            holders.iter().filter_map(|h| h.split_once(':')?.0.parse::<u32>().ok()).map(sysinfo::Pid::from_u32).collect();
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&pids),
            true,
            sysinfo::ProcessRefreshKind::nothing().without_tasks(),
        );
        Self(system)
    }

    fn start(&self, pid: u32) -> Option<u64> {
        self.0.process(sysinfo::Pid::from_u32(pid)).map(|p| p.start_time())
    }

    fn alive(&self, holder: &str) -> bool {
        let Some((pid, start)) = holder.split_once(':') else { return false };
        let Ok(raw) = pid.parse::<u32>() else { return false };
        let Some(pid) = rustix::process::Pid::from_raw(raw as i32) else { return false };
        // Only "no such process" counts as gone: another user's process exists too.
        if matches!(rustix::process::test_kill_process(pid), Err(e) if e == rustix::io::Errno::SRCH) {
            return false;
        }
        same_start(start, self.start(raw), || process_start(raw))
    }
}

// A sysinfo match is sufficient to retain a pin. A mismatch may reflect platform or timezone
// differences, so only the original ps lookup can decide that the process was replaced.
fn same_start(stored: &str, observed: Option<u64>, fallback: impl FnOnce() -> Option<String>) -> bool {
    if observed.filter(|&time| time != 0).and_then(format_start).is_some_and(|start| start == stored) {
        return true;
    }
    match fallback() {
        Some(now) if !stored.is_empty() => now == stored,
        _ => true,
    }
}

pub(crate) fn process_start(pid: u32) -> Option<String> {
    let out =
        std::process::Command::new("ps").env("LC_ALL", "C").args(["-o", "lstart=", "-p", &pid.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

// Keep the existing `ps -o lstart` representation: older processes may still hold pins.
fn format_start(start: u64) -> Option<String> {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(start as i64, 0).single().map(|t| t.format("%a %b %e %T %Y").to_string())
}

/// Where a guard releases its pins: a deferred read owns only a path; a scoped read
/// borrows the database so its pins leave before that connection closes.
pub(crate) trait ReleasePins {
    fn release(&self, states: &[String], holder: &str) -> Result<()>;
}

impl ReleasePins for PathBuf {
    fn release(&self, states: &[String], holder: &str) -> Result<()> {
        crate::db::Db::open(self)?.pins_remove(states, holder)
    }
}

impl ReleasePins for &crate::db::Db {
    fn release(&self, states: &[String], holder: &str) -> Result<()> {
        self.pins_remove(states, holder)
    }
}

/// Pins of states that a command reads outside the store lock, or under it while another
/// process may free them (docs/ownership.md, "Pins"). Released when the guard goes.
pub(crate) struct Pin<D: ReleasePins = PathBuf> {
    db: D,
    holder: String,
    states: Vec<String>,
}

/// Pin states until the command ends. Held guards keep paths, never database connections.
pub fn pin(p: &Project, states: &[&str]) -> Result<Pin> {
    pin_using(&p.db, crate::store::db_file(&p.dir), states)
}

/// Pin a scoped read. The borrow keeps the reader's database open through release, avoiding
/// a checkpoint between the read and unpinning. An early error releases through the same guard.
pub fn pin_borrowed<'a>(p: &'a Project<'_>, states: &[&str]) -> Result<Pin<&'a crate::db::Db>> {
    pin_using(&p.db, &p.db, states)
}

fn pin_using<D: ReleasePins>(db: &crate::db::Db, release: D, states: &[&str]) -> Result<Pin<D>> {
    let holder = self_holder();
    let pinned = db.pins_add(states, &holder)?;
    let guard = Pin { db: release, holder, states: pinned };
    hold_for_tests();
    Ok(guard)
}

thread_local! {
    /// The pins of the command that runs in this thread: released at its end (`finish`).
    static HELD: std::cell::RefCell<Vec<Pin>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Keep a pin until the command ends (`finish`), so every read of the command stays protected.
pub fn hold(pin: Pin) {
    if !pin.states.is_empty() {
        HELD.with(|h| h.borrow_mut().push(pin));
    }
}

/// Release the pins of this command now.
fn release_held() {
    let pins = HELD.with(|h| std::mem::take(&mut *h.borrow_mut()));
    drop(pins);
}

/// Test only (tests/t_pin_free.sh): `LAYR_TEST_PIN_HOLD_MS` sleeps once per process after its
/// first pins, so a second process can free the pinned states while this one holds them.
fn hold_for_tests() {
    static HELD_ONCE: std::sync::Once = std::sync::Once::new();
    let Some(ms) = std::env::var("LAYR_TEST_PIN_HOLD_MS").ok().and_then(|v| v.parse::<u64>().ok()) else { return };
    HELD_ONCE.call_once(|| std::thread::sleep(std::time::Duration::from_millis(ms)));
}

impl<D: ReleasePins> Drop for Pin<D> {
    fn drop(&mut self) {
        if self.states.is_empty() {
            return;
        }
        let released = self.db.release(&self.states, &self.holder);
        if let Err(e) = released {
            eprintln!("layr: cannot release the pins of this command: {e:#}");
        }
    }
}

#[cfg(test)]
mod inspection_tests {
    use super::*;

    #[test]
    fn inspections_see_expiration_and_queue_writes_without_a_snapshot() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-inspection-{}", crate::ids::new_id()));
        let store = Store::open(&root)?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        let path = crate::store::db_file(&dir);
        let db = crate::db::Db::open(&path)?;
        let conn = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.conn.execute("INSERT INTO refs(name, target, expires_at) VALUES('old','dead',0)", [])?;
        assert!(expired_refs(&conn));
        assert!(!pending(&conn));
        // Expiration on another connection can release owners and queue states between inspections.
        db.conn.execute_batch(
            "BEGIN; UPDATE refs SET deleted=1 WHERE name='old'; INSERT INTO free_queue(state) VALUES('dead'); COMMIT;",
        )?;
        assert!(!expired_refs(&conn));
        assert!(pending(&conn));
        LEFT_QUEUED.lock().unwrap().insert((store.root.clone(), "p".into()), vec!["dead".into()]);
        assert!(unchanged_queue(&store, "p", &conn));
        db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES('dead','pin','reader')", [])?;
        assert!(!unchanged_queue(&store, "p", &conn));
        db.conn.execute("DELETE FROM owners", [])?;
        db.conn.execute("DELETE FROM free_queue", [])?;
        assert!(!pending(&conn));
        assert!(!unchanged_queue(&store, "p", &conn));
        assert!(conn.is_autocommit());
        LEFT_QUEUED.lock().unwrap().remove(&(store.root.clone(), "p".into()));
        drop(conn);
        drop(db);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    #[test]
    fn pins_survive_the_reader_connection_and_release_at_command_end() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-pin-guard-{}", crate::ids::new_id()));
        let store = Store::open(&root)?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        let path = crate::store::db_file(&dir);
        let db = crate::db::Db::open(&path)?;
        let state = crate::ids::new_id();
        db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'store','cloud')", [&state])?;
        let project = Project { store: &store, name: "p".into(), dir, id: crate::ids::new_id(), db };
        let guard = pin(&project, &[&state, &state])?;
        // Compile-time check: a deferred guard owns a path, not a connection per batch.
        let _: &PathBuf = &guard.db;
        drop(project);
        let observer = crate::db::Db::open(&path)?;
        assert!(observer.is_pinned(&state)?);
        hold(guard);
        assert!(observer.reserve_trees(&[state.clone()], "gc")?.is_empty());
        release_held();
        assert!(!observer.is_pinned(&state)?);
        assert_eq!(observer.reserve_trees(&[state], "gc")?.len(), 1);
        drop(observer);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn borrowed_pins_release_through_the_reader_database_on_early_error() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-pin-borrow-{}", crate::ids::new_id()));
        let store = Store::open(&root)?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        let path = crate::store::db_file(&dir);
        let db = crate::db::Db::open(&path)?;
        let state = crate::ids::new_id();
        db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'head','line')", [&state])?;
        let project = Project { store: &store, name: "p".into(), dir, id: crate::ids::new_id(), db };
        let observer = crate::db::Db::open(&path)?;
        let read = || -> Result<()> {
            let guard = pin_borrowed(&project, &[&state])?;
            assert!(std::ptr::eq(guard.db, &project.db));
            assert!(observer.is_pinned(&state)?);
            anyhow::bail!("test read failed");
        };
        assert_eq!(read().unwrap_err().to_string(), "test read failed");
        assert!(!observer.is_pinned(&state)?);
        assert!(observer.is_owned(&state)?);
        drop(observer);
        drop(project);
        drop(store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}

#[cfg(test)]
mod holder_tests {
    #[test]
    fn system_lookup_only_retains_and_mismatch_uses_fallback() {
        let timestamp = 1_700_000_000;
        let matching = super::format_start(timestamp).unwrap();
        assert!(super::same_start(&matching, Some(timestamp), || panic!("matching lookup must not run ps")));
        for observed in [None, Some(0), Some(timestamp)] {
            let mut called = false;
            assert!(super::same_start("legacy", observed, || {
                called = true;
                Some("legacy".into())
            }));
            assert!(called);
            assert!(!super::same_start("legacy", observed, || Some("reused".into())));
            assert!(super::same_start("legacy", observed, || None));
        }
    }

    #[test]
    fn current_holder_has_start_when_system_lookup_succeeds() {
        let pid = std::process::id();
        let processes = super::HolderProcesses::new(&[format!("{pid}:")]);
        if processes.start(pid).filter(|&time| time != 0).is_some() {
            assert!(!super::self_holder().split_once(':').unwrap().1.is_empty());
        }
    }

    #[test]
    fn detects_live_reused_and_dead_holders() {
        let holder = super::self_holder();
        assert!(super::holder_alive(&holder));
        let pid = std::process::id();
        // A sandbox may refuse to run setuid ps. In that case a live PID is
        // retained because its start time cannot be checked safely.
        assert_eq!(super::holder_alive(&format!("{pid}:1")), super::process_start(pid).is_none());
        assert!(!super::holder_alive("2147483647:1"));
        let output = std::process::Command::new("ps").env("LC_ALL", "C").args(["-o", "lstart=", "-p", &pid.to_string()]).output().unwrap();
        if output.status.success() {
            let legacy = String::from_utf8(output.stdout).unwrap();
            let processes = super::HolderProcesses::new(&[format!("{pid}:")]);
            let native = processes.start(pid).and_then(super::format_start).unwrap();
            assert_eq!(native, legacy.trim());
            assert!(super::holder_alive(&format!("{pid}:{}", legacy.trim())));
        }
    }
}
