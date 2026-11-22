//! The cache folders of fast lines (docs/layrfs-lab.md, "Caches in fast lines").
//!
//! The folder of a fast line is a LayrFS mount, so no nested subvolume (btrfs) or layer (APFS)
//! can live in it. Its cache folders live in `lines/.caches/<line id>/<folder>` instead: native
//! trees, as the caches of other lines are. The line shows each one with a link at the folder's
//! path. LayrFS leaves links into that root out of the line's content (status, snapshots), as a
//! line's snapshot leaves a nested cache out.
//!
//! A tool that removes the folder (`cargo clean`) removes the link only. `settle` then drops the
//! cache tree, and a folder that a tool made again moves into a new cache tree.
use crate::ctx::Ctx;
use crate::fsutil;
use crate::model::Line;
use crate::repo::Repo;
use crate::store::Project;
use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The cache root of the fast line `line_id`.
pub fn root(p: &Project, line_id: &str) -> PathBuf {
    p.lines_dir().join(".caches").join(line_id)
}

/// The cache root when `w` is the folder of the fast line `line` (its mount, or on macOS its
/// shadow); None for every other line or folder (a check copy).
pub fn root_for(repo: &Repo, line: &Line, w: &Path) -> Option<PathBuf> {
    crate::views::fast_of(repo, line)?;
    let shadow = repo.p.meta_dir().join("vlines").join(&line.id).join("shadow");
    (w == repo.working(line) || w == shadow).then(|| root(&repo.p, &line.id))
}

