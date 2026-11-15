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
enum Input<'a> {
    Empty,
    Reader(&'a dyn TreeRead),
    Native(Arc<ApfsTree>),
    Prepared(TreeKey),
    Layer { base: layr_types::StateId, attributes: layr_fs_core::EntryAttributes },
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
    Entry(crate::fs::custody::Entry),
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
struct PendingExchange {
    keys: (TreeKey, TreeKey),
    operation: layr_fs_unix::Exchange,
    readers: (Arc<ApfsTree>, Arc<ApfsTree>),
    destinations: (Destination, Destination),
}
pub(crate) struct ApfsFs {
    issuer: TreeIssuer,
    anchor: Arc<File>,
    anchor_path: PathBuf,
    links: Links,
    trees: Mutex<HashMap<TreeKey, Resource>>,
    destinations: Mutex<HashMap<TreeKey, Destination>>,
    exchange: Mutex<Option<PendingExchange>>,
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
    pub(crate) fn for_destination(path: &Path, links: Links) -> layr_fs_core::Result<Arc<Self>> {
        let path = absolute(path)?;
        let parent = path.parent().ok_or(FsError::Invalid("APFS destination has no parent"))?;
        if !supports_volume(parent) {
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
                    trees: Mutex::new(HashMap::new()),
                    destinations: Mutex::new(HashMap::new()),
                    exchange: Mutex::new(None),
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
        backend(native_info(path))?;
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
        source: Input<'_>,
        destination: NewTreeLocation,
        access: Access,
    ) -> layr_fs_core::Result<(Arc<ApfsTree>, OwnedTree, CopyReport)> {
        let destination = self.destination(destination)?;
        if let Input::Native(source) = &source {
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
            let (report, private) = if let Input::Native(native) = &source {
                native.validate()?;
                let report = backend(snapshot_native(
                    &native.path,
                    &stage.tree_path(),
                    access == Access::Frozen,
                    matches!(self.links, Links::Keep),
                ))?;
                native.validate()?;
                (report, ApfsTree::open(&stage.tree_path())?)
            } else if let Input::Prepared(source_key) = &source {
                let mut trees = self.trees()?;
                let Some(Resource::Pinned(pinned)) = trees.get_mut(source_key) else {
                    return Err(FsError::Invalid("prepared capture absent"));
                };
                if pinned.tree.access != Access::Frozen {
                    return Err(FsError::Invalid("prepared capture is writable"));
                }
                let root = pinned.tree.reader.directory.root_file();
                let mode = root.metadata()?.mode() & 0o7777;
                let original = pinned.custody(self)?.ok_or(FsError::Invalid("prepared capture already removed"))?;
                io(rustix::fs::renameat_with(
                    original.directory.as_ref(),
                    "tree",
                    stage.directory.as_ref(),
                    "tree",
                    RenameFlags::NOREPLACE,
                ))?;
                // The new staged resource owns the tree as soon as rename succeeds. The
                // original receipt now owns only its empty operation directories.
                pinned.placement = Placement::Removed;
                let root = pinned.tree.reader.directory.root_file();
                io(rustix::fs::fchmod(root, permission_mode(mode)?))?;
                crate::sys::apfs_immutable_fd(root.as_fd(), true)?;
                let private =
                    ApfsTree { path: stage.tree_path(), directory: layr_fs_unix::DirectoryTree::from_file(root.try_clone()?)? };
                (pinned.tree.report, private)
            } else {
                backend(create_tree_native(&stage.tree_path()))?;
                let temporary = ApfsTree::open(&stage.tree_path())?;
                match &source {
                    Input::Reader(reader) => layr_fs_common::copy_tree(*reader, &temporary)?,
                    Input::Layer { attributes, .. } => temporary.set_metadata(&RelDir::Root, attributes)?,
                    _ => {}
                }
                if access == Access::Frozen || matches!(source, Input::Layer { .. }) {
                    let root = temporary.directory.root_file();
                    let mode = root.metadata()?.mode() & 0o7777;
                    io(rustix::fs::fchmod(root, permission_mode(mode | 0o700)?))?;
                    let mut info = read_info(root)?;
                    info.readonly = access == Access::Frozen;
                    if let Input::Layer { base, .. } = &source {
                        info.base = Some(base.as_str().to_owned());
                    }
                    io(rustix::fs::fsetxattr(
                        root,
                        "com.layr.subvolume",
                        backend(serde_json::to_vec(&info).map_err(Into::into))?.as_slice(),
                        rustix::fs::XattrFlags::empty(),
                    ))?;
                    io(rustix::fs::fchmod(root, permission_mode(mode)?))?;
                    if access == Access::Frozen {
                        seal_directory(root, mode)?;
                    }
                }
                (
                    CopyReport {
                        consistency: if matches!(source, Input::Layer { .. }) {
                            CaptureConsistency::ValidatedCopy
                        } else {
                            CaptureConsistency::UnsettledCopy
                        },
                        owners: Owners::EveryEntry,
                    },
                    temporary,
                )
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
    fn publish_capture(&self, capture: Capture, dst: NewTreeLocation) -> layr_fs_core::Result<Capture> {
        let Capture { tree, ownership, consistency } = capture;
        let checked = (|| -> layr_fs_core::Result<()> {
            self.issuer.location_key(&dst)?;
            if self.issuer.frozen_key(&tree)? != self.issuer.owned_key(&ownership)? {
                return Err(FsError::Invalid("capture ownership mismatch"));
            }
            Ok(())
        })();
        drop(tree);
        let mut output = None;
        let result = self.issuer.remove(ownership, |key| {
            checked?;
            let (reader, ownership, _) = self.create(Input::Prepared(key.clone()), dst, Access::Frozen)?;
            let tree = match self.issuer.attach_frozen(&ownership, reader) {
                Ok(tree) => tree,
                Err(error) => return Err(self.rollback(ownership, error)),
            };
            output = Some(Capture { tree, ownership, consistency });
            let mut trees = self.trees()?;
            let Some(Resource::Pinned(pinned)) = trees.get_mut(key) else {
                return Err(FsError::Invalid("prepared capture absent"));
            };
            pinned.remove(self)?;
            trees.remove(key);
            Ok(())
        });
        match result {
            Ok(()) => output.ok_or(FsError::Invalid("published capture absent")),
            Err(RemovalFailure { error, ownership }) => {
                let cause = if let Some(published) = output {
                    drop(published.tree);
                    self.rollback(published.ownership, error)
                } else {
                    error
                };
                Err(FsError::Cleanup {
                    cause: Box::new(cause),
                    failure: Box::new(RemovalFailure { ownership, error: FsError::Invalid("prepared capture requires cleanup") }),
                })
            }
        }
    }
    fn working_created(&self, input: Input<'_>, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        let (reader, ownership, _) = self.create(input, dst, Access::Working)?;
        match self.issuer.attach_working(&ownership, reader) {
            Ok(tree) => Ok(CreatedTree { tree, ownership }),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
    fn frozen_created(&self, input: Input<'_>, dst: NewTreeLocation) -> layr_fs_core::Result<Capture> {
        let (reader, ownership, report) = self.create(input, dst, Access::Frozen)?;
        match self.issuer.attach_frozen(&ownership, reader) {
            Ok(tree) => Ok(Capture { tree, ownership, consistency: report.consistency }),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
    /// Sealing an owned sparse write changes its existing resource, with all working leases
    /// released. The receipt remains recoverable even after a partially completed seal.
    pub(crate) fn seal_owned(&self, created: CreatedTree, consistency: CaptureConsistency) -> layr_fs_core::Result<Capture> {
        let CreatedTree { tree, ownership } = created;
        let checked = self.issuer.working_key(&tree).and_then(|key| {
            if key == self.issuer.owned_key(&ownership)? {
                Ok(())
            } else {
                Err(FsError::Invalid("APFS seal ownership mismatch"))
            }
        });
        drop(tree);
        let result = checked.and_then(|()| {
            self.issuer.with_exclusive(&ownership, |key| {
                let mut trees = self.trees()?;
                let resource = trees.get_mut(key).ok_or(FsError::Invalid("APFS owned tree absent"))?;
                let Resource::Pinned(pinned) = resource else { return Err(FsError::Invalid("cannot seal a borrowed tree")) };
                let root = pinned.tree.reader.directory.root_file();
                let mode = root.metadata()?.mode() & 0o7777;
                io(rustix::fs::fchmod(root, permission_mode(mode | 0o700)?))?;
                let mut marker = read_info(root)?;
                marker.readonly = true;
                io(rustix::fs::fsetxattr(
                    root,
                    "com.layr.subvolume",
                    backend(serde_json::to_vec(&marker).map_err(Into::into))?.as_slice(),
                    rustix::fs::XattrFlags::empty(),
                ))?;
                seal_directory(root, mode)?;
                pinned.tree.access = Access::Frozen;
                pinned.tree.report.consistency = consistency;
                Ok(pinned.tree.reader.clone())
            })
        });
        match result {
            Ok(reader) => match self.issuer.attach_frozen(&ownership, reader) {
                Ok(tree) => Ok(Capture { tree, ownership, consistency }),
                Err(cause) => Err(self.rollback(ownership, cause)),
            },
            Err(cause) => Err(self.rollback(ownership, cause)),
        }
    }
    pub(crate) fn working_layer(
        &self,
        attributes: layr_fs_core::EntryAttributes,
        base: layr_types::StateId,
        dst: NewTreeLocation,
    ) -> layr_fs_core::Result<CreatedTree> {
        self.working_created(Input::Layer { base, attributes }, dst)
    }
    pub(crate) fn frozen_layer(
        &self,
        attributes: layr_fs_core::EntryAttributes,
        base: layr_types::StateId,
        dst: NewTreeLocation,
    ) -> layr_fs_core::Result<Capture> {
        self.frozen_created(Input::Layer { base, attributes }, dst)
    }
}

impl Filesystem for ApfsFs {
    fn create_empty(&self, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        self.working_created(Input::Empty, dst)
    }
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        let native = self
            .trees()?
            .values()
            .filter_map(Resource::tree)
            .find_map(|tree| std::ptr::addr_eq(tree.reader.as_ref() as &dyn TreeRead, source).then(|| tree.reader.clone()));
        let input = match native {
            Some(native) => Input::Native(native),
            None => Input::Reader(source),
        };
        self.working_created(input, dst)
    }

    fn freeze(&self, source: layr_fs_core::FreezeSource<'_>, dst: NewTreeLocation) -> layr_fs_core::Result<Capture> {
        let input = match source {
            layr_fs_core::FreezeSource::Captured(capture) => return self.publish_capture(capture, dst),
            layr_fs_core::FreezeSource::Working(source) => {
                let reader = self
                    .trees()?
                    .get(&self.issuer.working_key(source)?)
                    .and_then(Resource::tree)
                    .map(|tree| tree.reader.clone())
                    .ok_or(FsError::Invalid("APFS working tree absent"))?;
                Input::Native(reader)
            }
            layr_fs_core::FreezeSource::Reader(reader) => Input::Reader(reader),
        };
        self.frozen_created(input, dst)
    }

    fn changes(
        &self,
        before: &FrozenTree,
        after: &FrozenTree,
        request: layr_fs_core::ChangeRequest,
    ) -> layr_fs_core::Result<Vec<evidence::EntryChange>> {
        self.issuer.frozen_key(before)?;
        self.issuer.frozen_key(after)?;
        layr_fs_common::changes(self, before, after, request)
    }
    fn remove(&self, ownership: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(ownership, |key| {
            let mut trees = self.trees()?;
            let pending = self.exchange.lock().map_err(|_| FsError::Invalid("APFS exchange lock poisoned"))?;
            if pending.as_ref().is_some_and(|pending| key == &pending.keys.0 || key == &pending.keys.1) {
                return Err(FsError::Busy);
            }
            let resource = trees.get_mut(key).ok_or(FsError::Invalid("APFS tree absent"))?;
            match resource {
                Resource::Entry(entry) => entry.remove(|directory, name, file| {
                    crate::sys::apfs_unseal_fd(file.as_fd())?;
                    io(rustix::fs::unlinkat(directory, name, AtFlags::empty()))
                })?,
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
        if self
            .issuer
            .scope_key(scope)
            .ok()
            .is_some_and(|key| self.destinations.lock().ok().is_some_and(|locations| locations.contains_key(&key)))
        {
            return Capabilities { capture_copy: Some(self), ..Capabilities::default() };
        }
        let Some(access) =
            self.issuer.scope_key(scope).ok().and_then(|key| {
                self.trees().ok().and_then(|trees| trees.get(&key).and_then(Resource::tree).map(|tree| tree.access))
            })
        else {
            return Capabilities::default();
        };
        Capabilities {
            exchange: Some(self),
            capture_copy: Some(self),
            change_hints: (access == Access::Frozen).then_some(self as &dyn hints::ChangeHints),
            clone_new: Some(self),
            reflink_range: None,
        }
    }
}
impl hints::ChangeHints for ApfsFs {
    fn between(
        &self,
        base: &FrozenTree,
        tree: &FrozenTree,
        request: layr_fs_core::ChangeRequest,
    ) -> layr_fs_core::Result<hints::HintSet> {
        let (a, b) = (self.issuer.frozen_key(base)?, self.issuer.frozen_key(tree)?);
        let trees = self.trees()?;
        let before = trees.get(&a).and_then(Resource::tree).ok_or(FsError::Invalid("APFS frozen tree absent"))?;
        let after = trees.get(&b).and_then(Resource::tree).ok_or(FsError::Invalid("APFS frozen tree absent"))?;
        before.reader.validate()?;
        after.reader.validate()?;
        Ok(hints::HintSet::Complete(backend(candidate_observations(
            before.reader.path.as_path(),
            after.reader.path.as_path(),
            request,
            None,
        ))?))
    }
}
impl CloneToNew for ApfsFs {
    fn clone_new(
        &self,
        source: layr_fs_core::CloneSource<'_>,
        path: &RelPath,
        dst: &WorkingTree,
        target: &RelPath,
    ) -> layr_fs_core::Result<Attempt<()>> {
        let (a, b) = (self.issuer.clone_source_key(source)?, self.issuer.working_key(dst)?);
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
    fn custody(&mut self, driver: &ApfsFs) -> layr_fs_core::Result<Option<Stage>> {
        loop {
            match &mut self.placement {
                Placement::Private(stage) => return Ok(Some(stage.clone())),
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
                    Custody::Owned => return Ok(Some(stage.clone())),
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
                Placement::Removed => return Ok(None),
            }
        }
    }
    fn remove(&mut self, driver: &ApfsFs) -> layr_fs_core::Result<()> {
        if let Some(stage) = self.custody(driver)? {
            unseal_directory(self.tree.reader.directory.root_file())?;
            layr_fs_unix::DirectoryTree::remove_owned(
                stage.directory.as_ref(),
                OsStr::new("tree"),
                self.tree.reader.directory.root_file(),
            )?;
            self.placement = Placement::Removed;
        }
        cleanup_directories(&mut self.cleanup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_freeze_and_owned_layer_sealing_publish_readonly_markers() -> Result<()> {
        use layr_fs_core::{EntryAttributes, EntryData, FreezeSource};
        let root =
            crate::repo::Temp::directory(std::env::temp_dir().join(format!("layr-apfs-seal-{}", crate::ids::new_id())), 0o700)?;
        let destination = root.path.join("frozen");
        let driver = ApfsFs::for_destination(&destination, Links::Keep)?;
        let attributes = EntryAttributes {
            mode: 0o500,
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            modified_ns: 1_000_000_007,
            stored_time: Some(1_000_000_007),
            xattrs: Vec::new(),
            acl: None,
        };
        let source = layr_fs_plain::MemoryTree::new(attributes.clone());
        source.put(&RelPath::new(b"file")?, EntryData::File(&mut &b"logical data"[..]), &attributes)?;
        let capture = driver.freeze(FreezeSource::Reader(&source), driver.location(&destination)?)?;
        assert!(
            read_info(
                driver
                    .trees()?
                    .get(&driver.issuer.frozen_key(&capture.tree)?)
                    .and_then(Resource::tree)
                    .context("frozen tree absent")?
                    .reader
                    .directory
                    .root_file()
            )?
            .readonly
        );
        assert_eq!(fs::metadata(&destination)?.mode() & 0o7777, 0o500);
        assert!(fs::OpenOptions::new().write(true).open(destination.join("file")).is_err());
        let admitted = crate::fs::PathAdmission::frozen(driver.as_ref(), &destination)?;
        drop(admitted);
        drop(capture.tree);
        crate::fs::PathAdmission::release_unused(driver.as_ref());
        let received = crate::ids::new_id();
        crate::fs::PathAdmission::received_identity(driver.as_ref(), &capture.ownership, &received, 0)?;
        let marker = native_info(&destination)?;
        assert!(marker.readonly);
        assert_eq!(marker.received_uuid.as_deref(), Some(received.as_str()));
        assert!(fs::OpenOptions::new().write(true).open(destination.join("file")).is_err());
        driver.remove(capture.ownership).map_err(|failure| failure.error)?;
        let destination = root.path.join("layer");
        let created = driver.working_layer(
            attributes,
            layr_types::StateId::parse(&crate::ids::new_id())?,
            driver.location(&destination)?,
        )?;
        let capture = driver.seal_owned(created, CaptureConsistency::ValidatedCopy)?;
        let marker = native_info(&destination)?;
        assert!(marker.readonly && marker.base.is_some());
        let admitted = crate::fs::PathAdmission::frozen(driver.as_ref(), &destination)?;
        drop(admitted);
        drop(capture.tree);
        driver.remove(capture.ownership).map_err(|failure| failure.error)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires macOS /tmp and an APFS volume; cross-compiled on Linux"]
    fn temporary_aliases_and_sticky_parent_children_keep_cleanup_authority() -> Result<()> {
        use std::os::unix::fs::symlink;
        // These are direct children of the root-owned sticky directory, with no user-owned
        // ancestor available above them. /tmp and /var also exercise the system aliases.
        for parent in [Path::new("/tmp"), Path::new("/private/tmp"), Path::new("/var/tmp")] {
            let path = parent.join(format!("layr-apfs-location-{}", crate::ids::new_id()));
            crate::fs::create(&path)?;
            fs::write(path.join("file"), b"owned")?;
            let driver = ApfsFs::for_destination(&path, Links::Keep)?;
            assert_eq!(driver.anchor_path, fs::canonicalize(parent)?);
            let expected = driver.anchor.metadata()?;
            let stage = driver.operation()?;
            io(rustix::fs::mkdirat(stage.parent.as_ref(), OsStr::from_bytes(stage.name.as_bytes()), Mode::from_raw_mode(0o700)))?;
            assert_eq!(stage.parent.metadata()?.ino(), expected.ino());
            stage.remove()?;
            crate::fs::remove(&path)?;
            assert!(!path.exists());
        }
        let scratch = Path::new("/tmp").join(format!("layr-apfs-alias-{}", crate::ids::new_id()));
        fs::create_dir(&scratch)?;
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
        let result = (|| -> Result<()> {
            let target = scratch.join("target");
            crate::fs::create(&target)?;
            fs::write(target.join("sentinel"), b"keep")?;
            symlink("target", scratch.join("link"))?;
            // Canonicalisation of the parent must not adopt the final symlink target.
            crate::fs::remove(&scratch.join("link"))?;
            assert_eq!(fs::read(target.join("sentinel"))?, b"keep");
            crate::fs::remove(&target)?;
            crate::fs::remove(&scratch.join("missing/child"))?;
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(&scratch);
        result?;
        cleanup?;
        Ok(())
    }
}

impl layr_fs_core::CaptureCopy for ApfsFs {
    fn prepare_before_lock(&self) -> bool {
        true
    }
    fn copy_capture(&self, source: &WorkingTree, dst: NewTreeLocation) -> layr_fs_core::Result<layr_fs_core::WritableCapture> {
        let reader = self
            .trees()?
            .get(&self.issuer.working_key(source)?)
            .and_then(Resource::tree)
            .map(|tree| tree.reader.clone())
            .ok_or(FsError::Invalid("APFS working tree absent"))?;
        let (reader, ownership, report) = self.create(Input::Native(reader), dst, Access::Working)?;
        match self.issuer.attach_working(&ownership, reader) {
            Ok(tree) => Ok(layr_fs_core::WritableCapture {
                created: CreatedTree { tree, ownership },
                consistency: report.consistency,
                owners: match report.owners {
                    Owners::Root => layr_fs_core::OwnerLayout::Uniform,
                    Owners::EveryEntry => layr_fs_core::OwnerLayout::PerEntry,
                },
            }),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
}
impl crate::fs::PathAdmission for ApfsFs {
    fn received_identity(&self, ownership: &OwnedTree, uuid: &str, _transid: u64) -> layr_fs_core::Result<()> {
        backend(uuid::Uuid::parse_str(uuid).map_err(Into::into))?;
        self.issuer.with_exclusive(ownership, |key| {
            let trees = self.trees()?;
            let Some(Resource::Pinned(pinned)) = trees.get(key) else {
                return Err(FsError::Invalid("received root is not owned"));
            };
            let root = pinned.tree.reader.directory.root_file();
            let mut marker = read_info(root)?;
            marker.received_uuid = Some(uuid.to_owned());
            let bytes = backend(serde_json::to_vec(&marker).map_err(Into::into))?;
            let stat = io(rustix::fs::fstat(root))?;
            let acl = crate::sys::apfs_acl_text_fd(root.as_fd())?;
            crate::sys::apfs_unseal_fd(root.as_fd())?;
            let result = (|| {
                io(rustix::fs::fchmod(root, permission_mode((u32::from(stat.st_mode) & 0o7777) | 0o200)?))?;
                io(rustix::fs::fsetxattr(root, "com.layr.subvolume", &bytes, rustix::fs::XattrFlags::empty()))
            })();
            let restore = (|| -> layr_fs_core::Result<()> {
                io(rustix::fs::fchmod(root, permission_mode(u32::from(stat.st_mode) & 0o7777)?))?;
                crate::sys::apfs_set_acl_text_fd(root.as_fd(), acl.as_deref())?;
                if stat.st_flags & libc::UF_IMMUTABLE != 0 {
                    crate::sys::apfs_immutable_fd(root.as_fd(), true)?;
                }
                Ok(())
            })();
            restore?;
            result
        })
    }
    fn release_unused(&self) {
        // Releasing unreferenced borrowed readers cannot mutate or discard an owned resource.
        self.trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|key, resource| !matches!(resource, Resource::Borrowed(_)) || self.issuer.has_handles(key));
    }
    fn working(&self, path: &Path) -> layr_fs_core::Result<WorkingTree> {
        ApfsFs::working(self, path)
    }
    fn frozen(&self, path: &Path) -> layr_fs_core::Result<FrozenTree> {
        let reader = Arc::new(ApfsTree::open(path)?);
        if !read_info(reader.directory.root_file())?.readonly
            || io(rustix::fs::fstat(reader.directory.root_file()))?.st_flags & libc::UF_IMMUTABLE == 0
        {
            return Err(FsError::Invalid("APFS tree is not readonly"));
        }
        let tree = self.issuer.import(reader.clone());
        self.trees()?.insert(
            self.issuer.frozen_key(&tree)?,
            Resource::Borrowed(NativeTree {
                reader,
                access: Access::Frozen,
                report: CopyReport { consistency: CaptureConsistency::ValidatedCopy, owners: Owners::EveryEntry },
            }),
        );
        Ok(tree)
    }
    fn location(&self, path: &Path) -> layr_fs_core::Result<NewTreeLocation> {
        ApfsFs::location(self, path)
    }
    fn claim(&self, path: crate::fs::ManagedPath) -> layr_fs_core::Result<OwnedTree> {
        let path = absolute(path.path())?;
        if !fs::symlink_metadata(&path)?.is_dir() {
            let entry = crate::fs::custody::Entry::claim(&path)?;
            let owner = self.issuer.own();
            self.trees()?.insert(self.issuer.owned_key(&owner)?, Resource::Entry(entry));
            return Ok(owner);
        }
        let reader = Arc::new(ApfsTree::open(&path)?);
        let parent = Arc::new(File::open(path.parent().ok_or(FsError::Invalid("tree has no parent"))?)?);
        let name = RelPath::new(path.file_name().ok_or(FsError::Invalid("tree has no name"))?.as_bytes())?;
        let ownership = self.issuer.own();
        self.trees()?.insert(
            self.issuer.owned_key(&ownership)?,
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
        Ok(ownership)
    }
}

fn read_info(root: &File) -> layr_fs_core::Result<SubvolInfo> {
    let mut marker = vec![0; io(rustix::fs::fgetxattr_query(root, "com.layr.subvolume"))?];
    let length = io(rustix::fs::fgetxattr(root, "com.layr.subvolume", marker.as_mut_slice()))?;
    backend(serde_json::from_slice(&marker[..length]).map_err(Into::into))
}

fn seal_directory(directory: &File, mode: u32) -> layr_fs_core::Result<()> {
    io(rustix::fs::fchmod(directory, permission_mode(mode | 0o700)?))?;
    for entry in io(rustix::fs::Dir::read_from(directory))? {
        let entry = io(entry)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let stat = io(rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW))?;
        let kind = rustix::fs::FileType::from_raw_mode(stat.st_mode);
        let flags = if kind == rustix::fs::FileType::Symlink {
            OFlags::RDONLY | OFlags::SYMLINK | OFlags::CLOEXEC
        } else {
            OFlags::from_bits_retain(u32::try_from(libc::O_EVTONLY).map_err(|_| FsError::Invalid("invalid event-only flag"))?)
                | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
        };
        let file = File::from(io(rustix::fs::openat(directory, name, flags, Mode::empty()))?);
        if kind == rustix::fs::FileType::Directory {
            let mode = file.metadata()?.mode() & 0o7777;
            io(rustix::fs::fchmod(&file, permission_mode(mode | 0o700)?))?;
            let child = File::from(io(rustix::fs::openat(
                &file,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?);
            seal_directory(&child, mode)?;
        }
        if kind != rustix::fs::FileType::Directory {
            crate::sys::apfs_immutable_fd(file.as_fd(), true)?;
        }
    }
    io(rustix::fs::fchmod(directory, permission_mode(mode)?))?;
    crate::sys::apfs_immutable_fd(directory.as_fd(), true)?;
    Ok(())
}

impl layr_fs_core::ExchangeTrees for ApfsFs {
    fn exchange(
        &self,
        first: OwnedTree,
        second: OwnedTree,
    ) -> std::result::Result<(OwnedTree, OwnedTree), layr_fs_core::ExchangeFailure> {
        use layr_fs_core::{ExchangeError, ExchangeProgress};
        self.issuer.exchange(first, second, |a, b| {
            let mut trees = self.trees()?;
            let mut pending = self.exchange.lock().map_err(|_| FsError::Invalid("APFS exchange lock poisoned"))?;
            if pending.as_ref().is_some_and(|pending| &pending.keys.0 != a || &pending.keys.1 != b) {
                return Err(FsError::Busy.into());
            }
            // The issuer rejected duplicate keys and active leases before taking this lock.
            let [Some(Resource::Pinned(first)), Some(Resource::Pinned(second))] = trees.get_disjoint_mut([a, b]) else {
                return Err(FsError::Invalid("exchange requires owned APFS trees").into());
            };
            if pending.is_none() {
                first.tree.reader.validate()?;
                second.tree.reader.validate()?;
                let (Placement::Public(from), Placement::Public(to)) = (&first.placement, &second.placement) else {
                    return Err(FsError::Invalid("APFS exchange requires published tree locations").into());
                };
                // Allocate descriptors and destination readers before changing the namespace.
                let new_first = Arc::new(ApfsTree {
                    path: to.path.clone(),
                    directory: layr_fs_unix::DirectoryTree::from_file(first.tree.reader.directory.root_file().try_clone()?)?,
                });
                let new_second = Arc::new(ApfsTree {
                    path: from.path.clone(),
                    directory: layr_fs_unix::DirectoryTree::from_file(second.tree.reader.directory.root_file().try_clone()?)?,
                });
                let first_location = layr_fs_unix::ExchangeLocation::new(
                    from.parent.clone(),
                    from.name.clone(),
                    Arc::new(first.tree.reader.directory.root_file().try_clone()?),
                )?;
                let second_location = layr_fs_unix::ExchangeLocation::new(
                    to.parent.clone(),
                    to.name.clone(),
                    Arc::new(second.tree.reader.directory.root_file().try_clone()?),
                )?;
                *pending = Some(PendingExchange {
                    keys: (a.clone(), b.clone()),
                    operation: layr_fs_unix::Exchange::new(first_location, second_location)?,
                    readers: (new_first, new_second),
                    destinations: (to.clone(), from.clone()),
                });
            }
            let transaction = pending.as_mut().ok_or(FsError::Invalid("APFS exchange reservation absent"))?;
            let result = transaction.operation.run();
            let progress = transaction.operation.progress();
            if progress == ExchangeProgress::Swapped {
                // Update both records even when post-swap validation failed. The retained
                // operation validates these destinations again on retry without swapping twice.
                first.tree.reader = transaction.readers.0.clone();
                second.tree.reader = transaction.readers.1.clone();
                first.placement = Placement::Public(transaction.destinations.0.clone());
                second.placement = Placement::Public(transaction.destinations.1.clone());
            }
            let result = result.and_then(|()| {
                first.tree.reader.validate()?;
                second.tree.reader.validate()
            });
            if result.is_ok() || progress == ExchangeProgress::Unchanged {
                *pending = None;
            }
            result.map_err(|error| ExchangeError { error, progress })
        })
    }
}
