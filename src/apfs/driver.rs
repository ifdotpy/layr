//! Operation-local APFS clone/copy adapter with explicit consistency and retained cleanup authority.
use super::*;
use layr_fs_core::{
    evidence, hints, Attempt, Capabilities, Capture, CloneToNew, CreatedTree, Filesystem, FrozenTree, FsError, NewTreeLocation,
    OwnedTree, RelDir, RelPath, RemovalFailure, TreeIssuer, TreeKey, TreeRead, TreeScope, TreeWrite, WorkingTree,
};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use std::ffi::OsStr;
use std::os::{fd::AsFd, unix::ffi::OsStrExt};
use std::sync::Mutex;

fn backend<T>(result: anyhow::Result<T>) -> layr_fs_core::Result<T> {
    result.map_err(FsError::backend)
}
fn io<T>(result: rustix::io::Result<T>) -> layr_fs_core::Result<T> {
    result.map_err(|error| FsError::Io(error.into()))
}
fn permission_mode(bits: u32) -> layr_fs_core::Result<Mode> {
    Ok(Mode::from_raw_mode(bits.try_into().map_err(|_| FsError::Invalid("APFS mode out of range"))?))
}
#[derive(Clone, Copy)]
pub(crate) enum Links {
    Keep,
    Split,
}
#[derive(Clone, Copy)]
pub(crate) enum PreparedCopy {
    Use,
    Fresh,
}
#[derive(Clone, Copy)]
pub(super) enum Owners {
    Root,
    EveryEntry,
}
#[derive(Clone, Copy)]
pub(super) struct CopyReport {
    pub consistency: CaptureConsistency,
    pub owners: Owners,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Working,
    Frozen,
}
#[derive(Clone)]
struct Destination {
    path: PathBuf,
    parent: Arc<File>,
    name: RelPath,
}
#[derive(Clone)]
struct Operation {
    parent: Arc<File>,
    name: RelPath,
    path: PathBuf,
}
impl Operation {
    fn remove(&self) -> layr_fs_core::Result<()> {
        match rustix::fs::unlinkat(self.parent.as_ref(), OsStr::from_bytes(self.name.as_bytes()), AtFlags::REMOVEDIR) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(FsError::Io(error.into())),
        }
    }
}
#[derive(Clone)]
struct Stage {
    operation: Operation,
    directory: Arc<File>,
}
impl Stage {
    fn tree_path(&self) -> PathBuf {
        self.operation.path.join("tree")
    }
}
enum Custody {
    Empty,
    Inspect,
    Owned,
    Foreign,
}
enum Placement {
    Private(Stage),
    Public(Destination),
    Quarantine { stage: Stage, restore: Destination, state: Custody },
    Restored(Destination),
    Removed,
}
struct NativeTree {
    reader: Arc<ApfsTree>,
    access: Access,
    report: CopyReport,
}
struct Pinned {
    tree: NativeTree,
    placement: Placement,
    cleanup: Vec<Operation>,
}
enum Resource {
    Vacant,
    Directories(Vec<Operation>),
    Staged(Stage),
    Borrowed(NativeTree),
    Pinned(Pinned),
}
impl Resource {
    fn tree(&self) -> Option<&NativeTree> {
        match self {
            Self::Borrowed(tree) => Some(tree),
            Self::Pinned(pinned) => Some(&pinned.tree),
            _ => None,
        }
    }
}
pub(crate) struct ApfsFs {
    issuer: TreeIssuer,
    anchor: Arc<File>,
    anchor_path: PathBuf,
    links: Links,
    prepared: PreparedCopy,
    trees: Mutex<HashMap<TreeKey, Resource>>,
    destinations: Mutex<HashMap<TreeKey, Destination>>,
}
pub(super) fn absolute(path: &Path) -> layr_fs_core::Result<PathBuf> {
    let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if path.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
        return Err(FsError::Invalid("parent component in APFS path"));
    }
    // Resolve external aliases such as macOS /tmp -> /private/tmp, preserving the final
    // component so a tree symlink never becomes deletion authority for its target.
    let parent = path.parent().ok_or(FsError::Invalid("APFS tree has no parent"))?;
    let name = path.file_name().ok_or(FsError::Invalid("APFS tree has no name"))?;
    Ok(fs::canonicalize(parent)?.join(name))
}
fn stable_namespace(path: &Path) -> layr_fs_core::Result<bool> {
    let uid = rustix::process::geteuid().as_raw();
    for parent in path.ancestors() {
        let metadata = fs::symlink_metadata(parent)?;
        let sticky_root = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir()
            || !matches!(metadata.uid(), 0) && metadata.uid() != uid
            || metadata.mode() & 0o022 != 0 && !sticky_root
        {
            return Ok(false);
        }
    }
    Ok(true)
}
impl ApfsFs {
    pub(crate) fn for_destination(path: &Path, links: Links, prepared: PreparedCopy) -> layr_fs_core::Result<Arc<Self>> {
        let path = absolute(path)?;
        let parent = path.parent().ok_or(FsError::Invalid("APFS destination has no parent"))?;
        if !is_supported(parent) {
            return Err(FsError::Invalid("APFS driver requires an APFS volume"));
        }
        let device = fs::metadata(parent)?.dev();
        let uid = rustix::process::geteuid().as_raw();
        for candidate in parent.ancestors() {
            let metadata = fs::symlink_metadata(candidate)?;
            if metadata.is_dir()
                && metadata.dev() == device
                && ((metadata.uid() == uid && metadata.mode() & 0o022 == 0)
                    // A root-owned sticky parent protects each private operation directory
                    // from other users; the retained anchor descriptor locates all mutations.
                    || (metadata.uid() == 0 && metadata.mode() & 0o1000 != 0))
                && stable_namespace(candidate)?
            {
                let anchor = Arc::new(File::open(candidate)?);
                return Ok(Arc::new(Self {
                    issuer: TreeIssuer::default(),
                    anchor,
                    anchor_path: candidate.to_path_buf(),
                    links,
                    prepared,
                    trees: Mutex::new(HashMap::new()),
                    destinations: Mutex::new(HashMap::new()),
                }));
            }
        }
        Err(FsError::Invalid("APFS needs a trusted operation directory on the destination volume"))
    }
    fn trees(&self) -> layr_fs_core::Result<std::sync::MutexGuard<'_, HashMap<TreeKey, Resource>>> {
        self.trees.lock().map_err(|_| FsError::Invalid("APFS handle registry lock poisoned"))
    }
    fn working(&self, path: &Path) -> layr_fs_core::Result<WorkingTree> {
        let reader = Arc::new(ApfsTree::open(path)?);
        backend(subvol_info(path))?;
        let tree = self.issuer.import_working(reader.clone());
        self.trees()?.insert(
            self.issuer.working_key(&tree)?,
            Resource::Borrowed(NativeTree {
                reader,
                access: Access::Working,
                report: CopyReport { consistency: CaptureConsistency::UnsettledCopy, owners: Owners::EveryEntry },
            }),
        );
        Ok(tree)
    }
    pub(crate) fn location(&self, path: &Path) -> layr_fs_core::Result<NewTreeLocation> {
        let path = absolute(path)?;
        let parent = Arc::new(File::open(path.parent().ok_or(FsError::Invalid("APFS destination has no parent"))?)?);
        let name = RelPath::new(path.file_name().ok_or(FsError::Invalid("APFS destination has no name"))?.as_bytes())?;
        let location = self.issuer.location(name.clone(), parent.clone())?;
        self.destinations
            .lock()
            .map_err(|_| FsError::Invalid("APFS destination registry lock poisoned"))?
            .insert(self.issuer.location_key(&location)?, Destination { path, parent, name });
        Ok(location)
    }
    fn destination(&self, location: NewTreeLocation) -> layr_fs_core::Result<Destination> {
        let key = self.issuer.location_key(&location)?;
        let destination = self
            .destinations
            .lock()
            .map_err(|_| FsError::Invalid("APFS destination registry lock poisoned"))?
            .remove(&key)
            .ok_or(FsError::Invalid("APFS destination absent"))?;
        let named = fs::symlink_metadata(destination.path.parent().ok_or(FsError::Invalid("APFS destination has no parent"))?)?;
        let held = destination.parent.metadata()?;
        if !named.is_dir() || (named.dev(), named.ino()) != (held.dev(), held.ino()) {
            return Err(FsError::LocationChanged);
        }
        match rustix::fs::statat(
            destination.parent.as_ref(),
            OsStr::from_bytes(destination.name.as_bytes()),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Err(rustix::io::Errno::NOENT) => Ok(destination),
            Err(error) => Err(FsError::Io(error.into())),
            Ok(_) => Err(FsError::Invalid("APFS destination exists")),
        }
    }
    fn operation(&self) -> layr_fs_core::Result<Operation> {
        let name = RelPath::new(format!(".layr-apfs-{}", crate::ids::new_id()).as_bytes())?;
        Ok(Operation { parent: self.anchor.clone(), path: self.anchor_path.join(OsStr::from_bytes(name.as_bytes())), name })
    }
    fn open_stage(operation: Operation) -> layr_fs_core::Result<Stage> {
        let directory = Arc::new(File::from(io(rustix::fs::openat(
            operation.parent.as_ref(),
            OsStr::from_bytes(operation.name.as_bytes()),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ))?));
        Ok(Stage { operation, directory })
    }
    fn rollback(&self, ownership: OwnedTree, cause: FsError) -> FsError {
        match self.remove(ownership) {
            Ok(()) => cause,
            Err(failure) => FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) },
        }
    }
    fn create(
        &self,
        source: &dyn TreeRead,
        native: Option<Arc<ApfsTree>>,
        destination: NewTreeLocation,
        access: Access,
    ) -> layr_fs_core::Result<(Arc<ApfsTree>, OwnedTree, CopyReport)> {
        let destination = self.destination(destination)?;
        if let Some(source) = &native {
            if self.anchor_path.starts_with(&source.path) || destination.path.starts_with(&source.path) {
                return Err(FsError::Invalid("APFS operation workspace is inside its source tree"));
            }
        }
        if access == Access::Frozen {
            let parent = destination.parent.metadata()?;
            if parent.uid() != rustix::process::geteuid().as_raw() || parent.mode() & 0o077 != 0 {
                return Err(FsError::Invalid("APFS frozen publication requires a private parent"));
            }
        }
        if destination.parent.metadata()?.dev() != self.anchor.metadata()?.dev() {
            return Err(FsError::Invalid("APFS staging anchor is on another volume"));
        }
        let ownership = self.issuer.own();
        let key = self.issuer.owned_key(&ownership)?;
        self.trees()?.insert(key.clone(), Resource::Vacant);
        let result = (|| -> layr_fs_core::Result<(Arc<ApfsTree>, CopyReport)> {
            let operation = self.operation()?;
            {
                let mut trees = self.trees()?;
                let resource = trees.get_mut(&key).ok_or(FsError::Invalid("APFS creation reservation absent"))?;
                let cleanup = vec![operation.clone()];
                io(rustix::fs::mkdirat(
                    operation.parent.as_ref(),
                    OsStr::from_bytes(operation.name.as_bytes()),
                    Mode::from_raw_mode(0o700),
                ))?;
                *resource = Resource::Directories(cleanup);
            }
            let stage = Self::open_stage(operation)?;
            self.trees()?.insert(key.clone(), Resource::Staged(stage.clone()));
            let (report, private) = if let Some(native) = &native {
                native.validate()?;
                let report = backend(snapshot_native(
                    &native.path,
                    &stage.tree_path(),
                    access == Access::Frozen,
                    matches!(self.links, Links::Keep),
                    self.prepared,
                ))?;
                native.validate()?;
                (report, ApfsTree::open(&stage.tree_path())?)
            } else {
                backend(create_subvolume(&stage.tree_path()))?;
                let temporary = ApfsTree::open(&stage.tree_path())?;
                layr_fs_common::copy_tree(source, &temporary)?;
                (CopyReport { consistency: CaptureConsistency::UnsettledCopy, owners: Owners::EveryEntry }, temporary)
            };
            let root = private.directory.root_file();
            let mode = root.metadata()?.mode() & 0o7777;
            let reader = Arc::new(ApfsTree {
                path: destination.path.clone(),
                directory: layr_fs_unix::DirectoryTree::from_file(root.try_clone()?)?,
            });
            self.trees()?.insert(
                key.clone(),
                Resource::Pinned(Pinned {
                    tree: NativeTree { reader: reader.clone(), access, report },
                    placement: Placement::Private(stage.clone()),
                    cleanup: vec![stage.operation.clone()],
                }),
            );
            crate::sys::apfs_unseal_fd(root.as_fd())?;
            io(rustix::fs::fchmod(root, permission_mode(mode | 0o700)?))?;
            {
                let mut trees = self.trees()?;
                let Some(Resource::Pinned(pinned)) = trees.get_mut(&key) else {
                    return Err(FsError::Invalid("APFS prepared tree absent"));
                };
                io(rustix::fs::renameat_with(
                    stage.directory.as_ref(),
                    "tree",
                    destination.parent.as_ref(),
                    OsStr::from_bytes(destination.name.as_bytes()),
                    RenameFlags::NOREPLACE,
                ))?;
                pinned.placement = Placement::Public(destination);
            }
            io(rustix::fs::fchmod(root, permission_mode(mode)?))?;
            if access == Access::Frozen {
                crate::sys::apfs_immutable_fd(root.as_fd(), true)?;
            }
            {
                let mut trees = self.trees()?;
                let Some(Resource::Pinned(pinned)) = trees.get_mut(&key) else {
                    return Err(FsError::Invalid("APFS published tree absent"));
                };
                cleanup_directories(&mut pinned.cleanup)?;
            }
            Ok((reader, report))
        })();
        match result {
            Ok((reader, report)) => Ok((reader, ownership, report)),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
    fn report(&self, tree: &WorkingTree) -> layr_fs_core::Result<CopyReport> {
        self.trees()?
            .get(&self.issuer.working_key(tree)?)
            .and_then(Resource::tree)
            .map(|tree| tree.report)
            .ok_or(FsError::Invalid("APFS report absent"))
    }
    pub(super) fn snapshot_path(
        src: &Path,
        dst: &Path,
        readonly: bool,
        links: Links,
        prepared: PreparedCopy,
    ) -> Result<CopyReport> {
        let driver = Self::for_destination(dst, links, prepared)?;
        let result = (|| -> layr_fs_core::Result<CopyReport> {
            let source = driver.working(src)?;
            let destination = driver.location(dst)?;
            let fs: &dyn Filesystem = driver.as_ref();
            if readonly {
                let capture = fs.freeze(&source, destination)?;
                Ok(CopyReport { consistency: capture.consistency, owners: Owners::EveryEntry })
            } else {
                let created = fs.materialise(source.reader(), destination)?;
                driver.report(&created.tree)
            }
        })();
        result.map_err(|error| driver.error(error))
    }
    pub(super) fn remove_path(path: &Path) -> Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let path = absolute(path)?;
        if !fs::symlink_metadata(&path)?.is_dir() {
            // The legacy facade owns no directory custody for a standalone file or link.
            return delete_tree_native(&path);
        }
        let driver = Self::for_destination(&path, Links::Keep, PreparedCopy::Use)?;
        let reader = Arc::new(ApfsTree::open(&path)?);
        let parent = Arc::new(File::open(path.parent().ok_or(FsError::Invalid("APFS tree has no parent"))?)?);
        let name = RelPath::new(path.file_name().ok_or(FsError::Invalid("APFS tree has no name"))?.as_bytes())?;
        let ownership = driver.issuer.own();
        driver.trees()?.insert(
            driver.issuer.owned_key(&ownership)?,
            Resource::Pinned(Pinned {
                tree: NativeTree {
                    reader,
                    access: Access::Working,
                    report: CopyReport { consistency: CaptureConsistency::UnsettledCopy, owners: Owners::EveryEntry },
                },
                placement: Placement::Public(Destination { path, parent, name }),
                cleanup: Vec::new(),
            }),
        );
        driver.remove(ownership).map_err(|failure| {
            driver.error(FsError::Cleanup {
                cause: Box::new(FsError::Invalid("APFS removal requires recovery")),
                failure: Box::new(failure),
            })
        })
    }
    fn error(self: Arc<Self>, error: FsError) -> anyhow::Error {
        match error {
            FsError::Cleanup { cause, failure } => anyhow::Error::new(Cleanup { driver: self, cause: *cause, failure: *failure }),
            error => anyhow::Error::new(error),
        }
    }
}
pub(crate) struct Cleanup {
    pub(crate) driver: Arc<ApfsFs>,
    pub(crate) failure: RemovalFailure,
    cause: FsError,
}
impl std::fmt::Debug for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::fmt::Display for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; APFS cleanup requires retry: {} (anchor {})",
            self.cause,
            self.failure.error,
            self.driver.anchor_path.display()
        )
    }
}
impl std::error::Error for Cleanup {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}
impl Filesystem for ApfsFs {
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        let native = self
            .trees()?
            .values()
            .filter_map(Resource::tree)
            .find_map(|tree| std::ptr::addr_eq(tree.reader.as_ref() as &dyn TreeRead, source).then(|| tree.reader.clone()));
        let (reader, ownership, _) = self.create(source, native, dst, Access::Working)?;
        match self.issuer.attach_working(&ownership, reader) {
            Ok(tree) => Ok(CreatedTree { tree, ownership }),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
    fn freeze(&self, source: &WorkingTree, dst: NewTreeLocation) -> layr_fs_core::Result<Capture> {
        let reader = self
            .trees()?
            .get(&self.issuer.working_key(source)?)
            .and_then(Resource::tree)
            .map(|tree| tree.reader.clone())
            .ok_or(FsError::Invalid("APFS working tree absent"))?;
        let (reader, ownership, report) = self.create(source.reader(), Some(reader), dst, Access::Frozen)?;
        match self.issuer.attach_frozen(&ownership, reader) {
            Ok(tree) => Ok(Capture { tree, ownership, consistency: report.consistency }),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
    fn changes(&self, before: &FrozenTree, after: &FrozenTree) -> layr_fs_core::Result<Vec<evidence::EntryChange>> {
        self.issuer.frozen_key(before)?;
        self.issuer.frozen_key(after)?;
        layr_fs_common::changes(self, before, after)
    }
    fn remove(&self, ownership: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(ownership, |key| {
            let mut trees = self.trees()?;
            let resource = trees.get_mut(key).ok_or(FsError::Invalid("APFS tree absent"))?;
            match resource {
                Resource::Vacant => {}
                Resource::Directories(directories) => cleanup_directories(directories)?,
                Resource::Staged(stage) => {
                    unseal_directory(stage.directory.as_ref())?;
                    layr_fs_unix::DirectoryTree::remove_owned(
                        &stage.operation.parent,
                        OsStr::from_bytes(stage.operation.name.as_bytes()),
                        stage.directory.as_ref(),
                    )?;
                }
                Resource::Borrowed(_) => return Err(FsError::Invalid("borrowed APFS tree has no deletion authority")),
                Resource::Pinned(pinned) => {
                    pinned.remove(self)?;
                }
            }
            trees.remove(key);
            Ok(())
        })
    }
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_> {
        let Some(access) =
            self.issuer.scope_key(scope).ok().and_then(|key| {
                self.trees().ok().and_then(|trees| trees.get(&key).and_then(Resource::tree).map(|tree| tree.access))
            })
        else {
            return Capabilities::default();
        };
        Capabilities {
            change_hints: (access == Access::Frozen).then_some(self as &dyn hints::ChangeHints),
            clone_new: Some(self),
            reflink_range: None,
        }
    }
}
impl hints::ChangeHints for ApfsFs {
    fn between(&self, base: &FrozenTree, tree: &FrozenTree) -> layr_fs_core::Result<hints::HintSet> {
        let (a, b) = (self.issuer.frozen_key(base)?, self.issuer.frozen_key(tree)?);
        let trees = self.trees()?;
        let before = trees.get(&a).and_then(Resource::tree).ok_or(FsError::Invalid("APFS frozen tree absent"))?;
        let after = trees.get(&b).and_then(Resource::tree).ok_or(FsError::Invalid("APFS frozen tree absent"))?;
        before.reader.validate()?;
        after.reader.validate()?;
        Ok(hints::HintSet::Complete(backend(candidates(before.reader.path.as_path(), after.reader.path.as_path()))?))
    }
}
impl CloneToNew for ApfsFs {
    fn clone_new(
        &self,
        source: &FrozenTree,
        path: &RelPath,
        dst: &WorkingTree,
        target: &RelPath,
    ) -> layr_fs_core::Result<Attempt<()>> {
        let (a, b) = (self.issuer.frozen_key(source)?, self.issuer.working_key(dst)?);
        let (source, target_tree) = {
            let trees = self.trees()?;
            let reader = |key| {
                trees
                    .get(key)
                    .and_then(Resource::tree)
                    .map(|tree| tree.reader.clone())
                    .ok_or(FsError::Invalid("APFS clone tree absent"))
            };
            (reader(&a)?, reader(&b)?)
        };
        source.validate()?;
        target_tree.validate()?;
        let metadata = layr_fs_common::required(source.as_ref(), &RelDir::Path(path.clone()))?;
        if metadata.kind != crate::fsutil::FType::File {
            return Ok(Attempt::Unavailable(io::Error::from_raw_os_error(libc::ENOTSUP)));
        }
        let file = source.directory.open_native_file(path)?;
        let bytes = target.as_bytes();
        let (parent, name) = match bytes.iter().rposition(|byte| *byte == b'/') {
            Some(index) => (&bytes[..index], &bytes[index + 1..]),
            None => (b"".as_slice(), bytes),
        };
        let parent = target_tree.directory.open_directory(&RelDir::from_bytes(parent)?)?;
        let name = OsStr::from_bytes(name);
        match crate::sys::apfs_clone_at(file.as_fd(), parent.as_fd(), name) {
            Ok(()) => {
                let cloned = target_tree.directory.open_native_file(target)?;
                crate::sys::apfs_unseal_fd(cloned.as_fd())?;
                target_tree.set_metadata(&RelDir::Path(target.clone()), &metadata.attributes)?;
                Ok(Attempt::Applied(()))
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV | libc::EINVAL)) => {
                match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Err(rustix::io::Errno::NOENT) => Ok(Attempt::Unavailable(error)),
                    _ => Err(error.into()),
                }
            }
            Err(error) => Err(error.into()),
        }
    }
}
fn cleanup_directories(directories: &mut Vec<Operation>) -> layr_fs_core::Result<()> {
    while let Some(directory) = directories.last() {
        directory.remove()?;
        directories.pop();
    }
    Ok(())
}
fn unseal_directory(directory: &File) -> layr_fs_core::Result<()> {
    crate::sys::apfs_unseal_fd(directory.as_fd())?;
    let mode = directory.metadata()?.mode() & 0o7777;
    io(rustix::fs::fchmod(directory, permission_mode(mode | 0o700)?))?;
    for entry in io(rustix::fs::Dir::read_from(directory))? {
        let entry = io(entry)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let stat = io(rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW))?;
        let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        // O_EVTONLY pins unreadable entries without granting data access. rustix has no
        // named flag for it; metadata mutations still check the descriptor's owner.
        let flags = if kind == rustix::fs::FileType::Symlink {
            OFlags::RDONLY | OFlags::SYMLINK | OFlags::CLOEXEC
        } else {
            OFlags::from_bits_retain(u32::try_from(libc::O_EVTONLY).map_err(|_| FsError::Invalid("invalid event-only flag"))?)
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC
        };
        let entry = File::from(io(rustix::fs::openat(directory, name, flags, Mode::empty()))?);
        crate::sys::apfs_unseal_fd(entry.as_fd())?;
        if kind == rustix::fs::FileType::Directory {
            io(rustix::fs::fchmod(&entry, Mode::from_raw_mode((stat.st_mode & 0o7777) | 0o700)))?;
            let child = File::from(io(rustix::fs::openat(
                &entry,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?);
            unseal_directory(&child)?;
        }
    }
    Ok(())
}
impl Pinned {
    fn remove(&mut self, driver: &ApfsFs) -> layr_fs_core::Result<()> {
        loop {
            match &mut self.placement {
                Placement::Private(stage) => {
                    unseal_directory(self.tree.reader.directory.root_file())?;
                    layr_fs_unix::DirectoryTree::remove_owned(
                        stage.directory.as_ref(),
                        OsStr::new("tree"),
                        self.tree.reader.directory.root_file(),
                    )?;
                    self.placement = Placement::Removed;
                }
                Placement::Public(destination) => {
                    let stat = io(rustix::fs::statat(
                        destination.parent.as_ref(),
                        OsStr::from_bytes(destination.name.as_bytes()),
                        AtFlags::SYMLINK_NOFOLLOW,
                    ))?;
                    let held = self.tree.reader.directory.root_file().metadata()?;
                    if (stat.st_dev as u64, stat.st_ino as u64) != (held.dev(), held.ino()) {
                        return Err(FsError::LocationChanged);
                    }
                    let operation = driver.operation()?;
                    self.cleanup.reserve(1);
                    io(rustix::fs::mkdirat(
                        operation.parent.as_ref(),
                        OsStr::from_bytes(operation.name.as_bytes()),
                        Mode::from_raw_mode(0o700),
                    ))?;
                    self.cleanup.push(operation.clone());
                    let stage = ApfsFs::open_stage(operation)?;
                    self.placement = Placement::Quarantine { stage, restore: destination.clone(), state: Custody::Empty };
                }
                Placement::Quarantine { stage, restore, state } => match state {
                    Custody::Empty => {
                        crate::sys::apfs_unseal_fd(self.tree.reader.directory.root_file().as_fd())?;
                        let mode = self.tree.reader.directory.root_file().metadata()?.mode() & 0o7777;
                        io(rustix::fs::fchmod(self.tree.reader.directory.root_file(), permission_mode(mode | 0o700)?))?;
                        io(rustix::fs::renameat_with(
                            restore.parent.as_ref(),
                            OsStr::from_bytes(restore.name.as_bytes()),
                            stage.directory.as_ref(),
                            "tree",
                            RenameFlags::NOREPLACE,
                        ))?;
                        *state = Custody::Inspect;
                    }
                    Custody::Inspect => {
                        let actual = io(rustix::fs::statat(stage.directory.as_ref(), "tree", AtFlags::SYMLINK_NOFOLLOW))?;
                        let expected = self.tree.reader.directory.root_file().metadata()?;
                        *state = if (actual.st_dev as u64, actual.st_ino as u64) == (expected.dev(), expected.ino()) {
                            Custody::Owned
                        } else {
                            Custody::Foreign
                        };
                    }
                    Custody::Owned => {
                        unseal_directory(self.tree.reader.directory.root_file())?;
                        layr_fs_unix::DirectoryTree::remove_owned(
                            stage.directory.as_ref(),
                            OsStr::new("tree"),
                            self.tree.reader.directory.root_file(),
                        )?;
                        self.placement = Placement::Removed;
                    }
                    Custody::Foreign => {
                        io(rustix::fs::renameat_with(
                            stage.directory.as_ref(),
                            "tree",
                            restore.parent.as_ref(),
                            OsStr::from_bytes(restore.name.as_bytes()),
                            RenameFlags::NOREPLACE,
                        ))?;
                        self.placement = Placement::Restored(restore.clone());
                    }
                },
                Placement::Restored(destination) => {
                    cleanup_directories(&mut self.cleanup)?;
                    self.placement = Placement::Public(destination.clone());
                    return Err(FsError::LocationChanged);
                }
                Placement::Removed => {
                    cleanup_directories(&mut self.cleanup)?;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires macOS /tmp and an APFS volume; cross-compiled on Linux"]
    fn temporary_aliases_and_sticky_parent_children_keep_cleanup_authority() -> Result<()> {
        use std::os::unix::fs::symlink;
        // These are direct children of the root-owned sticky directory, with no user-owned
        // ancestor available above them. /tmp and /var also exercise the system aliases.
        for parent in [Path::new("/tmp"), Path::new("/private/tmp"), Path::new("/var/tmp")] {
            let path = parent.join(format!("layr-apfs-location-{}", crate::ids::new_id()));
            create_subvolume(&path)?;
            fs::write(path.join("file"), b"owned")?;
            let driver = ApfsFs::for_destination(&path, Links::Keep, PreparedCopy::Fresh)?;
            assert_eq!(driver.anchor_path, fs::canonicalize(parent)?);
            let expected = driver.anchor.metadata()?;
            let stage = driver.operation()?;
            io(rustix::fs::mkdirat(stage.parent.as_ref(), OsStr::from_bytes(stage.name.as_bytes()), Mode::from_raw_mode(0o700)))?;
            assert_eq!(stage.parent.metadata()?.ino(), expected.ino());
            stage.remove()?;
            ApfsFs::remove_path(&path)?;
            assert!(!path.exists());
        }
        let scratch = Path::new("/tmp").join(format!("layr-apfs-alias-{}", crate::ids::new_id()));
        fs::create_dir(&scratch)?;
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
        let result = (|| -> Result<()> {
            let target = scratch.join("target");
            create_subvolume(&target)?;
            fs::write(target.join("sentinel"), b"keep")?;
            symlink("target", scratch.join("link"))?;
            // Canonicalisation of the parent must not adopt the final symlink target.
            ApfsFs::remove_path(&scratch.join("link"))?;
            assert_eq!(fs::read(target.join("sentinel"))?, b"keep");
            ApfsFs::remove_path(&target)?;
            ApfsFs::remove_path(&scratch.join("missing/child"))?;
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&scratch);
        result?;
        cleanup?;
        Ok(())
    }
}
