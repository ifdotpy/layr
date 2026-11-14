//! Portable filesystem contracts. Drivers own IO; this crate owns evidence and authority.
#![forbid(unsafe_code)]

pub mod evidence;
mod handles;
pub mod hints;
mod paths;

pub use handles::*;
pub use paths::*;
use std::io::{Read, Seek};

#[derive(Debug)]
pub enum FsError {
    Io(std::io::Error),
    Invalid(&'static str),
    WrongInstance,
    Busy,
    Backend(Box<dyn std::error::Error + Send + Sync>),
}
impl FsError {
    pub fn backend(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Backend(error.into())
    }
}
impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Invalid(s) => f.write_str(s),
            Self::WrongInstance => f.write_str("filesystem handle belongs to another instance"),
            Self::Busy => f.write_str("tree still has active readers"),
            Self::Backend(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Backend(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}
impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub type Result<T> = std::result::Result<T, FsError>;

/// Link identities are meaningful only within one reader's namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryMetadata {
    pub kind: evidence::FType,
    pub size: u64,
    pub link: Option<LinkIdentity>,
    pub attributes: EntryAttributes,
}
/// Attributes shared by entry kinds. The write payload alone determines the kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryAttributes {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub modified_ns: i64,
    /// None for entries whose time is not part of the stored state (subvolume placeholders).
    pub stored_time: Option<i64>,
    pub xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkIdentity {
    pub device: u64,
    pub inode: u64,
    pub count: u64,
}

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// All lookups are relative, no parent symlinks may be traversed, and absence differs from IO failure.
pub trait TreeRead: Send + Sync {
    fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>>;
    /// Immediate child names in deterministic byte order. A name is one checked component.
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()>;
    fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>>;
    fn read_link(&self, path: &RelPath) -> Result<Vec<u8>>;
}

/// Mutations required by the shared copy and merge executors. Drivers enforce exclusive tree ownership.
pub trait TreeWrite: TreeRead {
    fn put(&self, path: &RelPath, data: EntryData<'_>, attributes: &EntryAttributes) -> Result<()>;
    fn remove_entry(&self, path: &RelPath) -> Result<()>;
    fn set_metadata(&self, path: &RelDir, attributes: &EntryAttributes) -> Result<()>;
}
pub enum EntryData<'a> {
    File(&'a mut dyn Read),
    Directory,
    Symlink(&'a [u8]),
    HardLink(&'a RelPath),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureConsistency {
    AtomicPoint,
    ValidatedCopy,
    UnsettledCopy,
}
impl CaptureConsistency {
    /// Preserve the existing signed record's `running` semantics explicitly at publication.
    pub fn running(self) -> bool {
        matches!(self, Self::UnsettledCopy)
    }
}
#[must_use]
pub struct Capture {
    pub tree: FrozenTree,
    pub consistency: CaptureConsistency,
    pub ownership: OwnedTree,
}

/// Core changes are a complete candidate superset; hints alone may be partial.
pub trait Filesystem: Send + Sync {
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> Result<CreatedTree>;
    fn freeze(&self, source: &WorkingTree, dst: NewTreeLocation) -> Result<Capture>;
    fn changes(&self, before: &FrozenTree, after: &FrozenTree) -> Result<Vec<evidence::EntryChange>>;
    fn remove(&self, tree: OwnedTree) -> std::result::Result<(), RemovalFailure>;
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_>;
}

#[derive(Default)]
pub struct Capabilities<'a> {
    pub change_hints: Option<&'a dyn hints::ChangeHints>,
    pub clone_new: Option<&'a dyn CloneToNew>,
    pub reflink_range: Option<&'a dyn ReflinkRange>,
}

/// Unavailable guarantees that the destination was not changed, so full copying is safe.
#[must_use]
pub enum Attempt<T> {
    Applied(T),
    Unavailable,
}

/// Clone into an exclusively prepared destination, preserving the requested entry's metadata.
pub trait CloneToNew: Send + Sync {
    fn clone_new(&self, source: &FrozenTree, path: &RelPath, dst: &WorkingTree, target: &RelPath) -> Result<Attempt<()>>;
}
pub trait ReflinkRange: Send + Sync {
    fn clone_range(&self, source: &std::fs::File, range: ByteRange, dst: &std::fs::File, offset: u64) -> Result<Attempt<()>>;
}
#[derive(Clone, Copy, Debug)]
pub struct ByteRange {
    offset: u64,
    length: std::num::NonZeroU64,
}
impl ByteRange {
    pub fn new(offset: u64, length: u64) -> Result<Self> {
        let length = std::num::NonZeroU64::new(length).ok_or(FsError::Invalid("empty byte range"))?;
        offset.checked_add(length.get()).ok_or(FsError::Invalid("byte range overflow"))?;
        Ok(Self { offset, length })
    }
    pub fn offset(self) -> u64 {
        self.offset
    }
    pub fn length(self) -> u64 {
        self.length.get()
    }
}
