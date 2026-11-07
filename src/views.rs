//! Experimental virtual lines (macOS, `LAYR_EXPERIMENTAL_VIEWS=1`, docs/views.md).
//!
//! The working folder of a virtual line is a LayrFS mount (tools/layrfs). The mount merges an
//! upper folder over the complete tree of the state the line started from (the lower tree, which
//! never changes). The upper folder is a thin layer (docs/apfs-thin-states.md): changed files,
//! new files, and the removed paths in its `com.layr.removed` attribute. Status, diff and save
//! read the upper folder and the head; they never scan the merged tree.
//!
//! The kind of view is local to this machine: `meta/vlines/<line id>/view.json`, never a record.
use crate::changes::{self, Change};
use crate::cmd::{exit, here, Here};
use crate::ctx::Ctx;
use crate::fsutil::{self, Meta};
use crate::ids;
use crate::model::{Line, LineChange, Record, StateKind, StateRec};
use crate::repo::{skip_path, Repo, Status};
use crate::store::Project;
use crate::thin::{self, Layered};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The switch of the experiment.
pub const ENV: &str = "LAYR_EXPERIMENTAL_VIEWS";
/// The settings file that makes LayrFS mount a folder as an overlay.
const CONF: &str = "layrfs-view.conf";
const FS_TYPE: &str = "layrfs";
/// Folders that macOS services make at the root of a mount. LayrFS refuses them; older upper
/// folders may hold them. They are no content of the line.
const SYSTEM_ROOT_NAMES: &[&str] = &[".fseventsd", ".Spotlight-V100", ".Trashes", ".TemporaryItems", ".DocumentRevisions-V100"];

/// The local record of a virtual line.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct View {
    /// The line id.
    pub line: String,
    /// The line name when the view was made (lines with a view cannot be renamed).
    pub name: String,
    /// The state whose complete tree is the lower tree.
    pub base: String,
    /// The complete tree of `base` (`states/<base>`).
    pub lower: PathBuf,
    /// False after `layr line show <line> --none`: the layer stays, the mount is gone.
    pub mounted: bool,
}

pub fn enabled(ctx: &Ctx) -> bool {
    ctx.env(ENV) == Some("1")
}

fn require_enabled(ctx: &Ctx) -> Result<()> {
    if !enabled(ctx) {
        return Err(exit(2, format!("virtual lines are experimental: set {ENV}=1 (docs/views.md)")));
    }
    Ok(())
}

fn views_dir(meta: &Path) -> PathBuf {
    meta.join("vlines")
}

fn dir(p: &Project, line_id: &str) -> PathBuf {
    views_dir(&p.meta_dir()).join(line_id)
}

/// The upper folder of a virtual line.
pub fn upper(p: &Project, line_id: &str) -> PathBuf {
    dir(p, line_id).join("upper")
}

fn read_view(file: &Path) -> Option<View> {
    std::fs::read(file).ok().and_then(|d| serde_json::from_slice(&d).ok())
}

/// The view of a line, when it is a virtual line.
pub fn of_line(p: &Project, line_id: &str) -> Option<View> {
    read_view(&dir(p, line_id).join("view.json"))
}

/// Every virtual line of a project.
pub fn all_in(meta: &Path) -> Vec<View> {
    let Ok(rd) = std::fs::read_dir(views_dir(meta)) else { return Vec::new() };
    rd.flatten().filter_map(|e| read_view(&e.path().join("view.json"))).collect()
}

/// The base states of the virtual lines of a project: their trees must stay (`thin` upkeep).
pub fn protected_states(p: &Project) -> BTreeSet<String> {
    all_in(&p.meta_dir()).into_iter().map(|v| v.base).collect()
}

fn write_view(p: &Project, v: &View) -> Result<()> {
    fsutil::atomic_write(&dir(p, &v.line).join("view.json"), &serde_json::to_vec_pretty(v)?, 0o600)
}

pub fn is_mounted(path: &Path) -> bool {
    crate::sys::fs_type_name(path).map(|t| t == FS_TYPE).unwrap_or(false)
}

