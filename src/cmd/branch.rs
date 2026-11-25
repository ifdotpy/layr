//! Lines, adoption, removed folder-switch commands, and tags.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::{Caller, Ctx};
use crate::ids;
use crate::model::{Line, LineChange, StateKind};
use crate::repo::Repo;
use crate::revs;
use anyhow::{Context, Result};

const DELETE_USAGE: &str = "usage: layr line delete <name>... [-D] [--discard | -f]\nChecks saves against the current line, or main when there is no current line.\nThe refusal names that destination; comparing a line with itself is refused.\n-D skips the merge check; --discard allows unsaved changes; -f does both.";

fn owner_arg(ctx: &Ctx, v: Option<&str>) -> Result<u32> {
    match v {
        None => Ok(ctx.caller.uid),
        Some(s) => {
            if let Ok(n) = s.parse::<u32>() {
                return Ok(n);
            }
            match crate::sys::user_by_name(s) {
                Some(u) => Ok(u.uid),
                None => {
                    return Err(exit(128, format!("fatal: unknown user '{s}'")));
                }
            }
        }
    }
}

fn line_json(repo: &crate::repo::Repo, line: &crate::model::Line) -> serde_json::Value {
    serde_json::json!({"id": line.id, "name": line.name, "head": line.head, "owner": line.owner,
        "machine": line.machine, "path": repo.working(line)})
}

pub fn line(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let (sub, rest) = args.split_first().ok_or_else(|| exit(2, "usage: layr line new/list/delete/rename/move"))?;
    if sub == "move" {
        return line_move(ctx, rest);
    }
    if sub == "show" {
        return crate::cmd::view_ops::show(ctx, rest);
    }
    if sub == "target" {
        return Err(exit(
            2,
            "layr line target was removed: a named save writes to the overlay that the folder shows. Use layr overlay use <overlay>, or layr save -m <message> --to <ancestor>",
        ));
    }
    if !matches!(sub.as_str(), "new" | "list" | "delete" | "rename") {
        return Err(exit(2, "usage: layr line new/list/delete/rename/move/show"));
    }
    line_command(ctx, rest, sub)
}

/// The checked absolute target of `layr line move <line> <path>`.
pub(crate) fn move_target(ctx: &Ctx, repo: &crate::repo::Repo, from: &std::path::Path, arg: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, PathBuf};
    let fail = |m: String| exit(1, format!("error: {m}"));
    let raw = match arg.strip_prefix('~') {
        Some("") => ctx.caller.home.clone(),
        Some(rest) if rest.starts_with('/') => ctx.caller.home.join(&rest[1..]),
        _ => PathBuf::from(arg),
    };
    let raw = if raw.is_absolute() { raw } else { ctx.cwd.join(raw) };
    let mut clean = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => {}
            c => clean.push(c),
        }
    }
    if clean.symlink_metadata().is_ok() {
        return Err(fail(format!("{} already exists", clean.display())));
    }
    let (Some(parent), Some(name)) = (clean.parent(), clean.file_name()) else {
        return Err(fail(format!("{} has no parent folder", clean.display())));
    };
    let parent = parent.canonicalize().map_err(|_| fail(format!("the parent folder {} does not exist", parent.display())))?;
    if !parent.is_dir() {
        return Err(fail(format!("{} is not a folder", parent.display())));
    }
    use rustix::fs::Access;
    if rustix::fs::access(&parent, Access::WRITE_OK | Access::EXEC_OK).is_err() {
        return Err(fail(format!("you cannot write in {}", parent.display())));
    }
    let to = parent.join(name);
    let root = ctx.store.root.canonicalize().unwrap_or_else(|_| ctx.store.root.clone());
    if to.starts_with(&root) {
        return Err(fail(format!(
            "{} is inside the store {}; use 'layr line move <line>' to move a line back",
            to.display(),
            root.display()
        )));
    }
    if to.starts_with(from) {
        return Err(fail("the target is inside the folder of the line".into()));
    }
    let store_dev = std::fs::metadata(repo.p.lines_dir())?.dev();
    let same_device = std::fs::metadata(&parent)?.dev() == store_dev;
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    let same_device = if same_device {
        true
    } else {
        // Btrfs gives each subvolume a distinct device number.
        use std::os::fd::AsFd;
        let source = std::fs::File::open(repo.p.lines_dir())?;
        let target = std::fs::File::open(&parent)?;
        match (crate::sys::filesystem_uuid(source.as_fd()), crate::sys::filesystem_uuid(target.as_fd())) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    };
    if !same_device {
        return Err(fail(format!(
            "{} is on another file system than the store; a line can only move within one file system",
            parent.display()
        )));
    }
    if crate::linepaths::index(&ctx.store.root).iter().any(|e| e.path.starts_with(&to) || to.starts_with(&e.path)) {
        return Err(fail(format!("{} overlaps the folder of another moved line", to.display())));
    }
    Ok(to)
}

