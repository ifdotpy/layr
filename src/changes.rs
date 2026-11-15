//! Shared comparison of trees, optionally accelerated by complete filesystem candidates.
//!
//! A change list is a candidate list: an entry that kept its inode and path has the changed byte
//! ranges, so a content check reads only those ranges. Other entries are checked in full.

use crate::compare::DataEvidence;
pub use crate::compare::EntryChange;
use crate::fsutil::{self, Meta, Src};
use anyhow::Result;
use layr_hashers::{FastCryptographic, PurposeHasher};
use layr_hashers::{FastMap, FastSet};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

pub(crate) use layr_fs_core::{ChangeDetail, ChangeRequest, ChangeScope};
/// Candidate observations and content detail follow the caller's explicit semantic scope.
pub fn candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<EntryChange>> {
    candidates_request(parent, child, ChangeRequest { scope: ChangeScope::Content, detail: ChangeDetail::Ranges })
}
pub(crate) fn candidates_request(
    parent: &(impl Src + ?Sized),
    child: &(impl Src + ?Sized),
    request: ChangeRequest,
) -> Result<Vec<EntryChange>> {
    candidates_with_hints(parent, child, request, None, |a, b| crate::fs::candidate_hints(a, b, request))
}
pub(crate) fn candidates_selected(
    parent: &(impl Src + ?Sized),
    child: &(impl Src + ?Sized),
    paths: &std::collections::BTreeSet<String>,
) -> Result<Vec<EntryChange>> {
    candidates_with_hints(
        parent,
        child,
        ChangeRequest { scope: ChangeScope::Stored, detail: ChangeDetail::Equality },
        Some(paths),
        |_, _| Ok(layr_fs_core::hints::HintSet::Partial),
    )
}
fn candidates_with_hints(
    parent: &(impl Src + ?Sized),
    child: &(impl Src + ?Sized),
    request: ChangeRequest,
    selection: Option<&std::collections::BTreeSet<String>>,
    hints: impl FnOnce(&Path, &Path) -> layr_fs_core::Result<layr_fs_core::hints::HintSet>,
) -> Result<Vec<EntryChange>> {
    use layr_fs_core::hints::HintSet;
    // Object and logical-layer identities never acquire native tree authority.
    if selection.is_none() && parent.objects().is_none() && child.objects().is_none() {
        if let (Some(a), Some(b)) = (parent.folder(), child.folder()) {
            match hints(a, b) {
                Ok(HintSet::Complete(entries)) => return Ok(entries),
                Ok(HintSet::Partial) | Err(_) => {}
            }
        }
    }
    enum Reader<'a> {
        Borrowed(&'a dyn layr_fs_core::TreeRead),
        Native(std::sync::Arc<dyn layr_fs_core::TreeRead>),
    }
    impl Reader<'_> {
        fn read(&self) -> &dyn layr_fs_core::TreeRead {
            match self {
                Self::Borrowed(reader) => *reader,
                Self::Native(reader) => reader.as_ref(),
            }
        }
    }
    fn reader(source: &(impl Src + ?Sized)) -> Result<Reader<'_>> {
        if let Some(reader) = source.core_reader() {
            return Ok(Reader::Borrowed(reader));
        }
        if let Some(objects) = source.objects() {
            return Ok(Reader::Borrowed(objects));
        }
        Ok(Reader::Native(crate::fs::reader(&source.at("")?)?))
    }
    let (before, after) = (reader(parent)?, reader(child)?);
    let checked = selection
        .map(|paths| {
            paths
                .iter()
                .map(|path| layr_fs_core::RelPath::new(path.as_bytes()))
                .collect::<layr_fs_core::Result<std::collections::BTreeSet<_>>>()
        })
        .transpose()?;
    match before.read().candidate_evidence(after.read(), request, checked.as_ref())? {
        HintSet::Complete(entries) => return Ok(entries),
        HintSet::Partial => {}
    }
    let mut entries = object_change_candidates(parent, child)?;
    if let Some(selected) = selection {
        entries
            .retain(|change| selected.iter().any(|path| change.path() == path || change.path().starts_with(&format!("{path}/"))));
    }
    if request.detail == ChangeDetail::Ranges && parent.objects().is_none() && child.objects().is_none() {
        for change in &mut entries {
            if let (Some(old), Some(new)) = (change.old(), change.new()) {
                if old.is_file() && new.is_file() {
                    let mut a = fsutil::open_source(parent, change.src(), old)?;
                    let mut b = fsutil::open_source(child, change.path(), new)?;
                    let ranges = layr_fs_common::changed_ranges(&mut a, old.size, &mut b, new.size)?;
                    change.set_data(DataEvidence::from_native(&ranges, old.size, new.size).map_err(anyhow::Error::msg)?);
                }
            }
        }
    }
    Ok(entries)
}

