//! Operation-local btrfs filesystem with backend-affine handles and retained cleanup authority.
mod creation;
mod removal;
use crate::btrfs as native;
use creation::{NativeTree, Resource};
use layr_fs_core::{evidence::FType, *};
use layr_fs_unix::DirectoryTree;
use removal::{Locator, Pinned, Placement};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn backend<T>(result: anyhow::Result<T>) -> Result<T> {
    result.map_err(FsError::backend)
}
fn io<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|e| FsError::Io(e.into()))
}
pub(crate) struct Reader {
    directory: DirectoryTree,
}
impl Reader {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Ok(Self { directory: DirectoryTree::open(path)? })
    }
}
impl TreeRead for Reader {
    fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>> {
        let mut metadata = self.directory.metadata(path)?;
        if let Some(meta) = &mut metadata {
            if meta.kind == FType::Dir {
                let directory = self.directory.open_directory(path)?;
                let stat = directory.metadata()?;
                if stat.ino() == 2 {
                    let parent = File::from(io(rustix::fs::openat(
                        &directory,
                        "..",
                        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
                        rustix::fs::Mode::empty(),
                    ))?);
                    if parent.metadata()?.dev() != stat.dev() {
                        meta.attributes.stored_time = None;
                    }
                }
            }
        }
        Ok(metadata)
    }
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()> {
        self.directory.entries(dir, visit)
    }
    fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>> {
        self.directory.open_file(path)
    }
    fn read_link(&self, path: &RelPath) -> Result<Vec<u8>> {
        self.directory.read_link(path)
    }
}
impl TreeWrite for Reader {
    fn put(&self, path: &RelPath, data: EntryData<'_>, attributes: &EntryAttributes) -> Result<()> {
        self.directory.put(path, data, attributes)
    }
    fn rename_new(&self, source: &RelPath, destination: &RelPath) -> Result<()> {
        self.directory.rename_new(source, destination)
    }
    fn remove_empty_dir(&self, path: &RelPath) -> Result<bool> {
        self.directory.remove_empty_dir(path)
    }
    fn overwrite_file(&self, path: &RelPath, source: &mut dyn std::io::Read, modified_ns: i64) -> Result<Attempt<()>> {
        self.directory.overwrite_file(path, source, modified_ns)
    }
    fn write_range(&self, path: &RelPath, offset: u64, bytes: &[u8], modified_ns: i64) -> Result<()> {
        self.directory.write_range(path, offset, bytes, modified_ns)
    }
    fn remove_entry(&self, path: &RelPath) -> Result<()> {
        self.directory.remove_entry(path)
    }
    fn set_metadata(&self, path: &RelDir, attributes: &EntryAttributes) -> Result<()> {
        self.directory.set_metadata(path, attributes)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Working,
    Frozen,
}
fn subvolume_id(root: &File) -> Result<u64> {
    // GET_SUBVOL_INFO also accepts ordinary directories and returns their containing
    // subvolume's ID. Only inode 256 denotes the pinned subvolume root itself.
    if root.metadata()?.ino() != 256 {
        return Err(FsError::Invalid("tree is not a subvolume root"));
    }
    Ok(crate::sys::subvol_info(root.as_fd())?.tree_id)
}

struct Destination {
    path: PathBuf,
    parent: Arc<File>,
}
pub(crate) struct BtrfsFs {
    anchor: Arc<File>,
    #[cfg(test)]
    fault: Mutex<Option<Box<dyn FnMut(&'static str) -> Result<()> + Send>>>,
    issuer: TreeIssuer,
    trees: Mutex<HashMap<TreeKey, Resource>>,
    destinations: Mutex<HashMap<TreeKey, Destination>>,
}
impl BtrfsFs {
    fn trees(&self) -> Result<std::sync::MutexGuard<'_, HashMap<TreeKey, Resource>>> {
        self.trees.lock().map_err(|_| FsError::Invalid("btrfs registry lock poisoned"))
    }
    pub(crate) fn location(&self, path: &Path) -> Result<NewTreeLocation> {
        let parent_path = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        let parent = Arc::new(File::open(parent_path)?);
        let name = RelPath::new(path.file_name().ok_or(FsError::Invalid("destination has no name"))?.as_bytes())?;
        let location = self.issuer.location(name, parent.clone())?;
        self.destinations
            .lock()
            .map_err(|_| FsError::Invalid("destination registry lock poisoned"))?
            .insert(self.issuer.location_key(&location)?, Destination { path: path.to_path_buf(), parent });
        Ok(location)
    }
    fn destination(&self, location: &NewTreeLocation) -> Result<Destination> {
        let key = self.issuer.location_key(location)?;
        self.destinations
            .lock()
            .map_err(|_| FsError::Invalid("destination registry lock poisoned"))?
            .remove(&key)
            .ok_or(FsError::Invalid("destination absent"))
    }
    fn parent(reader: &Reader) -> Result<Arc<File>> {
        Ok(Arc::new(File::from(io(rustix::fs::openat(
            reader.directory.root_file(),
            "..",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ))?)))
    }
    fn reader(path: &Path) -> Result<Arc<Reader>> {
        let directory = DirectoryTree::open(path)?;
        subvolume_id(directory.root_file())?;
        Ok(Arc::new(Reader { directory }))
    }
    pub(crate) fn working(&self, path: &Path) -> Result<WorkingTree> {
        let reader = Self::reader(path)?;
        let tree = self.issuer.import_working(reader.clone());
        self.trees()?.insert(
            self.issuer.working_key(&tree)?,
            Resource::Borrowed(NativeTree {
                path: path.to_path_buf(),
                reader,
                access: Access::Working,
                provenance: creation::Provenance::Other,
            }),
        );
        Ok(tree)
    }
    fn frozen(&self, path: &Path) -> Result<FrozenTree> {
        let reader = Self::reader(path)?;
        if crate::sys::subvol_flags(reader.directory.root_file().as_fd())? & crate::sys::SUBVOL_RDONLY == 0 {
            return Err(FsError::Invalid("tree is not read-only"));
        }
        let tree = self.issuer.import(reader.clone());
        self.trees()?.insert(
            self.issuer.frozen_key(&tree)?,
            Resource::Borrowed(NativeTree {
                path: path.to_path_buf(),
                reader,
                access: Access::Frozen,
                provenance: creation::Provenance::Other,
            }),
        );
        Ok(tree)
    }
    pub(crate) fn owned(&self, path: &Path) -> Result<OwnedTree> {
        let reader = Self::reader(path)?;
        let ownership = self.issuer.own();
        self.trees()?.insert(
            self.issuer.owned_key(&ownership)?,
            Resource::Pinned(Pinned {
                placement: Placement::Public(Locator {
                    parent: Self::parent(&reader)?,
                    name: RelPath::new(path.file_name().ok_or(FsError::Invalid("owned tree has no entry name"))?.as_bytes())?,
                }),
                tree: NativeTree {
                    path: path.to_path_buf(),
                    reader,
                    access: Access::Working,
                    provenance: creation::Provenance::Other,
                },
                cleanup: Vec::new(),
                children: Vec::new(),
            }),
        );
        Ok(ownership)
    }
    pub(crate) fn native_snapshot(&self, source: &WorkingTree, dst: NewTreeLocation) -> Result<CreatedTree> {
        let key = self.issuer.working_key(source)?;
        let reader = self.trees()?.get(&key).and_then(Resource::pinned).ok_or(FsError::Invalid("source absent"))?.reader.clone();
        let (reader, ownership) = self.create(creation::Source::Snapshot(reader.directory.root_file()), dst, Access::Working)?;
        match self.issuer.attach_working(&ownership, reader) {
            Ok(tree) => Ok(CreatedTree { tree, ownership }),
            Err(cause) => Err(self.rollback(ownership, cause)),
        }
    }
}
impl Filesystem for BtrfsFs {
    fn create_empty(&self, dst: NewTreeLocation) -> Result<CreatedTree> {
        let (reader, ownership) = self.create(creation::Source::Empty, dst, Access::Working)?;
        match self.issuer.attach_working(&ownership, reader) {
            Ok(tree) => Ok(CreatedTree { tree, ownership }),
            Err(error) => Err(self.rollback(ownership, error)),
        }
    }
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> Result<CreatedTree> {
        let native_source = {
            let trees = self.trees()?;
            trees.values().filter_map(Resource::pinned).find_map(|tree| {
                std::ptr::addr_eq(tree.reader.as_ref() as &dyn TreeRead, source).then(|| {
                    (tree.reader.clone(), tree.access == Access::Frozen && tree.provenance == creation::Provenance::Snapshot)
                })
            })
        };
        if let Some((reader, _)) = &native_source {
            if crate::sys::subvol_info(reader.directory.root_file().as_fd())?.tree_id
                == crate::sys::subvol_info(self.anchor.as_fd())?.tree_id
            {
                return Err(FsError::Invalid("materialisation staging anchor is inside its source tree"));
            }
        }
        let source = match &native_source {
            Some((reader, true)) => creation::Source::Snapshot(reader.directory.root_file()),
            _ => creation::Source::Copy(source),
        };
        let (reader, ownership) = self.create(source, dst, Access::Working)?;
        match self.issuer.attach_working(&ownership, reader) {
            Ok(tree) => Ok(CreatedTree { tree, ownership }),
            Err(cause) => Err(self.rollback(ownership, cause)),
        }
    }
    fn freeze(&self, source: FreezeSource<'_>, dst: NewTreeLocation) -> Result<Capture> {
        let source = match source {
            FreezeSource::Captured(capture) => return layr_fs_common::republish(self, &self.issuer, capture, dst),
            FreezeSource::Working(source) => source,
            FreezeSource::Reader(reader) => {
                let (reader, ownership) = self.create(creation::Source::Copy(reader), dst, Access::Frozen)?;
                return match self.issuer.attach_frozen(&ownership, reader) {
                    Ok(tree) => Ok(Capture { tree, ownership, consistency: CaptureConsistency::UnsettledCopy }),
                    Err(error) => Err(self.rollback(ownership, error)),
                };
            }
        };
        let key = self.issuer.working_key(source)?;
        let reader = self
            .trees()?
            .get(&key)
            .and_then(Resource::pinned)
            .ok_or(FsError::Invalid("source is not a pinned tree"))?
            .reader
            .clone();
        let (reader, ownership) = self.create(creation::Source::Snapshot(reader.directory.root_file()), dst, Access::Frozen)?;
        match self.issuer.attach_frozen(&ownership, reader) {
            Ok(tree) => Ok(Capture { tree, ownership, consistency: CaptureConsistency::AtomicPoint }),
            Err(cause) => Err(self.rollback(ownership, cause)),
        }
    }
    fn changes(
        &self,
        before: &FrozenTree,
        after: &FrozenTree,
        request: layr_fs_core::ChangeRequest,
    ) -> Result<Vec<evidence::EntryChange>> {
        self.issuer.frozen_key(before)?;
        self.issuer.frozen_key(after)?;
        layr_fs_common::changes(self, before, after, request)
    }
    fn remove(&self, ownership: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(ownership, |key| {
            let mut trees = self.trees()?;
            let resource = trees.get_mut(key).ok_or(FsError::Invalid("tree absent"))?;
            self.step("cleanup")?;
            resource.remove(self)?;
            trees.remove(key);
            Ok(())
        })
    }
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_> {
        let Ok(key) = self.issuer.scope_key(scope) else { return Capabilities::default() };
        let Ok(trees) = self.trees() else { return Capabilities::default() };
        let Some(tree) = trees.get(&key).and_then(Resource::pinned) else { return Capabilities::default() };
        if !rustix::fs::fstatfs(tree.reader.directory.root_file())
            .is_ok_and(|stat| stat.f_type as i64 == native::BTRFS_SUPER_MAGIC)
        {
            return Capabilities::default();
        }
        Capabilities {
            exchange: None,
            capture_copy: Some(self),
            change_hints: (tree.access == Access::Frozen
                && rustix::thread::capabilities(None)
                    .is_ok_and(|caps| caps.effective.contains(rustix::thread::CapabilitySet::SYS_ADMIN)))
            .then_some(self as &dyn hints::ChangeHints),
            clone_new: Some(self),
            reflink_range: Some(&RangeClone),
        }
    }
}
impl hints::ChangeHints for BtrfsFs {
    fn between(&self, before: &FrozenTree, after: &FrozenTree, request: layr_fs_core::ChangeRequest) -> Result<hints::HintSet> {
        {
            let _ = request;
            self.hints_with(before, after, native::send_no_data_pinned)
        }
    }
}
impl BtrfsFs {
    pub(crate) fn hints_with(
        &self,
        before: &FrozenTree,
        after: &FrozenTree,
        send: impl FnOnce(&Path, &Path, &File, &File) -> anyhow::Result<Vec<crate::stream::Entry>>,
    ) -> Result<hints::HintSet> {
        let (a, b) = (self.issuer.frozen_key(before)?, self.issuer.frozen_key(after)?);
        let trees = self.trees()?;
        let base = trees.get(&a).and_then(Resource::pinned).ok_or(FsError::Invalid("tree absent"))?;
        let tree = trees.get(&b).and_then(Resource::pinned).ok_or(FsError::Invalid("tree absent"))?;
        let entries =
            backend(send(&base.path, &tree.path, base.reader.directory.root_file(), tree.reader.directory.root_file()))?;
        let candidates = backend(crate::fs_candidates::build(base.reader.as_ref(), tree.reader.as_ref(), entries))?;
        for candidate in &candidates {
            for (reader, path, meta) in
                [(&base.reader, candidate.src(), candidate.old()), (&tree.reader, candidate.path().as_str(), candidate.new())]
            {
                if meta.is_some_and(|m| !m.is_dir())
                    && reader
                        .metadata(&RelDir::from_bytes(path.as_bytes())?)?
                        .and_then(|m| m.link)
                        .is_some_and(|link| link.count > 1)
                {
                    return Ok(hints::HintSet::Partial);
                }
            }
        }
        Ok(hints::HintSet::Complete(candidates))
    }
}
pub(crate) struct RangeClone;
impl ReflinkRange for RangeClone {
    fn clone_range(&self, source: &File, range: ByteRange, target: &File, offset: u64) -> Result<Attempt<()>> {
        match crate::sys::clone_range(source, range.offset(), range.length(), target, offset) {
            Ok(()) => Ok(Attempt::Applied(())),
            Err(error) if unavailable(&error) => Ok(Attempt::Unavailable(error)),
            Err(error) => Err(error.into()),
        }
    }
}
fn unavailable(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == rustix::io::Errno::OPNOTSUPP.raw_os_error() || code == rustix::io::Errno::XDEV.raw_os_error() || code == rustix::io::Errno::INVAL.raw_os_error() || code == rustix::io::Errno::NOTTY.raw_os_error())
}
impl CloneToNew for BtrfsFs {
    fn clone_new(
        &self,
        source: CloneSource<'_>,
        path: &RelPath,
        target: &WorkingTree,
        destination: &RelPath,
    ) -> Result<Attempt<()>> {
        let (source_key, target_key) = (self.issuer.clone_source_key(source)?, self.issuer.working_key(target)?);
        let (source, target) = {
            let trees = self.trees()?;
            let lookup = |key| -> Result<Arc<Reader>> {
                trees.get(key).and_then(Resource::pinned).map(|tree| tree.reader.clone()).ok_or(FsError::Invalid("tree absent"))
            };
            (lookup(&source_key)?, lookup(&target_key)?)
        };
        let file = source.directory.open_native_file(path)?;
        let bytes = destination.as_bytes();
        let (parent, name) = match bytes.iter().rposition(|b| *b == b'/') {
            Some(i) => (&bytes[..i], &bytes[i + 1..]),
            None => (b"".as_slice(), bytes),
        };
        let parent = target.directory.open_directory(&RelDir::from_bytes(parent)?)?;
        let name = std::ffi::OsStr::from_bytes(name);
        let output = File::from(io(rustix::fs::openat(
            &parent,
            name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ))?);
        match rustix::fs::ioctl_ficlone(&output, &file).map_err(std::io::Error::from) {
            Ok(()) => {
                let metadata = layr_fs_common::required(source.as_ref(), &RelDir::Path(path.clone()))?;
                target.set_metadata(&RelDir::Path(destination.clone()), &metadata.attributes)?;
                Ok(Attempt::Applied(()))
            }
            Err(error) => {
                io(rustix::fs::unlinkat(&parent, name, rustix::fs::AtFlags::empty()))?;
                if unavailable(&error) {
                    Ok(Attempt::Unavailable(error))
                } else {
                    Err(error.into())
                }
            }
        }
    }
}