fn mount(vdir: &Path, at: &Path) -> Result<()> {
    let _t = crate::trace::span("view mount");
    let out = Command::new("/sbin/mount")
        .args(["-F", "-t", FS_TYPE, "-o", "nobrowse"])
        .arg(vdir)
        .arg(at)
        .output()
        .context("run mount")?;
    if !out.status.success() || !is_mounted(at) {
        bail!(
            "cannot mount the virtual line at {}: {}\n(is the LayrFS file system extension installed and enabled? see tools/layrfs/README.md)",
            at.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn unmount(at: &Path) -> Result<()> {
    let _t = crate::trace::span("view unmount");
    let out = Command::new("/sbin/umount").arg(at).output().context("run umount")?;
    if !out.status.success() && is_mounted(at) {
        bail!(
            "cannot unmount {}: {}\n(close the programs and shells that use the folder, then run the command again)",
            at.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The settings file of the mount. `openclose=off`: FSKit sends no open and close calls (the
/// fastest reads measured, docs/views.md).
fn write_conf(vdir: &Path, lower: &Path, ctx: &Ctx) -> Result<()> {
    let mut conf = format!("# LayrFS overlay of a virtual line (docs/views.md)\nlower={}\nopenclose=off\n", lower.display());
    if ctx.env("LAYR_VIEW_PREFETCH") == Some("1") {
        conf.push_str("prefetch=1\n");
    }
    fsutil::atomic_write(&vdir.join(CONF), conf.as_bytes(), 0o600)
}

/// Make a virtual line `name` at `start`: the line record, its upper folder, and the mount at the
/// line's folder. The lower tree is the complete tree of `start`; a thin state gets one once.
pub fn create(ctx: &Ctx, repo: &Repo, name: &str, start: &str, owner: u32) -> Result<Line> {
    require_enabled(ctx)?;
    if owner != ctx.caller.uid {
        bail!("a virtual line belongs to the user who makes it (no --owner)");
    }
    if repo.p.is_overlay() {
        bail!("an overlay project has no virtual lines");
    }
    crate::store::check_name("line", name)?;
    if repo.p.db.line_by_name(name)?.is_some() {
        bail!("a line named '{name}' already exists");
    }
    let at = repo.p.line_path(name);
    if at.symlink_metadata().is_ok() {
        bail!("{} already exists", at.display());
    }
    repo.source_state(start)?;
    let lower = {
        let _t = crate::trace::span("view lower tree");
        repo.state_root(start)?
    };
    let line = Line {
        id: ids::new_id(),
        name: name.to_string(),
        head: start.to_string(),
        owner,
        machine: repo.p.store.machine.id.clone(),
    };
    let vdir = dir(&repo.p, &line.id);
    let up = vdir.join("upper");
    let made = (|| -> Result<()> {
        std::fs::create_dir_all(&up)?;
        std::fs::set_permissions(&vdir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&up, std::fs::Permissions::from_mode(0o700))?;
        // LayrFS names entries by the inode numbers of both trees: one volume.
        if std::fs::metadata(&lower)?.dev() != std::fs::metadata(&up)?.dev() {
            bail!("the base tree {} and the project folder are on different volumes", lower.display());
        }
        write_conf(&vdir, &lower, ctx)?;
        std::fs::create_dir(&at)?;
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
        mount(&vdir, &at)
    })();
    let cleanup = || {
        if is_mounted(&at) {
            let _ = unmount(&at);
        }
        let _ = std::fs::remove_dir(&at);
        let _ = std::fs::remove_dir_all(&vdir);
    };
    if let Err(e) = made {
        cleanup();
        return Err(e);
    }
    let recorded = (|| -> Result<()> {
        let _lock = repo.p.lock()?;
        if repo.p.db.line_by_name(name)?.is_some() {
            bail!("a line named '{name}' already exists");
        }
        repo.source_state(start)?;
        let mut rec = Record::new("line.create");
        rec.data = serde_json::json!({"name": name, "start": start});
        rec.lines.insert(
            line.id.clone(),
            LineChange { before: None, after: Some(repo.line_state(&line, start)), working_before: None, working_after: None },
        );
        repo.p.append(rec, &ctx.actor())?;
        write_view(
            &repo.p,
            &View { line: line.id.clone(), name: name.to_string(), base: start.to_string(), lower, mounted: true },
        )
    })();
    if let Err(e) = recorded {
        if repo.p.db.line_by_id(&line.id)?.is_none() {
            cleanup();
        }
        return Err(e);
    }
    Ok(line)
}

/// Removes the quarantine mark that the sandbox of the LayrFS extension puts on the files it
/// makes. The mount hides it; a saved state must not keep it.
fn strip_quarantine(up: &Path, paths: &BTreeSet<String>) {
    const Q: &std::ffi::CStr = c"com.apple.quarantine";
    for p in paths {
        let path = up.join(p);
        if let Ok(v) = crate::sys::apfs_attribute(&path, Q, None) {
            if String::from_utf8_lossy(&v).contains(";LayrFS;") {
                let _ = crate::sys::apfs_remove_attribute(&path, Q);
            }
        }
    }
}

/// The trees of a virtual line: the merged working content and the head.
pub struct Trees {
    pub upper: PathBuf,
    pub merged: Layered,
    pub head: Layered,
}

fn trees(repo: &Repo, line: &Line, v: &View) -> Result<Trees> {
    let up = upper(&repo.p, &line.id);
    let merged = Layered::over(&up, Layered::complete(&v.lower))?;
    Ok(Trees { upper: up, merged, head: repo.state_src(&line.head)? })
}

fn system_path(p: &str) -> bool {
    let top = p.split('/').next().unwrap_or("");
    SYSTEM_ROOT_NAMES.contains(&top)
}

/// The paths that can differ between the head and the merged view: the entries of the upper
/// folder, its removed paths, and what the saves of this line changed over the lower tree (the
/// layers of the head above it). None when the head no longer rests on the lower tree (a gc
/// keyframe): then a full compare decides.
fn candidates(t: &Trees, v: &View, rules: &crate::ignore_rules::Rules) -> Result<Option<BTreeSet<String>>> {
    let _s = crate::trace::span("view candidates");
    if t.head.bottom() != v.lower.as_path() {
        return Ok(None);
    }
    let mut set = BTreeSet::new();
    let head = &t.head;
    fsutil::walk(t.upper.as_path(), "", &mut |rel, m| {
        if skip_path(rel) || system_path(rel) {
            return Ok(false);
        }
        // A new ignored folder (build output) is no content; its files are not listed.
        if m.is_dir() && rules.ignored(rel, true) && fsutil::lmeta(head, rel).is_none() {
            return Ok(false);
        }
        set.insert(rel.to_string());
        Ok(true)
    })?;
    let lower = Layered::complete(&v.lower);
    let mut removed: BTreeSet<String> = thin::removed_of(&t.upper)?;
    for l in &head.layers[..head.layers.len() - 1] {
        fsutil::walk(l.root.as_path(), "", &mut |rel, _| {
            set.insert(rel.to_string());
            Ok(true)
        })?;
        removed.extend(l.removed.iter().cloned());
    }
    for r in removed {
        for tree in [head, &lower] {
            set.extend(tree.scan_at(&r)?.into_keys().filter(|k| !k.is_empty()));
        }
        set.insert(r);
    }
    Ok(Some(set))
}

/// The confirmed file changes of the merged view against the head, without ignored new files.
fn file_changes(t: &Trees, v: &View) -> Result<Vec<Change>> {
    let rules = crate::ignore_rules::Rules::of(&t.merged);
    let list = match candidates(t, v, &rules)? {
        Some(set) => {
            strip_quarantine(&t.upper, &set);
            let raw: Vec<Change> = set
                .into_iter()
                .filter_map(|p| {
                    let (old, new) = (fsutil::lmeta(&t.head, &p), fsutil::lmeta(&t.merged, &p));
                    (old.is_some() || new.is_some()).then_some(Change { path: p, old_path: None, old, new, ranges: None })
                })
                .collect();
            changes::file_changes_of(&t.head, &t.merged, raw)?
        }
        None => changes::file_changes(&t.head, &t.merged)?,
    };
    Ok(list
        .into_iter()
        .filter(|c| !skip_path(&c.path) && !system_path(&c.path))
        .filter(|c| c.old.is_some() || !rules.ignored(&c.path, false))
        .collect())
}

/// `layr status` of a virtual line: every change is saved by the next `save -m`, so all are staged.
pub fn status(repo: &Repo, line: &Line, v: &View) -> Result<Status> {
    let t = trees(repo, line, v)?;
    let staged = changes::with_renames(&t.head, &t.merged, file_changes(&t, v)?);
    Ok(Status { staged, ..Default::default() })
}

/// The trees and changes for `layr diff` of a virtual line against its head.
pub fn diff_input(repo: &Repo, line: &Line, v: &View) -> Result<(Layered, Layered, Vec<Change>)> {
    let t = trees(repo, line, v)?;
    let list = file_changes(&t, v)?;
    Ok((t.head, t.merged, list))
}

fn topmost(paths: BTreeSet<String>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for p in paths {
        let covered = p.match_indices('/').any(|(i, _)| out.contains(&p[..i]));
        if !covered {
            out.insert(p);
        }
    }
    out
}

/// A new state of the merged view: a thin layer over the head with the changed entries of the
/// upper folder. None when nothing changed. The record is the caller's.
fn write_state(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    t: &Trees,
    list: &[Change],
    kind: StateKind,
    message: &str,
) -> Result<StateRec> {
    let _s = crate::trace::span("view write state");
    let is_dir = |m: &Option<Meta>| m.as_ref().is_some_and(|m| m.is_dir());
    let mut d = thin::Diff::default();
    let mut gone = BTreeSet::new();
    for c in list {
        if c.new.is_some() {
            d.write.insert(c.path.clone());
        }
        if c.old.is_some() && (c.new.is_none() || c.old.as_ref().map(|m| m.ftype) != c.new.as_ref().map(|m| m.ftype)) {
            gone.insert(c.path.clone());
        }
        if let Some(o) = &c.old_path {
            gone.insert(o.clone());
        }
    }
    // Folders: a removed path that the head has as a folder goes as a whole, as on a native save;
    // a new folder of the upper folder is written, also when it is empty.
    for r in thin::removed_of(&t.upper)? {
        if is_dir(&fsutil::lmeta(&t.head, &r)) && !is_dir(&fsutil::lmeta(&t.merged, &r)) {
            gone.insert(r);
        }
    }
    let rules = crate::ignore_rules::Rules::of(&t.merged);
    fsutil::walk(t.upper.as_path(), "", &mut |rel, m| {
        if !m.is_dir() || skip_path(rel) || system_path(rel) || rules.ignored(rel, true) {
            return Ok(false);
        }
        if !is_dir(&fsutil::lmeta(&t.head, rel)) {
            d.write.insert(rel.to_string());
        }
        Ok(true)
    })?;
    // A path below a removed folder of the head that comes back is written; the folder is new.
    d.removed = topmost(gone);
    let id = ids::new_id();
    let dst = repo.p.delta_path(&id);
    thin::write_layer(&t.upper, &dst, &d, &line.head, false, None).map_err(|e| {
        if thin::needs_complete(&e) {
            e.context("a virtual line cannot save hard links yet; convert it with 'layr line show <line> --folder'")
        } else {
            e
        }
    })?;
    let described = (|| -> Result<StateRec> {
        repo.p.db.record_base(&id, &line.head)?;
        repo.describe_state(ctx, id.clone(), kind, vec![line.head.clone()], message, Some(line))
    })();
    if described.is_err() {
        let _ = repo.p.delete_state(&id);
    }
    described
}

/// `layr save -m <message>` of a virtual line: every change of the view becomes the new head.
pub fn save(ctx: &Ctx, h: &Here, line: &Line, v: &View, message: &str, quiet: bool) -> Result<i32> {
    let repo = &h.repo;
    repo.authorize_line(ctx, line, crate::repo::LineOp::Direct)?;
    let _line_lock = repo.line_lock(line)?;
    let line = repo.line(&line.id)?;
    let t = trees(repo, &line, v)?;
    let list = file_changes(&t, v)?;
    if list.is_empty() {
        outln!(ctx, "On line {}\nnothing to save, working tree clean", line.name);
        return Ok(1);
    }
    crate::secrets::check_changes(ctx, repo, &t.head, &t.merged, &line.head, &list)?;
    let st = write_state(ctx, repo, &line, &t, &list, StateKind::Commit, message)?;
    let moved = repo.commit_line(&line, &line.head, |now| {
        repo.move_head(ctx, "commit", now, &st.id, vec![st.clone()], None, serde_json::json!({"line": line.id}))
    });
    if let Err(e) = moved {
        repo.discard_states(std::slice::from_ref(&st));
        return Err(e);
    }
    if !quiet {
        outln!(ctx, "[{} {}] {}", line.name, ids::short(&st.id), message.lines().next().unwrap_or(""));
        let new_root = repo.state_src(&st.id)?;
        for c in changes::with_renames(&t.head, &new_root, list) {
            let code = match (&c.old, &c.new, &c.old_path) {
                (_, _, Some(o)) => format!("rename {o} =>"),
                (None, _, _) => "create".into(),
                (_, None, _) => "delete".into(),
                _ => "modify".into(),
            };
            outln!(ctx, " {code} {}", c.path);
        }
    }
    Ok(0)
}

/// Delete a virtual line: unsaved changes become its final automatic state, as for a folder.
pub fn delete(ctx: &Ctx, repo: &Repo, l: &Line, v: &View) -> Result<()> {
    let _line_lock = repo.line_lock(l)?;
    let l = repo.line(&l.id)?;
    let at = repo.working(&l);
    let last = if v.mounted || upper(&repo.p, &l.id).exists() {
        let t = trees(repo, &l, v)?;
        let list = file_changes(&t, v)?;
        if list.is_empty() {
            None
        } else {
            Some(write_state(ctx, repo, &l, &t, &list, StateKind::Auto, "final state of a deleted virtual line")?)
        }
    } else {
        None
    };
    if is_mounted(&at) {
        if let Err(e) = unmount(&at) {
            if let Some(st) = &last {
                repo.discard_states(std::slice::from_ref(st));
            }
            return Err(e);
        }
    }
    let _lock = repo.p.lock()?;
    let fresh = repo.line(&l.id)?;
    let last_id = match last {
        Some(st) => {
            let mut rec = Record::new("save");
            let before = repo.recorded_working(&fresh)?;
            rec.data =
                serde_json::json!({"line": fresh.id, "head": fresh.head, "working_before": before, "working_after": st.id});
            rec.states.push(st.clone());
            repo.commit_record(ctx, rec)?;
            Some(st.id)
        }
        None => None,
    };
    crate::cmd::branch::delete_recorded(ctx, repo, &fresh, last_id, at.exists())?;
    repo.p.trash(&dir(&repo.p, &fresh.id))?;
    Ok(())
}

/// Refuse a command that does not support virtual lines yet, when it runs on one.
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
    ];
    if SUPPORTED.contains(&cmd) {
        return Ok(());
    }
    let Ok(loc) = ctx.locate() else { return Ok(()) };
    let Some(name) = loc.line else { return Ok(()) };
    let Ok(pdir) = ctx.store.project_dir(&loc.project) else { return Ok(()) };
    if all_in(&pdir.join("meta")).iter().any(|v| v.name == name) {
        return Err(exit(
            1,
            format!(
                "layr {cmd} does not support virtual lines yet ({ENV}); convert the line first: layr line show {name} --folder"
            ),
        ));
    }
    Ok(())
}

/// The view of `line` when it is virtual, for commands that branch on it.
pub fn of(repo: &Repo, line: &Line) -> Option<View> {
    of_line(&repo.p, &line.id)
}

/// `layr line show <line> --folder [<path>] | --none | --virtual`.
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
    let l = repo.line(&a.pos[0])?;
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
    let Some(mut v) = of_line(&repo.p, &l.id) else {
        return Err(exit(1, format!("line {} has a folder; only virtual lines change their view for now", l.name)));
    };
    if h.line.as_ref().is_some_and(|c| c.id == l.id) || ctx.cwd.starts_with(repo.working(&l)) {
        return Err(exit(1, "run this command outside the line's folder: its mount changes"));
    }
    let _line_lock = repo.line_lock(&l)?;
    let l = repo.line(&l.id)?;
    let at = repo.working(&l);
    if a.has("--none") {
        if is_mounted(&at) {
            unmount(&at)?;
        }
        let _ = std::fs::remove_dir(&at);
        v.mounted = false;
        write_view(&repo.p, &v)?;
        outln!(ctx, "Line {} has no folder now; its changes stay in {}", l.name, upper(&repo.p, &l.id).display());
        return Ok(0);
    }
    if a.has("--virtual") {
        if !is_mounted(&at) {
            if at.symlink_metadata().is_err() {
                std::fs::create_dir(&at)?;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
            }
            mount(&dir(&repo.p, &l.id), &at)?;
        }
        v.mounted = true;
        write_view(&repo.p, &v)?;
        outln!(ctx, "Line {} is virtual at {}", l.name, at.display());
        return Ok(0);
    }
    // --folder: a native folder with the merged content, then the view goes.
    let target = match a.pos.get(1) {
        Some(p) => {
            let p = ctx.cwd.join(p);
            if p.symlink_metadata().is_ok() {
                bail!("{} already exists", p.display());
            }
            Some(p)
        }
        None => None,
    };
    let t = trees(repo, &l, &v)?;
    strip_quarantine(&t.upper, &{
        let mut all = BTreeSet::new();
        fsutil::walk(t.upper.as_path(), "", &mut |rel, _| {
            all.insert(rel.to_string());
            Ok(true)
        })?;
        all
    });
    let tmp = repo.p.work_dir().join(crate::store::temp_name());
    std::fs::create_dir_all(repo.p.work_dir())?;
    let made = (|| -> Result<()> {
        t.merged.materialize(&tmp, false)?;
        for name in SYSTEM_ROOT_NAMES {
            let p = tmp.join(name);
            if p.symlink_metadata().is_ok() {
                crate::storage::delete_tree(&p)?;
            }
        }
        repo.prepare_at(&l, &l.head, &tmp, &[])
    })();
    if let Err(e) = made {
        let _ = crate::storage::delete_tree(&tmp);
        return Err(e);
    }
    if is_mounted(&at) {
        if let Err(e) = unmount(&at) {
            let _ = crate::storage::delete_tree(&tmp);
            return Err(e);
        }
    }
    let _ = std::fs::remove_dir(&at);
    let dst = target.clone().unwrap_or_else(|| at.clone());
    crate::fsutil::rename_tree(&tmp, &dst)?;
    if let Some(p) = &target {
        let _lock = repo.p.lock()?;
        repo.p.set_moved_path(&l.id, Some(p))?;
    }
    repo.p.trash(&dir(&repo.p, &l.id))?;
    outln!(ctx, "Line {} has a folder at {}", l.name, dst.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topmost_keeps_only_the_highest_removed_paths() {
        let set: BTreeSet<String> = ["a", "a/b", "a/b/c", "ab", "d/e", "d/e/f", "g/h"].iter().map(|s| s.to_string()).collect();
        let want: BTreeSet<String> = ["a", "ab", "d/e", "g/h"].iter().map(|s| s.to_string()).collect();
        assert_eq!(topmost(set), want);
    }

    #[test]
    fn system_folders_are_only_root_names() {
        assert!(system_path(".fseventsd"));
        assert!(system_path(".fseventsd/fseventsd-uuid"));
        assert!(!system_path("src/.fseventsd"));
        assert!(!system_path("notes.txt"));
    }
}