/// `layr line move <line> [<path>]`: keep the working folder of a line at `<path>` (outside the
/// store, same file system), or back in the store without a path. Only this machine knows the path.
fn line_move(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new(), args)?;
    if !(1..=2).contains(&a.pos.len()) {
        return Err(exit(2, "usage: layr line move <line> [<path>]"));
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    let l = repo.line(&a.pos[0])?;
    if crate::views::of(repo, &l).is_some() {
        return Err(exit(
            1,
            format!("line {} is virtual: use 'layr line show {} --folder <path>' (docs/views.md)", l.name, l.name),
        ));
    }
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
    if h.line.as_ref().is_some_and(|c| c.id == l.id) {
        return Err(exit(
            1,
            format!("error: cannot move line '{}' used as the current folder; run the command from another folder", l.name),
        ));
    }
    let _line_lock = repo.line_lock(&l)?;
    let _lock = repo.p.lock_maintenance()?;
    let from = repo.working(&l);
    if !from.is_dir() {
        return Err(exit(1, format!("error: line '{}' has no working folder on this machine", l.name)));
    }
    let moved = repo.p.moved_path(&l.id).is_some();
    let to = match a.pos.get(1) {
        Some(arg) => move_target(ctx, repo, &from, arg)?,
        None if moved => {
            let d = repo.p.default_line_path(&l.name);
            if d.symlink_metadata().is_ok() {
                return Err(exit(1, format!("error: {} already exists", d.display())));
            }
            d
        }
        None => return Err(exit(1, format!("error: line '{}' is already in the store: {}", l.name, from.display()))),
    };
    repo.validate_cache_locations(&l, &from, &to, crate::cache_overlay::CacheAccess::Live)?;
    crate::linepaths::register_with(&ctx.store.root, Some(&to));
    let back = a.pos.len() == 1;
    // A fast line's folder is a mount: it is mounted again at the new folder.
    if crate::views::fast_of(repo, &l).is_some() {
        let done = crate::views::fast_move(repo, &l, &from, &to)
            .and_then(|_| repo.p.set_moved_path(&l.id, if back { None } else { Some(&to) }));
        crate::linepaths::register(&ctx.store.root);
        done?;
        // As for a folder line: the caches that the check before the move left for later.
        let l = repo.line(&l.id)?;
        repo.validate_cache_locations(&l, &to, &to, crate::cache_overlay::CacheAccess::Live)?;
        outln!(ctx, "Moved line {} to {}", l.name, to.display());
        return Ok(0);
    }
    let done = crate::fsutil::rename_tree(&from, &to).map_err(anyhow::Error::from).and_then(|_| {
        let r = repo.p.set_moved_path(&l.id, if back { None } else { Some(&to) });
        if r.is_err() {
            let _ = crate::fsutil::rename_tree(&to, &from);
        }
        r
    });
    crate::linepaths::register(&ctx.store.root);
    done?;
    repo.validate_cache_locations(&l, &to, &to, crate::cache_overlay::CacheAccess::Live)?;
    outln!(ctx, "Moved line {} to {}", l.name, to.display());
    Ok(0)
}

/// The paths with unsaved changes in the folder of a line: modified, added, deleted and new files
/// that are not ignored. Files of quiet overlays (caches) do not count.
pub(crate) fn unsaved_paths(repo: &crate::repo::Repo, l: &Line) -> Result<Vec<String>> {
    let (mut st, _scan, _it, _idx) = repo.status(l)?;
    st.drop_paths(&repo.quiet_paths(l)?);
    let mut out = std::collections::BTreeSet::new();
    for c in st.staged.iter().chain(st.unstaged.iter()) {
        out.insert(c.path().clone());
        if let Some(o) = c.old_path() {
            out.insert(o.clone());
        }
    }
    out.extend(st.untracked.iter().cloned());
    Ok(out.into_iter().collect())
}

/// Read the destination again, including this machine's head, when checking a deletion.
fn deletion_target(repo: &Repo, current: Option<&Line>) -> Result<Line> {
    match current {
        Some(line) => repo.line(&line.id),
        None => repo
            .line("main")
            .context("cannot check mergedness without a current line: default deletion target 'main' is unavailable"),
    }
}

fn check_deletion_merged(repo: &Repo, line: &Line, current: Option<&Line>) -> Result<()> {
    // A line with no named saves has always been deletable without a destination.
    if repo.p.db.line_states(&line.id, StateKind::Commit)?.is_empty() {
        return Ok(());
    }
    let target = deletion_target(repo, current)?;
    if target.id == line.id {
        return Err(exit(
            1,
            format!("error: cannot delete line '{}': the deletion target '{}' is the same line", line.name, target.name),
        ));
    }
    if !repo.is_ancestor(&line.head, &target.head)? {
        return Err(exit(1, format!("error: the line '{}' is not fully merged into '{}'.\nIf you are sure you want to delete it, run 'layr line delete -D {}'.", line.name, target.name, line.name)));
    }
    Ok(())
}

