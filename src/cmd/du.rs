//! `layr du`: the disk space of each line, and the part of it that only that line holds.
use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::fs::accounting::{AllocationPart, Disappearance};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub fn du(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").flag("--store").flag("--volumes"), args)?;
    if a.has("--volumes") {
        if !a.pos.is_empty() {
            bail!("usage: layr du --volumes [--json]");
        }
        return volumes_report(ctx, &ctx.store.root, a.has("--json"));
    }
    let h = here(ctx)?;
    if a.has("--store") {
        if !a.pos.is_empty() {
            bail!("usage: layr du --store [--json]");
        }
        return store(ctx, a.has("--json"));
    }
    let repo = &h.repo;
    let mut lines = Vec::new();
    for l in repo.lines()? {
        // The folder of a virtual line is a mount; its upper folder is in the project's meta folder.
        if crate::views::of(repo, &l).is_some() {
            continue;
        }
        if a.pos.is_empty() || a.pos.contains(&l.name) {
            let path = repo.working(&l);
            if path.is_dir() {
                lines.push((l.name, path));
            }
        }
    }
    for name in &a.pos {
        if !lines.iter().any(|(n, _)| n == name) {
            bail!("no line '{name}' with a folder on this machine");
        }
    }
    // No store lock: the walks only read the line folders.
    let paths = lines.iter().map(|(_, path)| path.clone()).collect::<Vec<_>>();
    let results = crate::fs::host().accounting.line_spaces(&paths)?;
    let mut rows = Vec::new();
    for ((name, path), r) in lines.into_iter().zip(results) {
        match r {
            Ok(space) => rows.push((name, path, space)),
            Err(e) => eprintln!("warning: line '{name}': {e:#}"),
        }
    }
    rows.sort_by(|x, y| y.2.unique.cmp(&x.2.unique).then_with(|| x.0.cmp(&y.0)));
    if a.has("--json") {
        let v: Vec<_> = rows
            .iter()
            .map(|(name, path, s)| serde_json::json!({"line": name, "path": path, "total": s.total, "unique": s.unique}))
            .collect();
        outln!(ctx, "{}", serde_json::to_string(&v)?);
        return Ok(0);
    }
    let size = super::admin::human_size;
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max(4);
    outln!(ctx, "{:<width$}  {:>10}  {:>10}", "line", "unique", "total");
    for (name, _, s) in &rows {
        outln!(ctx, "{:<width$}  {:>10}  {:>10}", name, size(s.unique), size(s.total));
    }
    let unique: u64 = rows.iter().map(|r| r.2.unique).sum();
    outln!(ctx, "{:<width$}  {:>10}", "all lines", size(unique));
    outln!(ctx, "unique: bytes that only this line holds; deleting the line frees them.");
    Ok(0)
}

/// The child folders of a folder, by name, sorted.
fn child_dirs(p: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(p)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            v.push((e.file_name().to_string_lossy().into_owned(), e.path()));
        }
    }
    v.sort();
    Ok(v)
}

/// The parts of the projects of one volume: each line folder, each other folder of a project
/// (states, deltas, git, meta). `label` names the volume in the rows (empty for `main`).
fn project_parts(
    store: &crate::store::Store,
    projects: &Path,
    moved: &[crate::linepaths::Entry],
    label: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    for (project, ppath) in child_dirs(projects)? {
        // Lines moved out of the store (`layr line move`) are listed with the others.
        for e in moved.iter().filter(|e| e.project == project && e.path.is_dir()) {
            let project_db = crate::store::Project::open_partial(store, &project);
            if let Some(l) = project_db.ok().and_then(|p| p.db.line_by_id(&e.line).ok().flatten()) {
                out.push((format!("{label}{project}/lines/{}", l.name), e.path.clone()));
            }
        }
        for (part, path) in child_dirs(&ppath)? {
            if part == "lines" {
                for (line, path) in child_dirs(&path)? {
                    out.push((format!("{label}{project}/lines/{line}"), path));
                }
            } else {
                out.push((format!("{label}{project}/{part}"), path));
            }
        }
    }
    Ok(())
}

/// The parts of a store: each line folder, each other folder of a project (states, deltas, git,
/// meta), and each folder at the top (trash). The other volumes contribute their projects and trash.
fn parts(store: &crate::store::Store) -> Result<Vec<(String, PathBuf)>> {
    let root = &store.root;
    let mut out = Vec::new();
    let moved = crate::linepaths::index(root);
    for (name, path) in child_dirs(root)? {
        if let Some(label) = crate::fs::host().allocation_part.and_then(|label| label(&path)) {
            out.push((label, path));
            continue;
        }
        if name != "projects" {
            out.push((name, path));
            continue;
        }
        project_parts(store, &path, &moved, "", &mut out)?;
    }
    let layout = crate::volumes::Layout::for_root(root)?;
    for (volume, projects) in layout.ready_projects_dirs().into_iter().filter(|(v, _)| v != crate::volumes::MAIN) {
        let label = format!("volume {volume}: ");
        project_parts(store, &projects, &moved, &label, &mut out)?;
        if let Some(trash) = projects.parent().map(|v| v.join("trash")).filter(|t| t.is_dir()) {
            out.push((format!("{label}trash"), trash));
        }
    }
    for name in layout.names() {
        if let Err(e) = layout.ready(&name) {
            eprintln!("warning: {e:#}; its projects are not counted");
        }
    }
    Ok(out)
}

