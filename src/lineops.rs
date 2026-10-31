//! Batch line creation and the safe snapshot path for hard reset.

use crate::ctx::Ctx;
use crate::ids;
use crate::model::{Line, LineChange, Record};
use crate::repo::Repo;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

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
fn snapshots(src: &Path, destinations: &[PathBuf]) -> Result<()> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        let workers = destinations.len().min(8);
        let barrier = std::sync::Barrier::new(workers);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|worker| {
                    let barrier = &barrier;
                    scope.spawn(move || -> Result<()> {
                        barrier.wait();
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
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
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
        let tmps: Vec<PathBuf> = lines.iter().map(|_| self.p.work_dir().join(ids::new_id())).collect();
        let result = (|| -> Result<Vec<Record>> {
            snapshots(&self.state_root(start)?, &tmps)?;
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
                self.prepare_at(line, start, tmp)?;
                self.take_caches(ctx, line, start, source, tmp)?;
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
        let prepared = PreparedLines { start: start.to_string(), lines, tmps, recs: Vec::new() };
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
    /// state is still there, then moves each prepared folder into place and records the line.
    pub(crate) fn commit_prepared_lines(&self, ctx: &Ctx, prepared: PreparedLines) -> Result<Vec<Line>> {
        let _t = crate::trace::span("record lines");
        let PreparedLines { start, lines, tmps, recs } = prepared;
        let result = (|| -> Result<()> {
            let names: Vec<String> = lines.iter().map(|l| l.name.clone()).collect();
            self.check_names_free(&names)?;
            self.source_state(&start)?;
            for ((line, tmp), rec) in lines.iter().zip(&tmps).zip(recs) {
                crate::fsutil::rename_tree(tmp, self.working(line))?;
                self.commit_record(ctx, rec)?;
            }
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
                bail!("{e:#}; created lines: {}", created.join(", "));
            }
            return Err(e);
        }
        crate::gitbridge::sync_store_access(self)?;
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
            || !self.cache_paths(&working).is_empty()
            || !self.cache_paths(&self.state_root(target)?).is_empty()
            || !self.state(target)?.caches.is_empty()
            || line_in_use(&working)?
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
        self.prepare_at(line, target, &replacement.path)?;
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

/// An active process can retain a directory, an open file or a mapped file across a swap.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
fn line_in_use(root: &Path) -> Result<bool> {
    for entry in std::fs::read_dir("/proc")?.flatten() {
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let process = entry.path();
        for name in ["cwd", "root"] {
            if std::fs::read_link(process.join(name)).is_ok_and(|p| p.starts_with(root)) {
                return Ok(true);
            }
        }
        if let Ok(fds) = std::fs::read_dir(process.join("fd")) {
            for fd in fds.flatten() {
                if std::fs::read_link(fd.path()).is_ok_and(|p| p.starts_with(root)) {
                    return Ok(true);
                }
            }
        }
        if let Ok(maps) = std::fs::read_to_string(process.join("maps")) {
            if maps.lines().filter_map(|line| line.split_whitespace().nth(5)).any(|p| Path::new(p).starts_with(root)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
