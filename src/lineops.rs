//! Batch line creation and the safe snapshot path for hard reset.

use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Line, LineChange, Record};
use crate::repo::Repo;
use anyhow::{bail, Result};
#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;

/// A refused delete must not touch a replacement line or working folder.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DeleteRefused {
    LineMissing(layr_types::LineId),
    FolderChanged(layr_types::LineId),
}

impl std::fmt::Display for DeleteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineMissing(id) => write!(f, "line id {id} no longer exists; refusing to delete"),
            Self::FolderChanged(id) => write!(f, "working folder of line id {id} changed; refusing to delete"),
        }
    }
}
impl std::error::Error for DeleteRefused {}

/// Exact lookup: deleted rows remain in the catalog and must never be resolved by name.
pub(crate) fn live_line(repo: &Repo, id: layr_types::LineId) -> Result<Line> {
    let key = id.to_string();
    if repo.p.db.line_deleted(&key)? {
        return Err(DeleteRefused::LineMissing(id).into());
    }
    let line = repo.p.db.line_by_id(&key)?;
    repo.localize(line.ok_or(DeleteRefused::LineMissing(id))?)
}

/// Captured under the project lock together with name resolution. Keep metadata, not an open
/// directory handle: another delete must be able to unmount this folder while preparation runs.
pub(crate) struct DeleteTarget {
    id: layr_types::LineId,
    path: PathBuf,
    folder: Option<std::fs::Metadata>,
}

