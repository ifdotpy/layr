//! The in-memory index of one working tree: every entry below the root, and the files that
//! ripgrep would search from the root (its ignore rules: `.gitignore`, `.ignore`, `.rgignore`,
//! `.git/info/exclude`, the global git excludes and hidden names).
//!
//! Keys are root-relative paths whose separators are stored as NUL bytes, so that the byte order
//! of a `BTreeMap` is the component order of `rg --sort path` and of a sorted directory walk
//! (`a/b` sorts before `a.b`, as the directory `a` sorts before the file `a.b`).

use std::collections::{BTreeMap, BTreeSet};
#[allow(unused_imports)]
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The separator inside a key.
pub const SEP: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Link,
    Other,
}

impl Kind {
    fn of(ft: std::fs::FileType) -> Kind {
        if ft.is_symlink() {
            Kind::Link
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }
}

#[derive(Default)]
pub struct Tree {
    /// Every entry below the root (the root itself is not a key).
    pub all: BTreeMap<Vec<u8>, Kind>,
    /// The files ripgrep searches when it walks the root.
    pub visible: BTreeSet<Vec<u8>>,
    /// The folders ripgrep descends into. A new path whose parent is not here cannot be
    /// visible, which saves a walk for each file written below an ignored folder (`target/`).
    pub walked: BTreeSet<Vec<u8>>,
    /// Modification times of every folder (the root is the empty key) and every ignore file. A
    /// creation, removal or rename changes the time of its folder, so comparing these finds
    /// every change that the watcher has not delivered yet (`stale`).
    pub stamps: BTreeMap<Vec<u8>, Stamp>,
}

/// A modification time: seconds and nanoseconds.
pub type Stamp = (i64, i64);

fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    (meta.mtime(), meta.mtime_nsec())
}

/// Files whose content is ignore rules.
fn is_rules(name: &[u8]) -> bool {
    matches!(name, b".gitignore" | b".ignore" | b".rgignore")
}

/// The last component of a key.
fn key_name(key: &[u8]) -> &[u8] {
    key.rsplit(|&b| b == SEP).next().unwrap_or(key)
}

/// A folder or ignore file whose time differs from the index.
pub struct Stale {
    key: Vec<u8>,
    now: Option<Stamp>,
}

/// The key of a root-relative path (`a/b` becomes `a\0b`).
pub fn key(rel: &Path) -> Vec<u8> {
    rel.as_os_str().as_bytes().iter().map(|&b| if b == b'/' { SEP } else { b }).collect()
}

/// The root-relative path of a key.
pub fn rel_path(key: &[u8]) -> PathBuf {
    let bytes: Vec<u8> = key.iter().map(|&b| if b == SEP { b'/' } else { b }).collect();
    PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes))
}

/// The keys below `dir` (exclusive); `dir` empty means the whole tree.
pub fn below<'a, V>(map: &'a BTreeMap<Vec<u8>, V>, dir: &[u8]) -> impl Iterator<Item = (&'a Vec<u8>, &'a V)> + 'a {
    let (lo, hi) = bounds(dir);
    map.range(lo..hi)
}

pub fn below_set<'a>(set: &'a BTreeSet<Vec<u8>>, dir: &[u8]) -> impl Iterator<Item = &'a Vec<u8>> + 'a {
    let (lo, hi) = bounds(dir);
    set.range(lo..hi)
}

fn bounds(dir: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if dir.is_empty() {
        return (Vec::new(), vec![0xff; 4096]);
    }
    let mut lo = dir.to_vec();
    lo.push(SEP);
    let mut hi = dir.to_vec();
    hi.push(SEP + 1);
    (lo, hi)
}

/// Items of one walker thread, moved to the shared list in batches and when the thread ends.
struct Batch<'a, T> {
    out: &'a Mutex<Vec<T>>,
    items: Vec<T>,
}

impl<T> Batch<'_, T> {
    fn push(&mut self, item: T) {
        self.items.push(item);
        if self.items.len() >= 4096 {
            self.out.lock().unwrap().append(&mut self.items);
        }
    }
}

impl<T> Drop for Batch<'_, T> {
    fn drop(&mut self) {
        self.out.lock().unwrap().append(&mut self.items);
    }
}

/// A walker over `root` with ripgrep's default filters.
fn rg_walker(root: &Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(root);
    b.add_custom_ignore_filename(".rgignore");
    b
}

/// A walker over `root` without any filter.
fn raw_walker(root: &Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(root);
    b.standard_filters(false);
    b
}

