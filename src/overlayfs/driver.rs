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
pub(super) enum Publication {
    Untouched,
    Objects(PathBuf),
    Unpinned(PathBuf),
    Tree { home: PathBuf, identity: (u64, u64) },
}
impl Publication {
    pub(super) fn directory(&mut self, home: &Path, metadata: Option<&fs::Metadata>) {
        *self = match metadata {
            Some(metadata) => Self::Tree { home: home.to_path_buf(), identity: (metadata.dev(), metadata.ino()) },
            None => Self::Unpinned(home.to_path_buf()),
        };
    }
}

enum Resource {
    Published { destination: Destination, publication: Publication },
    Borrowed(Registered),
    Owned { tree: Registered, home: PathBuf, parent: Arc<File> },
    Removing(Removal),
}
struct HintPair {
    base: PathBuf,
    tree: PathBuf,
    layers: CandidateLayers,
}
struct Destination {
    path: PathBuf,
    parent: Arc<File>,
}
#[derive(Default)]
pub(crate) struct OverlayFs {
    issuer: TreeIssuer,
    trees: Mutex<HashMap<TreeKey, Resource>>,
    hint_pair: Option<HintPair>,
    #[cfg(test)]
    fail_registration: std::sync::atomic::AtomicBool,
    destinations: Mutex<HashMap<TreeKey, Destination>>,
}
impl OverlayFs {
    fn trees(&self) -> layr_fs_core::Result<std::sync::MutexGuard<'_, HashMap<TreeKey, Resource>>> {
        self.trees.lock().map_err(|_| FsError::Invalid("OverlayFS handle registry lock poisoned"))
    }
    fn register(&self, path: &Path) -> layr_fs_core::Result<Registered> {
        #[cfg(test)]
        if self.fail_registration.swap(false, std::sync::atomic::Ordering::Relaxed) {
            return Err(FsError::Invalid("injected registration failure"));
        }
        let path = backend(absolute(path))?;
        let directory = Arc::new(DirectoryTree::open(&path)?);
        Ok(Registered { path, directory })
    }
    fn working(&self, path: &Path) -> layr_fs_core::Result<WorkingTree> {
        backend(subvol_info(path))?;
        let registered = self.register(path)?;
        let tree = self.issuer.import_working(registered.directory.clone());
        self.trees()?.insert(self.issuer.working_key(&tree)?, Resource::Borrowed(registered));
        Ok(tree)
    }
    fn frozen(&self, path: &Path) -> layr_fs_core::Result<FrozenTree> {
        let registered = self.register(path)?;
        let tree = self.issuer.import(registered.directory.clone());
        self.trees()?.insert(self.issuer.frozen_key(&tree)?, Resource::Borrowed(registered));
        Ok(tree)
    }
    fn location(&self, path: &Path) -> layr_fs_core::Result<NewTreeLocation> {
        let path = backend(absolute(path))?;
        let parent = Arc::new(File::open(path.parent().ok_or(FsError::Invalid("destination has no parent"))?)?);
        let name = RelPath::new(path.file_name().ok_or(FsError::Invalid("destination has no name"))?.as_bytes())?;
        let location = self.issuer.location(name, parent.clone())?;
        self.destinations
            .lock()
            .map_err(|_| FsError::Invalid("OverlayFS destination registry lock poisoned"))?
            .insert(self.issuer.location_key(&location)?, Destination { path, parent });
        Ok(location)
    }
    fn destination(&self, location: NewTreeLocation) -> layr_fs_core::Result<Destination> {
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
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(dst),
            Err(error) => Err(error.into()),
            Ok(_) => Err(FsError::Invalid("destination already exists")),
        }
    }
    fn path(&self, key: TreeKey) -> layr_fs_core::Result<PathBuf> {
        let trees = self.trees()?;
        let tree = match trees.get(&key) {
            Some(Resource::Borrowed(tree) | Resource::Owned { tree, .. }) => tree,
            _ => return Err(FsError::Invalid("tree is not readable")),
        };
        tree.validate()?;
        Ok(tree.path.clone())
    }
    fn create_with(
        &self,
        dst: NewTreeLocation,
        create: impl FnOnce(&Path, &mut Publication) -> Result<CaptureConsistency>,
    ) -> layr_fs_core::Result<(CreatedTree, CaptureConsistency)> {
        let dst = self.destination(dst)?;
        // Reserve authority and the registry slot before the native operation can publish.
        let ownership = self.issuer.own();
        let key = self.issuer.owned_key(&ownership)?;
        let mut trees = self.trees()?;
        let mut publication = Publication::Untouched;
        let result = backend(create(&dst.path, &mut publication));
        let path = dst.path.clone();
        trees.insert(key.clone(), Resource::Published { destination: dst, publication });
        let consistency = match result {
            Ok(consistency) => consistency,
            Err(cause) => {
                drop(trees);
                return Err(self.rollback(ownership, cause));
            }
        };
        let result = self.register(&path).and_then(|registered| {
            let tree = self.issuer.attach_working(&ownership, registered.directory.clone())?;
            let Some(Resource::Published { destination, publication: Publication::Tree { home, identity } }) = trees.get(&key)
            else {
                return Err(FsError::Invalid("native creation did not publish its tree"));
            };
            let held = registered.directory.root_file().metadata()?;
            if (held.dev(), held.ino()) != *identity {
                return Err(FsError::LocationChanged);
            }
            registered.validate()?;
            let (home, parent) = (home.clone(), destination.parent.clone());
            trees.insert(key, Resource::Owned { tree: registered, home, parent });
            Ok(tree)
        });
        drop(trees);
        match result {
            Ok(tree) => Ok((CreatedTree { tree, ownership }, consistency)),
            Err(cause) => Err(self.rollback(ownership, cause)),
        }
    }
    fn rollback(&self, ownership: OwnedTree, cause: FsError) -> FsError {
        match self.remove(ownership) {
            Ok(()) => cause,
            Err(failure) => FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) },
        }
    }
    fn copy_native(
        &self,
        source: &WorkingTree,
        dst: NewTreeLocation,
        readonly: bool,
    ) -> layr_fs_core::Result<(CreatedTree, CaptureConsistency)> {
        let src = self.path(self.issuer.working_key(source)?)?;
        self.create_with(dst, |dst, publication| snapshot_native(&src, dst, readonly, publication))
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
    pub(super) fn create_path(dst: &Path) -> Result<()> {
        let driver = Arc::new(Self::default());
        let result = driver.location(dst).and_then(|dst| {
            driver.create_with(dst, |dst, publication| {
                create_subvolume_native(dst, publication)?;
                Ok(CaptureConsistency::UnsettledCopy)
            })
        });
        result.map(|_| ()).map_err(|error| driver.error(error))
    }
    pub(super) fn remove_path(path: &Path) -> Result<()> {
        // Ordinary cleanup includes files and missing paths; those convey no managed-tree token.
        if !is_subvolume(path) {
            return delete_tree_native(path);
        }
        let driver = Arc::new(Self::default());
        let registered = driver.register(path)?;
        let ownership = driver.issuer.own();
        driver.trees()?.insert(
            driver.issuer.owned_key(&ownership)?,
            Resource::Owned {
                tree: registered,
                home: home_of(path)?,
                parent: Arc::new(File::open(path.parent().ok_or(FsError::Invalid("tree has no parent"))?)?),
            },
        );
        driver.remove(ownership).map_err(|failure| {
            driver.error(FsError::Cleanup {
                cause: Box::new(FsError::Invalid("OverlayFS removal requires recovery")),
                failure: Box::new(failure),
            })
        })
    }
    pub(super) fn path_hints(base: &Path, tree: &Path) -> layr_fs_core::Result<hints::HintSet> {
        Self::path_hints_with(base, tree, own_layers)
    }
    fn path_hints_with(
        base: &Path,
        tree: &Path,
        load: impl FnOnce(&Path, &Path) -> Result<Option<CandidateLayers>>,
    ) -> layr_fs_core::Result<hints::HintSet> {
        let (base, tree) = (backend(absolute(base))?, backend(absolute(tree))?);
        let Some(layers) = backend(load(&base, &tree))? else { return Ok(hints::HintSet::Partial) };
        // One operation-local registry snapshot supplies both registrations and the capability.
        let driver = Self { hint_pair: Some(HintPair { base: base.clone(), tree: tree.clone(), layers }), ..Self::default() };
        let (base, tree) = (driver.frozen(&base)?, driver.frozen(&tree)?);
        match driver.capabilities(&base.scope()).change_hints {
            Some(hints) => hints.between(&base, &tree),
            None => Ok(hints::HintSet::Partial),
        }
    }
    fn error(self: Arc<Self>, error: FsError) -> anyhow::Error {
        // Retain the driver together with cleanup authority across the path-based boundary.
        match error {
            FsError::Cleanup { cause, failure } => anyhow::Error::new(Cleanup { driver: self, cause: *cause, failure: *failure }),
            error => anyhow::Error::new(error),
        }
    }
}
pub(crate) struct Cleanup {
    pub(crate) driver: Arc<OverlayFs>,
    cause: FsError,
    pub(crate) failure: RemovalFailure,
}
impl std::fmt::Debug for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_error(f)
    }
}
impl Cleanup {
    fn fmt_error(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; cleanup requires retry: {}", self.cause, self.failure.error)?;
        if let (Ok(key), Ok(trees)) = (self.driver.issuer.owned_key(&self.failure.ownership), self.driver.trees()) {
            let path = match trees.get(&key) {
                Some(Resource::Published { destination, .. }) => Some(&destination.path),
                Some(Resource::Owned { tree, .. } | Resource::Borrowed(tree)) => Some(&tree.path),
                Some(Resource::Removing(removal)) => Some(&removal.path),
                None => None,
            };
            if let Some(path) = path {
                write!(f, "; pending tree {}", path.display())?;
            }
        }
        Ok(())
    }
}
impl std::fmt::Display for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.fmt_error(f)
    }
}
impl std::error::Error for Cleanup {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}
impl Filesystem for OverlayFs {
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> layr_fs_core::Result<CreatedTree> {
        let (created, _) = self.create_with(dst, |dst, publication| {
            create_subvolume_native(dst, publication)?;
            Ok(CaptureConsistency::UnsettledCopy)
        })?;
        if let Err(cause) = layr_fs_common::copy_tree(source, created.tree.writer()) {
            drop(created.tree);
            return Err(self.rollback(created.ownership, cause));
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
        self.remove_with(ownership, Removal::run)
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
        let (base, tree) = (self.path(self.issuer.frozen_key(base)?)?, self.path(self.issuer.frozen_key(tree)?)?);
        let loaded;
        let layers = if let Some(pair) = &self.hint_pair {
            if pair.base != base || pair.tree != tree {
                return Ok(hints::HintSet::Partial);
            }
            &pair.layers
        } else {
            loaded = backend(own_layers(&base, &tree))?;
            let Some(layers) = &loaded else { return Ok(hints::HintSet::Partial) };
            layers
        };
        Ok(hints::HintSet::Complete(backend(candidates_in(&base, &tree, layers))?))
    }
}
impl OverlayFs {
    fn remove_with(
        &self,
        ownership: OwnedTree,
        remove: impl FnOnce(&mut Removal) -> Result<()>,
    ) -> std::result::Result<(), RemovalFailure> {
        self.issuer.remove(ownership, |key| {
            let mut trees = self.trees()?;
            let resource = trees.get_mut(key).ok_or(FsError::Invalid("tree absent"))?;
            match resource {
                Resource::Owned { tree, home, parent } => {
                    tree.validate()?;
                    let held = tree.directory.root_file().metadata()?;
                    let mut removal = Removal::new(tree.path.clone(), home.clone(), parent.clone());
                    removal.identity = RootIdentity::Expected(held.dev(), held.ino());
                    // No reader lease remains. Drop the mount descriptor before normal unmount.
                    *resource = Resource::Removing(removal);
                }
                Resource::Published { destination, publication } => match publication {
                    Publication::Untouched => {
                        trees.remove(key);
                        return Ok(());
                    }
                    Publication::Unpinned(home) => {
                        return Err(FsError::backend(anyhow::anyhow!(
                            "new directory could not be pinned; registry {} requires recovery",
                            home.display()
                        )));
                    }
                    Publication::Objects(home) => {
                        let mut removal = Removal::new(destination.path.clone(), home.clone(), destination.parent.clone());
                        removal.stage = RemovalStage::Collect;
                        *resource = Resource::Removing(removal);
                    }
                    Publication::Tree { home, identity } => {
                        let mut removal = Removal::new(destination.path.clone(), home.clone(), destination.parent.clone());
                        removal.identity = RootIdentity::Expected(identity.0, identity.1);
                        *resource = Resource::Removing(removal);
                    }
                },
                Resource::Borrowed(_) => return Err(FsError::Invalid("borrowed tree has no deletion authority")),
                Resource::Removing(_) => {}
            }
            let Resource::Removing(removal) = resource else { return Err(FsError::Invalid("tree still pinned")) };
            backend(remove(removal))?;
            trees.remove(key);
            Ok(())
        })
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
        driver: Arc<OverlayFs>,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir().join(format!("overlay-driver-{}", crate::ids::new_id()));
            fs::create_dir(&root)?;
            // Native rollback cannot open this fake registry lock, so tests exercise
            // recovery through retained ownership without requiring a root-owned store.
            fs::create_dir(root.join("lock"))?;
            Ok(Self { root, driver: Arc::new(OverlayFs::default()) })
        }
        fn tree(&self, name: &str) -> Result<FrozenTree> {
            let path = self.root.join(name);
            fs::create_dir(&path)?;
            let registered = self.driver.register(&path)?;
            let tree = self.driver.issuer.import(registered.directory.clone());
            self.driver.trees()?.insert(self.driver.issuer.frozen_key(&tree)?, Resource::Borrowed(registered));
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
        let loads = std::cell::Cell::new(0);
        let hints = OverlayFs::path_hints_with(&fixture.root.join("base"), &fixture.root.join("tree"), |base, tree| {
            loads.set(loads.get() + 1);
            assert!(base.is_absolute() && tree.is_absolute());
            Ok(Some(CandidateLayers { dirs: vec![upper], nested: HashSet::new() }))
        })?;
        assert_eq!(loads.get(), 1);
        let hints::HintSet::Complete(changes) = hints else { panic!("shared layers must supply complete hints") };
        assert_eq!(changes.iter().map(|c| c.path().as_str()).collect::<Vec<_>>(), ["edit", "gone"]);
        assert!(changes.iter().find(|c| c.path() == "gone").unwrap().new().is_none());
        let fallback = layr_fs_common::changes(&NoHints(&fixture.driver), &base, &tree)?;
        assert_eq!(
            changes.iter().map(|c| c.path().as_str()).collect::<Vec<_>>(),
            fallback.iter().map(|c| c.path().as_str()).collect::<Vec<_>>()
        );
        assert!(matches!(
            OverlayFs::path_hints_with(&fixture.root.join("base"), &fixture.root.join("tree"), |_, _| Ok(None))?,
            hints::HintSet::Partial
        ));
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
        let result = hints::ChangeHints::between(fixture.driver.as_ref(), &base, &tree);
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
    fn registration_failure_retains_cleanup_authority() -> Result<()> {
        use std::sync::atomic::Ordering;
        let fixture = Fixture::new()?;
        let destination = fixture.root.join("new");
        let location = fixture.driver.location(&destination)?;
        fixture.driver.fail_registration.store(true, Ordering::Relaxed);
        let error = fixture
            .driver
            .create_with(location, |path, publication| {
                fs::create_dir(path)?;
                let metadata = fs::symlink_metadata(path)?;
                *publication = Publication::Tree { home: fixture.root.clone(), identity: (metadata.dev(), metadata.ino()) };
                Ok(CaptureConsistency::ValidatedCopy)
            })
            .err()
            .unwrap();
        // A plain fixture has no privileged registry. Failed native cleanup must retain
        // the reserved ownership so the operation can recover without a mounted filesystem.
        let FsError::Cleanup { cause, failure } = error else { panic!("cleanup authority was lost") };
        assert!(cause.to_string().contains("injected registration"));
        fixture
            .driver
            .remove_with(failure.ownership, |removal| {
                assert_eq!(removal.path, destination);
                fs::remove_dir(&removal.path)?;
                Ok(())
            })
            .unwrap();
        assert!(!destination.exists());
        assert!(fixture.driver.trees()?.is_empty());
        Ok(())
    }
    #[test]
    fn removal_drops_the_mount_pin_before_unmount_and_retains_retry() -> Result<()> {
        let fixture = Fixture::new()?;
        let location = fixture.driver.location(&fixture.root.join("new"))?;
        let (created, _) = fixture.driver.create_with(location, |path, publication| {
            fs::create_dir(path)?;
            let metadata = fs::symlink_metadata(path)?;
            *publication = Publication::Tree { home: fixture.root.clone(), identity: (metadata.dev(), metadata.ino()) };
            Ok(CaptureConsistency::ValidatedCopy)
        })?;
        let key = fixture.driver.issuer.owned_key(&created.ownership)?;
        let pin = {
            let trees = fixture.driver.trees()?;
            let Resource::Owned { tree, .. } = trees.get(&key).unwrap() else { panic!("tree must be pinned") };
            Arc::downgrade(&tree.directory)
        };
        drop(created.tree);
        let failure = fixture
            .driver
            .remove_with(created.ownership, |_| {
                assert!(pin.upgrade().is_none(), "normal unmount must hold no tree descriptor");
                anyhow::bail!("injected unmount failure")
            })
            .unwrap_err();
        fixture
            .driver
            .remove_with(failure.ownership, |removal| {
                fs::remove_dir(&removal.path)?;
                Ok(())
            })
            .unwrap();
        Ok(())
    }
    #[test]
    fn creation_error_after_publication_retains_the_destination() -> Result<()> {
        let fixture = Fixture::new()?;
        let destination = fixture.root.join("new");
        let location = fixture.driver.location(&destination)?;
        let error = fixture
            .driver
            .create_with(location, |path, publication| {
                fs::create_dir(path)?;
                let metadata = fs::symlink_metadata(path)?;
                *publication = Publication::Tree { home: fixture.root.clone(), identity: (metadata.dev(), metadata.ino()) };
                anyhow::bail!("injected registry save failure")
            })
            .err()
            .unwrap();
        let FsError::Cleanup { cause, failure } = error else { panic!("creation lost its cleanup token") };
        assert!(cause.to_string().contains("injected registry save"));
        fixture
            .driver
            .remove_with(failure.ownership, |removal| {
                fs::remove_dir(&removal.path)?;
                removal.directory_removed();
                Ok(())
            })
            .unwrap();
        assert!(!destination.exists());
        Ok(())
    }
    #[test]
    fn detected_cache_replacement_survives_public_error_recovery() -> Result<()> {
        let fixture = Fixture::new()?;
        let destination = fixture.root.join("new");
        let held = fixture.root.join("held");
        let location = fixture.driver.location(&destination)?;
        let core_error = fixture
            .driver
            .create_with(location, |path, publication| {
                fs::create_dir(path)?;
                publication.directory(&fixture.root, Some(&fs::symlink_metadata(path)?));
                // Match the cache-copy boundary: its retained descriptor detects a replacement
                // after copying, then returns an error. Cleanup must not adopt the new directory.
                fs::rename(path, &held)?;
                fs::create_dir(path)?;
                fs::write(path.join("foreign"), b"keep")?;
                anyhow::bail!("cache destination moved during restore")
            })
            .err()
            .unwrap();
        let error = fixture.driver.clone().error(core_error);
        let Cleanup { driver, failure, .. } = error.downcast::<Cleanup>()?;
        fs::remove_dir(fixture.root.join("lock"))?;
        fs::create_dir(fixture.root.join("objects"))?;
        let failure = driver.remove(failure.ownership).unwrap_err();
        assert!(failure.error.to_string().contains("owned tree location changed"));
        assert_eq!(fs::read(destination.join("foreign"))?, b"keep");
        fs::remove_dir_all(&destination)?;
        fs::rename(held, &destination)?;
        driver.remove(failure.ownership).unwrap();
        assert!(!destination.exists());
        Ok(())
    }
    #[test]
    fn descriptor_acquisition_failure_never_adopts_an_unobserved_directory() -> Result<()> {
        let fixture = Fixture::new()?;
        let destination = fixture.root.join("new");
        let error = fixture
            .driver
            .create_with(fixture.driver.location(&destination)?, |path, publication| {
                fs::create_dir(path)?;
                publication.directory(&fixture.root, None);
                anyhow::bail!("injected descriptor acquisition failure")
            })
            .err()
            .unwrap();
        let FsError::Cleanup { failure, .. } = error else { panic!("must retain uncertain directory authority") };
        assert!(failure.error.to_string().contains("could not be pinned"));
        assert!(destination.is_dir());
        Ok(())
    }
    #[test]
    fn removal_retries_after_unmount_and_after_directory_removal() -> Result<()> {
        let fixture = Fixture::new()?;
        let destination = fixture.root.join("new");
        let location = fixture.driver.location(&destination)?;
        let (created, _) = fixture.driver.create_with(location, |path, publication| {
            fs::create_dir(path)?;
            let metadata = fs::symlink_metadata(path)?;
            *publication = Publication::Tree { home: fixture.root.clone(), identity: (metadata.dev(), metadata.ino()) };
            Ok(CaptureConsistency::ValidatedCopy)
        })?;
        drop(created.tree);
        let failure = fixture
            .driver
            .remove_with(created.ownership, |removal| {
                removal.validate()?;
                // A normal unmount exposes the original mountpoint, with another inode/device.
                fs::rename(&removal.path, fixture.root.join("old-mount"))?;
                fs::create_dir(&removal.path)?;
                removal.after_unmount()?;
                anyhow::bail!("injected registry persistence failure after unmount")
            })
            .unwrap_err();
        let failure = fixture
            .driver
            .remove_with(failure.ownership, |removal| {
                removal.validate()?;
                fs::remove_dir(&removal.path)?;
                removal.directory_removed();
                anyhow::bail!("injected collection failure after directory removal")
            })
            .unwrap_err();
        fixture
            .driver
            .remove_with(failure.ownership, |removal| {
                removal.validate()?;
                assert!(matches!(removal.stage, RemovalStage::Collect));
                assert!(!removal.path.exists());
                Ok(())
            })
            .unwrap();
        assert!(fixture.driver.trees()?.is_empty());
        Ok(())
    }
    #[test]
    fn relative_hint_adapter_normalizes_before_its_only_registry_load() -> Result<()> {
        let root = PathBuf::from(format!("target/overlay-driver-relative-{}", crate::ids::new_id()));
        fs::create_dir_all(root.join("base"))?;
        fs::create_dir(root.join("tree"))?;
        let loads = std::cell::Cell::new(0);
        let result = OverlayFs::path_hints_with(&root.join("base"), &root.join("tree"), |base, tree| {
            loads.set(loads.get() + 1);
            assert_eq!(base, absolute(&root.join("base"))?);
            assert_eq!(tree, absolute(&root.join("tree"))?);
            Ok(Some(CandidateLayers { dirs: Vec::new(), nested: HashSet::new() }))
        });
        fs::remove_dir_all(root)?;
        assert_eq!(loads.get(), 1);
        assert!(matches!(result?, hints::HintSet::Complete(entries) if entries.is_empty()));
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
