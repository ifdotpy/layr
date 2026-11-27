//! Linux view lifecycle over the native copy-on-write tree backend, or over LayrFS.
//!
//! Btrfs snapshots and OverlayFS branches already share unchanged data. Keep their
//! native save path, and persist the view mode only on this machine.
//!
//! LayrFS (`LAYR_VIEW_BACKEND=layrfs`, a build with the feature `layrfs-fuse`): the line's folder
//! is a FUSE mount that one server process per machine serves (the hub, started by the service).
//! The mount shows the objects of a state and keeps changes in `meta/vlines/<id>/fs`. Status,
//! diff and save read a snapshot of the view as objects; a state of the line has objects only.
//! A hidden line stays mounted at `meta/vlines/<id>/mnt`.
use crate::cmd::{exit, here};
use crate::errors::{error, Kind as ErrorKind};
use crate::ctx::Ctx;
use crate::model::Line;
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const ENV: &str = "LAYR_EXPERIMENTAL_VIEWS";
#[derive(serde::Serialize, serde::Deserialize)]
pub struct View {
    line: String,
    name: String,
    /// Empty for a native tree; "layrfs" for a LayrFS mount.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    backend: String,
    /// LayrFS: the state that the view started on (its journal names later ones).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    base: String,
}

impl View {
    pub fn is_layrfs(&self) -> bool {
        self.backend == "layrfs"
    }
}
pub(crate) fn dir(repo: &Repo, id: &str) -> PathBuf {
    repo.p.meta_dir().join("vlines").join(id)
}
/// The view of a virtual line. A fast line counts as a folder line here: every command takes it
/// through the folder paths (`fast_of` names it).
pub fn of(repo: &Repo, line: &Line) -> Option<View> {
    view_of(repo, line).filter(|v| !v.is_layrfs())
}

/// The view of a fast line (a LayrFS mount).
pub fn fast_of(repo: &Repo, line: &Line) -> Option<View> {
    view_of(repo, line).filter(|v| v.is_layrfs())
}

/// `layr line show` of a fast line (LayrFS): a hidden mount, or a folder line.
pub(crate) fn show_fast(ctx: &Ctx, repo: &Repo, line: &Line, view: &View, none: bool, folder: bool) -> Result<i32> {
    lfs::show(ctx, repo, line, view, none, folder)
}

pub(crate) fn view_of(repo: &Repo, line: &Line) -> Option<View> {
    let data = std::fs::read(dir(repo, &line.id).join("native.json")).ok()?;
    serde_json::from_slice(&data).ok()
}
/// Why a fast line (LayrFS) cannot start at `start` here, if it cannot.
pub fn fast_available(repo: &Repo, start: &str) -> std::result::Result<(), String> {
    if !cfg!(feature = "layrfs-fuse") {
        return Err("this layr is built without LayrFS (the feature layrfs-fuse)".into());
    }
    if repo.p.is_overlay() {
        return Err("an overlay project has no fast lines".into());
    }
    match crate::objstore::verified(&repo.p, start) {
        // LayrFS gives each name of a hard link its own file, and its snapshots write no link
        // groups: a save would split the links.
        Ok(Some((store, r))) => match store.get_tree(&r.tree) {
            Ok(t) if !t.links.is_empty() => {
                Err(format!("state {} has hard links, which fast lines do not keep", crate::ids::short(start)))
            }
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{e:#}")),
        },
        Ok(None) => Err(format!("state {} has no objects (run 'layr migrate')", crate::ids::short(start))),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// A fast line: its folder is a LayrFS mount (docs/layrfs-lab.md).
pub fn create_fast(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32) -> Result<Line> {
    if owner != ctx.caller.uid {
        bail!("a fast line belongs to the user who makes it (no --owner)");
    }
    lfs::create(ctx, repo, name, start, owner)
}

/// The folder of a fast line as the source of a copy (`crate::fs::source_of`): None when `src`
/// is not the mount of one.
#[cfg(feature = "layrfs-fuse")]
pub(crate) use lfs::FastSource;

pub(crate) fn fast_source(src: &Path) -> Option<Result<FastSource>> {
    #[cfg(feature = "layrfs-fuse")]
    return lfs::source_of_mount(src);
    #[cfg(not(feature = "layrfs-fuse"))]
    {
        let _ = src;
        None
    }
}

/// True when `src` is the mount of a fast line (no work).
pub(crate) fn is_fast_mount(src: &Path) -> bool {
    #[cfg(feature = "layrfs-fuse")]
    return lfs::is_mount(src);
    #[cfg(not(feature = "layrfs-fuse"))]
    {
        let _ = src;
        false
    }
}

/// Without LayrFS no folder is a fast line's mount.
#[cfg(not(feature = "layrfs-fuse"))]
pub(crate) struct FastSource {
    pub shadow: PathBuf,
}

#[cfg(not(feature = "layrfs-fuse"))]
impl FastSource {
    pub fn finish(self, _dst: &Path, _readonly: bool) -> Result<()> {
        Ok(())
    }
}

/// Remove the copies that fast lines no longer need; returns how many went:
/// - a shared checkout of a start state (`meta/shadows/<tree>-<times>`) unused for `seed_ttl`;
/// - read-only snapshots of a line's shadow (`shadow-ro/`) but the newest, after ten minutes
///   (only the command that made one uses it);
/// - the shadow of a line that no longer exists (a crash between the record and the cleanup).
pub fn sweep_fast(repo: &Repo, seed_ttl: std::time::Duration, ro_ttl: std::time::Duration) -> Result<usize> {
    let old = |p: &Path, ttl: std::time::Duration| {
        std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d >= ttl)
    };
    let mut n = 0;
    let shadows = repo.p.meta_dir().join("shadows");
    if let Ok(rd) = std::fs::read_dir(&shadows) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".used") || name.starts_with('.') {
                continue;
            }
            let used = shadows.join(format!("{name}.used"));
            let mark = if used.exists() { used.clone() } else { p.clone() };
            if old(&mark, seed_ttl) {
                crate::fs::remove(&p)?;
                let _ = std::fs::remove_file(&used);
                n += 1;
            }
        }
    }
    let Ok(rd) = std::fs::read_dir(repo.p.meta_dir().join("vlines")) else { return Ok(n) };
    for e in rd.flatten() {
        let vdir = e.path();
        let id = e.file_name().to_string_lossy().into_owned();
        let fast = std::fs::read(vdir.join("native.json")).ok().and_then(|d| serde_json::from_slice::<View>(&d).ok()).is_some_and(|v| v.is_layrfs());
        let gone = repo.p.db.line_by_id(&id)?.is_none();
        if gone && (fast || vdir.join("shadow").exists()) {
            for sub in ["shadow"] {
                if vdir.join(sub).symlink_metadata().is_ok() {
                    crate::fs::remove(&vdir.join(sub))?;
                    n += 1;
                }
            }
        }
        let current: Option<String> = std::fs::read(vdir.join("shadow.json"))
            .ok()
            .and_then(|d| serde_json::from_slice::<(String, String)>(&d).ok())
            .map(|(t, m)| format!("{t}-{m}"));
        if let Ok(ros) = std::fs::read_dir(vdir.join("shadow-ro")) {
            for r in ros.flatten() {
                let keep = !gone && current.as_deref() == Some(&*r.file_name().to_string_lossy());
                if !keep && (gone || old(&r.path(), ro_ttl)) {
                    crate::fs::remove(&r.path())?;
                    n += 1;
                }
            }
        }
        if gone {
            let _ = std::fs::remove_dir(vdir.join("shadow-ro"));
            let _ = std::fs::remove_file(vdir.join("shadow.json"));
            let _ = std::fs::remove_dir_all(vdir.join("object-reads"));
        }
    }
    Ok(n)
}

