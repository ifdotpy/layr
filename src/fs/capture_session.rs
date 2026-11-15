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

impl CaptureSession {
    pub(crate) fn prepare(&self, source: &Path, directory: &Path) -> Result<()> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("prepared-{}", crate::store::temp_name()));
        let driver = composition::operation(None, &path, CapturePurpose::State)?;
        let location = driver.location(&path)?;
        if !driver.capabilities(&location.scope()).capture_copy.is_some_and(|copy| copy.prepare_before_lock()) {
            return Ok(());
        }
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
        let mut failures = Vec::new();
        for Prepared { driver, capture, .. } in self.prepared.take() {
            drop(capture.tree);
            if let Err(failure) = driver.remove(capture.ownership) {
                failures.push(Cleanup {
                    driver,
                    failure,
                    cause: CleanupCause::Operation(FsError::Invalid("unused prepared capture cleanup failed")),
                    guards: Vec::new(),
                });
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow::Error::new(CaptureCleanup(failures)))
        }
    }
}

/// A command can retry failed cleanup even after converting the error to anyhow::Error.
pub(crate) struct CaptureCleanup(Vec<Cleanup>);
impl std::fmt::Debug for CaptureCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::fmt::Display for CaptureCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} unused capture(s) require cleanup", self.0.len())
    }
}
impl std::error::Error for CaptureCleanup {}
impl CaptureCleanup {
    pub(crate) fn retry(self) -> std::result::Result<(), Self> {
        let remaining = self.0.into_iter().filter_map(|cleanup| cleanup.retry().err()).collect::<Vec<_>>();
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(Self(remaining))
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        if let Err(error) = self.discard() {
            eprintln!("warning: {error:#}");
        }
    }
}
