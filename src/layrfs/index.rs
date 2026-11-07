//! The in-memory index of one working tree: every entry below the root, and the files that
//! ripgrep would search from the root (its ignore rules: `.gitignore`, `.ignore`, `.rgignore`,
//! `.git/info/exclude`, the global git excludes and hidden names).
//!
//! Keys are root-relative paths whose separators are stored as NUL bytes, so that the byte order
//! of a `BTreeMap` is the component order of `rg --sort path` and of a sorted directory walk
//! (`a/b` sorts before `a.b`, as the directory `a` sorts before the file `a.b`).

use std::collections::{BTreeMap, BTreeSet};
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
        raw_walker(root).build_parallel().run(|| {
            let mut local = Batch { out: &all, items: Vec::new() };
            Box::new(move |e| {
                if let Ok(e) = e {
                    if e.depth() > 0 {
                        if let (Ok(rel), Some(ft)) = (e.path().strip_prefix(root), e.file_type()) {
                            local.push((key(rel), Kind::of(ft)));
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
        Tree { all: all.into_inner().unwrap().into_iter().collect(), visible, walked }
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
        self.all.insert(k, kind);
        if kind == Kind::Dir {
            for e in raw_walker(&abs).build().flatten() {
                if e.depth() == 0 {
                    continue;
                }
                if let (Ok(r), Some(ft)) = (e.path().strip_prefix(root), e.file_type()) {
                    self.all.insert(key(r), Kind::of(ft));
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

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("layrfs-index-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }
}