/// Test barrier after final-state preparation and before the deletion lock (t256, t257).
pub(crate) fn deletion_barrier() -> Result<()> {
    if let Some(root) = std::env::var_os("LAYR_TEST_LINE_DELETE_BARRIER") {
        let root = std::path::PathBuf::from(root);
        std::fs::write(root.join("ready"), b"ready")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !root.join("release").exists() {
            anyhow::ensure!(std::time::Instant::now() < deadline, "line delete test barrier timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Ok(())
}

/// Prepare a folder or virtual line's final save, then recheck deletion before publication.
/// The owned preparation drops unpublished work on every refusal or retry.
pub(crate) fn prepare_deletion<'p>(
    ctx: &Ctx,
    repo: &'p Repo<'p>,
    l: &Line,
    line_lock: crate::store::LineLeaseSet,
    made: bool,
    discard: bool,
    check: impl Fn() -> Result<Line>,
) -> Result<(crate::store::LockedProject<'p>, Line, Option<String>, Option<String>)> {
    let name = &l.name;
    let mut attempt = 0;
    let mut initial_lease = Some(line_lock);
    loop {
        attempt += 1;
        let lease = match initial_lease.take() {
            Some(lease) => lease,
            None => repo.line_lock(l)?,
        };
        let fresh = repo.line(&l.id)?;
        if !repo.setup_save(ctx, &fresh, true)? {
            if attempt < 3 {
                continue;
            }
            return Err(exit(1, "cache setup is busy; retry line deletion"));
        }
        let fresh = repo.line(&l.id)?;
        // Automatic states leave overlay files out: uncommitted changes of them would be lost.
        if !discard && repo.is_stacked(&fresh)? && repo.working(&fresh).exists() {
            let private = repo.private_paths(&fresh)?;
            let (st, _scan, _it, _idx) = repo.status(&fresh)?;
            let changed = st
                .staged
                .iter()
                .chain(st.unstaged.iter())
                .flat_map(|c| [Some(c.path().clone()), c.old_path().cloned()])
                .flatten()
                .any(|p| private.contains(&p));
            let any = st.staged.len() + st.unstaged.len() + st.untracked.len() > 0;
            if changed || !repo.pending_moves(&fresh).is_empty() || (any && repo.private_view(&fresh)?) {
                return Err(exit(
                    1,
                    format!("error: line '{name}' has unsaved changes of overlay files, which automatic states do not keep; save them, or delete the line with --discard"),
                ));
            }
        }
        let prepared = match repo.prepare_final_state(ctx, &fresh, &lease) {
            Err(error) if error.is::<crate::repo::StalePreparation>() && attempt < 3 => continue,
            // Content that cannot be read (#452, #453) makes no final save: with --discard the line
            // goes without one. Any other failure keeps the line, so its work stays undoable.
            Err(error) if discard && layr_objects::store::is_unreadable(error.as_ref()) => {
                eprintln!("warning: line {name} goes without a final save: {error:#}");
                drop(lease);
                deletion_barrier()?;
                let lock = repo.line_lock(l)?.lock(&repo.p)?;
                return Ok((lock, check()?, None, None));
            }
            result => result?,
        };
        // Preparation owns its inputs; management changes may invalidate the target
        // before publication. Reacquire the line lease before the final project lock.
        drop(lease);
        // Test hook (tests/t123-line-delete-save.sh): wait after the prepare step, so a
        // test can write into the folder before the lock.
        if let Some(ms) = std::env::var("LAYR_TEST_LINE_DELETE_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let staged = prepared.staged_id();
        deletion_barrier()?;
        let lease = repo.line_lock(l)?;
        let lock = lease.lock(&repo.p)?;
        let fresh = check()?;
        match repo.record_final_state(ctx, &lock, prepared)? {
            crate::repo::FinalState::Recorded(last) => return Ok((lock, fresh, if made { last } else { None }, staged)),
            crate::repo::FinalState::Moved if attempt < 3 => drop(lock),
            crate::repo::FinalState::Moved => {
                return Err(exit(
                    1,
                    format!(
                        "line {name} moved during the command (head {} -> {}); run it again",
                        ids::short(&l.head),
                        ids::short(&fresh.head)
                    ),
                ))
            }
        }
    }
}

fn line_command(ctx: &Ctx, args: &[String], sub: &str) -> Result<i32> {
    let spec = Spec::new()
        .flag("-l|--list")
        .flag("-v|--verbose")
        .flag("-a|--all")
        .flag("-r|--remotes")
        .flag("-d|--delete")
        .flag("-D")
        .flag("-m|--move")
        .flag("-M")
        .flag("-f|--force")
        .flag("--discard")
        .flag("--show-current")
        .flag("--path")
        .value("--id")
        .value("--owner")
        .value("--count")
        .value("--view")
        .flag("--fast")
        .flag("--classic")
        .flag("--json")
        .flag("-q|--quiet");
    let mut a = parse(&spec, args)?;
    let delete = a.has("-d") || a.has("-D");
    let rename = a.has("-m") || a.has("-M");
    let list = a.has("-l") || a.has("--show-current");
    if (delete && sub != "delete") || (rename && sub != "rename") || (list && sub != "list") {
        return Err(exit(2, "Use 'layr line new', 'layr line list', 'layr line delete', or 'layr line rename'."));
    }
    match sub {
        "new" if !(1..=2).contains(&a.pos.len()) => {
            return Err(exit(2, "usage: layr line new <name> [<rev>] [--count N]"));
        }
        "list" => {
            a.flags.insert("-l".into(), 1);
        }
        "delete" if !a.has("-D") => {
            a.flags.insert("-d".into(), 1);
        }
        "rename" if !a.has("-M") => {
            a.flags.insert("-m".into(), 1);
        }
        _ => {}
    }
    if a.has("--count")
        && (!(1..=2).contains(&a.pos.len())
            || a.has("-d")
            || a.has("-D")
            || a.has("-m")
            || a.has("-M")
            || a.has("-l")
            || a.has("--show-current"))
    {
        return Err(exit(128, "usage: layr line new <prefix> [<rev>] --count <N>"));
    }
    let exact_id = a.get("--id").map(layr_types::LineId::parse).transpose()?;
    if exact_id.is_some() && (sub != "delete" || !a.pos.is_empty()) {
        return Err(exit(2, "usage: layr line delete --id <line-id> [-D] [--discard] [--json]"));
    }
    let h = here(ctx)?;
    let repo = &h.repo;
    let current = h.line.clone();
    if a.has("--show-current") {
        if let Some(l) = &current {
            outln!(ctx, "{}", l.name);
        }
        return Ok(0);
    }
    if a.has("-d") || a.has("-D") {
        if a.pos.is_empty() && exact_id.is_none() {
            return Err(exit(128, format!("fatal: line name or --id required\n{DELETE_USAGE}")));
        }
        let discard = a.has("--discard") || a.has("-f");
        // Checked again under the lock with fresh heads (t256): the current line, or main without one.
        let check_merged = |line: &Line| -> Result<()> {
            if a.has("-d") && !a.has("-f") {
                check_deletion_merged(repo, line, current.as_ref())?;
            }
            Ok(())
        };
        let selectors = if let Some(id) = exact_id { vec![id.to_string()] } else { a.pos.clone() };
        for selector in &selectors {
            let (l, target) = {
                let _lock = repo.p.lock_maintenance()?;
                let l = match exact_id {
                    Some(id) => crate::lineops::live_line(repo, id)?,
                    None => repo.line(selector)?,
                };
                repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
                let target = crate::lineops::DeleteTarget::capture(repo, &l)?;
                (l, target)
            };
            let name = &l.name;
            if current.as_ref().map(|c| c.id == l.id).unwrap_or(false) {
                return Err(exit(1, format!("error: cannot delete line '{name}' used as the current folder")));
            }
            #[cfg(target_os = "macos")]
            let _lifecycle = crate::views::of(repo, &l).map(|_| crate::views::lifecycle_lock(repo, &l)).transpose()?;
            let line_lock = repo.line_lock(&l)?;
            let l = repo.line(&l.id)?;
            repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
            #[cfg(target_os = "macos")]
            let view = crate::views::of(repo, &l);
            check_merged(&l)?;
            // A fast line whose server is gone (a restart of the service) shows an empty folder:
            // mount it again, so the check below reads its files.
            #[cfg(target_os = "linux")]
            if !discard && crate::views::fast_of(repo, &l).is_some() {
                crate::views::ensure_fast_mounted(repo, &l)?;
            }
            // Unsaved changes in the folder of the line need an explicit --discard (or -f): -D
            // allows only unmerged saves.
            if !discard && repo.working(&l).exists() {
                let flags = if a.has("-D") { "-D --discard" } else { "--discard" };
                let unsaved = unsaved_paths(repo, &l).map_err(|e| {
                    exit(
                        1,
                        format!(
                            "error: cannot read the working folder of line '{name}': {e:#}\nTo delete the line and its folder as it is, run 'layr line delete {flags} {name}'."
                        ),
                    )
                })?;
                if !unsaved.is_empty() {
                    let mut shown = unsaved.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
                    if unsaved.len() > 5 {
                        shown.push_str(&format!(", and {} more", unsaved.len() - 5));
                    }
                    return Err(exit(
                        1,
                        format!(
                            "error: line '{name}' has unsaved changes in {} file(s): {shown}\nIn the line, save them ('layr add', then 'layr save -m <message>') or park them ('layr park'). To discard them, run 'layr line delete {flags} {name}'.",
                            unsaved.len()
                        ),
                    ));
                }
            }
            // A fast line: as a folder line, unsaved changes need --discard above; the ones it
            // has then become its final state (docs/layrfs-lab.md).
            if crate::views::fast_of(repo, &l).is_some() {
                let deleted = line_json(repo, &l);
                #[cfg(target_os = "linux")]
                crate::views::delete_layrfs(ctx, repo, &l, line_lock, discard, check_merged)?;
                #[cfg(target_os = "macos")]
                crate::cmd::view_ops::delete(ctx, repo, line_lock, &target, discard, check_merged)?;
                if a.has("--json") {
                    outln!(ctx, "{}", deleted);
                } else {
                    outln!(ctx, "Deleted line {} (was {}).", l.name, ids::short(&l.head));
                }
                continue;
            }
            // A virtual line: its unsaved changes come from its upper folder (docs/views.md).
            #[cfg(target_os = "macos")]
            if view.is_some() {
                let deleted = line_json(repo, &l);
                crate::cmd::view_ops::delete(ctx, repo, line_lock, &target, discard, check_merged)?;
                if a.has("--json") {
                    outln!(ctx, "{}", deleted);
                } else {
                    outln!(ctx, "Deleted line {} (was {}).", l.name, ids::short(&l.head));
                }
                continue;
            }
            // The working state is prepared before the lock; under it the record and the renames only
            // (issue #194). Changed content or moved inputs prepare again outside the lock,
            // up to three times.
            // A line that this machine never made a folder for (another machine's line that a fetch brought, as a
            // CI line) has no working state to keep: the record alone deletes it.
            #[cfg(target_os = "linux")]
            crate::views::ensure_no_pending_move(repo, &l)?;
            let made = target.has_folder();
            let (_lock, _fresh, last, staged) = prepare_deletion(ctx, repo, &l, line_lock, made, discard, || {
                let fresh = target.validate(repo)?;
                repo.authorize_line(ctx, &fresh, crate::repo::LineOp::Manage)?;
                check_merged(&fresh)?;
                Ok(fresh)
            })?;
            // Final preparation already made the complete APFS tree. Background retirement
            // only drops an older layer after its readers have had time to finish.
            #[cfg(target_os = "macos")]
            if let Some(id) = &last {
                if repo.p.delta_path(id).is_dir() {
                    crate::free::keyframe_later(id);
                }
            }
            let l = target.validate(repo)?;
            let deleted = line_json(repo, &l);
            delete_recorded(&_lock, ctx, repo, &l, last, made, staged)?;
            drop(_lock);
            if let Err(e) = repo.finish_line_delete_caches(ctx) {
                eprintln!("warning: cache overlay lines of deleted lines stay until the next gc: {e:#}");
            }
            if a.has("--json") {
                outln!(ctx, "{}", deleted);
            } else {
                outln!(ctx, "Deleted line {} (was {}).", l.name, ids::short(&l.head));
            }
        }
        return Ok(0);
    }
    if a.has("-m") || a.has("-M") {
        let (old, new) = match a.pos.len() {
            1 => (current.as_ref().map(|c| c.name.clone()).unwrap_or_default(), a.pos[0].clone()),
            2 => (a.pos[0].clone(), a.pos[1].clone()),
            _ => return Err(exit(128, "usage: layr line rename [<old>] <new>")),
        };
        crate::store::check_name("line", &new)?;
        let l = repo.line(&old)?;
        if crate::views::of(repo, &l).is_some() {
            return Err(exit(
                1,
                format!("line {old} is virtual: rename it after 'layr line show {old} --folder' (docs/views.md)"),
            ));
        }
        repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
        let _lock = repo.p.lock_maintenance()?;
        if repo.p.db.line_by_name(&new)?.is_some() {
            return Err(exit(128, format!("fatal: a line named '{new}' already exists")));
        }
        // The folder `lines/<new>` that `line rename` kept for another line stays that line's: two lines never
        // share one folder (a delete of one would trash the other's work).
        if let Some(owner) = repo.p.kept_folder_line(&new).filter(|id| *id != l.id) {
            let owner = repo.p.db.line_by_id(&owner)?.map_or(owner, |o| o.name);
            return Err(exit(
                128,
                format!(
                    "fatal: the folder {} belongs to line '{owner}' (renamed from '{new}'); choose another name, or run 'layr line move {owner}' to move that folder to the folder of its name",
                    repo.p.default_line_path(&new).display()
                ),
            ));
        }
        // The folder stays where it is: programs that work in it (a shell, an editor, an agent)
        // keep their folder. `line move <line>` gives it the folder of the new name.
        let folder_before = repo.p.keep_folder(&l.id, &l.name, &new)?;
        let mut after = repo.line_state(&l, &l.head);
        after.name = new.clone();
        let mut rec = crate::model::DraftRecord::new("line.rename");
        rec.lines.insert(
            l.id.clone(),
            LineChange { transition: crate::model::LineTransition::Move { before: repo.line_state(&l, &l.head), after: after }, working_before: None, working_after: None },
        );
        rec.data = serde_json::json!({"from": old, "to": new});
        if let Err(e) = repo.commit_record(&_lock, ctx, rec) {
            if matches!(e, crate::db::CommitResolution::NotCommitted(_)) {
                if let Err(b) = repo.p.restore_folder(&l.id, folder_before.as_deref()) {
                    return Err(
                        anyhow::Error::from(e).context(format!("the folder record of line {old} could not go back: {b:#}"))
                    );
                }
            }
            return Err(e.into());
        }
        // The line has the same name on every overlay. A failure here is no loss: the overlay finds
        // the line by its id, and its next write gives it the new name.
        if let Err(e) = repo.rename_overlay_lines(ctx, &l, &new) {
            eprintln!("layr: warning: the overlays keep the old name of line {old} until their next write: {e:#}");
        }
        outln!(ctx, "Renamed line {old} to {new}; its folder stays at {}", repo.p.line_path(&new).display());
        return Ok(0);
    }
    if !a.pos.is_empty() && !a.has("-l") {
        let name = &a.pos[0];
        // The start state: the revision, or the head of the current line (or of main), read once.
        // It is pinned, so the line starts there even when that line moves before the commit (a
        // save, a fetch or a follow): the start is the state at the time of the command.
        let resolve_start = || -> Result<String> {
            Ok(match a.pos.get(1) {
                Some(r) => revs::resolve_state(repo, current.as_ref(), r)?,
                None => match &current {
                    Some(c) => repo.p.db.line_by_id(&c.id)?.map_or_else(|| c.head.clone(), |l| l.head),
                    None => match repo.p.db.line_by_name("main")? {
                        Some(m) => m.head,
                        None => return Err(exit(128, "fatal: give a start revision")),
                    },
                },
            })
        };
        // A line made from a line (its name, or the current line) shows the overlays of that line
        // that its owner reads; a line made from a state shows none.
        let source_of = |start: &str| -> Result<Option<Line>> {
            Ok(match a.pos.get(1) {
                Some(r) => repo.p.db.line_by_name(r)?.map(|l| repo.localize(l)).transpose()?,
                None => match &current {
                    Some(c) => repo.p.db.line_by_id(&c.id)?.map(|l| repo.localize(l)).transpose()?,
                    None => repo.p.db.line_by_name("main")?.map(|l| repo.localize(l)).transpose()?,
                },
            }
            .filter(|l| l.head == start))
        };
        let mut tries = 0;
        let start = loop {
            let start = resolve_start()?;
            // A state that another command frees at this moment refuses the pin: read it again.
            match repo.pin(&[&start]) {
                Ok(pin) => {
                    crate::free::hold(pin);
                    break start;
                }
                Err(_) if tries < 2 => tries += 1,
                Err(e) => return Err(e),
            }
        };
        let source = source_of(&start)?;
        let explicit_view = match a.pos.get(1) {
            Some(rev) if rev.starts_with("views/") => Some(revs::resolve_view(repo, current.as_ref(), rev)?),
            Some(rev) if rev.starts_with("attestations/") => {
                let view = revs::resolve_view(repo, current.as_ref(), rev)?;
                (!view.layers.is_empty()).then_some(view)
            }
            _ => None,
        };
        // An automatic state holds the unsaved work of its line. A line started on it would carry
        // that work into its own saves and pushes; one with caches only is a warm start.
        let start_state = repo.state(&start)?;
        if start_state.kind == StateKind::Auto && !start_state.parents.is_empty() {
            let from = start_state
                .line
                .as_deref()
                .map(|id| repo.p.db.line_by_id(id))
                .transpose()?
                .flatten()
                .map(|l| repo.localize(l))
                .transpose()?;
            let mut unsaved = repo.unsaved_paths(&start_state.parents[0], &start_state)?;
            // A later named state of its line may hold that work: then it is saved.
            if let Some(head) = from.as_ref().map(|l| l.head.clone()).filter(|h| *h != start_state.parents[0]) {
                if !unsaved.is_empty() && repo.unsaved_paths(&head, &start_state)?.is_empty() {
                    unsaved.clear();
                }
            }
            if !unsaved.is_empty() {
                let mut shown = unsaved.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
                if unsaved.len() > 5 {
                    shown.push_str(&format!(" and {} more", unsaved.len() - 5));
                }
                return Err(exit(
                    1,
                    format!(
                        "error: {} is an automatic state of line '{}' with unsaved work ({shown}); a new line would carry it.\nStart from a named state, for example {} or origin/main, or save the work in its line first.",
                        ids::short(&start),
                        from.map(|l| l.name).unwrap_or_else(|| "deleted line".into()),
                        ids::short(&start_state.parents[0]),
                    ),
                ));
            }
        }
        let owner = owner_arg(ctx, a.get("--owner"))?;
        repo.require(ctx, "line.create").map_err(|e| exit(128, format!("fatal: {e}")))?;
        if owner != ctx.caller.uid {
            repo.require(ctx, "line.manage").map_err(|e| exit(128, format!("fatal: a line for another user: {e}")))?;
        }
        if owner == 0 && !ctx.caller.is_root() {
            return Err(exit(128, "fatal: only root can create a line owned by root"));
        }
        // The kind of line: fast (a LayrFS mount, Linux) or classic (a folder). Without a flag the
        // project's `line.kind` decides; a fast default that cannot apply gives a classic line.
        if a.has("--fast") && a.has("--classic") {
            return Err(exit(2, "--fast and --classic exclude each other"));
        }
        let configured = repo.p.config_value("line.kind")?.and_then(|v| v.as_str().map(str::to_owned));
        let fast = if a.has("--fast") {
            if a.get("--view").is_some() || explicit_view.is_some() {
                return Err(exit(2, "--fast makes a line from a state (no --view, no views/ start)"));
            }
            if let Err(why) = crate::views::fast_available(repo, &start) {
                return Err(exit(1, format!("error: cannot make a fast line: {why}")));
            }
            true
        } else if !a.has("--classic") && a.get("--view").is_none() && explicit_view.is_none() && configured.as_deref() == Some("fast") {
            match crate::views::fast_available(repo, &start) {
                Ok(()) => true,
                Err(why) => {
                    eprintln!("note: a classic line: {why}");
                    false
                }
            }
        } else {
            false
        };
        if fast {
            let names = match a.get("--count") {
                Some(count) => {
                    let count = count.parse::<usize>().map_err(|_| exit(128, "--count must be a positive integer"))?;
                    repo.batch_line_names(name, count)?
                }
                None => vec![name.to_string()],
            };
            for n in names {
                let l = crate::views::create_fast(ctx, repo, &n, &start, owner)?;
                crate::fast_cache::start(ctx, repo, &l, &start, source.as_ref());
                // The `.git` folder of a fast line, as `prepare_at` makes for other lines.
                if let Err(e) = crate::gitbridge::make_git_folder(repo, &l, &start, &repo.working(&l)) {
                    eprintln!("warning: line {} has no .git folder: {e:#}", l.name);
                }
                // The caches name the place of their build: relocate them at this line, as below.
                let working = repo.working(&l);
                if let Err(e) = repo
                    .cache_candidates(&working)
                    .and_then(|folders| repo.relocate_caches(&l, &folders, &working, crate::cache_overlay::CacheAccess::Prepared))
                {
                    eprintln!("warning: the caches of line {} start cold: {e:#}", l.name);
                }
                if a.has("--json") {
                    outln!(ctx, "{}", line_json(repo, &l));
                } else if a.has("--path") {
                    outln!(ctx, "{}", repo.working(&l).display());
                } else if !a.has("-q") {
                    outln!(ctx, "Created fast line {} at {} from {}", l.name, repo.working(&l).display(), ids::short(&start));
                }
            }
            return Ok(0);
        }
        match a.get("--view") {
            None | Some("folder") => {}
            Some("virtual") => {
                if a.get("--count").is_some() || explicit_view.is_some() {
                    return Err(exit(2, "--view virtual makes one line from a state (no --count, no views/ start)"));
                }
                {
                    let l = crate::views::create(ctx, repo, name, &start, owner)?;
                    if a.has("--json") {
                        outln!(ctx, "{}", line_json(repo, &l));
                    } else if a.has("--path") {
                        outln!(ctx, "{}", repo.working(&l).display());
                    } else if !a.has("-q") {
                        outln!(
                            ctx,
                            "Created virtual line {} at {} from {}",
                            l.name,
                            repo.working(&l).display(),
                            ids::short(&start)
                        );
                    }
                    return Ok(0);
                }
            }
            Some(other) => return Err(exit(2, format!("--view takes folder or virtual, not '{other}'"))),
        }
        // The tree clone, .git folder and caches are made before the lock. Under it the command
        // moves the folders into place and records them.
        let view = explicit_view.as_ref();
        let (names, from, from_line) = match a.get("--count") {
            Some(count) => {
                let count = count.parse::<usize>().map_err(|_| exit(128, "--count must be a positive integer"))?;
                (repo.batch_line_names(name, count)?, start.clone(), source.as_ref())
            }
            // A single line made from a view starts at the view's shared state, from no line.
            None => match view {
                Some(v) => (vec![name.to_string()], v.shared.clone(), None),
                None => (vec![name.to_string()], start.clone(), source.as_ref()),
            },
        };
        // A plain fetch or clone takes only the records of a cache overlay: the cache that the new lines start from
        // comes now from the store remote, before the lock.
        crate::store_remote::fetch_caches_on_demand(ctx, repo, &from, from_line, a.has("-q") || a.has("--json"));
        let prepared = repo.prepare_named_lines(ctx, names, &from, owner, from_line, view)?;
        // Test hook: wait after the prepare step, so a test can move the start before the commit.
        if let Some(ms) = std::env::var("LAYR_TEST_LINE_NEW_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let lock = match repo.p.lock_maintenance() {
            Ok(l) => l,
            Err(e) => {
                repo.discard_prepared_lines(prepared);
                return Err(e);
            }
        };
        let lines = repo.commit_prepared_lines(&lock, ctx, prepared)?;
        drop(lock);
        // The caches of the new lines name the place of their build: relocate them at the place of the line. The
        // relocation runs as the line owner, who cannot enter the store's work folder before the move.
        for l in &lines {
            let working = repo.working(l);
            let folders = repo.cache_candidates(&working)?;
            repo.relocate_caches(l, &folders, &working, crate::cache_overlay::CacheAccess::Prepared)?;
        }
        if a.get("--count").is_some() {
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&lines.iter().map(|line| line_json(repo, line)).collect::<Vec<_>>())?);
            } else {
                for l in lines {
                    if a.has("--path") {
                        outln!(ctx, "{}", repo.working(&l).display());
                    } else if !a.has("-q") {
                        outln!(ctx, "Created line {} at {} from {}", l.name, repo.working(&l).display(), ids::short(&start));
                    }
                }
            }
            return Ok(0);
        }
        let l = lines.into_iter().next().expect("one line");
        if a.has("--json") {
            outln!(ctx, "{}", line_json(repo, &l));
            return Ok(0);
        }
        if a.has("--path") {
            outln!(ctx, "{}", repo.working(&l).display());
        } else if !a.has("-q") {
            outln!(ctx, "Created line {} at {} from {}", l.name, repo.working(&l).display(), ids::short(&start));
        }
        return Ok(0);
    }
    // List.
    let lines = repo.lines()?;
    let pat = a.pos.first().cloned();
    if a.has("--json") {
        if a.has("-a") || a.has("-r") {
            return Err(exit(2, "line list --json supports local lines; omit --all and --remotes"));
        }
        let rows: Vec<_> = lines
            .iter()
            .filter(|line| pat.as_ref().map(|p| super::glob(p, &line.name)).unwrap_or(true))
            .map(|line| line_json(repo, line))
            .collect();
        outln!(ctx, "{}", serde_json::to_string(&rows)?);
        return Ok(0);
    }
    let show_local = !a.has("-r");
    if show_local {
        for l in &lines {
            if let Some(p) = &pat {
                if !super::glob(p, &l.name) {
                    continue;
                }
            }
            let mark = if current.as_ref().map(|c| c.id == l.id).unwrap_or(false) { "* " } else { "  " };
            if a.has("-v") {
                let s = repo.state(&l.head)?;
                let owner = Caller::from_uid(l.owner).user;
                let here_m = if l.machine == repo.p.store.machine.id {
                    String::new()
                } else {
                    format!(" [machine {}]", &ids::display(&l.machine)[..8])
                };
                outln!(
                    ctx,
                    "{mark}{:<20} {} {} ({owner}){here_m}",
                    l.name,
                    ids::short(&l.head),
                    s.message.lines().next().unwrap_or("")
                );
            } else {
                outln!(ctx, "{mark}{}", l.name);
            }
        }
    }
    if a.has("-a") || a.has("-r") {
        for (r, s, _) in repo.p.db.remote_refs()? {
            let prefix = if a.has("-a") { "remotes/" } else { "" };
            if a.has("-v") {
                outln!(ctx, "  {prefix}{r} {}", ids::short(&s));
            } else {
                outln!(ctx, "  {prefix}{r}");
            }
        }
    }
    Ok(0)
}