impl Tree {
    /// Build the index of `root` with parallel walks.
    pub fn build(root: &Path) -> Tree {
        let all = Mutex::new(Vec::new());
        let stamps = Mutex::new(Vec::new());
        raw_walker(root).build_parallel().run(|| {
            let mut local = Batch { out: &all, items: Vec::new() };
            let mut times = Batch { out: &stamps, items: Vec::new() };
            Box::new(move |e| {
                if let Ok(e) = e {
                    if let (Ok(rel), Some(ft)) = (e.path().strip_prefix(root), e.file_type()) {
                        let k = key(rel);
                        if ft.is_dir() || is_rules(key_name(&k)) {
                            if let Ok(meta) = e.metadata() {
                                times.push((k.clone(), stamp_of(&meta)));
                            }
                        }
                        if e.depth() > 0 {
                            local.push((k, Kind::of(ft)));
                        }
                    }
                }
                ignore::WalkState::Continue
            })
        });
        let found = Mutex::new(Vec::new());
        rg_walker(root).build_parallel().run(|| {
            let mut local = Batch { out: &found, items: Vec::new() };
            Box::new(move |e| {
                if let Ok(e) = e {
                    if let (Some(ft), Ok(rel)) = (e.file_type(), e.path().strip_prefix(root)) {
                        if ft.is_file() {
                            local.push((key(rel), true));
                        } else if ft.is_dir() && e.depth() > 0 {
                            local.push((key(rel), false));
                        }
                    }
                }
                ignore::WalkState::Continue
            })
        });
        let (mut visible, mut walked) = (BTreeSet::new(), BTreeSet::new());
        for (k, file) in found.into_inner().unwrap() {
            if file {
                visible.insert(k);
            } else {
                walked.insert(k);
            }
        }
        Tree {
            all: all.into_inner().unwrap().into_iter().collect(),
            visible,
            walked,
            stamps: stamps.into_inner().unwrap().into_iter().collect(),
        }
    }

    /// The folders and ignore files that changed since the index saw them: every folder in
    /// `scope` (a folder key) and below it, and the ignore files of its ancestors. A query calls
    /// this first; the index then needs only `apply_stale`.
    pub fn stale(&self, root: &Path, scope: &[u8]) -> Vec<Stale> {
        use rayon::prelude::*;
        let mut keys: Vec<&Vec<u8>> = below(&self.stamps, scope).map(|(k, _)| k).collect();
        let mut extra: Vec<Vec<u8>> = vec![scope.to_vec()];
        // The ignore files of the scope's ancestors, and `.git/info/exclude`, apply below it.
        let mut ancestor: Vec<u8> = Vec::new();
        let parts: Vec<&[u8]> = if scope.is_empty() { Vec::new() } else { scope.split(|&b| b == SEP).collect() };
        for i in 0..=parts.len() {
            for name in [&b".gitignore"[..], b".ignore", b".rgignore"] {
                let mut k = ancestor.clone();
                if !k.is_empty() {
                    k.push(SEP);
                }
                k.extend_from_slice(name);
                extra.push(k);
            }
            if i < parts.len() {
                if !ancestor.is_empty() {
                    ancestor.push(SEP);
                }
                ancestor.extend_from_slice(parts[i]);
            }
        }
        extra.push(key(Path::new(".git/info/exclude")));
        keys.extend(extra.iter());
        keys.sort();
        keys.dedup();
        // A folder changed within the last two seconds may change again within the resolution of
        // its time stamp (a coarse clock on Linux), so it is listed again until the time is old
        // (git's rule for racy timestamps).
        let clock =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(i64::MAX);
        keys.par_iter()
            .filter_map(|k| {
                let now = std::fs::symlink_metadata(root.join(rel_path(k))).ok().map(|m| stamp_of(&m));
                let racy = now.is_some_and(|n| clock - n.0 < 2);
                (racy || now != self.stamps.get(*k).copied()).then(|| Stale { key: (*k).clone(), now })
            })
            .collect()
    }