/// True when the entry changed in a way git would see: type, content, link target or the
/// executable bit of a file.
pub fn really_changed(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), c: &EntryChange) -> Result<bool> {
    let (Some(o), Some(n)) = (c.old(), c.new()) else { return Ok(true) };
    if c.old_path().is_some() {
        return Ok(true);
    }
    if o.ftype != n.ftype {
        return Ok(true);
    }
    if o.is_file() && o.executable() != n.executable() {
        return Ok(true);
    }
    if o.is_dir() {
        return Ok(!stored_same(parent, child, c.path()));
    }
    // Equal data does not imply equal lengths or equal stored attributes.
    if o.is_file() && o.size == n.size && matches!(c.data(), DataEvidence::Equal) {
        return Ok(!stored_same(parent, child, c.path()));
    }
    let ranges = match c.data() {
        DataEvidence::ChangedRanges(ranges) => Some(ranges),
        _ => None,
    };
    let equal = fsutil::same_entry_observed((parent, c.src(), o), (child, c.path(), n), ranges)?;
    Ok(!equal || (o.is_file() && !stored_same(parent, child, c.path())))
}

/// True when two entries hold the same stored attributes (and ACL on APFS).
fn stored_same(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), path: &str) -> bool {
    match (crate::fs::attributes(parent, path), crate::fs::attributes(child, path)) {
        (Ok(a), Ok(b)) => a.xattrs == b.xattrs && a.acl == b.acl,
        _ => false,
    }
}