/// The states that fast lines rest on (`B <state>` of each view's journal, and the start state):
/// their objects must stay.
pub fn fast_bases(p: &crate::store::Project) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(rd) = std::fs::read_dir(p.meta_dir().join("vlines")) else { return out };
    for e in rd.flatten() {
        let Some(v) = std::fs::read(e.path().join("native.json")).ok().and_then(|d| serde_json::from_slice::<View>(&d).ok()) else {
            continue;
        };
        if !v.is_layrfs() {
            continue;
        }
        out.insert(v.base.clone());
        // The state whose trees the clean folders of the view name (`adopt`).
        if let Ok(a) = std::fs::read_to_string(e.path().join("fs").join("adopted")) {
            out.insert(a.trim().to_string());
        }
        if let Ok(j) = std::fs::read_to_string(e.path().join("fs").join("journal")) {
            if let Some(b) = j.lines().next().and_then(|l| l.strip_prefix("B ")) {
                out.insert(b.to_string());
            }
        }
    }
    out
}

/// Mount a fast line again at `to` (a folder that does not exist yet), then remove `from`.
pub fn fast_move(repo: &Repo, line: &Line, from: &Path, to: &Path) -> Result<()> {
    #[cfg(feature = "layrfs-fuse")]
    return lfs::move_mount(repo, line, from, to);
    #[cfg(not(feature = "layrfs-fuse"))]
    {
        let _ = (repo, line, from, to);
        bail!("fast lines need a layr built with the feature layrfs-fuse")
    }
}

/// A line whose folder is a LayrFS mount on this machine.
/// The macOS views keep the shadow of a fast line locked for a command; a Linux fast line has
/// no shadow, so there is nothing to free (`cmd::adopt_git`).
pub fn release_shadow(_p: &crate::store::Project, _line_id: &str) {}

