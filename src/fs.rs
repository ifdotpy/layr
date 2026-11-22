//! Application ownership of native locations. Tree algorithms consume filesystem contracts;
//! only composition and checked admission know which native driver implements them.
mod composition;
pub(crate) mod owned_capture;
pub(crate) use owned_capture::{
    copy_capture_owned, freeze_source, materialise_owned, retain_owned, OwnedCapture, OwnedWorking, ResourceOwner,
};
pub(crate) mod accounting;
pub(crate) mod capture_session;
pub(crate) mod observations;
pub(crate) use capture_session::CaptureSession;
pub(crate) mod custody;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
pub(crate) mod receive_native;
pub(crate) mod services;
use crate::fsutil::Src;
use anyhow::{Context, Result};
pub(crate) use composition::host;
use layr_fs_core::*;
pub(crate) use services::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// The application has selected a location it owns (temporary work or a project tree).
/// Reading a user path never produces this authority; cleanup callers claim it explicitly.
pub(crate) struct ManagedPath(PathBuf);
impl ManagedPath {
    pub(crate) fn claim(path: &Path) -> Self {
        Self(path.to_path_buf())
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}
pub(crate) trait PathAdmission: Filesystem {
    fn release_unused(&self);
    /// Transport bookkeeping for an exclusively owned root; user metadata and content stay frozen.
    fn received_identity(&self, ownership: &OwnedTree, uuid: &str, transid: u64) -> layr_fs_core::Result<()>;
    fn working(&self, path: &Path) -> layr_fs_core::Result<WorkingTree>;
    fn frozen(&self, path: &Path) -> layr_fs_core::Result<FrozenTree>;
    fn location(&self, path: &Path) -> layr_fs_core::Result<NewTreeLocation>;
    fn claim(&self, path: ManagedPath) -> layr_fs_core::Result<OwnedTree>;
}
#[derive(Clone, Copy)]
pub(crate) enum CapturePurpose {
    State,
    Scan,
    Cache,
}
#[derive(Clone, Copy)]
pub(crate) enum Protection {
    Writable,
    Frozen,
}
pub(crate) struct CaptureReport {
    pub owners: OwnerLayout,
}

/// Recovery retains the exact operation registry and the remaining deletion authority.
pub(crate) struct Cleanup {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) driver: Arc<dyn Filesystem>,
    pub(crate) failure: RemovalFailure,
    cause: CleanupCause,
    description: String,
    _guards: Vec<composition::CaptureLocation>,
}
enum CleanupCause {
    Operation(FsError),
    Retained(Box<Cleanup>),
}
impl std::fmt::Debug for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::fmt::Display for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; cleanup requires retry: {:#}", self.description, self.failure.error)
    }
}
impl std::error::Error for Cleanup {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match &self.cause {
            CleanupCause::Operation(cause) => cause,
            CleanupCause::Retained(recovery) => recovery.as_ref(),
        })
    }
}
pub(crate) fn operation_error(driver: Arc<dyn Filesystem>, error: FsError) -> anyhow::Error {
    retained_error(driver, error, Vec::new())
}
fn retained_error(driver: Arc<dyn Filesystem>, error: FsError, guards: Vec<composition::CaptureLocation>) -> anyhow::Error {
    match error {
        FsError::Cleanup { cause, failure } => {
            let description = format!("{cause:#}");
            anyhow::Error::new(Cleanup {
                driver,
                cause: CleanupCause::Operation(*cause),
                failure: *failure,
                description,
                _guards: guards,
            })
        }
        error => error.into(),
    }
}
pub(crate) fn create(path: &Path) -> Result<()> {
    create_owned(path).map(OwnedWorking::release)
}
pub(crate) fn create_owned(path: &Path) -> Result<OwnedWorking> {
    let driver = composition::operation(None, path, CapturePurpose::State)?;
    let result = driver.location(path).and_then(|location| driver.create_empty(location));
    let created = result.map_err(|error| operation_error(driver.clone(), error))?;
    Ok(OwnedWorking::new(driver, created))
}
pub(crate) fn snapshot(src: &Path, dst: &Path, readonly: bool) -> Result<()> {
    capture(src, dst, if readonly { Protection::Frozen } else { Protection::Writable }, CapturePurpose::State).map(|_| ())
}
pub(crate) fn cache(src: &Path, dst: &Path) -> Result<OwnerLayout> {
    capture(src, dst, Protection::Writable, CapturePurpose::Cache).map(|report| report.owners)
}
pub(crate) fn capture(src: &Path, dst: &Path, protection: Protection, purpose: CapturePurpose) -> Result<CaptureReport> {
    let _t = crate::trace::span(format!("snapshot {}", dst.display()));
    match protection {
        Protection::Frozen => {
            let captured = freeze_path_owned(src, dst, purpose)?;
            captured.release();
            Ok(CaptureReport { owners: OwnerLayout::PerEntry })
        }
        Protection::Writable => {
            let (created, owners) = copy_capture_owned(src, dst, purpose)?;
            created.release();
            Ok(CaptureReport { owners })
        }
    }
}
/// The source of a copy: the folder itself, or for the folder of a fast line (a LayrFS mount)
/// its shadow, a native tree that the call brings up to the view.
pub(crate) fn source_of(src: &Path) -> Result<(PathBuf, Option<crate::views::FastSource>)> {
    match crate::views::fast_source(src) {
        Some(fast) => {
            let fast = fast?;
            Ok((fast.shadow.clone(), Some(fast)))
        }
        None => Ok((src.to_path_buf(), None)),
    }
}