impl DeleteTarget {
    pub(crate) fn capture(repo: &Repo, line: &Line) -> Result<Self> {
        let id = layr_types::LineId::parse(&line.id)?;
        let path = repo.working(line);
        let folder = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => Some(meta),
            Ok(_) => return Err(DeleteRefused::FolderChanged(id).into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let target = Self { id, path, folder };
        target.validate(repo)?;
        Ok(target)
    }

    pub(crate) fn has_folder(&self) -> bool {
        self.folder.is_some()
    }

    /// The caller holds the project lock through the subsequent record and folder removal.
    pub(crate) fn validate(&self, repo: &Repo) -> Result<Line> {
        use std::os::unix::fs::MetadataExt;
        let line = live_line(repo, self.id)?;
        if repo.working(&line) != self.path {
            return Err(DeleteRefused::FolderChanged(self.id).into());
        }
        let same = match (&self.folder, std::fs::symlink_metadata(&self.path)) {
            (Some(before), Ok(now)) => now.is_dir() && before.dev() == now.dev() && before.ino() == now.ino(),
            (None, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => true,
            (_, Err(e)) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => false,
        };
        if !same {
            return Err(DeleteRefused::FolderChanged(self.id).into());
        }
        Ok(line)
    }
}

fn batch_names(prefix: &str, count: usize) -> Result<Vec<String>> {
    if count == 0 {
        bail!("--count must be greater than zero");
    }
    crate::store::check_name("line", prefix)?;
    (1..=count)
        .map(|n| {
            let name = format!("{prefix}-{n}");
            crate::store::check_name("line", &name)?;
            Ok(name)
        })
        .collect()
}

/// Submit standard snapshots concurrently so the kernel can share transaction commits.
#[cfg(not(target_os = "macos"))]
fn snapshots(src: &Path, destinations: &[PathBuf]) -> Result<()> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        let workers = destinations.len().min(8);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|worker| {
                    scope.spawn(move || -> Result<()> {
                        for dst in destinations.iter().skip(worker).step_by(workers) {
                            crate::storage::snapshot(src, dst, false)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            // Join every worker before any rollback can remove its destination.
            let mut error = None;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error = Some(e),
                    Err(_) => error = Some(anyhow::anyhow!("snapshot worker failed")),
                }
            }
            match error {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
    }
    #[cfg(feature = "overlayfs")]
    {
        for dst in destinations {
            crate::storage::snapshot(src, dst, false)?;
        }
        Ok(())
    }
}

/// Line folders made without the project lock, waiting for `commit_prepared_lines`.
pub(crate) struct PreparedLines {
    start: String,
    pub(crate) lines: Vec<Line>,
    tmps: Vec<PathBuf>,
    recs: Vec<Record>,
    /// The line that the lines start from: an OverlayFS commit takes its caches (`commit_prepared_lines`).
    #[cfg(feature = "overlayfs")]
    source: Option<Line>,
}

impl Repo<'_> {
    /// The names of a batch: `<prefix>-1` to `<prefix>-<count>`, all valid.
    pub fn batch_line_names(&self, prefix: &str, count: usize) -> Result<Vec<String>> {
        batch_names(prefix, count)
    }

    /// Create lines with the project lock held (the caller took it). The lock-free command path
    /// calls `prepare_named_lines` first and `commit_prepared_lines` under the lock.
    pub(crate) fn create_named_lines(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        names: Vec<String>,
        start: &str,
        owner: u32,
        source: Option<&Line>,
        view: Option<&crate::overlay::View>,
    ) -> Result<Vec<Line>> {
        locked.check(&self.p)?;
        let prepared = self.prepare_named_lines(ctx, names, start, owner, source, view)?;
        self.commit_prepared_lines(locked, ctx, prepared)
    }

    /// The object store and tree of `start` when the state has no snapshot here and `layr migrate`
    /// recorded its tree (docs/objects.md, section 13).
    fn objects_start(&self, start: &str) -> Result<Option<(layr_objects::Store, layr_objects::StateRef)>> {
        if self.p.has_state(start) {
            return Ok(None);
        }
        crate::objstore::verified(&self.p, start)
    }

    fn check_names_free(&self, names: &[String]) -> Result<()> {
        for name in names {
            crate::store::check_name("line", name)?;
            if self.p.db.line_by_name(name)?.is_some() {
                bail!("a line named '{name}' already exists");
            }
            // `line rename` keeps the folder of the old name: a new line with that name gets no
            // folder of another line.
            let folder = self.p.line_path(name);
            if let Some(owner) = self.p.kept_folder_line(name).and_then(|id| self.p.db.line_by_id(&id).ok().flatten()) {
                bail!(
                    "the folder {} belongs to line '{}' (renamed from '{name}'); choose another name, or run 'layr line move {}' to move that folder to {}",
                    folder.display(),
                    owner.name,
                    owner.name,
                    self.p.default_line_path(&owner.name).display()
                );
            }
            if folder.symlink_metadata().is_ok() {
                bail!("a line named '{name}' already exists");
            }
        }
        Ok(())
    }

    /// The heavy part of line creation, without the project lock: the tree clone, the overlay
    /// layers, the .git folder and the caches of every line, in the service's work folder. The
    /// lines appear only in `commit_prepared_lines`, which checks again under the lock.
    pub(crate) fn prepare_named_lines(
        &self,
        ctx: &Ctx,
        names: Vec<String>,
        start: &str,
        owner: u32,
        source: Option<&Line>,
        view: Option<&crate::overlay::View>,
    ) -> Result<PreparedLines> {
        let _t = crate::trace::span("prepare lines");
        self.check_names_free(&names)?;
        self.source_state(start)?;
        // The store's git folder and its ACL: a walk of the whole store, so it runs here, without
        // the lock. It admits the owner of the lines that the commit records.
        crate::gitbridge::sync_store_access_with(self, &[owner])?;
        let lines: Vec<Line> = names
            .into_iter()
            .map(|name| Line {
                id: ids::new_id(),
                name,
                head: start.to_string(),
                owner,
                machine: self.p.store.machine.id.clone(),
            })
            .collect();
        let tmps: Vec<PathBuf> = lines.iter().map(|_| self.p.work_dir().join(crate::store::temp_name())).collect();
        let result = (|| -> Result<Vec<Record>> {
            match self.objects_start(start)? {
                // A migrated state without a snapshot here: each folder is a checkout of its tree
                // with its times (hot blobs are clones), no snapshot is built first.
                Some((store, refs)) => {
                    for tmp in &tmps {
                        crate::storage::create_subvolume(tmp)?;
                        // This private work directory is inaccessible to the caller. Decode
                        // here with the service identity; prepare_at assigns the line owner
                        // before publication, including every file below the root.
                        layr_objects::checkout_with(&store, &refs.tree, Some(&refs.times), tmp)?;
                    }
                }
                None => {
                    #[cfg(target_os = "macos")]
                    for tmp in &tmps {
                        self.snapshot_state(start, tmp, false)?;
                    }
                    #[cfg(not(target_os = "macos"))]
                    snapshots(&self.state_root(start)?, &tmps)?;
                }
            }
            let mut recs = Vec::new();
            for (line, tmp) in lines.iter().zip(&tmps) {
                let layers = if let Some(view) = view {
                    self.prepare_view_layers(ctx, line, view)?;
                    view.layers.clone()
                } else {
                    if self.p.is_overlay() {
                        Vec::new()
                    } else {
                        self.inherit_layers(ctx, source, start, &line)?
                    }
                };
                self.put_layers(tmp, &layers, self.own(line))?;
                // On OverlayFS the caches are taken after the folder move (`commit_prepared_lines`): a
                // mount under a moved folder is lost by MS_MOVE. Elsewhere a take is a snapshot taken
                // before the store lock, and it runs first so `prepare_at` does not copy the folder again.
                #[cfg(not(feature = "overlayfs"))]
                let cached = self.take_caches(ctx, line, start, source, tmp)?;
                #[cfg(feature = "overlayfs")]
                let cached: Vec<String> = Vec::new();
                self.prepare_at(line, start, tmp, &cached)?;
                let mut rec = Record::new("line.create");
                rec.data = serde_json::json!({"name": line.name, "start": start});
                if !layers.is_empty() {
                    let base = self.temp_snapshot(tmp, true)?;
                    self.install_base(line, base, start, &layers)?;
                    if !self.public_layers(&layers)?.is_empty() {
                        rec.data["folder_view"] = self.public_view_data(&line.id, &layers)?;
                    }
                }
                rec.lines.insert(
                    line.id.clone(),
                    LineChange {
                        before: None,
                        after: Some(self.line_state(line, start)),
                        working_before: None,
                        working_after: None,
                    },
                );
                recs.push(rec);
            }
            Ok(recs)
        })();
        let prepared = PreparedLines {
            start: start.to_string(),
            lines,
            tmps,
            recs: Vec::new(),
            #[cfg(feature = "overlayfs")]
            source: source.cloned(),
        };
        match result {
            Ok(recs) => Ok(PreparedLines { recs, ..prepared }),
            Err(e) => {
                self.discard_prepared_lines(prepared);
                Err(e)
            }
        }
    }

    /// Delete the prepared folders and the bases of lines that were never recorded.
    pub(crate) fn discard_prepared_lines(&self, prepared: PreparedLines) {
        for (line, tmp) in prepared.lines.iter().zip(&prepared.tmps) {
            let _ = crate::storage::delete_tree(tmp);
            if self.p.db.line_by_id(&line.id).ok().flatten().is_none() {
                let _ = self.prune_bases(line, None, 0);
            }
        }
    }

    /// The caller holds the project lock. Checks that the names are still free and the start
    /// state is still there, then publishes every prepared folder and records the whole batch in one transaction.
    pub(crate) fn commit_prepared_lines(
        &self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        prepared: PreparedLines,
    ) -> Result<Vec<Line>> {
        locked.check(&self.p)?;
        let _t = crate::trace::span("record lines");
        #[cfg(feature = "overlayfs")]
        let source = prepared.source.clone();
        let PreparedLines { start, lines, tmps, mut recs, .. } = prepared;
        let result = (|| -> crate::db::PublishResult<()> {
            let names: Vec<String> = lines.iter().map(|l| l.name.clone()).collect();
            self.check_names_free(&names)?;
            self.source_state(&start)?;
            for (line, tmp) in lines.iter().zip(&tmps) {
                crate::fsutil::rename_tree(tmp, self.working(line))?;
                #[cfg(feature = "overlayfs")]
                self.take_caches(ctx, line, &start, source.as_ref(), &self.working(line))?;
            }
            locked.append_batch(&mut recs, &ctx.actor())?;
            Ok(())
        })();
        if let Err(e) = result {
            match e {
                crate::db::CommitResolution::Unknown(_) => return Err(e.into()),
                crate::db::CommitResolution::Committed(error) => {
                    let _ = crate::gitbridge::sync_store_access(self);
                    anyhow::bail!(
                        "{error:#}; batch committed despite the reported error; recorded lines: {}",
                        lines.iter().map(|line| line.name.as_str()).collect::<Vec<_>>().join(", ")
                    );
                }
                crate::db::CommitResolution::NotCommitted(_) => {}
            }
            for (line, tmp) in lines.iter().zip(&tmps) {
                // A moved tmp is ours at the line path; a folder there while tmp exists is not ours.
                let moved = !tmp.exists();
                let _ = crate::storage::delete_tree(tmp);
                if self.p.db.line_by_id(&line.id)?.is_none() {
                    if moved && self.p.db.line_by_name(&line.name)?.is_none() {
                        let _ = crate::storage::delete_tree(&self.working(line));
                    }
                    let _ = self.prune_bases(line, None, 0);
                }
            }
            return Err(e.into());
        }
        // The store access was synced in `prepare_named_lines`, without the lock.
        Ok(lines)
    }

    /// Snapshot replacement is unsafe with local-only files, caches, overlays or active users.
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    pub fn reset_snapshot(&self, ctx: &Ctx, line: &Line, target: &str) -> Result<bool> {
        let working = self.working(line);
        let git = working.join(".git");
        if git.symlink_metadata().is_ok() && !crate::storage::is_subvolume(&git) {
            return Ok(false);
        }
        if ctx.cwd.starts_with(&working)
            || self.is_stacked(line)?
            || !self.pending_moves(line).is_empty()
            || !self.cache_paths(&working)?.is_empty()
            || !self.cache_paths(&self.state_root(target)?)?.is_empty()
            || crate::busy::user_of(&working)?.is_some()
        {
            return Ok(false);
        }
        let (status, _, _, _) = self.status(line)?;
        if !status.untracked.is_empty() || !status.untracked_dirs.is_empty() || !status.ignored.is_empty() {
            return Ok(false);
        }
        // Only .git may be nested. Other subvolumes must keep their identity.
        let target_root = self.state_root(target)?;
        let mut nested = false;
        crate::fsutil::walk(&working, "", &mut |p, m| {
            if p == ".git" {
                return Ok(false);
            }
            if m.is_dir() && (crate::storage::is_subvolume(&working.join(p)) || crate::fsutil::lmeta(&target_root, p).is_none()) {
                nested = true;
                return Ok(false);
            }
            Ok(true)
        })?;
        if nested {
            return Ok(false);
        }
        let replacement = self.temp_work(&self.state_root(target)?)?;
        self.prepare_at(line, target, &replacement.path, &[])?;
        // Preserve the existing Git folder, including local settings and object links.
        crate::storage::delete_tree(&replacement.path.join(".git"))?;
        let has_git = git.symlink_metadata().is_ok();
        if has_git {
            // Moving a folder to another parent needs write permission on it.
            crate::fsutil::git_writable(&working, true);
            std::fs::rename(&git, replacement.path.join(".git"))?;
        }
        let swap = rustix::fs::renameat_with(
            rustix::fs::CWD,
            &replacement.path,
            rustix::fs::CWD,
            &working,
            rustix::fs::RenameFlags::EXCHANGE,
        );
        if let Err(e) = swap {
            if has_git {
                std::fs::rename(replacement.path.join(".git"), &git)?;
                crate::fsutil::git_writable(&working, false);
            }
            return Err(e.into());
        }
        crate::fsutil::git_writable(&working, false);
        Ok(true)
    }

    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    pub fn reset_snapshot(&self, _ctx: &Ctx, _line: &Line, _target: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batch_names_are_numbered_and_validated() {
        assert_eq!(batch_names("trial", 3).unwrap(), ["trial-1", "trial-2", "trial-3"]);
        assert!(batch_names("trial", 0).is_err());
        assert!(batch_names("../trial", 1).is_err());
    }
}

#[cfg(all(test, not(feature = "overlayfs")))]
mod batch_commit_tests {
    use super::*;
    use crate::ctx::Caller;
    use crate::store::{Project, Store};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn context() -> Result<(Scratch, Ctx)> {
        let root = Scratch(std::env::temp_dir().join(format!("layr-batch-{}", ids::new_id())));
        std::fs::create_dir_all(&root.0)?;
        let ctx = Ctx {
            store: Store::open(&root.0.join("store"))?,
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.0.clone(),
            env: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
            author: Default::default(),
        };
        Ok((root, ctx))
    }

    // Plain folders exercise the actual publication/record/rollback path on any filesystem.
    fn prepared(ctx: &Ctx) -> Result<(Repo<'_>, PreparedLines)> {
        let dir = ctx.store.project_dir("batch")?;
        for name in ["meta/db", "lines", "work", "states"] {
            std::fs::create_dir_all(dir.join(name))?;
        }
        let p = Project {
            lease: Default::default(),
            store: &ctx.store,
            name: "batch".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: crate::db::Db::open(&crate::store::db_file(&dir))?,
        };
        let mut project = Record::new("project");
        project.data = serde_json::json!({"id": p.id, "name": p.name, "config": {}});
        p.lock_maintenance()?.append(project, &ctx.actor())?;
        let start = ids::new_id();
        std::fs::create_dir(p.state_path(&start))?;
        let mut state = Record::new("state");
        state.states.push(serde_json::from_value(serde_json::json!({
            "id": start, "kind": "commit", "subvol": "", "author": ctx.author(), "time": ids::now_ms()
        }))?);
        p.lock_maintenance()?.append(state, &ctx.actor())?;
        let repo = Repo { p, ctx: None };
        let mut lines = Vec::new();
        let mut tmps = Vec::new();
        let mut recs = Vec::new();
        for name in batch_names("trial", 3)? {
            let line = Line {
                id: ids::new_id(),
                name,
                head: start.clone(),
                owner: ctx.caller.uid,
                machine: ctx.store.machine.id.clone(),
            };
            let tmp = dir.join("work").join(&line.id);
            std::fs::create_dir(&tmp)?;
            std::fs::write(tmp.join("content"), &line.name)?;
            let mut rec = Record::new("line.create");
            rec.data = serde_json::json!({"name": line.name, "start": start});
            rec.lines.insert(
                line.id.clone(),
                LineChange {
                    before: None,
                    after: Some(repo.line_state(&line, &start)),
                    working_before: None,
                    working_after: None,
                },
            );
            lines.push(line);
            tmps.push(tmp);
            recs.push(rec);
        }
        Ok((repo, PreparedLines { start, lines, tmps, recs }))
    }

    #[test]
    fn delete_guard_rejects_folder_replacement_and_symlinks() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let _lock = repo.p.lock_maintenance()?;
        let lines = repo.commit_prepared_lines(&_lock, &ctx, prepared)?;
        let line = &lines[0];
        let target = DeleteTarget::capture(&repo, line)?;
        assert_eq!(target.validate(&repo)?.id, line.id);
        let path = repo.working(line);
        let old = path.with_extension("old");
        std::fs::rename(&path, &old)?;
        std::fs::create_dir(&path)?;
        let err = target.validate(&repo).unwrap_err();
        assert_eq!(
            err.downcast_ref::<DeleteRefused>(),
            Some(&DeleteRefused::FolderChanged(layr_types::LineId::parse(&line.id)?))
        );
        std::fs::remove_dir(&path)?;
        std::os::unix::fs::symlink(&old, &path)?;
        assert!(matches!(
            target.validate(&repo).unwrap_err().downcast_ref::<DeleteRefused>(),
            Some(DeleteRefused::FolderChanged(_))
        ));
        std::fs::remove_file(&path)?;
        assert!(target.validate(&repo).is_err());
        std::fs::rename(&old, &path)?;
        assert!(target.validate(&repo).is_ok());
        Ok(())
    }

    #[test]
    fn delete_guard_rejects_deleted_identity_even_with_a_replacement_name() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let _lock = repo.p.lock_maintenance()?;
        let lines = repo.commit_prepared_lines(&_lock, &ctx, prepared)?;
        let line = &lines[0];
        let target = DeleteTarget::capture(&repo, line)?;
        repo.p.db.test_connection().execute("UPDATE lines SET deleted=1 WHERE id=?1", [&line.id])?;
        repo.p.db.test_connection().execute("UPDATE lines SET name=?1 WHERE id=?2", [&line.id, &lines[1].id])?;
        let err = target.validate(&repo).unwrap_err();
        assert_eq!(err.downcast_ref::<DeleteRefused>(), Some(&DeleteRefused::LineMissing(layr_types::LineId::parse(&line.id)?)));
        assert!(repo.working(line).is_dir());
        Ok(())
    }

    #[test]
    fn delete_guard_rejects_a_changed_path_or_new_folder() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let _lock = repo.p.lock_maintenance()?;
        let lines = repo.commit_prepared_lines(&_lock, &ctx, prepared)?;
        let line = &lines[0];
        let target = DeleteTarget::capture(&repo, line)?;
        let path = repo.working(line);
        let moved = path.with_extension("moved");
        repo.p.set_moved_path(&line.id, Some(&moved))?;
        assert!(target.validate(&repo).is_err());
        repo.p.set_moved_path(&line.id, None)?;
        std::fs::remove_dir_all(&path)?;
        let absent = DeleteTarget::capture(&repo, line)?;
        assert!(!absent.has_folder());
        assert!(absent.validate(&repo).is_ok());
        std::fs::create_dir(&path)?;
        assert!(absent.validate(&repo).is_err());
        Ok(())
    }