/// File level changes (folders dropped), confirmed by content. A move keeps `old_path`.
pub fn file_changes(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<EntryChange>> {
    file_changes_of(parent, child, candidates_equal(parent, child)?)
}

/// A complete object delta and the user-visible changes from the same comparison.
/// Constructed only against an immutable object base and an immutable folder snapshot.
pub struct WriteComparison {
    pub files: Vec<EntryChange>,
    pub changed: Vec<Vec<u8>>,
    pub dirty: Vec<Vec<u8>>,
}

pub fn compare_for_write(parent: &(impl Src + ?Sized), child: &Path) -> Result<WriteComparison> {
    use std::os::unix::fs::MetadataExt;
    let objects = parent.objects().ok_or_else(|| anyhow::anyhow!("comparison requires an object base"))?;
    let child = child.to_path_buf();
    // Include every previously linked path even when bytes are equal: native inode numbers
    // cannot be compared with object link identities, and a group may have split or joined.
    let linked: HashSet<Vec<u8>> = objects.store.get_tree(&objects.refs.tree)?.links.into_iter().flatten().collect();
    let mut out = WriteComparison { files: Vec::new(), changed: Vec::new(), dirty: Vec::new() };
    for c in object_change_candidates(parent, &child)? {
        let both_dirs = c.old().is_some_and(Meta::is_dir) && c.new().is_some_and(Meta::is_dir);
        let metadata_changed = match (c.old(), c.new()) {
            (Some(old), Some(_)) => {
                let path = child.at(c.path())?;
                let meta = std::fs::symlink_metadata(&path)?;
                let previous = objects.entry(c.path())?.ok_or_else(|| anyhow::anyhow!("object path missing"))?;
                (!meta.file_type().is_symlink() && old.mode != meta.mode() & 0o7777)
                    || (meta.file_type().is_symlink() && previous.entry.xattrs != layr_objects::fsx::xattrs(&path)?)
                    || previous.ns != layr_objects::times::ns_of(&meta)
                    || (meta.is_file() && (meta.nlink() > 1 || linked.contains(c.path().as_bytes())))
            }
            _ => true,
        };
        let path = c.path().clone();
        let changed = file_change(parent, &child, c)?;
        if metadata_changed || changed.is_some() {
            if both_dirs {
                out.dirty.push(path.into_bytes());
            } else {
                out.changed.push(path.into_bytes());
            }
        }
        out.files.extend(changed);
    }
    Ok(out)
}

/// `file_changes`, and the files whose metadata changed while their content stayed equal (a new
/// modification time, for example). A store push keeps their new times (`Tree::times`).
pub fn file_changes_and_touched(
    parent: &(impl Src + ?Sized),
    child: &(impl Src + ?Sized),
) -> Result<(Vec<EntryChange>, Vec<String>)> {
    let list = candidates_equal(parent, child)?;
    let files = |m: Option<&fsutil::Meta>| m.is_some_and(|m| !m.is_dir());
    let both: Vec<String> = list.iter().filter(|c| files(c.old()) && files(c.new())).map(|c| c.path().clone()).collect();
    let changes = file_changes_of(parent, child, list)?;
    let changed: std::collections::HashSet<&str> = changes.iter().map(|c| c.path().as_str()).collect();
    let touched = both.into_iter().filter(|p| !changed.contains(p.as_str())).collect();
    Ok((changes, touched))
}

/// File level changes of a candidate list (`file_changes`).
pub fn file_changes_of(
    parent: &(impl Src + ?Sized),
    child: &(impl Src + ?Sized),
    list: Vec<EntryChange>,
) -> Result<Vec<EntryChange>> {
    list.into_iter().filter_map(|c| file_change(parent, child, c).transpose()).collect()
}

/// Stop at the first confirmed change outside the skipped paths.
pub fn any_file_change(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), skip: impl Fn(&str) -> bool) -> Result<bool> {
    for c in candidates_equal(parent, child)? {
        if !skip(c.path()) && file_change(parent, child, c)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_change(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), c: EntryChange) -> Result<Option<EntryChange>> {
    // Directories appear only when their stored attributes changed.
    if c.old().is_none_or(Meta::is_dir) && c.new().is_none_or(Meta::is_dir) {
        return Ok(matches!(&c, EntryChange::Modify { .. }).then_some(c).filter(|c| !stored_same(parent, child, c.path())));
    }
    // Report the non-directory side of a type replacement, as before.
    let c = match c {
        EntryChange::ReplaceType { path, old, new } if old.is_dir() => EntryChange::Add { path, new },
        EntryChange::ReplaceType { path, old, new } if new.is_dir() => EntryChange::Remove { path, old },
        c => c,
    };
    if c.old().is_some() && c.new().is_some() && !really_changed(parent, child, &c)? {
        return Ok(None);
    }
    Ok(Some(c))
}

/// Split moves into a delete and an add (git shows unstaged moves this way).
pub fn without_moves(v: Vec<EntryChange>) -> Vec<EntryChange> {
    let mut out = BTreeMap::new();
    for c in v {
        match c {
            EntryChange::Rename { path, old_path, old, new, .. } => {
                out.insert(old_path.clone(), EntryChange::Remove { path: old_path, old });
                out.insert(path.clone(), EntryChange::Add { path, new });
            }
            c => {
                out.insert(c.path().clone(), c);
            }
        }
    }
    out.into_values().collect()
}

/// Pair deleted and added files as moves when the content is equal, then (for small lists)
/// when at least half of the lines are equal, as git's rename detection does.
pub fn with_renames(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized), v: Vec<EntryChange>) -> Vec<EntryChange> {
    // Files larger than MAX_TEXT are not compared by content.
    let is_file =
        |m: Option<&Meta>| m.map(|x| x.ftype == crate::fsutil::FType::File && x.size <= crate::fsutil::MAX_TEXT).unwrap_or(false);
    let dels: Vec<usize> = v
        .iter()
        .enumerate()
        .filter(|(_, c)| c.new().is_none() && c.old_path().is_none() && is_file(c.old()))
        .map(|(i, _)| i)
        .collect();
    let adds: Vec<usize> = v.iter().enumerate().filter(|(_, c)| c.old().is_none() && is_file(c.new())).map(|(i, _)| i).collect();
    let read_parent = |p: &str| crate::fsutil::read_file(parent, p).ok();
    let read_child = |p: &str| crate::fsutil::read_file(child, p).ok();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut used_d: FastSet<usize> = FastSet::default();
    let mut used_a: FastSet<usize> = FastSet::default();
    // Exact: same size and same bytes. Each candidate is read and hashed once; the chosen pair
    // is compared byte by byte.
    let hash = |bytes: &[u8]| <FastCryptographic as PurposeHasher>::hash(bytes);
    let mut by_key: FastMap<(u64, [u8; 32]), Vec<usize>> = FastMap::default();
    let del_sizes: HashSet<u64> = dels.iter().filter_map(|&d| v[d].old().map(|m| m.size)).collect();
    let add_sizes: HashSet<u64> = adds.iter().filter_map(|&a| v[a].new().map(|m| m.size)).collect();
    for &d in &dels {
        let Some(old) = v[d].old() else { continue };
        let size = old.size;
        if add_sizes.contains(&size) {
            if let Some(x) = read_parent(v[d].path()) {
                by_key.entry((size, hash(&x))).or_default().push(d);
            }
        }
    }
    for &a in &adds {
        let Some(new) = v[a].new() else { continue };
        let size = new.size;
        if !del_sizes.contains(&size) {
            continue;
        }
        let Some(ad) = read_child(v[a].path()) else { continue };
        let Some(ds) = by_key.get(&(size, hash(&ad))) else { continue };
        for &d in ds {
            if !used_d.contains(&d) && read_parent(v[d].path()).as_ref() == Some(&ad) {
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
            let dd = match read_parent(v[d].path()) {
                Some(x) if !crate::fsutil::is_binary(&x) => x,
                _ => continue,
            };
            for &a in &ra {
                let ad = match read_child(v[a].path()) {
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
    // Symlinks are moves only when their targets match exactly.
    for (d, deleted) in v.iter().enumerate() {
        if !deleted.old().is_some_and(|m| m.ftype == crate::fsutil::FType::Symlink) || deleted.new().is_some() {
            continue;
        }
        let Ok(target) = crate::fsutil::read_link_bytes(parent, deleted.path()) else { continue };
        for (a, added) in v.iter().enumerate() {
            if !used_a.contains(&a)
                && added.old().is_none()
                && added.new().is_some_and(|m| m.ftype == crate::fsutil::FType::Symlink)
                && crate::fsutil::read_link_bytes(child, added.path()).ok().as_ref() == Some(&target)
            {
                pairs.push((d, a));
                used_d.insert(d);
                used_a.insert(a);
                break;
            }
        }
    }
    let mut out: Vec<EntryChange> = Vec::new();
    for (d, a) in &pairs {
        if let (EntryChange::Remove { path: old_path, old }, EntryChange::Add { path, new }) = (&v[*d], &v[*a]) {
            out.push(EntryChange::Rename {
                path: path.clone(),
                old_path: old_path.clone(),
                old: old.clone(),
                new: new.clone(),
                data: DataEvidence::Unknown,
            });
        }
    }
    for (i, c) in v.into_iter().enumerate() {
        if !used_d.contains(&i) && !used_a.contains(&i) {
            out.push(c);
        }
    }
    out.sort_by(|a, b| a.path().cmp(b.path()));
    out
}

fn split(d: &[u8]) -> Vec<&[u8]> {
    d.split_inclusive(|b| *b == b'\n').collect()
}

/// Paths needed for an object import, including time-only changes. A directory whose
/// only change is its time is dirty, but does not need a full subtree import.
pub fn object_candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<(Vec<Vec<u8>>, Vec<Vec<u8>>)> {
    let candidates = candidates_request(parent, child, ChangeRequest::default())?;
    let (mut changed, mut dirty) = (Vec::new(), Vec::new());
    for change in candidates {
        if change.old_path().is_some() || change.old().is_none() || change.new().is_none() {
            changed.push(change.path().as_bytes().to_vec());
            if let Some(old) = change.old_path() {
                changed.push(old.as_bytes().to_vec());
            }
            continue;
        }
        let a = crate::fs::metadata(parent, change.src())?;
        let b = crate::fs::metadata(child, change.path())?;
        let same_attributes = a.attributes.mode == b.attributes.mode
            && a.attributes.xattrs == b.attributes.xattrs
            && a.attributes.acl == b.attributes.acl;
        let same_time = a.attributes.stored_time.is_none() && b.attributes.stored_time.is_none()
            || a.attributes.modified_ns == b.attributes.modified_ns;
        if a.kind == fsutil::FType::Dir && b.kind == fsutil::FType::Dir && same_attributes {
            if !same_time {
                dirty.push(change.path().as_bytes().to_vec());
            }
            continue;
        }
        let linked = a.link.is_some_and(|link| link.count > 1) || b.link.is_some_and(|link| link.count > 1);
        if !linked && a.kind == b.kind && same_attributes && same_time && !really_changed(parent, child, &change)? {
            continue;
        }
        changed.push(change.path().as_bytes().to_vec());
    }
    changed.sort();
    changed.dedup();
    dirty.sort();
    dirty.dedup();
    Ok((changed, dirty))
}

/// Confirm complete persisted-state differences, including nanosecond timestamps and stored
/// attributes. This is shared by sparse representation writers and portable transfer plans.
pub(crate) fn state_changes(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<EntryChange>> {
    let candidates = without_moves(candidates_request(
        parent,
        child,
        ChangeRequest { scope: ChangeScope::FullState, detail: ChangeDetail::Equality },
    )?);
    let mut changes = Vec::new();
    for candidate in candidates {
        let (Some(_), Some(_)) = (candidate.old(), candidate.new()) else {
            changes.push(candidate);
            continue;
        };
        let before = crate::fs::metadata(parent, candidate.path())?;
        let after = crate::fs::metadata(child, candidate.path())?;
        if before.kind != after.kind || before.attributes != after.attributes || really_changed(parent, child, &candidate)? {
            changes.push(candidate);
        }
    }
    Ok(changes)
}

fn candidates_equal(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<EntryChange>> {
    candidates_request(parent, child, ChangeRequest { scope: ChangeScope::Stored, detail: ChangeDetail::Equality })
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

    #[test]
    fn any_change_stops_before_later_content_and_skips_before_comparing() -> Result<()> {
        let tmp = std::env::temp_dir().join(format!("layr-any-change-{}", crate::ids::new_id()));
        let native = tmp.join("native");
        std::fs::create_dir_all(&native)?;
        std::fs::write(native.join("a"), b"first")?;
        std::fs::write(native.join("z"), b"later")?;
        std::fs::create_dir(native.join("dir"))?;
        let store = layr_objects::Store::open(&tmp.join("objects"), layr_objects::Keying::Plain)?;
        let report = layr_objects::write::write_tree(
            &store,
            &native,
            &layr_objects::WriteOptions {
                temperature: layr_objects::Temperature::Hot,
                previous: None,
                exclude: Vec::new(),
                similar: false,
            },
        )?;
        let tree = crate::object_tree::ObjectTree::new(
            store,
            layr_objects::StateRef { tree: report.tree.unwrap(), times: report.times.unwrap() },
            &tmp.join("reads"),
        )?;
        struct Reads(PathBuf, std::sync::Mutex<Vec<String>>);
        impl Src for Reads {
            fn at(&self, rel: &str) -> Result<PathBuf> {
                self.1.lock().unwrap().push(rel.to_owned());
                fsutil::safe_join(&self.0, rel)
            }
            fn folder(&self) -> Option<&Path> {
                Some(&self.0)
            }
        }
        let child = Reads(native.clone(), Default::default());
        #[cfg(target_os = "macos")]
        let parent = crate::thin::Layered::objects(tree.clone());
        #[cfg(not(target_os = "macos"))]
        let parent = crate::repo::Snap::Objects(tree.clone());
        assert!(!any_file_change(&parent, &child, |path| path == "z")?);
        assert!(!child.1.lock().unwrap().iter().any(|p| p == "z"), "skipped paths are never compared");
        assert!(!any_file_change(&parent, &child, |_| false)?);
        assert!(child.1.lock().unwrap().iter().any(|p| p == "z"), "unchanged candidates need comparison");
        child.1.lock().unwrap().clear();
        std::fs::write(native.join("a"), b"other")?;
        assert!(any_file_change(&parent, &child, |_| false)?);
        assert!(!child.1.lock().unwrap().iter().any(|p| p == "z"), "later candidates are never compared");
        assert_eq!(file_changes(&parent, &child)?.len(), 1);
        assert!(child.1.lock().unwrap().iter().any(|p| p == "z"));
        std::fs::write(native.join("a"), b"first")?;
        layr_objects::fsx::set_xattrs(&native.join("dir"), &[(b"user.test".to_vec(), b"changed".to_vec())])?;
        assert!(any_file_change(&parent, &child, |_| false)?, "directory attributes count as a change");
        assert!(!any_file_change(&parent, &child, |p| p == "dir")?);
        #[cfg(target_os = "linux")]
        assert_eq!(std::fs::read_dir(tree.root())?.count(), 0, "metadata comparisons do not materialize entries");
        drop(parent);
        drop(tree);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

    #[test]
    fn file_candidate_normalization_keeps_file_sides_and_moves() -> Result<()> {
        let root = Path::new("/");
        let dir = Meta { ftype: fsutil::FType::Dir, ..meta(0) };
        let c =
            file_change(root, root, EntryChange::ReplaceType { path: "child".into(), old: dir.clone(), new: meta(1) })?.unwrap();
        assert!(matches!(c, EntryChange::Add { .. }));
        let c =
            file_change(root, root, EntryChange::ReplaceType { path: "child".into(), old: meta(1), new: dir.clone() })?.unwrap();
        assert!(matches!(c, EntryChange::Remove { .. }));
        assert!(file_change(root, root, EntryChange::Add { path: "child".into(), new: dir })?.is_none());
        let c = file_change(
            root,
            root,
            EntryChange::Rename {
                path: "child".into(),
                old_path: "parent".into(),
                old: meta(1),
                new: meta(1),
                data: DataEvidence::Unknown,
            },
        )?
        .unwrap();
        assert_eq!(c.old_path().map(String::as_str), Some("parent"));
        Ok(())
    }

    #[test]
    fn equal_data_does_not_hide_length_or_attribute_changes() -> Result<()> {
        let tmp = std::env::temp_dir().join(format!("layr-equal-data-{}", crate::ids::new_id()));
        let (parent, child) = (tmp.join("parent"), tmp.join("child"));
        std::fs::create_dir_all(&parent)?;
        std::fs::create_dir_all(&child)?;
        std::fs::write(parent.join("file"), b"a")?;
        std::fs::write(child.join("file"), b"ab")?;
        let change = || {
            EntryChange::observed(
                "file".into(),
                fsutil::lmeta(&parent, "file"),
                fsutil::lmeta(&child, "file"),
                DataEvidence::Equal,
            )
            .unwrap()
        };
        assert!(really_changed(&parent, &child, &change())?);
        std::fs::write(child.join("file"), b"a")?;
        assert!(!really_changed(&parent, &child, &change())?);
        layr_objects::fsx::set_xattrs(&child.join("file"), &[(b"user.test".to_vec(), b"value".to_vec())])?;
        assert!(really_changed(&parent, &child, &change())?);
        std::fs::remove_dir_all(tmp)?;
        Ok(())
    }

    #[test]
    fn inaccessible_metadata_is_not_treated_as_equal() {
        let root = Counting(PathBuf::from("/"), AtomicUsize::new(0));
        assert!(!stored_same(&root, &root, "../outside"));
    }

    #[test]
    fn a_new_hard_link_includes_its_existing_source() {
        let tmp = std::env::temp_dir().join(format!("layr-link-candidates-{}", crate::ids::new_id()));
        let (parent, child) = (tmp.join("parent"), tmp.join("child"));
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(parent.join("source"), "data").unwrap();
        std::fs::write(child.join("source"), "data").unwrap();
        std::fs::hard_link(child.join("source"), child.join("link")).unwrap();
        let entry = crate::stream::Entry { path: Some("link".into()), linked_from: Some("source".into()), ..Default::default() };
        let result = crate::fs_candidates::build(
            &layr_fs_unix::DirectoryTree::open(&parent).unwrap(),
            &layr_fs_unix::DirectoryTree::open(&child).unwrap(),
            vec![entry],
        )
        .unwrap();
        assert!(result.iter().any(|c| c.path() == "source"));
        assert!(result.iter().any(|c| c.path() == "link"));
        std::fs::remove_dir_all(tmp).unwrap();
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
            changes.push(EntryChange::Remove { path: format!("old{i}"), old: meta(8) });
            changes.push(EntryChange::Add { path: format!("new{i}"), new: meta(8) });
        }
        let (parent, child) = (Counting(p, AtomicUsize::new(0)), Counting(c, AtomicUsize::new(0)));
        let out = with_renames(&parent, &child, changes);
        let moves = out.iter().filter(|c| c.old_path().is_some()).count();
        assert_eq!(moves, n, "every file pairs with its equal");
        let reads = parent.1.load(Ordering::Relaxed) + child.1.load(Ordering::Relaxed);
        assert!(reads <= 4 * n + 2, "{reads} reads for {n} + {n} candidates");
        std::fs::remove_dir_all(tmp).unwrap();
    }
}

/// Object trees have no native inode stream. Enumerate their entries and let the
/// content comparison reject unchanged candidates without a checkout.
fn object_change_candidates(parent: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<EntryChange>> {
    let objects = parent.objects().zip(child.objects());
    let prunable = objects.filter(|(a, b)| a.same_links(b));
    if let Some((a, b)) = prunable {
        if a.same_subtree(b, "")? {
            return Ok(Vec::new());
        }
    }
    let mut entries: BTreeMap<String, (Option<Meta>, Option<Meta>)> = BTreeMap::new();
    let mut pruned = HashSet::new();
    fsutil::walk(parent, "", &mut |path, meta| {
        if meta.is_dir() {
            if let Some((a, b)) = prunable {
                if a.same_subtree(b, path)? {
                    pruned.insert(path.to_owned());
                    return Ok(false);
                }
            }
        }
        entries.entry(path.to_owned()).or_default().0 = Some(meta.clone());
        Ok(true)
    })?;
    fsutil::walk(child, "", &mut |path, meta| {
        if pruned.contains(path) {
            return Ok(false);
        }
        entries.entry(path.to_owned()).or_default().1 = Some(meta.clone());
        Ok(true)
    })?;
    entries
        .into_iter()
        .map(|(path, (old, new))| {
            let data = if old.as_ref().is_some_and(Meta::is_file) && new.as_ref().is_some_and(Meta::is_file) {
                match objects {
                    Some((a, b)) if a.entry(&path)?.zip(b.entry(&path)?).is_some_and(|(a, b)| a.entry.node == b.entry.node) => {
                        DataEvidence::Equal
                    }
                    _ => DataEvidence::Unknown,
                }
            } else {
                DataEvidence::Unknown
            };
            Ok(EntryChange::observed(path, old, new, data))
        })
        .filter_map(|result| result.transpose())
        .collect()
}

#[cfg(test)]
#[path = "compare_source_tests.rs"]
mod source_tests;
