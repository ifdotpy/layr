use crate::{FsError, RelPath, Result, TreeRead, TreeWrite};
use std::sync::{Arc, Weak};

/// The trusted driver's issuer. One issuer per instance; no process-global registry or IDs.
#[derive(Default)]
pub struct TreeIssuer {
    instance: Arc<()>,
}

/// Opaque, non-reused identity retained by the driver's resource registry.
#[derive(Clone)]
pub struct TreeKey(Arc<Weak<Lease>>);
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
impl TreeScope {
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.lease.instance, &other.lease.instance)
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
impl OwnedTree {
    pub fn scope(&self) -> TreeScope {
        TreeScope { lease: self.lease.clone() }
    }
}
impl NewTreeLocation {
    pub fn scope(&self) -> TreeScope {
        TreeScope { lease: Arc::new(Lease { instance: self.instance.clone(), key: self.key.clone() }) }
    }
    pub fn name(&self) -> &RelPath {
        &self.name
    }
}
impl TreeIssuer {
    fn lease(&self) -> Arc<Lease> {
        Arc::new_cyclic(|lease| Lease { instance: self.instance.clone(), key: TreeKey(Arc::new(lease.clone())) })
    }
    /// Borrowed registrations may release native descriptors after the last handle or scope
    /// disappears. Owned resources remain governed by their distinct deletion authority.
    pub fn has_handles(&self, key: &TreeKey) -> bool {
        key.0.upgrade().is_some_and(|lease| Arc::ptr_eq(&lease.instance, &self.instance))
    }
    pub fn create(&self, writer: Arc<dyn TreeWrite>) -> CreatedTree {
        let lease = self.lease();
        CreatedTree { tree: WorkingTree { lease: lease.clone(), writer }, ownership: OwnedTree { lease } }
    }
    pub fn capture(&self, reader: Arc<dyn TreeRead>, consistency: crate::CaptureConsistency) -> crate::Capture {
        let lease = self.lease();
        crate::Capture { tree: FrozenTree { lease: lease.clone(), reader }, consistency, ownership: OwnedTree { lease } }
    }
    /// The driver has completed a private copy and relinquishes all writable handles.
    pub fn seal(&self, created: CreatedTree, consistency: crate::CaptureConsistency) -> Result<crate::Capture> {
        self.check(&created.tree.lease.instance)?;
        Ok(crate::Capture {
            tree: FrozenTree { lease: created.tree.lease, reader: created.tree.writer },
            ownership: created.ownership,
            consistency,
        })
    }
    /// Trusted registration of an existing working tree grants mutation, never deletion.
    pub fn import_working(&self, writer: Arc<dyn TreeWrite>) -> WorkingTree {
        WorkingTree { lease: self.lease(), writer }
    }
    /// Trusted ownership transfer at an existing storage adapter's deletion boundary.
    pub fn own(&self) -> OwnedTree {
        OwnedTree { lease: self.lease() }
    }
    pub fn owned_key(&self, tree: &OwnedTree) -> Result<TreeKey> {
        self.check(&tree.lease.instance)?;
        Ok(tree.lease.key.clone())
    }
    /// Trusted registration of an existing tree conveys reading, never deletion authority.
    pub fn import(&self, reader: Arc<dyn TreeRead>) -> FrozenTree {
        FrozenTree { lease: self.lease(), reader }
    }
    pub fn location(&self, name: RelPath, parent: Arc<dyn Send + Sync>) -> Result<NewTreeLocation> {
        if !name.is_name() {
            return Err(FsError::Invalid("destination must be one child name"));
        }
        Ok(NewTreeLocation { instance: self.instance.clone(), key: TreeKey(Arc::new(Weak::new())), name, _parent: parent })
    }
    pub fn location_key(&self, dst: &NewTreeLocation) -> Result<TreeKey> {
        self.check(&dst.instance)?;
        Ok(dst.key.clone())
    }
    pub fn working_key(&self, tree: &WorkingTree) -> Result<TreeKey> {
        self.check(&tree.lease.instance)?;
        Ok(tree.lease.key.clone())
    }
    pub fn clone_source_key(&self, source: crate::CloneSource<'_>) -> Result<TreeKey> {
        match source {
            crate::CloneSource::Frozen(tree) => self.frozen_key(tree),
            crate::CloneSource::Working(tree) => self.working_key(tree),
        }
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
    /// Attach a readable/writable lease after an exclusively owned resource has been prepared.
    pub fn attach_working(&self, ownership: &OwnedTree, writer: Arc<dyn TreeWrite>) -> Result<WorkingTree> {
        self.check(&ownership.lease.instance)?;
        Ok(WorkingTree { lease: ownership.lease.clone(), writer })
    }
    pub fn attach_frozen(&self, ownership: &OwnedTree, reader: Arc<dyn TreeRead>) -> Result<FrozenTree> {
        self.check(&ownership.lease.instance)?;
        Ok(FrozenTree { lease: ownership.lease.clone(), reader })
    }
    /// An exchange retains both authorities on failure, including failures after the native
    /// namespace change. Read handles and retained scopes must release their pins first.
    pub fn exchange(
        &self,
        first: OwnedTree,
        second: OwnedTree,
        operation: impl FnOnce(&TreeKey, &TreeKey) -> std::result::Result<(), crate::ExchangeError>,
    ) -> std::result::Result<(OwnedTree, OwnedTree), crate::ExchangeFailure> {
        let checked = (|| -> Result<()> {
            self.check(&first.lease.instance)?;
            self.check(&second.lease.instance)?;
            if first.lease.key == second.lease.key {
                return Err(FsError::Invalid("exchange requires distinct trees"));
            }
            if Arc::strong_count(&first.lease) != 1 || Arc::strong_count(&second.lease) != 1 {
                return Err(FsError::Busy);
            }
            Ok(())
        })();
        match checked.map_err(crate::ExchangeError::from).and_then(|()| operation(&first.lease.key, &second.lease.key)) {
            Ok(()) => Ok((first, second)),
            Err(crate::ExchangeError { error, progress }) => Err(crate::ExchangeFailure { error, progress, first, second }),
        }
    }
    /// Native owned mutations use the same affinity and lease exclusion as deletion.
    pub fn with_exclusive<T>(&self, ownership: &OwnedTree, operation: impl FnOnce(&TreeKey) -> Result<T>) -> Result<T> {
        self.check(&ownership.lease.instance)?;
        if Arc::strong_count(&ownership.lease) != 1 {
            return Err(FsError::Busy);
        }
        operation(&ownership.lease.key)
    }
    /// The operation runs only after all readers have released their pin. Errors retain authority.
    pub fn remove(
        &self,
        ownership: OwnedTree,
        operation: impl FnOnce(&TreeKey) -> Result<()>,
    ) -> std::result::Result<(), RemovalFailure> {
        let result = self.with_exclusive(&ownership, operation);
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
        fn rename_new(&self, _: &RelPath, _: &RelPath) -> Result<()> {
            Err(FsError::Invalid("absent"))
        }
        fn remove_entry(&self, _: &RelPath) -> Result<()> {
            Ok(())
        }
        fn remove_empty_dir(&self, _: &RelPath) -> Result<bool> {
            Ok(true)
        }
        fn overwrite_file(&self, _: &RelPath, _: &mut dyn std::io::Read, _: i64) -> Result<crate::Attempt<()>> {
            Err(FsError::Invalid("absent"))
        }
        fn write_range(&self, _: &RelPath, _: u64, _: &[u8], _: i64) -> Result<()> {
            Err(FsError::Invalid("absent"))
        }
        fn set_metadata(&self, _: &RelDir, _: &EntryAttributes) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn registration_liveness_tracks_readers_scopes_and_authority_without_pinning_them() {
        let issuer = TreeIssuer::default();
        let other = TreeIssuer::default();
        let created = issuer.create(Arc::new(Empty));
        let key = issuer.working_key(&created.tree).unwrap();
        let scope = created.tree.scope();
        assert!(issuer.has_handles(&key));
        assert!(!other.has_handles(&key));
        drop(created.tree);
        drop(created.ownership);
        assert!(issuer.has_handles(&key));
        drop(scope);
        assert!(!issuer.has_handles(&key));
        let borrowed = issuer.import(Arc::new(Empty));
        let key = issuer.frozen_key(&borrowed).unwrap();
        assert!(issuer.has_handles(&key));
        drop(borrowed);
        assert!(!issuer.has_handles(&key));
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
    fn reserved_authority_attaches_readers_without_issuing_another_identity() {
        let issuer = TreeIssuer::default();
        let other = TreeIssuer::default();
        let ownership = issuer.own();
        assert!(matches!(other.attach_working(&ownership, Arc::new(Empty)), Err(FsError::WrongInstance)));
        let working = issuer.attach_working(&ownership, Arc::new(Empty)).unwrap();
        let frozen = issuer.attach_frozen(&ownership, Arc::new(Empty)).unwrap();
        assert!(issuer.working_key(&working).unwrap() == issuer.owned_key(&ownership).unwrap());
        assert!(issuer.frozen_key(&frozen).unwrap() == issuer.owned_key(&ownership).unwrap());
        let failure = issuer.remove(ownership, |_| panic!("attached readers pin deletion")).unwrap_err();
        assert!(matches!(failure.error, FsError::Busy));
        drop(working);
        drop(frozen);
        issuer.remove(failure.ownership, |_| Ok(())).unwrap();
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
