//! Physical preparation and publication of an index. Comparison, checks, object writing,
//! record construction and presentation remain in the shared command implementation.
use super::*;
use crate::{fs::OwnedCapture, model::Line, repo::Repo};
use anyhow::{Context, Result};
use std::fs::File;

enum Storage<'p, 's> {
    Captured(OwnedCapture),
    #[cfg(target_os = "macos")]
    Journal(crate::apfs_commit::PendingCommit<'p, 's>),
    Catalog(&'p crate::store::Project<'s>),
}
pub(crate) struct IndexPublication<'p, 's> {
    project: &'p crate::store::Project<'s>,
    id: layr_types::StateId,
    subvol: String,
    source: Option<Source>,
    storage: Storage<'p, 's>,
    _pin: crate::free::Pin,
}
impl<'p, 's> IndexPublication<'p, 's> {
    pub(crate) fn prepare(repo: &'p Repo<'s>, line: &Line, head: &Source) -> Result<Self> {
        let stage = repo.current_stage(line)?;
        #[cfg(target_os = "macos")]
        if stage.is_some() {
            let pending = crate::apfs_commit::PendingCommit::prepare(&repo.p, &line.id)?;
            crate::store::failpoint("pending-cloned");
            let pin = crate::free::prepare_state(&repo.p, &pending.id)?;
            let source = if pending.thin {
                crate::thin::Layered::over(&pending.tree, repo.head_src(line)?.into_layered())?.into()
            } else {
                Source::native(&pending.tree)
            };
            return Ok(Self {
                project: &repo.p,
                id: layr_types::StateId::parse(&pending.id)?,
                subvol: pending.subvol.clone(),
                source: Some(source),
                storage: Storage::Journal(pending),
                _pin: pin,
            });
        }
        let id = layr_types::StateId::parse(&crate::ids::new_id())?;
        let pin = crate::free::prepare_state(&repo.p, id.as_str())?;
        let source = match stage {
            Some(stage) => repo.stage_tree(line, &stage)?,
            None => head.clone(),
        };
        let (captured, source) = super::storage::freeze_state(&repo.p, &line.head, &source, id.as_str())?;
        let info = match crate::fs::host().namespace.info(source.root_hint()) {
            Ok(info) => info,
            Err(error) => {
                drop(source);
                return Err(captured.abort(error));
            }
        };
        let publication = Self {
            project: &repo.p,
            id,
            subvol: info.uuid,
            source: Some(source),
            storage: Storage::Captured(captured),
            _pin: pin,
        };
        if let Some(layered) = publication.source()?.layered_storage() {
            layered.record_base(&repo.p, publication.id.as_str())?;
        }
        Ok(publication)
    }
    pub(crate) fn id(&self) -> &str {
        self.id.as_str()
    }
    pub(crate) fn subvol(&self) -> &str {
        &self.subvol
    }
    pub(crate) fn source(&self) -> Result<&Source> {
        self.source.as_ref().context("index preparation was already published")
    }
    pub(crate) fn consistency(&self) -> layr_fs_core::Result<CaptureConsistency> {
        match &self.storage {
            Storage::Captured(capture) => Ok(capture.consistency()),
            #[cfg(target_os = "macos")]
            Storage::Journal(_) => Ok(CaptureConsistency::ValidatedCopy),
            _ => Err(FsError::Invalid("index preparation was already published")),
        }
    }
    pub(crate) fn synchronize(&self) -> Result<()> {
        match &self.storage {
            Storage::Captured(_) => File::open(self.source()?.root_hint())?.sync_all()?,
            #[cfg(target_os = "macos")]
            Storage::Journal(pending) => pending.sync_tree()?,
            _ => anyhow::bail!("index preparation was already published"),
        }
        crate::store::failpoint("pending-synced");
        Ok(())
    }
    pub(crate) fn publish(&mut self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        if let Storage::Journal(pending) = &mut self.storage {
            if !pending.publish()? {
                return Ok(false);
            }
        }
        if let Storage::Captured(_) = &self.storage {
            let path = self.source()?.root_hint();
            let info = crate::fs::host().namespace.info(path)?;
            anyhow::ensure!(info.readonly && info.uuid == self.subvol, "prepared index changed before publication");
        }
        // A read view can retain open mount descriptors. Release it before catalog failure
        // handling takes over cleanup of a published location.
        self.source.take();
        Ok(true)
    }
    pub(crate) fn record_attempted(&mut self) {
        if matches!(self.storage, Storage::Captured(_)) {
            if let Storage::Captured(capture) = std::mem::replace(&mut self.storage, Storage::Catalog(self.project)) {
                capture.release();
            }
        }
    }
    pub(crate) fn publication_failed(&mut self, error: &crate::db::PublishError) {
        #[cfg(target_os = "macos")]
        if let Storage::Journal(pending) = &mut self.storage {
            pending.publication_failed(error);
        }
        let _ = error;
    }
    pub(crate) fn finish(mut self) -> Result<()> {
        self.source.take();
        let storage = std::mem::replace(&mut self.storage, Storage::Catalog(self.project));
        match storage {
            #[cfg(target_os = "macos")]
            Storage::Journal(pending) => pending.finish(),
            Storage::Catalog(project) => {
                anyhow::ensure!(project.db.state(self.id.as_str())?.is_some(), "prepared index has no saved record");
                Ok(())
            }
            _ => anyhow::bail!("index preparation was not published"),
        }
    }
}
impl Drop for IndexPublication<'_, '_> {
    fn drop(&mut self) {
        let layered = self.source.take().and_then(|source| source.layered_storage());
        if matches!(self.storage, Storage::Captured(_)) {
            if let Storage::Captured(capture) = std::mem::replace(&mut self.storage, Storage::Catalog(self.project)) {
                match capture.discard() {
                    Ok(()) => {
                        if let Some(layered) = layered {
                            if let Err(error) = layered.forget_base(self.project, self.id.as_str()) {
                                eprintln!("warning: unpublished state metadata cleanup failed: {error:#}");
                            }
                        }
                    }
                    Err(error) => eprintln!("warning: {error:#}"),
                }
            }
        }
    }
}