/// Make the cache root of `line` (owner: the line's owner). Its parent lets others pass only.
pub fn ensure_root(repo: &Repo, line: &Line) -> Result<PathBuf> {
    let base = repo.p.lines_dir().join(".caches");
    if base.symlink_metadata().is_err() {
        std::fs::create_dir(&base).or_else(|e| if base.is_dir() { Ok(()) } else { Err(e) })?;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o711))?;
    }
    let r = root(&repo.p, &line.id);
    if r.symlink_metadata().is_err() {
        std::fs::create_dir(&r)?;
        let (uid, gid) = repo.owner_ids(line);
        fsutil::chown_nofollow(&r, uid, gid)?;
        std::fs::set_permissions(&r, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(r)
}

/// True when a link to `target` is a link of a fast line to one of its cache trees: a path
/// inside its cache root `root` (`root_of_view`). LayrFS leaves such links out of the line's
/// content. A link into another folder (another line's root, a copy of a root) is content.
pub fn is_cache_target(target: &Path, root: &Path) -> bool {
    plain(target) && target != root && target.starts_with(root)
}

/// An absolute path without `..` parts: its text says where it goes.
fn plain(target: &Path) -> bool {
    use std::path::Component;
    target.is_absolute() && target.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// The cache root of the line whose view folder is `vdir` (`<project>/meta/vlines/<line id>`).
pub fn root_of_view(vdir: &Path) -> Option<PathBuf> {
    let id = vdir.file_name()?;
    let project = vdir.parent()?.parent()?.parent()?;
    Some(project.join("lines").join(".caches").join(id))
}

/// True when a link to `target` goes into the cache root of any fast line (a folder for
/// `regenerable_folders`, not a decision about content).
pub fn is_any_cache_target(target: &Path) -> bool {
    plain(target) && target.to_str().is_some_and(|t| t.contains("/lines/.caches/"))
}

/// True when the entry `rel` of `w` (metadata `m`) is a link into the cache root `root`.
pub fn is_cache_link(w: &Path, rel: &str, m: &fsutil::Meta, root: &Path) -> bool {
    m.ftype == fsutil::FType::Symlink && std::fs::read_link(w.join(rel)).is_ok_and(|t| is_cache_target(&t, root))
}

/// True when the entry `rel` of `w` is a link into any cache root (`is_any_cache_target`).
pub fn is_any_cache_link(w: &Path, rel: &str, m: &fsutil::Meta) -> bool {
    m.ftype == fsutil::FType::Symlink && std::fs::read_link(w.join(rel)).is_ok_and(|t| is_any_cache_target(&t))
}

/// True when `w/folder` is the link to `target`.
pub fn is_link(w: &Path, folder: &str, target: &Path) -> bool {
    fsutil::safe_join(w, folder).ok().and_then(|p| std::fs::read_link(p).ok()).is_some_and(|t| t == target)
}

/// The link at `folder` in the fast line folder `w` to its cache tree `target`. An empty folder
/// or another link at the path goes; any other entry is an error.
pub fn link(w: &Path, folder: &str, target: &Path, uid: u32, gid: u32) -> Result<()> {
    if is_link(w, folder, target) {
        return Ok(());
    }
    let at = fsutil::safe_join(w, folder)?;
    match std::fs::symlink_metadata(&at) {
        Ok(m) if m.file_type().is_symlink() => std::fs::remove_file(&at)?,
        Ok(m) if m.is_dir() => std::fs::remove_dir(&at).with_context(|| format!("{folder} is in the way of its cache"))?,
        Ok(_) => bail!("{folder} is in the way of its cache"),
        Err(_) => {}
    }
    fsutil::make_parents(w, folder, Some((uid, gid)))?;
    std::os::unix::fs::symlink(target, &at).with_context(|| format!("link {folder} to its cache"))?;
    let _ = rustix::fs::chownat(
        rustix::fs::CWD,
        &at,
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    );
    // Git sees the link as a file: a `target/` pattern matches folders only, so `git status`
    // would list it and `git clean -fd` would remove it. The `.git` folder of the line (in the
    // same cache root) leaves it out.
    if let Some(root) = target.ancestors().nth(Path::new(folder).components().count()) {
        let _ = exclude_in_git(root, folder, (uid, gid));
    }
    Ok(())
}

/// The cache links of the working folder `w` whose caches are in `root`: a walk of the folders of
/// the cache root (only caches and their parent folders are there).
pub fn links(root: &Path, w: &Path) -> Vec<String> {
    fn walk(root: &Path, w: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(root.join(rel)) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if rel.is_empty() && name == ".git" || !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let sub = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if is_link(w, &sub, &root.join(&sub)) {
                out.push(sub);
            } else {
                walk(root, w, &sub, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, w, "", &mut out);
    out.sort();
    out
}

/// Add `/<folder>` to `info/exclude` of the `.git` folder in the cache root `root`, when the
/// folder is there. Through descriptors: the folder belongs to the line owner.
fn exclude_in_git(root: &Path, folder: &str, own: (u32, u32)) -> Result<()> {
    use std::io::Write;
    let Ok(git) = fsutil::open_dir_nofollow(&root.join(".git")) else { return Ok(()) };
    let fd = rustix::fs::openat(
        &git,
        "info",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let info = std::fs::File::from(fd);
    let line = format!("/{folder}");
    let old = fsutil::read_at(&info, "exclude").unwrap_or_default();
    let mut text = String::from_utf8_lossy(&old).into_owned();
    if text.lines().any(|l| l == line) {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&line);
    text.push('\n');
    fsutil::replace_at(&info, "exclude", &mut |f| Ok((&*f).write_all(text.as_bytes())?), Some(own))
}

/// The caches of a new fast line: the ones of its start state (`take_caches`), and an empty one
/// for each folder of the machine setting `regenerable`, as `prepare_at` makes for other lines.
/// A failure leaves the line without caches: builds start cold.
pub fn start(ctx: &Ctx, repo: &Repo, line: &Line, start: &str, source: Option<&Line>) {
    let r = (|| -> Result<()> {
        let w = repo.working(line);
        let taken = repo.take_caches(ctx, line, start, source, &w)?;
        let (uid, gid) = repo.owner_ids(line);
        for p in repo.p.config_strings("regenerable") {
            if taken.contains(&p) || crate::repo::holds_files(&w, &p) {
                continue;
            }
            let r = ensure_root(repo, line)?;
            crate::repo::make_cache(&r, &p, uid, gid)?;
            link(&w, &p, &fsutil::safe_join(&r, &p)?, uid, gid)?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        // LAYR_ERROR_BACKTRACE=1 (with RUST_BACKTRACE=1): where the error came from.
        if std::env::var_os("LAYR_ERROR_BACKTRACE").is_some() {
            eprintln!("warning: the caches of line {} start empty: {e:?}", line.name);
        } else {
            eprintln!("warning: the caches of line {} start empty: {e:#}", line.name);
        }
    }
}

/// Bring the cache trees of fast line `line` in line with its folder, before a snapshot (the
/// `restore_caches` of other lines). The caller holds the project lock.
/// - a cache tree whose link is gone (`cargo clean`, `rm -rf`) goes;
/// - a cache folder that a tool made again as a plain folder moves into a new cache tree.
pub fn settle(repo: &Repo, line: &Line, plain: &[String], candidates: &[String]) -> Result<()> {
    let w = repo.working(line);
    let r = root(&repo.p, &line.id);
    let mut links = None;
    for p in candidates {
        let Ok(c) = fsutil::safe_join(&r, p) else { continue };
        if c.symlink_metadata().is_ok() && !is_link(&w, p, &c) {
            // A link that a tool moved keeps its tree (`mv target target-old`).
            let all = match &links {
                Some(l) => l,
                None => links.insert(crate::views::cache_links(repo, line)?),
            };
            if all.iter().any(|(_, t)| t == &c) {
                continue;
            }
            let _t = crate::trace::span(format!("drop the cache {p} of fast line {}", line.name));
            repo.p.trash(&c)?;
        }
    }
    // Trees that moved aside with a moved link (`free_tree`) go once no link names them.
    if let Ok(rd) = std::fs::read_dir(r.join(".moved")) {
        let aside: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
        if !aside.is_empty() {
            let all = match &links {
                Some(l) => l,
                None => links.insert(crate::views::cache_links(repo, line)?),
            };
            for a in aside {
                if !all.iter().any(|(_, t)| t == &a) {
                    let _t = crate::trace::span(format!("drop a moved cache of fast line {}", line.name));
                    repo.p.trash(&a)?;
                }
            }
        }
    }
    for p in plain {
        convert(repo, line, p)?;
    }
    Ok(())
}

/// Free the place of the cache tree `c` of `p` for a new tree: a link elsewhere in the line (a
/// moved link) keeps the old tree, which moves aside and the link with it; else it goes.
pub fn free_tree(repo: &Repo, line: &Line, w: &Path, c: &Path) -> Result<()> {
    let moved: Vec<String> =
        crate::views::cache_links(repo, line)?.into_iter().filter(|(_, t)| t == c).map(|(rel, _)| rel).collect();
    if moved.is_empty() {
        return repo.p.trash(c);
    }
    let aside = root(&repo.p, &line.id).join(".moved").join(crate::ids::new_id());
    std::fs::create_dir_all(aside.parent().context("a folder for moved caches")?)?;
    std::fs::rename(c, &aside).with_context(|| format!("move the cache tree {} aside", c.display()))?;
    let (uid, gid) = repo.owner_ids(line);
    for rel in moved {
        let at = fsutil::safe_join(w, &rel)?;
        std::fs::remove_file(&at)?;
        std::os::unix::fs::symlink(&aside, &at)?;
        let _ = rustix::fs::chownat(
            rustix::fs::CWD,
            &at,
            Some(rustix::fs::Uid::from_raw(uid)),
            Some(rustix::fs::Gid::from_raw(gid)),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        );
    }
    Ok(())
}

/// `convert_cache` for fast line `line`: a plain folder at `p` moves into a new cache tree (a copy
/// of its files), and a missing one gets an empty tree; the link takes its place. False when `p`
/// is a cache already, or no folder.
pub fn convert(repo: &Repo, line: &Line, p: &str) -> Result<bool> {
    let w = repo.working(line);
    let at = fsutil::safe_join(&w, p)?;
    let (uid, gid) = repo.owner_ids(line);
    let r = ensure_root(repo, line)?;
    let c = fsutil::safe_join(&r, p)?;
    match std::fs::symlink_metadata(&at) {
        Ok(_) if is_link(&w, p, &c) => return Ok(false),
        Ok(m) if !m.is_dir() => return Ok(false),
        _ => {}
    }
    let _t = crate::trace::span(format!("make {p} of fast line {} a cache", line.name));
    if c.symlink_metadata().is_ok() {
        free_tree(repo, line, &w, &c)?;
    }
    crate::repo::make_cache(&r, p, uid, gid)?;
    if at.symlink_metadata().is_ok() {
        copy_below(&w, &r, p, uid, gid)?;
        fsutil::remove_entry(&w, p)?;
    }
    link(&w, p, &c, uid, gid)?;
    Ok(true)
}

/// `uncache` for fast line `line`: the cache tree of `p` becomes a plain folder of the line with
/// the same files. False when `p` is no cache.
pub fn uncache(repo: &Repo, line: &Line, p: &str) -> Result<bool> {
    let w = repo.working(line);
    let c = fsutil::safe_join(&root(&repo.p, &line.id), p)?;
    if !is_link(&w, p, &c) {
        return Ok(false);
    }
    let (uid, gid) = repo.owner_ids(line);
    let r = root(&repo.p, &line.id);
    std::fs::remove_file(fsutil::safe_join(&w, p)?)?;
    std::fs::create_dir(fsutil::safe_join(&w, p)?)?;
    fsutil::chown_nofollow(&fsutil::safe_join(&w, p)?, uid, gid)?;
    copy_below(&r, &w, p, uid, gid)?;
    repo.p.trash(&c)?;
    Ok(true)
}

/// Copy every entry below the folder `p` of `from` to the same path in `to` (the folder is there).
fn copy_below(from: &Path, to: &Path, p: &str, uid: u32, gid: u32) -> Result<()> {
    let mut rels = Vec::new();
    fsutil::walk(from, p, &mut |rel, _| {
        if rel != p {
            rels.push(rel.to_string());
        }
        Ok(true)
    })?;
    // A folder comes before its content.
    for rel in rels {
        fsutil::copy_entry(from, &rel, to, &rel, Some((uid, gid)))?;
    }
    Ok(())
}

/// The cache root of a fast line that goes (`line delete`).
pub fn remove(p: &Project, line_id: &str) -> Result<()> {
    let r = root(p, line_id);
    if r.symlink_metadata().is_err() {
        return Ok(());
    }
    // The trees in it are nested subvolumes (btrfs) or layers (APFS): the trash removes them.
    p.trash(&r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_links_into_the_line_cache_root_are_caches() {
        let root = root_of_view(Path::new("/s/projects/p/meta/vlines/L1")).unwrap();
        assert_eq!(root, Path::new("/s/projects/p/lines/.caches/L1"));
        assert!(is_cache_target(Path::new("/s/projects/p/lines/.caches/L1/target"), &root));
        assert!(!is_cache_target(Path::new("/s/projects/p/lines/.caches/L1"), &root));
        assert!(!is_cache_target(Path::new("/s/projects/p/lines/.caches/L2/target"), &root));
        assert!(!is_cache_target(Path::new("/backup/s/projects/p/lines/.caches/L1/target"), &root));
        assert!(!is_cache_target(Path::new("/s/projects/p/lines/.caches/L1x/target"), &root));
        assert!(!is_cache_target(Path::new("lines/.caches/L1/target"), &root));
        assert!(!is_cache_target(Path::new("/s/projects/p/lines/.caches/L1/../file"), &root));
    }
}
