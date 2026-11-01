//! Lines, adoption, removed folder-switch commands, and tags.

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::{Caller, Ctx};
use crate::ids;
use crate::model::{LineChange, Record, StateKind};
use crate::revs;
use anyhow::Result;

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
    if !matches!(sub.as_str(), "new" | "list" | "delete" | "rename") {
        return Err(exit(2, "usage: layr line new/list/delete/rename/move"));
    }
    line_command(ctx, rest, sub)
}

/// The checked absolute target of `layr line move <line> <path>`.
fn move_target(ctx: &Ctx, repo: &crate::repo::Repo, from: &std::path::Path, arg: &str) -> Result<std::path::PathBuf> {
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
        return Err(fail(format!("{} is inside the store {}; use 'layr line move <line>' to move a line back", to.display(), root.display())));
    }
    if to.starts_with(from) {
        return Err(fail("the target is inside the folder of the line".into()));
    }
    let store_dev = std::fs::metadata(repo.p.lines_dir())?.dev();
    if std::fs::metadata(&parent)?.dev() != store_dev {
        return Err(fail(format!("{} is on another file system than the store; a line can only move within one file system", parent.display())));
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
    repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
    if h.line.as_ref().is_some_and(|c| c.id == l.id) {
        return Err(exit(1, format!("error: cannot move line '{}' used as the current folder; run the command from another folder", l.name)));
    }
    let _lock = repo.p.lock()?;
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
    crate::linepaths::register_with(&ctx.store.root, Some(&to));
    let back = a.pos.len() == 1;
    let done = crate::fsutil::rename_tree(&from, &to).map_err(anyhow::Error::from).and_then(|_| {
        let r = repo.p.set_moved_path(&l.id, if back { None } else { Some(&to) });
        if r.is_err() {
            let _ = crate::fsutil::rename_tree(&to, &from);
        }
        r
    });
    crate::linepaths::register(&ctx.store.root);
    done?;
    outln!(ctx, "Moved line {} to {}", l.name, to.display());
    Ok(0)
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
        .flag("--show-current")
        .flag("--path")
        .value("--owner")
        .value("--count")
        .flag("--json")
        .optional("--format")
        .value("--contains")
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
        if a.pos.is_empty() {
            return Err(exit(128, "fatal: line name required"));
        }
        for name in &a.pos {
            let l = repo.line(name)?;
            repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
            if current.as_ref().map(|c| c.id == l.id).unwrap_or(false) {
                return Err(exit(1, format!("error: cannot delete line '{name}' used as the current folder")));
            }
            if a.has("-d") && !a.has("-f") {
                let into = current.as_ref().map(|c| c.head.clone());
                let merged = match &into {
                    Some(h2) => repo.is_ancestor(&l.head, h2)?,
                    None => false,
                };
                let base_only = repo.p.db.line_states(&l.id, StateKind::Commit)?.is_empty();
                if !merged && !base_only {
                    return Err(exit(1, format!("error: the line '{name}' is not fully merged.\nIf you are sure you want to delete it, run 'layr line delete -D {name}'.")));
                }
            }
            repo.prepare_save(&l)?;
            let _lock = repo.p.lock()?;
            // Automatic states leave overlay files out: uncommitted changes of them would be lost.
            if !a.has("-f") && repo.is_stacked(&l)? && repo.working(&l).exists() {
                let private = repo.private_paths(&l)?;
                let (st, _scan, _it, _idx) = repo.status(&l)?;
                let changed = st
                    .staged
                    .iter()
                    .chain(st.unstaged.iter())
                    .flat_map(|c| [Some(c.path.clone()), c.old_path.clone()])
                    .flatten()
                    .any(|p| private.contains(&p));
                if changed || !repo.pending_moves(&l).is_empty() {
                    return Err(exit(
                        1,
                        format!("error: line '{name}' has unsaved changes of overlay files, which automatic states do not keep; save them, or delete the line with -f"),
                    ));
                }
            }
            let l = repo.line(&l.id)?;
            let last = repo.working_state(ctx, &l, "before line delete")?;
            // A staged version may exist nowhere else: keep it as a state, so `undo` restores the index.
            let staged = staged_state(ctx, repo, &l)?;
            // Record first, then delete the folder: the content is in the saved state.
            let mut rec = Record::new("line.delete");
            rec.lines.insert(
                l.id.clone(),
                LineChange {
                    before: Some(repo.line_state(&l, &l.head)),
                    after: None,
                    working_before: last.clone(),
                    working_after: None,
                },
            );
            rec.data = serde_json::json!({"name": l.name, "final_state": last, "staged_state": staged});
            repo.commit_record(ctx, rec)?;
            crate::fsutil::git_writable(&repo.working(&l), true);
            // Into the trash, emptied after the store lock: a folder with build output takes
            // minutes to delete, and other commands must not wait for it.
            repo.p.trash(&repo.working(&l))?;
            repo.p.set_moved_path(&l.id, None)?;
            repo.drop_stage(&l)?;
            repo.set_pending_moves(&l, &Default::default())?;
            repo.prune_bases(&l, None, 0)?;
            // The owner of a deleted line no longer reads the git objects.
            crate::gitbridge::sync_store_access(repo)?;
            // The line's head no longer keeps its caches.
            super::admin::trash_excess_caches(repo)?;
            // Its cache overlay lines go too; their states go at the next gc.
            if let Err(e) = repo.drop_orphan_cache_lines(ctx) {
                eprintln!("warning: cache overlay lines of deleted lines stay until the next gc: {e:#}");
            }
            outln!(ctx, "Deleted line {} (was {}).", l.name, ids::short(&l.head));
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
        repo.authorize_line(ctx, &l, crate::repo::LineOp::Manage)?;
        let _lock = repo.p.lock()?;
        if repo.p.db.line_by_name(&new)?.is_some() {
            return Err(exit(128, format!("fatal: a line named '{new}' already exists")));
        }
        // A line moved to a chosen path keeps its folder.
        if repo.p.moved_path(&l.id).is_none() {
            crate::fsutil::rename_tree(repo.p.line_path(&l.name), repo.p.default_line_path(&new))?;
        }
        let mut after = repo.line_state(&l, &l.head);
        after.name = new.clone();
        let mut rec = Record::new("line.rename");
        rec.lines.insert(
            l.id.clone(),
            LineChange {
                before: Some(repo.line_state(&l, &l.head)),
                after: Some(after),
                working_before: None,
                working_after: None,
            },
        );
        rec.data = serde_json::json!({"from": old, "to": new});
        repo.commit_record(ctx, rec)?;
        outln!(ctx, "Renamed line {old} to {new}: {}", repo.p.line_path(&new).display());
        return Ok(0);
    }
    if !a.pos.is_empty() && !a.has("-l") {
        let name = &a.pos[0];
        let start = match a.pos.get(1) {
            Some(r) => revs::resolve(repo, current.as_ref(), r)?,
            None => match &current {
                Some(c) => c.head.clone(),
                None => match repo.p.db.line_by_name("main")? {
                    Some(m) => m.head,
                    None => return Err(exit(128, "fatal: give a start revision")),
                },
            },
        };
        // A line made from a line (its name, or the current line) shows the overlays of that line
        // that its owner reads; a line made from a state shows none.
        let source = match a.pos.get(1) {
            Some(r) => repo.p.db.line_by_name(r)?.map(|l| repo.localize(l)).transpose()?,
            None => match &current {
                Some(c) => Some(c.clone()),
                None => repo.p.db.line_by_name("main")?.map(|l| repo.localize(l)).transpose()?,
            },
        }
        .filter(|l| l.head == start);
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
            let from = start_state.line.as_deref().map(|id| repo.p.db.line_by_id(id)).transpose()?.flatten().map(|l| repo.localize(l)).transpose()?;
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
        // The tree clone, .git folder and caches are made before the lock; under it the command
        // checks the names and the start state, moves the folders into place and records them.
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
        let prepared = repo.prepare_named_lines(ctx, names, &from, owner, from_line, view)?;
        let lock = match repo.p.lock() {
            Ok(l) => l,
            Err(e) => {
                repo.discard_prepared_lines(prepared);
                return Err(e);
            }
        };
        let lines = repo.commit_prepared_lines(ctx, prepared)?;
        drop(lock);
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

/// Removed folder-switch commands never change files or create a line.
pub fn removed_switch(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let name = args.iter().find(|a| !a.starts_with('-')).map(String::as_str);
    let folder = match here(ctx) {
        Ok(h) => match name {
            Some(n) if crate::store::check_name("line", n).is_ok() => h.repo.p.line_path(n),
            _ => h.line.as_ref().map(|l| h.repo.working(l)).unwrap_or_else(|| h.repo.p.line_path("<name>")),
        },
        Err(_) => ctx.store.root.join("projects/<project>/lines").join(name.unwrap_or("<name>")),
    };
    Err(exit(
        2,
        format!("A line is a folder. Use 'cd {}' to work there. Create a line with 'layr line new <name>'.", folder.display()),
    ))
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
    let _lock = repo.p.lock()?;
    let w = repo.working(&l);
    if w.exists() {
        return Err(exit(1, format!("line '{name}' already has a working folder here: {}", w.display())));
    }
    let start = match a.get("--state") {
        Some(r) => revs::resolve(repo, Some(&l), r)?,
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
    crate::gitbridge::sync_store_access(repo)?;
    let mut rec = Record::new("line.adopt");
    rec.lines.insert(
        l.id.clone(),
        LineChange {
            before: Some(repo.line_state(&l, &l.head)),
            after: Some(repo.line_state(&moved, &l.head)),
            working_before: None,
            working_after: Some(start.clone()),
        },
    );
    rec.data = serde_json::json!({"name": l.name, "from_state": start, "stack":repo.public_stack_data(&l.id, &repo.readable_layers(&moved)?)?});
    repo.commit_record(ctx, rec)?;
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
        let _lock = repo.p.lock()?;
        for t in &a.pos {
            let id = repo.p.db.tag(t)?.ok_or_else(|| exit(1, format!("error: tag '{t}' not found.")))?;
            let mut rec = Record::new("tag.delete");
            rec.data = serde_json::json!({"name": t, "state": id});
            repo.commit_record(ctx, rec)?;
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
    let id = revs::resolve(repo, line.as_ref(), a.pos.get(1).map(|s| s.as_str()).unwrap_or("head"))?;
    let _lock = repo.p.lock()?;
    if repo.p.db.tag(name)?.is_some() && !a.has("-f") {
        return Err(exit(128, format!("fatal: tag '{name}' already exists")));
    }
    let mut rec = Record::new("tag");
    rec.data = serde_json::json!({"name": name, "state": id, "message": a.get("-m")});
    repo.commit_record(ctx, rec)?;
    Ok(0)
}

/// The index of a line as an automatic state, when it differs from the head (None otherwise).
fn staged_state(ctx: &Ctx, repo: &crate::repo::Repo, l: &crate::model::Line) -> Result<Option<String>> {
    if repo.current_stage(l)?.is_none() {
        return Ok(None);
    }
    let (index, _temp) = repo.index_view(l)?;
    let head = repo.head_src(l)?;
    if !crate::changes::file_changes(&head, &index)?.iter().any(|c| !crate::repo::skip_path(&c.path)) {
        return Ok(None);
    }
    let mut keep = Vec::new();
    let tree = repo.complete(index, &mut keep)?;
    let st = repo.new_state(ctx, &tree, StateKind::Auto, vec![l.head.clone()], "staged before line delete", Some(l))?;
    let mut rec = Record::new("save");
    rec.states.push(st.clone());
    rec.data = serde_json::json!({"line": l.id});
    repo.commit_record(ctx, rec)?;
    Ok(Some(st.id))
}