pub fn is_layrfs_line(p: &crate::store::Project, id: &str) -> bool {
    std::fs::read(p.meta_dir().join("vlines").join(id).join("native.json"))
        .ok()
        .and_then(|d| serde_json::from_slice::<View>(&d).ok())
        .is_some_and(|v| v.is_layrfs())
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
        let view = View { line: line.id.clone(), name: name.to_owned(), backend: String::new(), base: String::new() };
        crate::fsutil::atomic_write(&vdir.join("native.json"), &serde_json::to_vec(&view)?, 0o600)
    })();
    if let Err(e) = made {
        repo.discard_prepared_lines(prepared);
        let _ = std::fs::remove_dir_all(&vdir);
        return Err(e);
    }
    let _lock = repo.p.lock_maintenance()?;
    match repo.commit_prepared_lines(&_lock, ctx, prepared) {
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

/// The files of a deleted fast line (upper files, journal): its server keeps them until the
/// record of the delete (`lfs::delete`), then they go.
pub fn drop_fast_files(repo: &Repo, id: &str) -> Result<()> {
    let fs = dir(repo, id).join("fs");
    if fs.symlink_metadata().is_ok() {
        repo.p.trash(&fs)?;
    }
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
        // Fast lines take every command (feature parity, docs/layrfs-lab.md).
        if view.name == name && !view.is_layrfs() {
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

pub fn diff_input(
    repo: &Repo,
    line: &Line,
    _view: &View,
    keep: &mut Vec<crate::repo::Temp>,
) -> Result<(crate::repo::Snap, PathBuf, Vec<crate::changes::EntryChange>)> {
    ensure_no_pending_move(repo, line)?;
    let head = repo.state_src(&line.head)?;
    let scan = repo.scan(line)?;
    let working = scan.path.clone();
    keep.push(scan);
    let rules = crate::ignore_rules::Rules::new(&working);
    let list = crate::changes::file_changes(&head, &working)?
        .into_iter()
        .filter(|c| !crate::repo::skip_path(c.path()))
        .filter(|c| c.old().is_some() || !rules.ignored(c.path(), false))
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

#[cfg(not(feature = "layrfs-fuse"))]
mod lfs {
    use super::*;
    fn off() -> anyhow::Error {
        anyhow::anyhow!("LayrFS lines need a layr built with the feature layrfs-fuse")
    }
    pub fn create(_: &Ctx, _: &Repo, _: &str, _: &str, _: u32) -> Result<Line> {
        Err(off())
    }
    pub fn show(_: &Ctx, _: &Repo, _: &Line, _: &View, _: bool, _: bool) -> Result<i32> {
        Err(off())
    }
}


#[cfg(not(feature = "layrfs-fuse"))]
pub fn delete_layrfs(_: &Ctx, _: &Repo, _: &Line, _: crate::store::LineLeaseSet, _: bool, _: impl Fn(&Line) -> Result<()>) -> Result<()> {
    bail!("LayrFS lines need a layr built with the feature layrfs-fuse")
}

#[cfg(feature = "layrfs-fuse")]
pub use lfs::delete as delete_layrfs;

/// The links to cache trees in the folder of fast line `line` (`fast_cache`): path and target.
pub fn cache_links(repo: &Repo, line: &Line) -> Result<Vec<(String, PathBuf)>> {
    #[cfg(feature = "layrfs-fuse")]
    return lfs::cache_links(repo, line);
    #[cfg(not(feature = "layrfs-fuse"))]
    {
        let _ = (repo, line);
        Ok(Vec::new())
    }
}

/// Tell the view of fast line `line` that its last snapshot became state `state` (`Inner::adopt`).
/// Only a speed-up: a failure changes nothing.
pub fn fast_adopt(line: &Line, state: &str) {
    #[cfg(feature = "layrfs-fuse")]
    {
        let _t = crate::trace::span("fast line adopt");
        let _ = lfs::adopt(line, state);
    }
    #[cfg(not(feature = "layrfs-fuse"))]
    let _ = (line, state);
}

/// Mount a fast line again when its server is gone (a restart of the machine or a crash of the
/// service). The service does it for every fast line at its start; a command does it for the
/// lines it reads (`Repo::working_src`).
pub fn ensure_fast_mounted(repo: &Repo, line: &Line) -> Result<()> {
    #[cfg(feature = "layrfs-fuse")]
    return lfs::ensure_mounted(repo, line);
    #[cfg(not(feature = "layrfs-fuse"))]
    {
        let _ = (repo, line);
        Ok(())
    }
}

#[cfg(feature = "layrfs-fuse")]
mod lfs {
    use super::*;
    use crate::layrfs::view::{ensure_hub, request};
    use crate::model::{LineChange, Record, StateKind, StateRec};
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    /// The folder of the service socket: the hub and the control sockets of views live there.
    fn run_dir() -> PathBuf {
        crate::service::socket_path().parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/run/layr"))
    }

    fn hub_sock() -> PathBuf {
        run_dir().join("layrfs-hub.sock")
    }

    /// The control socket of a view: the owner's `layr tool rg|find` and git's fsmonitor hook use it.
    fn line_sock(id: &str) -> PathBuf {
        run_dir().join("views").join(format!("{id}.sock"))
    }

    fn hidden(repo: &Repo, id: &str) -> PathBuf {
        dir(repo, id).join("mnt")
    }

    /// Mount the view of `line` at `at` in the hub. The journal in `meta/vlines/<id>/fs` names the
    /// state it rests on once the view saved; `base` is the state of a new view.
    fn mount(repo: &Repo, line: &Line, base: &str, at: &Path) -> Result<()> {
        let _t = crate::trace::span("layrfs mount");
        let run = run_dir();
        std::fs::create_dir_all(run.join("views"))?;
        std::fs::set_permissions(run.join("views"), std::fs::Permissions::from_mode(0o755))?;
        ensure_hub(&hub_sock(), &run.join("layrfs-hub.log"))?;
        let owner = line.owner.to_string();
        let args: Vec<String> = vec![
            "--objects".into(),
            crate::objstore::dir(&repo.p).display().to_string(),
            "--state".into(),
            base.into(),
            "--line".into(),
            dir(repo, &line.id).join("fs").display().to_string(),
            "--mnt".into(),
            at.display().to_string(),
            "--sock".into(),
            line_sock(&line.id).display().to_string(),
            "--allow-other".into(),
            "--allow".into(),
            owner.clone(),
            "--owner".into(),
            owner,
            "--group".into(),
            crate::privs::user(line.owner)?.gid.to_string(),
            "--no-open".into(),
        ];
        let (code, text) = request(&hub_sock(), &serde_json::json!({"op": "new", "args": args}))?;
        if code != 0 {
            bail!("cannot mount the LayrFS line at {}: {}", at.display(), text.trim());
        }
        Ok(())
    }

    fn stop(line: &Line, remove: bool) -> Result<()> {
        let mut req = serde_json::json!({"op": "stop"});
        if remove {
            req["state"] = "remove".into();
        }
        let (code, text) = request(&line_sock(&line.id), &req)?;
        if code != 0 {
            bail!("cannot stop the LayrFS line {}: {}", line.name, text.trim());
        }
        Ok(())
    }

    pub(super) fn adopt(line: &Line, state: &str) -> Result<()> {
        request(&line_sock(&line.id), &serde_json::json!({"op": "adopt", "state": state})).map(|_| ())
    }

    pub(super) fn cache_links(repo: &Repo, line: &Line) -> Result<Vec<(String, PathBuf)>> {
        ensure_mounted(repo, line)?;
        let (code, text) = request(&line_sock(&line.id), &serde_json::json!({"op": "links"}))?;
        if code != 0 {
            bail!("the fast line {} gave no links: {}", line.name, text.trim());
        }
        let list: Vec<(String, String)> = serde_json::from_str(text.trim())?;
        Ok(list.into_iter().map(|(p, t)| (p, PathBuf::from(t))).collect())
    }

    /// Mount the view again when its server is gone (a restart of the machine or of the hub).
    pub(super) fn ensure_mounted(repo: &Repo, line: &Line) -> Result<()> {
        if std::os::unix::net::UnixStream::connect(line_sock(&line.id)).is_ok() {
            return Ok(());
        }
        let view = view_of(repo, line).context("the line has no view")?;
        let at = repo.working(line);
        // The mount of a server that is gone (a crash of the service) answers ENOTCONN: it goes
        // first. The line's changes are in its own folder, not in the mount.
        if std::fs::symlink_metadata(&at).is_err_and(|e| e.raw_os_error() == Some(libc::ENOTCONN)) {
            let _t = crate::trace::span(format!("unmount the dead mount of fast line {}", line.name));
            rustix::mount::unmount(&at, rustix::mount::UnmountFlags::DETACH)
                .with_context(|| format!("unmount the dead mount at {}", at.display()))?;
        }
        mount(repo, line, &view.base, &at)
    }

    pub fn create(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32) -> Result<Line> {
        crate::store::check_name("line", name)?;
        if repo.p.db.line_by_name(name)?.is_some() {
            bail!("a line named '{name}' already exists");
        }
        let at = repo.p.line_path(name);
        if at.symlink_metadata().is_ok() || repo.p.kept_folder_line(name).is_some() {
            bail!("{} already exists", at.display());
        }
        repo.source_state(start)?;
        let (store, r) = crate::objstore::verified(&repo.p, start)?.with_context(|| {
            format!("state {} has no objects; a LayrFS line needs them (layr migrate)", crate::ids::short(start))
        })?;
        // The view finds its state by name.
        store.set_state(start, &r)?;
        let line = Line {
            id: crate::ids::new_id(),
            name: name.to_string(),
            head: start.to_string(),
            owner,
            machine: repo.p.store.machine.id.clone(),
        };
        let vdir = dir(repo, &line.id);
        std::fs::create_dir_all(&vdir)?;
        std::fs::set_permissions(&vdir, std::fs::Permissions::from_mode(0o700))?;
        if let Err(e) = mount(repo, &line, start, &at) {
            let _ = std::fs::remove_dir_all(&vdir);
            return Err(e);
        }
        let recorded = (|| -> Result<()> {
            let _lock = repo.p.lock_maintenance()?;
            if repo.p.db.line_by_name(name)?.is_some() {
                bail!("a line named '{name}' already exists");
            }
            let mut rec = crate::model::DraftRecord::new("line.create");
            rec.data = serde_json::json!({"name": name, "start": start});
            rec.lines.insert(
                line.id.clone(),
                LineChange {
                    transition: crate::model::LineTransition::Create { after: repo.line_state(&line, start) },
                    working_before: None,
                    working_after: None,
                },
            );
            _lock.append(rec, &ctx.actor())?;
            let view = View { line: line.id.clone(), name: name.to_owned(), backend: "layrfs".into(), base: start.to_string() };
            crate::fsutil::atomic_write(&vdir.join("native.json"), &serde_json::to_vec(&view)?, 0o600)
        })();
        if let Err(e) = recorded {
            if repo.p.db.line_by_id(&line.id)?.is_none() {
                let _ = stop(&line, true);
                let _ = std::fs::remove_dir_all(&vdir);
            }
            return Err(e);
        }
        Ok(line)
    }

    /// Compares a snapshot of the view with the head. Returns the head, the folder where the view
    /// is mounted, the file changes without new ignored files, and the topmost new paths that the
    /// ignore rules leave out of a save.
    fn changes(repo: &Repo, line: &Line) -> Result<(crate::repo::Snap, PathBuf, Vec<crate::changes::EntryChange>, BTreeSet<Vec<u8>>)> {
        let _t = crate::trace::span("layrfs snapshot");
        ensure_mounted(repo, line)?;
        let head = repo.state_src(&line.head)?;
        let (code, text) = request(&line_sock(&line.id), &serde_json::json!({"op": "snapshot"}))?;
        if code != 0 {
            bail!("the LayrFS line {} gave no snapshot: {}", line.name, text.trim());
        }
        let v: serde_json::Value = serde_json::from_str(text.trim())?;
        let id = |k: &str| -> Result<layr_objects::Id> {
            Ok(layr_objects::Id::parse(v.get(k).and_then(|x| x.as_str()).context("snapshot without ids")?)?)
        };
        let refs = layr_objects::StateRef { tree: id("tree")?, times: id("times")? };
        let tree =
            crate::object_tree::ObjectTree::new(crate::objstore::open(&repo.p)?, refs, &repo.p.meta_dir().join("object-reads"))?;
        let working = repo.working(line);
        let rules = crate::ignore_rules::Rules::new(&working);
        let mut skip = BTreeSet::new();
        let mut list = Vec::new();
        for c in crate::changes::file_changes(&head, &crate::repo::Snap::Objects(tree))? {
            if crate::repo::skip_path(c.path()) {
                continue;
            }
            if c.old().is_some() || !rules.ignored(c.path(), false) {
                list.push(c);
                continue;
            }
            // The topmost folder (or the file) that is new and ignored.
            let parts: Vec<&str> = c.path().split('/').collect();
            for i in 1..=parts.len() {
                let p = parts[..i].join("/");
                let dir = i < parts.len();
                if crate::fsutil::lmeta(&head, &p).is_none() && rules.ignored(&p, dir) {
                    skip.insert(p.into_bytes());
                    break;
                }
            }
        }
        Ok((head, working, list, skip))
    }

    /// The mount as a source of an object write, without new ignored paths.
    struct Source<'a> {
        root: &'a Path,
        skip: &'a BTreeSet<Vec<u8>>,
    }

    impl layr_objects::write::Source for Source<'_> {
        fn path(&self, relative: &[u8]) -> Result<PathBuf> {
            use std::os::unix::ffi::OsStrExt;
            Ok(self.root.join(std::ffi::OsStr::from_bytes(relative)))
        }
        fn names(&self, relative: &[u8]) -> Result<Vec<Vec<u8>>> {
            use std::os::unix::ffi::OsStrExt;
            let mut out = Vec::new();
            for e in std::fs::read_dir(self.path(relative)?)? {
                let name = e?.file_name().as_bytes().to_vec();
                let full = if relative.is_empty() { name.clone() } else { [relative, b"/", &name].concat() };
                if !self.skip.contains(&full) {
                    out.push(name);
                }
            }
            Ok(out)
        }
    }

    /// A state with the changes of `list` over the line's head, read from the mount: objects only.
    #[allow(clippy::too_many_arguments)]
    fn write_state(
        ctx: &Ctx,
        repo: &Repo,
        line: &Line,
        working: &Path,
        list: &[crate::changes::EntryChange],
        skip: &BTreeSet<Vec<u8>>,
        kind: StateKind,
        message: &str,
    ) -> Result<StateRec> {
        let _t = crate::trace::span("layrfs write state");
        let (store, base) = crate::objstore::verified(&repo.p, &line.head)?
            .with_context(|| format!("the head {} has no objects", crate::ids::short(&line.head)))?;
        let mut paths = BTreeSet::new();
        for c in list {
            paths.insert(c.path().to_string().into_bytes());
            if let Some(o) = c.old_path() {
                paths.insert(o.clone().into_bytes());
            }
        }
        let paths: Vec<Vec<u8>> = paths.into_iter().collect();
        let options = layr_objects::WriteOptions {
            temperature: layr_objects::Temperature::Hot,
            previous: None,
            exclude: Vec::new(),
            similar: false,
        };
        let written = layr_objects::write::write_changed_from(
            &store,
            &Source { root: working, skip },
            &layr_objects::write::Base { tree: base.tree, times: base.times },
            &paths,
            &options,
        )?;
        let refs = layr_objects::StateRef {
            tree: written.tree.context("object write gave no tree")?,
            times: written.times.context("object write gave no times")?,
        };
        let id = crate::ids::new_id();
        store.set_state(&id, &refs)?;
        let mut st = repo.state_record(ctx, id, kind, vec![line.head.clone()], message, Some(line), String::new());
        st.tree = Some(refs.tree.hex());
        st.times = Some(refs.times.hex());
        Ok(st)
    }

    /// `layr save -m <message>` of a LayrFS line with the virtual-line rules (every change). Fast
    /// lines take the folder paths now; kept for the object-only save (docs/layrfs-lab.md).
    #[allow(dead_code)]
    pub fn save(ctx: &Ctx, h: &crate::cmd::Here, line: &Line, _v: &View, message: &str, quiet: bool, json: bool) -> Result<i32> {
        let repo = &h.repo;
        repo.authorize_line(ctx, line, crate::repo::LineOp::Direct)?;
        let _line_lock = repo.line_lock(line)?;
        let line = repo.line(&line.id)?;
        let (head, working, list, skip) = changes(repo, &line)?;
        if list.is_empty() {
            if json {
                outln!(ctx, "{}", crate::cmd::work::nothing_json(&line));
            } else {
                outln!(ctx, "On line {}\nnothing to save, working tree clean", line.name);
            }
            return Ok(1);
        }
        let st = write_state(ctx, repo, &line, &working, &list, &skip, StateKind::Commit, message)?;
        let moved = repo.commit_line(&_line_lock, &line, &line.head, |locked, now| {
            repo.move_head(locked, ctx, "commit", now, &st.id, vec![st.clone()], None, serde_json::json!({"line": line.id}))
        });
        if let Err(e) = moved {
            if matches!(e, crate::db::CommitResolution::NotCommitted(_)) {
                repo.discard_states(std::slice::from_ref(&st));
            }
            return Err(e.into());
        }
        let new_root = repo.state_src(&st.id)?;
        let shown = crate::changes::with_renames(&head, &new_root, list);
        if json {
            let fds = crate::diff::file_diffs(&head, &new_root, &shown)?;
            let mut out = crate::cmd::work::saved_json(&line, &st, &fds);
            out["amend"] = serde_json::json!(false);
            outln!(ctx, "{out}");
        } else if !quiet {
            outln!(ctx, "[{} {}] {}", line.name, crate::ids::short(&st.id), message.lines().next().unwrap_or(""));
            for c in shown {
                let code = match (c.old(), c.new(), c.old_path()) {
                    (_, _, Some(o)) => format!("rename {o} =>"),
                    (None, _, _) => "create".into(),
                    (_, None, _) => "delete".into(),
                    _ => "modify".into(),
                };
                outln!(ctx, " {code} {}", c.path());
            }
        }
        Ok(0)
    }

    /// A snapshot of a fast line's mount at `dst`: the view's content as objects, put into the
    /// line's shadow subvolume (`meta/vlines/<id>/shadow`; only the paths that changed since the
    /// last snapshot are written), then a native snapshot of the shadow.
    /// The socket of the fast line whose mount is `src` (None when `src` is not one).
    fn mount_socket(src: &Path) -> Option<PathBuf> {
        let m = crate::layrfs::viewclient::find(src)?;
        // Only the service's hub (root) serves fast lines.
        (m.root == src && m.owner == 0).then_some(m.socket)
    }

    pub fn is_mount(src: &Path) -> bool {
        mount_socket(src).is_some()
    }

    pub fn source_of_mount(src: &Path) -> Option<Result<FastSource>> {
        let sock = mount_socket(src)?;
        let _t = crate::trace::span("fast line shadow");
        let lock = |vdir: &Path| -> Result<std::fs::File> {
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(vdir.join("shadow.lock"))?;
            rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
            Ok(f)
        };
        Some(shadow_source(&sock, &lock))
    }

    /// The folder of a fast line as the source of a copy (`crate::fs`): its shadow, brought up to
    /// the view, under the shadow lock until `finish`. Two commands on one line take turns: the
    /// update and the copy are one step.
    pub struct FastSource {
        pub shadow: PathBuf,
        _held: std::fs::File,
        vdir: PathBuf,
        new: layr_objects::StateRef,
        objects: PathBuf,
    }

    impl FastSource {
        /// After the copy of the shadow at `dst`: its objects are known, and a read-only snapshot
        /// of the shadow at these objects is the base of later object writes of copies that change
        /// (an index, a work folder).
        pub fn finish(self, dst: &Path, readonly: bool) -> Result<()> {
            let open = || layr_objects::Store::open_existing(&self.objects, layr_objects::Keying::Plain);
            let reads = self.vdir.join("object-reads");
            sweep_reads(&reads);
            let ros = self.vdir.join("shadow-ro");
            // A frozen copy needs a private parent (`crate::fs`).
            std::fs::create_dir_all(&ros)?;
            std::fs::set_permissions(&ros, std::fs::Permissions::from_mode(0o700))?;
            let ro = ros.join(format!("{}-{}", self.new.tree.hex(), self.new.times.hex()));
            if !ro.exists() {
                crate::fs::snapshot(&self.shadow, &ro, true)?;
            }
            let base = crate::object_tree::Base { ro, refs: self.new, objects: self.objects.clone(), reads: reads.clone() };
            if readonly {
                // A read-only snapshot stays equal to the objects: comparisons with it use them.
                crate::object_tree::register(dst, crate::object_tree::ObjectTree::new(open()?, self.new, &reads)?);
            }
            crate::object_tree::register_base(dst, base);
            Ok(())
        }
    }

    fn shadow_source(sock: &Path, lock: &dyn Fn(&Path) -> Result<std::fs::File>) -> Result<FastSource> {
        let (code, text) = request(sock, &serde_json::json!({"op": "snapshot"}))?;
        if code != 0 {
            bail!("the fast line gave no snapshot: {}", text.trim());
        }
        let v: serde_json::Value = serde_json::from_str(text.trim())?;
        let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned).context("a snapshot answer without its fields");
        let new = layr_objects::StateRef { tree: layr_objects::Id::parse(&field("tree")?)?, times: layr_objects::Id::parse(&field("times")?)? };
        let objects = PathBuf::from(field("objects")?);
        let vdir = PathBuf::from(field("line")?).parent().context("the line folder of a view")?.to_path_buf();
        let _held = lock(&vdir)?;
        let shadow = vdir.join("shadow");
        let mark = vdir.join("shadow.json");
        let old: Option<(String, String)> = std::fs::read(&mark).ok().and_then(|d| serde_json::from_slice(&d).ok());
        let open = || layr_objects::Store::open_existing(&objects, layr_objects::Keying::Plain);
        match old {
            Some((t, m)) if shadow.exists() => {
                let old = layr_objects::StateRef { tree: layr_objects::Id::parse(&t)?, times: layr_objects::Id::parse(&m)? };
                if old != new {
                    let _t = crate::trace::span("shadow update");
                    let cache = vdir.join("object-reads");
                    let updated = (|| -> Result<()> {
                        let a = crate::repo::Snap::Objects(crate::object_tree::ObjectTree::new(open()?, old, &cache)?);
                        let b = crate::repo::Snap::Objects(crate::object_tree::ObjectTree::new(open()?, new, &cache)?);
                        // Folders count too: the shadow must hold the tree of `new`, also its
                        // empty folders, and no folder that `new` lost.
                        let all = crate::changes::candidates(&a, &b)?;
                        let dir = |m: Option<&crate::fsutil::Meta>| m.map(|m| m.is_dir());
                        let dirs: Vec<(String, Option<u32>)> = all
                            .iter()
                            .filter(|c| c.old_path().is_none())
                            // A new folder (also in place of a file), or a folder that went (also
                            // for a file in its place).
                            .filter(|c| dir(c.new()) == Some(true) || dir(c.old()) == Some(true))
                            .map(|c| (c.path().to_string(), c.new().filter(|m| m.is_dir()).map(|m| m.mode & 0o7777)))
                            .collect();
                        let list = crate::changes::file_changes_of(&a, &b, all)?;
                        for c in &list {
                            if let Some(o) = c.old_path() {
                                crate::fsutil::remove_entry(&shadow, o)?;
                            }
                            if c.new().is_none() {
                                crate::fsutil::remove_entry(&shadow, c.path())?;
                            }
                        }
                        for (path, mode) in &dirs {
                            let at = crate::fsutil::safe_join(&shadow, path)?;
                            match mode {
                                None if at.symlink_metadata().is_ok() => crate::fsutil::remove_entry(&shadow, path)?,
                                None => {}
                                Some(_) => std::fs::create_dir_all(&at)?,
                            }
                        }
                        for c in &list {
                            if c.new().is_some() {
                                crate::fsutil::copy_entry(&b, c.path(), &shadow, c.path(), None)?;
                            }
                        }
                        // The modes last: a read-only folder still took its files.
                        for (path, mode) in &dirs {
                            if let Some(mode) = mode {
                                std::fs::set_permissions(crate::fsutil::safe_join(&shadow, path)?, std::fs::Permissions::from_mode(*mode))?;
                            }
                        }
                        Ok(())
                    })();
                    // The objects of the last update can be gone (`gc` keeps the objects of states
                    // only): the shadow is made again from the start state.
                    if let Err(e) = updated {
                        let _t = crate::trace::span(format!("shadow made again: {e:#}"));
                        let _ = std::fs::remove_file(&mark);
                        crate::fs::remove(&shadow)?;
                        drop(_held);
                        return shadow_source(sock, lock);
                    }
                }
            }
            _ => {
                if shadow.symlink_metadata().is_ok() {
                    crate::fs::remove(&shadow)?;
                }
                // The first shadow of a line: a snapshot of the shared checkout of its start
                // state (made once per state), then the line's changes on top.
                let start = seed(&objects, &vdir, open)?;
                match start {
                    Some((base, refs)) => {
                        crate::fs::snapshot(&base, &shadow, false)?;
                        crate::fsutil::atomic_write(&mark, &serde_json::to_vec(&(refs.tree.hex(), refs.times.hex()))?, 0o600)?;
                        drop(_held);
                        return shadow_source(sock, lock);
                    }
                    None => {
                        let _t = crate::trace::span("shadow checkout");
                        crate::fs::create(&shadow)?;
                        layr_objects::checkout_with(&open()?, &new.tree, Some(&new.times), &shadow)?;
                    }
                }
            }
        }
        crate::fsutil::atomic_write(&mark, &serde_json::to_vec(&(new.tree.hex(), new.times.hex()))?, 0o600)?;
        Ok(FastSource { shadow, _held, vdir, new, objects })
    }

    /// Read caches of earlier commands (an hour old) go.
    fn sweep_reads(dir: &Path) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() > 3600);
            if old {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    /// The shared read-only checkout of the state a fast line started from:
    /// `<objects>/../shadows/<tree>-<times>`, made once. None when the line has no start state.
    fn seed(
        objects: &Path,
        vdir: &Path,
        open: impl Fn() -> Result<layr_objects::Store>,
    ) -> Result<Option<(PathBuf, layr_objects::StateRef)>> {
        let Some(view) = std::fs::read(vdir.join("native.json")).ok().and_then(|d| serde_json::from_slice::<View>(&d).ok()) else {
            return Ok(None);
        };
        let store = open()?;
        let Some(refs) = store.state_ref(&view.base)? else { return Ok(None) };
        let dir = objects.parent().context("the object store folder")?.join("shadows");
        let base = dir.join(format!("{}-{}", refs.tree.hex(), refs.times.hex()));
        // The time of the last use: `sweep_fast` removes a checkout that no new line used for a day.
        let _ = std::fs::write(dir.join(format!("{}-{}.used", refs.tree.hex(), refs.times.hex())), b"");
        if !base.exists() {
            let _t = crate::trace::span("shared shadow checkout");
            // A frozen copy needs a private parent (`crate::fs`).
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            let tmp = dir.join(crate::store::temp_name());
            crate::fs::create(&tmp)?;
            let made = layr_objects::checkout_with(&store, &refs.tree, Some(&refs.times), &tmp)
                .map_err(anyhow::Error::from)
                .and_then(|_| crate::fs::snapshot(&tmp, &base, true));
            let _ = crate::fs::remove(&tmp);
            if let Err(e) = made {
                if !base.exists() {
                    return Err(e);
                }
            }
        }
        Ok(Some((base, refs)))
    }

    /// The shadow of a fast line and its caches go with the line.
    fn drop_shadow(repo: &Repo, id: &str) -> Result<()> {
        let vdir = dir(repo, id);
        if vdir.join("shadow").symlink_metadata().is_ok() {
            crate::fs::remove(&vdir.join("shadow"))?;
        }
        let _ = std::fs::remove_file(vdir.join("shadow.json"));
        let _ = std::fs::remove_file(vdir.join("shadow.lock"));
        if let Ok(rd) = std::fs::read_dir(vdir.join("shadow-ro")) {
            for e in rd.flatten() {
                crate::fs::remove(&e.path())?;
            }
            let _ = std::fs::remove_dir(vdir.join("shadow-ro"));
        }
        let _ = std::fs::remove_dir_all(vdir.join("object-reads"));
        Ok(())
    }

    /// Delete a LayrFS line with the final-save protocol of folder lines: unsaved changes and
    /// the index become its final states, prepared before the project lock under the
    /// preparation lease (`branch::prepare_deletion`). The server stops after the record.
    pub fn delete(
        ctx: &Ctx,
        repo: &Repo,
        l: &Line,
        line_lock: crate::store::LineLeaseSet,
        discard: bool,
        check: impl Fn(&Line) -> Result<()>,
    ) -> Result<()> {
        let l = repo.line(&l.id)?;
        let made = repo.working(&l).exists();
        let (lock, fresh, last, staged) = crate::cmd::branch::prepare_deletion(ctx, repo, &l, line_lock, made, discard, || {
            let fresh = repo.line(&l.id)?;
            check(&fresh)?;
            Ok(fresh)
        })?;
        // The final states are prepared: the server stops and keeps the line's files (upper
        // files, journal); until the record below, a crash keeps the line with its work.
        let recorded = (|| -> Result<()> {
            stop(&fresh, false)?;
            drop_shadow(repo, &fresh.id)?;
            // The empty mount point goes (a hidden line has it in its view folder); the record,
            // then the line's files go (`drop_fast_files`, `forget`).
            let _ = std::fs::remove_dir(repo.working(&fresh));
            crate::cmd::branch::delete_recorded(&lock, ctx, repo, &fresh, last, false, staged)
        })();
        if let Err(e) = recorded {
            drop(lock);
            if let Err(m) = ensure_mounted(repo, &fresh) {
                eprintln!("warning: cannot mount the fast line {} again: {m:#}", fresh.name);
            }
            return Err(e);
        }
        drop(lock);
        crate::fast_cache::remove(&repo.p, &fresh.id)?;
        Ok(())
    }

    /// `line move` of a fast line: the folder is made for the owner (as a moved folder line's),
    /// the server mounts the line there (the journal keeps every change), and the old folder goes.
    pub fn move_mount(repo: &Repo, line: &Line, from: &Path, to: &Path) -> Result<()> {
        ensure_mounted(repo, line)?;
        let view = view_of(repo, line).context("the line has no view")?;
        std::fs::create_dir(to).with_context(|| format!("mkdir {}", to.display()))?;
        let u = crate::privs::user(line.owner)?;
        rustix::fs::chown(to, Some(rustix::fs::Uid::from_raw(u.uid)), Some(rustix::fs::Gid::from_raw(u.gid)))?;
        stop(line, false)?;
        if let Err(e) = mount(repo, line, &view.base, to) {
            // Back to the old folder: the line stays usable.
            let _ = mount(repo, line, &view.base, from);
            let _ = std::fs::remove_dir(to);
            return Err(e);
        }
        let _ = std::fs::remove_dir(from);
        Ok(())
    }

    /// `line show --none | --virtual` of a LayrFS line: the server mounts it again at the hidden
    /// or the visible folder; the journal keeps every change.
    pub fn show(ctx: &Ctx, repo: &Repo, line: &Line, view: &View, none: bool, folder: bool) -> Result<i32> {
        if folder {
            bail!("a LayrFS line cannot become a folder yet");
        }
        ensure_mounted(repo, line)?;
        let from = repo.working(line);
        let to = if none { hidden(repo, &line.id) } else { repo.p.default_line_path(&line.name) };
        if from != to {
            if to.symlink_metadata().is_ok() {
                bail!("{} already exists", to.display());
            }
            stop(line, false)?;
            let _ = std::fs::remove_dir(&from);
            mount(repo, line, &view.base, &to)?;
            repo.p.set_moved_path(&line.id, if none { Some(to.as_path()) } else { None })?;
        }
        if none {
            outln!(ctx, "Line {} has no visible folder; its changes stay in {}", line.name, to.display());
        } else {
            outln!(ctx, "Line {} has a virtual view at {}", line.name, to.display());
        }
        Ok(0)
    }
}
