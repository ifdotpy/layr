//! Persisted representation IO. Shared callers choose logical sources and requested output;
//! a native layer format is used only by the adapter that already stores that format.
use super::*;
use crate::{
    fs::{OwnedCapture, OwnedWorking},
    store::Project,
};

pub(crate) fn freeze_state(project: &Project, base: &str, source: &Source, id: &str) -> Result<(OwnedCapture, Source)> {
    #[cfg(target_os = "macos")]
    if !project.is_overlay() {
        let path = project.delta_path(id);
        crate::thin::ensure_parent(&path)?;
        let driver = crate::apfs::driver::ApfsFs::for_destination(&path, crate::apfs::driver::Links::Keep)?;
        let result = (|| -> layr_fs_core::Result<_> {
            let location = driver.location(&path)?;
            let root = layr_fs_common::required(source, &RelDir::Root)?.attributes;
            driver.frozen_layer(root, layr_types::StateId::parse(base).map_err(FsError::backend)?, location)
        })();
        let captured = result.map_err(|error| crate::fs::operation_error(driver.clone(), error))?;
        let owned = OwnedCapture::new(driver, captured);
        let source = match crate::thin::Layered::over(&path, source.clone().into_layered()) {
            Ok(source) => source,
            Err(error) => return Err(owned.abort(error)),
        };
        return Ok((owned, source.into()));
    }
    let _ = base;
    let path = project.state_path(id);
    Ok((crate::fs::freeze_source(source, &path)?, Source::native(&path)))
}
pub(crate) fn create_stage(source: &Source, path: &Path, base: Option<&str>) -> Result<OwnedWorking> {
    #[cfg(target_os = "macos")]
    if let Some(base) = base {
        let driver = crate::apfs::driver::ApfsFs::for_destination(path, crate::apfs::driver::Links::Keep)?;
        let result = (|| -> layr_fs_core::Result<_> {
            let root = layr_fs_common::required(source, &RelDir::Root)?.attributes;
            driver.working_layer(root, layr_types::StateId::parse(base).map_err(FsError::backend)?, driver.location(path)?)
        })();
        let created = result.map_err(|error| crate::fs::operation_error(driver.clone(), error))?;
        return Ok(OwnedWorking::new(driver, created));
    }
    let _ = base;
    if let Some(native) = source.folder() {
        return crate::fs::copy_capture_owned(native, path, crate::fs::CapturePurpose::State).map(|(created, _)| created);
    }
    crate::fs::materialise_owned(source, path)
}

pub(crate) enum LayeredStorage {
    #[cfg(target_os = "macos")]
    Layer(crate::thin::Layered),
}
impl LayeredStorage {
    pub(crate) fn for_import(self, project: &Project, base: &str) -> Option<Self> {
        let _ = (project, base);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer(_)
                if !project.is_overlay()
                    && crate::thin::chain_len(project, base)
                        < project.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize =>
            {
                Some(self)
            }
            #[cfg(target_os = "macos")]
            _ => None,
        }
    }
    pub(crate) fn write_import(
        self,
        project: &Project,
        source: &Path,
        base: &str,
        state: &str,
        written: std::collections::BTreeSet<String>,
        removed: std::collections::BTreeSet<String>,
    ) -> Result<OwnedCapture> {
        let _ = (project, source, base, state, &written, &removed);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer(_) => {
                let diff = crate::thin::Diff { write: written, removed, whole: Default::default() };
                let captured = crate::thin::write_layer_owned(source, &project.delta_path(state), &diff, base, None)?.freeze()?;
                match project.lock_or_borrow()?.write(|tx| tx.record_base(state, base)) {
                    Ok(()) => Ok(captured),
                    Err(crate::db::CommitResolution::NotCommitted(error)) => Err(captured.abort(error)),
                    Err(error) => {
                        captured.release();
                        Err(error.into())
                    }
                }
            }
        }
    }

    pub(crate) fn stage_entries(
        self,
        source: &Path,
        paths: &std::collections::BTreeSet<String>,
        owner: Option<(u32, u32)>,
    ) -> Result<()> {
        let _ = (source, paths, owner);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer(layered) => crate::thin::stage_entries(&layered, source, paths, owner),
        }
    }
    pub(crate) fn publish_complete(self, created: OwnedWorking, stage: &Path) -> Result<()> {
        let _ = (&created, stage);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer(layered) => {
                anyhow::ensure!(layered.top() == stage, "layered index location changed");
                created.exchange_existing(crate::fs::ManagedPath::claim(stage))
            }
        }
    }
    pub(crate) fn record_base(self, project: &Project, id: &str) -> Result<()> {
        let _ = (project, id);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer(layered) => {
                let base =
                    crate::apfs::native_info(layered.top())?.base.ok_or_else(|| anyhow::anyhow!("layer has no base identity"))?;
                Ok(project.lock_or_borrow()?.write(|tx| tx.record_base(id, &base))?)
            }
        }
    }
    pub(crate) fn forget_base(self, project: &Project, id: &str) -> Result<()> {
        let _ = (project, id);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer(_) => Ok(project.lock_or_borrow()?.write(|tx| tx.forget_base(id))?),
        }
    }
}
impl Source {
    pub(crate) fn layered_storage(&self) -> Option<LayeredStorage> {
        #[cfg(target_os = "macos")]
        if let Self::Layers(layered) = self {
            if !layered.is_complete() {
                return Some(LayeredStorage::Layer(layered.clone()));
            }
        }
        None
    }
}

