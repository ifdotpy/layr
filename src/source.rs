//! Immutable source representations. Native folders, objects and persisted layers share one
//! reader contract; exporting paths is reserved for native tools.
use crate::fsutil::Src;
pub(crate) mod publication;
pub(crate) mod storage;
use anyhow::Result;
use layr_fs_core::*;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
#[derive(Clone)]
pub(crate) struct NativeSource {
    path: PathBuf,
    reader: Arc<OnceLock<std::result::Result<Arc<dyn TreeRead>, Arc<FsError>>>>,
}
impl std::fmt::Debug for NativeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}
#[derive(Clone, Debug)]
pub enum Source {
    Folder(NativeSource),
    Objects(Arc<crate::object_tree::ObjectTree>),
    #[cfg(target_os = "macos")]
    Layers(crate::thin::Layered),
}
impl Source {
    pub fn native(path: &Path) -> Self {
        Self::Folder(NativeSource { path: path.to_path_buf(), reader: Default::default() })
    }
    fn reader(&self) -> layr_fs_core::Result<&dyn TreeRead> {
        match self {
            Self::Folder(native) => match native.reader.get_or_init(|| crate::fs::reader(&native.path).map_err(Arc::new)) {
                Ok(reader) => Ok(reader.as_ref()),
                Err(error) => Err(FsError::backend(error.clone())),
            },
            Self::Objects(objects) => Ok(objects.as_ref()),
            #[cfg(target_os = "macos")]
            Self::Layers(layered) => Ok(layered),
        }
    }
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Folder(a), Self::Folder(b)) => a.path == b.path,
            (Self::Objects(a), Self::Objects(b)) => {
                a.store.root() == b.store.root() && a.refs.tree == b.refs.tree && a.refs.times == b.refs.times
            }
            #[cfg(target_os = "macos")]
            (Self::Layers(a), Self::Layers(b)) => {
                a.layers().len() == b.layers().len()
                    && a.layers().iter().zip(b.layers()).all(|(a, b)| a.root == b.root && a.removed == b.removed)
                    && match (a.object_base(), b.object_base()) {
                        (None, None) => true,
                        (Some(a), Some(b)) => {
                            a.store.root() == b.store.root() && a.refs.tree == b.refs.tree && a.refs.times == b.refs.times
                        }
                        _ => false,
                    }
            }
            _ => false,
        }
    }
    /// A namespace prefix for diagnostics and rule matching; it does not export objects.
    pub(crate) fn root_hint(&self) -> &Path {
        match self {
            Self::Folder(native) => &native.path,
            Self::Objects(objects) => objects.root(),
            #[cfg(target_os = "macos")]
            Self::Layers(layered) => layered.top(),
        }
    }
    pub fn path(&self, relative: &str) -> Result<PathBuf> {
        self.at(relative)
    }
    pub(crate) fn of_state(project: &crate::store::Project, id: &str) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Ok(crate::thin::Layered::of_state(project, id)?.into())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(Self::native(&project.state_path(id)))
        }
    }
    pub(crate) fn stage(path: &Path, base: Self) -> Result<Self> {
        #[cfg(target_os = "macos")]
        if crate::thin::is_thin(path) {
            return Ok(crate::thin::Layered::over(path, base.into_layered())?.into());
        }
        let _ = base;
        Ok(Self::native(path))
    }
    pub(crate) fn below(&self) -> Option<OverlayBase> {
        #[cfg(target_os = "macos")]
        if let Self::Layers(layered) = self {
            return layered.below().map(OverlayBase::Thin);
        }
        None
    }
}
impl From<PathBuf> for Source {
    fn from(path: PathBuf) -> Self {
        Self::native(&path)
    }
}
impl Src for Source {
    fn core_reader(&self) -> Option<&dyn TreeRead> {
        Some(self)
    }
    fn at(&self, relative: &str) -> Result<PathBuf> {
        match self {
            Self::Folder(native) => native.path.at(relative),
            Self::Objects(objects) => objects.path(relative),
            #[cfg(target_os = "macos")]
            Self::Layers(layered) => layered.path(relative),
        }
    }
    fn folder(&self) -> Option<&Path> {
        match self {
            Self::Folder(native) => Some(&native.path),
            _ => None,
        }
    }
    fn objects(&self) -> Option<&crate::object_tree::ObjectTree> {
        match self {
            Self::Objects(objects) => Some(objects),
            _ => None,
        }
    }
}
impl TreeRead for Source {
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.reader().ok().and_then(TreeRead::as_any)
    }
    fn candidate_evidence(
        &self,
        after: &dyn TreeRead,
        request: ChangeRequest,
        selection: Option<&std::collections::BTreeSet<RelPath>>,
    ) -> layr_fs_core::Result<hints::HintSet> {
        self.reader()?.candidate_evidence(after, request, selection)
    }
    fn link_domain(&self) -> Option<&LinkDomain> {
        self.reader().ok().and_then(TreeRead::link_domain)
    }
    fn kind(&self, path: &RelDir) -> layr_fs_core::Result<Option<crate::fsutil::FType>> {
        self.reader()?.kind(path)
    }
    fn metadata(&self, path: &RelDir) -> layr_fs_core::Result<Option<EntryMetadata>> {
        self.reader()?.metadata(path)
    }
    fn entries(&self, path: &RelDir, visit: &mut dyn FnMut(&RelPath) -> layr_fs_core::Result<()>) -> layr_fs_core::Result<()> {
        self.reader()?.entries(path, visit)
    }
    fn open_file(&self, path: &RelPath) -> layr_fs_core::Result<Box<dyn ReadSeek + '_>> {
        self.reader()?.open_file(path)
    }
    fn read_link(&self, path: &RelPath) -> layr_fs_core::Result<Vec<u8>> {
        self.reader()?.read_link(path)
    }
}
#[cfg(target_os = "macos")]
impl From<crate::thin::Layered> for Source {
    fn from(layered: crate::thin::Layered) -> Self {
        if layered.is_complete() {
            return Self::native(layered.bottom());
        }
        if layered.layers().is_empty() {
            if let Some(objects) = layered.object_base() {
                return Self::Objects(objects.clone());
            }
        }
        Self::Layers(layered)
    }
}
#[cfg(target_os = "macos")]
impl Source {
    pub(crate) fn into_layered(self) -> crate::thin::Layered {
        match self {
            Self::Folder(native) => crate::thin::Layered::complete(&native.path),
            Self::Objects(objects) => crate::thin::Layered::objects(objects),
            Self::Layers(layered) => layered,
        }
    }
}
#[cfg(target_os = "macos")]
impl From<&Source> for crate::thin::Layered {
    fn from(source: &Source) -> Self {
        source.clone().into_layered()
    }
}
/// Builds without persisted layers cannot construct an overlay attachment token.
pub(crate) enum OverlayBase {
    #[cfg(target_os = "macos")]
    Thin(crate::thin::Layered),
}
impl OverlayBase {
    pub(crate) fn attach(self, _path: &Path) -> Result<Source> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Thin(base) => Ok(crate::thin::Layered::over(_path, base)?.into()),
        }
    }
}
