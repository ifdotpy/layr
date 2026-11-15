//! Descriptor-relative APFS observations. Internal state markers and seals are not user metadata.
use super::*;
use layr_fs_core::{Attempt, EntryAttributes, EntryData, EntryMetadata, FsError, ReadSeek, RelDir, RelPath, TreeRead, TreeWrite};
use layr_fs_unix::DirectoryTree;
use std::os::fd::AsFd;

pub(crate) struct ApfsTree {
    pub(super) path: PathBuf,
    pub(super) directory: DirectoryTree,
}
impl ApfsTree {
    pub(crate) fn open(path: &Path) -> layr_fs_core::Result<Self> {
        let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
        Ok(Self { directory: DirectoryTree::open(&path)?, path })
    }
    pub(super) fn validate(&self) -> layr_fs_core::Result<()> {
        let actual = fs::symlink_metadata(&self.path)?;
        let held = self.directory.root_file().metadata()?;
        if !actual.is_dir() || (actual.dev(), actual.ino()) != (held.dev(), held.ino()) {
            return Err(FsError::LocationChanged);
        }
        Ok(())
    }
    fn nested(path: &Path) -> layr_fs_core::Result<bool> {
        if !fs::symlink_metadata(path)?.is_dir() {
            return Ok(false);
        }
        match subvol_info(path) {
            Ok(_) => Ok(true),
            Err(error)
                if error.chain().any(|cause| {
                    cause.downcast_ref::<io::Error>().is_some_and(|error| error.raw_os_error() == Some(libc::ENOATTR))
                }) =>
            {
                Ok(false)
            }
            Err(error) => Err(FsError::backend(error)),
        }
    }
    fn visible(&self, path: &RelDir) -> layr_fs_core::Result<bool> {
        self.validate()?;
        let rel = std::str::from_utf8(path.as_bytes()).map_err(|_| FsError::Invalid("APFS legacy namespace requires UTF-8"))?;
        let joined = crate::fsutil::safe_join(&self.path, rel).map_err(FsError::backend)?;
        for (index, _) in rel.match_indices('/') {
            if Self::nested(&self.path.join(&rel[..index]))? {
                return Ok(false);
            }
        }
        Ok(crate::fsutil::exact_metadata_checked(&joined, rel)?.is_some())
    }
    fn native_attributes(&self, path: &RelDir, attributes: &EntryAttributes) -> layr_fs_core::Result<EntryAttributes> {
        let mut plain = attributes.clone();
        plain.acl = None;
        // Synchronising user attributes must not erase the destination's own state identity.
        if matches!(path, RelDir::Root) {
            if let Some(metadata) = self.directory.metadata(path)? {
                plain.xattrs.extend(metadata.attributes.xattrs.into_iter().filter(|(name, _)| name.starts_with(b"com.layr.")));
            }
        }
        Ok(plain)
    }
}
impl TreeRead for ApfsTree {
    fn link_domain(&self) -> Option<&layr_fs_core::LinkDomain> {
        self.directory.link_domain()
    }
    fn kind(&self, path: &RelDir) -> layr_fs_core::Result<Option<crate::fsutil::FType>> {
        if !self.visible(path)? {
            return Ok(None);
        }
        self.directory.kind(path)
    }
    fn metadata(&self, path: &RelDir) -> layr_fs_core::Result<Option<EntryMetadata>> {
        if !self.visible(path)? {
            return Ok(None);
        }
        let Some(mut metadata) = self.directory.metadata(path)? else { return Ok(None) };
        metadata.attributes.xattrs.retain(|(name, _)| layr_objects::fsx::keep_xattr(name));
        if metadata.kind != crate::fsutil::FType::Special {
            let entry = self.directory.open_metadata(path)?;
            metadata.attributes.acl = crate::sys::apfs_acl_text_fd(entry.as_fd())?;
        }
        Ok(Some(metadata))
    }
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> layr_fs_core::Result<()>) -> layr_fs_core::Result<()> {
        if !self.visible(dir)? {
            return Err(FsError::Invalid("APFS directory absent"));
        }
        if !matches!(dir, RelDir::Root) {
            let rel =
                std::str::from_utf8(dir.as_bytes()).map_err(|_| FsError::Invalid("APFS legacy namespace requires UTF-8"))?;
            if Self::nested(&self.path.join(rel))? {
                return Ok(());
            }
        }
        self.directory.entries(dir, visit)
    }
    fn open_file(&self, path: &RelPath) -> layr_fs_core::Result<Box<dyn ReadSeek + '_>> {
        if !self.visible(&RelDir::Path(path.clone()))? {
            return Err(FsError::Invalid("APFS file absent"));
        }
        self.directory.open_file(path)
    }
    fn read_link(&self, path: &RelPath) -> layr_fs_core::Result<Vec<u8>> {
        if !self.visible(&RelDir::Path(path.clone()))? {
            return Err(FsError::Invalid("APFS link absent"));
        }
        self.directory.read_link(path)
    }
}
impl TreeWrite for ApfsTree {
    fn put(&self, path: &RelPath, data: EntryData<'_>, attributes: &EntryAttributes) -> layr_fs_core::Result<()> {
        self.validate()?;
        let at = RelDir::Path(path.clone());
        self.directory.put_with_metadata(path, data, &self.native_attributes(&at, attributes)?, &|file| {
            Ok(crate::sys::apfs_set_acl_text_fd(file.as_fd(), attributes.acl.as_deref())?)
        })
    }
    fn set_metadata(&self, path: &RelDir, attributes: &EntryAttributes) -> layr_fs_core::Result<()> {
        self.validate()?;
        self.directory.set_metadata_with(path, &self.native_attributes(path, attributes)?, &|file| {
            Ok(crate::sys::apfs_set_acl_text_fd(file.as_fd(), attributes.acl.as_deref())?)
        })
    }
    fn rename_new(&self, source: &RelPath, destination: &RelPath) -> layr_fs_core::Result<()> {
        self.validate()?;
        self.directory.rename_new(source, destination)
    }
    fn remove_entry(&self, path: &RelPath) -> layr_fs_core::Result<()> {
        self.validate()?;
        self.directory.remove_entry(path)
    }
    fn remove_empty_dir(&self, path: &RelPath) -> layr_fs_core::Result<bool> {
        self.validate()?;
        self.directory.remove_empty_dir(path)
    }
    fn overwrite_file(
        &self,
        path: &RelPath,
        source: &mut dyn std::io::Read,
        modified_ns: i64,
    ) -> layr_fs_core::Result<Attempt<()>> {
        self.validate()?;
        self.directory.overwrite_file(path, source, modified_ns)
    }
    fn write_range(&self, path: &RelPath, offset: u64, bytes: &[u8], modified_ns: i64) -> layr_fs_core::Result<()> {
        self.validate()?;
        self.directory.write_range(path, offset, bytes, modified_ns)
    }
}