pub(crate) fn freeze_path_owned(src: &Path, dst: &Path, purpose: CapturePurpose) -> Result<OwnedCapture> {
    let (source, fast) = source_of(src)?;
    let captured = freeze_native_owned(&source, dst, purpose)?;
    if let Some(fast) = fast {
        fast.finish(dst, true)?;
    }
    Ok(captured)
}

fn freeze_native_owned(src: &Path, dst: &Path, purpose: CapturePurpose) -> Result<OwnedCapture> {
    let driver = composition::operation(Some(src), dst, purpose)?;
    let result = (|| -> layr_fs_core::Result<_> {
        let source = driver.working(src)?;
        driver.freeze(FreezeSource::Working(&source), driver.location(dst)?)
    })();
    driver.release_unused();
    let captured = result.map_err(|error| operation_error(driver.clone(), error))?;
    if captured.consistency.running() {
        crate::fsutil::mark_unsettled(src);
    }
    Ok(OwnedCapture::new(driver, captured))
}

pub(crate) fn remove(path: &Path) -> Result<()> {
    if !host().namespace.managed_or_present(path)? {
        return Ok(());
    }
    let driver = composition::operation(None, path, CapturePurpose::State)?;
    let ownership = driver.claim(ManagedPath::claim(path))?;
    driver
        .remove(ownership)
        .map_err(|failure| {
            operation_error(
                driver,
                FsError::Cleanup {
                    cause: Box::new(FsError::Invalid("tree removal requires recovery")),
                    failure: Box::new(failure),
                },
            )
        })
        .with_context(|| format!("remove {}", path.display()))
}
#[cfg(test)]
pub(crate) fn set_readonly(path: &Path) -> Result<()> {
    host().namespace.protect(ManagedPath::claim(path), Protection::Frozen)
}
#[cfg(test)]
pub(crate) fn set_writable(path: &Path) -> Result<()> {
    host().namespace.protect(ManagedPath::claim(path), Protection::Writable)
}
pub(crate) fn move_tree(path: &Path, to: &Path) -> Result<()> {
    host().namespace.relocate(ManagedPath::claim(path), to)
}
pub(crate) fn observe_tree(path: &Path) -> Result<std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>> {
    host().observations.scan(path, None)
}
pub(crate) fn observe_paths(
    path: &Path,
    paths: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeMap<String, crate::status_cache::FileFingerprint>> {
    host().observations.scan(path, Some(paths))
}
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
pub(crate) fn is_not_empty(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::DirectoryNotEmpty))
}
/// A full disk anywhere in the chain of `error`.
pub(crate) fn is_storage_full(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::StorageFull))
}
pub(crate) fn copy_range(src: &std::fs::File, off: u64, len: u64, dst: &std::fs::File, dst_off: u64) -> std::io::Result<()> {
    if let Some(clone) = composition::range_clone() {
        if let Ok(range) = ByteRange::new(off, len) {
            match clone.clone_range(src, range, dst, dst_off) {
                Ok(Attempt::Applied(())) => return Ok(()),
                Ok(Attempt::Unavailable(error)) => return Err(error),
                Err(FsError::Io(error)) => return Err(error),
                Err(error) => return Err(std::io::Error::other(error)),
            }
        }
    }
    Err(std::io::ErrorKind::Unsupported.into())
}
pub(crate) fn reader(path: &Path) -> layr_fs_core::Result<Arc<dyn TreeRead>> {
    composition::reader(path)
}
pub(crate) fn candidate_hints(before: &Path, after: &Path, request: ChangeRequest) -> layr_fs_core::Result<hints::HintSet> {
    let Some(driver) = composition::comparison(before, after).map_err(FsError::backend)? else {
        return Ok(hints::HintSet::Partial);
    };
    let (before, after) = (driver.frozen(before)?, driver.frozen(after)?);
    if driver.capabilities(&before.scope()).change_hints.is_none() {
        return Ok(hints::HintSet::Partial);
    }
    Ok(hints::HintSet::Complete(driver.changes(&before, &after, request)?))
}
pub(crate) fn prefers_captured_comparison(source: &(impl crate::fsutil::Src + ?Sized)) -> bool {
    let Some(path) = source.folder() else { return false };
    let Ok(driver) = composition::operation(None, path, CapturePurpose::State) else { return false };
    let Ok(tree) = driver.working(path) else { return false };
    driver.capabilities(&tree.scope()).capture_copy.is_some_and(|copy| copy.prefers_capture_for_changes())
}
pub(crate) fn metadata(source: &(impl crate::fsutil::Src + ?Sized), relative: &str) -> Result<EntryMetadata> {
    let at = RelDir::from_bytes(relative.as_bytes())?;
    let mut metadata = if let Some(reader) = source.core_reader() {
        layr_fs_common::required(reader, &at)?
    } else if let Some(objects) = source.objects() {
        layr_fs_common::required(objects, &at)?
    } else {
        let path = source.at(relative)?;
        let (root, at) = match path.file_name() {
            Some(name) => (
                path.parent().unwrap_or_else(|| Path::new(".")),
                RelDir::Path(RelPath::new(std::os::unix::ffi::OsStrExt::as_bytes(name))?),
            ),
            None => (path.as_path(), RelDir::Root),
        };
        layr_fs_common::required(reader(root)?.as_ref(), &at)?
    };
    metadata.attributes.xattrs.retain(|(name, _)| layr_objects::fsx::keep_xattr(name));
    Ok(metadata)
}
pub(crate) fn attributes(source: &(impl crate::fsutil::Src + ?Sized), relative: &str) -> Result<EntryAttributes> {
    Ok(metadata(source, relative)?.attributes)
}

