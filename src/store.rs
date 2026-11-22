//! The store: machine identity, projects, their folders and the project database.
//!
//! ```text
//! <root>/machine/{id,name,key}        machine identity (0700)
//! <root>/projects/<name>/
//!   states/<state-id>                 read-only snapshots (0700)
//!   lines/<line-name>                 writable snapshots (0711, no listing)
//!   git/                              git object store for the remote bridge (0755)
//!   meta/db/layr.sqlite               signed operation records and derived tables (0700); meta/db
//!                                     is its own subvolume, so a snapshot elsewhere does not turn
//!                                     its fsync into a full transaction commit (100 ms against 6)
//!   meta/stage/<line-id>              staging lines (the index of `layr add`)
//!   meta/scan, meta/work              temporary snapshots
//!   meta/base/<line-id>               the full tree of a line with overlays (its layers together)
//!   meta/overlays/<overlay>/          an overlay: an internal project `<name>%<overlay>` with the
//!                                     same layout (its own states, records and readers)
//! ```
//!
//! Write order of an operation: snapshots first, then one database transaction with the record
//! and its derived rows. After a crash only a snapshot without a record can exist; `layr fsck`
//! deletes it. Records leave the database as JSONL lines (replication, remote copies, export).

use crate::db::{CommitResolution, Db, OpenedDb, PublishResult, RebuildAction, RebuildTxn};
use crate::ids;
use crate::model::{Actor, Record};
use crate::oplog;
use crate::volumes::{self, Layout};
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct Machine {
    pub id: String,
    pub name: String,
    pub key: SigningKey,
}

pub struct Store {
    pub root: PathBuf,
    pub machine: Machine,
    _version_guard: Option<crate::store_version::Guard>,
}

#[cfg(test)]
thread_local! {
    static PROJECT_OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static PROJECT_LOCK_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn project_open_count() -> usize {
    PROJECT_OPEN_COUNT.with(std::cell::Cell::get)
}

pub fn default_root() -> PathBuf {
    std::env::var_os("LAYR_ROOT").map(PathBuf::from).unwrap_or_else(|| {
        #[cfg(target_os = "macos")]
        {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/layr")
        }
        #[cfg(not(target_os = "macos"))]
        {
            PathBuf::from("/var/lib/layr")
        }
    })
}

pub(crate) fn mkdir_mode(p: &Path, mode: u32) -> Result<()> {
    if !p.exists() {
        fs::create_dir_all(p).with_context(|| format!("mkdir {}", p.display()))?;
    }
    #[cfg(target_os = "macos")]
    crate::sys::apfs_clear_acl(p)?;
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Publish a complete identity file once. A retry keeps the existing identity.
fn create_machine_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("machine file has no parent")?;
    let temporary = parent.join(format!(".identity-{}", ids::new_id()));
    let result = (|| -> Result<()> {
        let mut file =
            fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    })();
    let cleanup = fs::remove_file(&temporary);
    result?;
    cleanup?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "machine".into())
}

/// The name of a new machine: `LAYR_MACHINE_NAME` when it is set (a CI job names its short-lived machine, and a
/// trust delegation checks the name prefix), else the host name.
fn new_machine_name() -> Result<String> {
    match std::env::var("LAYR_MACHINE_NAME") {
        Ok(name) if !name.is_empty() => {
            check_name("machine", &name)?;
            Ok(name)
        }
        _ => Ok(hostname()),
    }
}

/// The store root with the links above it resolved once (for example `/tmp` to `/private/tmp`).
/// Links below the root stay refused: the no-follow walks in `sys` open every component of a
/// path, so a link inside the store cannot redirect a write.
pub fn resolve_root(root: &Path) -> Result<PathBuf> {
    fn existing(path: &Path) -> std::io::Result<PathBuf> {
        match path.canonicalize() {
            Ok(p) => Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => Ok(existing(parent)?.join(name)),
                _ => Err(e),
            },
            Err(e) => Err(e),
        }
    }
    Ok(existing(&std::path::absolute(root)?)?)
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        // Resolve aliases once at the store boundary. Paths inside a line still refuse links.
        // A refused store is not created, so the resolution comes first.
        let root = resolve_root(root)?;
        let root = root.as_path();
        // The volume files are read before anything is written. A volume that cannot be used
        // (unmounted, or another store's) is not checked here: only its projects are refused.
        // The machine id makes the volumes of this store ready (an empty id matches none).
        let layout = Layout::for_root(root)?;
        crate::rootcheck::check_store(root, &layout.ready_volume_paths())?;
        if !root.exists() {
            mkdir_mode(root, 0o755)?;
        }
        let version_guard = crate::store_version::Guard::open(root)?;
        #[cfg(not(feature = "overlayfs"))]
        if root.join(".layr-overlayfs").exists() {
            bail!("this store requires a layr build with the overlayfs feature");
        }
        crate::linepaths::register(root);
        if let Some(registry) = &crate::fs::host().registry {
            (registry.recover)(root)?;
        }
        let machine_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join(".layr-machine.lock"))?;
        anyhow::ensure!(machine_lock.metadata()?.is_file(), "the machine initialization lock is not a regular file");
        rustix::fs::flock(&machine_lock, rustix::fs::FlockOperation::LockExclusive)?;
        let mdir = root.join("machine");
        // Recheck only under the lease. The ID and key must belong to the same initialization.
        if !mdir.join("key").exists() {
            if root.join("projects").is_dir() && fs::read_dir(root.join("projects"))?.next().is_some() {
                bail!("the machine key is missing for an existing project store; restore the machine identity");
            }
            mkdir_mode(&mdir, 0o700)?;
            let id = ids::new_id();
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            create_machine_file(&mdir.join("id"), id.as_bytes())?;
            create_machine_file(&mdir.join("name"), new_machine_name()?.as_bytes())?;
            create_machine_file(&mdir.join("key"), hex::encode(key.to_bytes()).as_bytes())?;
        }
        let id = fs::read_to_string(mdir.join("id"))?.trim().to_string();
        let name = fs::read_to_string(mdir.join("name")).unwrap_or_else(|_| hostname()).trim().to_string();
        let kb = hex::decode(fs::read_to_string(mdir.join("key"))?.trim())?;
        let arr: [u8; 32] = kb.try_into().map_err(|_| anyhow!("bad machine key"))?;
        let key = SigningKey::from_bytes(&arr);
        drop(machine_lock);
        mkdir_mode(&root.join("projects"), 0o755)?;
        version_guard.ready(root)?;
        // Every volume folder must hold the identity file of this store (checked when used).
        let store = Store { root: root.to_path_buf(), machine: Machine { id, name, key }, _version_guard: Some(version_guard) };
        // A volume with its own OverlayFS registry (a project moved there) is recovered too.
        if let Some(registry) = &crate::fs::host().registry {
            for volume in store.layout()?.ready_volume_paths() {
                (registry.recover_existing)(&volume)?;
            }
        }
        // A `project move` that stopped midway is finished or rolled back before the command runs.
        crate::project_move::recover(&store);
        Ok(store)
    }

    /// The volume configuration of the store, read now (`volumes.rs`).
    pub fn layout(&self) -> Result<Layout> {
        Layout::load(&self.root, &self.machine.id)
    }

    /// The folder of a project. An overlay is the internal project `<project>%<overlay>`, kept
    /// below the project's private `meta` folder. Refused when the project's volume cannot be used.
    pub fn project_dir(&self, name: &str) -> Result<PathBuf> {
        self.layout()?.project_dir(name)
    }

    /// The root folder of the volume that holds a project (`main`: the store root).
    pub fn volume_root(&self, name: &str) -> Result<PathBuf> {
        let layout = self.layout()?;
        layout.ready(&layout.volume_of(name.split('%').next().unwrap_or(name)))
    }

    /// Record the volume of a new project (`volumes::assign`). True when this call recorded it.
    /// An overlay has no record of its own: it lives with its project.
    pub fn place_project(&self, name: &str, volume: Option<&str>) -> Result<bool> {
        if name.contains('%') {
            return Ok(false);
        }
        volumes::assign(&self.root, &self.machine.id, name, volume).map(|(_, added)| added)
    }

    /// Remove the record of a project that `place_project` recorded and whose create failed.
    pub fn unplace_project(&self, name: &str) {
        let _ = volumes::unassign(&self.root, name);
    }

    /// The names of the projects that can be used: a project on a volume that is not usable
    /// (unmounted, or not this store's) is left out, so every command skips it.
    pub fn project_names(&self) -> Result<Vec<String>> {
        let layout = self.layout()?;
        let mut v = Vec::new();
        for (_, projects) in layout.ready_projects_dirs() {
            for e in fs::read_dir(&projects)?.flatten() {
                if db_file(&e.path()).is_file() {
                    v.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        v.sort();
        v.dedup();
        Ok(v)
    }
}

/// The database of a project (`meta/db/layr.sqlite`; older projects: `meta/layr.sqlite`).
pub fn db_file(project_dir: &Path) -> PathBuf {
    let old = project_dir.join("meta").join("layr.sqlite");
    if old.is_file() {
        old
    } else {
        project_dir.join("meta").join("db").join("layr.sqlite")
    }
}

pub fn check_name(kind: &str, name: &str) -> Result<()> {
    layr_model::validation::check_name(kind, name)
}

/// Which machines an import takes records from: this machine, machines this project trusts,
/// and machines vouched for by a `machine.trust` record of a trusted machine in the same batch.
#[derive(Clone, Copy)]
pub enum Trust<'a> {
    Known,
    /// Also this machine with this key, for this batch only (the peer of a sync; the sync records
    /// its trust after the records were taken).
    Peer(&'a str, &'a str),
    /// Every machine of the batch (a new replica or a restore).
    All,
    /// Every machine that a Layr Cloud server serves: its access list decided who writes there. The records and
    /// their effects come into the project, but a machine that this project does not trust yet cannot change
    /// local trust (its `machine.trust`, `machine.untrust` and `trust.delegate` records have no effect), so peer
    /// sync, cache readers and other remotes keep the local trusted set.
    Server,
}

/// The record operations that change which machines a project trusts.
const TRUST_OPS: &[&str] = &["machine.trust", "machine.untrust", "trust.delegate"];

/// An open project is a read handle; only a locked token can publish records.
///
/// ```compile_fail,E0599
/// use layr::Project;
/// fn publish_unlocked(project: &Project<'_>) {
///     project.append(todo!(), todo!());
/// }
/// ```
///
/// ```
/// use layr::Project;
/// fn acquire(project: &Project<'_>) {
///     let _ = project.lock_maintenance();
/// }
/// ```
pub struct Project<'a> {
    pub(crate) lease: ProjectLease,
    pub store: &'a Store,
    pub name: String,
    pub dir: PathBuf,
    pub id: String,
    pub db: Db,
}

pub(crate) struct Lock {
    f: Option<fs::File>,
    /// When the lock was taken, how long the command waited for it, and the log of slow holds.
    since: std::time::Instant,
    waited: std::time::Duration,
    log: PathBuf,
    /// The project name, for `LAYR_TRACE`.
    project: String,
    /// The meta folder of the project whose lock this is (`Project::holds_lock`).
    meta: PathBuf,
    _lines: Vec<std::rc::Rc<LineLock>>,
}

/// A handle never keeps its own flock alive. Nested helpers can retain an existing lease,
/// and the final token releases the flock. The weak reference is local to this exact handle.
#[derive(Default)]
pub(crate) struct ProjectLease(std::cell::RefCell<std::rc::Weak<Lock>>);

/// Exclusive access to this project. Publication is available only through this token.
#[must_use]
pub struct LockedProject<'p> {
    project: &'p Project<'p>,
    _lock: std::rc::Rc<Lock>,
}

impl LockedProject<'_> {
    pub(crate) fn catalog(&self) -> &Db {
        &self.project.db
    }

    pub(crate) fn check(&self, project: &Project) -> Result<()> {
        anyhow::ensure!(std::ptr::eq(self.project, project), "lock belongs to another project handle");
        Ok(())
    }
}

/// Sorted, distinct line leases, acquired before the project lock.
#[must_use]
pub struct LineLeaseSet {
    meta: PathBuf,
    _locks: Vec<std::rc::Rc<LineLock>>,
}

impl LineLeaseSet {
    fn check(&self, project: &Project) -> Result<()> {
        anyhow::ensure!(self.meta == project.meta_dir(), "line leases belong to another project");
        Ok(())
    }

    pub(crate) fn require_line(&self, project: &Project, id: &str) -> Result<()> {
        self.check(project)?;
        anyhow::ensure!(
            self._locks.iter().any(|lease| lease.key.1 == id
                && lease.f.as_ref().is_some_and(|file| same_lock_file(file, &self.meta.join("line-locks").join(id)))),
            "line {id} has no held lease"
        );
        Ok(())
    }

    pub fn lock<'p>(&self, project: &'p Project<'p>) -> Result<LockedProject<'p>> {
        self.check(project)?;
        project.lock_with_lines(self._locks.clone())
    }

    #[cfg(test)]
    pub fn into_locked<'p>(self, project: &'p Project<'p>) -> Result<LockedProject<'p>> {
        self.check(project)?;
        project.lock_with_lines(self._locks)
    }

    pub fn try_lock<'p>(&'p self, project: &'p Project<'p>) -> Result<LockAttempt<LockedProject<'p>>> {
        self.check(project)?;
        project.try_lock_with_lines(self._locks.clone())
    }
}

/// Auxiliary leases precede the working-line lease for the same line. Different lines
/// always follow line-id order, including across overlapping nested requests.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LeaseKind {
    View,
    Cache,
    Line,
}

fn lease_order(id: &str) -> (&str, LeaseKind) {
    if let Some(line) = id.strip_prefix("view-") {
        (line, LeaseKind::View)
    } else if let Some(line) = id.strip_suffix("-cache") {
        (line, LeaseKind::Cache)
    } else {
        (id, LeaseKind::Line)
    }
}

/// The outcome of a non-blocking lock attempt.
#[must_use]
pub enum LockAttempt<T> {
    Acquired(T),
    Busy,
}

