//! Experimental virtual lines (macOS, `LAYR_EXPERIMENTAL_VIEWS=1`, docs/views.md).
//!
//! The working folder of a virtual line is a LayrFS mount (tools/layrfs). The mount merges an
//! upper folder over the complete tree of the state the line started from (the lower tree, which
//! never changes). The upper folder is a thin layer (docs/apfs-thin-states.md): changed files,
//! new files, and the removed paths in its `com.layr.removed` attribute. Status, diff and save
//! read the upper folder and the head; they never scan the merged tree.
//!
//! The kind of view is local to this machine: `meta/vlines/<line id>/view.json`, never a record.
use crate::changes::{self, EntryChange};
use crate::ctx::Ctx;
use crate::errors::{error, Kind as ErrorKind};
use crate::fsutil::{self, Meta};
use crate::ids;
use crate::model::{Line, LineChange, StateKind, StateRec};
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
pub(crate) const SYSTEM_ROOT_NAMES: &[&str] =
    &[".fseventsd", ".Spotlight-V100", ".Trashes", ".TemporaryItems", ".DocumentRevisions-V100"];

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

pub(crate) fn require_enabled(ctx: &Ctx) -> Result<()> {
    if !enabled(ctx) {
        return Err(error(ErrorKind::Unsupported, format!("virtual lines are experimental: set {ENV}=1 (docs/views.md)")));
    }
    Ok(())
}

fn views_dir(meta: &Path) -> PathBuf {
    meta.join("vlines")
}

