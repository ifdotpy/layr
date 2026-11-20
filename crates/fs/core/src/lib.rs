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
    SymlinkAncestor,
    WrongInstance,
    LocationChanged,
    Busy,
    Cleanup { cause: Box<FsError>, failure: Box<RemovalFailure> },
    Backend(Box<dyn std::error::Error + Send + Sync>),
}
impl FsError {
    /// Namespace traversal rejects symlink ancestors even when an adapter adds context.
    pub fn is_symlink_ancestor(&self) -> bool {
        std::iter::successors(Some(self as &(dyn std::error::Error + 'static)), |cause| cause.source())
            .any(|cause| matches!(cause.downcast_ref::<Self>(), Some(Self::SymlinkAncestor)))
    }

    pub fn backend(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Backend(error.into())
    }
}
impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Invalid(s) => f.write_str(s),
            Self::SymlinkAncestor => f.write_str("path goes through a symbolic link"),
            Self::WrongInstance => f.write_str("filesystem handle belongs to another instance"),
            Self::LocationChanged => f.write_str("owned tree location changed; deletion authority retained"),
            Self::Busy => f.write_str("tree still has active readers"),
            Self::Cleanup { cause, failure } => write!(f, "{cause}; cleanup requires retry: {}", failure.error),
            Self::Backend(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Cleanup { cause, .. } => Some(cause.as_ref()),
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

/// The metadata needed by a caller and how much content detail it will consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeScope {
    Content,
    Stored,
    FullState,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeDetail {
    Equality,
    Ranges,
}
#[derive(Clone, Copy, Debug)]
pub struct ChangeRequest {
    pub scope: ChangeScope,
    pub detail: ChangeDetail,
}
impl Default for ChangeRequest {
    fn default() -> Self {
        Self { scope: ChangeScope::FullState, detail: ChangeDetail::Ranges }
    }
}

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
    /// Native ACL text, without backend sealing entries. Portable formats may reject it.
    pub acl: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkIdentity {
    pub device: u64,
    pub inode: u64,
    pub count: u64,
}

/// Equality of link identifiers is meaningful across readers only when they share this domain.
#[derive(Clone, Default)]
pub struct LinkDomain(std::sync::Arc<()>);
impl PartialEq for LinkDomain {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for LinkDomain {}
impl std::hash::Hash for LinkDomain {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(std::sync::Arc::as_ptr(&self.0), state)
    }
}

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// All lookups are relative, no parent symlinks may be traversed, and absence differs from IO failure.
pub trait TreeRead: Send + Sync {
    /// Driver-local evidence adapters may recognise each other. Shared algorithms never
    /// downcast readers and always retain the complete-enumeration fallback.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
    fn candidate_evidence(
        &self,
        _after: &dyn TreeRead,
        _request: ChangeRequest,
        _selection: Option<&std::collections::BTreeSet<RelPath>>,
    ) -> Result<hints::HintSet> {
        Ok(hints::HintSet::Partial)
    }
    /// None confines link identities to this reader. Adapters may share an explicit domain.
    fn link_domain(&self) -> Option<&LinkDomain> {
        None
    }
    /// Namespace lookup without loading user attributes. Default readers reuse metadata.
    fn kind(&self, path: &RelDir) -> Result<Option<evidence::FType>> {
        Ok(self.metadata(path)?.map(|metadata| metadata.kind))
    }
    fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>>;
    /// Immediate child names in deterministic byte order. A name is one checked component.
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()>;
    fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>>;
    fn read_link(&self, path: &RelPath) -> Result<Vec<u8>>;
}

/// Mutations required by the shared copy and merge executors. Drivers enforce exclusive tree ownership.
pub trait TreeWrite: TreeRead {
    fn put(&self, path: &RelPath, data: EntryData<'_>, attributes: &EntryAttributes) -> Result<()>;
    /// Move an entry/subtree without replacing a destination; preserve link identities.
    fn rename_new(&self, source: &RelPath, destination: &RelPath) -> Result<()>;
    fn remove_entry(&self, path: &RelPath) -> Result<()>;
    /// Remove only an empty directory; false means it still contains entries.
    fn remove_empty_dir(&self, path: &RelPath) -> Result<bool>;
    /// Mutate an existing regular file, preserving its inode and every hard-link alias.
    /// Unavailable is returned before reading the source or changing the destination.
    fn overwrite_file(&self, path: &RelPath, source: &mut dyn Read, modified_ns: i64) -> Result<Attempt<()>>;
    /// Overwrite an in-bounds byte range without truncating or changing link identities.
    fn write_range(&self, path: &RelPath, offset: u64, bytes: &[u8], modified_ns: i64) -> Result<()>;
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
/// Capture a backend-affine working tree, or materialise and seal a logical reader.
/// The distinction preserves wrong-instance rejection without exporting logical sources.
pub enum FreezeSource<'a> {
    Working(&'a WorkingTree),
    Reader(&'a dyn TreeRead),
    Captured(Capture),
}
pub trait Filesystem: Send + Sync {
    /// Create an empty working tree with the driver's normal creation defaults.
    fn create_empty(&self, dst: NewTreeLocation) -> Result<CreatedTree>;
    fn materialise(&self, source: &dyn TreeRead, dst: NewTreeLocation) -> Result<CreatedTree>;
    fn freeze(&self, source: FreezeSource<'_>, dst: NewTreeLocation) -> Result<Capture>;
    fn changes(
        &self,
        before: &FrozenTree,
        after: &FrozenTree,
        request: crate::ChangeRequest,
    ) -> Result<Vec<evidence::EntryChange>>;
    fn remove(&self, tree: OwnedTree) -> std::result::Result<(), RemovalFailure>;
    fn capabilities(&self, scope: &TreeScope) -> Capabilities<'_>;
}

#[derive(Default)]
pub struct Capabilities<'a> {
    pub exchange: Option<&'a dyn ExchangeTrees>,
    pub capture_copy: Option<&'a dyn CaptureCopy>,
    pub change_hints: Option<&'a dyn hints::ChangeHints>,
    pub clone_new: Option<&'a dyn CloneToNew>,
    pub reflink_range: Option<&'a dyn ReflinkRange>,
}

/// Ownership guarantees of a writable capture. Used when handing a cache to another user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerLayout {
    PerEntry,
    Uniform,
}
#[must_use]
pub struct WritableCapture {
    pub created: CreatedTree,
    pub consistency: CaptureConsistency,
    pub owners: OwnerLayout,
}
/// A shortcut for freezing a working tree then materialising that captured namespace.
/// It preserves capture semantics (including nested-tree boundaries), rather than promising
/// native snapshot support or changing full logical materialisation of live readers.
pub trait CaptureCopy: Send + Sync {
    /// Capture-based candidate evidence is cheaper than enumerating the live namespace.
    fn prefers_capture_for_changes(&self) -> bool {
        false
    }
    /// Preparing a capture before taking an application lock avoids expensive work under it.
    fn prepare_before_lock(&self) -> bool {
        false
    }
    fn copy_capture(&self, source: &WorkingTree, dst: NewTreeLocation) -> Result<WritableCapture>;
}

/// Unavailable guarantees that the destination was not changed, so full copying is safe.
#[must_use]
pub enum Attempt<T> {
    Applied(T),
    Unavailable(std::io::Error),
}

/// A file clone can read a captured tree, or a live tree whose surrounding operation
/// validates its observations. A live entry clone makes no whole-tree consistency claim.
#[derive(Clone, Copy)]
pub enum CloneSource<'a> {
    Frozen(&'a FrozenTree),
    Working(&'a WorkingTree),
}
impl CloneSource<'_> {
    pub fn reader(&self) -> &dyn TreeRead {
        match self {
            Self::Frozen(tree) => tree.reader(),
            Self::Working(tree) => tree.reader(),
        }
    }
}
/// Clone into an exclusively prepared destination, preserving the requested entry's metadata.
pub trait CloneToNew: Send + Sync {
    fn clone_new(&self, source: CloneSource<'_>, path: &RelPath, dst: &WorkingTree, target: &RelPath) -> Result<Attempt<()>>;
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

/// Atomic replacement of two owned tree locations, without accepting a readable path as
/// authority. A failed post-exchange validation retains progress and both receipts for retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExchangeProgress {
    Unchanged,
    Swapped,
}
pub struct ExchangeError {
    pub error: FsError,
    pub progress: ExchangeProgress,
}
impl From<std::io::Error> for ExchangeError {
    fn from(error: std::io::Error) -> Self {
        FsError::Io(error).into()
    }
}
impl From<FsError> for ExchangeError {
    fn from(error: FsError) -> Self {
        Self { error, progress: ExchangeProgress::Unchanged }
    }
}
#[must_use]
pub struct ExchangeFailure {
    pub error: FsError,
    pub progress: ExchangeProgress,
    pub first: OwnedTree,
    pub second: OwnedTree,
}
impl std::fmt::Debug for ExchangeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
pub trait ExchangeTrees: Send + Sync {
    fn exchange(&self, first: OwnedTree, second: OwnedTree) -> std::result::Result<(OwnedTree, OwnedTree), ExchangeFailure>;
}