fn same_lock_file(file: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().zip(fs::symlink_metadata(path).ok()).is_some_and(|(a, b)| (a.dev(), a.ino()) == (b.dev(), b.ino()))
}
impl Lock {
    pub(crate) fn protects(&self, project: &Project) -> bool {
        self.meta == project.meta_dir() && self.f.as_ref().is_some_and(|file| same_lock_file(file, &self.meta.join("lock")))
    }
}
// Save preparation must not acquire its parent lock, including through native rebuild helpers.
// Missing Git inputs live in this scope, never in the published states directory.
struct SaveInputs {
    meta: PathBuf,
    sources: std::collections::HashMap<String, (crate::repo::Snap, crate::repo::Temp)>,
}
thread_local! {
    static SAVE_PREPARATIONS: std::cell::RefCell<Vec<SaveInputs>> = const { std::cell::RefCell::new(Vec::new()) };
}
pub(crate) struct SavePreparation(std::marker::PhantomData<std::rc::Rc<()>>);
impl SavePreparation {
    pub(crate) fn new(project: &Project) -> Self {
        SAVE_PREPARATIONS
            .with(|inputs| inputs.borrow_mut().push(SaveInputs { meta: project.meta_dir(), sources: Default::default() }));
        Self(Default::default())
    }
}
pub(crate) fn save_preparing(project: &Project) -> bool {
    SAVE_PREPARATIONS.with(|inputs| inputs.borrow().iter().any(|input| input.meta == project.meta_dir()))
}
pub(crate) fn save_source(project: &Project, id: &str) -> Option<crate::repo::Snap> {
    SAVE_PREPARATIONS.with(|inputs| {
        inputs
            .borrow()
            .iter()
            .rev()
            .find(|input| input.meta == project.meta_dir())
            .and_then(|input| input.sources.get(id))
            .map(|(source, _)| source.clone())
    })
}
pub(crate) fn hold_save_source(project: &Project, id: &str, source: crate::repo::Snap, temp: crate::repo::Temp) {
    SAVE_PREPARATIONS.with(|inputs| {
        inputs
            .borrow_mut()
            .iter_mut()
            .rev()
            .find(|input| input.meta == project.meta_dir())
            .expect("save input scope")
            .sources
            .insert(id.into(), (source, temp));
    });
}
pub(crate) fn take_save_inputs(project: &Project) -> Vec<crate::repo::Temp> {
    SAVE_PREPARATIONS.with(|inputs| {
        inputs
            .borrow_mut()
            .iter_mut()
            .rev()
            .find(|input| input.meta == project.meta_dir())
            .map(|input| input.sources.drain().map(|(_, (_, temp))| temp).collect())
            .unwrap_or_default()
    })
}
impl Drop for SavePreparation {
    fn drop(&mut self) {
        // Drop temporary sources after releasing the TLS borrow (their backend may use TLS).
        let inputs = SAVE_PREPARATIONS.with(|inputs| inputs.borrow_mut().pop());
        drop(inputs);
    }
}

/// The store locks this process holds. The trash is emptied when the last one goes.
static HELD_LOCKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    static LOCAL_LOCKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(feature = "overlayfs")]
pub(crate) fn holds_project_lock() -> bool {
    LOCAL_LOCKS.get() > 0
}

/// The meta folders of the projects whose lock this process holds. The runtime line-before-project check also
/// catches attempts through a separately opened handle. Publication uses typed leases.
static HELD_PROJECTS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// A wait for or a hold of the store lock this long goes to `meta/lock-slow.log`: one lock
/// serializes the commands of every user of the project, so a slow hold stops all of them.
const SLOW_LOCK: std::time::Duration = std::time::Duration::from_secs(1);
/// The log starts again above this size (bytes).
const SLOW_LOCK_LOG_MAX: u64 = 1 << 20;
/// A command that waits this long for the project lock prints the lock holder, once.
const LOCK_NOTICE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
/// The holder of a project lock, written beside `meta/lock` while the lock is held (JSON: pid,
/// command, line, start in ms). A waiting command reads it; a crashed holder leaves it stale.
const LOCK_HOLDER: &str = "lock.holder";
/// The line of this command, for the holder file (`set_holder_line`).
static HOLDER_LINE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Name the line of this command in the holder file of the project locks it takes.
pub fn set_holder_line(line: Option<&str>) {
    if let Ok(mut l) = HOLDER_LINE.lock() {
        *l = line.map(str::to_string);
    }
}
/// The command of this process for the holder file (`set_holder_command`). Without it, the process arguments: a
/// request process of the service has the arguments of `layr daemon`.
static HOLDER_COMMAND: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Name the command (its arguments without `layr`) in the holder file of the project locks it takes.
pub fn set_holder_command(argv: &[String]) {
    if let Ok(mut c) = HOLDER_COMMAND.lock() {
        *c = Some(format!("layr {}", argv.join(" ")));
    }
}

#[derive(Clone, Copy)]
enum Wait {
    Block,
    Try,
    Reserve,
}

impl Wait {
    fn acquire(self, f: &fs::File, notice: Option<&Path>) -> Result<bool> {
        use rustix::fs::FlockOperation;
        match self {
            Self::Block => {
                if let Some(meta) = notice {
                    flock_with_notice(f, meta)?;
                } else {
                    rustix::fs::flock(f, FlockOperation::LockExclusive)?;
                }
                Ok(true)
            }
            Self::Try | Self::Reserve => match rustix::fs::flock(f, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => Ok(true),
                Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
                Err(e) => Err(e.into()),
            },
        }
    }
}

/// Take the exclusive lock of `f`. A command that has to wait prints the lock holder once,
/// when the wait passes `LOCK_NOTICE_AFTER`.
fn flock_with_notice(f: &fs::File, meta: &Path) -> Result<()> {
    use rustix::fs::FlockOperation;
    match rustix::fs::flock(f, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => return Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => {}
        Err(e) => return Err(e.into()),
    }
    // The helper thread prints while the blocking wait below holds this thread.
    let (done, waiting) = std::sync::mpsc::channel::<()>();
    let meta = meta.to_path_buf();
    let notice = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = waiting.recv_timeout(LOCK_NOTICE_AFTER) {
            eprintln!("{}", waiting_message(&meta));
        }
    });
    let locked = rustix::fs::flock(f, FlockOperation::LockExclusive);
    let _ = done.send(());
    let _ = notice.join();
    locked.map_err(Into::into)
}

/// The notice for a waiting command: the live holder of `meta/lock.holder`. A holder whose pid is
/// gone is named by nothing; its file is removed when the file still holds what was read.
fn waiting_message(meta: &Path) -> String {
    const PLAIN: &str = "layr: waiting for the project lock";
    let path = meta.join(LOCK_HOLDER);
    let Ok(text) = fs::read_to_string(&path) else { return PLAIN.to_string() };
    let Ok(holder) = serde_json::from_str::<serde_json::Value>(&text) else { return PLAIN.to_string() };
    let pid = holder["pid"].as_i64().unwrap_or(0) as i32;
    if !pid_alive(pid) {
        if fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            let _ = fs::remove_file(&path);
        }
        return PLAIN.to_string();
    }
    let secs = (ids::now_ms() - holder["since_ms"].as_i64().unwrap_or(0)).max(0) / 1000;
    format!("layr: waiting for the project lock: held by '{}' (pid {pid}) for {secs} s", holder["command"].as_str().unwrap_or(""))
}

/// True when a process with this pid exists (it may belong to another user).
fn pid_alive(pid: i32) -> bool {
    use rustix::process::{test_kill_process, Pid};
    let Some(pid) = Pid::from_raw(pid) else { return false };
    !matches!(test_kill_process(pid), Err(e) if e == rustix::io::Errno::SRCH)
}