    /// Bring the index up to date for the changes that `stale` found.
    pub fn apply_stale(&mut self, root: &Path, stale: Vec<Stale>) {
        let mut changed: Vec<PathBuf> = Vec::new();
        for s in &stale {
            let rel = rel_path(&s.key);
            let was_dir = s.key.is_empty() || self.all.get(&s.key) == Some(&Kind::Dir);
            if is_rules(key_name(&s.key)) || s.key == key(Path::new(".git/info/exclude")) {
                changed.push(rel); // `refresh` maps it to its folder or the whole tree
                continue;
            }
            if s.now.is_none() || !was_dir {
                changed.push(rel); // removed, or a new folder: refresh it whole
                continue;
            }
            // A folder whose entries changed: compare its listing with the index.
            let Ok(entries) = std::fs::read_dir(root.join(&rel)) else {
                changed.push(rel);
                continue;
            };
            let mut on_disk: BTreeMap<Vec<u8>, Kind> = BTreeMap::new();
            for e in entries.flatten() {
                if let Ok(ft) = e.file_type() {
                    on_disk.insert(e.file_name().as_bytes().to_vec(), Kind::of(ft));
                }
            }
            let skip = if s.key.is_empty() { 0 } else { s.key.len() + 1 };
            let in_index: BTreeMap<Vec<u8>, Kind> = below(&self.all, &s.key)
                .filter(|(k, _)| !k[skip..].contains(&SEP))
                .map(|(k, v)| (k[skip..].to_vec(), *v))
                .collect();
            for (name, kind) in &on_disk {
                if in_index.get(name) != Some(kind) {
                    changed.push(rel.join(std::ffi::OsStr::from_bytes(name)));
                }
            }
            for name in in_index.keys() {
                if !on_disk.contains_key(name) {
                    changed.push(rel.join(std::ffi::OsStr::from_bytes(name)));
                }
            }
        }
        if !changed.is_empty() {
            self.refresh(root, &changed);
        }
        for s in stale {
            match s.now {
                Some(now) => {
                    self.stamps.insert(s.key, now);
                }
                None => {
                    self.stamps.remove(&s.key);
                }
            }
        }
    }

    /// Bring the index up to date for the changed paths (root-relative). A changed ignore file
    /// refreshes its folder; a changed `.git/info/exclude` refreshes the whole tree.
    pub fn refresh(&mut self, root: &Path, changed: &[PathBuf]) {
        let mut targets: Vec<PathBuf> = Vec::new();
        for rel in changed {
            let name = rel.file_name().map(|n| n.as_bytes()).unwrap_or(b"");
            if rel == Path::new(".git/info/exclude") || rel.as_os_str().is_empty() {
                *self = Tree::build(root);
                return;
            }
            if matches!(name, b".gitignore" | b".ignore" | b".rgignore") {
                targets.push(rel.parent().map(Path::to_path_buf).unwrap_or_default());
            } else {
                targets.push(rel.clone());
            }
        }
        targets.sort();
        targets.dedup();
        // A path below another target is covered by that target's refresh.
        let mut kept: Vec<PathBuf> = Vec::new();
        for t in targets {
            if kept.iter().any(|k| k.as_os_str().is_empty() || t.starts_with(k)) {
                continue;
            }
            kept.push(t);
        }
        for t in kept {
            if t.as_os_str().is_empty() {
                *self = Tree::build(root);
                return;
            }
            self.refresh_one(root, &t);
        }
    }

