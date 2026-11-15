//! The single native composition boundary. Configuration chooses implementations, not algorithms.
use super::*;
#[cfg(target_os = "macos")]
use crate::apfs::HostServices;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use crate::btrfs::HostServices;
#[cfg(feature = "overlayfs")]
use crate::overlayfs::HostServices;
pub(crate) fn host() -> &'static Host {
    static HOST: Host = Host {
        file_copies: file_copies(),
        managed_entries: managed_entries(),
        remove_entry: remove_entry(),
        create_entry: create_entry(),
        nested_entry: nested_entry(),
        allocation_part: allocation_part(),
        namespace: &HostServices,
        transfer: &HostServices,
        accounting: &HostServices,
        observations: &HostServices,
        registry: registry(),
        #[cfg(target_os = "macos")]
        prune_observations: Some(crate::apfs::prune_saved_scans),
        #[cfg(not(target_os = "macos"))]
        prune_observations: None,
        register_cache: register_cache(),
        copier: HostServices::copier,
    };
    &HOST
}
pub(super) fn operation(source: Option<&Path>, destination: &Path, purpose: CapturePurpose) -> Result<Arc<dyn PathAdmission>> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        let _ = purpose;
        Ok(match source {
            Some(source) => crate::btrfs_driver::BtrfsFs::for_snapshot(source, destination)?,
            None => crate::btrfs_driver::BtrfsFs::for_path(destination)?,
        })
    }
    #[cfg(feature = "overlayfs")]
    {
        let _ = (source, destination, purpose);
        Ok(Arc::new(crate::overlayfs::driver::OverlayFs::default()))
    }
    #[cfg(target_os = "macos")]
    {
        use crate::apfs::driver::{ApfsFs, Links};
        let _ = source;
        Ok(ApfsFs::for_destination(
            destination,
            match purpose {
                CapturePurpose::State => Links::Keep,
                _ => Links::Split,
            },
        )?)
    }
}
pub(crate) struct CaptureLocation {
    pub path: PathBuf,
    parent: std::fs::File,
    name: std::ffi::OsString,
}
impl Drop for CaptureLocation {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(&self.parent, &self.name, rustix::fs::AtFlags::REMOVEDIR);
    }
}
pub(crate) fn capture_location(destination: &Path) -> layr_fs_core::Result<CaptureLocation> {
    use std::os::unix::fs::MetadataExt;
    let parent_path = destination.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let parent = std::fs::File::open(parent_path)?;
    let metadata = parent.metadata()?;
    let uid = rustix::process::geteuid().as_raw();
    if !((metadata.uid() == uid && metadata.mode() & 0o022 == 0) || (metadata.uid() == 0 && metadata.mode() & 0o1000 != 0)) {
        return Err(FsError::Invalid("capture workspace needs a protected parent"));
    }
    let name = std::ffi::OsString::from(format!(".layr-capture-{}", crate::ids::new_id()));
    let path = parent_path.join(&name).join("tree");
    rustix::fs::mkdirat(&parent, &name, rustix::fs::Mode::from_raw_mode(0o700)).map_err(|error| FsError::Io(error.into()))?;
    Ok(CaptureLocation { path, parent, name })
}
pub(super) fn range_clone() -> Option<&'static dyn ReflinkRange> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        Some(&crate::btrfs_driver::RangeClone)
    }
    #[cfg(feature = "overlayfs")]
    {
        Some(&crate::overlayfs::driver::RangeClone)
    }
    #[cfg(target_os = "macos")]
    {
        None
    }
}
pub(super) fn reader(path: &Path) -> layr_fs_core::Result<Arc<dyn TreeRead>> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        Ok(Arc::new(crate::btrfs_driver::Reader::open(path)?))
    }
    #[cfg(feature = "overlayfs")]
    {
        Ok(Arc::new(layr_fs_unix::DirectoryTree::open(path)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Arc::new(crate::apfs::ApfsTree::open(path)?))
    }
}
pub(super) fn comparison(before: &Path, after: &Path) -> Result<Option<Arc<dyn PathAdmission>>> {
    #[cfg(feature = "overlayfs")]
    {
        Ok(crate::overlayfs::driver::OverlayFs::comparison(before, after)?
            .map(|driver| Arc::new(driver) as Arc<dyn PathAdmission>))
    }
    #[cfg(not(feature = "overlayfs"))]
    {
        let _ = before;
        operation(None, after, CapturePurpose::State).map(Some)
    }
}

const fn registry() -> Option<Registry> {
    #[cfg(feature = "overlayfs")]
    {
        Some(Registry {
            recover: crate::overlayfs::recover,
            recover_existing: crate::overlayfs::recover_existing,
            external: crate::overlayfs::set_external,
            verify: crate::overlayfs::verify_registry,
        })
    }
    #[cfg(not(feature = "overlayfs"))]
    {
        None
    }
}

const fn file_copies() -> FileCopies {
    #[cfg(target_os = "macos")]
    {
        FileCopies {
            unseal_cloned: Some(crate::sys::apfs_unseal_fd),
            clone_new: Some(crate::apfs::entry::clone_new),
            replace_private: Some(crate::apfs::entry::replace_private),
            replace_content: None,
        }
    }
    #[cfg(target_os = "linux")]
    {
        FileCopies {
            unseal_cloned: None,
            clone_new: None,
            replace_private: None,
            replace_content: Some(layr_fs_unix::clone_contents),
        }
    }
}

const fn allocation_part() -> Option<fn(&Path) -> Option<String>> {
    #[cfg(feature = "overlayfs")]
    {
        Some(crate::overlayfs::accounting_part)
    }
    #[cfg(not(feature = "overlayfs"))]
    {
        None
    }
}

const fn managed_entries() -> Option<ManagedEntries> {
    #[cfg(feature = "overlayfs")]
    {
        Some(ManagedEntries {
            contains: crate::overlayfs::contains_managed_tree,
            rename_plain: crate::overlayfs::rename_plain_tree,
        })
    }
    #[cfg(not(feature = "overlayfs"))]
    {
        None
    }
}
const fn remove_entry() -> fn(&std::os::fd::OwnedFd, &str) -> Result<()> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        crate::btrfs::entries::remove_at
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    {
        crate::tree::remove_plain_at
    }
}

const fn create_entry() -> fn(&std::os::fd::OwnedFd, &str, &Path) -> Result<()> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        |parent, name, _| crate::sys::subvol_create(std::os::fd::AsFd::as_fd(parent), name)
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    {
        |_, _, path| super::create(path)
    }
}
const fn nested_entry() -> fn(&Path, &str, &crate::fsutil::Meta) -> bool {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        |_, _, metadata| metadata.is_dir() && metadata.ino == 256
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    {
        |root, path, metadata| metadata.is_dir() && host().namespace.managed(&root.join(path))
    }
}

const fn register_cache() -> Option<fn(&Path) -> Result<()>> {
    #[cfg(target_os = "macos")]
    {
        Some(crate::apfs::mark_cache)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
