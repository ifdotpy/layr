use super::*;
use layr_fs_plain::MemoryTree;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

struct Fixture {
    path: PathBuf,
    driver: Arc<BtrfsFs>,
}
impl Fixture {
    fn new() -> anyhow::Result<Option<Self>> {
        let path = std::env::temp_dir().join(format!("layr-btrfs-create-{}", crate::ids::new_id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        if !native::is_supported(&path) {
            std::fs::remove_dir(path)?;
            return Ok(None);
        }
        let driver = Arc::new(BtrfsFs::new(File::open(&path)?)?);
        Ok(Some(Self { path, driver }))
    }
    fn source(&self) -> anyhow::Result<CreatedTree> {
        let seed = MemoryTree::new(attributes(0o755));
        seed.put(&RelPath::new(b"kept".to_vec())?, EntryData::File(&mut &b"source"[..]), &attributes(0o644))?;
        Ok(self.driver.materialise(&seed, self.driver.location(&self.path.join("source"))?)?)
    }
    fn no_stages(&self) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(&self.path)? {
            assert!(!entry?.file_name().as_bytes().starts_with(b".layr-fs-"));
        }
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.driver.set_fault(|_| Ok(()));
        if let Ok(mut trees) = self.driver.trees() {
            for resource in trees.values_mut() {
                if let Some(tree) = resource.pinned() {
                    let file = tree.reader.directory.root_file();
                    let _ = crate::sys::subvol_set_flags(file.as_fd(), 0);
                    let _ = rustix::fs::fchmod(file, Mode::from_raw_mode(0o700));
                }
                let _ = resource.remove(&self.driver);
            }
        }
        if let Ok(entries) = std::fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let _ = native::delete_tree_native(&entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}
fn attributes(mode: u32) -> EntryAttributes {
    EntryAttributes {
        mode,
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        modified_ns: 123,
        stored_time: Some(123),
        xattrs: Vec::new(),
    }
}
fn bytes(reader: &dyn TreeRead) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.open_file(&RelPath::new(b"kept".to_vec())?)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[test]
fn creation_pins_private_root_before_public_destination_can_be_replaced() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let destination = f.path.join("snapshot");
    let replaced = destination.clone();
    f.driver.set_fault(move |point| {
        if point == "created" {
            backend(native::create_subvolume(&replaced))?;
            std::fs::write(replaced.join("sentinel"), b"unrelated")?;
        }
        Ok(())
    })?;
    assert!(
        matches!(f.driver.freeze(&source.tree, f.driver.location(&destination)?), Err(FsError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists)
    );
    assert_eq!(std::fs::read(destination.join("sentinel"))?, b"unrelated");
    assert_eq!(bytes(source.tree.reader())?, b"source");
    f.no_stages()?;
    Ok(())
}

#[test]
fn published_replacement_does_not_change_sealing_or_removal_identity() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let destination = f.path.join("snapshot");
    let moved = f.path.join("moved");
    let (to_replace, old) = (destination.clone(), moved.clone());
    f.driver.set_fault(move |point| {
        if point == "published" {
            std::fs::rename(&to_replace, &old)?;
            backend(native::create_subvolume(&to_replace))?;
            std::fs::write(to_replace.join("sentinel"), b"unrelated")?;
        }
        Ok(())
    })?;
    let captured = f.driver.freeze(&source.tree, f.driver.location(&destination)?)?;
    assert_eq!(bytes(captured.tree.reader())?, b"source");
    assert!(!native::subvol_info(&destination)?.readonly);
    assert!(native::subvol_info(&moved)?.readonly);
    drop(captured.tree);
    let failure = f.driver.remove(captured.ownership).unwrap_err();
    assert!(matches!(failure.error, FsError::LocationChanged));
    assert!(moved.exists());
    assert_eq!(std::fs::read(destination.join("sentinel"))?, b"unrelated");
    native::delete_tree(&destination)?;
    std::fs::rename(&moved, &destination)?;
    f.driver.remove(failure.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    assert!(!destination.exists());
    f.no_stages()?;
    Ok(())
}

#[test]
fn root_open_and_preparation_failures_roll_back_both_creation_paths() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    for phase in ["operation-directory", "open-created", "prepared", "publish"] {
        for snapshot in [false, true] {
            f.driver.set_fault(move |point| {
                if point == phase {
                    Err(std::io::Error::from_raw_os_error(rustix::io::Errno::IO.raw_os_error()).into())
                } else {
                    Ok(())
                }
            })?;
            let destination = f.path.join("failed");
            let result = if snapshot {
                f.driver.freeze(&source.tree, f.driver.location(&destination)?).map(|_| ())
            } else {
                f.driver.materialise(&MemoryTree::new(attributes(0)), f.driver.location(&destination)?).map(|_| ())
            };
            assert!(matches!(result, Err(FsError::Io(_))), "phase {phase}: {result:?}");
            assert!(!destination.exists());
            f.no_stages()?;
        }
    }
    Ok(())
}

#[test]
fn legacy_cleanup_error_retains_driver_and_retry_authority() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let _source = f.source()?;
    let driver = BtrfsFs::for_path(&f.path.join("failed"))?;
    let weak = Arc::downgrade(&driver);
    let source = driver.working(&f.path.join("source"))?;
    let mut failed_cleanup = false;
    driver.set_fault(move |point| {
        if point == "open-created" {
            return Err(std::io::Error::from_raw_os_error(rustix::io::Errno::MFILE.raw_os_error()).into());
        }
        if point == "cleanup" && !failed_cleanup {
            failed_cleanup = true;
            return Err(std::io::Error::from_raw_os_error(rustix::io::Errno::IO.raw_os_error()).into());
        }
        Ok(())
    })?;
    let error = driver.freeze(&source, driver.location(&f.path.join("failed"))?).err().expect("injected root open must fail");
    drop(source);
    let error = driver.legacy_error(error);
    assert!(weak.upgrade().is_some());
    let NativeCleanup { driver, failure, .. } = error.downcast::<NativeCleanup>()?;
    driver.remove(failure.ownership).map_err(|e| anyhow::anyhow!("{}", e.error))?;
    drop(driver);
    assert!(weak.upgrade().is_none());
    f.no_stages()?;
    Ok(())
}

#[test]
fn mode_zero_snapshot_preserves_root_acl_and_does_not_chmod_source() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let reader =
        f.driver.trees()?.get(&f.driver.issuer.working_key(&source.tree)?).and_then(Resource::pinned).unwrap().reader.clone();
    let root = reader.directory.root_file();
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in [
        (1u16, 0u16, u32::MAX),
        (2, 4, rustix::process::geteuid().as_raw() + 1),
        (4, 0, u32::MAX),
        (16, 0, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&permissions.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    rustix::fs::fsetxattr(root, "system.posix_acl_access", &acl, rustix::fs::XattrFlags::empty())?;
    assert_eq!(root.metadata()?.mode() & 0o7777, 0);
    let captured = f.driver.freeze(&source.tree, f.driver.location(&f.path.join("snapshot"))?)?;
    let captured_reader =
        f.driver.trees()?.get(&f.driver.issuer.frozen_key(&captured.tree)?).and_then(Resource::pinned).unwrap().reader.clone();
    for file in [root, captured_reader.directory.root_file()] {
        assert_eq!(file.metadata()?.mode() & 0o7777, 0);
        let mut actual = vec![0; 128];
        let count = rustix::fs::fgetxattr(file, "system.posix_acl_access", actual.as_mut_slice())?;
        assert_eq!(&actual[..count], acl);
    }
    assert_ne!(crate::sys::subvol_flags(captured_reader.directory.root_file().as_fd())? & crate::sys::SUBVOL_RDONLY, 0);
    f.no_stages()?;
    Ok(())
}

#[test]
fn frozen_publication_rejects_a_nonprivate_parent_before_creation() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let public = f.path.join("public");
    std::fs::create_dir(&public)?;
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755))?;
    assert!(matches!(f.driver.freeze(&source.tree, f.driver.location(&public.join("snapshot"))?), Err(FsError::Invalid(_))));
    assert!(!public.join("snapshot").exists());
    f.no_stages()?;
    Ok(())
}