/// Record the deletion of line `l` and remove what it holds on this machine: its folder (when it
/// has one, `made`), stage, bases and caches. The caller holds the project lock. `last` is the state
/// of the working folder that the record keeps.
pub(crate) fn delete_recorded(
    locked: &crate::store::LockedProject<'_>,
    ctx: &Ctx,
    repo: &crate::repo::Repo,
    l: &crate::model::Line,
    last: Option<String>,
    made: bool,
    staged: Option<String>,
) -> Result<()> {
    // Read the configured detachment plan before committing. Filesystem checks are
    // best effort after publication: an inaccessible cache must not refuse deletion.
    let root = repo.working(l);
    let detach_path = |folder: &str| -> Result<Option<std::path::PathBuf>> {
        crate::fsutil::check_rel(folder)?;
        let mut path = root.clone();
        for component in folder.split('/').filter(|c| !c.is_empty()) {
            path.push(component);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Some(path))
    };
    let mut folders = std::collections::BTreeSet::new();
    if made {
        folders.extend(repo.folder_overlays()?.into_iter().map(|(_, folder)| folder));
        folders.extend(repo.view_cache_overlays()?.into_iter().map(|(_, folder, _)| folder));
        for folder in &folders {
            crate::fsutil::check_rel(folder)?;
        }
    }
    // An OverlayFS folder is a mount. Unmount it before the record: an unmount error then keeps
    // the line, and its folder is mounted again by the next command.
    #[cfg(feature = "overlayfs")]
    if made {
        crate::overlayfs::release_tree(&repo.working(l))?;
    }
    // Record first, then delete the folder: the content is in the saved state.
    let mut rec = crate::model::DraftRecord::new("line.delete");
    rec.lines.insert(
        l.id.clone(),
        LineChange { transition: crate::model::LineTransition::Delete { before: Some(repo.line_state(l, &l.head)) }, working_before: last.clone(), working_after: None },
    );
    rec.data = serde_json::json!({"name": l.name, "final_state": last, "staged_state": staged});
    repo.commit_record(locked, ctx, rec)?;
    if made {
        crate::fsutil::git_writable(&repo.working(l), true);
        // Detach known folder overlays first. A nested cache would otherwise force
        // the trash worker to search the entire working tree for storage roots.
        {
            let _known = crate::trace::span("delete: known nested subvolumes");
            for folder in folders {
                // Walk without following links; a build may have replaced a parent.
                let result = detach_path(&folder).and_then(|path| match path {
                    Some(path) => repo.p.trash(&path),
                    None => Ok(()),
                });
                if let Err(e) = result {
                    eprintln!("warning: could not detach cache folder '{folder}' of deleted line '{}': {e:#}", l.name);
                }
            }
        }
        // Into the trash, emptied after the store lock: a folder with build output takes
        // minutes to delete, and other commands must not wait for it.
        repo.p.trash(&repo.working(l))?;
    }
    repo.p.set_moved_path(&l.id, None)?;
    #[cfg(target_os = "linux")]
    crate::views::drop_fast_files(repo, &l.id)?;
    #[cfg(target_os = "linux")]
    crate::views::forget(repo, &l.id)?;
    repo.drop_stage(l)?;
    repo.set_pending_moves(l, &Default::default())?;
    repo.prune_bases(l, None, 0)?;
    // The owner of a deleted line no longer reads the git objects.
    crate::gitbridge::sync_store_access(repo)?;
    // The line's head no longer keeps its caches.
    super::admin::trash_excess_caches(&repo.p)?;
    Ok(())
}

