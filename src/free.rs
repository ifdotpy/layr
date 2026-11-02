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

/// Called when a project opens: its queue is checked at the end of the command.
pub fn register(store: &Store, name: &str) {
    if let Ok(mut opened) = OPENED.lock() {
        opened.insert((store.root.clone(), name.to_string()));
    }
}

/// The free step of the projects that this command opened. Errors wait for the next command
/// (the queue keeps the states). Test only: `LAYR_TEST_EXIT_AFTER_COMMIT` exits with status 86
/// here, before any storage moves, when a queue is not empty (tests/t99-ownership-free.sh).
pub fn finish(store: &Store) {
    let names: Vec<String> = match OPENED.lock() {
        Ok(mut opened) => {
            let names = opened.iter().filter(|(root, _)| *root == store.root).map(|(_, n)| n.clone()).collect();
            opened.retain(|(root, _)| *root != store.root);
            names
        }
        Err(_) => return,
    };
    for name in names {
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
    release_dead_pins(p)?;
    let queued = p.db.queued_frees()?;
    if queued.is_empty() {
        return Ok(0);
    }
    let mut dead = Vec::new();
    for state in queued {
        if p.db.is_owned(&state)? {
            // Owned again (a record named it after its death): it is live, if its storage is here.
            if p.has_state(&state) {
                p.db.set_present(&state, true)?;
            }
            p.db.dequeue_frees(std::slice::from_ref(&state))?;
        } else {
            dead.push(state);
        }
    }
    if dead.is_empty() {
        return Ok(0);
    }
    for state in &dead {
        if let Some(s) = p.db.state(state)? {
            for snap in s.caches.values() {
                p.trash(&p.cache_path(state, snap))?;
            }
        }
    }
    let gone = p.delete_states(&dead);
    p.db.dequeue_frees(&gone)?;
    if !gone.is_empty() {
        crate::store::kick_trash(&p.store.trash_dir());
    }
    Ok(gone.len())
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
fn self_holder() -> String {
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

/// A pin of a state for this process. The state stays on disk until the guard goes.
pub struct Pin<'p, 'a> {
    p: &'p Project<'a>,
    state: String,
    holder: String,
    pinned: bool,
}

/// Pin a state that a reader uses outside the store lock. Fails when another command freed it.
pub fn pin<'p, 'a>(p: &'p Project<'a>, state: &str) -> Result<Pin<'p, 'a>> {
    let holder = self_holder();
    let pinned = p.db.pin_add(state, &holder)?;
    Ok(Pin { p, state: state.to_string(), holder, pinned })
}

impl Drop for Pin<'_, '_> {
    fn drop(&mut self) {
        if self.pinned {
            if let Err(e) = self.p.db.pin_remove(&self.state, &self.holder) {
                eprintln!("layr: cannot release the pin of {}: {e:#}", self.state);
            }
        }
    }
}