/// Existing sparse persistence, selected by representation composition. A build without that
/// format cannot produce this token and follows the same caller's complete-capture fallback.
pub(crate) enum SparseFormat {
    #[cfg(target_os = "macos")]
    Layer,
}
pub(crate) fn sparse_format(project: &Project) -> Option<SparseFormat> {
    #[cfg(target_os = "macos")]
    if !project.is_overlay() {
        return Some(SparseFormat::Layer);
    }
    let _ = project;
    None
}
impl SparseFormat {
    pub(crate) fn write_snapshot_at(
        self,
        source: &Source,
        base: &str,
        work: &Path,
        destination: &Path,
    ) -> Result<Option<OwnedCapture>> {
        let _ = (source, base, work, destination);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer => {
                let layered = source.clone().into_layered();
                match crate::thin::snapshot_layer(&layered, base, work, destination) {
                    Ok(Some((layer, _))) => Ok(Some(layer.freeze()?)),
                    Ok(None) => {
                        let root = layr_fs_common::required(source, &RelDir::Root)?.attributes;
                        let driver = crate::apfs::driver::ApfsFs::for_destination(destination, crate::apfs::driver::Links::Keep)?;
                        let captured = driver
                            .frozen_layer(root, layr_types::StateId::parse(base)?, driver.location(destination)?)
                            .map_err(|error| crate::fs::operation_error(driver.clone(), error))?;
                        Ok(Some(OwnedCapture::new(driver, captured)))
                    }
                    Err(error) if crate::thin::needs_complete(&error) => Ok(None),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

/// PreparedTree::source and Layered::of_state resolve the top layer over its marker base.
/// That base can be a newer automatic save than the state's logical parent.
pub(crate) fn capture_delta(source: &Source) -> Result<Option<(String, Vec<Vec<u8>>)>> {
    #[cfg(target_os = "macos")]
    if let Source::Layers(layered) = source {
        return layer_delta(layered.top());
    }
    let _ = source;
    Ok(None)
}

pub(crate) fn stored_delta(project: &Project, state: &str) -> Result<Option<(String, Vec<Vec<u8>>)>> {
    #[cfg(target_os = "macos")]
    if project.delta_path(state).is_dir() {
        return layer_delta(&project.delta_path(state));
    }
    let _ = (project, state);
    Ok(None)
}

#[cfg(target_os = "macos")]
fn layer_delta(root: &Path) -> Result<Option<(String, Vec<Vec<u8>>)>> {
    let Some(base) = crate::apfs::native_info(root)?.base else { return Ok(None) };
    let scan = crate::apfs::scan_shared(root, false)?;
    let paths = layer_paths(scan.keys().cloned(), crate::thin::removed_of(root)?);
    Ok(Some((base, paths)))
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn layer_paths(written: impl IntoIterator<Item = String>, removed: impl IntoIterator<Item = String>) -> Vec<Vec<u8>> {
    written
        .into_iter()
        .chain(removed)
        .filter(|path| !path.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(String::into_bytes)
        .collect()
}

pub(crate) fn complete(project: &Project, id: &str) -> Result<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        return crate::thin::complete_tree(project, id);
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(project.state_path(id))
    }
}
pub(crate) fn prepare_object_read(project: &Project, id: &str) -> Result<Option<std::path::PathBuf>> {
    #[cfg(target_os = "macos")]
    if project.delta_path(id).is_dir() {
        if project.state_path(id).is_dir() {
            crate::thin::touch_view(project, id);
        } else if crate::thin::chain_len(project, id) > project.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize {
            return complete(project, id).map(Some);
        }
    }
    let _ = (project, id);
    Ok(None)
}
pub(crate) fn recovery_retains(project: &Project, id: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::apfs_commit::preserves(project, id).unwrap_or(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (project, id);
        false
    }
}