    #[test]
    fn a_batch_publishes_all_lines_with_contiguous_records_and_full_sync() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let _lock = repo.p.lock_maintenance()?;
        let before = repo.p.db.last_seq(&ctx.store.machine.id)?;
        let lines = repo.commit_prepared_lines(&_lock, &ctx, prepared)?;
        assert_eq!(lines.len(), 3);
        assert_eq!(repo.p.db.last_seq(&ctx.store.machine.id)?, before + lines.len() as u64);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(repo.p.db.line_by_id(&line.id)?.unwrap().name, line.name);
            assert_eq!(std::fs::read_to_string(repo.working(line).join("content"))?, line.name);
            let raw = repo.p.db.record_line(&ctx.store.machine.id, before + index as u64 + 1)?.unwrap();
            let rec = crate::oplog::parse_line(&raw, None)?.record;
            assert!(rec.lines.contains_key(&line.id));
        }
        let sync: i64 = repo.p.db.test_connection().pragma_query_value(None, "synchronous", |r| r.get(0))?;
        assert_eq!(sync, 2, "FULL synchronous is required");
        Ok(())
    }

    #[test]
    fn a_mid_batch_database_failure_rolls_back_every_record_and_published_folder() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let paths: Vec<_> = prepared.lines.iter().map(|line| repo.working(line)).collect();
        let tmps = prepared.tmps.clone();
        repo.p.db.test_connection().execute_batch(
            "CREATE TEMP TRIGGER fail_second_line BEFORE INSERT ON lines
            WHEN NEW.name = 'trial-2' BEGIN SELECT RAISE(ABORT, 'injected mid-batch failure'); END;",
        )?;
        let _lock = repo.p.lock_maintenance()?;
        let before = repo.p.db.last_seq(&ctx.store.machine.id)?;
        let err = repo.commit_prepared_lines(&_lock, &ctx, prepared).unwrap_err();
        assert!(err.to_string().contains("injected mid-batch failure"));
        assert!(repo.p.db.lines()?.is_empty());
        assert_eq!(repo.p.db.last_seq(&ctx.store.machine.id)?, before);
        assert!(paths.iter().chain(&tmps).all(|path| !path.exists()));
        Ok(())
    }

    #[test]
    fn an_error_after_commit_keeps_the_entire_recorded_batch_and_its_folders() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let paths: Vec<_> = prepared.lines.iter().map(|line| repo.working(line)).collect();
        let _lock = repo.p.lock_maintenance()?;
        crate::db::FAIL_AFTER_COMMIT.with(|f| f.set(true));
        let err = repo.commit_prepared_lines(&_lock, &ctx, prepared).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("batch committed despite the reported error"), "{message}");
        assert_eq!(repo.p.db.lines()?.len(), 3);
        for (index, path) in paths.iter().enumerate() {
            assert!(path.join("content").is_file());
            assert!(message.contains(&format!("trial-{}", index + 1)));
        }
        Ok(())
    }
    #[test]
    fn an_unknown_commit_keeps_all_prepared_folders_for_recovery() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let paths: Vec<_> = prepared.lines.iter().map(|line| repo.working(line)).collect();
        let locked = repo.p.lock_maintenance()?;
        crate::db::FAIL_AFTER_COMMIT.with(|flag| flag.set(true));
        crate::db::FAIL_RECONCILE.with(|flag| flag.set(true));
        let error = repo.commit_prepared_lines(&locked, &ctx, prepared).unwrap_err();
        assert!(error.to_string().contains("commit outcome is unknown"));
        assert!(paths.iter().all(|path| path.join("content").is_file()));
        Ok(())
    }
}
