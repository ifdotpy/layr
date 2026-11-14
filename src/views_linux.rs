//! Linux view lifecycle over the native copy-on-write tree backend.
//!
//! Btrfs snapshots and OverlayFS branches already share unchanged data. Keep their
//! native save path, and persist the view mode only on this machine.
use crate::errors::{error, Kind as ErrorKind};
use crate::ctx::Ctx;
use crate::model::Line;
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const ENV: &str = "LAYR_EXPERIMENTAL_VIEWS";
#[derive(serde::Serialize, serde::Deserialize)]
pub struct View {
    line: String,
    name: String,
}
pub(crate) fn dir(repo: &Repo, id: &str) -> PathBuf {
    repo.p.meta_dir().join("vlines").join(id)
}
pub fn of(repo: &Repo, line: &Line) -> Option<View> {
    let data = std::fs::read(dir(repo, &line.id).join("native.json")).ok()?;
    serde_json::from_slice(&data).ok()
}
pub(crate) fn require_enabled(ctx: &Ctx) -> Result<()> {
    if ctx.env(ENV) != Some("1") {
        return Err(error(ErrorKind::Unsupported, format!("virtual lines are experimental: set {ENV}=1 (docs/views.md)")));
    }
    Ok(())
}

pub fn create(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32) -> Result<Line> {
    require_enabled(ctx)?;
    if owner != ctx.caller.uid {
        bail!("a virtual line belongs to the user who makes it (no --owner)");
    }
    if repo.p.is_overlay() {
        bail!("an overlay project has no virtual lines");
    }
    let prepared = repo.prepare_named_lines(ctx, vec![name.to_owned()], start, owner, None, None)?;
    let line = prepared.lines[0].clone();
    let vdir = dir(repo, &line.id);
    let made = (|| -> Result<()> {
        std::fs::create_dir_all(&vdir)?;
        let view = View { line: line.id.clone(), name: name.to_owned() };
        crate::fsutil::atomic_write(&vdir.join("native.json"), &serde_json::to_vec(&view)?, 0o600)
    })();
    if let Err(e) = made {
        repo.discard_prepared_lines(prepared);
        let _ = std::fs::remove_dir_all(&vdir);
        return Err(e);
    }
    let _lock = repo.p.lock()?;
    match repo.commit_prepared_lines(ctx, prepared) {
        Ok(mut lines) => Ok(lines.remove(0)),
        Err(e) => {
            if repo.p.db.line_by_id(&line.id)?.is_none() {
                let _ = std::fs::remove_dir_all(&vdir);
            }
            Err(e)
        }
    }
}

