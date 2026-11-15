//! Backend-affine handles over OverlayFS's persistent upper/lower registry.
use super::*;
use layr_fs_core::{
    evidence, hints, Attempt, ByteRange, Capabilities, Capture, CreatedTree, Filesystem, FrozenTree, FsError, NewTreeLocation,
    OwnedTree, ReflinkRange, RelPath, RemovalFailure, TreeIssuer, TreeKey, TreeRead, TreeScope, WorkingTree,
};
use layr_fs_unix::DirectoryTree;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Mutex};

fn backend<T>(value: anyhow::Result<T>) -> layr_fs_core::Result<T> {
    value.map_err(FsError::backend)
}
struct Registered {
    path: PathBuf,
    directory: Arc<DirectoryTree>,
}
impl Registered {
    fn validate(&self) -> layr_fs_core::Result<()> {
        let actual = fs::symlink_metadata(&self.path)?;
        let held = self.directory.root_file().metadata()?;
        if !actual.is_dir() || (actual.dev(), actual.ino()) != (held.dev(), held.ino()) {
            return Err(FsError::LocationChanged);
        }
        Ok(())
    }
}
struct Destination {
    path: PathBuf,
    parent: File,
}
#[derive(Default)]
pub(crate) struct OverlayFs {
    issuer: TreeIssuer,
    trees: Mutex<HashMap<TreeKey, Registered>>,
    destinations: Mutex<HashMap<TreeKey, Destination>>,
}
impl OverlayFs {
    fn trees(&self) -> layr_fs_core::Result<std::sync::MutexGuard<'_, HashMap<TreeKey, Registered>>> {
        self.trees.lock().map_err(|_| FsError::Invalid("OverlayFS handle registry lock poisoned"))
    }
    fn register(&self, path: &Path) -> layr_fs_core::Result<Registered> {
        let path = backend(absolute(path))?;
        let directory = Arc::new(DirectoryTree::open(&path)?);
        Ok(Registered { path, directory })
    }
    fn working(&self, path: &Path) -> layr_fs_core::Result<WorkingTree> {
        backend(subvol_info(path))?;
        let registered = self.register(path)?;
        let tree = self.issuer.import_working(registered.directory.clone());
        self.trees()?.insert(self.issuer.working_key(&tree)?, registered);
        Ok(tree)
    }
    fn frozen(&self, path: &Path) -> layr_fs_core::Result<FrozenTree> {
        if !backend(subvol_info(path))?.readonly {
            return Err(FsError::Invalid("OverlayFS tree is not sealed"));
        }
        let registered = self.register(path)?;
        let tree = self.issuer.import(registered.directory.clone());
        self.trees()?.insert(self.issuer.frozen_key(&tree)?, registered);
        Ok(tree)
    }
    fn location(&self, path: &Path) -> layr_fs_core::Result<NewTreeLocation> {
        let path = backend(absolute(path))?;
        let parent = File::open(path.parent().ok_or(FsError::Invalid("destination has no parent"))?)?;
        let name = RelPath::new(path.file_name().ok_or(FsError::Invalid("destination has no name"))?.as_bytes())?;
        let location = self.issuer.location(name, Arc::new(parent.try_clone()?))?;
        self.destinations
            .lock()
            .map_err(|_| FsError::Invalid("OverlayFS destination registry lock poisoned"))?
            .insert(self.issuer.location_key(&location)?, Destination { path, parent });
        Ok(location)
    }
    fn destination(&self, location: NewTreeLocation) -> layr_fs_core::Result<PathBuf> {
        let key = self.issuer.location_key(&location)?;
        let dst = self
            .destinations
            .lock()
            .map_err(|_| FsError::Invalid("OverlayFS destination registry lock poisoned"))?
            .remove(&key)
            .ok_or(FsError::Invalid("destination absent"))?;
        let held = dst.parent.metadata()?;
        let actual = fs::symlink_metadata(dst.path.parent().ok_or(FsError::Invalid("destination has no parent"))?)?;
        if !actual.is_dir() || (held.dev(), held.ino()) != (actual.dev(), actual.ino()) {
            return Err(FsError::LocationChanged);
        }
        match fs::symlink_metadata(&dst.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(dst.path),
            Err(error) => Err(error.into()),
            Ok(_) => Err(FsError::Invalid("destination already exists")),
        }
    }
    fn path(&self, key: TreeKey) -> layr_fs_core::Result<PathBuf> {
        let trees = self.trees()?;
        let tree = trees.get(&key).ok_or(FsError::Invalid("tree absent"))?;
        tree.validate()?;
        Ok(tree.path.clone())
    }
    fn created(&self, path: &Path) -> layr_fs_core::Result<CreatedTree> {
        let registered = self.register(path)?;
        let created = self.issuer.create(registered.directory.clone());
        self.trees()?.insert(self.issuer.owned_key(&created.ownership)?, registered);
        Ok(created)
    }
    fn copy_native(
        &self,
        source: &WorkingTree,
        dst: NewTreeLocation,
        readonly: bool,
    ) -> layr_fs_core::Result<(CreatedTree, CaptureConsistency)> {
        let src = self.path(self.issuer.working_key(source)?)?;
        let dst = self.destination(dst)?;
        let consistency = backend(snapshot_native(&src, &dst, readonly))?;
        Ok((self.created(&dst)?, consistency))
    }
    pub(super) fn snapshot_path(src: &Path, dst: &Path, readonly: bool) -> Result<CaptureConsistency> {
        let driver = Arc::new(Self::default());
        let result = (|| -> layr_fs_core::Result<CaptureConsistency> {
            let source = driver.working(src)?;
            let destination = driver.location(dst)?;
            let filesystem: &dyn Filesystem = driver.as_ref();
            if readonly {
                Ok(filesystem.freeze(&source, destination)?.consistency)
            } else {
                let (_, consistency) = driver.copy_native(&source, destination, false)?;
                Ok(consistency)
            }
        })();
        result.map_err(|error| driver.error(error))
    }
    pub(super) fn remove_path(path: &Path) -> Result<()> {
        // Ordinary cleanup includes files and missing paths; those convey no managed-tree token.
        if !is_subvolume(path) {
            return delete_tree_native(path);
        }
        let driver = Arc::new(Self::default());
        let registered = driver.register(path)?;
        let ownership = driver.issuer.own();
        driver.trees()?.insert(driver.issuer.owned_key(&ownership)?, registered);
        driver.remove(ownership).map_err(|failure| {
            driver.error(FsError::Cleanup {
                cause: Box::new(FsError::Invalid("OverlayFS removal requires recovery")),
                failure: Box::new(failure),
            })
        })
    }
    pub(super) fn path_hints(&self, base: &Path, tree: &Path) -> layr_fs_core::Result<hints::HintSet> {
        // Preserve partial evidence for unmanaged pairs, as before the handle adapter.
        if backend(own_layers(base, tree))?.is_none() {
            return Ok(hints::HintSet::Partial);
        }
        let (base, tree) = (self.frozen(base)?, self.frozen(tree)?);
        match self.capabilities(&base.scope()).change_hints {
            Some(hints) => hints.between(&base, &tree),
            None => Ok(hints::HintSet::Partial),
        }
    }
    fn error(self: Arc<Self>, error: FsError) -> anyhow::Error {
        // Retain the driver together with cleanup authority across the path-based boundary.
        match error {
            FsError::Cleanup { cause, failure } => {
                anyhow::Error::new(Cleanup { _driver: self, cause: *cause, failure: *failure })
            }
            error => anyhow::Error::new(error),
        }
    }
}
struct Cleanup {
    _driver: Arc<OverlayFs>,
    cause: FsError,
    failure: RemovalFailure,
}
impl std::fmt::Debug for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_error(f)
    }
}
impl Cleanup {
    fn fmt_error(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; cleanup requires retry: {}", self.cause, self.failure.error)
    }
}
impl std::fmt::Display for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_error(f)
    }
}
impl std::error::Error for Cleanup {}
impl Filesystem for OverlayFs {
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        let dst = self.destination(dst)?;
        backend(create_subvolume(&dst))?;
        let created = self.created(&dst)?;
        if let Err(cause) = layr_fs_common::copy_tree(source, created.tree.writer()) {
            drop(created.tree);
            return match self.remove(created.ownership) {
                Ok(()) => Err(cause),
                Err(failure) => Err(FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) }),
            };
        }
        Ok(created)
    }
    fn freeze(&self, source: &WorkingTree, dst: NewTreeLocation) -> layr_fs_core::Result<Capture> {
        let (created, consistency) = self.copy_native(source, dst, true)?;
        self.issuer.seal(created, consistency)
    }
    fn changes(&self, before: &FrozenTree, after: &FrozenTree) -> layr_fs_core::Result<Vec<evidence::EntryChange>> {
        self.issuer.frozen_key(before)?;
        self.issuer.frozen_key(after)?;
        layr_fs_common::changes(self, before, after)
    }
    fn remove(&self, ownership: OwnedTree) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(ownership, |key| {
            let mut trees = self.trees()?;
            let tree = trees.get(key).ok_or(FsError::Invalid("tree absent"))?;
            tree.validate()?;
            backend(delete_tree_native(&tree.path))?;
            trees.remove(key);
            Ok(())
        })
    }
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_> {
        if self.issuer.scope_key(scope).ok().and_then(|key| self.trees().ok().map(|trees| trees.contains_key(&key))) != Some(true)
        {
            return Capabilities::default();
        }
        Capabilities { change_hints: Some(self), clone_new: None, reflink_range: Some(&RangeClone) }
    }
}
impl hints::ChangeHints for OverlayFs {
    fn between(&self, base: &FrozenTree, tree: &FrozenTree) -> layr_fs_core::Result<hints::HintSet> {
        self.hints_with(base, tree, own_layers)
    }
}
impl OverlayFs {
    fn hints_with(
        &self,
        base: &FrozenTree,
        tree: &FrozenTree,
        layers: impl FnOnce(&Path, &Path) -> Result<Option<CandidateLayers>>,
    ) -> layr_fs_core::Result<hints::HintSet> {
        let (base, tree) = (self.path(self.issuer.frozen_key(base)?)?, self.path(self.issuer.frozen_key(tree)?)?);
        let Some(layers) = backend(layers(&base, &tree))? else { return Ok(hints::HintSet::Partial) };
        Ok(hints::HintSet::Complete(backend(candidates_in(&base, &tree, layers))?))
    }
}
pub(super) struct RangeClone;
impl ReflinkRange for RangeClone {
    fn clone_range(&self, source: &File, range: ByteRange, dst: &File, offset: u64) -> layr_fs_core::Result<Attempt<()>> {
        match sys::clone_range(source, range.offset(), range.length(), dst, offset) {
            Ok(()) => Ok(Attempt::Applied(())),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY)) => {
                Ok(Attempt::Unavailable(error))
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        driver: OverlayFs,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir().join(format!("overlay-driver-{}", crate::ids::new_id()));
            fs::create_dir(&root)?;
            Ok(Self { root, driver: OverlayFs::default() })
        }
        fn tree(&self, name: &str) -> Result<FrozenTree> {
            let path = self.root.join(name);
            fs::create_dir(&path)?;
            let registered = self.driver.register(&path)?;
            let tree = self.driver.issuer.import(registered.directory.clone());
            self.driver.trees()?.insert(self.driver.issuer.frozen_key(&tree)?, registered);
            Ok(tree)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn layered_hints_use_issued_handles_and_preserve_whiteouts() -> Result<()> {
        let fixture = Fixture::new()?;
        let (base, tree) = (fixture.tree("base")?, fixture.tree("tree")?);
        let upper = fixture.root.join("upper");
        fs::create_dir(&upper)?;
        for root in ["base", "tree"] {
            fs::write(fixture.root.join(root).join("edit"), root.as_bytes())?;
        }
        fs::write(fixture.root.join("base/gone"), b"removed")?;
        fs::write(upper.join("gone"), b"")?;
        fs::write(upper.join("edit"), b"tree")?;
        let hints = fixture
            .driver
            .hints_with(&base, &tree, |_, _| Ok(Some(CandidateLayers { dirs: vec![upper], nested: HashSet::new() })))?;
        let hints::HintSet::Complete(changes) = hints else { panic!("shared layers must supply complete hints") };
        assert_eq!(changes.iter().map(|c| c.path().as_str()).collect::<Vec<_>>(), ["edit", "gone"]);
        assert!(changes.iter().find(|c| c.path() == "gone").unwrap().new().is_none());
        let fallback = layr_fs_common::changes(&NoHints(&fixture.driver), &base, &tree)?;
        assert_eq!(
            changes.iter().map(|c| c.path().as_str()).collect::<Vec<_>>(),
            fallback.iter().map(|c| c.path().as_str()).collect::<Vec<_>>()
        );
        assert!(matches!(fixture.driver.hints_with(&base, &tree, |_, _| Ok(None))?, hints::HintSet::Partial));
        Ok(())
    }
    struct NoHints<'a>(&'a OverlayFs);
    impl Filesystem for NoHints<'_> {
        fn materialise(&self, s: &dyn TreeRead, d: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
            self.0.materialise(s, d)
        }
        fn freeze(&self, s: &WorkingTree, d: NewTreeLocation) -> layr_fs_core::Result<Capture> {
            self.0.freeze(s, d)
        }
        fn changes(&self, a: &FrozenTree, b: &FrozenTree) -> layr_fs_core::Result<Vec<evidence::EntryChange>> {
            layr_fs_common::changes(self, a, b)
        }
        fn remove(&self, t: OwnedTree) -> std::result::Result<(), RemovalFailure> {
            self.0.remove(t)
        }
        fn capabilities(&self, _: &TreeScope) -> Capabilities<'_> {
            Capabilities::default()
        }
    }
    #[test]
    fn foreign_handles_and_replaced_roots_cannot_authorize_hints() -> Result<()> {
        let fixture = Fixture::new()?;
        let (base, tree) = (fixture.tree("base")?, fixture.tree("tree")?);
        let other = OverlayFs::default();
        assert!(other.capabilities(&base.scope()).change_hints.is_none());
        assert!(matches!(other.changes(&base, &tree), Err(FsError::WrongInstance)));
        fs::rename(fixture.root.join("base"), fixture.root.join("held"))?;
        fs::create_dir(fixture.root.join("base"))?;
        let result = fixture.driver.hints_with(&base, &tree, |_, _| panic!("must reject before consulting layers"));
        assert!(matches!(result, Err(FsError::LocationChanged)));
        Ok(())
    }
    #[test]
    fn destinations_reject_parent_replacement_and_existing_entries() -> Result<()> {
        let fixture = Fixture::new()?;
        let parent = fixture.root.join("parent");
        fs::create_dir(&parent)?;
        let destination = fixture.driver.location(&parent.join("new"))?;
        fs::rename(&parent, fixture.root.join("old"))?;
        fs::create_dir(&parent)?;
        assert!(matches!(fixture.driver.destination(destination), Err(FsError::LocationChanged)));
        let destination = fixture.driver.location(&parent.join("new"))?;
        std::os::unix::fs::symlink("absent", parent.join("new"))?;
        assert!(fixture.driver.destination(destination).is_err());
        Ok(())
    }
    #[test]
    fn range_capability_matches_copy_without_changing_file_identity() -> Result<()> {
        use std::io::Write;
        let fixture = Fixture::new()?;
        let tree = fixture.tree("tree")?;
        let source = fixture.root.join("source");
        let target = fixture.root.join("target");
        fs::write(&source, vec![b'a'; 8192])?;
        fs::write(&target, vec![b'b'; 8192])?;
        let (src, mut dst) = (File::open(source)?, fs::OpenOptions::new().write(true).open(&target)?);
        let before = dst.metadata()?.ino();
        let caps = fixture.driver.capabilities(&tree.scope());
        assert!(caps.clone_new.is_none());
        match caps.reflink_range.unwrap().clone_range(&src, ByteRange::new(0, 4096)?, &dst, 0)? {
            Attempt::Applied(()) => {}
            Attempt::Unavailable(_) => {
                assert_eq!(fs::read(&target)?, vec![b'b'; 8192]);
                dst.write_all(&vec![b'a'; 4096])?;
            }
        }
        assert_eq!(fs::read(&target)?, [vec![b'a'; 4096], vec![b'b'; 4096]].concat());
        assert_eq!(before, dst.metadata()?.ino());
        Ok(())
    }
}
