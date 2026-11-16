//! Ports for genuine host services. They do not select save/status/merge algorithms.
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::Metadata,
    path::{Path, PathBuf},
    process::Command,
};
#[derive(Debug, Clone)]
pub(crate) struct TreeInfo {
    pub uuid: String,
    pub received_uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub readonly: bool,
}
pub(crate) trait Namespace: Send + Sync {
    fn supported(&self, path: &Path) -> bool;
    fn entry_kind(&self, _path: &Path, _metadata: &Metadata) -> Result<layr_objects::fsx::EntryKind> {
        Ok(layr_objects::fsx::EntryKind::Ordinary)
    }
    fn managed(&self, path: &Path) -> bool;
    fn managed_or_present(&self, path: &Path) -> Result<bool> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    fn info(&self, path: &Path) -> Result<TreeInfo>;
    #[cfg(test)]
    fn protect(&self, path: super::ManagedPath, protection: super::Protection) -> Result<()>;
    fn relocate(&self, path: super::ManagedPath, to: &Path) -> Result<()>;
    fn file_identity(&self, path: &Path) -> Result<Vec<u8>>;
}
#[derive(Clone, Copy)]
pub(crate) enum CloneLookup {
    ReceivedOnly,
    LocalOrReceived,
}
#[derive(Clone, Copy)]
pub(crate) struct StreamOffer {
    pub compressed: bool,
    pub clone_lookup: CloneLookup,
}
pub(crate) trait Transfer: Send + Sync {
    fn receive(&self, source: &mut dyn std::io::Read, destination: &Path, expected: &crate::receive_stream::Expect)
        -> Result<()>;
    fn offer(&self) -> StreamOffer;
    fn send(&self, parent: Option<&Path>, child: &Path, clones: &[PathBuf], compressed: bool) -> Result<Command>;
    fn clone_identity(&self, path: &Path) -> Result<(String, Option<u64>)>;
    fn send_identity(&self, path: &Path) -> Result<(String, u64)>;
}
pub(crate) trait Accounting: Send + Sync {
    fn line_spaces(&self, paths: &[PathBuf]) -> Result<Vec<Result<crate::fsutil::Space>>>;
    fn allocations(
        &self,
        root: &Path,
        parts: &[super::accounting::AllocationPart],
    ) -> Result<super::accounting::AllocationReport>;
    fn space(&self, path: &Path) -> Result<crate::fsutil::Space>;
    fn copy_need(&self, path: &Path) -> Result<u64>;
    fn copy_total(&self, path: &Path) -> Result<u64>;
}
pub(crate) trait Observations: Send + Sync {
    fn read_policy(&self) -> super::observations::ReadPolicy;
    fn begin_read(&self, roots: &[&Path], selection: Option<&BTreeSet<String>>) -> Result<super::observations::ReadObservation> {
        let entries = roots.iter().map(|root| self.scan(root, selection)).collect::<Result<Vec<_>>>()?;
        Ok(super::observations::ReadObservation::new(entries, Box::new(())))
    }
    fn scan(
        &self,
        root: &Path,
        paths: Option<&BTreeSet<String>>,
    ) -> Result<BTreeMap<String, crate::status_cache::FileFingerprint>>;
    fn stage_identity(&self, path: &Path) -> Result<Vec<u8>>;
}
pub(crate) trait ProjectCopier {
    fn native_subvolume(&self, path: &Path) -> bool;
    fn shell_dir(&mut self, src: &Path, dst: &Path) -> Result<()>;
    fn dir_close(&mut self, src: &Path, dst: &Path, metadata: &Metadata) -> Result<()>;
    fn file(&mut self, src: &Path, dst: &Path, metadata: &Metadata) -> Result<()>;
    fn symlink(&mut self, src: &Path, dst: &Path, metadata: &Metadata) -> Result<()>;
    fn subvolume(&mut self, src: &Path, dst: &Path) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}
pub(crate) struct FileCopies {
    pub unseal_cloned: Option<fn(std::os::fd::BorrowedFd<'_>) -> std::io::Result<()>>,
    pub clone_new: Option<fn(&std::fs::File, &std::fs::File, &std::ffi::OsStr) -> Result<Option<std::fs::File>>>,
    pub replace_private: Option<
        fn(&std::fs::File, &std::fs::File, &std::ffi::OsStr, &mut dyn FnMut(&std::fs::File) -> Result<()>) -> Result<bool>,
    >,
    pub replace_content: Option<fn(&std::fs::File, &std::fs::File) -> std::io::Result<layr_fs_core::Attempt<()>>>,
}
/// Optional namespace bookkeeping around ordinary descriptor operations. Only a native
/// registry needs these effects; directory traversal remains in the shared executor.
pub(crate) struct ManagedEntries {
    pub contains: fn(&Path) -> bool,
    pub rename_plain: fn(&Path, &Path, &mut dyn FnMut() -> Result<()>) -> Result<()>,
}
pub(crate) struct Registry {
    pub recover: fn(&Path) -> Result<()>,
    pub recover_existing: fn(&Path) -> Result<()>,
    pub external: fn(&Path, Vec<PathBuf>),
    pub verify: fn(&Path, &Path) -> Result<()>,
}
pub(crate) struct Host {
    pub file_copies: FileCopies,
    pub managed_entries: Option<ManagedEntries>,
    pub remove_entry: fn(&std::os::fd::OwnedFd, &str) -> Result<()>,
    pub create_entry: fn(&std::os::fd::OwnedFd, &str, &Path) -> Result<()>,
    pub nested_entry: fn(&Path, &str, &crate::fsutil::Meta) -> bool,
    pub allocation_part: Option<fn(&Path) -> Option<String>>,
    pub namespace: &'static dyn Namespace,
    pub transfer: &'static dyn Transfer,
    pub accounting: &'static dyn Accounting,
    pub observations: &'static dyn Observations,
    pub registry: Option<Registry>,
    pub prune_observations: Option<fn(&Path) -> Result<usize>>,
    pub register_cache: Option<fn(&Path) -> Result<()>>,
    pub copier: fn(&Path, &Path) -> Result<Box<dyn ProjectCopier>>,
}
