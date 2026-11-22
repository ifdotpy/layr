use super::*;
use std::cell::RefCell;

struct Prepared {
    source: PathBuf,
    driver: Arc<dyn PathAdmission>,
    capture: Capture,
}

/// Preparation belongs to a command, including captures it never publishes. Native drivers
/// decide whether preparing is useful; publication still consumes the core capture contract.
#[derive(Default)]
pub(crate) struct CaptureSession {
    prepared: RefCell<Vec<Prepared>>,
}

fn source_key(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let parent = path.parent().ok_or(FsError::Invalid("capture source has no parent"))?;
    let name = path.file_name().ok_or(FsError::Invalid("capture source has no name"))?;
    Ok(std::fs::canonicalize(parent)?.join(name))
}

fn preparation(directory: &Path) -> Result<Option<(Arc<dyn PathAdmission>, NewTreeLocation)>> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("prepared-{}", crate::store::temp_name()));
    let driver = composition::operation(None, &path, CapturePurpose::State)?;
    let location = driver.location(&path)?;
    let enabled = driver.capabilities(&location.scope()).capture_copy.is_some_and(|copy| copy.prepare_before_lock());
    Ok(enabled.then_some((driver, location)))
}

impl CaptureSession {
    pub(crate) fn may_prepare(directory: &Path) -> bool {
        // Defer admission errors until a due cache actually needs a capture.
        !matches!(preparation(directory), Ok(None))
    }

    pub(crate) fn prepare(&self, source: &Path, directory: &Path) -> Result<()> {
        // A fast line's copy comes from its shadow at the snapshot (`source_of`), not before.
        if crate::views::is_fast_mount(source) {
            return Ok(());
        }
        let Some((driver, location)) = preparation(directory)? else {
            return Ok(());
        };
        let key = source_key(source)?;
        let result = driver.working(source).and_then(|working| driver.freeze(FreezeSource::Working(&working), location));
        driver.release_unused();
        let capture = result.map_err(|error| operation_error(driver.clone(), error))?;
        if capture.consistency.running() {
            crate::fsutil::mark_unsettled(source);
        }
        self.prepared.borrow_mut().push(Prepared { source: key, driver, capture });
        Ok(())
    }

    pub(crate) fn snapshot(&self, source: &Path, destination: &Path) -> Result<OwnedCapture> {
        if self.prepared.borrow().is_empty() {
            return super::freeze_path_owned(source, destination, CapturePurpose::State);
        }
        let key = source_key(source)?;
        let prepared = {
            let mut entries = self.prepared.borrow_mut();
            entries.iter().position(|entry| entry.source == key).map(|index| entries.remove(index))
        };
        let Some(Prepared { driver, capture, .. }) = prepared else {
            return super::freeze_path_owned(source, destination, CapturePurpose::State);
        };
        let consistency = capture.consistency;
        // Register the destination on the original instance. Even a failed location admission
        // must return the retained ownership, rather than silently dropping the prepared tree.
        let result = match driver.location(destination) {
            Ok(location) => driver.freeze(FreezeSource::Captured(capture), location),
            Err(cause) => {
                drop(capture.tree);
                match driver.remove(capture.ownership) {
                    Ok(()) => Err(cause),
                    Err(failure) => Err(FsError::Cleanup { cause: Box::new(cause), failure: Box::new(failure) }),
                }
            }
        };
        let captured = result.map_err(|error| operation_error(driver.clone(), error))?;
        if consistency.running() {
            crate::fsutil::mark_unsettled(source);
        }
        Ok(OwnedCapture::new(driver, captured))
    }

    pub(crate) fn discard(&self) -> Result<()> {
        let mut result = Ok(());
        for Prepared { driver, capture, .. } in self.prepared.take() {
            drop(capture.tree);
            if let Err(failure) = driver.remove(capture.ownership) {
                let cause = result.err().unwrap_or_else(|| anyhow::anyhow!("unused prepared capture cleanup failed"));
                result = Err(anyhow::Error::new(Cleanup::after(driver, failure, cause)));
            }
        }
        result
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        if let Err(error) = self.discard() {
            eprintln!("warning: {error:#}");
        }
    }
}

#[cfg(test)]
#[test]
fn preparation_probe_defers_destination_errors() -> Result<()> {
    let file = std::env::temp_dir().join(format!("capture-not-directory-{}", crate::ids::new_id()));
    std::fs::File::create(&file)?;
    let may_prepare = CaptureSession::may_prepare(&file);
    let result = CaptureSession::default().prepare(&file, &file);
    std::fs::remove_file(file)?;
    assert!(may_prepare);
    let io = result.err().and_then(|error| error.downcast::<std::io::Error>().ok());
    assert_eq!(io.map(|error| error.kind()), Some(std::io::ErrorKind::AlreadyExists));
    Ok(())
}

#[cfg(all(test, target_os = "linux", not(feature = "overlayfs")))]
mod tests {
    use super::*;
    use anyhow::Result;
    #[test]
    fn discarded_preparations_share_the_owned_cleanup_chain_across_drivers() -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let root = std::env::temp_dir().join(format!("prepared-cleanup-{}", crate::ids::new_id()));
        std::fs::DirBuilder::new().mode(0o700).create(&root)?;
        let session = CaptureSession::default();
        let source = layr_fs_plain::MemoryTree::new(EntryAttributes {
            mode: 0o700,
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            modified_ns: 0,
            stored_time: Some(0),
            xattrs: Vec::new(),
            acl: None,
        });
        let mut leases = Vec::new();
        for name in ["first", "second"] {
            let destination = root.join(name);
            let driver = composition::operation(None, &destination, CapturePurpose::State)?;
            let capture = driver.freeze(FreezeSource::Reader(&source), driver.location(&destination)?)?;
            leases.push(capture.tree.clone());
            session.prepared.borrow_mut().push(Prepared { source: destination, driver, capture });
        }
        let error = session.discard().expect_err("live readers must retain both preparations");
        assert!(session.prepared.borrow().is_empty());
        let recovery = error.downcast::<Cleanup>()?;
        drop(leases.pop());
        let recovery = recovery.retry().expect_err("the first preparation still has a reader");
        assert!(root.join("first").exists());
        assert!(!root.join("second").exists());
        drop(leases.pop());
        recovery.retry().map_err(anyhow::Error::new)?;
        assert!(!root.join("first").exists());
        std::fs::remove_dir(root)?;
        Ok(())
    }
}
