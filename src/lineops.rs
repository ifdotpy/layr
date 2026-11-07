//! Batch line creation and the safe snapshot path for hard reset.

use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Line, LineChange, Record};
use crate::repo::Repo;
use anyhow::{bail, Result};
#[cfg(not(target_os = "macos"))]
use std::path::Path;
use std::path::PathBuf;

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
    lines: Vec<Line>,
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
        ctx: &Ctx,
        names: Vec<String>,
        start: &str,
        owner: u32,
        source: Option<&Line>,
        view: Option<&crate::overlay::View>,
    ) -> Result<Vec<Line>> {
        let prepared = self.prepare_named_lines(ctx, names, start, owner, source, view)?;
        self.commit_prepared_lines(ctx, prepared)
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
            if self.p.db.line_by_name(name)?.is_some() || self.p.line_path(name).symlink_metadata().is_ok() {
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
<<<<<<< head
            match self.objects_start(start)? {
                // A migrated state without a snapshot here: each folder is a checkout of its tree
                // with its times (hot blobs are clones), no snapshot is built first.
                Some((store, refs)) => {
                    for tmp in &tmps {
                        crate::storage::create_subvolume(tmp)?;
                        if rustix::process::geteuid().is_root() {
                            std::os::unix::fs::chown(tmp, Some(ctx.caller.uid), Some(ctx.caller.gid))?;
                        }
                        crate::objstore::checkout(ctx, &store, &refs, tmp)?;
                    }
                }
                None => snapshots(&self.state_root(start)?, &tmps)?,
            }
||||||| base
            snapshots(&self.state_root(start)?, &tmps)?;
=======
            // On APFS each folder is made from the layers of the state: a complete view of a thin
            // state first, and a copy of the view after it, cloned the whole tree twice.
            #[cfg(target_os = "macos")]
            for tmp in &tmps {
                self.snapshot_state(start, tmp, false)?;
            }
            #[cfg(not(target_os = "macos"))]
            snapshots(&self.state_root(start)?, &tmps)?;
>>>>>>> origin-https/main
            let mut recs = Vec::new();
            for (line, tmp) in lines.iter().zip(&tmps) {
                let layers = if let Some(view) = view {
                    self.prepare_view_layers(ctx, line, view)?;
                    view.layers.clone()
                } else {
                    match source {
                        Some(s) if !self.p.is_overlay() => self.inherit_layers(ctx, s, &line.id, owner)?,
                        _ => Vec::new(),
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
                        rec.data["stack"] = self.public_stack_data(&line.id, &layers)?;
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
    pub(crate) fn commit_prepared_lines(&self, ctx: &Ctx, prepared: PreparedLines) -> Result<Vec<Line>> {
        let _t = crate::trace::span("record lines");
        #[cfg(feature = "overlayfs")]
        let source = prepared.source.clone();
        let PreparedLines { start, lines, tmps, recs, .. } = prepared;
        let result = (|| -> Result<()> {
            let names: Vec<String> = lines.iter().map(|l| l.name.clone()).collect();
            self.check_names_free(&names)?;
            self.source_state(&start)?;
            for (line, tmp) in lines.iter().zip(&tmps) {
                crate::fsutil::rename_tree(tmp, self.working(line))?;
                #[cfg(feature = "overlayfs")]
                self.take_caches(ctx, line, &start, source.as_ref(), &self.working(line))?;
            }
            self.p.append_batch(recs, &ctx.actor())?;
            Ok(())
        })();
        if let Err(e) = result {
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
            let created = lines
                .iter()
                .filter(|line| self.p.db.line_by_id(&line.id).ok().flatten().is_some())
                .map(|line| line.name.as_str())
                .collect::<Vec<_>>();
            if !created.is_empty() {
                let _ = crate::gitbridge::sync_store_access(self);
                bail!("{e:#}; batch committed despite the reported error; recorded lines: {}", created.join(", "));
            }
            return Err(e);
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
            store: &ctx.store,
            name: "batch".into(),
            dir: dir.clone(),
            id: ids::new_id(),
            db: crate::db::Db::open(&crate::store::db_file(&dir))?,
        };
        let mut project = Record::new("project");
        project.data = serde_json::json!({"id": p.id, "name": p.name, "config": {}});
        p.append(project, &ctx.actor())?;
        let start = ids::new_id();
        std::fs::create_dir(p.state_path(&start))?;
        let mut state = Record::new("state");
        state.states.push(serde_json::from_value(serde_json::json!({
            "id": start, "kind": "commit", "subvol": "", "author": ctx.author(), "time": ids::now_ms()
        }))?);
        p.append(state, &ctx.actor())?;
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
    fn a_batch_publishes_all_lines_with_contiguous_records_and_full_sync() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let _lock = repo.p.lock()?;
        let before = repo.p.db.last_seq(&ctx.store.machine.id)?;
        let lines = repo.commit_prepared_lines(&ctx, prepared)?;
        assert_eq!(lines.len(), 3);
        assert_eq!(repo.p.db.last_seq(&ctx.store.machine.id)?, before + lines.len() as u64);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(repo.p.db.line_by_id(&line.id)?.unwrap().name, line.name);
            assert_eq!(std::fs::read_to_string(repo.working(line).join("content"))?, line.name);
            let raw = repo.p.db.record_line(&ctx.store.machine.id, before + index as u64 + 1)?.unwrap();
            let rec = crate::oplog::parse_line(&raw, None)?.record;
            assert!(rec.lines.contains_key(&line.id));
        }
        let sync: i64 = repo.p.db.conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        assert_eq!(sync, 2, "FULL synchronous is required");
        Ok(())
    }

    #[test]
    fn a_mid_batch_database_failure_rolls_back_every_record_and_published_folder() -> Result<()> {
        let (_root, ctx) = context()?;
        let (repo, prepared) = prepared(&ctx)?;
        let paths: Vec<_> = prepared.lines.iter().map(|line| repo.working(line)).collect();
        let tmps = prepared.tmps.clone();
        repo.p.db.conn.execute_batch(
            "CREATE TEMP TRIGGER fail_second_line BEFORE INSERT ON lines
            WHEN NEW.name = 'trial-2' BEGIN SELECT RAISE(ABORT, 'injected mid-batch failure'); END;",
        )?;
        let _lock = repo.p.lock()?;
        let before = repo.p.db.last_seq(&ctx.store.machine.id)?;
        let err = repo.commit_prepared_lines(&ctx, prepared).unwrap_err();
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
        let _lock = repo.p.lock()?;
        crate::db::FAIL_AFTER_COMMIT.with(|f| f.set(true));
        let err = repo.commit_prepared_lines(&ctx, prepared).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("batch committed despite the reported error"), "{message}");
        assert_eq!(repo.p.db.lines()?.len(), 3);
        for (index, path) in paths.iter().enumerate() {
            assert!(path.join("content").is_file());
            assert!(message.contains(&format!("trial-{}", index + 1)));
        }
        Ok(())
    }
}
