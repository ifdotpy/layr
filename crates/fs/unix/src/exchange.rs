use layr_fs_core::{ExchangeProgress, FsError, RelPath, Result};
use rustix::fs::{AtFlags, RenameFlags};
use std::{
    ffi::OsStr,
    fs::File,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    sync::Arc,
};

/// An operation retains the parent and original root, rather than adopting whatever later
/// occupies a pathname. The two native locations remain bound throughout retry.
pub struct ExchangeLocation {
    pub parent: Arc<File>,
    pub name: RelPath,
    root: Arc<File>,
}
impl ExchangeLocation {
    pub fn new(parent: Arc<File>, name: RelPath, root: Arc<File>) -> Result<Self> {
        if !name.is_name() || !parent.metadata()?.is_dir() || !root.metadata()?.is_dir() {
            return Err(FsError::Invalid("exchange requires two directory locations"));
        }
        Ok(Self { parent, name, root })
    }
    fn validate(&self, expected: &File) -> Result<()> {
        let actual = rustix::fs::statat(self.parent.as_ref(), OsStr::from_bytes(self.name.as_bytes()), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| FsError::Io(error.into()))?;
        let expected = expected.metadata()?;
        if (actual.st_dev as u64, actual.st_ino as u64) != (expected.dev(), expected.ino()) {
            return Err(FsError::LocationChanged);
        }
        Ok(())
    }
}
pub struct Exchange {
    pub first: ExchangeLocation,
    pub second: ExchangeLocation,
    progress: ExchangeProgress,
}
impl Exchange {
    pub fn new(first: ExchangeLocation, second: ExchangeLocation) -> Result<Self> {
        let a = first.root.metadata()?;
        let b = second.root.metadata()?;
        if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
            return Err(FsError::Invalid("exchange requires distinct directory identities"));
        }
        Ok(Self { first, second, progress: ExchangeProgress::Unchanged })
    }
    pub fn progress(&self) -> ExchangeProgress {
        self.progress
    }
    pub fn run(&mut self) -> Result<()> {
        self.run_with(|| Ok(()), || Ok(()))
    }
    fn run_with(&mut self, before: impl FnOnce() -> Result<()>, after: impl FnOnce() -> Result<()>) -> Result<()> {
        if self.progress == ExchangeProgress::Unchanged {
            self.first.validate(&self.first.root)?;
            self.second.validate(&self.second.root)?;
            before()?;
            rustix::fs::renameat_with(
                self.first.parent.as_ref(),
                OsStr::from_bytes(self.first.name.as_bytes()),
                self.second.parent.as_ref(),
                OsStr::from_bytes(self.second.name.as_bytes()),
                RenameFlags::EXCHANGE,
            )
            .map_err(|error| FsError::Io(error.into()))?;
            self.progress = ExchangeProgress::Swapped;
            after()?;
        }
        self.first.validate(&self.second.root)?;
        self.second.validate(&self.first.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::DirBuilderExt};
    struct Fixture {
        root: std::path::PathBuf,
        operation: Exchange,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir().join(format!(
                "layr-exchange-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            fs::DirBuilder::new().mode(0o700).create(&root)?;
            for (name, bytes) in [("first", b"new"), ("second", b"old")] {
                fs::create_dir(root.join(name))?;
                fs::write(root.join(name).join("file"), bytes)?;
            }
            let parent = Arc::new(File::open(&root)?);
            let location = |name: &str| {
                ExchangeLocation::new(parent.clone(), RelPath::new(name.as_bytes())?, Arc::new(File::open(root.join(name))?))
            };
            let operation = Exchange::new(location("first")?, location("second")?)?;
            Ok(Self { root, operation })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn failure_after_exchange_retries_validation_without_swapping_again() -> Result<()> {
        let mut fixture = Fixture::new()?;
        assert!(fixture.operation.run_with(|| Ok(()), || Err(FsError::Invalid("post-exchange fault"))).is_err());
        assert_eq!(fixture.operation.progress(), ExchangeProgress::Swapped);
        fixture.operation.run_with(|| panic!("exchange must not run twice"), || panic!("exchange must not run twice"))?;
        assert_eq!(fs::read(fixture.root.join("first/file"))?, b"old");
        assert_eq!(fs::read(fixture.root.join("second/file"))?, b"new");
        Ok(())
    }
    #[test]
    fn substituted_directory_survives_failed_validation_and_retry() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let root = &fixture.root;
        assert!(fixture
            .operation
            .run_with(
                || {
                    fs::rename(root.join("first"), root.join("saved"))?;
                    fs::create_dir(root.join("first"))?;
                    fs::write(root.join("first/sentinel"), b"foreign")?;
                    Ok(())
                },
                || Ok(())
            )
            .is_err());
        assert_eq!(fixture.operation.progress(), ExchangeProgress::Swapped);
        assert!(fixture.operation.run().is_err());
        assert_eq!(fs::read(root.join("second/sentinel"))?, b"foreign");
        assert_eq!(fs::read(root.join("first/file"))?, b"old");
        fs::rename(root.join("second"), root.join("foreign"))?;
        fs::rename(root.join("saved"), root.join("second"))?;
        fixture.operation.run()?;
        assert_eq!(fs::read(root.join("foreign/sentinel"))?, b"foreign");
        assert_eq!(fs::read(root.join("second/file"))?, b"new");
        Ok(())
    }
    #[test]
    fn pre_exchange_error_changes_neither_location() -> Result<()> {
        let mut fixture = Fixture::new()?;
        assert!(fixture
            .operation
            .run_with(|| Err(FsError::Invalid("before exchange")), || panic!("exchange must not start"))
            .is_err());
        assert_eq!(fixture.operation.progress(), ExchangeProgress::Unchanged);
        assert_eq!(fs::read(fixture.root.join("first/file"))?, b"new");
        assert_eq!(fs::read(fixture.root.join("second/file"))?, b"old");
        fixture.operation.run()?;
        Ok(())
    }
}
