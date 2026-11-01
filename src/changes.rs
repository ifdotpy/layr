//! Change lists between two read-only snapshots, from `btrfs send --no-data`.
//!
//! A change list is a candidate list: an entry that kept its inode and path has the changed byte
//! ranges, so a content check reads only those ranges. Other entries are checked in full.

use crate::fsutil::{self, Meta, Src};
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
use crate::storage as btrfs;
use crate::stream;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Change {
    /// Path in the child (or in the parent for a delete).
    pub path: String,
    /// Path in the parent when it differs from `path` (a move detected at inode level).
    pub old_path: Option<String>,
    pub old: Option<Meta>,
    pub new: Option<Meta>,
    /// Changed byte ranges when the entry kept its inode and path; `None` means "compare all".
    pub ranges: Option<Vec<(u64, u64)>>,
}

impl Change {
    pub fn src(&self) -> &str {
        self.old_path.as_deref().unwrap_or(&self.path)
    }
    pub fn is_rename(&self) -> bool {
        self.old_path.is_some() && self.old.is_some() && self.new.is_some()
    }
}

/// Raw candidate list from the stream, without content checks.
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
pub fn candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<Change>> {
    let (parent, child) = (folder(parent)?, folder(child)?);
    let entries = btrfs::send_no_data(parent, child)?;
    build(parent, child, entries)
}

fn build(parent: &Path, child: &Path, entries: Vec<stream::Entry>) -> Result<Vec<Change>> {
    // Every path the stream touched, in the parent and in the child. `Some(ranges)` when the only
    // entry at the path kept its inode there, so unchanged byte ranges share extents.
    let mut paths: BTreeMap<String, Option<Vec<(u64, u64)>>> = BTreeMap::new();
    let mut moves: Vec<(String, String, Vec<(u64, u64)>)> = Vec::new();
    let mut moved_dirs: Vec<(String, String)> = Vec::new();
    let deleted: HashSet<String> = entries.iter().filter(|e| e.path.is_none()).filter_map(|e| e.origin.clone()).collect();
    let touch = |paths: &mut BTreeMap<String, Option<Vec<(u64, u64)>>>, p: &str, r: Option<Vec<(u64, u64)>>| match paths.get(p) {
        Some(_) => {
            paths.insert(p.to_string(), None);
        }
        None => {
            paths.insert(p.to_string(), r);
        }
    };
    for e in &entries {
        match (&e.origin, &e.path) {
            (Some(o), Some(p)) if o == p => touch(&mut paths, p, Some(e.ranges.clone())),
            (Some(o), Some(p)) => {
                touch(&mut paths, o, None);
                touch(&mut paths, p, None);
                moves.push((o.clone(), p.clone(), e.ranges.clone()));
                let od = fsutil::lmeta(parent, o).map(|m| m.is_dir()).unwrap_or(false);
                let nd = fsutil::lmeta(child, p).map(|m| m.is_dir()).unwrap_or(false);
                if od && nd {
                    moved_dirs.push((o.clone(), p.clone()));
                }
            }
            (Some(o), None) => touch(&mut paths, o, None),
            (None, Some(p)) => {
                touch(&mut paths, p, None);
                if let Some(l) = &e.linked_from {
                    if deleted.contains(l) {
                        moves.push((l.clone(), p.clone(), e.ranges.clone()));
                    }
                }
            }
            (None, None) => {}
        }
    }
    // Content below a moved folder moved too. One walk per outermost moved folder; a folder
    // moved inside it again maps its own files (the longest moved prefix of a path wins).
    let map: BTreeMap<String, String> = moved_dirs.iter().map(|(o, p)| (p.clone(), o.clone())).collect();
    let roots: Vec<&String> = map
        .keys()
        .filter(|p| !p.match_indices('/').any(|(i, _)| map.contains_key(&p[..i])))
        .collect();
    for p in roots {
        let mut found: Vec<(String, String)> = Vec::new();
        fsutil::walk(child, p, &mut |rel, _m| {
            let Some((top, from)) = rel.rmatch_indices('/').find_map(|(i, _)| map.get(&rel[..i]).map(|o| (i, o))) else {
                return Ok(true);
            };
            found.push((format!("{from}/{}", &rel[top + 1..]), rel.to_string()));
            Ok(true)
        })?;
        for (src, dst) in found {
            if !paths.contains_key(&dst) {
                paths.insert(dst.clone(), None);
                if !paths.contains_key(&src) {
                    paths.insert(src.clone(), None);
                }
                moves.push((src, dst, Vec::new()));
            }
        }
    }
    let mut out: BTreeMap<String, Change> = BTreeMap::new();
    for (p, ranges) in paths {
        let old = fsutil::lmeta(parent, &p);
        let new = fsutil::lmeta(child, &p);
        if old.is_none() && new.is_none() {
            continue;
        }
        out.insert(p.clone(), Change { path: p, old_path: None, old, new, ranges });
    }
    // Moves: the source is gone in the child and the target is new in the parent.
    let mut used: HashSet<String> = HashSet::new();
    for (src, dst, ranges) in moves {
        if used.contains(&src) || used.contains(&dst) {
            continue;
        }
        let ok = match (out.get(&src), out.get(&dst)) {
            (Some(s), Some(d)) => {
                s.new.is_none() && d.old.is_none() && s.old.as_ref().map(|m| m.ftype) == d.new.as_ref().map(|m| m.ftype)
            }
            _ => false,
        };
        if !ok {
            continue;
        }
        let s = out.remove(&src).unwrap();
        let d = out.get_mut(&dst).unwrap();
        d.old_path = Some(src.clone());
        d.old = s.old;
        d.ranges = Some(ranges);
        used.insert(src);
        used.insert(dst);
    }
    Ok(out.into_values().collect())
}

