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
pub(crate) fn capture_base(project: &Project, head: &str, previous: &str) -> String {
    #[cfg(target_os = "macos")]
    if sparse_format(project).is_some()
        && previous != head
        && project.delta_path(previous).is_dir()
        && crate::thin::chain_len(project, previous) < project.config_u64("thin.chain", crate::thin::MAX_CHAIN) as usize
    {
        return previous.to_owned();
    }
    let _ = (project, previous);
    head.to_owned()
}
impl SparseFormat {
    pub(crate) fn write_snapshot(
        self,
        project: &Project,
        id: &str,
        source: &Source,
        base: &str,
        work: &Path,
    ) -> Result<Option<OwnedCapture>> {
        let _ = (project, id, source, base, work);
        match self {
            #[cfg(target_os = "macos")]
            Self::Layer => {
                let destination = project.delta_path(id);
                let layered = source.clone().into_layered();
                match crate::thin::snapshot_layer(&layered, base, work, &destination) {
                    Ok(Some((layer, _))) => {
                        let captured = layer.freeze()?;
                        let recorded =
                            project.lock_or_borrow().and_then(|db| db.write(|tx| tx.record_base(id, base)).map_err(Into::into));
                        if let Err(error) = recorded {
                            return Err(captured.abort(error));
                        }
                        Ok(Some(captured))
                    }
                    Ok(None) => {
                        let (captured, _) = freeze_state(project, base, source, id)?;
                        let recorded =
                            project.lock_or_borrow().and_then(|db| db.write(|tx| tx.record_base(id, base)).map_err(Into::into));
                        if let Err(error) = recorded {
                            return Err(captured.abort(error));
                        }
                        Ok(Some(captured))
                    }
                    Err(error) if crate::thin::needs_complete(&error) => Ok(None),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

pub(crate) fn stored_delta(project: &Project, state: &str) -> Result<Option<(String, Vec<Vec<u8>>)>> {
    #[cfg(target_os = "macos")]
    if let Some(base) = crate::thin::layer_base(project, state)? {
        let delta = project.delta_path(state);
        let mut paths = crate::apfs::scan_shared(&delta, false)?
            .keys()
            .filter(|path| !path.is_empty())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        paths.extend(crate::thin::removed_of(&delta)?);
        return Ok(Some((base, paths.into_iter().map(String::into_bytes).collect())));
    }
    let _ = (project, state);
    Ok(None)
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