#[test]
fn descriptor_exhaustion_rolls_back_without_opening_more_files() -> anyhow::Result<()> {
    const CHILD: &str = "LAYR_TEST_FS_CREATION_FD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // RLIMIT_NOFILE is process-wide. Isolate this mutation from parallel test cases.
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "btrfs_driver::creation::tests::descriptor_exhaustion_rolls_back_without_opening_more_files",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()?;
        assert!(status.success());
        return Ok(());
    }
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    struct RestoreLimit(rustix::process::Rlimit);
    impl Drop for RestoreLimit {
        fn drop(&mut self) {
            let _ = rustix::process::setrlimit(rustix::process::Resource::Nofile, self.0);
        }
    }
    let original = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let restore = RestoreLimit(original);
    let held = Arc::new(Mutex::new(Vec::<File>::new()));
    let exhausted = held.clone();
    f.driver.set_fault(move |point| {
        if point == "open-created" {
            io(rustix::process::setrlimit(
                rustix::process::Resource::Nofile,
                rustix::process::Rlimit { current: Some(original.current.map_or(64, |n| n.min(64))), maximum: original.maximum },
            ))?;
            let mut held = exhausted.lock().unwrap();
            loop {
                match File::open("/dev/null") {
                    Ok(file) => held.push(file),
                    Err(error) if error.raw_os_error() == Some(rustix::io::Errno::MFILE.raw_os_error()) => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    })?;
    let destination = f.path.join("failed");
    let result = f.driver.freeze(&source.tree, f.driver.location(&destination)?);
    held.lock().unwrap().clear();
    drop(restore);
    assert!(matches!(result, Err(FsError::Io(error)) if error.raw_os_error() == Some(rustix::io::Errno::MFILE.raw_os_error())));
    assert!(!destination.exists());
    f.no_stages()?;
    assert_eq!(bytes(source.tree.reader())?, b"source");
    Ok(())
}

#[test]
fn private_creation_publishes_byte_names_and_rejects_untrusted_anchors() -> anyhow::Result<()> {
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let working =
        f.driver.materialise(source.tree.reader(), f.driver.location(&f.path.join(OsStr::from_bytes(b"working-\xff")))?)?;
    let snapshot = f.driver.freeze(&working.tree, f.driver.location(&f.path.join(OsStr::from_bytes(b"snapshot-\xfe")))?)?;
    assert_eq!(bytes(snapshot.tree.reader())?, b"source");
    let public = f.path.join("untrusted-anchor");
    std::fs::create_dir(&public)?;
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o777))?;
    assert!(matches!(BtrfsFs::new(File::open(&public)?), Err(FsError::Invalid(_))));
    f.no_stages()?;
    Ok(())
}

