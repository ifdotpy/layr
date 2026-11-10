//! Linux view lifecycle over the native copy-on-write tree backend.
//!
//! Btrfs snapshots and OverlayFS branches already share unchanged data. Keep their
//! native save path, and persist the view mode only on this machine.
use crate::cmd::{exit, here};
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
fn dir(repo: &Repo, id: &str) -> PathBuf {
    repo.p.meta_dir().join("vlines").join(id)
}
pub fn of(repo: &Repo, line: &Line) -> Option<View> {
    let data = std::fs::read(dir(repo, &line.id).join("native.json")).ok()?;
    serde_json::from_slice(&data).ok()
}
fn require_enabled(ctx: &Ctx) -> Result<()> {
    if ctx.env(ENV) != Some("1") {
        return Err(exit(2, format!("virtual lines are experimental: set {ENV}=1 (docs/views.md)")));
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
fn recover_move(repo: &Repo, line: &Line) -> Result<()> {
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

fn move_tree(ctx: &Ctx, repo: &Repo, line: &Line, from: &Path, to: &Path) -> Result<()> {
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

pub fn show(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = crate::args::Spec::new().flag("--folder").flag("--none").flag("--virtual");
    let a = crate::args::parse(&spec, args)?;
    let modes = ["--folder", "--none", "--virtual"].iter().filter(|m| a.has(m)).count();
    if a.pos.is_empty() || a.pos.len() > 2 || modes != 1 || (a.pos.len() == 2 && !a.has("--folder")) {
        return Err(exit(2, "usage: layr line show <line> --folder [<path>] | --none | --virtual"));
    }
    require_enabled(ctx)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = repo.line(&a.pos[0])?;
    repo.authorize_line(ctx, &line, crate::repo::LineOp::Manage)?;
    if of(repo, &line).is_none() {
        bail!("line {} has a folder; only virtual lines change their view for now", line.name);
    }
    if h.line.as_ref().is_some_and(|l| l.id == line.id) || ctx.cwd.starts_with(repo.working(&line)) {
        bail!("run this command outside the line's folder: its view changes");
    }
    let _line_lock = repo.line_lock(&line)?;
    let _lock = repo.p.lock()?;
    let line = repo.line(&line.id)?;
    recover_move(repo, &line)?;
    let hidden = dir(repo, &line.id).join("tree");
    let visible = repo.p.default_line_path(&line.name);
    let mut from = repo.working(&line);
    if !from.exists() {
        let found: Vec<_> = [&hidden, &visible].into_iter().filter(|p| p.exists()).collect();
        if found.len() != 1 {
            bail!("cannot locate one working tree for view {}; no files were removed", line.name);
        }
        from = found[0].clone();
        repo.p.set_moved_path(&line.id, Some(&from))?;
    }
    let to = if a.has("--none") {
        hidden
    } else if let Some(arg) = a.pos.get(1) {
        // A retry of an interrupted conversion already has its tree at the target.
        // Accept that same tree, but never another existing destination.
        let requested = ctx.cwd.join(arg);
        if requested.canonicalize().ok().as_ref() == Some(&from) {
            from.clone()
        } else {
            crate::cmd::branch::move_target(ctx, repo, &from, arg)?
        }
    } else {
        visible.clone()
    };
    move_tree(ctx, repo, &line, &from, &to)?;
    if to == visible {
        repo.p.set_moved_path(&line.id, None)?;
    }
    if a.has("--folder") {
        forget(repo, &line.id)?;
    }
    if a.has("--none") {
        outln!(ctx, "Line {} has no visible folder; its changes stay in {}", line.name, to.display());
    } else {
        outln!(ctx, "Line {} has a {} view at {}", line.name, if a.has("--folder") { "folder" } else { "virtual" }, to.display());
    }
    Ok(0)
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