/// Give a line a working folder on this machine (after a move or a restore).
pub fn adopt(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().value("--state").flag("--head");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let name = a.pos.first().ok_or_else(|| exit(128, "usage: layr adopt <line> [--state <rev>|--head]"))?;
    let l = repo.line(name)?;
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
    // The caches of the line: a plain fetch takes only the records of a cache overlay, so they come now from the
    // store remote, before the lock (`take_caches` below runs under it).
    if !repo.working(&l).exists() {
        crate::store_remote::fetch_caches_on_demand(ctx, repo, &l.head, Some(&l), false);
    }
    let _lock = repo.p.lock_maintenance()?;
    let w = repo.working(&l);
    if w.exists() {
        return Err(exit(1, format!("line '{name}' already has a working folder here: {}", w.display())));
    }
    // `Project::line_folder`: another line keeps the folder of this name.
    if w.starts_with(repo.p.meta_dir()) {
        return Err(exit(
            1,
            format!(
                "the folder {} belongs to another line (kept by 'line rename'); rename line '{name}' first",
                repo.p.default_line_path(name).display()
            ),
        ));
    }
    let start = match a.get("--state") {
        Some(r) => revs::resolve_state(repo, Some(&l), r)?,
        None if a.has("--head") => l.head.clone(),
        None => repo
            .p
            .db
            .line_states(&l.id, StateKind::Auto)?
            .into_iter()
            .find(|s| repo.p.has_state(&s.id))
            .map(|s| s.id)
            .unwrap_or(l.head.clone()),
    };
    let mut moved = l.clone();
    moved.machine = repo.p.store.machine.id.clone();
    repo.materialize(&moved, &start, &w)?;
    // The caches of the line come with it (a live move sends them with `sync --caches`). A cache that cannot be
    // taken leaves its folder cold; the line is adopted.
    let folders = repo.take_caches(ctx, &moved, &start, Some(&l), &w)?;
    repo.relocate_caches(&moved, &folders, &w, crate::cache_overlay::CacheAccess::Prepared)?;
    crate::gitbridge::sync_store_access(repo)?;
    let mut rec = crate::model::DraftRecord::new("line.adopt");
    rec.lines.insert(
        l.id.clone(),
        LineChange { transition: crate::model::LineTransition::Move { before: repo.line_state(&l, &l.head), after: repo.line_state(&moved, &l.head) }, working_before: None, working_after: Some(start.clone()) },
    );
    let shown = repo.readable_layers(&moved)?;
    rec.data = serde_json::json!({
        "name": l.name,
        "from_state": start,
        "folder_view": repo.public_view_data(&l.id, &shown)?,
    });
    repo.commit_record(&_lock, ctx, rec)?;
    outln!(ctx, "Line {} now has a working folder here: {} (content of {})", l.name, w.display(), ids::short(&start));
    Ok(0)
}