/// Write `meta/lock.holder` for this process, which holds the lock. Best effort: a lock never
/// fails for the holder file.
fn write_lock_holder(meta: &Path) {
    #[cfg(test)]
    PROJECT_LOCK_COUNT.with(|count| count.set(count.get() + 1));
    let command = HOLDER_COMMAND.lock().ok().and_then(|c| c.clone());
    let command = command.unwrap_or_else(|| format!("layr {}", std::env::args().skip(1).collect::<Vec<_>>().join(" ")));
    let command: String = command.chars().take(200).collect();
    let line = HOLDER_LINE.lock().ok().and_then(|l| l.clone());
    let holder = serde_json::json!({
        "pid": std::process::id(),
        "command": command,
        "line": line,
        "since_ms": ids::now_ms(),
    });
    let tmp = meta.join(format!("{LOCK_HOLDER}.tmp"));
    if fs::write(&tmp, holder.to_string()).is_ok() {
        let _ = fs::rename(&tmp, meta.join(LOCK_HOLDER));
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        // The steps of the hold, before the trash work after it adds its own.
        let steps = crate::trace::slowest(3);
        // The holder is still this process: no other command writes the file under the lock.
        let _ = fs::remove_file(self.meta.join(LOCK_HOLDER));
        drop(self.f.take());
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace {:>8.1} ms  store lock held: {}", held.as_secs_f64() * 1000.0, self.project);
        }
        if let Ok(mut held) = HELD_PROJECTS.lock() {
            if let Some(i) = held.iter().position(|m| *m == self.meta) {
                held.remove(i);
            }
        }
        LOCAL_LOCKS.set(LOCAL_LOCKS.get() - 1);
        if HELD_LOCKS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            // Deleting build output takes minutes: it runs after the last lock, so other
            // commands do not wait for it, and in a process of its own, so this command does
            // not wait either. `layr gc` empties it at once; LAYR_TRASH_BUDGET_MS (tests) or a
            // failed start spend a bounded time here instead. Each volume of the locks has its trash.
            let dirs = LOCK_TRASH.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default();
            for dir in dirs {
                if crate::pools::is_forking_parent() {
                    spawn_trash_emptier(&dir);
                    continue;
                }
                if EMPTY_TRASH_FULLY.load(std::sync::atomic::Ordering::SeqCst) {
                    let _t = crate::trace::span("empty trash (all)");
                    if let Err(error) = empty_all(&dir) {
                        eprintln!("layr: cannot empty trash: {error}");
                    }
                } else if std::env::var_os("LAYR_TRASH_BUDGET_MS").is_some() || !spawn_trash_emptier(&dir) {
                    let budget = trash_budget();
                    let _t = crate::trace::span(format!("empty trash ({} ms)", budget.as_millis()));
                    if let Err(error) = empty_trash(&dir, Some(budget)) {
                        eprintln!("layr: cannot empty trash: {error}");
                    }
                }
            }
        }
        if held < SLOW_LOCK && self.waited < SLOW_LOCK {
            return;
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut command = args.join(" ");
        command.truncate(300);
        // The slowest steps of the hold name its cause (`trace::span`).
        let line = format!(
            "{}\t{}\twait_ms={}\theld_ms={}\t{}\tspans={}\n",
            crate::ids::now_ms(),
            std::process::id(),
            self.waited.as_millis(),
            held.as_millis(),
            command.replace(['\t', '\n'], " "),
            steps.replace(['\t', '\n'], " ")
        );
        if fs::metadata(&self.log).map(|m| m.len() > SLOW_LOCK_LOG_MAX).unwrap_or(false) {
            let _ = fs::remove_file(&self.log);
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.log) {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The lock of one line (`meta/line-locks/<line id>`). A command that moves the head of a line, or
/// writes its working folder, holds it for the whole command. It is taken before the project lock,
/// never under it (a waiter would hold the project lock while a line holder waits for it), and two
/// lines of one command are taken in id order. Other lines do not wait for it.
struct LineLock {
    f: Option<fs::File>,
    /// Meta folder and line id, removed when the final physical lease is released.
    key: (PathBuf, String),
    since: std::time::Instant,
    waited: std::time::Duration,
    log: PathBuf,
    /// The line name, for `LAYR_TRACE` and `meta/lock-slow.log`.
    line: String,
}

thread_local! {
    static LINE_LEASES: std::cell::RefCell<std::collections::BTreeMap<(PathBuf, String), std::rc::Weak<LineLock>>> = Default::default();
}

impl Drop for LineLock {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        drop(self.f.take());
        LINE_LEASES.with(|leases| {
            leases.borrow_mut().remove(&self.key);
        });
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace {:>8.1} ms  line lock held: {}", held.as_secs_f64() * 1000.0, self.line);
        }
        if held < SLOW_LOCK && self.waited < SLOW_LOCK {
            return;
        }
        let mut command = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
        command.truncate(300);
        let line = format!(
            "{}\t{}\tline={}\twait_ms={}\theld_ms={}\t{}\n",
            crate::ids::now_ms(),
            std::process::id(),
            self.line.replace(['\t', '\n'], " "),
            self.waited.as_millis(),
            held.as_millis(),
            command.replace(['\t', '\n'], " ")
        );
        if fs::metadata(&self.log).map(|m| m.len() > SLOW_LOCK_LOG_MAX).unwrap_or(false) {
            let _ = fs::remove_file(&self.log);
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.log) {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// The trash folders (one per volume) of the project locks this process holds. The last lock
/// empties them.
static LOCK_TRASH: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// The time one command spends on emptying the trash after its last lock.
const TRASH_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// `LAYR_TRASH_BUDGET_MS` replaces it (tests).
fn trash_budget() -> std::time::Duration {
    std::env::var("LAYR_TRASH_BUDGET_MS").ok().and_then(|v| v.parse().ok()).map_or(TRASH_BUDGET, std::time::Duration::from_millis)
}
/// Name of a temporary object of `meta/work` or `meta/scan`. It ends with the pid of its creator:
/// a copy keeps the times of its source, so the age of the copy says nothing about when it was made.
pub fn temp_name() -> String {
    format!("{}.{}", crate::ids::new_id(), std::process::id())
}

/// For a name from `temp_name`: whether its creator is gone. None for a name without a pid (an
/// older layr). Cleanup keeps it because copied timestamps do not prove that it is abandoned.
pub fn creator_gone(name: &str) -> Option<bool> {
    use rustix::process::{test_kill_process, Pid};
    let pid: i32 = name.rsplit_once('.')?.1.parse().ok().filter(|p| *p > 0)?;
    let Some(pid) = Pid::from_raw(pid) else { return Some(false) };
    Some(matches!(test_kill_process(pid), Err(e) if e == rustix::io::Errno::SRCH))
}

/// Set by `layr gc`: it empties the whole trash.
pub static EMPTY_TRASH_FULLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Empty the trash after a free step (`free.rs`) that runs without the store lock: in the
/// background, or within the test budget when one is set or the start fails.
pub fn kick_trash(dir: &Path) {
    if crate::pools::is_forking_parent() {
        spawn_trash_emptier(dir);
        return;
    }
    if std::env::var_os("LAYR_TRASH_BUDGET_MS").is_some() || !spawn_trash_emptier(dir) {
        let budget = trash_budget();
        let _t = crate::trace::span(format!("empty trash ({} ms)", budget.as_millis()));
        if let Err(error) = empty_trash(dir, Some(budget)) {
            eprintln!("layr: cannot empty trash: {error}");
        }
    }
}

/// Delete what is in the trash, for at most `budget` (`None`: all of it; a started delete of
/// one file or folder is not cut). A big tree is deleted entry by entry, so the budget holds
/// for it too. Two processes may work on one tree: an entry that is gone is not an error.
/// Start `layr __empty-trash <dir>` in the background when the trash holds something: true when
/// it started (or nothing waits). The child deletes everything; one child per trash at a time.
/// False while a command only reports (`fsck`, `-n`, `--dry-run`): `Project::open` then removes no temporary copy.
pub static SWEEP_TEMPORARIES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// At most one emptier start per `TRASH_KICK_MS` from `Project::trash` in one process.
const TRASH_KICK_MS: u64 = 30_000;
static LAST_TRASH_KICK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// True when the trash emptier should start at `now_ms`: the first time, then every `TRASH_KICK_MS`.
fn trash_kick_due(last_kick: &std::sync::atomic::AtomicU64, now_ms: u64) -> bool {
    use std::sync::atomic::Ordering;
    let last = last_kick.load(Ordering::SeqCst);
    (last == 0 || now_ms.saturating_sub(last) >= TRASH_KICK_MS)
        && last_kick.compare_exchange(last, now_ms.max(1), Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

#[cfg(test)]
mod trash_kick_tests {
    #[test]
    fn the_emptier_starts_at_most_once_per_interval() {
        let last = std::sync::atomic::AtomicU64::new(0);
        assert!(super::trash_kick_due(&last, 1_000_000));
        assert!(!super::trash_kick_due(&last, 1_000_001), "a second move at once starts no second emptier");
        assert!(!super::trash_kick_due(&last, 1_000_000 + super::TRASH_KICK_MS - 1));
        assert!(super::trash_kick_due(&last, 1_000_000 + super::TRASH_KICK_MS), "the next interval starts it again");
    }
}

pub(crate) fn spawn_trash_emptier(dir: &Path) -> bool {
    if fs::read_dir(dir).map(|mut rd| rd.next().is_none()).unwrap_or(true) {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe)
        .arg("__empty-trash")
        .arg(dir)
        // Not the caller's folder: a line folder that is a mount (a fast line) must stay free to unmount.
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// `layr __empty-trash <dir>`: delete everything in the trash, unless another process does it.
pub fn empty_trash_process(dir: &Path) -> anyhow::Result<()> {
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.with_extension("lock"))?;
    match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    empty_all(dir)?;
    Ok(())
}

/// Delete the trash until a pass deletes nothing: trees that commands move here while it runs go
/// too, and a path that cannot be deleted stays. Its line goes to `<store>/trash.errors`, which
/// the next run replaces. Returns those lines.
pub fn empty_all(dir: &Path) -> Result<Vec<String>, crate::pools::PoolError> {
    loop {
        let (deleted, failed) = empty_trash(dir, None)?;
        if !failed.is_empty() || deleted == 0 || fs::read_dir(dir).map(|mut rd| rd.next().is_none()).unwrap_or(true) {
            if let Some(store) = dir.parent() {
                let _ = fs::write(store.join("trash.errors"), failed.iter().map(|line| format!("{line}\n")).collect::<String>());
            }
            return Ok(failed);
        }
    }
}

/// One pass over the trash, for at most `budget`. Returns the number of trees deleted and a
/// `path: error` line for each path that stayed.
pub fn empty_trash(dir: &Path, budget: Option<std::time::Duration>) -> Result<(usize, Vec<String>), crate::pools::PoolError> {
    if crate::pools::is_forking_parent() {
        return Err(crate::pools::PoolError::ForkingParent);
    }
    let deadline = budget.map(|b| std::time::Instant::now() + b);
    let late = || deadline.is_some_and(|d| std::time::Instant::now() >= d);
    let (mut deleted, mut failed) = (0, Vec::new());
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if late() || !empty_tree(&e.path(), &mut deleted, &mut failed)? {
                break;
            }
        }
    }
    Ok((deleted, failed))
}

/// Delete `path`, and what is in it first. Counts each tree deleted and records each failure;
/// false when the budget ended before `path` was gone.
fn empty_tree(path: &Path, deleted: &mut usize, failed: &mut Vec<String>) -> Result<bool, crate::pools::PoolError> {
    if fs::symlink_metadata(path).is_err() {
        return Ok(true);
    }
    match crate::fs::remove(path) {
        Ok(()) => *deleted += 1,
        // Another emptier deleted it, or an entry of it, at the same time: that is no failure. A path that is still
        // here goes in the next pass.
        Err(error) if error.downcast_ref::<crate::fs::Cleanup>().is_some() => {
            failed.push(format!("{}: {error:#}", path.display()));
            return Ok(false);
        }
        Err(_) if fs::symlink_metadata(path).is_err() => *deleted += 1,
        Err(e)
            if e.chain()
                .any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)) => {}
        Err(e) => failed.push(format!("{}: {e:#}", path.display())),
    }
    Ok(true)
}

impl<'a> Project<'a> {
    pub fn states_dir(&self) -> PathBuf {
        self.dir.join("states")
    }
    pub fn lines_dir(&self) -> PathBuf {
        self.dir.join("lines")
    }
    pub fn meta_dir(&self) -> PathBuf {
        self.dir.join("meta")
    }
    /// The trash of this project's volume: trees that left a project and that no record or command
    /// reads any more. Under a lock a command only renames a tree here (same file system); the
    /// last `Lock` empties it.
    pub fn trash_dir(&self) -> Result<PathBuf> {
        let layout = self.store.layout()?;
        layout.trash_of(&layout.volume_of(&self.name))
    }
    /// True when this process holds the lock of this project (taking it again would wait forever).
    pub fn holds_lock(&self) -> bool {
        HELD_PROJECTS.lock().map(|held| held.contains(&self.meta_dir())).unwrap_or(false)
    }
    pub fn git_dir(&self) -> PathBuf {
        self.dir.join("git")
    }
    /// Path of a state snapshot. Only a canonical UUID names one; anything else gives a path
    /// that never exists, so no record can make the service touch another path.
    pub fn state_path(&self, id: &str) -> PathBuf {
        if crate::model::is_uuid(id) {
            self.states_dir().join(id)
        } else {
            self.states_dir().join(".invalid-id")
        }
    }
    /// The thin layer of a state: the entries that differ from its base state (APFS).
    pub fn delta_path(&self, id: &str) -> PathBuf {
        if crate::model::is_uuid(id) {
            self.dir.join("deltas").join(id)
        } else {
            self.dir.join("deltas").join(".invalid-id")
        }
    }
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn deltas_dir(&self) -> PathBuf {
        self.dir.join("deltas")
    }
    /// Remove the tree `path` of this project. On APFS deleting build output takes minutes, so
    /// under a lock the tree is only renamed into the trash of its volume, which `Lock` empties after
    /// the last lock. A tree that cannot move, and any tree on the other file systems (whose
    /// snapshots delete at once), is deleted in place.
    pub fn trash(&self, path: &Path) -> Result<()> {
        self.trash_with(path, HELD_LOCKS.load(std::sync::atomic::Ordering::SeqCst) > 0)
    }

    /// Queue build output without holding the project lock during its removal.
    pub fn trash_background(&self, path: &Path) -> Result<()> {
        self.trash_with(path, true)
    }

    fn trash_with(&self, path: &Path, background: bool) -> Result<()> {
        let _t = crate::trace::span(format!("trash {}", path.display()));
        if !crate::fs::host().namespace.managed_or_present(path)? {
            return Ok(());
        }
        // A subvolume whose only nested subvolume is its .git goes at once, with no search
        // (a line folder); one with more nested subvolumes waits in the trash.
        #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
        if HELD_LOCKS.load(std::sync::atomic::Ordering::SeqCst) > 0 && destroy_known(path) {
            return Ok(());
        }
        if background {
            let dir = self.trash_dir()?;
            let name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
            let to = dir.join(format!("{name}.{}", crate::ids::new_id()));
            if fs::create_dir_all(&dir).is_ok() && crate::fs::move_tree(path, &to).is_ok() {
                if trash_kick_due(&LAST_TRASH_KICK_MS, crate::ids::now_ms().max(0) as u64) {
                    spawn_trash_emptier(&dir);
                }
                return Ok(());
            }
        }
        crate::fs::remove(path)
    }
    /// Delete the trees of a state. On APFS a state that another thin state uses as its base
    /// stays until that state goes (`thin::delete_state`); this returns false then.
    pub fn delete_state(&self, id: &str) -> Result<bool> {
        #[cfg(target_os = "macos")]
        return crate::thin::delete_state(self, id);
        #[cfg(not(target_os = "macos"))]
        {
            crate::fs::remove(&self.state_path(id))?;
            Ok(true)
        }
    }
    /// Delete the trees of many states; on APFS with one walk over the thin layers
    /// (`thin::delete_states`). A state kept as a base counts as handled, as in `delete_state`.
    pub fn delete_states(&self, ids: &[String]) -> Vec<String> {
        #[cfg(target_os = "macos")]
        if let Ok(gone) = crate::thin::delete_states(self, ids) {
            return ids.iter().filter(|id| gone.contains(id.as_str())).cloned().collect();
        }
        ids.iter().filter(|id| self.delete_state(id).is_ok()).cloned().collect()
    }
    /// True when the content of a state is on this machine: a complete tree or a thin layer.
    pub fn has_state(&self, id: &str) -> bool {
        self.state_path(id).exists() || self.delta_path(id).exists()
    }
    /// The working folder of a line: the path recorded by `layr line move`, the folder that
    /// `line rename` kept, else `lines/<name>`.
    pub fn line_path(&self, name: &str) -> PathBuf {
        match self.db.line_by_name(name).ok().flatten() {
            Some(l) => self.line_folder(&l.id, name),
            None => self.default_line_path(name),
        }
    }
    /// The folder of a line in the store, `lines/<name>`.
    pub fn default_line_path(&self, name: &str) -> PathBuf {
        if check_name("line", name).is_ok() || (name.starts_with(".try-") && name[5..].chars().all(|c| c.is_ascii_hexdigit())) {
            self.lines_dir().join(name)
        } else {
            self.lines_dir().join(".invalid-name")
        }
    }
    /// Path of a cache snapshot `<state-id>@<path>`.
    pub fn cache_path(&self, state: &str, name: &str) -> PathBuf {
        if crate::model::is_cache_name(name, state) {
            self.states_dir().join(name)
        } else {
            self.states_dir().join(".invalid-cache")
        }
    }
    pub fn stage_path(&self, line_id: &str) -> PathBuf {
        self.meta_dir().join("stage").join(line_id)
    }
    pub fn scan_dir(&self) -> PathBuf {
        self.meta_dir().join("scan")
    }
    pub fn work_dir(&self) -> PathBuf {
        self.meta_dir().join("work")
    }
    /// The full trees of lines with overlays, by line id (derived; rebuilt when missing).
    pub fn base_dir(&self) -> PathBuf {
        self.meta_dir().join("base")
    }
    /// True for an overlay: the internal project `<name>%<overlay>`.
    pub fn is_overlay(&self) -> bool {
        self.name.contains('%')
    }

    /// A new project on the default volume (the tests make most of their projects this way).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn create(store: &'a Store, name: &str, actor: &Actor, config: serde_json::Value) -> Result<Project<'a>> {
        Self::create_on(store, name, None, actor, config)
    }

    /// A new project on the named volume (`None`: the default volume).
    pub fn create_on(
        store: &'a Store,
        name: &str,
        volume: Option<&str>,
        actor: &Actor,
        config: serde_json::Value,
    ) -> Result<Project<'a>> {
        check_name("project", name)?;
        Self::create_in(store, name, volume, actor, config)
    }

    /// The internal project of an overlay, `<project>%<overlay>` (`layr overlay create` only).
    pub fn create_overlay(store: &'a Store, name: &str, actor: &Actor, config: serde_json::Value) -> Result<Project<'a>> {
        if !name.contains('%') {
            bail!("not an overlay name: {name}");
        }
        Self::create_in(store, name, None, actor, config)
    }

    /// Record the volume of the project, then make its folder. A failure removes the record
    /// again when this call made it.
    fn create_in(
        store: &'a Store,
        name: &str,
        volume: Option<&str>,
        actor: &Actor,
        config: serde_json::Value,
    ) -> Result<Project<'a>> {
        let added = store.place_project(name, volume)?;
        let result = Self::create_folder(store, name, actor, config);
        if result.is_err() && added && !store.project_dir(name)?.exists() {
            store.unplace_project(name);
        }
        result
    }

    /// Make a new project folder. A path that exists (a partial folder too) is refused and left
    /// unchanged. A failure after this call made the folder removes the whole folder.
    fn create_folder(store: &'a Store, name: &str, actor: &Actor, config: serde_json::Value) -> Result<Project<'a>> {
        let dir = store.project_dir(name)?;
        let volume_root = store.volume_root(name)?;
        if !crate::fs::host().namespace.supported(&volume_root) {
            #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
            bail!("{} is not on btrfs", volume_root.display());
            #[cfg(target_os = "macos")]
            bail!("{} is not on APFS", volume_root.display());
            #[cfg(feature = "overlayfs")]
            bail!("OverlayFS requires a local store and mount permission");
        }
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        }
        // The exclusive create decides who owns the folder: only a successful create is ours.
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("{} already exists; nothing was changed", dir.display())
            }
            Err(e) => return Err(e.into()),
        }
        let made = (|| -> PublishResult<Project<'a>> {
            mkdir_mode(&dir, 0o755)?;
            mkdir_mode(&dir.join("states"), 0o700)?;
            mkdir_mode(&dir.join("deltas"), 0o700)?;
            mkdir_mode(&dir.join("lines"), 0o711)?;
            mkdir_mode(&dir.join("meta"), 0o700)?;
            for d in ["stage", "scan", "work"] {
                mkdir_mode(&dir.join("meta").join(d), 0o700)?;
            }
            crate::fs::create(&dir.join("meta").join("db"))?;
            std::fs::set_permissions(dir.join("meta").join("db"), fs::Permissions::from_mode(0o700))?;
            let id = ids::new_id();
            let db = Db::open(&db_file(&dir))?;
            let p = Project { lease: Default::default(), store, name: name.to_string(), dir: dir.clone(), id: id.clone(), db };
            let mut rec = crate::model::DraftRecord::new("project");
            rec.data = serde_json::json!({"name": name, "id": id, "config": config});
            p.lock_maintenance()?.append(rec, actor)?;
            Ok(p)
        })();
        match made {
            Ok(p) => Ok(p),
            Err(e) => {
                if matches!(e, CommitResolution::NotCommitted(_)) {
                    let _ = crate::fs::remove(&dir);
                }
                Err(e.into())
            }
        }
    }

    /// Folders of a project without records (for a restore or a replica).
    pub fn create_empty(store: &'a Store, name: &str) -> Result<Project<'a>> {
        Self::create_empty_on(store, name, None)
    }

    /// Folders of a project without records on the named volume (`None`: the default volume).
    pub fn create_empty_on(store: &'a Store, name: &str, volume: Option<&str>) -> Result<Project<'a>> {
        check_name("project", name)?;
        let added = store.place_project(name, volume)?;
        let result = Self::create_empty_in(store, name);
        if result.is_err() && added {
            store.unplace_project(name);
        }
        result
    }

    pub fn create_empty_overlay(store: &'a Store, name: &str) -> Result<Project<'a>> {
        if !name.contains('%') {
            bail!("not an overlay name: {name}");
        }
        Self::create_empty_in(store, name)
    }

    fn create_empty_in(store: &'a Store, name: &str) -> Result<Project<'a>> {
        let dir = store.project_dir(name)?;
        if db_file(&dir).exists() {
            bail!("project '{name}' already exists");
        }
        let volume_root = store.volume_root(name)?;
        if !crate::fs::host().namespace.supported(&volume_root) {
            #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
            bail!("{} is not on btrfs", volume_root.display());
            #[cfg(target_os = "macos")]
            bail!("{} is not on APFS", volume_root.display());
            #[cfg(feature = "overlayfs")]
            bail!("OverlayFS requires a local store and mount permission");
        }
        mkdir_mode(&dir, 0o755)?;
        mkdir_mode(&dir.join("states"), 0o700)?;
        mkdir_mode(&dir.join("deltas"), 0o700)?;
        mkdir_mode(&dir.join("lines"), 0o711)?;
        mkdir_mode(&dir.join("meta"), 0o700)?;
        for d in ["stage", "scan", "work"] {
            mkdir_mode(&dir.join("meta").join(d), 0o700)?;
        }
        if !dir.join("meta").join("db").exists() {
            crate::fs::create(&dir.join("meta").join("db"))?;
            std::fs::set_permissions(dir.join("meta").join("db"), fs::Permissions::from_mode(0o700))?;
        }
        let db = Db::open(&db_file(&dir))?;
        Ok(Project { lease: Default::default(), store, name: name.to_string(), dir, id: String::new(), db })
    }

    /// Open a project that may not have its records yet.
    pub fn open_partial(store: &'a Store, name: &str) -> Result<Project<'a>> {
        let dir = store.project_dir(name)?;
        let mut p = Self::open_catalog(store, name, dir, Wait::Block)?
            .ok_or_else(|| anyhow!("blocking project open unexpectedly busy"))?;
        p.id = p.project_id().unwrap_or_default();
        Ok(p)
    }

    pub fn open(store: &'a Store, name: &str) -> Result<Project<'a>> {
        Self::open_wait(store, name, Some(Wait::Block))?.ok_or_else(|| anyhow!("blocking project open unexpectedly busy"))
    }

    /// Opens a project to read its records only: a live APFS commit of it is not waited for. An overlay fetch
    /// that held the overlay's lock opened the parent project to check the records' authority, and waited
    /// for a save that held the parent's lock and waited for that overlay's lock: a deadlock of `follow` and
    /// `save` (2026-11-15). The database serves the committed records while a commit runs.
    pub fn open_records(store: &'a Store, name: &str) -> Result<Project<'a>> {
        Self::open_wait(store, name, None)?.ok_or_else(|| anyhow!("project open without commit recovery unexpectedly busy"))
    }

    /// Defer maintenance when a live APFS commit needs recovery under the lock.
    pub fn open_maintenance(store: &'a Store, name: &str) -> Result<Option<Project<'a>>> {
        Self::open_wait(store, name, Some(Wait::Try))
    }

    /// `wait`: how to take the lock for the recovery of a pending APFS commit; None leaves it to the next open.
    fn open_wait(store: &'a Store, name: &str, wait: Option<Wait>) -> Result<Option<Project<'a>>> {
        #[cfg(not(target_os = "macos"))]
        let _ = wait;
        #[cfg(test)]
        PROJECT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
        let _t = crate::trace::span(format!("open project {name}"));
        let dir = store.project_dir(name)?;
        {
            let _t = crate::trace::span("project folder validation");
            crate::rootcheck::check_project_folder(&dir)?;
        }
        if !db_file(&dir).is_file() {
            bail!("project '{name}' not found");
        }
        let Some(mut p) = ({
            let _t = crate::trace::span("project database open");
            // A stale catalog migrates first, also for `open_records`: only the wait for an APFS commit is skipped.
            Self::open_catalog(store, name, dir, wait.unwrap_or(Wait::Block))?
        }) else {
            return Ok(None);
        };
        {
            let _t = crate::trace::span("project rollback validation");
            crate::rootcheck::check_project(&p)?;
        }
        {
            let _t = crate::trace::span("project identity");
            p.id = p.project_id()?;
        }
        crate::free::register(store, name);
        #[cfg(target_os = "macos")]
        {
            let _t = crate::trace::span("project commit recovery");
            if let Some(wait) = wait {
                if crate::apfs_commit::pending(&p)? {
                    // Wait for a live commit, or recover the index after an interrupted one.
                    let Some(_lock) = p.lock_wait(wait, Vec::new())? else { return Ok(None) };
                }
            }
        }
        p.remove_dead_temporaries();
        Ok(Some(p))
    }

    /// Temporary copies whose creator process is gone (a crash, a kill, a stopped migration): each
    /// names its pid (`<id>.<pid>`). Every command that opens the project moves them to the trash, so
    /// they do not wait for `gc --repair`. A copy without a pid, or of a live pid, stays.
    fn remove_dead_temporaries(&self) {
        if !SWEEP_TEMPORARIES.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let _t = crate::trace::span("dead temporaries");
        for dir in [self.scan_dir(), self.work_dir()] {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                if creator_gone(&entry.file_name().to_string_lossy()) == Some(true) {
                    let _ = self.trash(&entry.path());
                }
            }
        }
    }

    /// Take the project id from the records (after a replica or a restore received them).
    pub fn reload_id(&mut self) -> Result<()> {
        self.id = self.project_id()?;
        Ok(())
    }

    fn project_id(&self) -> Result<String> {
        self.db.project_id()?.ok_or_else(|| anyhow!("project record missing"))
    }

    pub fn line_leases(&self, lines: impl IntoIterator<Item = (String, String)>) -> Result<LineLeaseSet> {
        let mut lines: Vec<_> = lines.into_iter().collect();
        lines.sort_by(|a, b| lease_order(&a.0).cmp(&lease_order(&b.0)));
        lines.dedup_by(|a, b| a.0 == b.0);
        let locks = lines
            .iter()
            .map(|(id, name)| {
                self.line_lock_wait(id, name, Wait::Block)?.ok_or_else(|| anyhow!("blocking line lock unexpectedly busy"))
            })
            .collect::<Result<_>>()?;
        Ok(LineLeaseSet { meta: self.meta_dir(), _locks: locks })
    }

    /// The lock of one line for a whole command (`LineLock`). Taking it under the project lock of
    /// this process is refused: that order can deadlock with a command that holds the line lock.
    /// A nested take retains the same physical lease until its final holder releases it.
    pub fn line_lock(&self, id: &str, name: &str) -> Result<LineLeaseSet> {
        self.line_leases([(id.to_string(), name.to_string())])
    }

    pub fn try_line_lock(&self, id: &str, name: &str) -> Result<LockAttempt<LineLeaseSet>> {
        Ok(match self.line_lock_wait(id, name, Wait::Try)? {
            Some(lock) => LockAttempt::Acquired(LineLeaseSet { meta: self.meta_dir(), _locks: vec![lock] }),
            None => LockAttempt::Busy,
        })
    }

    fn line_lock_wait(&self, id: &str, name: &str, wait: Wait) -> Result<Option<std::rc::Rc<LineLock>>> {
        anyhow::ensure!(!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "invalid line id '{id}'");
        let meta = self.meta_dir();
        if HELD_PROJECTS.lock().map(|held| held.contains(&meta)).unwrap_or(false) {
            bail!("internal error: line '{name}' locked while the project lock is held (line locks come first)");
        }
        let log = meta.join("lock-slow.log");
        let key = (meta.clone(), id.to_string());
        if let Some(held) = LINE_LEASES.with(|leases| leases.borrow().get(&key).and_then(std::rc::Weak::upgrade)) {
            return Ok(Some(held));
        }
        let ordered = LINE_LEASES.with(|leases| {
            leases.borrow().iter().all(|((held_meta, held_id), lease)| {
                held_meta != &meta || lease.strong_count() == 0 || lease_order(held_id) <= lease_order(id)
            })
        });
        anyhow::ensure!(ordered, "line '{name}' locked below an already-held line id (line locks must be sorted)");
        let dir = meta.join("line-locks");
        fs::create_dir_all(&dir)?;
        let f = fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join(id))?;
        let asked = std::time::Instant::now();
        if !wait.acquire(&f, None)? {
            return Ok(None);
        }
        if std::env::var_os("LAYR_TRACE").is_some() {
            eprintln!("trace {:>8.1} ms  line lock taken: {name}", asked.elapsed().as_secs_f64() * 1000.0);
        }
        let guard =
            LineLock { f: Some(f), key, since: std::time::Instant::now(), waited: asked.elapsed(), log, line: name.to_string() };
        // Tests hold a line lock for a while (LAYR_TEST_LINE_LOCK_MS), to check that other lines and
        // other commands of the line wait only as the design says. The hold time includes the sleep.
        if let Some(ms) = std::env::var("LAYR_TEST_LINE_LOCK_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let guard = std::rc::Rc::new(guard);
        LINE_LEASES.with(|leases| leases.borrow_mut().insert((meta, id.to_string()), std::rc::Rc::downgrade(&guard)));
        Ok(Some(guard))
    }

    /// Borrow the live lease of this handle, or acquire one for a standalone helper.
    pub fn lock_or_borrow(&self) -> Result<LockedProject<'_>> {
        if let Some(lock) = self.lease.0.borrow().upgrade() {
            return Ok(LockedProject { project: self, _lock: lock });
        }
        self.lock_maintenance()
    }

    /// Acquire a project-only lease for maintenance, replication, or initial creation.
    /// Working-line commands acquire their line leases first.
    pub fn lock_maintenance(&self) -> Result<LockedProject<'_>> {
        self.lock_with_lines(Vec::new())
    }

    fn lock_with_lines(&self, lines: Vec<std::rc::Rc<LineLock>>) -> Result<LockedProject<'_>> {
        let lock = self.lock_wait(Wait::Block, lines)?.ok_or_else(|| anyhow!("blocking project lock unexpectedly busy"))?;
        Ok(self.locked_token(lock))
    }

    pub(crate) fn borrow_reserved(&self, lock: &std::rc::Rc<Lock>) -> LockedProject<'_> {
        assert!(lock.protects(self));
        *self.lease.0.borrow_mut() = std::rc::Rc::downgrade(lock);
        LockedProject { project: self, _lock: lock.clone() }
    }

    pub(crate) fn locked_token(&self, lock: Lock) -> LockedProject<'_> {
        let lock = std::rc::Rc::new(lock);
        *self.lease.0.borrow_mut() = std::rc::Rc::downgrade(&lock);
        LockedProject { project: self, _lock: lock }
    }

    pub fn try_lock(&self) -> Result<LockAttempt<LockedProject<'_>>> {
        self.try_lock_with_lines(Vec::new())
    }

    fn try_lock_with_lines(&self, lines: Vec<std::rc::Rc<LineLock>>) -> Result<LockAttempt<LockedProject<'_>>> {
        Ok(match self.lock_wait(Wait::Try, lines)? {
            Some(lock) => LockAttempt::Acquired(self.locked_token(lock)),
            None => LockAttempt::Busy,
        })
    }

    pub(crate) fn try_reserve(&self) -> Result<Option<Lock>> {
        self.lock_wait(Wait::Reserve, Vec::new())
    }

    fn lock_wait(&self, wait: Wait, lines: Vec<std::rc::Rc<LineLock>>) -> Result<Option<Lock>> {
        let preparing = save_preparing(self);
        assert!(
            !preparing || std::env::var_os("LAYR_TEST_SAVE_ASSERT_NO_PROJECT_LOCK").is_none(),
            "project lock acquired during save preparation"
        );
        anyhow::ensure!(!preparing, "project lock acquired during save preparation");
        #[cfg(feature = "overlayfs")]
        crate::repo::save_barrier("project_lock")?;
        let Some(lock) = lock_catalog(self.store, &self.name, &self.meta_dir(), wait, lines)? else { return Ok(None) };
        #[cfg(target_os = "macos")]
        if matches!(wait, Wait::Reserve) {
            if crate::apfs_commit::pending(self)? {
                return Ok(None);
            }
        } else {
            crate::apfs_commit::recover(self)?;
        }
        Ok(Some(lock))
    }

    fn state_present(&self) -> impl Fn(&str) -> bool + '_ {
        move |id: &str| self.has_state(id)
    }

    /// Rebuild a stale catalog before constructing a project that can answer normal queries.
    fn open_catalog(store: &'a Store, name: &str, dir: PathBuf, wait: Wait) -> Result<Option<Project<'a>>> {
        let make =
            |db| Project { lease: Default::default(), store, name: name.to_string(), dir: dir.clone(), id: String::new(), db };
        match Db::inspect(&db_file(&dir))? {
            OpenedDb::Current(db) => Ok(Some(make(db))),
            OpenedDb::Stale(stale) => {
                // A parent may need its own migration. Resolve it before the overlay lease,
                // using the same wait policy so maintenance yields without holding either writer.
                let parent = if let Some((parent, _)) = name.split_once('%') {
                    let parent_dir = store.project_dir(parent)?;
                    if db_file(&parent_dir).is_file() {
                        let Some(parent) = Self::open_catalog(store, parent, parent_dir, wait)? else { return Ok(None) };
                        Some(parent)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let Some(lease) = lock_catalog(store, name, &dir.join("meta"), wait, Vec::new())? else { return Ok(None) };
                let lease = std::rc::Rc::new(lease);
                let (project, committed) = stale.rebuild(&lease, |db, tx, action| {
                    let p = make(db);
                    *p.lease.0.borrow_mut() = std::rc::Rc::downgrade(&lease);
                    if action == RebuildAction::Replay {
                        let terms = match &parent {
                            Some(parent) => parent.trust_terms(&[])?,
                            None if p.is_overlay() => Default::default(),
                            None => p.trust_terms(&[])?,
                        };
                        p.lock_or_borrow()?.replay_derived(tx, &terms)?;
                    }
                    Ok(p)
                })?;
                crate::free::release_effects(committed.effects);
                if let Some(error) = committed.reported_error {
                    return Err(error);
                }
                Ok(Some(project))
            }
        }
    }

    /// The layers on disk and differences from their base rows. Read once so `gc` repairs
    /// the table using the same map.
    pub fn layer_bases_check(&self) -> Result<(std::collections::BTreeMap<String, String>, Vec<String>)> {
        let disk = self.layer_bases_on_disk()?;
        let rows = self.db.bases_rows()?;
        let short = crate::ids::short;
        let mut out = Vec::new();
        for (state, base) in &disk {
            match rows.get(state) {
                None => out.push(format!("missing row: layer {} over {}", short(state), short(base))),
                Some(row) if row != base => {
                    out.push(format!("wrong base: layer {} row {} disk {}", short(state), short(row), short(base)))
                }
                Some(_) => {}
            }
        }
        for state in rows.keys() {
            if !disk.contains_key(state) {
                out.push(format!("stale row: layer {} is not on disk", short(state)));
            }
        }
        Ok((disk, out))
    }

    /// The base of each thin layer on disk. Layers exist on APFS only.
    pub(crate) fn layer_bases_on_disk(&self) -> Result<std::collections::BTreeMap<String, String>> {
        #[cfg(target_os = "macos")]
        {
            crate::thin::bases(self)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(std::collections::BTreeMap::new())
        }
    }

    /// The trust rules of this project, or its parent for an overlay. `more` are incoming records not stored yet.
    pub fn trust_terms(&self, more: &[&Record]) -> Result<layr_store::trust::Rules> {
        let parent;
        let db = match self.name.split_once('%') {
            Some((project, _)) => {
                let path = db_file(&self.store.project_dir(project)?);
                if !path.is_file() {
                    return Ok(Default::default());
                }
                parent = Project::open_partial(self.store, project)?;
                &parent.db
            }
            None => &self.db,
        };
        let (mut creator, mut records) = db.trust_history()?;
        if !self.is_overlay() {
            for record in more {
                match record.op.as_str() {
                    "project" => {
                        creator.get_or_insert(record.machine.clone());
                    }
                    "machine.trust" | "machine.untrust" | "trust.delegate" => records.push((*record).clone()),
                    _ => {}
                }
            }
        }
        if records.is_empty() {
            return Ok(Default::default());
        }
        let Some(creator) = creator else { return Ok(Default::default()) };
        Ok(layr_store::trust::rules(
            &creator,
            records.iter().map(|r| layr_store::trust::TrustRecord { signer: &r.machine, time: r.time, op: &r.op, data: &r.data }),
        ))
    }

    /// `refusal` for a record that is applied next, after every record before it: a merge machine's record is
    /// checked against the derived tables as they are now (`layr_store::trust::Terms::refusal_in`).
    pub fn refusal_in_order(&self, terms: &layr_store::trust::Rules, record: &Record) -> Result<Option<&'static str>> {
        self.refusal_with(terms, record, true)
    }

    /// Why a record has no effect here (`layr_store::trust::Terms::refusal`); None when it counts. A machine always
    /// takes its own records. Outside an ordered replay a merge machine's record is checked by its shape only, which
    /// accepts at least what the replay accepts: owners and readers computed this way keep every state it keeps.
    pub fn refusal(&self, terms: &layr_store::trust::Rules, record: &Record) -> Result<Option<&'static str>> {
        self.refusal_with(terms, record, false)
    }

    fn refusal_with(&self, terms: &layr_store::trust::Rules, record: &Record, in_order: bool) -> Result<Option<&'static str>> {
        if record.machine == self.store.machine.id {
            return Ok(None);
        }
        if record.op == "machine.trust" && !terms.trust_counts(&record.machine, record.time, &record.data) {
            return Ok(Some("a trust outside the delegation of its broker"));
        }
        let Some(limits) = terms.get(&record.machine) else { return Ok(None) };
        // A line the record moves is its own when it is new, or when a record of the same machine made it.
        let mut lines = Vec::new();
        for (id, change) in &record.lines {
            let own = match self.db.line_machine(id)? {
                None => matches!(change.transition, crate::model::LineTransition::Create { .. }),
                Some(machine) => machine == record.machine,
            };
            let name = change.after().map(|after| after.name.as_str());
            let renamed = change.before().is_some_and(|before| Some(before.name.as_str()) != name);
            let after = change.after();
            lines.push(layr_store::trust::LineMove {
                id,
                new: matches!(change.transition, crate::model::LineTransition::Create { .. }),
                name,
                own,
                renamed,
                head: after.map(|a| a.head.as_str()),
                owner: after.map(|a| a.owner),
                machine: after.map(|a| a.machine.as_str()),
            });
        }
        let payload = layr_store::trust::Payload { states: record.states.iter().map(|s| s.line.as_deref()).collect(), lines };
        let overlay = self.name.split_once('%').map(|(_, name)| name);
        if !in_order {
            return Ok(limits.refusal(record.time, &record.op, &record.data, overlay, &payload));
        }
        let current = DbCurrent { project: self, terms };
        Ok(limits.refusal_in(record.time, &record.op, &record.data, overlay, &payload, &record.machine, &current))
    }

    /// The owners that the records with an effect give (`Db::recomputed_owners`).
    pub fn recomputed_owners(&self) -> Result<Vec<crate::db::owners::Row>> {
        let terms = self.trust_terms(&[])?;
        self.db.recomputed_owners(&self.store.machine.id, &|record| Ok(self.refusal(&terms, record)?.is_none()))
    }

    /// The records that have an effect here (`refusal`), as `export_jsonl`: the source of keys, readers and modes.
    pub fn effective_jsonl(&self) -> Result<String> {
        let terms = self.trust_terms(&[])?;
        let all = self.export_jsonl()?;
        if terms.is_empty() {
            return Ok(all);
        }
        let mut out = String::new();
        for line in all.lines() {
            if self.refusal(&terms, &oplog::parse_line(line)?.record)?.is_none() {
                out.push_str(line);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// The records that have an effect here (`refusal`), in time order: the input of the read model. A record of a
    /// limited machine outside its terms (a reader, an ended trust) stays in the log but changes no view.
    pub fn effective_records(&self) -> Result<Vec<Record>> {
        let terms = self.trust_terms(&[])?;
        let mut out = Vec::new();
        for line in self.db.all_lines()? {
            let record = oplog::parse_line(&line)?.record;
            if terms.is_empty() || self.refusal(&terms, &record)?.is_none() {
                out.push(record);
            }
        }
        Ok(out)
    }

    pub fn authorized_records(&self) -> Result<Vec<layr_model::record::AuthorizedRecord<layr_model::record::ProjectScope>>> {
        let mut keys = std::collections::BTreeMap::new();
        for (machine, key) in self.db.trusted_keys()? {
            let bytes: [u8; 32] = hex::decode(key)?.try_into().map_err(|_| anyhow!("invalid trusted machine key"))?;
            keys.insert(machine, ed25519_dalek::VerifyingKey::from_bytes(&bytes)?);
        }
        let trust_writers = keys.clone();
        // Committed catalog chains were admitted by import_lines (including explicit
        // Trust::All/Server imports). Pin their first announcement, then recheck the
        // signature before reconstructing local authorization after a restart.
        for machine in self.db.record_machines()? {
            if let Some(line) = self.db.record_line(&machine, 1)? {
                let record = oplog::parse_line(&line)?.record;
                if record.op == "machine" {
                    let key = oplog::machine_key(&record)?;
                    if let Some(known) = keys.insert(machine, key) {
                        if known != key {
                            bail!("catalog machine key differs from trusted key");
                        }
                    }
                }
            }
        }
        keys.insert(self.store.machine.id.clone(), self.store.machine.key.verifying_key());
        let terms = self.trust_terms(&[])?;
        layr_model::record::authorize_local_lines(
            &self.id,
            &self.db.all_lines()?,
            &keys,
            layr_model::record::LocalAuthority {
                machine: layr_types::MachineId::parse(&self.store.machine.id)?,
                trust_writers: &trust_writers,
                limits: &terms,
                overlay: self.name.split_once('%').map(|(_, overlay)| overlay),
            },
            ids::now_ms(),
        )
    }

    /// Machines whose records this project has but does not trust yet, with their keys.
    pub fn machine_keys_of(&self, lines: &[String]) -> Vec<(String, String)> {
        let mut v = Vec::new();
        for l in lines {
            if let Ok(s) = oplog::parse_line(l) {
                if s.record.op == "machine" {
                    if let Some(k) = s.record.data["public_key"].as_str() {
                        v.push((s.record.machine.clone(), k.to_string()));
                    }
                }
            }
        }
        v
    }

    /// All records as JSONL (by machine and sequence number): the exchange and remote copy format.
    pub fn export_jsonl(&self) -> Result<String> {
        let mut out = String::new();
        for m in self.db.record_machines()? {
            for l in self.db.lines_after(&m, 0)? {
                out.push_str(&l);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Check every machine's chain of records: key, sequence numbers, signatures.
    pub fn verify_records(&self) -> Result<Vec<(String, u64)>> {
        self.verify_records_with(&mut oplog::Verified::default())
    }

    /// `verify_records` that checks the signatures of each chain only after the run that it verified before
    /// (`oplog::Verified`, beside the catalog, under a MAC with this machine's key).
    pub fn check_records(&self) -> Result<Vec<(String, u64)>> {
        let path = self.meta_dir().join("verified-records");
        let mut verified = oplog::Verified::load(&path, &self.store.machine.key);
        let out = self.verify_records_with(&mut verified)?;
        // A cache: without it (a read-only folder), the next check verifies every signature again.
        let _ = verified.save(&path, &self.store.machine.key);
        Ok(out)
    }

    /// The stored lines of each machine, numbered by their order: each must carry its number.
    fn verify_records_with(&self, verified: &mut oplog::Verified) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for m in self.db.record_machines()? {
            let lines = self.db.lines_after(&m, 0)?;
            verified.check(&m, (1..).zip(lines.iter().map(String::as_str)))?;
            out.push((m, lines.len() as u64));
        }
        Ok(out)
    }

    pub fn config_value(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.db.config(key)
    }

    pub fn config_u64(&self, key: &str, default: u64) -> u64 {
        self.db.config(key).ok().flatten().and_then(|v| v.as_u64()).unwrap_or(default)
    }

    pub fn config_bool(&self, key: &str, default: bool) -> bool {
        self.db.config(key).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(default)
    }

    /// Project config key `gc.evict` (`on` or `off`, default `off`). This is its only owner:
    /// readers call `gc_evicts`, and `cmd::admin::set_config` accepts only the two values.
    pub const GC_EVICT_KEY: &'static str = "gc.evict";

    /// True when `gc.evict` is `on`. Any other value, or a read error, means off.
    pub fn gc_evicts(&self) -> bool {
        self.config_value(Self::GC_EVICT_KEY).ok().flatten().and_then(|v| v.as_str().map(|s| s == "on")).unwrap_or(false)
    }

    pub fn config_strings(&self, key: &str) -> Vec<String> {
        self.db
            .config(key)
            .ok()
            .flatten()
            .and_then(|v| v.as_array().cloned())
            .map(|a| a.into_iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    }

    /// The single admin of stores made before roles (`admin`, earlier `lead`); see `access`.
    pub fn legacy_admin(&self) -> u32 {
        self.config_u64("admin", self.config_u64("lead", 0)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_overlay_maintenance_yields_before_waiting_for_its_stale_parent() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-stale-overlay-{}", ids::new_id()));
        let root = base.join("store");
        let store = Store::open(&root)?;
        let actor = crate::model::Actor { uid: 1000, name: "test".into(), session: None };
        for name in ["p", "p%layer"] {
            let dir = store.project_dir(name)?;
            fs::create_dir_all(dir.join("meta/db"))?;
            let p = Project {
                lease: Default::default(),
                store: &store,
                name: name.into(),
                dir: dir.clone(),
                id: ids::new_id(),
                db: Db::open(&db_file(&dir))?,
            };
            let mut record = crate::model::DraftRecord::new("project");
            record.data = serde_json::json!({"id": p.id, "name": name, "config": {}});
            p.lock_maintenance()?.append(record, &actor)?;
        }
        // Overlay publication reads the parent's object setting, so downgrade both catalogs
        // only after all setup publications have finished.
        for name in ["p", "p%layer"] {
            rusqlite::Connection::open(db_file(&store.project_dir(name)?))?.pragma_update(None, "user_version", 9)?;
        }
        let parent_dir = store.project_dir("p")?;
        let overlay_dir = store.project_dir("p%layer")?;
        let held = fs::OpenOptions::new().write(true).open(parent_dir.join("meta/lock"))?;
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive)?;
        let (send, receive) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker_overlay = overlay_dir.clone();
        let worker = std::thread::spawn(move || -> Result<()> {
            let store = Store::open(&worker_root)?;
            let result = Project::open_catalog(&store, "p%layer", worker_overlay, Wait::Try).map(|p| p.is_none());
            send.send(result)?;
            Ok(())
        });
        // This bounds a deadlock regression, not filesystem performance. Always release the
        // parent and join the worker before asserting, so a failure leaves no blocked thread.
        let outcome = receive.recv_timeout(std::time::Duration::from_secs(5));
        drop(held);
        worker.join().unwrap()?;
        assert!(outcome.context("maintenance waited for the parent's migration lease")??);
        assert!(matches!(Db::inspect(&db_file(&parent_dir))?, OpenedDb::Stale(_)));
        assert!(matches!(Db::inspect(&db_file(&overlay_dir))?, OpenedDb::Stale(_)));
        assert!(!overlay_dir.join("meta").join(LOCK_HOLDER).exists());
        let overlay = Project::open_partial(&store, "p%layer")?;
        assert!(!overlay.id.is_empty());
        assert!(Db::open(&db_file(&parent_dir)).is_ok(), "the normal open upgrades the parent before the overlay");
        drop(overlay);
        drop(store);
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn stale_catalog_migration_respects_leases_and_rechecks_the_schema() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-stale-catalog-{}", ids::new_id()));
        let store = Store::open(&base.join("store"))?;
        let dir = store.project_dir("p")?;
        fs::create_dir_all(dir.join("meta/db"))?;
        let p = Project {
            lease: Default::default(),
            store: &store,
            name: "p".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: Db::open(&db_file(&dir))?,
        };
        let actor = crate::model::Actor { uid: 1000, name: "test".into(), session: None };
        let mut record = crate::model::DraftRecord::new("project");
        record.data = serde_json::json!({"id": p.id, "name": "p", "config": {"description": "replayed"}});
        p.lock_maintenance()?.append(record, &actor)?;
        p.db.test_connection().pragma_update(None, "user_version", 9)?;
        assert!(Db::open(&db_file(&dir)).is_err(), "normal opens cannot return a stale query handle");
        let OpenedDb::Stale(stale) = Db::inspect(&db_file(&dir))? else { anyhow::bail!("expected a stale catalog") };
        let held = fs::OpenOptions::new().write(true).open(p.meta_dir().join("lock"))?;
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive)?;
        assert!(
            Project::open_catalog(&store, "p", dir.clone(), Wait::Try)?.is_none(),
            "maintenance yields to the existing writer"
        );
        drop(held);
        let upgraded = Project::open_partial(&store, "p")?;
        assert_eq!(upgraded.id, p.id);
        assert_eq!(upgraded.config_value("description")?, Some(serde_json::json!("replayed")));
        assert!(!upgraded.holds_lock(), "opening releases the migration flock");
        let lease = lock_catalog(&store, "p", &p.meta_dir(), Wait::Block, Vec::new())?.unwrap();
        let (db, committed) = stale.rebuild(&lease, |db, _, action| {
            assert!(action == RebuildAction::AlreadyCurrent, "the lock recheck observes the other opener's rebuild");
            Ok(db)
        })?;
        assert!(committed.reported_error.is_none());
        assert_eq!(db.project_id()?, Some(p.id.clone()));
        drop(lease);
        p.db.test_connection().pragma_update(None, "user_version", 9)?;
        let OpenedDb::Stale(stale) = Db::inspect(&db_file(&dir))? else { anyhow::bail!("expected a stale catalog") };
        let other_meta = store.project_dir("other")?.join("meta");
        fs::create_dir_all(&other_meta)?;
        let other_lease = lock_catalog(&store, "other", &other_meta, Wait::Block, Vec::new())?.unwrap();
        let error = stale.rebuild::<()>(&other_lease, |_, _, _| anyhow::bail!("an unrelated lease must never reach replay"));
        assert!(matches!(error, Err(CommitResolution::NotCommitted(_))));
        assert!(matches!(Db::inspect(&db_file(&dir))?, OpenedDb::Stale(_)));
        drop(other_lease);
        drop(upgraded);
        drop(p);
        drop(db);
        drop(store);
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn try_locks_keep_bookkeeping_and_line_order() -> Result<()> {
        // Concurrent tests fork and can retain a copy of a lock descriptor after its guard
        // drops. Check immediate release in isolation, as with the preparation-lease test.
        if std::env::var_os("LAYR_TEST_TRY_LOCKS_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "store::tests::try_locks_keep_bookkeeping_and_line_order", "--nocapture"])
                .env("LAYR_TEST_TRY_LOCKS_CHILD", "1")
                .status()?;
            anyhow::ensure!(status.success(), "isolated try-lock test failed");
            return Ok(());
        }
        let base = std::env::temp_dir().join(format!("layr-try-locks-{}", ids::new_id()));
        let store = Store::open(&base.join("store"))?;
        let dir = store.project_dir("p")?;
        fs::create_dir_all(dir.join("meta/db"))?;
        let p = Project {
            lease: Default::default(),
            store: &store,
            name: "p".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: Db::open(&db_file(&dir))?,
        };
        let LockAttempt::Acquired(line) = p.try_line_lock("line", "main")? else { anyhow::bail!("line lock unexpectedly busy") };
        assert!(line._locks[0].f.is_some());
        let LockAttempt::Acquired(nested) = p.try_line_lock("line", "main")? else {
            anyhow::bail!("nested line lock unexpectedly busy")
        };
        assert!(std::rc::Rc::ptr_eq(&line._locks[0], &nested._locks[0]));
        drop(nested);
        let LockAttempt::Acquired(lock) = line.try_lock(&p)? else { anyhow::bail!("project lock unexpectedly busy") };
        assert!(p.holds_lock());
        assert!(p.meta_dir().join(LOCK_HOLDER).is_file());
        assert!(matches!(p.try_lock()?, LockAttempt::Busy), "a second file description cannot lock the project");
        assert!(p.try_line_lock("other", "other").is_err(), "line locks come first");
        drop(lock);
        assert!(!p.holds_lock());
        assert!(!p.meta_dir().join(LOCK_HOLDER).exists());
        let raw = p.try_reserve()?.expect("free publication reservation");
        let reservation = p.locked_token(raw);
        assert!(reservation.check(&p).is_ok());
        assert!(p.holds_lock());
        drop(reservation);
        assert!(!p.holds_lock());
        drop(line);
        let held = fs::OpenOptions::new().write(true).open(p.meta_dir().join("line-locks/line"))?;
        rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive)?;
        assert!(matches!(p.try_line_lock("line", "main")?, LockAttempt::Busy));
        drop(held);
        assert!(matches!(p.try_line_lock("line", "main")?, LockAttempt::Acquired(_)));
        assert!(p.try_line_lock("../bad", "bad").is_err());
        drop(p);
        drop(store);
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn leases_are_sorted_and_nested_tokens_retain_the_actual_flocks() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-typed-leases-{}", ids::new_id()));
        let store = Store::open(&base.join("store"))?;
        let dir = store.project_dir("p")?;
        fs::create_dir_all(dir.join("meta/db"))?;
        let p = Project {
            lease: Default::default(),
            store: &store,
            name: "p".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: Db::open(&db_file(&dir))?,
        };
        let lines = p.line_leases([("z".into(), "z".into()), ("a".into(), "a".into()), ("z".into(), "z".into())])?;
        let ids: Vec<_> = lines._locks.iter().map(|l| l.key.1.as_str()).collect();
        assert_eq!(ids, ["a", "z"]);
        assert!(p.line_leases([("a".into(), "a".into()), ("z".into(), "z".into())]).is_ok());
        assert!(p.line_leases([("0".into(), "lower".into()), ("z".into(), "z".into())]).is_err());
        assert!(!p.meta_dir().join("line-locks/0").exists(), "an out-of-order request acquires nothing");
        let locked = lines.lock(&p)?;
        let nested = p.lock_or_borrow()?;
        assert!(std::rc::Rc::ptr_eq(&locked._lock, &nested._lock));
        let other = Project {
            lease: Default::default(),
            store: &store,
            name: "p".into(),
            dir: dir.clone(),
            id: p.id.clone(),
            db: Db::open(&db_file(&dir))?,
        };
        assert!(nested.check(&other).is_err());
        drop(locked);
        drop(lines);
        let line = fs::OpenOptions::new().write(true).open(p.meta_dir().join("line-locks/a"))?;
        assert!(rustix::fs::flock(&line, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err());
        assert!(matches!(p.try_lock()?, LockAttempt::Busy));
        drop(nested);
        // A test that forks (cache_hook's hard kill) can hold an inherited copy of a lock
        // descriptor for a moment: wait for it rather than fail.
        let lock_when_free = |file: &fs::File| -> Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while let Err(error) = rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                anyhow::ensure!(error == rustix::io::Errno::WOULDBLOCK && std::time::Instant::now() < deadline, error);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Ok(())
        };
        lock_when_free(&line)?;
        drop(line);
        let upper = p.line_leases([("z".into(), "z".into())])?;
        assert!(p.line_leases([("a".into(), "lower".into()), ("z".into(), "z".into())]).is_err());
        let lower = fs::OpenOptions::new().write(true).open(p.meta_dir().join("line-locks/a"))?;
        lock_when_free(&lower)?;
        drop(lower);
        drop(upper);
        let phases =
            p.line_leases([("q".into(), "q".into()), ("q-cache".into(), "cache".into()), ("view-q".into(), "view".into())])?;
        assert_eq!(phases._locks.iter().map(|l| l.key.1.as_str()).collect::<Vec<_>>(), ["view-q", "q-cache", "q"]);
        let owned = phases.into_locked(&p)?;
        assert_eq!(owned._lock._lines.len(), 3);
        drop(owned);
        drop(other);
        drop(p);
        drop(store);
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn a_store_root_below_a_link_is_resolved_once() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-root-{}", ids::new_id()));
        let real = base.join("real");
        fs::create_dir_all(&real)?;
        std::os::unix::fs::symlink(&real, base.join("link"))?;
        let opened = Store::open(&base.join("link/store"));
        let expected = real.canonicalize()?.join("store");
        let _ = fs::remove_dir_all(&base);
        assert_eq!(opened?.root, expected);
        Ok(())
    }

    #[test]
    fn an_overlay_lives_in_the_private_folder_of_its_project() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let store = Store {
            root: PathBuf::from("/s"),
            machine: Machine { id: String::new(), name: String::new(), key },
            _version_guard: None,
        };
        assert_eq!(store.project_dir("sap").unwrap(), PathBuf::from("/s/projects/sap"));
        assert_eq!(store.project_dir("sap%keys").unwrap(), PathBuf::from("/s/projects/sap/meta/overlays/keys"));
        for bad in ["sap%", "%keys", "sap%keys%x", "sap%../x", "../sap%keys", "sap/x"] {
            assert!(store.project_dir(bad).is_err(), "{bad}");
        }
    }

    /// A cache writer's record has no effect beyond its role (docs/ci-trust.md): a `config` record that also makes a
    /// source state or moves a line is stored, and changes nothing.
    #[test]
    fn a_writer_config_record_makes_no_source_state_and_moves_no_line() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-writer-{}", ids::new_id()));
        let result = (|| -> Result<()> {
            let store = Store::open(&base.join("store"))?;
            let actor = Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None };
            // The project folder by hand: no subvolume, so the test runs on any file system.
            let dir = store.project_dir("p")?;
            fs::create_dir_all(dir.join("meta/db"))?;
            fs::create_dir(dir.join("states"))?;
            let id = ids::new_id();
            let p = Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: dir.clone(),
                id: id.clone(),
                db: Db::open(&db_file(&dir))?,
            };
            let mut project = crate::model::DraftRecord::new("project");
            project.data = serde_json::json!({"id": id, "name": "p", "config": {}});
            p.lock_maintenance().unwrap().append(project, &actor)?;
            let job_key = SigningKey::from_bytes(&[9u8; 32]);
            let (job, public_key) = (ids::new_id(), hex::encode(job_key.verifying_key().to_bytes()));
            let mut trust = crate::model::DraftRecord::new("machine.trust");
            trust.data = serde_json::json!({"machine": job, "public_key": public_key, "expires_at": ids::now_ms() + 3_600_000,
                "role": "writer"});
            p.lock_maintenance().unwrap().append(trust, &actor)?;
            let signed = |seq: u64, op: &str, extra: serde_json::Value| -> Result<String> {
                let mut rec = Record::untrusted_preview(op);
                (rec.id, rec.machine, rec.seq, rec.time) = (ids::new_id(), job.clone(), seq, ids::now_ms());
                (rec.project, rec.actor, rec.data) = (p.id.clone(), actor.clone(), extra["data"].clone());
                if let Some(states) = extra.get("states") {
                    rec.states = serde_json::from_value(states.clone())?;
                }
                if let Some(lines) = extra.get("lines") {
                    rec.lines = serde_json::from_value(lines.clone())?;
                }
                oplog::sign_line(&rec, &job_key)
            };
            let source = ids::new_id();
            let state = serde_json::json!({"id": source, "kind": "commit", "subvol": "", "time": 1,
                "author": crate::model::Author::default()});
            let made = serde_json::json!({"before": null, "after": {"name": "main", "head": source, "owner": 0, "machine": job}});
            let forged =
                serde_json::json!({"states": [state], "lines": {ids::new_id(): made}, "data": {"config": {"core.x": "1"}}});
            let lines = [
                signed(1, "machine", serde_json::json!({"data": {"name": "job", "public_key": public_key}}))?,
                signed(2, "config", forged)?,
                signed(3, "config", serde_json::json!({"data": {"config": {"core.y": "2"}}}))?,
            ];
            p.lock_maintenance().unwrap().import_lines(&lines, Trust::All)?;
            for when in ["on import", "after a rebuild"] {
                assert!(p.db.state(&source)?.is_none(), "{when}: the writer's source state is not taken");
                assert!(p.db.line_by_name("main")?.is_none(), "{when}: the writer's line is not taken");
                assert!(p.db.config(&format!("core.x@{job}"))?.is_none(), "{when}: nor its settings");
                assert!(p.db.config(&format!("core.y@{job}"))?.is_some(), "{when}: a plain config record counts");
                p.lock_maintenance()?.rebuild_index()?;
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&base);
        result
    }

    /// A merge machine can hold the change numbering role (docs/review.md, "How numbers work"): after the creator's
    /// handoff, its signed numbers count for readers. Its own handoff does not count; the creator takes the role
    /// back. A cache writer that gets the role gives no number: its records stay outside its role.
    #[test]
    fn a_merge_machine_with_the_numbering_role_gives_numbers() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-numbering-{}", ids::new_id()));
        let result = (|| -> Result<()> {
            let store = Store::open(&base.join("store"))?;
            let actor = Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None };
            let dir = store.project_dir("p")?;
            fs::create_dir_all(dir.join("meta/db"))?;
            fs::create_dir(dir.join("states"))?;
            let id = ids::new_id();
            let p = Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: dir.clone(),
                id: id.clone(),
                db: Db::open(&db_file(&dir))?,
            };
            let mut project = crate::model::DraftRecord::new("project");
            project.data = serde_json::json!({"id": id, "name": "p", "config": {}});
            p.lock_maintenance().unwrap().append(project, &actor)?;
            let machine = |seed: u8, role: &str| -> Result<(SigningKey, String)> {
                let key = SigningKey::from_bytes(&[seed; 32]);
                let machine = ids::new_id();
                let mut trust = crate::model::DraftRecord::new("machine.trust");
                trust.data = serde_json::json!({"machine": machine, "public_key": hex::encode(key.verifying_key().to_bytes()),
                    "role": role});
                p.lock_maintenance().unwrap().append(trust, &actor)?;
                Ok((key, machine))
            };
            let (merge_key, merge) = machine(11, "merge")?;
            let (writer_key, writer) = machine(12, "writer")?;
            let signed = |key: &SigningKey, machine: &str, seq: u64, op: &str, data: serde_json::Value| -> Result<String> {
                let mut rec = Record::untrusted_preview(op);
                (rec.id, rec.machine, rec.seq, rec.time) = (ids::new_id(), machine.to_string(), seq, ids::now_ms());
                (rec.project, rec.actor, rec.data) = (p.id.clone(), actor.clone(), data);
                if op == "machine" {
                    rec.data = serde_json::json!({"name": machine, "public_key": hex::encode(key.verifying_key().to_bytes())});
                }
                oplog::sign_line(&rec, key)
            };
            let numbering = || -> Result<layr_model::changes::Numbering> {
                Ok(layr_model::Project::preview_records(&p.id, p.effective_records()?, &p.store.machine.id)?.numbering().clone())
            };
            let mut handoff = crate::model::DraftRecord::new(layr_model::changes::HANDOFF_OP);
            handoff.data = serde_json::json!({"machine": merge, "next": 1});
            p.lock_maintenance().unwrap().append(handoff, &actor)?;
            p.lock_maintenance().unwrap().import_lines(
                &[
                    signed(&merge_key, &merge, 1, "machine", serde_json::json!({}))?,
                    signed(&merge_key, &merge, 2, layr_model::changes::NUMBER_OP, serde_json::json!({"numbers": {"c1": 1}}))?,
                    // The merge machine hands the role on, to the cache writer.
                    signed(
                        &merge_key,
                        &merge,
                        3,
                        layr_model::changes::HANDOFF_OP,
                        serde_json::json!({"machine": writer, "next": 2}),
                    )?,
                    signed(&writer_key, &writer, 1, "machine", serde_json::json!({}))?,
                    signed(&writer_key, &writer, 2, layr_model::changes::NUMBER_OP, serde_json::json!({"numbers": {"c2": 2}}))?,
                ],
                Trust::All,
            )?;
            let n = numbering()?;
            assert_eq!(n.number("c1"), Some(1), "the merge machine's number counts");
            assert_eq!(n.authority.as_deref(), Some(merge.as_str()), "its handoff does not count");
            // The creator takes the role back from the merge machine, then gives it to the cache writer.
            for to in [&p.store.machine.id, &writer] {
                let mut handoff = crate::model::DraftRecord::new(layr_model::changes::HANDOFF_OP);
                handoff.data = serde_json::json!({"machine": to, "next": 2});
                p.lock_maintenance().unwrap().append(handoff, &actor)?;
                assert_eq!(numbering()?.authority.as_deref(), Some(to.as_str()), "a handoff of the creator counts");
            }
            assert_eq!(numbering()?.number("c2"), None, "a cache writer's number does not count");
            Ok(())
        })();
        let _ = fs::remove_dir_all(&base);
        result
    }

    /// A state id names one state: an import refuses a record whose state an earlier record of the same batch (of
    /// any machine) or a stored record already names, and reports the later record.
    #[test]
    fn an_import_refuses_a_reused_state_id() -> Result<()> {
        let base = std::env::temp_dir().join(format!("layr-reused-state-{}", ids::new_id()));
        let result = (|| -> Result<()> {
            let store = Store::open(&base.join("store"))?;
            let actor = Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None };
            let dir = store.project_dir("p")?;
            fs::create_dir_all(dir.join("meta/db"))?;
            fs::create_dir(dir.join("states"))?;
            let id = ids::new_id();
            let p = Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: dir.clone(),
                id: id.clone(),
                db: Db::open(&db_file(&dir))?,
            };
            let mut project = crate::model::DraftRecord::new("project");
            project.data = serde_json::json!({"id": id, "name": "p", "config": {}});
            p.lock_maintenance()?.append(project, &actor)?;
            let machines =
                [(SigningKey::from_bytes(&[21u8; 32]), ids::new_id()), (SigningKey::from_bytes(&[22u8; 32]), ids::new_id())];
            let signed = |m: usize, seq: u64, states: &[&str]| -> Result<String> {
                let (key, machine) = &machines[m];
                let mut rec = Record::untrusted_preview(if seq == 1 { "machine" } else { "config" });
                (rec.id, rec.machine, rec.seq, rec.time) = (ids::new_id(), machine.clone(), seq, ids::now_ms());
                (rec.project, rec.actor) = (p.id.clone(), actor.clone());
                rec.data = if seq == 1 {
                    serde_json::json!({"name": "m", "public_key": hex::encode(key.verifying_key().to_bytes())})
                } else {
                    serde_json::json!({"config": {}})
                };
                let states: Vec<_> = states
                    .iter()
                    .map(|id| {
                        serde_json::json!({"id": id, "kind": "commit", "subvol": "", "time": 1,
                        "author": crate::model::Author::default()})
                    })
                    .collect();
                rec.states = serde_json::from_value(serde_json::Value::Array(states))?;
                oplog::sign_line(&rec, key)
            };
            let (s, t) = (ids::new_id(), ids::new_id());
            let refused = |lines: &[String], seq: u64| -> Result<()> {
                let Err(error) = p.lock_maintenance()?.import_lines(lines, Trust::All) else {
                    anyhow::bail!("a reused state id is refused");
                };
                let expected = format!("record {seq} of machine ");
                let message = format!("{error:#}");
                anyhow::ensure!(
                    message.starts_with(&expected) && message.ends_with(&format!("reuses state id {s}")),
                    "{message}"
                );
                Ok(())
            };
            // The same machine, a later record.
            refused(&[signed(0, 1, &[])?, signed(0, 2, &[&s])?, signed(0, 3, &[&t, &s])?], 3)?;
            // Another machine of the same batch.
            refused(&[signed(0, 1, &[])?, signed(0, 2, &[&s])?, signed(1, 1, &[])?, signed(1, 2, &[&s])?], 2)?;
            assert!(p.db.state(&s)?.is_none(), "a refused batch stores nothing");
            assert_eq!(
                p.lock_maintenance()?
                    .import_lines(&[signed(0, 1, &[])?, signed(0, 2, &[&s])?, signed(0, 3, &[&t])?], Trust::All)?,
                3
            );
            // A stored record.
            refused(&[signed(1, 1, &[])?, signed(1, 2, &[&s])?], 2)?;
            Ok(())
        })();
        let _ = fs::remove_dir_all(&base);
        result
    }
}

