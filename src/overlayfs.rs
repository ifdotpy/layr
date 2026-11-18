//! OverlayFS trees for a Linux VM. The private registry restores mounts after reboot.
//! Writable branches share an immutable lower tree. A snapshot of a writable tree
//! copies its changed upper layer. Saved layers are immutable and shared by branches.
use crate::sys;
use layr_fs_core::CaptureConsistency;
#[path = "overlayfs/driver.rs"]
pub(crate) mod driver;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolInfo {
    pub uuid: String,
    pub received_uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub readonly: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    info: SubvolInfo,
    // Older binaries require `lower`; omit it so they reject this registry
    // instead of ignoring delta layers or remounting an ordinary cache.
    #[serde(rename = "base", alias = "lower")]
    lower: String,
    #[serde(default)]
    hard_links: bool,
    #[serde(default)]
    bases: Vec<String>,
    #[serde(default)]
    plain: bool,
    #[serde(default)]
    restore_cache: bool,
    /// A layer over a saved state, taken by a new line (`snapshot`). It is a mount in its
    /// writable parent, so `recover` must not migrate it like an older nested cache mount.
    #[serde(default)]
    taken: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct Registry {
    trees: BTreeMap<PathBuf, Entry>,
    #[serde(default)]
    pending_move: Option<(PathBuf, PathBuf)>,
}
// The registry lock is held only to read or publish registry changes, never during
// a scan, copy or flush. A tree under construction lives in `objects/<id>.tmp` (see
// `Building`) and joins the registry in one short locked step.
struct Locked {
    home: PathBuf,
    _held: Option<crate::trace::Span>,
    _lock: File,
    registry: Registry,
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("parent component in managed path");
    }
    Ok(path.components().collect())
}
fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o077 != 0 {
        bail!("unsafe OverlayFS private directory {}", path.display());
    }
    Ok(())
}
// A save reserves the registry before its first publication. Nested backend calls clone the
// same open description; they neither wait nor unlock it when their short operation ends.
thread_local! {
    static PUBLICATIONS: std::cell::RefCell<Vec<(PathBuf, File)>> = const { std::cell::RefCell::new(Vec::new()) };
    static NONBLOCKING: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}
/// Periodic preparation skips a busy registry, including reads and optional cache work.
/// Remember collisions even when a best-effort caller catches the lock error.
pub(crate) struct NonBlocking {
    previous: Option<bool>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl NonBlocking {
    pub(crate) fn new() -> Self {
        Self { previous: NONBLOCKING.replace(Some(false)), _thread: Default::default() }
    }
    pub(crate) fn contended(&self) -> bool {
        NONBLOCKING.get() == Some(true)
    }
}
impl Drop for NonBlocking {
    fn drop(&mut self) {
        let contended = self.contended();
        NONBLOCKING.set(self.previous.map(|previous| previous || contended));
    }
}
pub(crate) struct Publication(std::marker::PhantomData<std::rc::Rc<()>>);
impl Drop for Publication {
    fn drop(&mut self) {
        PUBLICATIONS.with(|held| {
            held.borrow_mut().pop();
        });
    }
}
fn reserved(home: &Path) -> Option<io::Result<File>> {
    PUBLICATIONS.with(|held| held.borrow().iter().find(|(h, _)| h == home).map(|(_, f)| f.try_clone()))
}
pub(crate) fn try_publication(path: &Path) -> Result<Option<Publication>> {
    let home = home_of(path)?;
    let file = match reserved(&home) {
        Some(file) => file?,
        None => {
            let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(home.join("lock"))?;
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            file
        }
    };
    PUBLICATIONS.with(|held| held.borrow_mut().push((home, file)));
    Ok(Some(Publication(Default::default())))
}
fn lock_at(home: PathBuf) -> Result<Locked> {
    let lock = if let Some(file) = reserved(&home) {
        file?
    } else {
        let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(home.join("lock"))?;
        {
            let _wait = crate::trace::begin(format!("OverlayFS registry lock wait {}", home.display()));
            let operation = if NONBLOCKING.get().is_some() {
                rustix::fs::FlockOperation::NonBlockingLockExclusive
            } else {
                rustix::fs::FlockOperation::LockExclusive
            };
            if let Err(error) = rustix::fs::flock(&lock, operation) {
                if error == rustix::io::Errno::WOULDBLOCK && NONBLOCKING.get().is_some() {
                    NONBLOCKING.set(Some(true));
                }
                return Err(error.into());
            }
        }
        lock
    };
    let held = crate::trace::span("OverlayFS registry lock held");
    let registry = match fs::read(home.join("registry.json")) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if home.parent().is_some_and(|p| p.join("machine/key").exists()) {
                bail!("OverlayFS registry is missing; restore the store before use");
            }
            Registry::default()
        }
        Err(e) => return Err(e.into()),
    };
    Ok(Locked { home, _held: held, _lock: lock, registry })
}
/// Line folders outside the store (`layr line move`) and the registry that manages them.
static EXTERNAL: std::sync::Mutex<(PathBuf, Vec<PathBuf>)> = std::sync::Mutex::new((PathBuf::new(), Vec::new()));
/// Set the folders outside the store root that `home_root`'s registry manages.
pub fn set_external(home_root: &Path, mut folders: Vec<PathBuf>) {
    folders.extend(crate::views::pending_paths(home_root));
    *EXTERNAL.lock().unwrap() = (home_root.join(".layr-overlayfs"), folders);
}
/// The registry of a folder outside the store: the registry of its volume when the volume has
/// one (a copy made there, see `target_home`), else the store's registry.
fn external_home(path: &Path) -> Option<PathBuf> {
    let e = EXTERNAL.lock().unwrap();
    let folders: Vec<PathBuf> = e.1.iter().filter(|f| path.starts_with(f)).cloned().collect();
    if folders.is_empty() {
        return None;
    }
    folders.iter().map(|f| f.join(".layr-overlayfs")).find(|h| h.is_dir()).or_else(|| Some(e.0.clone()))
}
fn lock(path: &Path) -> Result<Locked> {
    lock_at(home_of(path)?)
}
/// The registry folder that manages `path`: the store's for a folder outside the store that
/// `set_external` names, else the nearest folder with a registry. Two paths with the same
/// folder share one lock, so a caller takes that lock once.
fn home_of(path: &Path) -> Result<PathBuf> {
    if let Some(home) = external_home(&absolute(path)?) {
        private_dir(&home)?;
        return Ok(home);
    }
    for parent in absolute(path)?.ancestors() {
        let home = parent.join(".layr-overlayfs");
        if home.is_dir() {
            private_dir(&home)?;
            return Ok(home);
        }
    }
    bail!("no OverlayFS registry for {}", path.display())
}
fn object_path(home: &Path, id: &str) -> PathBuf {
    home.join("objects").join(id)
}
// An object under construction. Its directory is `objects/<id>.tmp` and its owner holds
// a lock on it, so `collect` removes only the leftovers of a dead owner. `publish`
// renames it to `objects/<id>`; the caller holds the registry lock for that step.
struct Building {
    id: String,
    dir: PathBuf,
    _lock: File,
}
impl Building {
    fn begin(home: &Path) -> Result<Building> {
        let id = crate::ids::new_id();
        let dir = home.join("objects").join(format!("{id}.tmp"));
        private_dir(&dir)?;
        let lock = File::open(&dir)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        Ok(Building { id, dir, _lock: lock })
    }
    fn publish(self, home: &Path) -> Result<String> {
        fs::rename(&self.dir, object_path(home, &self.id))?;
        Ok(self.id)
    }
    fn discard(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Locked {
    fn save(&self) -> Result<()> {
        File::open(self.home.join("objects"))?.sync_all()?;
        crate::fsutil::atomic_write(&self.home.join("registry.json"), &serde_json::to_vec(&self.registry)?, 0o600)
    }
    fn object(&self, id: &str) -> PathBuf {
        object_path(&self.home, id)
    }
    fn new_object(&self) -> Result<String> {
        let id = crate::ids::new_id();
        private_dir(&self.object(&id))?;
        Ok(id)
    }
    fn mount(&self, path: &Path, e: &Entry) -> Result<()> {
        if e.plain {
            return Ok(());
        }
        let data = self.object(&e.lower).join("data");
        let lowers = std::iter::once(&e.lower)
            .chain(e.bases.iter())
            .map(|id| self.object(id).join("data").display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        if e.info.readonly && !e.bases.is_empty() {
            let opts = format!("lowerdir={lowers},redirect_dir=nofollow,index=off");
            sys::overlay_mount(None, path, Some(c"overlay"), libc::MS_RDONLY, Some(&opts))?;
        } else if e.info.readonly {
            sys::overlay_mount(Some(&data), path, None, libc::MS_BIND, None)?;
            if let Err(err) = sys::overlay_mount(None, path, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, None) {
                let _ = sys::overlay_unmount(path);
                return Err(err.into());
            }
        } else {
            let object = self.object(&e.info.uuid);
            let upper = object.join("upper");
            let work = object.join("work");
            let _t = crate::trace::named(|| {
                format!(
                    "overlay mount {} writable: hard_links={} upper_exists={} lowers={}",
                    path.display(),
                    e.hard_links,
                    upper.exists(),
                    lowers.matches(':').count() + 1
                )
            });
            if !upper.exists() {
                if e.hard_links {
                    prepare_upper(&object, &data)?;
                } else {
                    fs::create_dir(&upper)?;
                    let source = fs::metadata(&data)?;
                    sys::overlay_mtime(&upper, source.mtime(), source.mtime_nsec())?;
                }
            }
            if !work.exists() {
                fs::create_dir(&work)?;
            }
            let opts = format!(
                "lowerdir={},upperdir={},workdir={},index=on,redirect_dir=on,metacopy=off",
                lowers,
                upper.display(),
                work.display()
            );
            sys::overlay_mount(None, path, Some(c"overlay"), 0, Some(&opts))
                .with_context(|| format!("mount OverlayFS {}", path.display()))?;
        }
        Ok(())
    }
    fn collect(&self) -> Result<()> {
        // Reclamation is store-wide and recursive. Publication only changes registry ownership;
        // a later outside-lock collection reclaims abandoned construction and unreferenced data.
        if crate::store::holds_project_lock() {
            return Ok(());
        }
        crate::repo::save_barrier("overlay_collect")?;
        let used: HashSet<_> = self
            .registry
            .trees
            .values()
            .filter(|e| !e.plain || e.restore_cache)
            .flat_map(|e| std::iter::once(&e.lower).chain(std::iter::once(&e.info.uuid)).chain(e.bases.iter()))
            .collect();
        for e in fs::read_dir(self.home.join("objects"))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                // Removed only when no live command holds the lock of its owner.
                let dir = File::open(e.path())?;
                if rustix::fs::flock(&dir, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
                    fs::remove_dir_all(e.path())?;
                }
            } else if !used.contains(&name) {
                fs::remove_dir_all(e.path())?;
            }
        }
        Ok(())
    }
}
// A writable tree over hard-linked data starts from a full copy, so its links stay inside it.
fn prepare_upper(object: &Path, data: &Path) -> Result<()> {
    let _t = crate::trace::named(|| format!("overlay prepare upper (COPY) {}", data.display()));
    let temp = object.join("upper-pending");
    if temp.exists() {
        fs::remove_dir_all(&temp)?;
    }
    copy_tree(data, &temp, &HashSet::new(), &mut HashMap::new())?;
    sync_tree(&temp)?;
    fs::rename(&temp, object.join("upper"))?;
    File::open(object)?.sync_all()?;
    Ok(())
}
fn mounted(path: &Path) -> Result<bool> {
    // Managed roots reject whitespace and mount-option separators on creation.
    let name = path.to_str().context("non-UTF8 mountpoint")?;
    Ok(fs::read_to_string("/proc/self/mountinfo")?.lines().any(|l| l.split_whitespace().nth(4) == Some(name)))
}
fn valid_path(path: &Path) -> Result<()> {
    let s = path.to_str().context("non-UTF8 OverlayFS path")?;
    if s.chars().any(|c| c.is_whitespace() || ",:\\".contains(c)) {
        bail!("OverlayFS store paths cannot contain whitespace, commas, colons, or backslashes");
    }
    // Do not resolve mount paths through a caller-controlled symlink.
    for p in path.ancestors() {
        if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("symlink in OverlayFS path");
        }
    }
    Ok(())
}
fn shared_parent(path: &Path, mounts: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let mount = Path::new(*fields.get(4)?);
            if !path.starts_with(mount) {
                return None;
            }
            let shared = fields.iter().skip(6).take_while(|f| **f != "-").any(|f| f.starts_with("shared:"));
            Some((mount.components().count(), shared))
        })
        .max_by_key(|(depth, _)| *depth)
        .is_some_and(|(_, shared)| shared)
}
pub(super) fn recover_existing(root: &Path) -> Result<()> {
    if root.join(".layr-overlayfs").is_dir() {
        recover(root)
    } else {
        Ok(())
    }
}
pub fn recover(root: &Path) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        bail!("OverlayFS requires the root service or a root command");
    }
    let root = absolute(root)?;
    valid_path(&root)?;
    // MS_MOVE cannot publish a received tree below a shared parent mount.
    // Refuse this setup before creating a store or changing its registry.
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    if shared_parent(&root, &mounts) {
        bail!(
            "OverlayFS store requires a private mount; bind-mount the store onto itself and run mount --make-private on that mount"
        );
    }
    // OverlayFS does not accept layers in folders that ignore case.
    if crate::sys::casefold(&root) {
        bail!("{} ignores case (ext4 casefold); OverlayFS cannot use it. Use a folder without `chattr +F`", root.display());
    }
    fs::create_dir_all(&root)?;
    let home = root.join(".layr-overlayfs");
    if !home.exists() && root.join("machine/key").exists() {
        bail!("this store uses another backend; use peer sync to transfer into a new OverlayFS store");
    }
    private_dir(&home)?;
    private_dir(&home.join("objects"))?;
    let mut state = lock_at(home)?;
    if !state.home.join("registry.json").exists() {
        state.save()?;
    }
    if let Some((from, to)) = state.registry.pending_move.clone() {
        // A line moved out of the store (or back) has one end outside it.
        if !from.starts_with(&root) && !to.starts_with(&root) {
            bail!("rename outside OverlayFS store");
        }
        valid_path(&from)?;
        valid_path(&to)?;
        if mounted(&from)? {
            sys::overlay_mount(Some(&from), &to, None, libc::MS_MOVE, None)?;
        } else if !state.registry.trees.get(&from).is_some_and(|e| !e.plain) && from.exists() {
            if to.exists() {
                bail!("both paths exist during OverlayFS rename recovery");
            }
            fs::rename(&from, &to)?;
        }
        let paths: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(&from)).cloned().collect();
        for old in paths {
            let new = absolute(&to.join(old.strip_prefix(&from)?))?;
            let e = state.registry.trees.remove(&old).unwrap();
            state.registry.trees.insert(new, e);
        }
        state.registry.pending_move = None;
        state.save()?;
        if from.exists() {
            fs::remove_dir(&from)?;
        }
    }
    for (path, e) in state.registry.trees.clone().iter() {
        if !path.starts_with(&root) && external_home(path).is_none() {
            bail!("OverlayFS mount outside its store");
        }
        valid_path(path)?;
        if e.restore_cache {
            finish_cache_restore(&mut state, path, e)?;
        }
        if e.plain {
            continue;
        }
        if !mounted(path)? {
            fs::create_dir_all(path)?;
            state.mount(path, e)?;
        }
    }
    // Upgrade old nested cache mounts after their parent mounts exist (not a layer taken from a
    // saved state: that is a mount on purpose).
    let caches: Vec<_> = state
        .registry
        .trees
        .iter()
        .filter(|(p, e)| {
            !e.info.readonly
                && !e.plain
                && !e.taken
                && state
                    .registry
                    .trees
                    .iter()
                    .any(|(parent, pe)| *p != parent && p.starts_with(parent) && !pe.info.readonly && !pe.plain)
        })
        .map(|(p, e)| (p.clone(), e.clone()))
        .collect();
    for (path, mut e) in caches {
        let id = state.new_object()?;
        // Freeze the old cache before its durable migration record. Open
        // writers make this fail rather than lose writes after the copy.
        sys::overlay_mount(None, &path, None, libc::MS_REMOUNT | libc::MS_RDONLY, None)
            .context("close writers in the cache before its migration")?;
        let copied = (|| -> Result<()> {
            let consistency = copy_stable(&path, &state.object(&id).join("data"), &HashSet::new())?;
            report_consistency(&path, consistency);
            sync_tree(&state.object(&id).join("data"))
        })();
        if let Err(error) = copied {
            sys::overlay_mount(None, &path, None, libc::MS_REMOUNT, None)?;
            return Err(error);
        }
        e.lower = id;
        e.bases.clear();
        e.plain = true;
        e.restore_cache = true;
        state.registry.trees.insert(path.clone(), e.clone());
        state.save()?;
        finish_cache_restore(&mut state, &path, &e)?;
    }
    Ok(())
}
fn finish_cache_restore(state: &mut Locked, path: &Path, entry: &Entry) -> Result<()> {
    if mounted(path)? {
        sys::overlay_unmount(path).context("close files in the cache before its migration")?;
    }
    let parent = pinned_directory(path.parent().context("cache parent")?)?;
    let name = path.file_name().context("cache name")?;
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    if sys::overlay_attribute(&anchored.join(name), c"trusted.layr.cache-restore", None).ok().as_deref()
        != Some(entry.lower.as_bytes())
    {
        let temp_name = format!(".layr-cache-restore-{}", entry.lower);
        let temp = anchored.join(&temp_name);
        if fs::symlink_metadata(&temp).is_ok() {
            fs::remove_dir_all(&temp)?;
        }
        copy_cache_at(
            &state.object(&entry.lower).join("data"),
            &parent,
            std::ffi::OsStr::new(&temp_name),
            Some(entry.lower.as_bytes()),
            |_| {},
        )?;
        // Replaces only an empty mountpoint. Never replace existing user data.
        rustix::fs::renameat(&parent, &temp_name, &parent, name)?;
        parent.sync_all()?;
    }
    state.registry.trees.get_mut(path).context("cache registry entry")?.restore_cache = false;
    state.save()?;
    state.collect()
}
pub(super) fn supports_volume(path: &Path) -> bool {
    rustix::process::geteuid().is_root() && lock(path).is_ok()
}
pub(super) fn managed_root(path: &Path) -> bool {
    path.is_dir() && native_info(path).is_ok()
}
/// One registry snapshot per validation scan. A registered path remains a boundary
/// when a build replaces its cache with a plain directory, as it does for snapshots.
pub(crate) struct NestedRoots(HashSet<PathBuf>);
impl NestedRoots {
    pub(crate) fn read(root: &Path) -> Result<Self> {
        let root = absolute(root)?;
        let state = lock(&root)?;
        Ok(Self(nested_trees(&state.registry, &root)))
    }
    pub(crate) fn classify(&self, path: &Path, _metadata: &fs::Metadata) -> io::Result<crate::fsutil::StorageRoot> {
        Ok(if self.0.contains(path) { crate::fsutil::StorageRoot::Managed } else { crate::fsutil::StorageRoot::Unmanaged })
    }
}