#[path = "removal_tests.rs"]
mod removal_tests;

#[test]
fn live_reader_materialisation_preserves_nested_contents_and_snapshot_compatibility() -> anyhow::Result<()> {
    use layr_fs_plain::{MemoryFs, PlainFs};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Wrapped<'a>(&'a dyn TreeRead);
    impl TreeRead for Wrapped<'_> {
        fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>> {
            self.0.metadata(path)
        }
        fn entries(&self, path: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()> {
            self.0.entries(path, visit)
        }
        fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>> {
            self.0.open_file(path)
        }
        fn read_link(&self, path: &RelPath) -> Result<Vec<u8>> {
            self.0.read_link(path)
        }
    }
    fn save(store: &layr_objects::Store, reader: &dyn TreeRead) -> anyhow::Result<(layr_objects::Id, layr_objects::Id)> {
        let result = layr_objects::write::write_tree_from(
            store,
            &layr_objects::write::ReaderSource(reader),
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        Ok((result.tree.unwrap(), result.times.unwrap()))
    }
    let Some(f) = Fixture::new()? else {
        return Ok(());
    };
    let source = f.source()?;
    let nested = f.path.join("source/nested");
    native::create_subvolume(&nested)?;
    std::fs::write(nested.join("data"), b"nested contents")?;
    std::fs::hard_link(nested.join("data"), nested.join("alias"))?;
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o711))?;
    rustix::fs::fsetxattr(File::open(&nested)?, "user.distinct", b"metadata", rustix::fs::XattrFlags::empty())?;
    let snapshots = Arc::new(AtomicUsize::new(0));
    let count = snapshots.clone();
    f.driver.set_fault(move |point| {
        if point == "snapshot-created" {
            count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    })?;
    let direct = f.driver.materialise(source.tree.reader(), f.driver.location(&f.path.join("direct"))?)?;
    let wrapped = f.driver.materialise(&Wrapped(source.tree.reader()), f.driver.location(&f.path.join("wrapped"))?)?;
    let memory = MemoryFs::default();
    let memory_copy = memory.materialise(source.tree.reader(), memory.location(RelPath::new(b"memory".to_vec())?)?)?;
    let plain_root = f.path.join("plain");
    std::fs::DirBuilder::new().mode(0o700).create(&plain_root)?;
    let plain = PlainFs::open(&plain_root)?;
    let plain_copy = plain.materialise(source.tree.reader(), plain.location(RelPath::new(b"copy".to_vec())?)?)?;
    let store = layr_objects::Store::open(&f.path.join("objects"), layr_objects::Keying::Plain)?;
    let expected = save(&store, source.tree.reader())?;
    let nested_meta = source.tree.reader().metadata(&RelDir::Path(RelPath::new(b"nested".to_vec())?))?.unwrap().attributes;
    for reader in [direct.tree.reader(), wrapped.tree.reader(), memory_copy.tree.reader(), plain_copy.tree.reader()] {
        assert_eq!(save(&store, reader)?, expected);
        assert_eq!(reader.metadata(&RelDir::Path(RelPath::new(b"nested".to_vec())?))?.unwrap().attributes, nested_meta);
    }
    assert_eq!(snapshots.load(Ordering::SeqCst), 0);
    let legacy = f.driver.native_snapshot(&source.tree, f.driver.location(&f.path.join("legacy"))?)?;
    assert!(legacy.tree.reader().metadata(&RelDir::Path(RelPath::new(b"nested/data".to_vec())?))?.is_none());
    assert_eq!(snapshots.load(Ordering::SeqCst), 1);
    let root =
        f.driver.trees()?.get(&f.driver.issuer.working_key(&source.tree)?).and_then(Resource::pinned).unwrap().reader.clone();
    crate::sys::subvol_set_flags(root.directory.root_file().as_fd(), crate::sys::SUBVOL_RDONLY)?;
    let imported = f.driver.frozen(&f.path.join("source"))?;
    let imported_copy = f.driver.materialise(imported.reader(), f.driver.location(&f.path.join("imported"))?)?;
    assert_eq!(save(&store, imported_copy.tree.reader())?, expected);
    assert_eq!(snapshots.load(Ordering::SeqCst), 1);
    let known = f.driver.freeze(&source.tree, f.driver.location(&f.path.join("known"))?)?;
    let _accelerated = f.driver.materialise(known.tree.reader(), f.driver.location(&f.path.join("accelerated"))?)?;
    assert_eq!(snapshots.load(Ordering::SeqCst), 3);
    Ok(())
}
