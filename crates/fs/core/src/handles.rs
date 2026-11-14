use crate::{FsError, RelPath, Result, TreeRead, TreeWrite};
use std::sync::Arc;

/// The trusted driver's issuer. One issuer per instance; no process-global registry or IDs.
#[derive(Default)]
pub struct TreeIssuer {
    instance: Arc<()>,
}

/// Opaque, non-reused identity retained by the driver's resource registry.
#[derive(Clone)]
pub struct TreeKey(Arc<()>);
impl PartialEq for TreeKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for TreeKey {}
impl std::hash::Hash for TreeKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}
struct Lease {
    instance: Arc<()>,
    key: TreeKey,
}
#[derive(Clone)]
pub struct FrozenTree {
    lease: Arc<Lease>,
    reader: Arc<dyn TreeRead>,
}
pub struct WorkingTree {
    lease: Arc<Lease>,
    writer: Arc<dyn TreeWrite>,
}
/// Deletion authority. Read handles and imported user trees cannot mint this token.
#[must_use]
pub struct OwnedTree {
    lease: Arc<Lease>,
}
#[must_use]
pub struct CreatedTree {
    pub tree: WorkingTree,
    pub ownership: OwnedTree,
}

/// A checked single child name and a retained parent lease, issued by the destination's driver.
#[must_use]
pub struct NewTreeLocation {
    instance: Arc<()>,
    key: TreeKey,
    name: RelPath,
    _parent: Arc<dyn Send + Sync>,
}
pub struct TreeScope {
    lease: Arc<Lease>,
}
#[must_use]
pub struct RemovalFailure {
    pub error: FsError,
    pub ownership: OwnedTree,
}
impl std::fmt::Debug for RemovalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl FrozenTree {
    pub fn reader(&self) -> &dyn TreeRead {
        self.reader.as_ref()
    }
    pub fn scope(&self) -> TreeScope {
        TreeScope { lease: self.lease.clone() }
    }
}
impl WorkingTree {
    pub fn reader(&self) -> &dyn TreeRead {
        self.writer.as_ref()
    }
    pub fn writer(&self) -> &dyn TreeWrite {
        self.writer.as_ref()
    }
    pub fn scope(&self) -> TreeScope {
        TreeScope { lease: self.lease.clone() }
    }
}
impl NewTreeLocation {
    pub fn name(&self) -> &RelPath {
        &self.name
    }
}
impl TreeIssuer {
    fn lease(&self) -> Arc<Lease> {
        Arc::new(Lease { instance: self.instance.clone(), key: TreeKey(Arc::new(())) })
    }
    pub fn create(&self, writer: Arc<dyn TreeWrite>) -> CreatedTree {
        let lease = self.lease();
        CreatedTree { tree: WorkingTree { lease: lease.clone(), writer }, ownership: OwnedTree { lease } }
    }
    pub fn capture(&self, reader: Arc<dyn TreeRead>, consistency: crate::CaptureConsistency) -> crate::Capture {
        let lease = self.lease();
        crate::Capture { tree: FrozenTree { lease: lease.clone(), reader }, consistency, ownership: OwnedTree { lease } }
    }
    /// Trusted registration of an existing tree conveys reading, never deletion authority.
    pub fn import(&self, reader: Arc<dyn TreeRead>) -> FrozenTree {
        FrozenTree { lease: self.lease(), reader }
    }
    pub fn location(&self, name: RelPath, parent: Arc<dyn Send + Sync>) -> Result<NewTreeLocation> {
        if !name.is_name() {
            return Err(FsError::Invalid("destination must be one child name"));
        }
        Ok(NewTreeLocation { instance: self.instance.clone(), key: TreeKey(Arc::new(())), name, _parent: parent })
    }
    pub fn location_key(&self, dst: &NewTreeLocation) -> Result<TreeKey> {
        self.check(&dst.instance)?;
        Ok(dst.key.clone())
    }
    pub fn working_key(&self, tree: &WorkingTree) -> Result<TreeKey> {
        self.check(&tree.lease.instance)?;
        Ok(tree.lease.key.clone())
    }
    pub fn frozen_key(&self, tree: &FrozenTree) -> Result<TreeKey> {
        self.check(&tree.lease.instance)?;
        Ok(tree.lease.key.clone())
    }
    pub fn scope_key(&self, scope: &TreeScope) -> Result<TreeKey> {
        self.check(&scope.lease.instance)?;
        Ok(scope.lease.key.clone())
    }
    fn check(&self, instance: &Arc<()>) -> Result<()> {
        if Arc::ptr_eq(&self.instance, instance) {
            Ok(())
        } else {
            Err(FsError::WrongInstance)
        }
    }
    /// The operation runs only after all readers have released their pin. Errors retain authority.
    pub fn remove(
        &self,
        ownership: OwnedTree,
        operation: impl FnOnce(&TreeKey) -> Result<()>,
    ) -> std::result::Result<(), RemovalFailure> {
        let result = self.check(&ownership.lease.instance).and_then(|()| {
            if Arc::strong_count(&ownership.lease) != 1 {
                return Err(FsError::Busy);
            }
            operation(&ownership.lease.key)
        });
        result.map_err(|error| RemovalFailure { error, ownership })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryAttributes, EntryData, EntryMetadata, ReadSeek, RelDir};
    struct Empty;
    impl TreeRead for Empty {
        fn metadata(&self, _: &RelDir) -> Result<Option<EntryMetadata>> {
            Ok(None)
        }
        fn entries(&self, _: &RelDir, _: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()> {
            Ok(())
        }
        fn open_file(&self, _: &RelPath) -> Result<Box<dyn ReadSeek + '_>> {
            Err(FsError::Invalid("absent"))
        }
        fn read_link(&self, _: &RelPath) -> Result<Vec<u8>> {
            Err(FsError::Invalid("absent"))
        }
    }
    impl TreeWrite for Empty {
        fn put(&self, _: &RelPath, _: EntryData<'_>, _: &EntryAttributes) -> Result<()> {
            Ok(())
        }
        fn remove_entry(&self, _: &RelPath) -> Result<()> {
            Ok(())
        }
        fn set_metadata(&self, _: &RelDir, _: &EntryAttributes) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn authority_survives_busy_wrong_instance_and_io_failure() {
        let issuer = TreeIssuer::default();
        let other = TreeIssuer::default();
        let CreatedTree { tree, ownership } = issuer.create(Arc::new(Empty));
        assert!(matches!(other.working_key(&tree), Err(FsError::WrongInstance)));
        let ownership = issuer.remove(ownership, |_| panic!("reader is pinned")).unwrap_err().ownership;
        drop(tree);
        let ownership = other.remove(ownership, |_| panic!("wrong instance")).unwrap_err().ownership;
        let failed = issuer.remove(ownership, |_| Err(FsError::Invalid("injected remove failure"))).unwrap_err();
        assert!(matches!(failed.error, FsError::Invalid(_)));
        issuer.remove(failed.ownership, |_| Ok(())).unwrap();
    }
    #[test]
    fn scopes_retain_distinct_tree_identity_and_reject_other_instances() {
        let issuer = TreeIssuer::default();
        let a = issuer.import(Arc::new(Empty));
        let b = issuer.import(Arc::new(Empty));
        assert!(issuer.scope_key(&a.scope()).unwrap() != issuer.scope_key(&b.scope()).unwrap());
        assert!(matches!(TreeIssuer::default().scope_key(&a.scope()), Err(FsError::WrongInstance)));
    }
    #[test]
    fn frozen_clones_pin_storage_and_locations_reject_other_instances() {
        let issuer = TreeIssuer::default();
        let capture = issuer.capture(Arc::new(Empty), crate::CaptureConsistency::AtomicPoint);
        let clone = capture.tree.clone();
        drop(capture.tree);
        let failure = issuer.remove(capture.ownership, |_| panic!("clone pins storage")).unwrap_err();
        drop(clone);
        issuer.remove(failure.ownership, |_| Ok(())).unwrap();
        assert!(issuer.location(RelPath::new(b"a/b".to_vec()).unwrap(), Arc::new(())).is_err());
        let location = issuer.location(RelPath::new(b"new".to_vec()).unwrap(), Arc::new(())).unwrap();
        assert!(TreeIssuer::default().location_key(&location).is_err());
    }
}
