//! `layr du`: the disk space of each line, and the part of it that only that line holds.
use super::here;
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use crate::fsutil::{ExtentKey, Space};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// On btrfs, schedule directories independently so a single large line can use the pool.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
fn line_spaces(lines: &[(String, PathBuf)]) -> Vec<Result<Space>> {
    use std::collections::HashSet;
    let seen: Vec<_> = lines.iter().map(|_| Mutex::new(HashSet::new())).collect();
    let results: Vec<_> = lines.iter().map(|_| Mutex::new(Ok(Space::default()))).collect();
    let seed = lines.iter().enumerate().map(|(i, (_, path))| (i, path.clone())).collect();
    crate::fsutil::work_queue(seed, |(i, path), add| {
        let walked = (|| -> Result<Space> {
            let mut space = Space::default();
            crate::fsutil::fiemap_visit_shallow(&path, &seen[i], &mut |child| add((i, child)), &mut |held, _, _| {
                space.add(held)
            })?;
            Ok(space)
        })();
        let mut result = results[i].lock().unwrap();
        match (result.as_mut(), walked) {
            (Ok(total), Ok(space)) => {
                total.total += space.total;
                total.unique += space.unique;
            }
            (_, Err(e)) => *result = Err(e),
            _ => {}
        }
    });
    results.into_iter().map(|r| r.into_inner().unwrap()).collect()
}

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
        return store(ctx, &h.repo.p.store.root, a.has("--json"));
    }
    let repo = &h.repo;
    let mut lines = Vec::new();
    for l in repo.lines()? {
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
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    let results = line_spaces(&lines);
    #[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
    let results = {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: Vec<std::sync::Mutex<Option<Result<Space>>>> = lines.iter().map(|_| Default::default()).collect();
        std::thread::scope(|s| {
            for _ in 0..crate::fsutil::workers().min(lines.len()) {
                s.spawn(|| {
                    crate::sys::lower_thread_priority();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((_, path)) = lines.get(i) else { break };
                        *results[i].lock().unwrap() = Some(crate::storage::space(path));
                    }
                });
            }
        });
        results.into_iter().map(|r| r.into_inner().unwrap().expect("every line was read")).collect::<Vec<_>>()
    };
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
    projects: &Path,
    moved: &[crate::linepaths::Entry],
    label: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<()> {
    for (project, ppath) in child_dirs(projects)? {
        // Lines moved out of the store (`layr line move`) are listed with the others.
        for e in moved.iter().filter(|e| e.project == project && e.path.is_dir()) {
            let db = crate::db::Db::open(&crate::store::db_file(&ppath));
            if let Some(l) = db.ok().and_then(|db| db.line_by_id(&e.line).ok().flatten()) {
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

/// True when the error says that a path is gone (ENOENT): a trash entry that the emptier removed
/// during the walk. Other errors are not vanished.
fn vanished(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        || e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT)
}

/// The parts of a store: each line folder, each other folder of a project (states, deltas, git,
/// meta), and each folder at the top (trash). The other volumes contribute their projects and trash.
fn parts(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let moved = crate::linepaths::index(root);
    for (name, path) in child_dirs(root)? {
        // The OverlayFS registry holds layers (a part of their own) and the upper layers of
        // lines, which belong to the lines.
        if name == ".layr-overlayfs" {
            out.push(("overlayfs layers".into(), path));
            continue;
        }
        if name != "projects" {
            out.push((name, path));
            continue;
        }
        project_parts(&path, &moved, "", &mut out)?;
    }
    let layout = crate::volumes::Layout::for_root(root)?;
    for (volume, projects) in layout.ready_projects_dirs().into_iter().filter(|(v, _)| v != crate::volumes::MAIN) {
        let label = format!("volume {volume}: ");
        project_parts(&projects, &moved, &label, &mut out)?;
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
                let total = match crate::storage::space(&projects) {
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

/// What one part holds alone: bytes that no other file shares, and shared data that no other
/// part of the store references.
#[derive(Default)]
struct PartSpace {
    own: u64,
    sole: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum FsId {
    #[cfg(target_os = "linux")]
    BtrfsUuid([u8; 16]),
    Device(u64),
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mounts {
    None,
    Below,
    Unknown,
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
impl Mounts {
    fn resolve_directories(self) -> bool {
        self != Self::None
    }
}

#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
fn file_filesystem(dir_dev: u64, file_dev: u64, dir_fs: FsId, resolve: impl FnOnce() -> FsId) -> FsId {
    if dir_dev == file_dev {
        dir_fs
    } else {
        resolve()
    }
}

#[cfg(target_os = "linux")]
fn filesystem_fd(file: &std::fs::File, dev: u64) -> FsId {
    use std::os::fd::AsFd;
    crate::sys::filesystem_uuid(file.as_fd()).map(FsId::BtrfsUuid).unwrap_or(FsId::Device(dev))
}

#[cfg(feature = "overlayfs")]
fn overlay_namespace<'a>(path: &'a Path, backing: Option<&'a Path>) -> &'a Path {
    backing.unwrap_or(path)
}

fn filesystem(path: &Path) -> std::io::Result<FsId> {
    use std::os::unix::fs::MetadataExt;
    #[cfg(feature = "overlayfs")]
    let backing = crate::storage::du_backing(path);
    #[cfg(feature = "overlayfs")]
    let path = overlay_namespace(path, backing.as_deref());
    #[cfg(target_os = "linux")]
    {
        if let Ok(file) = std::fs::File::open(path) {
            return Ok(filesystem_fd(&file, file.metadata()?.dev()));
        }
    }
    Ok(FsId::Device(path.metadata()?.dev()))
}

fn filesystem_or_empty(path: &Path) -> std::io::Result<Option<FsId>> {
    match filesystem(path) {
        Ok(id) => Ok(Some(id)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// `layr du --store`: the space of every part of the store, and what deleting each one frees.
/// Data that clones share counts once, under its key (`fsutil::Held`): it belongs to a part
/// when no other part references it.
fn store(ctx: &Ctx, root: &Path, json: bool) -> Result<i32> {
    const SHARED: usize = usize::MAX;
    let parts = parts(root)?;
    let filesystems: Vec<_> = parts.iter().map(|(_, path)| filesystem_or_empty(path)).collect::<std::io::Result<_>>()?;
    let keys: Mutex<HashMap<(FsId, ExtentKey), (usize, u64)>> = Mutex::new(HashMap::new());
    let results: Vec<Mutex<Result<PartSpace>>> = parts.iter().map(|_| Mutex::new(Ok(PartSpace::default()))).collect();
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    let mounts = Mutex::new(Mounts::Unknown);
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    let outside = {
        let paths: Vec<_> = parts.iter().map(|(_, path)| path.clone()).collect();
        let plan = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    crate::sys::lower_thread_priority();
                    if let Some(mount) = crate::btrfs_scan::nested_mount(&paths)? {
                        eprintln!("note: nested mount {}: walking files instead", mount.display());
                        *mounts.lock().unwrap() = Mounts::Below;
                        return Ok(None);
                    }
                    *mounts.lock().unwrap() = Mounts::None;
                    crate::btrfs_scan::plan(root, &paths).map(Some)
                })
                .join()
                .unwrap()
        });
        match plan {
            Ok(Some(plan)) => {
                let failed = Mutex::new(None);
                let bulk = Mutex::new(HashMap::new());
                crate::fsutil::work_queue(plan.units, |unit, _| {
                    let mut local = HashMap::<(usize, ExtentKey), u64>::new();
                    let visited = crate::btrfs_scan::visit(&unit, &mut |part, held| {
                        for &(key, bytes) in held.shared {
                            let value = local.entry((part, key)).or_default();
                            *value = (*value).max(bytes);
                        }
                    });
                    if let Err(err) = visited {
                        *failed.lock().unwrap() = Some(err);
                        return;
                    }
                    let mut all = bulk.lock().unwrap();
                    for ((part, key), bytes) in local {
                        let Some(fsid) = filesystems[part] else { continue };
                        let entry = all.entry((fsid, key)).or_insert((part, bytes));
                        if entry.0 != part {
                            entry.0 = SHARED;
                        }
                        entry.1 = entry.1.max(bytes);
                    }
                });
                if let Some(err) = failed.into_inner().unwrap() {
                    eprintln!("warning: bulk btrfs scan failed: {err}; walking files instead");
                    None
                } else {
                    *keys.lock().unwrap() = bulk.into_inner().unwrap();
                    Some(plan.outside)
                }
            }
            Ok(None) => None,
            Err(err) => {
                if !matches!(err.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::ENOTTY | libc::EINVAL)) {
                    eprintln!("warning: bulk btrfs scan failed: {err}; walking files instead");
                }
                None
            }
        }
    };
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    let mounts = mounts.into_inner().unwrap();
    // OverlayFS layer registry entries contain metadata as well as data trees.
    let mut units: Vec<(usize, PathBuf)> = Vec::new();
    for (i, (name, path)) in parts.iter().enumerate() {
        if filesystems[i].is_none() {
            continue;
        }
        #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
        if outside.as_ref().is_some_and(|outside| !outside.contains(&i)) {
            continue;
        }
        if name == "overlayfs layers" {
            for e in std::fs::read_dir(path.join("objects"))? {
                let data = e?.path().join("data");
                if data.is_dir() {
                    units.push((i, data));
                }
            }
        } else {
            #[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
            {
                let children: Vec<_> = std::fs::read_dir(path)?.collect::<std::io::Result<_>>()?;
                if !children.is_empty() && children.iter().all(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
                    units.extend(children.into_iter().map(|e| (i, e.path())));
                    continue;
                }
            }
            units.push((i, path.clone()));
        }
    }
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    let seen: Vec<_> = parts.iter().map(|_| Mutex::new(std::collections::HashSet::new())).collect();
    crate::fsutil::work_queue(units, |(i, path), _add| {
        let in_trash = parts[i].0 == "trash" || parts[i].0.ends_with(": trash");
        #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
        let unit_fs = if mounts.resolve_directories() { filesystem_or_empty(&path) } else { Ok(filesystems[i]) };
        let mut own = 0u64;
        let mut local: HashMap<(FsId, ExtentKey), u64> = HashMap::new();
        let mut collect = |h: &crate::fsutil::Held, fsid: FsId| {
            own += h.exclusive;
            for &(k, b) in h.shared {
                let e = local.entry((fsid, k)).or_insert(0);
                *e = (*e).max(b);
            }
        };
        #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
        let walked = match unit_fs {
            Ok(Some(fsid)) => {
                use std::os::unix::fs::MetadataExt;
                match std::fs::metadata(&path) {
                    Ok(meta) => crate::fsutil::fiemap_visit_shallow(
                        &path,
                        &seen[i],
                        &mut |child| _add((i, child)),
                        &mut |held, file, dev| {
                            let file_fs = file_filesystem(meta.dev(), dev, fsid, || filesystem_fd(file, dev));
                            collect(held, file_fs);
                        },
                    )
                    .map(|()| ()),
                    Err(err) => Err(err.into()),
                }
            }
            Ok(None) => return,
            Err(err) => Err(err.into()),
        };
        #[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
        let walked =
            crate::storage::visit_space(&path, &mut |held| collect(held, filesystems[i].expect("only present parts are walked")));
        let mut result = results[i].lock().unwrap();
        match walked {
            Err(e) if in_trash && vanished(&e) => {}
            Err(e) => *result = Err(e),
            Ok(()) => {
                let mut all = keys.lock().unwrap();
                for (k, b) in local {
                    let e = all.entry(k).or_insert((i, b));
                    if e.0 != i {
                        e.0 = SHARED;
                    }
                    e.1 = e.1.max(b);
                }
                if let Ok(p) = result.as_mut() {
                    p.own += own;
                }
            }
        }
    });
    let mut spaces: Vec<Option<PartSpace>> = Vec::new();
    for ((name, _), r) in parts.iter().zip(results) {
        match r.into_inner().unwrap() {
            Ok(v) => spaces.push(Some(v)),
            Err(e) => {
                eprintln!("warning: {name}: {e:#}");
                spaces.push(None);
            }
        }
    }
    let mut shared = 0u64;
    for (owner, bytes) in keys.into_inner().unwrap().into_values() {
        match spaces.get_mut(owner).and_then(Option::as_mut) {
            Some(p) => p.sole += bytes,
            None => shared += bytes,
        }
    }
    let mut rows: Vec<(&str, PartSpace)> =
        parts.iter().zip(spaces).filter_map(|((n, _), s)| s.map(|s| (n.as_str(), s))).collect();
    rows.sort_by(|x, y| (y.1.own + y.1.sole).cmp(&(x.1.own + x.1.sole)).then_with(|| x.0.cmp(y.0)));
    let total = rows.iter().map(|r| r.1.own + r.1.sole).sum::<u64>() + shared;
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|(n, s)| serde_json::json!({"part": n, "frees": s.own + s.sole}))
            .collect();
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
    use super::vanished;
    use std::path::PathBuf;

    #[cfg(feature = "overlayfs")]
    #[test]
    fn overlay_mount_uses_backing_namespace() {
        let view = std::path::Path::new("/store/line");
        let backing = std::path::Path::new("/store/.layr-overlayfs");
        assert_eq!(super::overlay_namespace(view, Some(backing)), backing);
        assert_eq!(super::overlay_namespace(view, None), view);
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    fn unknown_mounts_require_directory_filesystem_checks() {
        use super::Mounts;
        assert!(!Mounts::None.resolve_directories());
        assert!(Mounts::Below.resolve_directories());
        assert!(Mounts::Unknown.resolve_directories());
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    fn file_mount_uses_its_own_filesystem() {
        use super::{file_filesystem, FsId};
        assert_eq!(file_filesystem(1, 1, FsId::Device(1), || panic!("same device needs no lookup")), FsId::Device(1));
        assert_eq!(file_filesystem(1, 2, FsId::Device(1), || FsId::Device(2)), FsId::Device(2));
    }

    #[test]
    fn vanished_part_has_no_filesystem() {
        let path = std::env::temp_dir().join(format!("layr-du-missing-{}", std::process::id()));
        assert!(super::filesystem_or_empty(&path).unwrap().is_none());
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    fn line_directory_units_count_hard_links_once() {
        let dir = std::env::temp_dir().join(format!("layr-du-parallel-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/file"), vec![1u8; 8192]).unwrap();
        std::fs::hard_link(dir.join("a/file"), dir.join("b/link")).unwrap();
        let expected = crate::fsutil::fiemap_space(&dir);
        let actual = super::line_spaces(&[("line".into(), dir.clone())]).pop().unwrap();
        match expected {
            Ok(expected) => assert_eq!(actual.unwrap(), expected),
            Err(_) => assert!(actual.is_err()),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_removed_path_is_vanished_and_other_errors_are_not() {
        let dir = PathBuf::from(std::env::temp_dir()).join(format!("layr-du-walk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let gone = dir.join("entry-gone");
        let missing = crate::storage::visit_space(&gone, &mut |_: &crate::fsutil::Held| {}).unwrap_err();
        let not_dir = crate::storage::visit_space(&file, &mut |_: &crate::fsutil::Held| {}).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(vanished(&missing), "a missing entry must be vanished: {missing:#}");
        assert!(!vanished(&not_dir), "a file that is not a folder must still warn: {not_dir:#}");
    }
}