pub(crate) fn materialise_source(source: &(impl crate::fsutil::Src + ?Sized), dst: &Path, protection: Protection) -> Result<()> {
    if let Some(path) = source.folder() {
        return snapshot(path, dst, matches!(protection, Protection::Frozen));
    }
    let driver = composition::operation(None, dst, CapturePurpose::State)?;
    let reader = source
        .core_reader()
        .or_else(|| source.objects().map(|objects| objects as &dyn TreeRead))
        .ok_or_else(|| anyhow::anyhow!("logical source has no reader"))?;
    let result = (|| -> layr_fs_core::Result<()> {
        let destination = driver.location(dst)?;
        match protection {
            Protection::Writable => drop(driver.materialise(reader, destination)?),
            Protection::Frozen => drop(driver.freeze(FreezeSource::Reader(reader), destination)?),
        }
        Ok(())
    })();
    result.map_err(|error| operation_error(driver, error))
}
pub(crate) fn export_selected_owned(
    source: &crate::source::Source,
    paths: &[String],
    destination: &Path,
) -> Result<OwnedWorking> {
    let driver = composition::operation(None, destination, CapturePurpose::State)?;
    // The folder of a fast line is read from its shadow (`source_of`).
    let fast = source.folder().map(source_of).transpose()?;
    let folder = fast.as_ref().map(|(path, _)| path.as_path());
    let result = (|| -> layr_fs_core::Result<CreatedTree> {
        let created = driver.create_empty(driver.location(destination)?)?;
        let copied = (|| -> layr_fs_core::Result<()> {
            let mut root = layr_fs_common::required(created.tree.reader(), &RelDir::Root)?.attributes;
            root.mode = 0o700;
            created.tree.writer().set_metadata(&RelDir::Root, &root)?;
            let paths = paths.iter().map(|path| RelPath::new(path.as_bytes())).collect::<layr_fs_core::Result<Vec<_>>>()?;
            let native = folder.map(|path| driver.working(path)).transpose()?;
            let reader = native.as_ref().map(WorkingTree::reader).unwrap_or(source);
            let acceleration = native
                .as_ref()
                .and_then(|source| driver.capabilities(&created.tree.scope()).clone_new.map(|clone| (source, clone)));
            let target = &created.tree;
            let mut clone = acceleration
                .map(|(source, clone)| move |path: &RelPath| clone.clone_new(CloneSource::Working(source), path, target, path));
            layr_fs_common::copy_selected(
                reader,
                created.tree.writer(),
                &paths,
                clone.as_mut().map(|clone| clone as &mut dyn FnMut(&RelPath) -> layr_fs_core::Result<Attempt<()>>),
            )
        })();
        if let Err(cause) = copied {
            drop(created.tree);
            return match driver.remove(created.ownership) {
                Ok(()) => Err(cause),
                Err(failure) => Err(FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) }),
            };
        }
        Ok(created)
    })();
    driver.release_unused();
    result.map(|created| OwnedWorking::new(driver.clone(), created)).map_err(|error| operation_error(driver, error))
}