#[cfg(test)]
mod temp_name_tests {
    use super::{creator_gone, temp_name};

    #[test]
    fn a_temporary_name_tells_its_creator() {
        assert_eq!(creator_gone(&temp_name()), Some(false), "this process is alive");
        assert_eq!(creator_gone("11111111-2222-3333-4444-555555555555.2000000000"), Some(true));
        assert_eq!(creator_gone("keep"), None, "a name without a pid has no known creator");
        assert_eq!(creator_gone("prepared-11111111-2222-3333-4444-555555555555"), None);
    }
}

/// Test only (tests/t125-save-line-lock.sh): `LAYR_TEST_SAVE_PAUSE_MS` keeps a named save this long
/// after its clone, before the project lock, so a test can run other commands meanwhile.
pub fn test_pause() {
    if let Some(ms) = std::env::var("LAYR_TEST_SAVE_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Test only (tests/t136-service-stop.sh): `LAYR_TEST_PANIC=<command>` panics at the start of that command.
pub fn panic_point(command: &str) {
    if std::env::var("LAYR_TEST_PANIC").ok().as_deref() == Some(command) {
        panic!("test panic in {command}");
    }
}

/// Test only (tests/t124-save-crash.sh): `LAYR_TEST_SAVE_KILL=<point>` kills this process with
/// SIGKILL at that point of a named save, as a power loss or a crash would. Other runs pass.
pub fn failpoint(point: &str) {
    if std::env::var("LAYR_TEST_SAVE_KILL").ok().as_deref() == Some(point) {
        let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    }
}

/// The parallel empty is APFS only; other file systems delete a tree in place (OverlayFS through its registry).
#[cfg(all(test, target_os = "macos"))]
mod trash_tests {
    use super::empty_trash;
    use std::fs;

    #[test]
    fn two_emptiers_of_one_trash_report_no_failure() {
        for round in 0..5 {
            let dir = std::env::temp_dir().join(format!("layr-trash-race-{}", crate::ids::new_id()));
            for t in 0..4 {
                for i in 0..400 {
                    let sub = dir.join(format!("t{t}")).join(format!("d{}", i % 20));
                    fs::create_dir_all(&sub).unwrap();
                    fs::write(sub.join(format!("f{i}")), b"x").unwrap();
                }
            }
            let failed: Vec<String> = std::thread::scope(|scope| {
                let a = scope.spawn(|| empty_trash(&dir, None).unwrap().1);
                let b = scope.spawn(|| empty_trash(&dir, None).unwrap().1);
                let mut f = a.join().unwrap();
                f.extend(b.join().unwrap());
                f
            });
            // A pass may leave a path that the other emptier was deleting; a second pass ends it.
            let (_, again) = empty_trash(&dir, None).unwrap();
            assert!(failed.is_empty() && again.is_empty(), "round {round}: {failed:?} {again:?}");
            assert!(fs::read_dir(&dir).unwrap().next().is_none(), "round {round}: the trash is empty");
            fs::remove_dir(&dir).unwrap();
        }
    }

    #[test]
    fn a_folder_of_many_entries_goes_whole_and_a_spent_budget_deletes_nothing() {
        let dir = std::env::temp_dir().join(format!("layr-trash-{}", crate::ids::new_id()));
        let tree = dir.join("tree");
        for i in 0..300 {
            let sub = tree.join(format!("d{}", i % 3));
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join(format!("f{i}")), b"x").unwrap();
        }
        fs::create_dir_all(tree.join("many")).unwrap();
        for i in 0..200 {
            fs::write(tree.join("many").join(format!("g{i}")), b"y").unwrap();
        }
        let (deleted, failed) = empty_trash(&dir, Some(std::time::Duration::ZERO)).unwrap();
        assert_eq!((deleted, failed.len()), (0, 0));
        assert!(tree.join("many/g0").exists(), "a spent budget deletes nothing");
        let (deleted, failed) = empty_trash(&dir, None).unwrap();
        assert!(failed.is_empty(), "{failed:?}");
        assert!(deleted > 0);
        assert!(!tree.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// The derived tables of a project as the reader of a merge machine's record (`layr_store::trust::Current`). A
/// database error reads as absent, so the record is refused rather than accepted.
struct DbCurrent<'a> {
    project: &'a Project<'a>,
    terms: &'a layr_store::trust::Rules,
}

impl layr_store::trust::Current for DbCurrent<'_> {
    fn line(&self, id: &str) -> Option<(String, u32, String)> {
        if self.project.db.line_deleted(id).unwrap_or(true) {
            return None;
        }
        self.project.db.line_by_id(id).ok().flatten().map(|line| (line.name, line.owner, line.machine))
    }

    fn queue_entry(&self, id: &str) -> Option<serde_json::Value> {
        self.project.db.queue_entry(id).ok().flatten().and_then(|entry| serde_json::to_value(entry).ok())
    }

    fn ref_target(&self, name: &str) -> Option<String> {
        self.project.db.ref_value(name, crate::ids::now_ms()).ok().flatten().map(|r| r.target)
    }

    /// The machine of the records with an effect that set the ref to its current target, in the newest run of
    /// such records; none when more than one machine is in that run. A database error reads as none, so a merge machine's
    /// list is refused.
    fn ref_setter(&self, name: &str) -> Option<String> {
        let target = self.ref_target(name)?;
        let needle = serde_json::to_string(name).ok()?;
        // Newest first.
        let records = self.project.db.records_naming(&needle, usize::MAX >> 1).ok()?;
        // Every record of that run must be of one machine: a merge record signed with an old time can sort before
        // the creator's set of the same target, and must not become the setter of the creator's release.
        let mut setter: Option<String> = None;
        for record in records {
            let change = &record.data["ref_change"];
            if change["name"] != name || self.project.refusal(self.terms, &record).ok()?.is_some() {
                continue;
            }
            if change["after"]["target"] != target.as_str() {
                break;
            }
            match &setter {
                Some(machine) if machine != &record.machine => return None,
                _ => setter = Some(record.machine),
            }
        }
        setter
    }

    fn now(&self) -> i64 {
        crate::ids::now_ms()
    }
}

impl<'p> std::ops::Deref for LockedProject<'p> {
    type Target = Project<'p>;
    fn deref(&self) -> &Self::Target {
        self.project
    }
}

fn lock_catalog(store: &Store, name: &str, meta: &Path, wait: Wait, lines: Vec<std::rc::Rc<LineLock>>) -> Result<Option<Lock>> {
    if matches!(wait, Wait::Block) && HELD_PROJECTS.lock().map(|held| held.iter().any(|path| path == meta)).unwrap_or(false) {
        bail!("project '{name}' is already locked in this process; borrow the held token");
    }
    // Resolved before the lock is taken: a failure here must not leave a count behind.
    let layout = store.layout()?;
    let trash = layout.trash_of(&layout.volume_of(name))?;
    let f = fs::OpenOptions::new().create(true).write(true).truncate(false).open(meta.join("lock"))?;
    let asked = std::time::Instant::now();
    if !wait.acquire(&f, Some(meta))? {
        return Ok(None);
    }
    write_lock_holder(meta);
    // Test only (tests/t121-lock-holder.sh): `LAYR_TEST_LOCK_HOLD_MS` keeps the lock this long.
    if let Some(ms) = std::env::var("LAYR_TEST_LOCK_HOLD_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    // `project move` may have taken the folder away while this call waited for the lock. The
    // lock then belongs to the old folder: refuse, the caller runs the command again.
    {
        use std::os::unix::fs::MetadataExt;
        let here = fs::symlink_metadata(meta.join("lock")).ok();
        let locked = f.metadata().ok();
        let same = here.zip(locked).is_some_and(|(a, b)| a.ino() == b.ino() && a.dev() == b.dev());
        if !same {
            bail!("project '{}' moved or was deleted while this command waited; run the command again", name);
        }
    }
    LOCAL_LOCKS.set(LOCAL_LOCKS.get() + 1);
    HELD_LOCKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut held) = HELD_PROJECTS.lock() {
        held.push(meta.to_path_buf());
    }
    if std::env::var_os("LAYR_TRACE").is_some() {
        eprintln!("trace {:>8.1} ms  store lock taken: {}", asked.elapsed().as_secs_f64() * 1000.0, name);
    }
    if let Ok(mut dirs) = LOCK_TRASH.lock() {
        if !dirs.contains(&trash) {
            dirs.push(trash);
        }
    }
    let lock = Lock {
        f: Some(f),
        since: {
            crate::trace::reset();
            std::time::Instant::now()
        },
        waited: asked.elapsed(),
        log: meta.join("lock-slow.log"),
        project: name.to_string(),
        meta: meta.to_path_buf(),
        _lines: lines,
    };
    Ok(Some(lock))
}

impl Lock {
    pub(crate) fn check_catalog(&self, catalog: Option<&str>) -> Result<()> {
        let catalog = catalog.ok_or_else(|| anyhow!("a migration requires a persistent catalog"))?;
        let path = Path::new(catalog);
        anyhow::ensure!(
            path == self.meta.join("layr.sqlite") || path == self.meta.join("db/layr.sqlite"),
            "catalog lease belongs to another project"
        );
        Ok(())
    }
}

impl LockedProject<'_> {
    pub fn write<T>(&self, change: impl FnOnce(&crate::db::WriteTxn<'_>) -> Result<T>) -> PublishResult<T> {
        let tx = self.begin_write()?;
        let value = change(&tx)?;
        Self::finish_write(tx)?;
        Ok(value)
    }

    fn finish_write(tx: crate::db::WriteTxn<'_>) -> PublishResult<()> {
        let committed = tx.finish().into_result()?;
        crate::free::release_effects(committed.effects);
        match committed.reported_error {
            Some(error) => Err(CommitResolution::Committed(error)),
            None => Ok(()),
        }
    }

    /// Rebuild the derived tables from the records.
    pub fn rebuild_index(&self) -> Result<()> {
        let terms = self.trust_terms(&[])?;
        let tx = self.begin_rebuild()?;
        tx.rebuild(|tx| self.replay_derived(tx, &terms))?;
        let committed = tx.finish().into_result()?;
        crate::free::release_effects(committed.effects);
        if let Some(error) = committed.reported_error {
            return Err(error);
        }
        self.db.remove_upgrade_copies();
        Ok(())
    }

    fn replay_derived(&self, tx: &RebuildTxn<'_>, terms: &layr_store::trust::Rules) -> Result<()> {
        let present = self.state_present();
        let bases = self.layer_bases_on_disk()?;
        for raw in tx.all_lines()? {
            let signed = oplog::parse_line(&raw)?;
            if self.refusal_in_order(terms, &signed.record)?.is_none() {
                tx.derive(&signed.record, &present, &self.store.machine.id)?;
            }
        }
        for (id, deleted) in tx.present_states()? {
            if deleted || !self.has_state(&id) {
                tx.set_present(&id, false)?;
            }
        }
        tx.fill_bases(&bases)
    }

    /// Write a record: sign it and store it with its derived rows in one transaction.
    /// The caller holds the project lock and has created all snapshots the record names.
    pub fn append(&self, mut rec: crate::model::DraftRecord, actor: &Actor) -> PublishResult<Record> {
        let mut records = self.append_batch(std::slice::from_mut(&mut rec), actor)?;
        records.pop().ok_or_else(|| anyhow!("record batch returned no record").into())
    }

    /// Sign and derive every record in one FULL transaction, with contiguous sequence numbers.
    /// Snapshots must already exist and the caller must hold the project lock.
    pub(crate) fn append_batch(&self, records: &mut [crate::model::DraftRecord], actor: &Actor) -> PublishResult<Vec<Record>> {
        let _t = crate::trace::span("record batch");
        crate::objstore::record_states(self, records)?;
        let Some(first) = records.first() else { return Ok(Vec::new()) };
        let m = &self.store.machine;
        let present = self.state_present();
        let project = if first.op == "project" { first.data["id"].as_str().unwrap_or("").to_string() } else { self.id.clone() };
        if project.is_empty() {
            return Err(anyhow!("project id unknown: the project has no records yet").into());
        }
        let mut published = Vec::new();
        let identity = |seq, time| -> Result<crate::model::RecordIdentity> {
            Ok(crate::model::RecordIdentity {
                id: layr_types::RecordId::parse(&ids::new_id())?,
                machine: layr_types::MachineId::parse(&m.id)?,
                project: layr_types::ProjectId::parse(&project)?,
                seq: std::num::NonZeroU64::new(seq).context("record sequence must be positive")?,
                time,
                actor: actor.clone(),
            })
        };
        let tx = self.begin_write()?;
        {
            let mut seq = self.db.last_seq(&m.id)?;
            if seq == 0 {
                // The first record of this machine publishes its key.
                let mut mr = crate::model::DraftRecord::new("machine");
                mr.data = serde_json::json!({"name": m.name, "public_key": hex::encode(m.key.verifying_key().to_bytes())});
                let signed = mr.sign(identity(1, ids::now_ms())?, &m.key)?;
                let mr: Record = serde_json::from_str(signed.get())?;
                tx.insert_record(&mr, signed.line())?;
                tx.derive(&mr, &present, &m.id)?;
                seq = 1;
            }
            for draft in records {
                let identity = identity(seq.checked_add(1).context("record sequence overflow")?, ids::now_ms())?;
                if let Some(a) = draft.data.get_mut("attestation").and_then(|v| v.as_object_mut()) {
                    a.insert("attestation_id".into(), serde_json::json!(identity.id.to_string()));
                    if a.get("signer").and_then(|v| v.as_str()).is_none_or(|s| !s.starts_with("person:")) {
                        a.insert("signer".into(), serde_json::json!(m.id));
                    }
                    a.insert("time".into(), serde_json::json!(identity.time));
                }
                let signed = draft.clone().sign(identity, &m.key)?;
                let record: Record = serde_json::from_str(signed.get())?;
                let rec = &record;
                tx.insert_record(rec, signed.line())?;
                tx.derive(rec, &present, &m.id)?;
                // A state whose thin layer is on disk: its base row, in this transaction (thin.rs).
                #[cfg(target_os = "macos")]
                for state in &rec.states {
                    if let Some(base) = crate::thin::layer_base(self, &state.id)? {
                        tx.record_base(&state.id, &base)?;
                    }
                }
                seq = rec.seq;
                published.push(record);
            }
        }
        Self::finish_write(tx)?;
        Ok(published)
    }

    /// Records of other machines (replication, restore): check each machine's chain (its key
    /// from its first record, contiguous sequence numbers, signatures) and the project id, then
    /// store them with their derived rows in one transaction. Returns the number of new records.
    ///
    /// Only records of trusted machines are taken (see `Trust`).
    pub fn import_lines(&self, lines: &[String], trust: Trust) -> Result<usize> {
        crate::overlay_managers::validate_import(self, lines, matches!(trust, Trust::Server))?;
        use std::collections::{BTreeMap, BTreeSet};
        let mut parsed: Vec<oplog::Signed> = Vec::new();
        for l in lines {
            parsed.push(oplog::parse_line(l)?);
        }
        let project = if self.id.is_empty() {
            parsed
                .iter()
                .find(|s| s.record.op == "project")
                .and_then(|s| s.record.data["id"].as_str().map(|x| x.to_string()))
                .unwrap_or_default()
        } else {
            self.id.clone()
        };
        for s in &parsed {
            crate::model::validate(&s.record)?;
            if !project.is_empty() && s.record.project != project {
                bail!(
                    "record {} of machine {} belongs to another project ({})",
                    s.record.seq,
                    s.record.machine,
                    s.record.project
                );
            }
        }
        let mut by: BTreeMap<String, Vec<oplog::Signed>> = BTreeMap::new();
        for s in parsed {
            by.entry(s.record.machine.clone()).or_default().push(s);
        }
        for recs in by.values_mut() {
            recs.sort_by_key(|s| s.record.seq);
        }
        let present = self.state_present();
        let mut fresh: Vec<&oplog::Signed> = Vec::new();
        // The state ids of the earlier fresh records: a record may not reuse one (O(1) per id).
        let mut fresh_states: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut keys: BTreeMap<String, String> = BTreeMap::new();
        for (machine, recs) in &by {
            let mut last = self.db.last_seq(machine)?;
            let first_seq = recs.first().ok_or_else(|| anyhow!("no records"))?.record.seq;
            let last_seq = recs.last().ok_or_else(|| anyhow!("no records"))?.record.seq.min(last);
            let stored =
                if first_seq <= last_seq { self.db.record_range(machine, first_seq, last_seq)? } else { BTreeMap::new() };
            let key = if last > 0 {
                let first = self.db.record_line(machine, 1)?.ok_or_else(|| anyhow!("no records"))?;
                oplog::machine_key(&oplog::parse_line(&first)?.record)?
            } else {
                let f = recs.first().ok_or_else(|| anyhow!("no records"))?;
                if f.record.op != "machine" || f.record.seq != 1 {
                    bail!("records of machine {machine} do not start with its key");
                }
                oplog::machine_key(&f.record)?
            };
            // Records that claim this machine must carry this machine's key.
            if machine == &self.store.machine.id && key != self.store.machine.key.verifying_key() {
                bail!("records claim this machine's id with another key");
            }
            keys.insert(machine.clone(), hex::encode(key.to_bytes()));
            for s in recs.iter() {
                if s.record.seq <= last {
                    // A record we have: the copy must be the same bytes (else a changed or a
                    // forked history).
                    if stored.get(&s.record.seq).map(String::as_str) != Some(s.line.as_str()) {
                        bail!(
                            "record {} of machine {machine} differs from the stored record (changed or forked history)",
                            s.record.seq
                        );
                    }
                    continue;
                }
                if s.record.seq != last + 1 {
                    bail!("gap in the records of machine {machine}: have {last}, got {}", s.record.seq);
                }
                oplog::verify_line(&s.line, &key).with_context(|| format!("record {} of machine {machine}", s.record.seq))?;
                for st in &s.record.states {
                    if self.db.state(&st.id)?.is_some() || fresh_states.contains(st.id.as_str()) {
                        bail!("record {} of machine {machine} reuses state id {}", s.record.seq, st.id);
                    }
                }
                fresh_states.extend(s.record.states.iter().map(|st| st.id.as_str()));
                last = s.record.seq;
                fresh.push(s);
            }
        }
        // Limited machines (docs/ci-trust.md): their records outside their terms are stored but have no effect, and
        // they vouch for nobody.
        let terms = self.trust_terms(&fresh.iter().map(|s| &s.record).collect::<Vec<_>>())?;
        // Trust: known machines, then machines vouched for by trusted ones in this batch.
        // Under `Trust::Server`, the machines whose trust records count: trusted here before this batch.
        let mut locally_trusted: BTreeSet<String> = BTreeSet::from([self.store.machine.id.clone()]);
        for m in keys.keys() {
            if self.db.trusted(m)?.is_some_and(|k| Some(&k) == keys.get(m)) {
                locally_trusted.insert(m.clone());
            }
        }
        if !matches!(trust, Trust::All | Trust::Server) {
            let mut trusted = locally_trusted.clone();
            if let Trust::Peer(m, k) = trust {
                if keys.get(m).map(|x| x == k).unwrap_or(false) {
                    trusted.insert(m.to_string());
                }
            }
            loop {
                let before = trusted.len();
                for s in &fresh {
                    if s.record.op == "machine.trust"
                        && trusted.contains(&s.record.machine)
                        && !terms.contains_key(&s.record.machine)
                        && terms.trust_counts(&s.record.machine, s.record.time, &s.record.data)
                    {
                        let (m, k) =
                            (s.record.data["machine"].as_str().unwrap_or(""), s.record.data["public_key"].as_str().unwrap_or(""));
                        if keys.get(m).map(|x| x == k).unwrap_or(false) {
                            trusted.insert(m.to_string());
                        }
                    }
                }
                if trusted.len() == before {
                    break;
                }
            }
            let untrusted: Vec<String> = keys.keys().filter(|m| !trusted.contains(*m)).cloned().collect();
            if !untrusted.is_empty() {
                let list: Vec<String> = untrusted
                    .iter()
                    .map(|m| format!("{} (key {})", &crate::ids::display(m)[..8], keys.get(m).map(|k| k.as_str()).unwrap_or("")))
                    .collect();
                bail!(
                    "records of untrusted machine(s): {}. If you trust them, run: layr remote trust <remote> <id>",
                    list.join(", ")
                );
            }
        }
        fresh.sort_by(|a, b| {
            (a.record.time, &a.record.machine, a.record.seq).cmp(&(b.record.time, &b.record.machine, b.record.seq))
        });
        let n = fresh.len();
        let local = self.store.machine.id.clone();
        let tx = self.begin_write()?;
        {
            for s in &fresh {
                if tx.insert_record(&s.record, &s.line)? {
                    let server_trust = matches!(trust, Trust::Server)
                        && TRUST_OPS.contains(&s.record.op.as_str())
                        && !locally_trusted.contains(&s.record.machine);
                    let refused = if server_trust {
                        Some("a trust change by a machine this project does not trust")
                    } else {
                        self.refusal_in_order(&terms, &s.record)?
                    };
                    if let Some(why) = refused {
                        if std::env::var_os("LAYR_TRACE").is_some() {
                            let (seq, machine, op) = (s.record.seq, crate::ids::short(&s.record.machine), &s.record.op);
                            eprintln!("trace record {seq} of machine {machine} ({op}) has no effect: {why}");
                        }
                        continue;
                    }
                    tx.derive(&s.record, &present, &local)?;
                }
            }
        }
        Self::finish_write(tx)?;
        Ok(n)
    }
}

/// Destroy the subvolume `path` without a search: O(1) when it has no nested subvolume, else its
/// `.git` subvolume goes first. False when the subvolume stays (more nested subvolumes, or not one).
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
fn destroy_known(path: &Path) -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let subvolume = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && m.ino() == 256);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else { return false };
    if !subvolume(path)
        || crate::sys::subvol_flags(
            match fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return false,
            }
            .as_fd(),
        )
        .map_or(true, |flags| flags & 1 != 0)
    {
        return false;
    }
    let Ok(dir) = fs::File::open(parent) else { return false };
    match crate::sys::subvol_destroy(dir.as_fd(), name) {
        Ok(()) => return true,
        Err(e) if crate::fs::is_not_empty(&e) => {}
        Err(_) => return false,
    }
    let _t = crate::trace::span("delete: known nested subvolumes");
    let git = path.join(".git");
    if subvolume(&git)
        && fs::symlink_metadata(&git).is_ok_and(|m| m.dev() != fs::symlink_metadata(path).map(|p| p.dev()).unwrap_or(0))
    {
        let Ok(inner) = fs::File::open(path) else { return false };
        if let Ok(g) = fs::File::open(&git) {
            if crate::sys::subvol_flags(g.as_fd()).is_ok_and(|flags| flags & 1 != 0) {
                return false;
            }
        }
        if crate::sys::subvol_destroy(inner.as_fd(), ".git").is_err() {
            return false;
        }
    }
    crate::sys::subvol_destroy(dir.as_fd(), name).is_ok()
}