/// Remove only the view metadata after the line's working tree is saved and trashed.
pub fn forget(repo: &Repo, id: &str) -> Result<()> {
    let d = dir(repo, id);
    if !d.exists() {
        return Ok(());
    }
    // Never recursively remove a hidden working tree.
    if d.join("tree").symlink_metadata().is_ok() {
        bail!("view {} still has a working tree; its files were kept", id);
    }
    match std::fs::remove_file(d.join("native.json")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    std::fs::remove_dir(d)?;
    Ok(())
}

/// Hide/show uses the same tree move and machine-local path index as `line move`.
/// A failed path-index write moves the tree back. After a crash between the move
/// and the index write, `show` locates the sole surviving tree and repairs the index.
pub(crate) fn recover_move(repo: &Repo, line: &Line) -> Result<()> {
    let journal = dir(repo, &line.id).join("move.json");
    let bytes = match std::fs::read(&journal) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let (from, to): (PathBuf, PathBuf) = serde_json::from_slice(&bytes)?;
    let surviving = match (from.exists(), to.exists()) {
        (true, false) => from,
        (false, true) => to,
        _ => bail!("ambiguous view move for {}; both paths are preserved", line.name),
    };
    repo.p.set_moved_path(&line.id, Some(&surviving))?;
    std::fs::remove_file(journal)?;
    Ok(())
}

pub(crate) fn move_tree(ctx: &Ctx, repo: &Repo, line: &Line, from: &Path, to: &Path) -> Result<()> {
    if from == to {
        return Ok(());
    }
    if to.symlink_metadata().is_ok() {
        bail!("{} already exists", to.display());
    }
    let journal = dir(repo, &line.id).join("move.json");
    crate::fsutil::atomic_write(&journal, &serde_json::to_vec(&(from, to))?, 0o600)?;
    crate::linepaths::register_with(&repo.p.store.root, Some(to));
    let result = crate::fsutil::rename_tree(from, to).map_err(anyhow::Error::from).and_then(|_| {
        // Test the durable journal at the boundary between the tree move and the path index.
        if ctx.env("LAYR_TEST_VIEW_MOVE_FAIL_AFTER_RENAME") == Some("1") {
            bail!("injected view move failure after rename");
        }
        if let Err(e) = repo.p.set_moved_path(&line.id, Some(to)) {
            crate::fsutil::rename_tree(to, from).context("restore the view after the path index failed")?;
            return Err(e);
        }
        Ok(())
    });
    crate::linepaths::register(&repo.p.store.root);
    if result.is_ok() || (from.exists() && !to.exists()) {
        std::fs::remove_file(journal)?;
    }
    result
}

pub fn guard(ctx: &Ctx, cmd: &str) -> Result<()> {
    const SUPPORTED: &[&str] = &[
        "help",
        "--help",
        "-h",
        "version",
        "--version",
        "status",
        "diff",
        "save",
        "line",
        "log",
        "show",
        "rev-parse",
        "merge-base",
        "projects",
        "gc",
        "du",
        "config",
        "usage",
        "init",
        "git",
    ];
    if SUPPORTED.contains(&cmd) {
        return Ok(());
    }
    let Ok(loc) = ctx.locate() else { return Ok(()) };
    let Some(name) = loc.line else { return Ok(()) };
    let Ok(project) = ctx.store.project_dir(&loc.project) else { return Ok(()) };
    let Ok(entries) = std::fs::read_dir(project.join("meta/vlines")) else { return Ok(()) };
    for entry in entries {
        let entry = entry?;
        let Ok(data) = std::fs::read(entry.path().join("native.json")) else { continue };
        let view: View = serde_json::from_slice(&data)?;
        if view.name == name {
            bail!(
                "layr {cmd} does not support virtual lines yet ({ENV}); convert the line first: layr line show {name} --folder"
            );
        }
    }
    Ok(())
}

/// Paths reserved by view moves, needed before OverlayFS opens its mount registry.
#[cfg(feature = "overlayfs")]
pub fn pending_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(layout) = crate::volumes::Layout::for_root(root) else { return paths };
    for (_, projects) in layout.ready_projects_dirs() {
        let Ok(projects) = std::fs::read_dir(projects) else { continue };
        for project in projects.flatten() {
            let Ok(views) = std::fs::read_dir(project.path().join("meta/vlines")) else { continue };
            for view in views.flatten() {
                let Ok(bytes) = std::fs::read(view.path().join("move.json")) else { continue };
                if let Ok((from, to)) = serde_json::from_slice::<(PathBuf, PathBuf)>(&bytes) {
                    paths.extend([from, to]);
                }
            }
        }
    }
    paths
}

/// All changes of a virtual line are included in its next save.
pub fn status(repo: &Repo, line: &Line, view: &View) -> Result<crate::repo::Status> {
    let mut keep = Vec::new();
    let (head, working, list) = diff_input(repo, line, view, &mut keep)?;
    let staged = crate::changes::with_renames(&head, &working, list);
    let mut conflicts = Vec::new();
    for conflict in repo.state(&line.head)?.conflicts {
        if crate::repo::conflict_open(&working, &conflict)? {
            conflicts.push(conflict);
        }
    }
    Ok(crate::repo::Status { staged, conflicts, ..Default::default() })
}

pub fn diff_input(repo: &Repo, line: &Line, _view: &View, keep: &mut Vec<crate::repo::Temp>) -> Result<(crate::repo::Snap, PathBuf, Vec<crate::changes::Change>)> {
    ensure_no_pending_move(repo, line)?;
    let head = repo.state_src(&line.head)?;
    let scan = repo.scan(line)?;
    let working = scan.path.clone();
    keep.push(scan);
    let rules = crate::ignore_rules::Rules::new(&working);
    let list = crate::changes::file_changes(&head, &working)?.into_iter()
        .filter(|c| !crate::repo::skip_path(&c.path))
        .filter(|c| c.old.is_some() || !rules.ignored(&c.path, false))
        .collect();
    Ok((head, working, list))
}

/// A retry of line show must resolve an interrupted move before another operation.
pub fn ensure_no_pending_move(repo: &Repo, line: &Line) -> Result<()> {
    if dir(repo, &line.id).join("move.json").exists() {
        bail!("line {} has an interrupted view move; retry line show first", line.name);
    }
    Ok(())
}