/// `layr du --volumes`: the bytes of the projects on each volume, and the volume state.
fn volumes_report(ctx: &Ctx, root: &Path, json: bool) -> Result<i32> {
    let layout = crate::volumes::Layout::for_root(root)?;
    let mut rows: Vec<(String, Option<usize>, Option<u64>, PathBuf, String)> = Vec::new();
    for name in layout.names() {
        let path = layout.path_of(&name).unwrap_or_default();
        match layout.ready(&name) {
            Ok(_) => {
                let projects = layout.projects_folder(&name)?;
                let count = child_dirs(&projects)?.iter().filter(|(_, p)| crate::store::db_file(p).is_file()).count();
                let total = match crate::fs::host().accounting.space(&projects) {
                    Ok(space) => Some(space.total),
                    Err(e) => {
                        eprintln!("warning: volume '{name}': {e:#}");
                        None
                    }
                };
                rows.push((name, Some(count), total, path, "ok".into()));
            }
            Err(e) => rows.push((name, None, None, path, format!("{e:#}"))),
        }
    }
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|(name, count, total, path, status)| {
                serde_json::json!({"volume": name, "projects": count, "total": total, "path": path, "status": status})
            })
            .collect();
        outln!(ctx, "{}", serde_json::to_string(&v)?);
        return Ok(0);
    }
    let size = super::admin::human_size;
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(6).max(6);
    outln!(ctx, "{:<width$}  {:>8}  {:>10}  {}", "volume", "projects", "total", "path");
    for (name, count, total, path, status) in &rows {
        let count = count.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
        let total = total.map(size).unwrap_or_else(|| "-".into());
        outln!(ctx, "{:<width$}  {:>8}  {:>10}  {}", name, count, total, path.display());
        if status != "ok" {
            outln!(ctx, "\t{status}");
        }
    }
    outln!(ctx, "total: bytes of the project folders of each volume (APFS and btrfs count shared data once).");
    Ok(0)
}

/// `layr du --store`: the space of every part of the store, and what deleting each one frees.
/// Data that clones share counts once, under its key (`fsutil::Held`): it belongs to a part
/// when no other part references it.
fn store(ctx: &Ctx, json: bool) -> Result<i32> {
    let parts = parts(&ctx.store)?;
    let requests = parts
        .iter()
        .map(|(name, path)| AllocationPart {
            path: path.clone(),
            disappearance: if name == "trash" || name.ends_with(": trash") {
                Disappearance::Ignore
            } else {
                Disappearance::Report
            },
        })
        .collect::<Vec<_>>();
    let report = crate::fs::host().accounting.allocations(&ctx.store.root, &requests)?;
    let shared = report.shared;
    let spaces = parts
        .iter()
        .zip(report.parts)
        .map(|((name, _), space)| match space {
            Ok(space) => Some(space),
            Err(error) => {
                eprintln!("warning: {name}: {error:#}");
                None
            }
        })
        .collect::<Vec<_>>();
    let mut rows: Vec<(&str, crate::fs::accounting::PartSpace)> =
        parts.iter().zip(spaces).filter_map(|((n, _), s)| s.map(|s| (n.as_str(), s))).collect();
    rows.sort_by(|x, y| (y.1.own + y.1.sole).cmp(&(x.1.own + x.1.sole)).then_with(|| x.0.cmp(y.0)));
    let total = rows.iter().map(|r| r.1.own + r.1.sole).sum::<u64>() + shared;
    if json {
        let v: Vec<_> = rows.iter().map(|(n, s)| serde_json::json!({"part": n, "frees": s.own + s.sole})).collect();
        outln!(ctx, "{}", serde_json::to_string(&serde_json::json!({"parts": v, "shared": shared, "total": total}))?);
        return Ok(0);
    }
    let size = super::admin::human_size;
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(4).max("shared by several parts".len());
    outln!(ctx, "{:<width$}  {:>10}", "part", "frees");
    for (n, s) in rows.iter().filter(|r| r.1.own + r.1.sole > 0) {
        outln!(ctx, "{:<width$}  {:>10}", n, size(s.own + s.sole));
    }
    outln!(ctx, "{:<width$}  {:>10}", "shared by several parts", size(shared));
    outln!(ctx, "{:<width$}  {:>10}", "store total", size(total));
    outln!(ctx, "frees: bytes that only this part holds; deleting it frees them.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::fs::accounting::vanished;
    use std::path::PathBuf;

    #[test]
    fn vanished_part_has_no_allocations() {
        let path = std::env::temp_dir().join(format!("layr-du-missing-{}", std::process::id()));
        let report = crate::fs::host()
            .accounting
            .allocations(
                path.parent().unwrap(),
                &[crate::fs::accounting::AllocationPart {
                    path: path.clone(),
                    disappearance: crate::fs::accounting::Disappearance::Ignore,
                }],
            )
            .unwrap();
        assert_eq!(report.shared, 0);
        let part = report.parts.into_iter().next().unwrap().unwrap();
        assert_eq!(part.own + part.sole, 0);
    }

    #[test]
    fn a_removed_path_is_vanished_and_other_errors_are_not() {
        let dir = PathBuf::from(std::env::temp_dir()).join(format!("layr-du-walk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let gone = dir.join("entry-gone");
        let missing = anyhow::Error::from(std::fs::read_dir(&gone).unwrap_err());
        let not_dir = anyhow::Error::from(std::fs::read_dir(&file).unwrap_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(vanished(&missing), "a missing entry must be vanished: {missing:#}");
        assert!(!vanished(&not_dir), "a file that is not a folder must still warn: {not_dir:#}");
    }
}