pub(crate) fn dir(p: &Project, line_id: &str) -> PathBuf {
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

pub(crate) fn write_view(p: &Project, v: &View) -> Result<()> {
    fsutil::atomic_write(&dir(p, &v.line).join("view.json"), &serde_json::to_vec_pretty(v)?, 0o600)
}

pub fn is_mounted(path: &Path) -> bool {
    crate::sys::fs_type_name(path).map(|t| t == FS_TYPE).unwrap_or(false)
}

pub(crate) fn mount(vdir: &Path, at: &Path) -> Result<()> {
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

pub(crate) fn unmount(at: &Path) -> Result<()> {
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

/// Serialize mount lifecycle changes with commands that need a working mount.
pub(crate) fn lifecycle_lock(repo: &Repo, line: &Line) -> Result<crate::store::LineLeaseSet> {
    repo.p.line_lock(&format!("view-{}", line.id), &line.name)
}

/// A hidden line gets a temporary mount for an in-place update. Its saved view stays hidden.
/// The lifecycle lock prevents show or delete from unmounting it during the command.
pub fn with_working<T>(ctx: &Ctx, repo: &Repo, line: &Line, run: impl FnOnce() -> Result<T>) -> Result<T> {
    if of(repo, line).is_none() {
        return run();
    }
    let _lifecycle = lifecycle_lock(repo, line)?;
    let hidden = {
        let _line = repo.line_lock(line)?;
        let view = of(repo, line).context("virtual line changed before pull")?;
        let at = repo.working(line);
        if !is_mounted(&at) {
            repo.authorize_line(ctx, line, crate::repo::LineOp::Manage)?;
            match std::fs::symlink_metadata(&at) {
                Ok(meta) => anyhow::ensure!(
                    meta.is_dir() && std::fs::read_dir(&at)?.next().is_none(),
                    "the virtual line mount path is not an empty directory: {}",
                    at.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&at)?;
                    std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o700))?;
                }
                Err(error) => return Err(error.into()),
            }
            mount(&dir(&repo.p, &line.id), &at)?;
        }
        !view.mounted
    };
    let result = run();
    if hidden {
        let _line = repo.line_lock(line)?;
        let at = repo.working(line);
        unmount(&at).context("the command finished, but its temporary view could not be hidden")?;
        std::fs::remove_dir(&at)?;
    }
    result
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
    // A folder that `line rename` kept for another line counts as taken, also when it is gone.
    if at.symlink_metadata().is_ok() || repo.p.kept_folder_line(name).is_some() {
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
        let _lock = repo.p.lock_maintenance()?;
        if repo.p.db.line_by_name(name)?.is_some() {
            bail!("a line named '{name}' already exists");
        }
        repo.source_state(start)?;
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
pub(crate) fn strip_quarantine(up: &Path, paths: &BTreeSet<String>) {
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

pub(crate) fn trees(repo: &Repo, line: &Line, v: &View) -> Result<Trees> {
    let up = upper(&repo.p, &line.id);
    let merged = Layered::over(&up, Layered::complete(&v.lower))?;
    Ok(Trees { upper: up, merged, head: repo.state_src(&line.head)?.into_layered() })
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
    for l in &head.layers()[..head.layers().len() - 1] {
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
pub(crate) fn file_changes(t: &Trees, v: &View) -> Result<Vec<EntryChange>> {
    let rules = crate::ignore_rules::Rules::of(&crate::source::Source::from(t.merged.clone()));
    let list = match candidates(t, v, &rules)? {
        Some(set) => {
            strip_quarantine(&t.upper, &set);
            let raw: Vec<EntryChange> = set
                .into_iter()
                .filter_map(|p| {
                    let (old, new) = (fsutil::lmeta(&t.head, &p), fsutil::lmeta(&t.merged, &p));
                    EntryChange::observed(p, old, new, crate::compare::DataEvidence::Unknown)
                })
                .collect();
            changes::file_changes_of(&t.head, &t.merged, raw)?
        }
        None => {
            // Object heads use another root path, so the native-layer shortcut cannot apply.
            // Normalize the extension's marker on upper entries before the full comparison too.
            let mut paths = BTreeSet::new();
            fsutil::walk(t.upper.as_path(), "", &mut |path, meta| {
                if skip_path(path) || system_path(path) {
                    return Ok(false);
                }
                if meta.is_dir() && rules.ignored(path, true) && fsutil::lmeta(&t.head, path).is_none() {
                    return Ok(false);
                }
                paths.insert(path.to_string());
                Ok(true)
            })?;
            strip_quarantine(&t.upper, &paths);
            changes::file_changes(&t.head, &t.merged)?
        }
    };
    Ok(list
        .into_iter()
        .filter(|c| !skip_path(c.path()) && !system_path(c.path()))
        .filter(|c| c.old().is_some() || !rules.ignored(c.path(), false))
        .collect())
}

/// `layr status` of a virtual line: every change is saved by the next `save -m`, so all are staged.
pub fn status(repo: &Repo, line: &Line, v: &View) -> Result<Status> {
    let t = trees(repo, line, v)?;
    let staged = changes::with_renames(&t.head, &t.merged, file_changes(&t, v)?);
    let mut conflicts = Vec::new();
    for conflict in repo.state(&line.head)?.conflicts {
        if crate::repo::conflict_open(&t.merged, &conflict)? {
            conflicts.push(conflict);
        }
    }
    Ok(Status { staged, conflicts, ..Default::default() })
}

/// Merge and replay treat unsaved view changes as local edits, not an explicit index.
pub fn working_status(repo: &Repo, line: &Line, v: &View) -> Result<Status> {
    let mut result = status(repo, line, v)?;
    result.unstaged = std::mem::take(&mut result.staged);
    Ok(result)
}

/// Record working content for a command without cloning the FSKit mount.
pub fn working_snapshot(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    v: &View,
    parents: Vec<String>,
    message: &str,
    base: &str,
    id: String,
) -> Result<StateRec> {
    let mut prior = line.clone();
    prior.head = base.to_string();
    let t = trees(repo, &prior, v)?;
    let list = file_changes(&t, v)?;
    let mut state = write_state_as(ctx, repo, &prior, &t, &list, StateKind::Auto, message, id)?;
    state.parents = parents;
    Ok(state)
}

/// The trees and changes for `layr diff` of a virtual line against its head.
pub fn diff_input(
    repo: &Repo,
    line: &Line,
    v: &View,
    _keep: &mut Vec<crate::repo::Temp>,
) -> Result<(crate::repo::Snap, crate::repo::Snap, Vec<EntryChange>)> {
    let t = trees(repo, line, v)?;
    let list = file_changes(&t, v)?;
    Ok((t.head.into(), t.merged.into(), list))
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
pub(crate) fn write_state(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    t: &Trees,
    list: &[EntryChange],
    kind: StateKind,
    message: &str,
) -> Result<StateRec> {
    write_state_as(ctx, repo, line, t, list, kind, message, ids::new_id())
}

#[allow(clippy::too_many_arguments)]
fn write_state_as(
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    t: &Trees,
    list: &[EntryChange],
    kind: StateKind,
    message: &str,
    id: String,
) -> Result<StateRec> {
    let _s = crate::trace::span("view write state");
    let is_dir = |m: &Option<Meta>| m.as_ref().is_some_and(|m| m.is_dir());
    let mut d = thin::Diff::default();
    let mut gone = BTreeSet::new();
    for c in list {
        if c.new().is_some() {
            d.write.insert(c.path().clone());
        }
        if c.old().is_some() && (c.new().is_none() || c.old().map(|m| m.ftype) != c.new().map(|m| m.ftype)) {
            gone.insert(c.path().clone());
        }
        if let Some(o) = c.old_path() {
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
    let rules = crate::ignore_rules::Rules::of(&crate::source::Source::from(t.merged.clone()));
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
    let dst = repo.p.delta_path(&id);
    // A reverted entry can live only in the lower tree. Hard links also need a complete
    // snapshot so their identity survives. Ordinary edits still write only the upper layer.
    let missing_upper = d.write.iter().any(|p| std::fs::symlink_metadata(t.upper.join(p)).is_err());
    let complete = if missing_upper || thin::any_linked(&t.upper, &d.write) {
        true
    } else {
        match thin::write_layer(&t.upper, &dst, &d, &line.head, false, None) {
            Ok(()) => false,
            Err(e) if thin::needs_complete(&e) => true,
            Err(e) => return Err(e),
        }
    };
    if complete {
        let path = repo.p.state_path(&id);
        let made = (|| -> Result<()> {
            t.merged.materialize(&path, false)?;
            // A complete fallback obeys the same exclusions as the thin write. In particular,
            // ignored build output and untracked secrets must not enter a named state.
            let mut excluded = Vec::new();
            fsutil::walk(path.as_path(), "", &mut |rel, meta| {
                if skip_path(rel)
                    || system_path(rel)
                    || (rules.ignored(rel, meta.is_dir()) && fsutil::lmeta(&t.head, rel).is_none())
                {
                    excluded.push(rel.to_string());
                    return Ok(false);
                }
                Ok(true)
            })?;
            for rel in excluded {
                crate::fs::remove(&path.join(rel))?;
            }
            crate::apfs::write_info(
                &path,
                &crate::apfs::SubvolInfo {
                    uuid: ids::new_id(),
                    received_uuid: None,
                    parent_uuid: None,
                    readonly: true,
                    base: None,
                },
            )?;
            crate::apfs::seal(&path, true, false)
        })();
        if let Err(error) = made {
            let _ = repo.p.delete_state(&id);
            return Err(error);
        }
    }
    let described = (|| -> Result<StateRec> {
        if !complete {
            repo.p.lock_or_borrow()?.write(|tx| tx.record_base(&id, &line.head))?;
        }
        repo.describe_state(ctx, id.clone(), kind, vec![line.head.clone()], message, Some(line))
    })();
    if described.is_err() {
        let _ = repo.p.delete_state(&id);
    }
    described
}

/// Save the working content without moving the named head. The caller holds the project
/// lock, as for a folder line. Compare against the last automatic state to avoid duplicates.
pub fn save_working(
    locked: &crate::store::LockedProject<'_>,
    ctx: &Ctx,
    repo: &Repo,
    line: &Line,
    v: &View,
    message: &str,
) -> Result<Option<StateRec>> {
    let prev = repo.save_base(line)?;
    let mut previous = line.clone();
    previous.head = prev;
    let prior = trees(repo, &previous, v)?;
    if file_changes(&prior, v)?.is_empty() {
        return Ok(None);
    }
    let t = trees(repo, line, v)?;
    let list = file_changes(&t, v)?;
    crate::secrets::check_changes(ctx, repo, &t.head.clone().into(), &t.merged.clone().into(), &line.head, &list)?;
    let st = write_state(ctx, repo, line, &t, &list, StateKind::Auto, message)?;
    let mut rec = crate::model::DraftRecord::new("save");
    rec.data = serde_json::json!({"line": line.id, "head": line.head,
        "working_before": repo.recorded_working(line)?, "working_after": st.id});
    rec.states.push(st.clone());
    if let Err(error) = repo.commit_record(locked, ctx, rec) {
        if matches!(error, crate::db::CommitResolution::NotCommitted(_)) {
            repo.discard_states(std::slice::from_ref(&st));
        }
        return Err(error.into());
    }
    Ok(Some(st))
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
        "git",
    ];
    if SUPPORTED.contains(&cmd) {
        return Ok(());
    }
    let Ok(loc) = ctx.locate() else { return Ok(()) };
    let Some(name) = loc.line else { return Ok(()) };
    let Ok(pdir) = ctx.store.project_dir(&loc.project) else { return Ok(()) };
    if all_in(&pdir.join("meta")).iter().any(|v| v.name == name) {
        return Err(error(
            ErrorKind::Failed,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_head_does_not_report_the_fskit_quarantine_marker_as_a_change() -> Result<()> {
        let tmp = std::env::temp_dir().join(format!("layr-view-quarantine-{}", crate::ids::new_id()));
        let lower = tmp.join("lower");
        let upper = tmp.join("upper");
        std::fs::create_dir_all(lower.join("added"))?;
        std::fs::create_dir_all(upper.join("added"))?;
        let store = layr_objects::Store::open(&tmp.join("objects"), layr_objects::Keying::Plain)?;
        let written = layr_objects::write_tree(
            &store,
            &lower,
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        let head = crate::object_tree::ObjectTree::new(
            store,
            layr_objects::StateRef { tree: written.tree.unwrap(), times: written.times.unwrap() },
            &tmp.join("reads"),
        )?;
        crate::sys::apfs_attribute(&upper.join("added"), c"com.apple.quarantine", Some(b"0086;00000000;LayrFS;"))?;
        let trees = Trees {
            upper: upper.clone(),
            head: Layered::objects(head),
            merged: Layered::over(&upper, Layered::complete(&lower))?,
        };
        let view = View { line: crate::ids::new_id(), name: "virtual".into(), base: crate::ids::new_id(), lower, mounted: false };
        let changes = file_changes(&trees, &view)?;
        assert!(changes.is_empty(), "unexpected changes: {changes:?}");
        crate::sys::apfs_attribute(&upper.join("added"), c"com.apple.quarantine", Some(b"0086;00000000;OtherAgent;"))?;
        let real_changes = file_changes(&trees, &view)?;
        assert_eq!(real_changes.len(), 1);
        assert_eq!(real_changes[0].path(), "added");
        drop(trees);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

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
