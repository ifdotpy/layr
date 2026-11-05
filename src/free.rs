//! The free step (docs/ownership.md). `owners.rs` marks a state dead in the database when its last
//! owner goes, and queues it in `free_queue`. This module moves the storage of queued states to
//! the trash. It runs at the end of every command, after the commit and after the store lock is
//! released. A crash before it leaves the queue: the next command finishes it.
//!
//! Pins keep a state for a reader that works outside the store lock (`pin`).

use crate::store::{Project, Store};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Projects this process opened: `(store root, project name)`. `finish` frees their queues when
/// the command ends and forgets them.
static OPENED: Mutex<BTreeSet<(PathBuf, String)>> = Mutex::new(BTreeSet::new());

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
    for name in names {
        if !moved.is_empty() {
            keyframe_heads(store, &name, &moved);
        }
        if !left.is_empty() || !deleted.is_empty() {
            if let Err(e) = drop_derived(store, &name, &left, &deleted) {
                eprintln!("layr: derived data of {name} stays until the next command that changes it: {e:#}");
            }
        }
        drop_old_backups(store, &name);
        // A ref whose time ran out releases its owners now: the free step below frees what it kept.
        if expired_refs(store, &name) {
            if let Err(e) = expire_refs(ctx, &name) {
                eprintln!("layr: expired refs of {name} stay until the next command: {e:#}");
            }
        }
        // A project without pending work is not opened: an open prints its trace (LAYR_TRACE).
        if !pending(store, &name) {
            continue;
        }
        let Ok(p) = Project::open(store, &name) else { continue };
        if std::env::var_os("LAYR_TEST_EXIT_AFTER_COMMIT").is_some() && !p.db.queued_frees().unwrap_or_default().is_empty() {
            eprintln!("test: exit after the commit, before the free step of {name}");
            std::process::exit(86);
        }
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
                spawn_keyframe(&store.root, name, id);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (store, name, moved);
}

/// Start `layr __keyframe <store> <project> <state>` in the background; without it the next command tries again.
#[cfg(target_os = "macos")]
fn spawn_keyframe(root: &std::path::Path, name: &str, id: &str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .args(["__keyframe".as_ref(), root.as_os_str(), name.as_ref(), id.as_ref()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The background keyframe (`layr __keyframe`): under the project lock, a head that still is a thin state with a
/// long chain gets a complete tree. A command that opened the layers of the state before reads them without the
/// lock, so the layer stays for `thin.view_seconds`, as a view that a command used. Then the layer goes and the
/// free step moves the bases that nothing uses now. One process per project makes complete trees at a time; another
/// one finds the work done or leaves it to the next save.
#[cfg(target_os = "macos")]
pub fn keyframe_process(root: &std::path::Path, name: &str, id: &str) -> Result<()> {
    let store = Store::open(root)?;
    let p = Project::open(&store, name)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.meta_dir().join("keyframe.lock"))?;
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        return Ok(());
    }
    {
        let _lock = p.lock()?;
        let max = p.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize;
        if !p.delta_path(id).is_dir() || crate::thin::chain_len(&p, id) <= max {
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
        let mut caches_left = false;
        for state in left.iter().filter(|s| !heads.contains(*s) && crate::model::is_uuid(s)) {
            if p.db.git_commit_of(state)?.is_none() {
                let _ = std::fs::remove_file(p.git_dir().join("layr-index").join(state));
            }
            caches_left |= p.db.state(state)?.is_some_and(|s| s.caches.values().any(|n| p.cache_path(state, n).exists()));
        }
        // A head left a state with cache snapshots: it no longer keeps them, so those beyond `retention.caches` go.
        if caches_left {
            let _lock = p.lock()?;
            crate::cmd::admin::trash_excess_caches(&p)?;
        }
    }
    Ok(())
}

/// How long the database backup of a schema upgrade stays (`layr.sqlite.schema-<old>.bak`, docs/commands.md).
const BACKUP_DAYS: u64 = 14;

/// The database backup of a schema upgrade of project `name` goes when it is older than `BACKUP_DAYS`: only the
/// newest one exists (`Db::backup_before_upgrade`), so this is one read of the database folder.
fn drop_old_backups(store: &Store, name: &str) {
    let Ok(dir) = store.project_dir(name) else { return };
    let Ok(old) = crate::db::old_upgrade_backups(&crate::store::db_file(&dir), BACKUP_DAYS) else { return };
    for (path, _, _) in old {
        let _ = std::fs::remove_file(path);
    }
}

/// True when a ref of project `name` has expired (a read-only query; no project open).
fn expired_refs(store: &Store, name: &str) -> bool {
    let Ok(dir) = store.project_dir(name) else { return false };
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let Ok(conn) = rusqlite::Connection::open_with_flags(crate::store::db_file(&dir), flags) else { return false };
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
fn pending(store: &Store, name: &str) -> bool {
    let Ok(dir) = store.project_dir(name) else { return false };
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let Ok(conn) = rusqlite::Connection::open_with_flags(crate::store::db_file(&dir), flags) else { return false };
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM free_queue) OR EXISTS(SELECT 1 FROM owners WHERE kind='pin')",
        [],
        |r| r.get::<_, bool>(0),
    )
    .unwrap_or(false)
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
        free_queued(p)?
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
    for holder in p.db.pin_holders()? {
        if !holder_alive(&holder) {
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
            format!("{pid}:{}", process_start(pid).unwrap_or_default())
        })
        .clone()
}

/// True when the process of a holder runs. An unknown start time counts as the same process,
/// so a failed `ps` never releases a live pin.
pub fn holder_alive(holder: &str) -> bool {
    let Some((pid, start)) = holder.split_once(':') else { return false };
    let Ok(raw) = pid.parse::<u32>() else { return false };
    let Some(pid) = rustix::process::Pid::from_raw(raw as i32) else { return false };
    // Only "no such process" counts as gone: another user's process exists too.
    if matches!(rustix::process::test_kill_process(pid), Err(e) if e == rustix::io::Errno::SRCH) {
        return false;
    }
    match process_start(raw) {
        Some(now) if !start.is_empty() => now == start,
        _ => true,
    }
}

/// The start time of a process as `ps` prints it (the same text for the same process).
fn process_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Pins of states that a command reads outside the store lock, or under it while another process
/// may free them (docs/ownership.md, "Pins"). A guard owns the database path, so a command can
/// hold its pins (`hold`) until its end. Released when the guard goes.
pub struct Pin {
    db: PathBuf,
    holder: String,
    states: Vec<String>,
}

/// Pin states for this process. A state with no owner needs no pin (nothing frees it). A state
/// that another command already freed makes the call fail: the reader must read it again.
pub fn pin(p: &Project, states: &[&str]) -> Result<Pin> {
    let holder = self_holder();
    let mut guard = Pin { db: crate::store::db_file(&p.dir), holder: holder.clone(), states: Vec::new() };
    for state in states {
        // On a refusal the guard built so far is dropped, which releases its pins.
        if p.db.pin_add(state, &holder)? {
            guard.states.push(state.to_string());
        }
    }
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

impl Drop for Pin {
    fn drop(&mut self) {
        if self.states.is_empty() {
            return;
        }
        let released = crate::db::Db::open(&self.db).and_then(|db| {
            for state in &self.states {
                db.pin_remove(state, &self.holder)?;
            }
            Ok(())
        });
        if let Err(e) = released {
            eprintln!("layr: cannot release the pins of this command: {e:#}");
        }
    }
}