    fn refresh_one(&mut self, root: &Path, rel: &Path) {
        let k = key(rel);
        // Forget the path and everything below it.
        let stale: Vec<Vec<u8>> = below(&self.all, &k).map(|(k, _)| k.clone()).collect();
        for s in stale {
            self.all.remove(&s);
        }
        self.all.remove(&k);
        let stale: Vec<Vec<u8>> = below(&self.stamps, &k).map(|(k, _)| k.clone()).collect();
        for s in stale {
            self.stamps.remove(&s);
        }
        self.stamps.remove(&k);
        // The parent's time is not updated here: other entries of the parent may have changed
        // too, and only a listing of the parent (`apply_stale`) may mark it seen.
        for set in [&mut self.visible, &mut self.walked] {
            let stale: Vec<Vec<u8>> = below_set(set, &k).cloned().collect();
            for s in stale {
                set.remove(&s);
            }
            set.remove(&k);
        }
        let abs = root.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            return; // removed
        };
        // Parents may be new as well (a refresh of `a/b/c` after `mkdir -p a/b`).
        let mut parent = rel.parent();
        while let Some(p) = parent {
            if p.as_os_str().is_empty() {
                break;
            }
            self.all.entry(key(p)).or_insert(Kind::Dir);
            parent = p.parent();
        }
        let kind = Kind::of(meta.file_type());
        if kind == Kind::Dir || is_rules(key_name(&k)) {
            self.stamps.insert(k.clone(), stamp_of(&meta));
        }
        self.all.insert(k, kind);
        if kind == Kind::Dir {
            for e in raw_walker(&abs).build().flatten() {
                if e.depth() == 0 {
                    continue;
                }
                if let (Ok(r), Some(ft)) = (e.path().strip_prefix(root), e.file_type()) {
                    let rk = key(r);
                    if ft.is_dir() || is_rules(key_name(&rk)) {
                        if let Ok(m) = e.metadata() {
                            self.stamps.insert(rk.clone(), stamp_of(&m));
                        }
                    }
                    self.all.insert(rk, Kind::of(ft));
                }
            }
        }
        // Below a folder that ripgrep does not enter, nothing is visible.
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            if !self.walked.contains(&key(parent)) {
                return;
            }
        }
        // Visibility of the path and its subtree: walk from the root, descending only through
        // the ancestors of the path, so that every ignore file on the way applies.
        let target = abs.clone();
        let mut walk = rg_walker(root);
        walk.filter_entry(move |e| {
            let p = e.path();
            target.starts_with(p) || p.starts_with(&target)
        });
        for e in walk.build().flatten() {
            if !e.path().starts_with(&abs) {
                continue;
            }
            if let (Some(ft), Ok(r)) = (e.file_type(), e.path().strip_prefix(root)) {
                if ft.is_file() {
                    self.visible.insert(key(r));
                } else if ft.is_dir() {
                    self.walked.insert(key(r));
                }
            }
        }
    }

    /// Bytes of keys held, an estimate of the memory use.
    pub fn key_bytes(&self) -> usize {
        self.all.keys().map(|k| k.len() + 48).sum::<usize>() + self.visible.iter().map(|k| k.len() + 40).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    fn visible(t: &Tree) -> Vec<String> {
        t.visible.iter().map(|k| rel_path(k).display().to_string()).collect()
    }

    #[test]
    fn keys_sort_in_component_order() {
        let mut v = vec![key(Path::new("a.b")), key(Path::new("a/b")), key(Path::new("a"))];
        v.sort();
        let names: Vec<String> = v.iter().map(|k| rel_path(k).display().to_string()).collect();
        assert_eq!(names, ["a", "a/b", "a.b"]);
    }

    #[test]
    fn build_and_refresh_follow_ignore_rules() {
        let d = tempfile_dir();
        let root = d.as_path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root.join("src/a.rs"), "fn a");
        write(&root.join("b.log"), "x");
        write(&root.join(".hidden/h.rs"), "h");
        write(&root.join(".gitignore"), "*.log\n");
        let mut t = Tree::build(root);
        // Hidden names (`.gitignore`, `.hidden/`) and ignored files are not searched by rg.
        assert_eq!(visible(&t), ["src/a.rs"]);
        assert_eq!(t.all.get(&key(Path::new("b.log"))), Some(&Kind::File));

        // A new folder with a file, a removed file, and a changed ignore file.
        write(&root.join("new/deep/c.rs"), "c");
        std::fs::remove_file(root.join("src/a.rs")).unwrap();
        write(&root.join(".gitignore"), "*.rs\n");
        t.refresh(root, &[PathBuf::from("new/deep/c.rs"), PathBuf::from("src/a.rs"), PathBuf::from(".gitignore")]);
        assert_eq!(visible(&t), ["b.log"]);
        assert_eq!(t.all.get(&key(Path::new("new"))), Some(&Kind::Dir));
        assert_eq!(t.all.get(&key(Path::new("src/a.rs"))), None);
        let fresh = Tree::build(root);
        assert_eq!(fresh.visible, t.visible);
        assert_eq!(fresh.walked, t.walked);
        assert_eq!(fresh.all, t.all);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_finds_changes_without_a_watcher() {
        let d = tempfile_dir();
        let root = d.as_path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root.join("src/a.rs"), "a");
        write(&root.join("src/old.rs"), "o");
        write(&root.join(".gitignore"), "*.log\n");
        // Folders changed in the last two seconds always count as stale (racy times).
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let mut t = Tree::build(root);
        assert!(t.stale(root, b"").is_empty(), "a fresh index is not stale");
        assert!(
            !{
                write(&root.join("src/racy.rs"), "r");
                t.stale(root, b"").is_empty()
            },
            "a folder changed just now is stale"
        );
        std::fs::remove_file(root.join("src/racy.rs")).unwrap();
        write(&root.join("src/new/b.rs"), "b");
        std::fs::remove_file(root.join("src/old.rs")).unwrap();
        write(&root.join("x.log"), "x");
        write(&root.join(".gitignore"), "*.rs\n");
        let stale = t.stale(root, &key(Path::new("src")));
        assert!(!stale.is_empty());
        t.apply_stale(root, stale);
        let all = t.stale(root, b"");
        t.apply_stale(root, all);
        let fresh = Tree::build(root);
        assert_eq!(visible(&t), visible(&fresh));
        assert_eq!(t.all, fresh.all);
        assert_eq!(t.walked, fresh.walked);
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert!(t.stale(root, b"").is_empty(), "applied changes leave nothing stale");
        let _ = std::fs::remove_dir_all(root);
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("layrfs-index-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }
}