pub fn tag(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new()
        .flag("-a|--annotate")
        .value("-m|--message")
        .flag("-d|--delete")
        .flag("-l|--list")
        .flag("-f|--force")
        .optional("-n");
    let a = parse(&spec, args)?;
    let h = here(ctx)?;
    let repo = &h.repo;
    let line = h.line.clone();
    if a.has("-d") {
        repo.require(ctx, "tag")?;
        let _lock = repo.p.lock_maintenance()?;
        for t in &a.pos {
            let id = repo.p.db.tag(t)?.ok_or_else(|| exit(1, format!("error: tag '{t}' not found.")))?;
            let mut rec = crate::model::DraftRecord::new("tag.delete");
            rec.data = serde_json::json!({"name": t, "state": id});
            repo.commit_record(&_lock, ctx, rec)?;
            outln!(ctx, "Deleted tag '{t}' (was {})", ids::short(&id));
        }
        return Ok(0);
    }
    if a.pos.is_empty() || a.has("-l") {
        for (t, _s, m) in repo.p.db.tags()? {
            if let Some(p) = a.pos.first() {
                if !super::glob(p, &t) {
                    continue;
                }
            }
            if a.has("-n") {
                outln!(ctx, "{:<15} {}", t, m.unwrap_or_default());
            } else {
                outln!(ctx, "{t}");
            }
        }
        return Ok(0);
    }
    let name = &a.pos[0];
    crate::store::check_name("tag", name)?;
    // Tags are names of the whole project and keep their states from gc: the admin sets them.
    repo.require(ctx, "tag")?;
    let id = revs::resolve_state(repo, line.as_ref(), a.pos.get(1).map(|s| s.as_str()).unwrap_or("head"))?;
    let _lock = repo.p.lock_maintenance()?;
    if repo.p.db.tag(name)?.is_some() && !a.has("-f") {
        return Err(exit(128, format!("fatal: tag '{name}' already exists")));
    }
    let mut rec = crate::model::DraftRecord::new("tag");
    rec.data = serde_json::json!({"name": name, "state": id, "message": a.get("-m")});
    repo.commit_record(&_lock, ctx, rec)?;
    Ok(0)
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use crate::model::{Author, LineState};
    use crate::store::{Project, Store};
    use serde_json::json;

    // Only records and plain directories: no snapshots, mounts, service or Btrfs send.
    #[test]
    fn deletion_destinations_use_fresh_heads_and_preserve_base_only_lines() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-deletion-target-{}", ids::new_id()));
        let store = Store::open(&dir)?;
        let project_dir = store.project_dir("p")?;
        std::fs::create_dir_all(project_dir.join("meta/db"))?;
        let repo = Repo {
            restore_from_remote: true,
            p: Project {
                lease: Default::default(),
                store: &store,
                name: "p".into(),
                dir: project_dir.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&project_dir.join("meta/db/layr.sqlite"))?,
            },
            ctx: None,
        };
        let machine = &store.machine.id;
        let main_id = ids::new_id();
        let topic_id = ids::new_id();
        let empty_id = ids::new_id();
        let base = ids::new_id();
        let saved = ids::new_id();
        let merged = ids::new_id();
        let mut sequence = 0;
        let mut apply = |mut rec: crate::model::Record| -> Result<()> {
            sequence += 1;
            rec.seq = sequence;
            rec.time = sequence as i64;
            rec.machine = machine.clone();
            repo.p.db.test_insert_record(&rec, &json!({"r": rec, "s": ""}).to_string())?;
            repo.p.db.test_derive(&rec, &|_| true, machine)
        };
        let line_change = |name: &str, head: &str| -> LineChange {
            crate::model::LineChangeWire {
                before: None,
                after: Some(LineState { name: name.into(), head: head.into(), owner: 1000, machine: machine.clone() }),
                working_before: None,
                working_after: None,
            }
            .try_into()
            .expect("a line change")
        };
        let mut rec = crate::model::Record::untrusted_preview("fixture");
        for (id, parents, line) in [
            (&base, vec![], &main_id),
            (&saved, vec![base.clone()], &topic_id),
            (&merged, vec![base.clone(), saved.clone()], &main_id),
        ] {
            rec.states.push(serde_json::from_value(json!({
                "id": id, "kind": StateKind::Commit, "parents": parents, "line": line,
                "subvol": "", "author": Author::default(), "time": 0
            }))?);
        }
        rec.lines.insert(main_id.clone(), line_change("main", &saved));
        rec.lines.insert(topic_id.clone(), line_change("topic", &saved));
        rec.lines.insert(empty_id, line_change("empty", &base));
        apply(rec)?;
        let topic = repo.line("topic")?;
        let current = repo.line("main")?;
        check_deletion_merged(&repo, &topic, None)?; // Equal heads after a fast-forward.
        assert!(check_deletion_merged(&repo, &topic, Some(&topic)).unwrap_err().to_string().contains("same line"));
        assert!(check_deletion_merged(&repo, &current, None).unwrap_err().to_string().contains("same line"));

        let mut rec = crate::model::Record::untrusted_preview("fixture");
        rec.lines.insert(main_id.clone(), line_change("main", &base));
        apply(rec)?;
        // A stale current-line value must not authorize deletion after its head is rewound.
        let err = check_deletion_merged(&repo, &topic, Some(&current)).unwrap_err().to_string();
        assert!(err.contains("not fully merged into 'main'"), "{err}");
        let mut rec = crate::model::Record::untrusted_preview("fixture");
        rec.lines.insert(main_id.clone(), line_change("main", &merged));
        apply(rec)?;
        check_deletion_merged(&repo, &topic, None)?; // Ordinary merge, second-parent ancestry.

        let mut rec = crate::model::Record::untrusted_preview("fixture");
        rec.lines.insert(main_id, line_change("trunk", &merged));
        apply(rec)?;
        let err = format!("{:#}", check_deletion_merged(&repo, &topic, None).unwrap_err());
        assert!(err.contains("default deletion target 'main' is unavailable"), "{err}");
        assert!(err.contains("line 'main' not found"), "{err}");
        check_deletion_merged(&repo, &topic, Some(&current))?; // Lookup uses stable identity after rename.
        check_deletion_merged(&repo, &repo.line("empty")?, None)?;
        drop(repo);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