impl CaptureCopy for BtrfsFs {
    fn prefers_capture_for_changes(&self) -> bool {
        rustix::thread::capabilities(None).is_ok_and(|caps| caps.effective.contains(rustix::thread::CapabilitySet::SYS_ADMIN))
    }
    fn copy_capture(&self, source: &WorkingTree, dst: NewTreeLocation) -> Result<WritableCapture> {
        Ok(WritableCapture {
            created: self.native_snapshot(source, dst)?,
            consistency: CaptureConsistency::AtomicPoint,
            owners: OwnerLayout::PerEntry,
        })
    }
}
impl crate::fs::PathAdmission for BtrfsFs {
    fn received_identity(&self, ownership: &OwnedTree, uuid: &str, transid: u64) -> Result<()> {
        let uuid = backend(uuid::Uuid::parse_str(uuid).map_err(Into::into))?;
        self.issuer.with_exclusive(ownership, |key| {
            let trees = self.trees()?;
            let Some(Resource::Pinned(pinned)) = trees.get(key) else {
                return Err(FsError::Invalid("received root is not owned"));
            };
            let root = pinned.tree.reader.directory.root_file();
            let flags = crate::sys::subvol_flags(root.as_fd())?;
            crate::sys::subvol_set_flags(root.as_fd(), flags & !crate::sys::SUBVOL_RDONLY)?;
            let result = crate::sys::subvol_set_received(root.as_fd(), *uuid.as_bytes(), transid);
            let restore = crate::sys::subvol_set_flags(root.as_fd(), flags);
            restore?;
            result.map_err(Into::into)
        })
    }
    fn release_unused(&self) {
        // Releasing unreferenced borrowed readers cannot mutate or discard an owned resource.
        self.trees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|key, resource| !matches!(resource, Resource::Borrowed(_)) || self.issuer.has_handles(key));
    }
    fn working(&self, path: &Path) -> Result<WorkingTree> {
        BtrfsFs::working(self, path)
    }
    fn frozen(&self, path: &Path) -> Result<FrozenTree> {
        BtrfsFs::frozen(self, path)
    }
    fn location(&self, path: &Path) -> Result<NewTreeLocation> {
        BtrfsFs::location(self, path)
    }
    fn claim(&self, path: crate::fs::ManagedPath) -> Result<OwnedTree> {
        if native::managed_root(path.path()) {
            return self.owned(path.path());
        }
        let entry = crate::fs::custody::Entry::claim(path.path())?;
        let owner = self.issuer.own();
        self.trees()?.insert(self.issuer.owned_key(&owner)?, Resource::Ordinary(entry));
        Ok(owner)
    }
}