/// True when the entry changed in a way git would see: type, content, link target or the
/// executable bit of a file.
pub fn really_changed(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), c: &Change) -> Result<bool> {
    let (o, n) = match (&c.old, &c.new) {
        (Some(o), Some(n)) => (o, n),
        _ => return Ok(true),
    };
    if c.old_path.is_some() {
        return Ok(true);
    }
    if o.ftype != n.ftype {
        return Ok(true);
    }
    if o.is_file() && o.executable() != n.executable() {
        return Ok(true);
    }
    if o.is_dir() {
        return Ok(false);
    }
    // The backend already proved equal data while it built these ranges.
    // APFS also uses kernel clone IDs; live status reads validate generations.
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    if o.is_file() && o.size == n.size && c.ranges.as_ref().is_some_and(|ranges| ranges.is_empty()) {
        return Ok(!stored_same(parent, child, &c.path));
    }
    let equal = fsutil::same_entry(parent, c.src(), child, &c.path, c.ranges.as_deref())?;
    Ok(!equal || (o.is_file() && !stored_same(parent, child, &c.path)))
}

/// True when two files hold the same extended attributes and ACL (APFS; else always).
#[cfg(target_os = "macos")]
fn stored_same(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), path: &str) -> bool {
    match (parent.at(path), child.at(path)) {
        (Ok(a), Ok(b)) => crate::thin::stored_metadata_same(&a, &b).unwrap_or(false),
        _ => true,
    }
}
#[cfg(not(target_os = "macos"))]
fn stored_same(_: &(impl Src + ?Sized), _: &(impl Src + ?Sized), _: &str) -> bool {
    true
}

