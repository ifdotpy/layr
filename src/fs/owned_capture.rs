use super::*;

pub(crate) struct ResourceOwner {
    driver: Arc<dyn PathAdmission>,
    ownership: Option<OwnedTree>,
}
impl ResourceOwner {
    pub(crate) fn release(mut self) {
        self.ownership.take();
    }
    pub(crate) fn abort(mut self, cause: anyhow::Error) -> anyhow::Error {
        let Some(ownership) = self.ownership.take() else { return cause };
        match self.driver.remove(ownership) {
            Ok(()) => cause,
            Err(failure) => anyhow::Error::new(Cleanup::after(self.driver.clone(), failure, cause)),
        }
    }
    pub(crate) fn discard(mut self) -> Result<()> {
        if let Some(ownership) = self.ownership.take() {
            self.driver.remove(ownership).map_err(|failure| {
                operation_error(
                    self.driver.clone(),
                    FsError::Cleanup {
                        cause: Box::new(FsError::Invalid("unpublished tree cleanup failed")),
                        failure: Box::new(failure),
                    },
                )
            })?;
        }
        Ok(())
    }
}
impl Drop for ResourceOwner {
    fn drop(&mut self) {
        if let Some(ownership) = self.ownership.take() {
            if let Err(failure) = self.driver.remove(ownership) {
                let error = operation_error(
                    self.driver.clone(),
                    FsError::Cleanup {
                        cause: Box::new(FsError::Invalid("unpublished tree cleanup failed")),
                        failure: Box::new(failure),
                    },
                );
                eprintln!("warning: {error:#}");
            }
        }
    }
}
/// Fields drop in declaration order: the readable lease is released before its owner removes
/// an unpublished tree. Explicit release transfers authority to the persistent application.
pub(crate) struct OwnedCapture {
    tree: FrozenTree,
    consistency: CaptureConsistency,
    owner: ResourceOwner,
}
impl OwnedCapture {
    pub(crate) fn new(driver: Arc<dyn PathAdmission>, capture: Capture) -> Self {
        driver.release_unused();
        let Capture { tree, consistency, ownership } = capture;
        Self { tree, consistency, owner: ResourceOwner { driver, ownership: Some(ownership) } }
    }
    pub(crate) fn received_identity(self, uuid: &str, transid: u64) -> Result<ResourceOwner> {
        let Self { tree, owner, .. } = self;
        drop(tree);
        owner.driver.release_unused();
        let result = owner
            .ownership
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("capture authority already transferred"))
            .and_then(|ownership| owner.driver.received_identity(ownership, uuid, transid).map_err(Into::into));
        match result {
            Ok(()) => Ok(owner),
            Err(error) => Err(owner.abort(error)),
        }
    }
    pub(crate) fn consistency(&self) -> CaptureConsistency {
        self.consistency
    }
    pub(crate) fn release(self) {
        let Self { tree, owner, .. } = self;
        drop(tree);
        owner.release();
    }
    pub(crate) fn abort(self, cause: anyhow::Error) -> anyhow::Error {
        let Self { tree, owner, .. } = self;
        drop(tree);
        owner.abort(cause)
    }
    pub(crate) fn discard(self) -> Result<()> {
        let Self { tree, owner, .. } = self;
        drop(tree);
        owner.discard()
    }
}
pub(crate) struct OwnedWorking {
    tree: WorkingTree,
    owner: ResourceOwner,
}
impl OwnedWorking {
    pub(crate) fn abort(self, cause: anyhow::Error) -> anyhow::Error {
        let Self { tree, owner } = self;
        drop(tree);
        owner.abort(cause)
    }
    pub(crate) fn discard(self) -> Result<()> {
        let Self { tree, owner } = self;
        drop(tree);
        owner.discard()
    }
    pub(crate) fn new(driver: Arc<dyn PathAdmission>, created: CreatedTree) -> Self {
        driver.release_unused();
        let CreatedTree { tree, ownership } = created;
        Self { tree, owner: ResourceOwner { driver, ownership: Some(ownership) } }
    }
    pub(crate) fn release(self) {
        let Self { tree, owner } = self;
        drop(tree);
        owner.release();
    }
}
pub(crate) fn freeze_source(source: &crate::source::Source, destination: &Path) -> Result<OwnedCapture> {
    let driver = composition::operation(source.folder(), destination, CapturePurpose::State)?;
    let result = (|| -> layr_fs_core::Result<Capture> {
        let location = driver.location(destination)?;
        if let Some(path) = source.folder() {
            let working = driver.working(path)?;
            driver.freeze(FreezeSource::Working(&working), location)
        } else {
            driver.freeze(FreezeSource::Reader(source), location)
        }
    })();
    driver.release_unused();
    let mut capture = result.map_err(|error| operation_error(driver.clone(), error))?;
    // Verified object trees are immutable logical inputs. Copying their checked bytes is
    // validated even when the driver cannot infer that provenance from TreeRead alone.
    if source.objects().is_some() {
        capture.consistency = CaptureConsistency::ValidatedCopy;
    }
    Ok(OwnedCapture::new(driver, capture))
}
pub(crate) fn materialise_owned(source: &crate::source::Source, destination: &Path) -> Result<OwnedWorking> {
    let driver = composition::operation(source.folder(), destination, CapturePurpose::State)?;
    let result = (|| -> layr_fs_core::Result<CreatedTree> {
        let location = driver.location(destination)?;
        driver.materialise(source, location)
    })();
    let created = result.map_err(|error| operation_error(driver.clone(), error))?;
    Ok(OwnedWorking::new(driver, created))
}