pub(super) fn native_info(path: &Path) -> Result<SubvolInfo> {
    let path = absolute(path)?;
    let state = lock(&path)?;
    Ok(state.registry.trees.get(&path).context("not an OverlayFS tree")?.info.clone())
}
enum InitialContent<'a> {
    Empty,
    Frozen(&'a dyn layr_fs_core::TreeRead),
}
fn create_subvolume_native(path: &Path, publication: &mut driver::Publication, content: InitialContent<'_>) -> Result<()> {
    let path = absolute(path)?;
    valid_path(&path)?;
    let mut state = lock(&path)?;
    if path.exists() {
        bail!("destination already exists");
    }
    *publication = driver::Publication::Objects(state.home.clone());
    let id = state.new_object()?;
    let data = state.object(&id).join("data");
    fs::create_dir(&data)?;
    let readonly = matches!(content, InitialContent::Frozen(_));
    if let InitialContent::Frozen(source) = content {
        let target = layr_fs_unix::DirectoryTree::open(&data)?;
        layr_fs_common::copy_tree(source, &target)?;
        sync_tree(&data)?;
    }
    let entry = Entry {
        info: SubvolInfo { uuid: id.clone(), received_uuid: None, parent_uuid: None, readonly },
        lower: id,
        hard_links: if readonly { tree_has_links(&data)? } else { false },
        bases: Vec::new(),
        restore_cache: false,
        taken: false,
        plain: !readonly && state.registry.trees.iter().any(|(p, e)| path.starts_with(p) && !e.info.readonly && !e.plain),
    };
    fs::create_dir(&path)?;
    publication.directory(&state.home, None);
    let metadata = fs::symlink_metadata(&path)?;
    anyhow::ensure!(metadata.is_dir() && metadata.uid() == 0, "created OverlayFS directory was replaced");
    *publication = driver::Publication::Tree { home: state.home.clone(), identity: (metadata.dev(), metadata.ino()) };
    state.registry.trees.insert(path.clone(), entry.clone());
    state.save()?;
    state.mount(&path, &entry)?;
    if !entry.plain {
        let metadata = fs::symlink_metadata(&path)?;
        *publication = driver::Publication::Tree { home: state.home.clone(), identity: (metadata.dev(), metadata.ino()) };
    }
    Ok(())
}
// A source writer must not silently produce a mixed snapshot.
type Fingerprint = (u64, u64, u32, u64, i64, i64, i64, i64);
fn scan(path: &Path, nested: &HashSet<PathBuf>) -> Result<BTreeMap<PathBuf, Fingerprint>> {
    fn visit(root: &Path, p: &Path, nested: &HashSet<PathBuf>, out: &mut BTreeMap<PathBuf, Fingerprint>) -> Result<()> {
        let m = fs::symlink_metadata(p)?;
        out.insert(
            p.strip_prefix(root)?.to_path_buf(),
            (m.dev(), m.ino(), m.mode(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()),
        );
        if m.is_dir() && !nested.contains(p) {
            for e in fs::read_dir(p)? {
                visit(root, &e?.path(), nested, out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(path, path, nested, &mut out)?;
    Ok(out)
}
fn copy_tree(src: &Path, dst: &Path, nested: &HashSet<PathBuf>, links: &mut HashMap<(u64, u64), PathBuf>) -> Result<()> {
    copy_tree_at(src, src, dst, nested, links, false)
}
fn copy_tree_at(
    src: &Path,
    logical: &Path,
    dst: &Path,
    nested: &HashSet<PathBuf>,
    links: &mut HashMap<(u64, u64), PathBuf>,
    root_exists: bool,
) -> Result<()> {
    let m = fs::symlink_metadata(src)?;
    if m.is_dir() {
        let dir = fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(src)?;
        let actual = dir.metadata()?;
        if (actual.dev(), actual.ino()) != (m.dev(), m.ino()) {
            bail!("source directory changed during snapshot");
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        if !root_exists {
            fs::create_dir(dst)?;
        }
        if !nested.contains(logical) {
            for e in fs::read_dir(&anchored)? {
                let name = e?.file_name();
                copy_tree_at(&anchored.join(&name), &logical.join(&name), &dst.join(&name), nested, links, false)?;
            }
        }
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if m.is_file() {
        if let Some(first) = links.get(&(m.dev(), m.ino())) {
            fs::hard_link(first, dst)?;
        } else {
            let s = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(src)?;
            let actual = s.metadata()?;
            if !actual.is_file() || (actual.dev(), actual.ino()) != (m.dev(), m.ino()) {
                bail!("source file changed during snapshot");
            }
            let d = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
            layr_fs_unix::copy_contents(&s, &d)?;
            links.insert((m.dev(), m.ino()), dst.to_path_buf());
        }
    } else {
        bail!("cannot snapshot special file {}", src.display());
    }
    rustix::fs::chownat(
        rustix::fs::CWD,
        dst,
        Some(rustix::fs::Uid::from_raw(m.uid())),
        Some(rustix::fs::Gid::from_raw(m.gid())),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?;
    if !m.file_type().is_symlink() {
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    }
    for (name, value) in sys::overlay_xattrs(src)? {
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            continue;
        }
        sys::overlay_attribute(dst, &std::ffi::CString::new(name)?, Some(&value))?;
    }
    sys::overlay_attribute(dst, c"trusted.layr.origin", Some(&native_file_identity(src)?))?;
    sys::overlay_mtime(dst, m.mtime(), m.mtime_nsec())?;
    Ok(())
}
// Cache destinations are inside a user's tree. Pin the parent and new root,
// and keep the root private until all children have been copied. A rename or
// symlink replacement cannot redirect privileged writes outside this directory.
fn pinned_directory(path: &Path) -> Result<File> {
    use rustix::fs::{open, openat, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = open("/", flags, Mode::empty())?;
    for component in absolute(path)?.components() {
        if let std::path::Component::Normal(name) = component {
            dir = openat(&dir, name, flags, Mode::empty())?;
        }
    }
    Ok(File::from(dir))
}
fn copy_cache(src: &Path, dst: &Path, created: impl FnMut(Option<&fs::Metadata>)) -> Result<()> {
    let parent = pinned_directory(dst.parent().context("cache parent")?)?;
    copy_cache_at(src, &parent, dst.file_name().context("cache name")?, None, created)
}
fn copy_cache_at(
    src: &Path,
    parent: &File,
    name: &std::ffi::OsStr,
    marker: Option<&[u8]>,
    mut created: impl FnMut(Option<&fs::Metadata>),
) -> Result<()> {
    use rustix::fs::{mkdirat, openat, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    mkdirat(parent, name, Mode::from_raw_mode(0o700))?;
    created(None);
    let root = File::from(openat(parent, name, flags, Mode::empty())?);
    let m = root.metadata()?;
    if m.uid() != 0 || m.mode() & 0o777 != 0o700 {
        bail!("cache destination changed during restore");
    }
    created(Some(&m));
    let anchored = PathBuf::from(format!("/proc/self/fd/{}/.", root.as_raw_fd()));
    let before = scan(src, &HashSet::new())?;
    copy_tree_at(src, src, &anchored, &HashSet::new(), &mut HashMap::new(), true)?;
    if before != scan(src, &HashSet::new())? {
        bail!("cache source changed during restore");
    }
    if let Some(value) = marker {
        sys::overlay_attribute(&anchored, c"trusted.layr.cache-restore", Some(value))?;
    }
    sync_tree(&anchored)?;
    let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
    if (named.st_dev as u64, named.st_ino as u64) != (m.dev(), m.ino()) {
        bail!("cache destination moved during restore");
    }
    Ok(())
}

// OverlayFS can update directory metadata on the first access. Retry a full
// copy, but publish only a copy whose before and after scans agree. When writers change the
// source in every round, the last copy stays and the save marks its state as taken while
// processes ran.
fn copy_stable(src: &Path, dst: &Path, nested: &HashSet<PathBuf>) -> Result<CaptureConsistency> {
    for _ in 0..3 {
        let before = scan(src, nested)?;
        copy_tree(src, dst, nested, &mut HashMap::new())?;
        crate::repo::save_barrier("copy")?;
        if before == scan(src, nested)? {
            return Ok(CaptureConsistency::ValidatedCopy);
        }
        fs::remove_dir_all(dst)?;
    }
    copy_tree(src, dst, nested, &mut HashMap::new())?;
    Ok(CaptureConsistency::UnsettledCopy)
}
// Persist the copied tree before publishing its registry entry. One filesystem
// flush covers file data, xattrs and directory entries without one fsync per file.
fn sync_tree(path: &Path) -> Result<()> {
    rustix::fs::syncfs(File::open(path)?)?;
    Ok(())
}
// The live mount and its open file descriptors stay unchanged. Copy only its
// upper layer, then mount that immutable copy above the same immutable bases.
// Compact long chains and hard-linked trees into one portable tree.
fn nested_trees(registry: &Registry, src: &Path) -> HashSet<PathBuf> {
    registry.trees.keys().filter(|p| **p != src && p.starts_with(src)).cloned().collect()
}
// Copies into `build` without the registry lock. The caller publishes the result only
// if the registry still holds the same parent entry and nested trees.
fn snapshot_layers(
    home: &Path,
    nested: &HashSet<PathBuf>,
    src: &Path,
    parent: &Entry,
    build: &Building,
) -> Result<(Vec<String>, bool, CaptureConsistency)> {
    let data = build.dir.join("data");
    if parent.plain {
        // A build can replace a cache directory. Never follow its replacement
        // symlink with the service's credentials.
        let pinned = pinned_directory(src)?;
        let anchored = PathBuf::from(format!("/proc/self/fd/{}/.", pinned.as_raw_fd()));
        let excluded = nested.iter().map(|p| anchored.join(p.strip_prefix(src).unwrap())).collect();
        let consistency = copy_stable(&anchored, &data, &excluded)?;
        sync_tree(&data)?;
        return Ok((Vec::new(), tree_has_links(&data)?, consistency));
    }
    let hard_links = tree_has_links_excluding(src, nested)?;
    let bases: Vec<_> = std::iter::once(parent.lower.clone()).chain(parent.bases.iter().cloned()).collect();
    let option_bytes: usize = bases.iter().map(|id| object_path(home, id).as_os_str().len() + 6).sum();
    if hard_links || bases.len() >= 16 || option_bytes > 2800 {
        let consistency = copy_stable(src, &data, nested)?;
        sync_tree(&data)?;
        return Ok((Vec::new(), hard_links, consistency));
    }
    let upper = object_path(home, &parent.info.uuid).join("upper");
    for round in 0..3 {
        let excluded: HashSet<_> = nested.iter().map(|p| upper.join(p.strip_prefix(src).unwrap())).collect();
        let upper_scan = || -> Result<_> {
            let mut found = scan(&upper, &excluded)?;
            for p in &excluded {
                found.remove(p.strip_prefix(&upper)?);
            }
            Ok(found)
        };
        let before = upper_scan()?;
        let mut consistency = CaptureConsistency::ValidatedCopy;
        copy_delta(&upper, src, &data, nested, &mut HashMap::new(), &mut consistency)?;
        // A cache may exist only in a lower layer. Mask it in this snapshot.
        for cache in nested {
            let rel = cache.strip_prefix(src)?;
            let target = data.join(rel);
            // Validate every parent before writing below a copied directory.
            let mut p = data.clone();
            for component in rel.components() {
                p.push(component);
                match fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => bail!("cache path crosses a non-directory"),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&p)?,
                    Err(e) => return Err(e.into()),
                }
            }
            sys::overlay_attribute(&target, c"trusted.overlay.opaque", Some(b"y"))?;
        }
        crate::repo::save_barrier("copy")?;
        let settled = before == upper_scan()?;
        // Writers changed the upper layer in every round: keep the last copy (see `copy_stable`).
        if settled || round == 2 {
            if !settled {
                consistency = CaptureConsistency::UnsettledCopy;
            }
            sync_tree(&data)?;
            return Ok((bases, false, consistency));
        }
        fs::remove_dir_all(&data)?;
    }
    unreachable!("the last round returns")
}
// The tree that was copied must still be the one the registry names.
fn same_source(state: &Locked, src: &Path, parent: &Entry, nested: &HashSet<PathBuf>) -> Result<()> {
    let now = state.registry.trees.get(src).context("snapshot source changed during the copy; retry")?;
    if now.info.uuid != parent.info.uuid
        || now.info.readonly != parent.info.readonly
        || now.lower != parent.lower
        || now.bases != parent.bases
        || nested_trees(&state.registry, src) != *nested
    {
        bail!("snapshot source changed during the copy; retry");
    }
    Ok(())
}
fn tree_has_links_excluding(path: &Path, nested: &HashSet<PathBuf>) -> Result<bool> {
    if nested.contains(path) {
        return Ok(false);
    }
    let m = fs::symlink_metadata(path)?;
    if m.is_file() {
        return Ok(m.nlink() > 1);
    }
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            if tree_has_links_excluding(&e?.path(), nested)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn copy_delta(
    src: &Path,
    visible: &Path,
    dst: &Path,
    nested: &HashSet<PathBuf>,
    links: &mut HashMap<(u64, u64), PathBuf>,
    consistency: &mut CaptureConsistency,
) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let m = fs::symlink_metadata(src)?;
    let attrs = sys::overlay_xattrs(src)?;
    if nested.contains(visible) {
        fs::create_dir(dst)?;
        sys::overlay_attribute(dst, c"trusted.overlay.opaque", Some(b"y"))?;
        return Ok(());
    }
    if attrs.iter().any(|(name, _)| name == b"trusted.overlay.redirect") {
        // Redirects refer to the old lower namespace. Materialize only this
        // moved directory, with an opaque marker to hide its previous target.
        if copy_stable(visible, dst, nested)?.running() {
            *consistency = CaptureConsistency::UnsettledCopy;
        }
        sys::overlay_attribute(dst, c"trusted.overlay.opaque", Some(b"y"))?;
        return Ok(());
    }
    if m.is_dir() {
        let dir = fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(src)?;
        let actual = dir.metadata()?;
        if (actual.dev(), actual.ino()) != (m.dev(), m.ino()) {
            bail!("source directory changed during snapshot");
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        fs::create_dir(dst)?;
        for e in fs::read_dir(&anchored)? {
            let name = e?.file_name();
            copy_delta(&anchored.join(&name), &visible.join(&name), &dst.join(&name), nested, links, consistency)?;
        }
    } else if m.file_type().is_char_device() && m.rdev() == 0 {
        rustix::fs::mknodat(rustix::fs::CWD, dst, rustix::fs::FileType::CharacterDevice, rustix::fs::Mode::empty(), 0)?;
    } else {
        copy_tree(src, dst, &HashSet::new(), links)?;
    }
    rustix::fs::chownat(
        rustix::fs::CWD,
        dst,
        Some(rustix::fs::Uid::from_raw(m.uid())),
        Some(rustix::fs::Gid::from_raw(m.gid())),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?;
    if !m.file_type().is_symlink() {
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    }
    for (name, value) in attrs {
        if name.starts_with(b"trusted.overlay.") && name != b"trusted.overlay.opaque" {
            continue;
        }
        sys::overlay_attribute(dst, &std::ffi::CString::new(name)?, Some(&value))?;
    }
    sys::overlay_mtime(dst, m.mtime(), m.mtime_nsec())?;
    Ok(())
}

/// A read-only snapshot for comparing content.
/// A writable copy of a cache folder (see the APFS and btrfs versions).
/// Returns false: every entry needs a check of its owner (see the btrfs version).
fn report_consistency(src: &Path, consistency: CaptureConsistency) {
    if consistency.running() {
        crate::fsutil::mark_unsettled(src);
    }
}

fn snapshot_native(src: &Path, dst: &Path, readonly: bool, publication: &mut driver::Publication) -> Result<CaptureConsistency> {
    let src = absolute(src)?;
    let dst = absolute(dst)?;
    valid_path(&dst)?;
    // Read the plan under the registry lock, copy without it, then publish under it.
    let (home, parent, nested, plain, build) = {
        let state = lock(&src)?;
        if lock_home(&dst)? != state.home {
            bail!("snapshot must stay in one store");
        }
        if dst.exists() {
            bail!("snapshot destination exists");
        }
        let parent = state.registry.trees.get(&src).context("snapshot source is unmanaged")?.clone();
        let nested = nested_trees(&state.registry, &src);
        // A writable copy of a saved state (a cache taken by a new line) is a layer over the state's
        // lower layers: no copy at line new. Only a writable snapshot of a writable tree copies.
        let plain = !readonly
            && !parent.info.readonly
            && state.registry.trees.iter().any(|(p, e)| dst.starts_with(p) && !e.info.readonly && !e.plain);
        let _t = crate::trace::named(|| {
            format!(
                "overlay snapshot {} -> {}: readonly={} src_readonly={} src_plain={} src_hard_links={} src_bases={} => {}",
                src.display(),
                dst.display(),
                readonly,
                parent.info.readonly,
                parent.plain,
                parent.hard_links,
                parent.bases.len(),
                if plain {
                    "COPY (plain)"
                } else if !readonly && parent.hard_links {
                    "LAYER + COPY (hard links)"
                } else {
                    "LAYER"
                }
            )
        });
        *publication = driver::Publication::Objects(state.home.clone());
        (state.home.clone(), parent, nested, plain, Building::begin(&state.home)?)
    };
    let copied = copy_snapshot(&home, &nested, &src, &dst, readonly, plain, &parent, &build, publication);
    let (lower, bases, hard_links, consistency) = match copied {
        Ok(value) => value,
        Err(error) => {
            build.discard();
            return Err(error);
        }
    };
    let mut state = lock_at(home.clone())?;
    let published = (|| -> Result<Entry> {
        same_source(&state, &src, &parent, &nested)?;
        if state.registry.trees.contains_key(&dst) {
            bail!("snapshot destination exists");
        }
        Ok(Entry {
            info: SubvolInfo { uuid: String::new(), received_uuid: None, parent_uuid: Some(parent.info.uuid.clone()), readonly },
            lower,
            hard_links,
            bases,
            plain,
            restore_cache: false,
            taken: !readonly && !plain && parent.info.readonly,
        })
    })();
    let mut entry = match published {
        Ok(entry) => entry,
        Err(error) => {
            build.discard();
            return Err(error);
        }
    };
    let id = build.publish(&home)?;
    entry.info.uuid = id.clone();
    if entry.lower.is_empty() {
        entry.lower = id;
    }
    File::open(state.object(&entry.lower))?.sync_all()?;
    if !plain {
        fs::create_dir(&dst)?;
        publication.directory(&home, None);
        let metadata = fs::symlink_metadata(&dst)?;
        anyhow::ensure!(metadata.is_dir() && metadata.uid() == 0, "created OverlayFS directory was replaced");
        *publication = driver::Publication::Tree { home: home.clone(), identity: (metadata.dev(), metadata.ino()) };
    }
    state.registry.trees.insert(dst.clone(), entry.clone());
    state.save()?;
    state.mount(&dst, &entry)?;
    if !plain {
        let metadata = fs::symlink_metadata(&dst)?;
        *publication = driver::Publication::Tree { home, identity: (metadata.dev(), metadata.ino()) };
    }
    Ok(consistency)
}
// The slow part of a snapshot: no registry lock is held. Returns the lower layer (empty
// for the new object itself), the bases and the hard-link flag.
fn copy_snapshot(
    home: &Path,
    nested: &HashSet<PathBuf>,
    src: &Path,
    dst: &Path,
    readonly: bool,
    plain: bool,
    parent: &Entry,
    build: &Building,
    publication: &mut driver::Publication,
) -> Result<(String, Vec<String>, bool, CaptureConsistency)> {
    let (lower, bases, hard_links, consistency) = if parent.info.readonly {
        (parent.lower.clone(), parent.bases.clone(), parent.hard_links, CaptureConsistency::ValidatedCopy)
    } else {
        let (bases, hard_links, consistency) = snapshot_layers(home, nested, src, parent, build)?;
        (String::new(), bases, hard_links, consistency)
    };
    if plain {
        // A cache is an ordinary directory. Its saved copy remains private.
        if parent.plain {
            let data = if lower.is_empty() { build.dir.join("data") } else { object_path(home, &lower).join("data") };
            copy_cache(&data, dst, |metadata| publication.directory(home, metadata))?;
        } else {
            copy_cache(src, dst, |metadata| publication.directory(home, metadata))?;
        }
        sync_tree(dst)?;
    } else if !readonly && hard_links {
        // Build the private copy of hard-linked data before the registry lock is taken.
        let data = if lower.is_empty() { build.dir.join("data") } else { object_path(home, &lower).join("data") };
        prepare_upper(&build.dir, &data)?;
    }
    Ok((lower, bases, hard_links, consistency))
}
fn lock_home(path: &Path) -> Result<PathBuf> {
    if let Some(home) = external_home(&absolute(path)?) {
        return Ok(home);
    }
    for p in absolute(path)?.ancestors() {
        if p.join(".layr-overlayfs").is_dir() {
            return Ok(p.join(".layr-overlayfs"));
        }
    }
    bail!("path is outside the store")
}
#[cfg(test)]
pub(super) fn seal_tree(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    let mut state = lock(&path)?;
    let mut e = state.registry.trees.get(&path).context("unmanaged tree")?.clone();
    if e.info.readonly {
        return Ok(());
    }
    if e.plain {
        bail!("cannot seal a working cache in place; snapshot it instead");
    }
    // Sealing keeps the registry lock: the tree is unmounted and remounted in one step.
    let nested = nested_trees(&state.registry, &path);
    let build = Building::begin(&state.home)?;
    let (bases, hard_links, consistency) = match snapshot_layers(&state.home, &nested, &path, &e, &build) {
        Ok(value) => value,
        Err(error) => {
            build.discard();
            return Err(error);
        }
    };
    report_consistency(&path, consistency);
    let lower = build.publish(&state.home)?;
    sys::overlay_unmount(&path).with_context(|| format!("unmount {}", path.display()))?;
    e.hard_links = hard_links;
    e.lower = lower;
    e.bases = bases;
    e.info.readonly = true;
    state.registry.trees.insert(path.clone(), e.clone());
    state.save()?;
    state.mount(&path, &e)?;
    // The sealed copy owns the visible data. The old writable upper is unused.
    for name in ["upper", "work"] {
        let old = state.object(&e.info.uuid).join(name);
        if old.exists() {
            fs::remove_dir_all(old)?;
        }
    }
    File::open(state.object(&e.info.uuid))?.sync_all()?;
    state.collect()
}
#[cfg(test)]
pub(super) fn unseal_tree(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    let mut state = lock(&path)?;
    let mut e = state.registry.trees.get(&path).context("unmanaged tree")?.clone();
    if !e.info.readonly {
        return Ok(());
    }
    sys::overlay_unmount(&path).with_context(|| format!("unmount {}", path.display()))?;
    // Use a new upper directory after every read-only transition.
    for name in ["upper", "work"] {
        let p = state.object(&e.info.uuid).join(name);
        if p.exists() {
            fs::remove_dir_all(p)?;
        }
    }
    e.info.readonly = false;
    state.registry.trees.insert(path.clone(), e.clone());
    state.save()?;
    state.mount(&path, &e)
}
pub fn rename_tree(from: &Path, to: &Path) -> Result<()> {
    rename_tree_using(from, to, true, || Ok(fs::rename(from, to)?))
}
pub(super) fn rename_plain_tree(from: &Path, to: &Path, rename: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    rename_tree_using(from, to, false, rename)
}
fn rename_tree_using(from: &Path, to: &Path, allow_mount: bool, rename: impl FnOnce() -> Result<()>) -> Result<()> {
    let from = absolute(from)?;
    let to = absolute(to)?;
    valid_path(&to)?;
    let mut state = lock(&from)?;
    if lock_home(&to)? != state.home {
        bail!("rename must stay in one store");
    }
    let managed_root = state.registry.trees.get(&from).is_some_and(|e| !e.plain);
    anyhow::ensure!(allow_mount || !managed_root, "migrate a mounted cache before replacing its folder");
    if !state.registry.trees.keys().any(|p| p.starts_with(&from)) {
        return rename();
    }
    if to.exists() {
        bail!("destination already exists");
    }
    valid_path(&from)?;
    if managed_root {
        fs::create_dir(&to)?;
    }
    state.registry.pending_move = Some((from.clone(), to.clone()));
    state.save()?;
    let _t = crate::trace::named(|| {
        let nested = state.registry.trees.keys().filter(|p| p.starts_with(&from) && **p != from).count();
        format!("overlay move {} -> {}: managed_root={managed_root} nested_entries={nested}", from.display(), to.display())
    });
    let moved: Result<()> =
        if managed_root { sys::overlay_mount(Some(&from), &to, None, libc::MS_MOVE, None).map_err(Into::into) } else { rename() };
    if let Err(e) = moved {
        state.registry.pending_move = None;
        state.save()?;
        if managed_root {
            fs::remove_dir(&to)?;
        }
        return Err(e.into());
    }
    let moves: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(&from)).cloned().collect();
    for old in moves {
        let new = absolute(&to.join(old.strip_prefix(&from)?))?;
        let e = state.registry.trees.remove(&old).unwrap();
        state.registry.trees.insert(new, e);
    }
    state.registry.pending_move = None;
    state.save()?;
    if managed_root {
        fs::remove_dir(from)?;
    }
    Ok(())
}
/// The registered trees at and below `path`, the deepest first.
fn trees_below(state: &Locked, path: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(path)).cloned().collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    paths
}
fn is_busy(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EBUSY)
}
/// Unmount the trees at and below `path` before the caller records their deletion (`layr line
/// delete`), so an unmount error stops the command before the record. The registry keeps them: if
/// the record fails, the next open of the store mounts them again. A mount that a process uses (its
/// working directory or an open file) stays mounted here; the removal detaches it after the record.
pub fn release_tree(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    let state = lock(&path)?;
    valid_path(&path)?;
    for p in trees_below(&state, &path) {
        if mounted(&p)? {
            match sys::overlay_unmount(&p) {
                Err(e) if is_busy(&e) => {}
                r => r.with_context(|| format!("unmount before removal {}", p.display()))?,
            }
        }
    }
    Ok(())
}
/// Unmount a tree for its removal. A process uses the tree (a shell with its working directory in a
/// deleted line): the tree goes, as on APFS, so detach the mount from the folder. The process keeps a
/// private view of a tree that no command reads or saves again. Its writes go only into this tree's own
/// upper folder, which the removal deletes. Its lower layers are read-only, and every other mount has its
/// own upper and work folders.
fn unmount_for_removal(path: &Path) -> Result<()> {
    match sys::overlay_unmount(path) {
        Err(e) if is_busy(&e) => sys::overlay_detach(path),
        r => r,
    }
    .with_context(|| format!("unmount before removal {}", path.display()))
}
struct PrivateLocation {
    parent: std::sync::Arc<File>,
    name: std::ffi::OsString,
}
struct PrivateDirectory {
    location: PrivateLocation,
    directory: File,
}
impl PrivateDirectory {
    fn remove_empty(&self) -> Result<()> {
        let actual =
            rustix::fs::statat(self.location.parent.as_ref(), &self.location.name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
        let expected = self.directory.metadata()?;
        anyhow::ensure!(
            (actual.st_dev as u64, actual.st_ino as u64) == (expected.dev(), expected.ino()),
            "removal custody directory moved"
        );
        // Only unlink an empty directory here. Recursive deletion uses the private FD,
        // so a replacement in the writable parent can never have its contents removed.
        rustix::fs::unlinkat(self.location.parent.as_ref(), &self.location.name, rustix::fs::AtFlags::REMOVEDIR)?;
        Ok(())
    }
}
enum RemovalStage {
    Trees,
    Directory,
    OpenCustody(PrivateLocation),
    EmptyCustody(std::sync::Arc<PrivateDirectory>),
    Candidate(std::sync::Arc<PrivateDirectory>),
    Verified(std::sync::Arc<PrivateDirectory>),
    RestoreForeign(std::sync::Arc<PrivateDirectory>),
    RemoveCustody(std::sync::Arc<PrivateDirectory>),
    RemoveForeignCustody(std::sync::Arc<PrivateDirectory>),
    Collect,
}
struct PinnedMountpoint {
    _directory: File,
    device: u64,
    inode: u64,
}
impl PinnedMountpoint {
    fn new(directory: File) -> Result<Self> {
        let metadata = directory.metadata()?;
        Ok(Self { _directory: directory, device: metadata.dev(), inode: metadata.ino() })
    }
    fn below(parent: &File, name: &std::ffi::OsStr) -> Result<Self> {
        use rustix::{
            fs::{openat, Mode, OFlags},
            mount::{open_tree, OpenTreeFlags},
        };
        // std cannot look below a mount. A nonrecursive detached clone of the parent
        // excludes child mounts, so this pins the covered directory before unmount.
        let detached = open_tree(
            parent,
            "",
            OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC | OpenTreeFlags::AT_EMPTY_PATH,
        )?;
        let directory = File::from(openat(
            &detached,
            name,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        Self::new(directory)
    }
}
enum RootIdentity {
    Absent,
    Expected(u64, u64),
    Mountpoint(PinnedMountpoint),
}
impl RootIdentity {
    fn key(&self) -> Option<(u64, u64)> {
        match self {
            Self::Absent => None,
            Self::Expected(device, inode) => Some((*device, *inode)),
            Self::Mountpoint(pin) => Some((pin.device, pin.inode)),
        }
    }
}
impl PartialEq for RootIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
struct Removal {
    path: PathBuf,
    home: PathBuf,
    identity: RootIdentity,
    stage: RemovalStage,
    parent: std::sync::Arc<File>,
}
impl Removal {
    fn new(path: PathBuf, home: PathBuf, parent: std::sync::Arc<File>, identity: RootIdentity) -> Self {
        Self { path, home, parent, identity, stage: RemovalStage::Trees }
    }
    fn observe(&self) -> Result<RootIdentity> {
        let held_parent = self.parent.metadata()?;
        let parent = self.path.parent().context("owned tree has no parent")?;
        let actual_parent = fs::symlink_metadata(parent)?;
        anyhow::ensure!(
            actual_parent.is_dir() && (actual_parent.dev(), actual_parent.ino()) == (held_parent.dev(), held_parent.ino()),
            "owned tree parent changed"
        );
        match fs::symlink_metadata(&self.path) {
            Ok(actual) => Ok(RootIdentity::Expected(actual.dev(), actual.ino())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RootIdentity::Absent),
            Err(error) => Err(error.into()),
        }
    }
    fn validate(&mut self) -> Result<()> {
        if matches!(self.stage, RemovalStage::Collect) {
            return Ok(());
        }
        let actual = self.observe()?;
        anyhow::ensure!(actual == self.identity || actual == RootIdentity::Absent, "owned tree location changed");
        if actual == RootIdentity::Absent {
            self.identity = RootIdentity::Absent;
        }
        Ok(())
    }
    fn after_unmount(&mut self, covered: PinnedMountpoint) -> Result<()> {
        // Authority comes from the covered directory pinned before unmount, never from
        // the first pathname observation after unmount (which may already be foreign).
        self.identity = RootIdentity::Mountpoint(covered);
        self.validate()
    }
    fn directory_removed(&mut self) {
        self.stage = RemovalStage::Collect;
    }
    fn remove_directory(&mut self) -> Result<()> {
        self.remove_directory_with(|_| Ok(()))
    }
    fn remove_directory_with(&mut self, mut before: impl FnMut(&RemovalStage) -> Result<()>) -> Result<()> {
        use rustix::fs::{renameat_with, AtFlags, RenameFlags};
        loop {
            before(&self.stage)?;
            match &self.stage {
                RemovalStage::Directory => {
                    let name = std::ffi::OsString::from(format!(".remove-{}", crate::ids::new_id()));
                    // A sibling stays on the same filesystem even for a plain cache inside
                    // an OverlayFS mount. Its private contents are accessed only via its FD.
                    rustix::fs::mkdirat(self.parent.as_ref(), &name, rustix::fs::Mode::from_raw_mode(0o700))?;
                    self.stage = RemovalStage::OpenCustody(PrivateLocation { parent: self.parent.clone(), name });
                }
                RemovalStage::OpenCustody(location) => {
                    let directory = File::from(rustix::fs::openat(
                        location.parent.as_ref(),
                        &location.name,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::NOFOLLOW
                            | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    )?);
                    let metadata = directory.metadata()?;
                    anyhow::ensure!(
                        metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & 0o777 == 0o700,
                        "unsafe removal custody directory"
                    );
                    self.stage = RemovalStage::EmptyCustody(std::sync::Arc::new(PrivateDirectory {
                        location: PrivateLocation { parent: location.parent.clone(), name: location.name.clone() },
                        directory,
                    }));
                }
                RemovalStage::EmptyCustody(custody) => {
                    let name = self.path.file_name().context("owned tree has no name")?;
                    match renameat_with(self.parent.as_ref(), name, &custody.directory, "tree", RenameFlags::NOREPLACE) {
                        Ok(()) => self.stage = RemovalStage::Candidate(custody.clone()),
                        Err(rustix::io::Errno::NOENT) => self.stage = RemovalStage::RemoveCustody(custody.clone()),
                        Err(error) => return Err(error.into()),
                    }
                }
                RemovalStage::Candidate(custody) => {
                    let metadata = rustix::fs::statat(&custody.directory, "tree", AtFlags::SYMLINK_NOFOLLOW)?;
                    let expected = RootIdentity::Expected(metadata.st_dev as u64, metadata.st_ino as u64);
                    self.stage = if expected == self.identity {
                        RemovalStage::Verified(custody.clone())
                    } else {
                        RemovalStage::RestoreForeign(custody.clone())
                    };
                }
                RemovalStage::Verified(custody) => {
                    let path = PathBuf::from(format!("/proc/self/fd/{}/tree", custody.directory.as_raw_fd()));
                    match fs::symlink_metadata(&path) {
                        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&path)?,
                        Ok(_) => fs::remove_file(&path)?,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    self.stage = RemovalStage::RemoveCustody(custody.clone());
                }
                RemovalStage::RestoreForeign(custody) => {
                    let name = self.path.file_name().context("owned tree has no name")?;
                    // Never overwrite a new occupant. Failed restoration keeps the foreign
                    // candidate in private custody and the ownership token can retry it.
                    renameat_with(&custody.directory, "tree", self.parent.as_ref(), name, RenameFlags::NOREPLACE)?;
                    self.stage = RemovalStage::RemoveForeignCustody(custody.clone());
                }
                RemovalStage::RemoveCustody(custody) => {
                    custody.remove_empty()?;
                    self.directory_removed();
                }
                RemovalStage::RemoveForeignCustody(custody) => {
                    custody.remove_empty()?;
                    self.stage = RemovalStage::Directory;
                    bail!("owned tree location changed; replacement restored");
                }
                RemovalStage::Collect => return Ok(()),
                RemovalStage::Trees => bail!("trees must be unmounted before removal custody"),
            }
        }
    }
    fn run(&mut self) -> Result<()> {
        let mut state = lock_at(self.home.clone())?;
        if matches!(self.stage, RemovalStage::Trees | RemovalStage::Directory) {
            valid_path(&self.path)?;
            self.validate()?;
        }
        if matches!(self.stage, RemovalStage::Trees) {
            let mut paths: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(&self.path)).cloned().collect();
            paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for path in paths {
                if mounted(&path)? {
                    if path == self.path {
                        let name = self.path.file_name().context("owned mountpoint has no name")?;
                        let covered = PinnedMountpoint::below(&self.parent, name)?;
                        unmount_for_removal(&path)?;
                        self.after_unmount(covered)?;
                    } else {
                        unmount_for_removal(&path)?;
                    }
                }
                state.registry.trees.remove(&path);
                state.save()?;
            }
            self.stage = if self.identity == RootIdentity::Absent { RemovalStage::Collect } else { RemovalStage::Directory };
        }
        self.remove_directory()?;
        state.collect()
    }
}
fn remove_missing(path: &Path, home: &Path) -> Result<()> {
    let mut state = lock_at(home.to_path_buf())?;
    valid_path(path)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => bail!("owned tree location changed"),
    }
    let previous = state.registry.trees.len();
    state.registry.trees.retain(|key, _| !key.starts_with(path));
    if state.registry.trees.len() != previous {
        state.save()?;
    }
    state.collect()
}
pub(super) fn native_file_identity(path: &Path) -> Result<Vec<u8>> {
    match sys::overlay_attribute(path, c"trusted.layr.origin", None) {
        Ok(v) => Ok(v),
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => {
            let m = fs::symlink_metadata(path)?;
            Ok(format!("{}:{}", m.dev(), m.ino()).into_bytes())
        }
        Err(e) => Err(e.into()),
    }
}

pub(super) fn native_clone_identity(path: &Path) -> Result<(String, Option<u64>)> {
    let i = native_info(path)?;
    if !i.readonly {
        bail!("clone source is writable");
    }
    Ok((i.received_uuid.unwrap_or(i.uuid), None))
}
pub(super) fn native_send_identity(path: &Path) -> Result<(String, u64)> {
    Ok((native_clone_identity(path)?.0, 1))
}
pub fn send_command(parent: Option<&Path>, child: &Path) -> Result<Command> {
    let mut c = Command::new(std::env::current_exe()?);
    c.arg("__apfs-send").arg(child);
    // With a parent the stream holds only the differences (`apfs_stream::send`). A parent that
    // is no sealed tree gives a complete stream.
    if let Some((p, id)) = parent.and_then(|p| native_clone_identity(p).ok().map(|(id, _)| (p, id))) {
        c.arg(p).arg(id);
    }
    Ok(c)
}

struct ScanEntry {
    meta: crate::fsutil::Meta,
    // Modification and change time (seconds, nanoseconds) and mode, read from the tree itself.
    // A user can restore the modification time, never the change time.
    stamp: (i64, i64, i64, i64, u32),
}
// The entry at `rel` as a tree walk lists it: nested trees and mounts are not listed.
fn visible_entry(root: &Path, root_dev: u64, nested: &HashSet<PathBuf>, rel: &str) -> Result<Option<ScanEntry>> {
    let full = root.join(rel);
    let m = match fs::symlink_metadata(&full) {
        Ok(m) => m,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if m.is_dir() && (m.dev() != root_dev || nested.contains(&full)) {
        return Ok(None);
    }
    Ok(Some(ScanEntry {
        meta: crate::fsutil::meta_of(&m),
        stamp: (m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec(), m.mode()),
    }))
}
// The layers above the ones both trees share, top first. `None` when either tree is not an
// OverlayFS mount of this store or the two share no layer.
struct CandidateLayers {
    frozen: bool,
    dirs: Vec<PathBuf>,
    nested: HashSet<PathBuf>,
}

fn own_layers(parent: &Path, child: &Path) -> Result<Option<CandidateLayers>> {
    let Ok(state) = lock(parent) else { return Ok(None) };
    let stack = |path: &Path| -> Option<Vec<(String, PathBuf)>> {
        let e = state.registry.trees.get(path).filter(|e| !e.plain && !e.hard_links)?;
        let mut layers = Vec::new();
        if !e.info.readonly {
            layers.push((format!("upper:{}", e.info.uuid), state.object(&e.info.uuid).join("upper")));
        }
        for id in std::iter::once(&e.lower).chain(e.bases.iter()) {
            layers.push((id.clone(), state.object(id).join("data")));
        }
        Some(layers)
    };
    let (Some(a), Some(b)) = (stack(parent), stack(child)) else {
        return Ok(None);
    };
    let (mut i, mut j) = (a.len(), b.len());
    while i > 0 && j > 0 && a[i - 1].0 == b[j - 1].0 {
        i -= 1;
        j -= 1;
    }
    if i == a.len() {
        return Ok(None);
    }
    Ok(Some(CandidateLayers {
        frozen: [parent, child].iter().all(|path| state.registry.trees.get(*path).is_some_and(|entry| entry.info.readonly)),
        dirs: a[..i].iter().chain(&b[..j]).map(|(_, dir)| dir.clone()).collect(),
        nested: state.registry.trees.keys().filter(|p| p.as_path() != parent && p.as_path() != child).cloned().collect(),
    }))
}
// Every path a layer holds (files, folders, whiteouts), and whether the layer hides
// the lower content of a folder (opaque or redirected).
fn layer_paths(dir: &Path, rel: &str, out: &mut BTreeMap<String, bool>) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let path = e.path();
        let name = e.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 layer entry"))?;
        let name = format!("{rel}{name}");
        let mut hides = false;
        if e.file_type()?.is_dir() {
            hides = sys::overlay_xattrs(&path)?.iter().any(|(n, _)| {
                n == b"trusted.overlay.opaque"
                    || n == b"trusted.overlay.redirect"
                    || n == b"user.overlay.opaque"
                    || n == b"user.overlay.redirect"
            });
            layer_paths(&path, &format!("{name}/"), out)?;
        }
        *out.entry(name).or_default() |= hides;
    }
    Ok(())
}
// Paths whose content can differ. A path that no unshared layer holds is the same file in
// both trees. A folder that one tree lacks or replaces is listed with everything below it.
fn changed_entries(
    parent: &Path,
    child: &Path,
    layers: &CandidateLayers,
) -> Result<BTreeMap<String, (Option<ScanEntry>, Option<ScanEntry>)>> {
    let mut held = BTreeMap::new();
    for layer in &layers.dirs {
        layer_paths(layer, "", &mut held)?;
    }
    let (pdev, cdev) = (fs::symlink_metadata(parent)?.dev(), fs::symlink_metadata(child)?.dev());
    let mut entries = BTreeMap::new();
    let mut inside: Option<String> = None;
    for (rel, hides) in &held {
        if inside.as_ref().is_some_and(|prefix| rel.starts_with(prefix.as_str())) {
            continue;
        }
        let (old, new) = (visible_entry(parent, pdev, &layers.nested, rel)?, visible_entry(child, cdev, &layers.nested, rel)?);
        if old.is_none() && new.is_none() {
            inside = Some(format!("{rel}/"));
            continue;
        }
        let is_dir = |e: &Option<ScanEntry>| e.as_ref().is_some_and(|e| e.meta.is_dir());
        let deep = (is_dir(&old) || is_dir(&new)) && !(is_dir(&old) && is_dir(&new) && !hides);
        let directories = [is_dir(&old), is_dir(&new)];
        entries.insert(rel.clone(), (old, new));
        if deep {
            inside = Some(format!("{rel}/"));
            // Only a removed, replaced or opaque directory can change lower-only children.
            // Enumerate those subtrees with the same registry snapshot, never one lock per directory.
            for (index, ((root, dev), directory)) in [(parent, pdev), (child, cdev)].into_iter().zip(directories).enumerate() {
                if !directory {
                    continue;
                }
                let mut stack = vec![rel.clone()];
                while let Some(dir) = stack.pop() {
                    for e in fs::read_dir(root.join(&dir))? {
                        let name = e?.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 subtree entry"))?;
                        let name = format!("{dir}/{name}");
                        if let Some(entry) = visible_entry(root, dev, &layers.nested, &name)? {
                            if entry.meta.is_dir() {
                                stack.push(name.clone());
                            }
                            let pair = entries.entry(name).or_default();
                            if index == 0 {
                                pair.0 = Some(entry);
                            } else {
                                pair.1 = Some(entry);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(entries)
}
fn candidates_in(
    parent: &Path,
    child: &Path,
    layers: &CandidateLayers,
    request: layr_fs_core::ChangeRequest,
) -> Result<Vec<crate::compare::EntryChange>> {
    let (parent, child) = (absolute(parent)?, absolute(child)?);
    let entries = changed_entries(&parent, &child, layers)?;
    let mut changes: BTreeMap<_, _> = entries
        .iter()
        .filter_map(|(p, (old, new))| {
            crate::compare::EntryChange::observed(
                p.clone(),
                old.as_ref().map(|entry| entry.meta.clone()),
                new.as_ref().map(|entry| entry.meta.clone()),
                crate::compare::DataEvidence::Unknown,
            )
            .map(|c| (p.clone(), c))
        })
        .collect();
    // The shared lower layers establish the identity domain for these candidates.
    let mut removed: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    let mut added: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    for (path, change) in changes.iter() {
        if change.new().is_none() {
            removed.entry(native_file_identity(&parent.join(path))?).or_default().push(path.clone());
        }
        if change.old().is_none() {
            added.entry(native_file_identity(&child.join(path))?).or_default().push(path.clone());
        }
    }
    for (identity, sources) in removed {
        let Some(targets) = added.get(&identity) else { continue };
        // An inode with multiple aliases does not identify a unique rename.
        if sources.len() == 1 && targets.len() == 1 {
            crate::compare::pair_rename(&mut changes, &sources[0], &targets[0], crate::compare::DataEvidence::Unknown);
        }
    }
    for (p, c) in &mut changes {
        if let (None, Some(old), Some(new)) = (c.old_path(), c.old(), c.new()) {
            if !old.is_file() || !new.is_file() {
                continue;
            }
            let (old_size, new_size) = (old.size, new.size);
            // Same size, mode, modification and change time (the check git calls racy): equal
            // without a read. Other files get their exact changed ranges, which a merge needs.
            let quick = entries
                .get(p)
                .and_then(|(old, new)| old.as_ref().zip(new.as_ref()))
                .is_some_and(|(x, y)| x.stamp == y.stamp && x.meta.size == y.meta.size);
            if !quick && request.detail == layr_fs_core::ChangeDetail::Equality {
                continue;
            }
            let ranges = if quick {
                Vec::new()
            } else {
                {
                    let (mut a, mut b) =
                        (crate::fsutil::open_regular(&parent.join(p))?, crate::fsutil::open_regular(&child.join(p))?);
                    layr_fs_common::changed_ranges(&mut a, old_size, &mut b, new_size)?
                }
            };
            c.set_data(crate::compare::DataEvidence::from_native(&ranges, old_size, new_size).map_err(anyhow::Error::msg)?);
        }
    }
    Ok(changes.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidates_reject_non_utf8_layer_entries_and_opaque_descendants() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let root = std::env::temp_dir().join(format!("layr-invalid-hints-{}", crate::ids::new_id()));
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff");
        for source in ["layer", "opaque"] {
            let (base, tree, upper) =
                (root.join(source).join("base"), root.join(source).join("tree"), root.join(source).join("upper"));
            for path in [&base, &tree, &upper] {
                fs::create_dir_all(path)?;
            }
            if source == "layer" {
                fs::write(upper.join(bad), b"change")?;
                fs::write(tree.join(bad), b"change")?;
            } else {
                for path in [&base, &tree, &upper] {
                    fs::create_dir(path.join("opaque"))?;
                }
                fs::write(base.join("opaque").join(bad), b"hidden lower")?;
                sys::overlay_attribute(&upper.join("opaque"), c"user.overlay.opaque", Some(b"y"))?;
            }
            let layers = CandidateLayers { frozen: true, dirs: vec![upper], nested: HashSet::new() };
            assert!(
                candidates_in(&base, &tree, &layers, Default::default()).is_err(),
                "{source}: incomplete enumeration cannot authorize Complete"
            );
            assert!(
                crate::changes::candidates(&base, &tree).is_err(),
                "{source}: full fallback also rejects the invalid visible path"
            );
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn candidates_only_read_unshared_entries() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-candidates-{}", crate::ids::new_id()));
        let (parent, child, upper) = (root.join("parent"), root.join("child"), root.join("upper"));
        for path in [&parent, &child, &upper] {
            fs::create_dir_all(path)?;
        }
        for tree in [&parent, &child] {
            fs::create_dir(tree.join("lower"))?;
            fs::write(tree.join("lower/untouched"), b"lower only")?;
            // If candidate enumeration enters unchanged lower content, the test fails with EACCES.
            fs::set_permissions(tree.join("lower"), fs::Permissions::from_mode(0o0))?;
            fs::write(tree.join("edit"), if tree == &parent { b"before".as_slice() } else { b"after!!".as_slice() })?;
        }
        fs::write(upper.join("edit"), b"after!!")?;
        let result = candidates_in(
            &parent,
            &child,
            &CandidateLayers { frozen: true, dirs: vec![upper], nested: HashSet::new() },
            Default::default(),
        );
        for tree in [&parent, &child] {
            fs::set_permissions(tree.join("lower"), fs::Permissions::from_mode(0o700))?;
        }
        let changes = result?;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "edit");
        assert_eq!(changes[0].data(), &crate::compare::DataEvidence::from_native(&[(0, 7)], 6, 7).unwrap());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn candidates_keep_whiteouts_opaque_replacements_and_nested_trees() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-candidates-{}", crate::ids::new_id()));
        let (parent, child, upper) = (root.join("parent"), root.join("child"), root.join("upper"));
        for path in [&parent, &child, &upper] {
            fs::create_dir_all(path)?;
        }
        for tree in [&parent, &child] {
            fs::create_dir(tree.join("opaque"))?;
            fs::create_dir(tree.join("nested"))?;
            fs::write(tree.join("nested/cache"), b"private")?;
        }
        fs::write(parent.join("opaque/hidden"), b"lower")?;
        fs::create_dir(parent.join("gone"))?;
        fs::write(parent.join("gone/file"), b"deleted")?;
        fs::create_dir(parent.join("replace"))?;
        fs::write(parent.join("replace/file"), b"replaced")?;
        fs::write(child.join("replace"), b"now a file")?;
        // A whiteout is held in the upper layer but absent from the visible child.
        fs::write(upper.join("gone"), b"")?;
        fs::write(upper.join("replace"), b"now a file")?;
        fs::create_dir(upper.join("opaque"))?;
        sys::overlay_attribute(&upper.join("opaque"), c"user.overlay.opaque", Some(b"y"))?;
        fs::create_dir(upper.join("nested"))?;
        fs::write(upper.join("nested/cache"), b"private")?;
        let layers = CandidateLayers {
            frozen: true,
            dirs: vec![upper],
            nested: [parent.join("nested"), child.join("nested")].into_iter().collect(),
        };
        let changes = candidates_in(&parent, &child, &layers, Default::default())?;
        let paths: Vec<_> = changes.iter().map(|c| c.path().as_str()).collect();
        assert_eq!(paths, ["gone", "gone/file", "opaque", "opaque/hidden", "replace", "replace/file"]);
        assert!(changes.iter().filter(|c| c.path().ends_with("/file") || c.path() == "opaque/hidden").all(|c| c.new().is_none()));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn relative_candidates_exclude_absolute_managed_roots() -> Result<()> {
        // Keep the relative fixture in this line's build folder; do not change the process cwd.
        let root = PathBuf::from(format!("target/layr-candidates-{}", crate::ids::new_id()));
        let (parent, child, upper) = (root.join("parent"), root.join("child"), root.join("upper"));
        for tree in [&parent, &child, &upper] {
            fs::create_dir_all(tree.join("nested"))?;
            fs::write(tree.join("nested/cache"), b"private")?;
        }
        let layers = CandidateLayers {
            frozen: true,
            dirs: vec![upper],
            nested: [absolute(&parent)?.join("nested"), absolute(&child)?.join("nested")].into_iter().collect(),
        };
        assert!(candidates_in(&parent, &child, &layers, Default::default())?.is_empty());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn candidates_keep_renames_and_ambiguous_hard_links() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-candidates-{}", crate::ids::new_id()));
        let (parent, child, upper) = (root.join("parent"), root.join("child"), root.join("upper"));
        for path in [&parent, &child, &upper] {
            fs::create_dir_all(path)?;
        }
        fs::write(parent.join("old"), b"renamed")?;
        fs::hard_link(parent.join("old"), child.join("new"))?;
        fs::write(parent.join("link-a"), b"linked")?;
        fs::hard_link(parent.join("link-a"), parent.join("link-b"))?;
        fs::hard_link(parent.join("link-a"), child.join("link-c"))?;
        fs::hard_link(parent.join("link-a"), child.join("link-d"))?;
        for path in ["old", "new", "link-a", "link-b", "link-c", "link-d"] {
            fs::write(upper.join(path), b"")?;
        }
        let changes = candidates_in(
            &parent,
            &child,
            &CandidateLayers { frozen: true, dirs: vec![upper], nested: HashSet::new() },
            Default::default(),
        )?;
        let renamed = changes.iter().find(|c| c.path() == "new").unwrap();
        assert_eq!(renamed.old_path().map(String::as_str), Some("old"));
        assert!(!changes.iter().any(|c| c.path() == "old"));
        assert_eq!(changes.iter().filter(|c| c.path().starts_with("link-")).count(), 4);
        assert!(changes.iter().filter(|c| c.path().starts_with("link-")).all(|c| c.old_path().is_none()));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn periodic_registry_reads_skip_contention_and_restore_the_wait_policy() -> Result<()> {
        let home = std::env::temp_dir().join(format!("layr-overlay-lock-{}", crate::ids::new_id()));
        fs::create_dir(&home)?;
        let lock = File::create(home.join("lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
        {
            let periodic = NonBlocking::new();
            assert!(!periodic.contended());
            {
                let nested = NonBlocking::new();
                // Optional cache discovery can catch this error; the tick still skips.
                assert!(lock_at(home.clone()).is_err());
                assert!(nested.contended());
            }
            assert!(periodic.contended());
            assert!(NONBLOCKING.get().is_some());
        }
        assert_eq!(NONBLOCKING.get(), None);
        drop(lock);
        {
            let periodic = NonBlocking::new();
            drop(lock_at(home.clone())?);
            assert!(!periodic.contended());
        }
        // Foreground callers keep the default policy after the periodic scope exits.
        drop(lock_at(home.clone())?);
        fs::remove_dir_all(home)?;
        Ok(())
    }

    #[test]
    fn boundary_snapshot_keeps_replaced_cache_paths_and_excludes_unregistered_siblings() -> Result<()> {
        let root = std::env::temp_dir().join(format!("layr-overlay-boundaries-{}", crate::ids::new_id()));
        let cache = root.join("target");
        let sibling = root.join("target-sibling");
        fs::create_dir_all(&cache)?;
        fs::create_dir(&sibling)?;
        let entry: Entry = serde_json::from_value(serde_json::json!({
            "info": {"uuid": crate::ids::new_id(), "readonly": false},
            "base": "unused", "plain": true
        }))?;
        let mut registry = Registry::default();
        registry.trees.insert(root.clone(), entry.clone());
        registry.trees.insert(cache.clone(), entry);
        let roots = NestedRoots(nested_trees(&registry, &root));
        drop(registry);
        // There is deliberately no registry on disk: repeated queries must use the
        // scan's snapshot, including after a registered directory has been replaced.
        fs::rename(&cache, root.join("removed-cache"))?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("replacement"), b"plain replacement cache")?;
        for _ in 0..100 {
            assert_eq!(roots.classify(&cache, &fs::symlink_metadata(&cache)?)?, crate::fsutil::StorageRoot::Managed);
            assert_eq!(roots.classify(&sibling, &fs::symlink_metadata(&sibling)?)?, crate::fsutil::StorageRoot::Unmanaged);
            assert_eq!(roots.classify(&root, &fs::symlink_metadata(&root)?)?, crate::fsutil::StorageRoot::Unmanaged);
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires root and an isolated test filesystem"]
    fn shared_store_is_rejected_before_registry_changes() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("shared-{}", crate::ids::new_id()));
        fs::create_dir(&root)?;
        sys::overlay_mount(Some(&root), &root, None, libc::MS_BIND, None)?;
        let result = (|| -> Result<()> {
            sys::overlay_mount(None, &root, None, libc::MS_SHARED, None)?;
            let error = recover(&root).unwrap_err();
            assert!(error.to_string().contains("private mount"));
            assert!(!root.join(".layr-overlayfs").exists());
            sys::overlay_mount(None, &root, None, libc::MS_PRIVATE, None)?;
            recover(&root)?;
            assert!(root.join(".layr-overlayfs/registry.json").is_file());
            Ok(())
        })();
        // Another test of this process can fork while recover() holds the registry lock open: the child keeps
        // a copy of that file descriptor until exec closes it, and the unmount gives EBUSY for that time.
        let mut tries = 0;
        while let Err(error) = sys::overlay_unmount(&root) {
            tries += 1;
            if error.raw_os_error() != Some(libc::EBUSY) || tries == 50 {
                return Err(error.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        fs::remove_dir_all(&root)?;
        result
    }
    #[test]
    fn private_store_mount_overrides_shared_parent() {
        let mounts = "1 0 0:1 / / rw shared:1 - ext4 /dev/root rw\n2 1 0:1 /store /store rw - ext4 /dev/root rw\n";
        assert!(shared_parent(Path::new("/other/store"), mounts));
        assert!(!shared_parent(Path::new("/store/project"), mounts));
        assert!(shared_parent(Path::new("/store-other"), mounts));
    }
    #[test]
    #[ignore = "requires root and an OverlayFS test filesystem"]
    fn cache_copy_stays_in_its_pinned_parent() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("cache-copy-{}", crate::ids::new_id()));
        fs::create_dir(&root)?;
        let src = root.join("source");
        fs::create_dir(&src)?;
        fs::write(src.join("file"), b"cache")?;
        fs::hard_link(src.join("file"), src.join("hard"))?;
        let parent = root.join("parent");
        let moved = root.join("moved");
        let outside = root.join("outside");
        fs::create_dir(&parent)?;
        fs::create_dir(&outside)?;
        let fd = pinned_directory(&parent)?;
        fs::rename(&parent, &moved)?;
        std::os::unix::fs::symlink(&outside, &parent)?;
        copy_cache_at(&src, &fd, std::ffi::OsStr::new("cache"), None, |_| {})?;
        assert_eq!(fs::read(moved.join("cache/file"))?, b"cache");
        assert_eq!(fs::metadata(moved.join("cache/file"))?.ino(), fs::metadata(moved.join("cache/hard"))?.ino());
        assert!(copy_cache(&src, &parent.join("escape"), |_| {}).is_err());
        assert_eq!(fs::read_dir(&outside)?.count(), 0);
        drop(fd);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn legacy_entries_read_but_new_entries_require_the_new_reader() -> Result<()> {
        let old = serde_json::json!({
            "info": {"uuid":"id", "received_uuid":null,"parent_uuid":null,"readonly":false},
            "lower":"base", "hard_links":false
        });
        let entry: Entry = serde_json::from_value(old)?;
        assert_eq!(entry.lower, "base");
        assert!(entry.bases.is_empty());
        assert!(!entry.plain);
        let new = serde_json::to_value(entry)?;
        assert!(new.get("lower").is_none());
        assert_eq!(new["base"], "base");
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn delta_snapshots_keep_open_handles_and_bound_layers() -> Result<()> {
        use std::io::Write;
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("delta-{}", crate::ids::new_id()));
        recover(&root)?;
        let initial = root.join("initial");
        crate::fs::create(&initial)?;
        fs::create_dir(initial.join("dir"))?;
        fs::write(initial.join("dir/remove"), b"remove")?;
        fs::write(initial.join("dir/keep"), b"keep")?;
        fs::write(initial.join("unchanged"), vec![7; 1024 * 1024])?;
        fs::write(initial.join("edit"), b"before")?;
        seal_tree(&initial)?;
        let work = root.join("work");
        crate::fs::snapshot(&initial, &work, false)?;
        let cache = work.join("cache");
        crate::fs::create(&cache)?;
        fs::write(cache.join("output"), b"private")?;
        fs::remove_dir_all(&cache)?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("output"), b"rebuilt")?;
        assert!(!mounted(&cache)?);
        // A cache replaced by a symlink must never expose its target as history.
        let outside = root.join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("secret"), b"private")?;
        fs::remove_dir_all(&cache)?;
        std::os::unix::fs::symlink(&outside, &cache)?;
        assert!(crate::fs::snapshot(&cache, &root.join("escape"), true).is_err());
        assert!(!root.join("escape").exists());
        fs::remove_file(&cache)?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("output"), b"rebuilt")?;
        let mut writer = fs::OpenOptions::new().write(true).truncate(true).open(work.join("edit"))?;
        writer.write_all(b"saved")?;
        fs::remove_file(work.join("dir/remove"))?;
        fs::rename(work.join("dir"), work.join("moved"))?;
        let saved = root.join("saved");
        crate::fs::snapshot(&work, &saved, true)?;
        assert_eq!(fs::read(saved.join("edit"))?, b"saved");
        assert!(!saved.join("dir").exists());
        assert!(!saved.join("moved/remove").exists());
        assert_eq!(fs::read(saved.join("moved/keep"))?, b"keep");
        assert!(!saved.join("cache/output").exists());
        {
            let s = lock(&saved)?;
            let e = &s.registry.trees[&saved];
            assert!(!e.bases.is_empty());
            assert!(!s.object(&e.lower).join("data/unchanged").exists());
        }
        writer.write_all(b" later")?;
        assert_eq!(fs::read(work.join("edit"))?, b"saved later");
        assert_eq!(fs::read(saved.join("edit"))?, b"saved");
        drop(writer);
        // Each branch adds a layer. The backend must compact before its limit.
        let mut previous = saved;
        for n in 0..20 {
            let branch = root.join(format!("branch-{n}"));
            crate::fs::snapshot(&previous, &branch, false)?;
            fs::write(branch.join("edit"), format!("version {n}"))?;
            let next = root.join(format!("state-{n}"));
            crate::fs::snapshot(&branch, &next, true)?;
            crate::fs::remove(&branch)?;
            crate::fs::remove(&previous)?;
            assert_eq!(fs::read(next.join("edit"))?, format!("version {n}").as_bytes());
            assert_eq!(fs::read(next.join("unchanged"))?.len(), 1024 * 1024);
            assert!(!next.join("dir/remove").exists());
            assert!(lock(&next)?.registry.trees[&next].bases.len() < 16);
            previous = next;
        }
        crate::fs::remove(&work)?;
        crate::fs::remove(&initial)?;
        crate::fs::remove(&previous)?;
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn taken_cache_preserves_root_mtime() -> Result<()> {
        let root =
            PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("cache-times-{}", crate::ids::new_id()));
        recover(&root)?;
        let source = root.join("source");
        crate::fs::create(&source)?;
        fs::write(source.join("output"), "built")?;
        sys::overlay_mtime(&source.join("output"), 10, 123)?;
        sys::overlay_mtime(&source, 10, 123)?;
        let saved = root.join("saved");
        crate::fs::snapshot(&source, &saved, true)?;
        let taken = root.join("taken");
        crate::fs::snapshot(&saved, &taken, false)?;
        let before = fs::metadata(&saved)?;
        let after = fs::metadata(&taken)?;
        assert_eq!((after.mtime(), after.mtime_nsec()), (before.mtime(), before.mtime_nsec()));
        assert!(!crate::repo::folder_changed_since(&taken, 20_000), "an unchanged cache keeps its inherited part");
        fs::remove_file(taken.join("output"))?;
        assert!(crate::repo::folder_changed_since(&taken, 20_000), "deletion still changes the cache");
        crate::fs::remove(&taken)?;
        crate::fs::remove(&saved)?;
        crate::fs::remove(&source)?;
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn delete_removes_a_tree_that_a_process_uses() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("busy-{}", crate::ids::new_id()));
        recover(&root)?;
        let base = root.join("base");
        crate::fs::create(&base)?;
        fs::write(base.join("file"), b"base")?;
        crate::fs::set_readonly(&base)?;
        // A tree that no process uses: release unmounts it, and the next open mounts it again
        // (a line delete whose record failed keeps its folder).
        let idle = root.join("idle");
        crate::fs::snapshot(&base, &idle, false)?;
        fs::write(idle.join("unsaved"), b"idle")?;
        release_tree(&idle)?;
        assert!(!mounted(&idle)?);
        recover(&root)?;
        assert_eq!(fs::read(idle.join("unsaved"))?, b"idle");
        // A shell with its working directory in the tree: the unmount gives EBUSY.
        let busy = root.join("busy");
        crate::fs::snapshot(&base, &busy, false)?;
        fs::create_dir(busy.join("dir"))?;
        let mut shell = std::process::Command::new("sleep").arg("60").current_dir(busy.join("dir")).spawn()?;
        let result = (|| -> Result<()> {
            assert!(sys::overlay_unmount(&busy).is_err_and(|e| is_busy(&e)));
            release_tree(&busy)?;
            assert!(mounted(&busy)?, "release keeps a busy mount and its registry entry");
            crate::fs::remove(&busy)?;
            assert!(!mounted(&busy)?);
            assert!(!busy.exists());
            assert!(!lock(&base)?.registry.trees.contains_key(&busy));
            assert_eq!(fs::read(base.join("file"))?, b"base");
            assert_eq!(fs::read(idle.join("file"))?, b"base");
            Ok(())
        })();
        let _ = shell.kill();
        let _ = shell.wait();
        result?;
        crate::fs::remove(&idle)?;
        crate::fs::remove(&base)?;
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn old_cache_mount_migrates_without_losing_data() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("migration-{}", crate::ids::new_id()));
        recover(&root)?;
        let work = root.join("work");
        crate::fs::create(&work)?;
        let cache = work.join("cache");
        crate::fs::create(&cache)?;
        // Reproduce the previous registry format and mounted cache.
        {
            let mut state = lock(&cache)?;
            let mut e = state.registry.trees[&cache].clone();
            e.plain = false;
            state.mount(&cache, &e)?;
            state.registry.trees.insert(cache.clone(), e);
            state.save()?;
        }
        fs::write(cache.join("output"), b"old cache")?;
        let open_dir = File::open(&cache)?;
        assert!(recover(&root).is_err());
        assert_eq!(fs::read(cache.join("output"))?, b"old cache");
        drop(open_dir);
        recover(&root)?;
        assert!(!mounted(&cache)?);
        assert_eq!(fs::read(cache.join("output"))?, b"old cache");
        fs::write(cache.join("output"), b"new cache")?;
        recover(&root)?;
        assert_eq!(fs::read(cache.join("output"))?, b"new cache");
        fs::remove_dir_all(&cache)?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("rebuilt"), b"rebuilt")?;
        let saved = root.join("saved");
        crate::fs::snapshot(&work, &saved, true)?;
        assert!(!saved.join("cache/rebuilt").exists());
        crate::fs::remove(&saved)?;
        crate::fs::remove(&work)?;
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn snapshots_preserve_links_and_isolate_writes() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("unit-{}", crate::ids::new_id()));
        recover(&root)?;
        let source = root.join("source");
        crate::fs::create(&source)?;
        fs::write(source.join("file"), b"base")?;
        fs::hard_link(source.join("file"), source.join("hard"))?;
        let sparse = File::create(source.join("sparse"))?;
        sparse.set_len(2 * 1024 * 1024 * 1024)?;
        std::os::unix::fs::FileExt::write_all_at(&sparse, b"tail", 2 * 1024 * 1024 * 1024 - 4)?;
        drop(sparse);
        let cache = source.join("cache");
        crate::fs::create(&cache)?;
        fs::write(cache.join("generated"), b"cache data")?;
        assert!(crate::fsutil::is_layer(&source, "cache", &crate::fsutil::meta_of(&fs::metadata(&cache)?)));
        let state = root.join("state");
        crate::fs::snapshot(&source, &state, true)?;
        assert!(state.join("cache").is_dir());
        assert!(!state.join("cache/generated").exists());
        let sparse = File::open(state.join("sparse"))?;
        assert_eq!(sparse.metadata()?.len(), 2 * 1024 * 1024 * 1024);
        assert!(sparse.metadata()?.blocks() < 1024);
        let mut tail = [0; 4];
        std::os::unix::fs::FileExt::read_exact_at(&sparse, &mut tail, 2 * 1024 * 1024 * 1024 - 4)?;
        assert_eq!(&tail, b"tail");
        drop(sparse);
        assert!(!crate::fsutil::is_layer(&state, "cache", &crate::fsutil::meta_of(&fs::metadata(state.join("cache"))?)));
        assert_eq!(fs::metadata(state.join("file"))?.ino(), fs::metadata(state.join("hard"))?.ino());
        assert!(fs::write(state.join("file"), b"bad").is_err());
        let branch = root.join("branch");
        crate::fs::snapshot(&state, &branch, false)?;
        fs::write(branch.join("file"), b"changed")?;
        assert_eq!(fs::read(branch.join("hard"))?, b"changed");
        assert_eq!(fs::read(state.join("file"))?, b"base");
        crate::fs::remove(&state)?;
        assert_eq!(fs::read(branch.join("hard"))?, b"changed");
        seal_tree(&branch)?;
        assert_eq!(fs::read(branch.join("hard"))?, b"changed");
        {
            let store = lock(&branch)?;
            let entry = &store.registry.trees[&branch];
            assert!(!store.object(&entry.info.uuid).join("upper").exists());
        }
        crate::fs::remove(&branch)?;
        assert!(crate::repo::uncache(&source, "cache", 0, 0)?);
        assert!(!managed_root(&cache));
        assert_eq!(fs::read(cache.join("generated"))?, b"cache data");
        assert!(crate::repo::convert_cache(&source, "cache", 0, 0)?);
        assert!(managed_root(&cache));
        assert_eq!(fs::read(cache.join("generated"))?, b"cache data");
        fs::create_dir(source.join("group"))?;
        rename_tree(&cache, &source.join("group/cache"))?;
        rename_tree(&source.join("group"), &source.join("moved"))?;
        assert!(managed_root(&source.join("moved/cache")));
        assert_eq!(fs::read(source.join("moved/cache/generated"))?, b"cache data");
        crate::fs::remove(&source)?;
        Ok(())
    }
}

fn tree_has_links(path: &Path) -> Result<bool> {
    let m = fs::symlink_metadata(path)?;
    if m.is_file() {
        return Ok(m.nlink() > 1);
    }
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            if tree_has_links(&e?.path())? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
pub fn contains_managed_tree(path: &Path) -> bool {
    let Ok(path) = absolute(path) else {
        return false;
    };
    lock(&path).is_ok_and(|s| s.registry.trees.keys().any(|p| p.starts_with(&path)))
}

/// The backing registry of a managed overlay mount, for physical extent keys.
fn allocation_domain(path: &Path) -> io::Result<crate::fs::accounting::AllocationDomain> {
    use crate::fs::accounting::AllocationDomain;
    use std::os::fd::AsFd;
    let backing = absolute(path).ok().and_then(|path| {
        let state = lock(&path).ok()?;
        state.registry.trees.get(&path).filter(|entry| !entry.plain).map(|_| state.home.clone())
    });
    let path = backing.as_deref().unwrap_or(path);
    if let Ok(file) = File::open(path) {
        return Ok(crate::sys::filesystem_uuid(file.as_fd())
            .map(AllocationDomain::native)
            .unwrap_or(AllocationDomain::device(file.metadata()?.dev())));
    }
    Ok(AllocationDomain::device(path.metadata()?.dev()))
}
pub(crate) fn accounting_part(path: &Path) -> Option<String> {
    (path.file_name() == Some(std::ffi::OsStr::new(".layr-overlayfs"))).then(|| "overlayfs layers".to_owned())
}
fn allocation_units(path: &Path) -> Result<Vec<PathBuf>> {
    if accounting_part(path).is_some() {
        let mut units = Vec::new();
        for entry in fs::read_dir(path.join("objects"))? {
            let data = entry?.path().join("data");
            if data.is_dir() {
                units.push(data);
            }
        }
        Ok(units)
    } else {
        crate::fs::accounting::directory_units(path)
    }
}

/// Disk measure_space of a folder (`layr du`). A mounted tree reads its files from shared lower layers
/// and its own upper layer: only the upper layer is its own. A plain folder is its own files.
pub(super) fn measure_space(path: &Path) -> Result<crate::fsutil::Space> {
    let abs = absolute(path)?;
    let upper = {
        let state = lock(&abs)?;
        state.registry.trees.get(&abs).filter(|e| !e.plain && !e.info.readonly).map(|e| state.object(&e.info.uuid).join("upper"))
    };
    let total = crate::fsutil::fiemap_space(path)?;
    match upper {
        Some(upper) if upper.is_dir() => {
            Ok(crate::fsutil::Space { total: total.total, unique: crate::fsutil::fiemap_space(&upper)?.unique })
        }
        _ => Ok(total),
    }
}

/// Visit physical entries for allocation attribution.
fn visit_allocation(path: &Path, visit: &mut dyn FnMut(&crate::fsutil::Held)) -> Result<()> {
    crate::fsutil::fiemap_visit(path, visit)
}

/// Bytes that a copy of `path` occupies on another file system: each distinct extent once. The
/// layer folders of the project's trees count too: a mounted view shows them, but a file that
/// an upper layer hides is only in its layer.
pub(super) fn copy_allocation(path: &Path) -> Result<u64> {
    let src = absolute(path)?;
    let mut keys: HashMap<crate::fsutil::ExtentKey, u64> = Default::default();
    crate::fsutil::fiemap_visit(path, &mut |h| {
        for &(key, size) in h.shared {
            if !matches!(key, crate::fsutil::ExtentKey::Unaddressed { .. }) {
                keys.entry(key).or_insert(size);
            }
        }
    })?;
    let state = lock(&src)?;
    for e in state.registry.trees.iter().filter(|(p, _)| p.starts_with(&src)).map(|(_, e)| e) {
        for id in entry_objects(e) {
            let folder = state.home.join("objects").join(&id);
            if folder.is_dir() {
                crate::fsutil::fiemap_visit(&folder, &mut |h| {
                    for &(key, size) in h.shared {
                        if !matches!(key, crate::fsutil::ExtentKey::Unaddressed { .. }) {
                            keys.entry(key).or_insert(size);
                        }
                    }
                })?;
            }
        }
    }
    Ok(keys.values().sum())
}

/// Allocated bytes of `path` with shared data counted per file.
pub(super) fn copy_flat_allocation(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    crate::fsutil::fiemap_visit(path, &mut |h| total += h.alloc)?;
    Ok(total)
}

/// The layer folders of a tree: its lower layers and its upper layer.
fn entry_objects(e: &Entry) -> Vec<String> {
    let mut ids = vec![e.lower.clone(), e.info.uuid.clone()];
    ids.extend(e.bases.iter().cloned());
    ids.retain(|id| !id.is_empty());
    ids.sort();
    ids.dedup();
    ids
}

/// The registry key of a tree in a copy: the same path below the copy root.
fn copied_key(key: &Path, src: &Path, dst: &Path) -> Result<PathBuf> {
    Ok(dst.join(key.strip_prefix(src).context("registry entry outside the project")?))
}

/// The registry of the copy is the nearest registry above `dst`. A target without one gets a
/// registry at its mount point (the copy's own store root for OverlayFS).
fn target_home(dst: &Path) -> Result<PathBuf> {
    for p in dst.ancestors() {
        let home = p.join(".layr-overlayfs");
        if home.is_dir() {
            return Ok(home);
        }
    }
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let root = mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(PathBuf::from)
        .filter(|m| dst.starts_with(m))
        .max_by_key(|m| m.components().count())
        .context("no mount point for the copy target")?;
    let home = root.join(".layr-overlayfs");
    private_dir(&home)?;
    private_dir(&home.join("objects"))?;
    if !home.join("registry.json").exists() {
        crate::fsutil::atomic_write(&home.join("registry.json"), &serde_json::to_vec(&Registry::default())?, 0o600)?;
    }
    Ok(home)
}

/// Owner, mode, extended attributes and time of a copied entry (the time last, after the entries
/// of a folder). Overlay attributes (`trusted.overlay.*`) are copied too: opaque and redirect
/// define the layer. The origin handle is left out (see below).
fn set_meta(src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
    set_owner(dst, m)?;
    if m.file_type().is_symlink() {
        return Ok(());
    }
    fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    for (name, value) in sys::overlay_xattrs(src)? {
        // The origin is a file handle of the source file system: stale on the copy, and the
        // mount rebuilds it (index=on) instead of reading it.
        if name == b"trusted.overlay.origin" {
            continue;
        }
        sys::overlay_attribute(dst, &std::ffi::CString::new(name)?, Some(&value))?;
    }
    sys::overlay_mtime(dst, m.mtime(), m.mtime_nsec())?;
    Ok(())
}

fn set_owner(dst: &Path, m: &fs::Metadata) -> Result<()> {
    rustix::fs::chownat(
        rustix::fs::CWD,
        dst,
        Some(rustix::fs::Uid::from_raw(m.uid())),
        Some(rustix::fs::Gid::from_raw(m.gid())),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .with_context(|| format!("owner of {}", dst.display()))?;
    Ok(())
}

/// Regular files of a copy. A file with the same extents as an earlier copy shares that copy
/// when the target file system can reflink (XFS, btrfs); other file systems copy the data.
#[derive(Default)]
struct Files {
    seen: HashMap<Vec<(u64, u64, u64)>, PathBuf>,
    links: HashMap<(u64, u64), PathBuf>,
}

impl Files {
    /// A layer folder was renamed: the copies inside it now have new paths.
    fn moved(&mut self, from: &Path, to: &Path) {
        let relocate = |p: &mut PathBuf| {
            if let Ok(rest) = p.strip_prefix(from) {
                *p = to.join(rest);
            }
        };
        self.seen.values_mut().for_each(relocate);
        self.links.values_mut().for_each(relocate);
    }

    fn copy(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        if m.nlink() > 1 {
            if let Some(first) = self.links.get(&(m.dev(), m.ino())) {
                fs::hard_link(first, dst).with_context(|| format!("link {}", dst.display()))?;
                return Ok(());
            }
        }
        let from = File::open(src).with_context(|| format!("open {}", src.display()))?;
        let key = plain_extents(&from);
        let shared = key.as_ref().and_then(|k| self.seen.get(k).cloned());
        let to =
            fs::OpenOptions::new().write(true).create_new(true).open(dst).with_context(|| format!("create {}", dst.display()))?;
        match &shared {
            Some(first) => {
                layr_fs_unix::copy_contents(&File::open(first)?, &to).with_context(|| format!("share {}", dst.display()))?
            }
            None => layr_fs_unix::copy_contents(&from, &to).with_context(|| format!("copy {}", src.display()))?,
        }
        if let (None, Some(k)) = (shared, key) {
            self.seen.insert(k, dst.to_path_buf());
        }
        if m.nlink() > 1 {
            self.links.insert((m.dev(), m.ino()), dst.to_path_buf());
        }
        Ok(())
    }
}

/// A project copy on OverlayFS (`src/project_copy.rs`). The mounted trees are not walked: their
/// layer folders (lower and upper) are copied once each, and the registry of the copy names
/// them. The copy is mounted at the end, so it is readable like the source.
pub(crate) struct Copier {
    src_root: PathBuf,
    dst_root: PathBuf,
    src_home: PathBuf,
    dst_home: PathBuf,
    /// The registry entries of the source project.
    entries: BTreeMap<PathBuf, Entry>,
    /// Source trees that are mounts (not plain folders).
    mounted: HashSet<PathBuf>,
    /// Source trees whose copy is registered (or copied as a plain folder).
    done: HashSet<PathBuf>,
    /// Source trees copied as plain folders (their registry entry is plain in the copy).
    plain_copies: HashSet<PathBuf>,
    /// Layer folders already in the copy.
    objects: HashSet<String>,
    /// The registry entries of the copy.
    registered: BTreeMap<PathBuf, Entry>,
    files: Files,
}

impl Copier {
    pub(crate) fn new(src_root: &Path, dst_root: &Path) -> Result<Copier> {
        let src = absolute(src_root)?;
        let dst = absolute(dst_root)?;
        let state = lock(&src)?;
        let entries: BTreeMap<PathBuf, Entry> =
            state.registry.trees.iter().filter(|(p, _)| p.starts_with(&src)).map(|(p, e)| (p.clone(), e.clone())).collect();
        let src_home = state.home.clone();
        drop(state);
        let dst_home = target_home(&dst)?;
        let mounted = entries.iter().filter(|(_, e)| !e.plain).map(|(p, _)| p.clone()).collect();
        Ok(Copier {
            src_root: src,
            dst_root: dst,
            src_home,
            dst_home,
            entries,
            mounted,
            done: HashSet::new(),
            plain_copies: HashSet::new(),
            objects: HashSet::new(),
            registered: BTreeMap::new(),
            files: Files::default(),
        })
    }

    /// A mounted tree is copied from its layers, not from its view.
    pub(crate) fn native_subvolume(&self, path: &Path) -> bool {
        self.mounted.contains(path) && !self.plain_copies.contains(path)
    }

    /// The folder of a mounted tree that is copied as a plain folder (the database).
    pub(crate) fn shell_dir(&mut self, src: &Path, dst: &Path) -> Result<()> {
        if self.mounted.contains(src) {
            self.plain_copies.insert(src.to_path_buf());
        }
        fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))
    }

    pub(crate) fn dir_close(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        set_meta(src, dst, m)
    }

    pub(crate) fn file(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        self.files.copy(src, dst, m)?;
        set_meta(src, dst, m)
    }

    pub(crate) fn symlink(&mut self, src: &Path, dst: &Path, m: &fs::Metadata) -> Result<()> {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst).with_context(|| format!("link {}", dst.display()))?;
        crate::fsutil::copy_symlink_xattrs(src, dst)?;
        set_owner(dst, m)
    }

    /// The mount point of a tree: its layers are copied and the tree is registered.
    pub(crate) fn subvolume(&mut self, src: &Path, dst: &Path) -> Result<()> {
        self.register(src)?;
        fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))
    }

    /// Register one source tree in the copy. Its layers are copied first.
    fn register(&mut self, key: &Path) -> Result<()> {
        if !self.done.insert(key.to_path_buf()) {
            return Ok(());
        }
        let entry = self.entries.get(key).context("unknown registry entry")?.clone();
        if !entry.plain {
            for id in entry_objects(&entry) {
                if self.src_home.join("objects").join(&id).is_dir() {
                    self.copy_object(&id)?;
                } else if id == entry.lower {
                    bail!("layer {id} of {} is missing in the source", key.display());
                }
            }
        }
        let dst_key = copied_key(key, &self.src_root, &self.dst_root)?;
        let entry = if self.plain_copies.contains(key) { Entry { plain: true, ..entry } } else { entry };
        self.registered.insert(dst_key, entry);
        Ok(())
    }

    /// Copy one layer folder into the copy, once. It is complete under its own name, or absent.
    fn copy_object(&mut self, id: &str) -> Result<()> {
        if self.objects.contains(id) {
            return Ok(());
        }
        let objects = self.dst_home.join("objects");
        let target = objects.join(id);
        if target.is_dir() {
            // Renamed into place only when complete: a layer of this copy or of an earlier one.
            self.objects.insert(id.to_string());
            return Ok(());
        }
        let source = self.src_home.join("objects").join(id);
        let temp = objects.join(format!("{id}.tmp"));
        if temp.exists() {
            fs::remove_dir_all(&temp)?;
        }
        fs::DirBuilder::new().mode(0o700).create(&temp)?;
        for child in fs::read_dir(&source)? {
            let child = child?;
            let name = child.file_name().to_string_lossy().into_owned();
            if name == "work" || name == "upper-pending" || name.ends_with(".tmp") {
                continue;
            }
            self.copy_node(&child.path(), &temp.join(child.file_name()))?;
        }
        set_meta(&source, &temp, &fs::symlink_metadata(&source)?)?;
        fs::rename(&temp, &target).with_context(|| format!("publish layer {id}"))?;
        self.files.moved(&temp, &target);
        self.objects.insert(id.to_string());
        Ok(())
    }

    /// Copy one entry of a layer folder, with its content (directories recursively).
    fn copy_node(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let m = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
        let kind = m.file_type();
        if kind.is_dir() {
            fs::create_dir(dst).with_context(|| format!("create {}", dst.display()))?;
            for child in fs::read_dir(src)? {
                let child = child?;
                self.copy_node(&child.path(), &dst.join(child.file_name()))?;
            }
            set_meta(src, dst, &m)
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(src)?, dst).with_context(|| format!("link {}", dst.display()))?;
            crate::fsutil::copy_symlink_xattrs(src, dst)?;
            set_owner(dst, &m)
        } else if kind.is_file() {
            self.files.copy(src, dst, &m)?;
            set_meta(src, dst, &m)
        } else if std::os::unix::fs::FileTypeExt::is_char_device(&kind) && m.rdev() == 0 {
            // An overlay whiteout: a character device with device number 0.
            rustix::fs::mknodat(rustix::fs::CWD, dst, rustix::fs::FileType::CharacterDevice, rustix::fs::Mode::empty(), 0)
                .with_context(|| format!("whiteout {}", dst.display()))?;
            set_meta(src, dst, &m)
        } else {
            bail!("cannot copy special file {}", src.display())
        }
    }

    /// Register the trees that the walk did not reach (nested in a mount), write the registry of
    /// the copy and mount its trees.
    pub(crate) fn finish(mut self) -> Result<()> {
        let rest: Vec<PathBuf> = self.entries.keys().filter(|k| !self.done.contains(*k)).cloned().collect();
        for key in rest {
            self.register(&key)?;
        }
        {
            let mut state = lock_at(self.dst_home.clone())?;
            for (key, entry) in std::mem::take(&mut self.registered) {
                state.registry.trees.insert(key, entry);
            }
            state.save()?;
        }
        let root = self.dst_home.parent().context("registry without a folder")?.to_path_buf();
        recover(&root)
    }
}

/// The registry entries of a copy match the source, with their paths below the copy root.
/// A database folder is a plain folder in the copy (its content is a fresh copy of the database).
pub(crate) fn verify_registry(src: &Path, dst: &Path) -> Result<()> {
    let src = absolute(src)?;
    let dst = absolute(dst)?;
    // One lock per registry folder: a copy on a volume that the store registry manages shares the
    // source's lock, and a second flock on the same file from this process would wait forever.
    let (src_home, dst_home) = (home_of(&src)?, home_of(&dst)?);
    let s = lock_at(src_home.clone())?;
    let other = if dst_home == src_home { None } else { Some(lock_at(dst_home)?) };
    let dst_trees = match &other {
        Some(d) => &d.registry.trees,
        None => &s.registry.trees,
    };
    let mut expected: BTreeMap<PathBuf, serde_json::Value> = BTreeMap::new();
    for (key, entry) in s.registry.trees.iter().filter(|(p, _)| p.starts_with(&src)) {
        let mut entry = entry.clone();
        if key.ends_with("meta/db") {
            entry.plain = true;
        }
        expected.insert(copied_key(key, &src, &dst)?, serde_json::to_value(&entry)?);
    }
    let actual: BTreeMap<PathBuf, serde_json::Value> = dst_trees
        .iter()
        .filter(|(p, _)| p.starts_with(&dst))
        .map(|(p, e)| -> Result<(PathBuf, serde_json::Value)> { Ok((p.clone(), serde_json::to_value(e)?)) })
        .collect::<Result<_>>()?;
    if expected.keys().ne(actual.keys()) {
        bail!("the copy has {} registry entries, the source {}", actual.len(), expected.len());
    }
    for (key, value) in &expected {
        if actual.get(key) != Some(value) {
            bail!("registry entry {} differs", key.display());
        }
    }
    Ok(())
}

use crate::linux_status::status_stage_identity as index_identity;

/// Resolve only ordinary, unredirected layers. An ambiguous backing file cannot
/// validate an overlay mapping, so it keeps the snapshot fallback.
fn status_layers(root: &Path) -> Result<Option<Vec<PathBuf>>> {
    if rustix::fs::statfs(root)?.f_type as u64 != 0x794c_7630 {
        return Ok(None);
    }
    let state = lock(root)?;
    let root = absolute(root)?;
    let e = state.registry.trees.get(&root).context("unregistered overlay status root")?;
    let mut layers = Vec::new();
    if !e.info.readonly {
        layers.push(state.object(&e.info.uuid).join("upper"));
    }
    layers.extend(std::iter::once(&e.lower).chain(&e.bases).map(|id| state.object(id).join("data")));
    Ok(Some(layers))
}

fn status_scan_with(
    root: &Path,
    paths: Option<&std::collections::BTreeSet<String>>,
) -> Result<BTreeMap<String, crate::status_cache::FileFingerprint>> {
    use crate::status_cache::{FileFingerprint, FileStat, ObservationEvidence};
    let layers = status_layers(root)?;
    let mut safe_dirs = HashMap::<PathBuf, bool>::new();
    crate::linux_status::scan(root, paths, &mut |rel, observed| {
        let Some(layers) = layers.as_ref().filter(|_| observed.is_file()) else {
            return crate::linux_status::native_fingerprint(&root.join(rel), observed);
        };
        for layer in layers {
            // Redirects/opaque directories change how lower paths are resolved. Do not
            // guess a backing file from its name in those cases. Cache each directory check.
            for parent in Path::new(rel).ancestors().skip(1) {
                let dir = layer.join(parent);
                let safe = *safe_dirs.entry(dir.clone()).or_insert_with(|| match fs::symlink_metadata(&dir) {
                    Ok(m) if m.is_dir() => sys::overlay_xattrs(&dir).is_ok_and(|attrs| {
                        !attrs.iter().any(|(n, _)| {
                            matches!(
                                n.as_slice(),
                                b"trusted.overlay.redirect" | b"trusted.overlay.opaque" | b"trusted.overlay.metacopy"
                            )
                        })
                    }),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => true,
                    _ => false,
                });
                if !safe {
                    return FileFingerprint::Unverifiable;
                }
            }
            let path = layer.join(rel);
            let backing = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return FileFingerprint::Unverifiable,
            };
            // With xino, or an inode identity retained across copy-up, equality may not
            // be provable here. Falling back is safe. Never lease the merged inode.
            let mut stat = FileStat::of(&backing);
            let merged = FileStat::of(observed);
            stat.dev = merged.dev;
            stat.nlink = merged.nlink;
            if !backing.is_file() || stat != merged {
                return FileFingerprint::Unverifiable;
            }
            match crate::linux_status::native_fingerprint(&path, &backing) {
                FileFingerprint::Verified { stat, .. }
                    if fs::symlink_metadata(root.join(rel)).is_ok_and(|m| FileStat::of(&m) == merged) =>
                {
                    return FileFingerprint::Verified { stat: merged, evidence: Some(ObservationEvidence::Backing(stat)) };
                }
                _ => return FileFingerprint::Unverifiable,
            }
        }
        FileFingerprint::Unverifiable
    })
}

pub(super) fn observe_tree(root: &Path) -> Result<BTreeMap<String, crate::status_cache::FileFingerprint>> {
    status_scan_with(root, None)
}

pub(super) fn observe_paths(
    root: &Path,
    paths: &std::collections::BTreeSet<String>,
) -> Result<BTreeMap<String, crate::status_cache::FileFingerprint>> {
    if paths.is_empty() {
        return Ok(Default::default());
    }
    status_scan_with(root, Some(paths))
}

use crate::fs::{Accounting, Namespace, Observations, ProjectCopier, Transfer};
pub(crate) struct HostServices;
impl Namespace for HostServices {
    fn managed_or_present(&self, _path: &Path) -> Result<bool> {
        Ok(true)
    }
    fn supported(&self, path: &Path) -> bool {
        supports_volume(path)
    }
    fn managed(&self, path: &Path) -> bool {
        managed_root(path)
    }
    fn info(&self, path: &Path) -> Result<crate::fs::TreeInfo> {
        let info = native_info(path)?;
        Ok(crate::fs::TreeInfo {
            uuid: info.uuid,
            received_uuid: info.received_uuid,
            parent_uuid: info.parent_uuid,
            readonly: info.readonly,
        })
    }
    #[cfg(test)]
    fn protect(&self, path: crate::fs::ManagedPath, protection: crate::fs::Protection) -> Result<()> {
        match protection {
            crate::fs::Protection::Frozen => seal_tree(path.path()),
            crate::fs::Protection::Writable => unseal_tree(path.path()),
        }
    }
    fn relocate(&self, path: crate::fs::ManagedPath, to: &Path) -> Result<()> {
        rename_tree(path.path(), to)
    }
    fn file_identity(&self, path: &Path) -> Result<Vec<u8>> {
        native_file_identity(path)
    }
}
impl Transfer for HostServices {
    fn send(&self, parent: Option<&Path>, child: &Path, _clones: &[PathBuf], _compressed: bool) -> Result<Command> {
        send_command(parent, child)
    }
    fn offer(&self) -> crate::fs::StreamOffer {
        crate::fs::StreamOffer { compressed: false, clone_lookup: crate::fs::CloneLookup::LocalOrReceived }
    }
    fn receive(
        &self,
        source: &mut dyn std::io::Read,
        destination: &Path,
        expected: &crate::receive_stream::Expect,
    ) -> Result<()> {
        crate::apfs_stream::receive(source, destination, expected)
    }
    fn clone_identity(&self, path: &Path) -> Result<(String, Option<u64>)> {
        native_clone_identity(path)
    }
    fn send_identity(&self, path: &Path) -> Result<(String, u64)> {
        native_send_identity(path)
    }
}
impl Accounting for HostServices {
    fn line_spaces(&self, paths: &[PathBuf]) -> Result<Vec<Result<crate::fsutil::Space>>> {
        crate::fs::accounting::parallel_spaces(paths, measure_space)
    }
    fn allocations(
        &self,
        _root: &Path,
        parts: &[crate::fs::accounting::AllocationPart],
    ) -> Result<crate::fs::accounting::AllocationReport> {
        crate::fs::accounting::directory_allocations(parts, allocation_domain, allocation_units, visit_allocation)
    }
    fn space(&self, path: &Path) -> Result<crate::fsutil::Space> {
        measure_space(path)
    }
    fn copy_need(&self, path: &Path) -> Result<u64> {
        copy_allocation(path)
    }
    fn copy_total(&self, path: &Path) -> Result<u64> {
        copy_flat_allocation(path)
    }
}
impl Observations for HostServices {
    fn read_policy(&self) -> crate::fs::observations::ReadPolicy {
        crate::fs::observations::ReadPolicy::Capture
    }
    fn scan(
        &self,
        path: &Path,
        paths: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>> {
        match paths {
            Some(paths) => observe_paths(path, paths),
            None => observe_tree(path),
        }
    }
    fn stage_identity(&self, path: &Path) -> Result<Vec<u8>> {
        index_identity(path)
    }
}
impl HostServices {
    pub(crate) fn copier(src: &Path, dst: &Path) -> Result<Box<dyn ProjectCopier>> {
        Ok(Box::new(Copier::new(src, dst)?))
    }
}
impl ProjectCopier for Copier {
    fn native_subvolume(&self, path: &Path) -> bool {
        Copier::native_subvolume(self, path)
    }
    fn shell_dir(&mut self, src: &Path, dst: &Path) -> Result<()> {
        Copier::shell_dir(self, src, dst)
    }
    fn dir_close(&mut self, src: &Path, dst: &Path, metadata: &fs::Metadata) -> Result<()> {
        Copier::dir_close(self, src, dst, metadata)
    }
    fn file(&mut self, src: &Path, dst: &Path, metadata: &fs::Metadata) -> Result<()> {
        Copier::file(self, src, dst, metadata)
    }
    fn symlink(&mut self, src: &Path, dst: &Path, metadata: &fs::Metadata) -> Result<()> {
        Copier::symlink(self, src, dst, metadata)
    }
    fn subvolume(&mut self, src: &Path, dst: &Path) -> Result<()> {
        Copier::subvolume(self, src, dst)
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Copier::finish(*self)
    }
}

pub(crate) fn plain_extents(file: &File) -> Option<Vec<(u64, u64, u64)>> {
    const LAST: u32 = 0x1;
    const SHARED: u32 = 0x2000;
    let extents = crate::sys::fiemap_full(file).ok()?;
    if extents.is_empty() || extents.iter().any(|e| e.3 & !(LAST | SHARED) != 0) {
        return None;
    }
    Some(extents.iter().map(|&(logical, physical, len, _)| (logical, physical, len)).collect())
}
