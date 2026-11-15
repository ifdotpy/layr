use layr_fs_core::*;
use layr_fs_unix::DirectoryTree;
use rustix::fs::{self, Mode, OFlags};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn sys<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|error| FsError::Io(error.into()))
}

struct Stored {
    name: Vec<u8>,
    directory: Arc<DirectoryTree>,
}
/// Copy-only directories below a private operation-owned root. No snapshot or reflink promises.
pub struct PlainFs {
    issuer: TreeIssuer,
    root: Arc<File>,
    path: PathBuf,
    trees: Mutex<HashMap<TreeKey, Stored>>,
    exchange: Mutex<Option<(TreeKey, TreeKey, layr_fs_unix::Exchange)>>,
}
impl PlainFs {
    pub fn open(root: &Path) -> Result<Self> {
        let directory = DirectoryTree::open(root)?;
        let metadata = directory.root_file().metadata()?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(FsError::Invalid("plain filesystem requires an operation-private root"));
        }
        Ok(Self {
            issuer: TreeIssuer::default(),
            root: Arc::new(directory.root_file().try_clone()?),
            path: root.to_path_buf(),
            trees: Mutex::new(HashMap::new()),
            exchange: Mutex::new(None),
        })
    }
    pub fn location(&self, name: RelPath) -> Result<NewTreeLocation> {
        self.issuer.location(name, self.root.clone())
    }
    fn trees(&self) -> Result<std::sync::MutexGuard<'_, HashMap<TreeKey, Stored>>> {
        self.trees.lock().map_err(|_| FsError::Invalid("plain registry lock poisoned"))
    }
    /// Compatibility boundary for executors that require a working directory (external tools).
    pub fn working_path(&self, tree: &WorkingTree) -> Result<PathBuf> {
        let key = self.issuer.working_key(tree)?;
        let trees = self.trees()?;
        let stored = trees.get(&key).ok_or(FsError::Invalid("tree absent"))?;
        let root = std::fs::symlink_metadata(&self.path)?;
        let expected_root = self.root.metadata()?;
        let path = self.path.join(OsStr::from_bytes(&stored.name));
        let current = std::fs::symlink_metadata(&path)?;
        let expected = stored.directory.root_file().metadata()?;
        if !root.is_dir()
            || root.dev() != expected_root.dev()
            || root.ino() != expected_root.ino()
            || !current.is_dir()
            || current.dev() != expected.dev()
            || current.ino() != expected.ino()
        {
            return Err(FsError::Invalid("working directory was replaced"));
        }
        Ok(path)
    }
    fn create(&self, dst: &NewTreeLocation) -> Result<Arc<DirectoryTree>> {
        self.issuer.location_key(dst)?;
        let name = OsStr::from_bytes(dst.name().as_bytes());
        sys(fs::mkdirat(self.root.as_ref(), name, Mode::RUSR | Mode::WUSR | Mode::XUSR))?;
        Ok(Arc::new(DirectoryTree::from_file(File::from(sys(fs::openat(
            self.root.as_ref(),
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ))?))?))
    }
}
impl Filesystem for PlainFs {
    fn create_empty(&self, dst: NewTreeLocation) -> Result<CreatedTree> {
        let directory = self.create(&dst)?;
        let created = self.issuer.create(directory.clone());
        self.trees()?.insert(self.issuer.working_key(&created.tree)?, Stored { name: dst.name().as_bytes().to_vec(), directory });
        Ok(created)
    }
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> Result<CreatedTree> {
        let directory = self.create(&dst)?;
        let created = self.issuer.create(directory.clone());
        self.trees()?.insert(self.issuer.working_key(&created.tree)?, Stored { name: dst.name().as_bytes().to_vec(), directory });
        if let Err(cause) = layr_fs_common::copy_tree(source, created.tree.writer()) {
            drop(created.tree);
            return match self.remove(created.ownership) {
                Ok(()) => Err(cause),
                Err(failure) => Err(FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) }),
            };
        }
        Ok(created)
    }
    fn freeze(&self, source: FreezeSource<'_>, dst: NewTreeLocation) -> Result<Capture> {
        let reader = match source {
            FreezeSource::Captured(capture) => return layr_fs_common::republish(self, &self.issuer, capture, dst),
            FreezeSource::Working(source) => {
                self.issuer.working_key(source)?;
                source.reader()
            }
            FreezeSource::Reader(reader) => reader,
        };
        let created = self.materialise(reader, dst)?;
        // A before/after mtime or size check is not proof against preserved-time writes.
        self.issuer.seal(created, CaptureConsistency::UnsettledCopy)
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
    fn remove(&self, tree: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(tree, |key| {
            let mut trees = self.trees()?;
            let pending = self.exchange.lock().map_err(|_| FsError::Invalid("exchange lock poisoned"))?;
            if pending.as_ref().is_some_and(|(a, b, _)| key == a || key == b) {
                return Err(FsError::Busy);
            }
            let stored = trees.get(key).ok_or(FsError::Invalid("tree absent"))?;
            DirectoryTree::remove_owned(&self.root, OsStr::from_bytes(&stored.name), stored.directory.root_file())?;
            trees.remove(key);
            Ok(())
        })
    }
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_> {
        Capabilities {
            exchange: self
                .issuer
                .scope_key(scope)
                .ok()
                .filter(|key| self.trees().is_ok_and(|trees| trees.contains_key(key)))
                .map(|_| self as &dyn ExchangeTrees),
            ..Capabilities::default()
        }
    }
}

impl ExchangeTrees for PlainFs {
    fn exchange(&self, first: OwnedTree, second: OwnedTree) -> std::result::Result<(OwnedTree, OwnedTree), ExchangeFailure> {
        self.issuer.exchange(first, second, |a, b| {
            let mut trees = self.trees()?;
            let mut pending = self.exchange.lock().map_err(|_| FsError::Invalid("exchange lock poisoned"))?;
            if pending.as_ref().is_some_and(|(x, y, _)| a != x || b != y) {
                return Err(FsError::Busy.into());
            }
            // TreeIssuer checked that the two keys are distinct before invoking this closure.
            let [Some(first), Some(second)] = trees.get_disjoint_mut([a, b]) else {
                return Err(FsError::Invalid("exchange tree absent").into());
            };
            if pending.is_none() {
                let first_location = layr_fs_unix::ExchangeLocation::new(
                    self.root.clone(),
                    RelPath::new(first.name.clone())?,
                    Arc::new(first.directory.root_file().try_clone()?),
                )?;
                let second_location = layr_fs_unix::ExchangeLocation::new(
                    self.root.clone(),
                    RelPath::new(second.name.clone())?,
                    Arc::new(second.directory.root_file().try_clone()?),
                )?;
                *pending = Some((a.clone(), b.clone(), layr_fs_unix::Exchange::new(first_location, second_location)?));
            }
            let (_, _, operation) = pending.as_mut().ok_or(FsError::Invalid("exchange reservation absent"))?;
            let result = operation.run();
            let progress = operation.progress();
            if progress == ExchangeProgress::Swapped {
                first.name = operation.second.name.as_bytes().to_vec();
                second.name = operation.first.name.as_bytes().to_vec();
            }
            if result.is_ok() || progress == ExchangeProgress::Unchanged {
                *pending = None;
            }
            result.map_err(|error| ExchangeError { error, progress })
        })
    }
}