/// The namespace may already have changed, so recovery keeps both identities and the exact
/// driver. Only a verified completion transfers the new index and removes the old one.
#[cfg(any(target_os = "macos", test))]
pub(crate) struct ExchangeRecovery {
    _driver: Arc<dyn PathAdmission>,
    phase: ExchangeRecoveryPhase,
}
#[cfg(any(target_os = "macos", test))]
enum ExchangeRecoveryPhase {
    Exchange(ExchangeFailure),
    #[cfg(test)]
    Discard(RemovalFailure),
}
#[cfg(any(target_os = "macos", test))]
impl std::fmt::Debug for ExchangeRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
#[cfg(any(target_os = "macos", test))]
impl std::fmt::Display for ExchangeRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let error = match &self.phase {
            ExchangeRecoveryPhase::Exchange(failure) => &failure.error,
            #[cfg(test)]
            ExchangeRecoveryPhase::Discard(failure) => &failure.error,
        };
        write!(f, "index replacement requires recovery: {error}")
    }
}
#[cfg(any(target_os = "macos", test))]
impl std::error::Error for ExchangeRecovery {}
#[cfg(test)]
impl ExchangeRecovery {
    pub(crate) fn retry(self) -> std::result::Result<(), Self> {
        let Self { _driver: driver, phase } = self;
        let ownership = match phase {
            ExchangeRecoveryPhase::Discard(failure) => failure.ownership,
            ExchangeRecoveryPhase::Exchange(failure) => {
                let capability = {
                    let scope = failure.first.scope();
                    driver.capabilities(&scope).exchange
                };
                let Some(exchange) = capability else {
                    return Err(Self { _driver: driver, phase: ExchangeRecoveryPhase::Exchange(failure) });
                };
                match exchange.exchange(failure.first, failure.second) {
                    Ok((published, obsolete)) => {
                        drop(published);
                        obsolete
                    }
                    Err(mut failure) => {
                        failure.progress = ExchangeProgress::Swapped;
                        return Err(Self { _driver: driver, phase: ExchangeRecoveryPhase::Exchange(failure) });
                    }
                }
            }
        };
        driver.remove(ownership).map_err(|failure| Self { _driver: driver, phase: ExchangeRecoveryPhase::Discard(failure) })
    }
}
#[cfg(any(target_os = "macos", test))]
impl OwnedWorking {
    pub(crate) fn exchange_existing(self, destination: ManagedPath) -> Result<()> {
        let Self { tree, mut owner } = self;
        drop(tree);
        let second = owner.driver.claim(destination)?;
        let Some(first) = owner.ownership.take() else {
            return Err(FsError::Invalid("creation ownership already transferred").into());
        };
        let driver = owner.driver.clone();
        let capability = {
            let scope = first.scope();
            driver.capabilities(&scope).exchange
        };
        let Some(exchange) = capability else {
            drop(second);
            let cause = FsError::Invalid("filesystem cannot atomically exchange owned trees");
            return match driver.remove(first) {
                Ok(()) => Err(cause.into()),
                Err(failure) => {
                    Err(operation_error(driver, FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) }))
                }
            };
        };
        match exchange.exchange(first, second) {
            Ok((published, obsolete)) => {
                drop(published);
                driver.remove(obsolete).map_err(|failure| {
                    operation_error(
                        driver,
                        FsError::Cleanup {
                            cause: Box::new(FsError::Invalid("obsolete index cleanup failed")),
                            failure: Box::new(failure),
                        },
                    )
                })
            }
            Err(failure) if failure.progress == ExchangeProgress::Unchanged => {
                let ExchangeFailure { error, first, second, .. } = failure;
                drop(second);
                match driver.remove(first) {
                    Ok(()) => Err(error.into()),
                    Err(failure) => {
                        Err(operation_error(driver, FsError::Cleanup { cause: Box::new(error), failure: Box::new(failure) }))
                    }
                }
            }
            Err(failure) => {
                Err(anyhow::Error::new(ExchangeRecovery { _driver: driver, phase: ExchangeRecoveryPhase::Exchange(failure) }))
            }
        }
    }
}