impl Cleanup {
    /// A failed rollback can accompany an earlier operation's failed rollback. Keep the
    /// earlier registry and its guards, including any context on the returned error.
    pub(crate) fn after(driver: Arc<dyn Filesystem>, failure: RemovalFailure, cause: anyhow::Error) -> Self {
        let description = format!("{cause:#}");
        let cause = match cause.downcast::<Self>() {
            Ok(recovery) => CleanupCause::Retained(Box::new(recovery)),
            Err(error) => CleanupCause::Operation(FsError::backend(error)),
        };
        Self { driver, failure, cause, description, _guards: Vec::new() }
    }
    /// Resume all retained receipts in order; private capture directories stay alive until
    /// cleanup succeeds. Dropping an error grants no authority over a replacement tree.
    #[cfg(test)]
    pub(crate) fn retry(self) -> std::result::Result<(), Self> {
        let Self { driver, mut failure, mut cause, description, _guards: guards } = self;
        loop {
            if let Err(next) = driver.remove(failure.ownership) {
                return Err(Self { driver, failure: next, cause, description, _guards: guards });
            }
            match cause {
                CleanupCause::Operation(FsError::Cleanup { cause: inner, failure: next }) => {
                    cause = CleanupCause::Operation(*inner);
                    failure = *next;
                }
                CleanupCause::Retained(recovery) => {
                    return recovery.retry().map_err(|mut remaining| {
                        remaining.description = description;
                        remaining
                    })
                }
                CleanupCause::Operation(_) => return Ok(()),
            }
        }
    }
}