/// File level changes (folders dropped), confirmed by content. A move keeps `old_path`.
pub fn file_changes(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<Change>> {
    file_changes_of(parent, child, candidates_equal(parent, child)?)
}

/// File level changes of a candidate list (`file_changes`).
pub fn file_changes_of(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), list: Vec<Change>) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    for c in list {
        let is_dir_only =
            c.old.as_ref().map(|m| m.is_dir()).unwrap_or(true) && c.new.as_ref().map(|m| m.is_dir()).unwrap_or(true);
        if is_dir_only {
            continue;
        }
        // A folder replaced by a file or the reverse: report only the non-folder side.
        let mut c = c;
        if c.old.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            c.old = None;
            c.old_path = None;
        }
        if c.new.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            c.new = None;
        }
        if c.old.is_none() && c.new.is_none() {
            continue;
        }
        if c.old.is_some() && c.new.is_some() && !really_changed(parent, child, &c)? {
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

/// Split moves into a delete and an add (git shows unstaged moves this way).
pub fn without_moves(v: Vec<Change>) -> Vec<Change> {
    let mut out: BTreeMap<String, Change> = BTreeMap::new();
    for c in v {
        if let Some(src) = c.old_path.clone() {
            out.insert(src.clone(), Change { path: src, old_path: None, old: c.old.clone(), new: None, ranges: None });
            out.insert(
                c.path.clone(),
                Change { path: c.path.clone(), old_path: None, old: None, new: c.new.clone(), ranges: None },
            );
        } else {
            out.insert(c.path.clone(), c);
        }
    }
    out.into_values().collect()
}

/// Pair deleted and added files as moves when the content is equal, then (for small lists)
/// when at least half of the lines are equal, as git's rename detection does.
pub fn with_renames(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), v: Vec<Change>) -> Vec<Change> {
    // Files larger than MAX_TEXT are not compared by content.
    let is_file = |m: &Option<Meta>| {
        m.as_ref().map(|x| x.ftype == crate::fsutil::FType::File && x.size <= crate::fsutil::MAX_TEXT).unwrap_or(false)
    };
    let dels: Vec<usize> = v
        .iter()
        .enumerate()
        .filter(|(_, c)| c.new.is_none() && c.old_path.is_none() && is_file(&c.old))
        .map(|(i, _)| i)
        .collect();
    let adds: Vec<usize> = v.iter().enumerate().filter(|(_, c)| c.old.is_none() && is_file(&c.new)).map(|(i, _)| i).collect();
    if dels.is_empty() || adds.is_empty() {
        return v;
    }
    let read_parent = |p: &str| crate::fsutil::read_file(parent, p).ok();
    let read_child = |p: &str| crate::fsutil::read_file(child, p).ok();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut used_d: HashSet<usize> = HashSet::new();
    let mut used_a: HashSet<usize> = HashSet::new();
    // Exact: same size and same bytes. Each candidate is read and hashed once; the chosen pair
    // is compared byte by byte.
    let hash = |bytes: &[u8]| -> [u8; 32] { Sha256::digest(bytes).into() };
    let mut by_key: HashMap<(u64, [u8; 32]), Vec<usize>> = HashMap::new();
    let del_sizes: HashSet<u64> = dels.iter().map(|&d| v[d].old.as_ref().unwrap().size).collect();
    let add_sizes: HashSet<u64> = adds.iter().map(|&a| v[a].new.as_ref().unwrap().size).collect();
    for &d in &dels {
        let size = v[d].old.as_ref().unwrap().size;
        if add_sizes.contains(&size) {
            if let Some(x) = read_parent(&v[d].path) {
                by_key.entry((size, hash(&x))).or_default().push(d);
            }
        }
    }
    for &a in &adds {
        let size = v[a].new.as_ref().unwrap().size;
        if !del_sizes.contains(&size) {
            continue;
        }
        let Some(ad) = read_child(&v[a].path) else { continue };
        let Some(ds) = by_key.get(&(size, hash(&ad))) else { continue };
        for &d in ds {
            if !used_d.contains(&d) && read_parent(&v[d].path).as_ref() == Some(&ad) {
                pairs.push((d, a));
                used_d.insert(d);
                used_a.insert(a);
                break;
            }
        }
    }
    // Similar content, for small lists.
    let rd: Vec<usize> = dels.iter().copied().filter(|d| !used_d.contains(d)).collect();
    let ra: Vec<usize> = adds.iter().copied().filter(|a| !used_a.contains(a)).collect();
    if !rd.is_empty() && !ra.is_empty() && rd.len() * ra.len() <= 400 {
        let mut scored: Vec<(f32, usize, usize)> = Vec::new();
        for &d in &rd {
            let dd = match read_parent(&v[d].path) {
                Some(x) if !crate::fsutil::is_binary(&x) => x,
                _ => continue,
            };
            for &a in &ra {
                let ad = match read_child(&v[a].path) {
                    Some(x) if !crate::fsutil::is_binary(&x) => x,
                    _ => continue,
                };
                let (x, y) = (split(&dd), split(&ad));
                let ops = similar::capture_diff_slices(similar::Algorithm::Myers, &x, &y);
                let r = similar::get_diff_ratio(&ops, x.len(), y.len());
                if r >= 0.5 {
                    scored.push((r, d, a));
                }
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        for (_, d, a) in scored {
            if !used_d.contains(&d) && !used_a.contains(&a) {
                pairs.push((d, a));
                used_d.insert(d);
                used_a.insert(a);
            }
        }
    }
    let mut out: Vec<Change> = Vec::new();
    for (d, a) in &pairs {
        let mut c = v[*a].clone();
        c.old_path = Some(v[*d].path.clone());
        c.old = v[*d].old.clone();
        c.ranges = None;
        out.push(c);
    }
    for (i, c) in v.into_iter().enumerate() {
        if !used_d.contains(&i) && !used_a.contains(&i) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn split(d: &[u8]) -> Vec<&[u8]> {
    d.split_inclusive(|b| *b == b'\n').collect()
}

#[cfg(target_os = "macos")]
pub fn candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<Change>> {
    crate::storage::candidates(parent, child)
}

/// Candidates for a caller that only asks whether content differs: APFS then skips the exact
/// ranges of changed files. Other backends have the ranges from their stream at no cost.
#[cfg(target_os = "macos")]
fn candidates_equal(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<Change>> {
    crate::storage::candidates_equal(parent, child)
}

#[cfg(not(target_os = "macos"))]
fn candidates_equal(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<Change>> {
    candidates(parent, child)
}

#[cfg(feature = "overlayfs")]
pub fn candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<Change>> {
    crate::storage::candidates(folder(parent)?, folder(child)?)
}

/// Linux backends compare complete folders only.
#[cfg(not(target_os = "macos"))]
fn folder(s: &(impl Src + ?Sized)) -> Result<&Path> {
    s.folder().ok_or_else(|| anyhow::anyhow!("a layered tree exists only on APFS"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A folder that counts the reads of its files.
    struct Counting(PathBuf, AtomicUsize);
    impl Src for Counting {
        fn at(&self, rel: &str) -> Result<PathBuf> {
            self.1.fetch_add(1, Ordering::Relaxed);
            fsutil::safe_join(&self.0, rel)
        }
        fn folder(&self) -> Option<&Path> {
            Some(&self.0)
        }
    }

    fn meta(size: u64) -> Meta {
        Meta { ftype: fsutil::FType::File, mode: 0o644, size, ino: 0, uid: 0, gid: 0, mtime: 0 }
    }

    /// Exact rename search reads each candidate once, not once per pair of candidates.
    #[test]
    fn exact_renames_read_each_candidate_once() {
        let tmp = std::env::temp_dir().join(format!("layr-renames-{}", crate::ids::new_id()));
        let (p, c) = (tmp.join("p"), tmp.join("c"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        let n = 30;
        let mut changes = Vec::new();
        for i in 0..n {
            // All files have the same size and different bytes.
            std::fs::write(p.join(format!("old{i}")), format!("{i:08}")).unwrap();
            std::fs::write(c.join(format!("new{i}")), format!("{:08}", n - 1 - i)).unwrap();
            changes.push(Change { path: format!("old{i}"), old_path: None, old: Some(meta(8)), new: None, ranges: None });
            changes.push(Change { path: format!("new{i}"), old_path: None, old: None, new: Some(meta(8)), ranges: None });
        }
        let (parent, child) = (Counting(p, AtomicUsize::new(0)), Counting(c, AtomicUsize::new(0)));
        let out = with_renames(&parent, &child, changes);
        let moves = out.iter().filter(|c| c.old_path.is_some()).count();
        assert_eq!(moves, n, "every file pairs with its equal");
        let reads = parent.1.load(Ordering::Relaxed) + child.1.load(Ordering::Relaxed);
        assert!(reads <= 4 * n + 2, "{reads} reads for {n} + {n} candidates");
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