pub(crate) fn copy_capture_owned(
    source: &Path,
    destination: &Path,
    purpose: CapturePurpose,
) -> Result<(OwnedWorking, OwnerLayout)> {
    let driver = composition::operation(Some(source), destination, purpose)?;
    let mut guards = Vec::new();
    let result = (|| -> layr_fs_core::Result<WritableCapture> {
        let working = driver.working(source)?;
        let location = driver.location(destination)?;
        layr_fs_common::copy_capture(driver.as_ref(), &working, location, &mut || {
            let scratch = composition::capture_location(destination)?;
            let location = driver.location(&scratch.path)?;
            guards.push(scratch);
            Ok(location)
        })
    })();
    driver.release_unused();
    let copied = result.map_err(|error| retained_error(driver.clone(), error, guards))?;
    if copied.consistency.running() {
        crate::fsutil::mark_unsettled(source);
    }
    Ok((OwnedWorking::new(driver, copied.created), copied.owners))
}

pub(crate) fn retain_owned(path: &Path) -> Result<ResourceOwner> {
    let driver = composition::operation(None, path, CapturePurpose::State)?;
    let ownership = driver.claim(ManagedPath::claim(path))?;
    Ok(ResourceOwner { driver, ownership: Some(ownership) })
}

#[cfg(all(test, target_os = "linux", not(feature = "overlayfs")))]
mod transport_identity_tests {
    use super::*;
    use anyhow::{Context, Result};
    #[test]
    fn nested_abort_retry_retains_each_driver_and_its_capture_guards() -> Result<()> {
        if crate::test_runs_as_root() {
            return Ok(());
        }
        use std::os::{
            fd::AsFd,
            unix::fs::{DirBuilderExt, PermissionsExt},
        };
        let root = std::env::temp_dir().join(format!("nested-cleanup-{}", crate::ids::new_id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root)?;
        let mut fixtures = Vec::new();
        for label in ["inner", "outer"] {
            let guard = composition::capture_location(&root.join(label))?;
            std::fs::create_dir(&guard.path)?;
            let blocked = guard.path.join("blocked");
            std::fs::create_dir(&blocked)?;
            std::fs::write(blocked.join("keep"), b"pending cleanup")?;
            let held = std::fs::File::open(&guard.path)?;
            let child = std::fs::File::open(&blocked)?;
            let owner = retain_owned(&guard.path)?;
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0))?;
            let parent = guard.path.parent().unwrap().to_path_buf();
            fixtures.push((owner, guard, parent, held, child));
        }
        let (outer, outer_guard, outer_parent, outer_root, outer_child) = fixtures.pop().unwrap();
        let (inner, inner_guard, inner_parent, inner_root, inner_child) = fixtures.pop().unwrap();
        assert!(!Arc::ptr_eq(&outer.driver, &inner.driver));
        let mut error = inner.abort(anyhow::anyhow!("primary failure")).downcast::<Cleanup>()?;
        error._guards.push(inner_guard);
        let cause = anyhow::Error::new(error).context("inner operation failed");
        let mut error = outer.abort(cause).downcast::<Cleanup>()?;
        error._guards.push(outer_guard);
        assert!(format!("{error:#}").contains("inner operation failed"));
        rustix::fs::fchmod(outer_child.as_fd(), rustix::fs::Mode::from_raw_mode(0o700))?;
        let error = error.retry().expect_err("the inner operation must still report its retained failure");
        assert!(format!("{error:#}").contains("inner operation failed"), "retry must retain the original context");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(outer_root.metadata()?.nlink(), 0);
        assert!(inner_root.metadata()?.nlink() > 0);
        assert!(!outer_parent.exists(), "the completed owner's guard is released");
        assert!(inner_parent.exists(), "the pending owner's guard must remain");
        rustix::fs::fchmod(inner_child.as_fd(), rustix::fs::Mode::from_raw_mode(0o700))?;
        error.retry().map_err(anyhow::Error::new)?;
        assert_eq!(inner_root.metadata()?.nlink(), 0);
        assert!(!inner_parent.exists());
        std::fs::remove_dir(root)?;
        Ok(())
    }

    #[test]
    fn received_identity_requires_exclusive_authority_and_restores_readonly_on_failure() -> Result<()> {
        let root = crate::repo::Temp::directory(
            std::env::temp_dir().join(format!("received-authority-{}", crate::ids::new_id())),
            0o700,
        )?;
        let source = crate::repo::Temp::empty(root.path.join("working"))?;
        std::fs::write(source.path.join("content"), b"kept")?;
        let destination = root.path.join("frozen");
        let capture = super::super::freeze_path_owned(&source.path, &destination, CapturePurpose::State)?;
        let uuid = crate::ids::new_id();
        let ownership = capture.owner.ownership.as_ref().context("capture authority absent")?;
        assert!(matches!(capture.owner.driver.received_identity(ownership, &uuid, 1), Err(FsError::Busy)));
        let other = composition::operation(None, &destination, CapturePurpose::State)?;
        assert!(matches!(other.received_identity(ownership, &uuid, 1), Err(FsError::WrongInstance)));
        let OwnedCapture { tree, owner, .. } = capture;
        drop(tree);
        let result = owner.driver.received_identity(owner.ownership.as_ref().context("capture authority absent")?, &uuid, 1);
        let info = super::super::host().namespace.info(&destination)?;
        assert!(info.readonly);
        assert_eq!(std::fs::read(destination.join("content"))?, b"kept");
        match result {
            Ok(()) => assert_eq!(info.received_uuid.as_deref(), Some(uuid.as_str())),
            Err(error) => {
                assert!(matches!(error,FsError::Io(ref io) if io.kind()==std::io::ErrorKind::PermissionDenied), "{error:#}")
            }
        }
        owner.discard()?;
        assert!(!destination.exists());
        Ok(())
    }
}
