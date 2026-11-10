//! `.gitignore` rules, read from one snapshot root. A path below an ignored folder is ignored.
//! `.layrattributes` files give paths attributes with the same patterns: a line is a pattern and
//! its attributes, such as `node_modules regenerable`; `-regenerable` unsets one, and the last
//! matching line wins.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The local ignore file of a working folder: not versioned, read by git and layr. The project
/// setting `ignore.local` fills it in the `.git` folder of each line.
pub const LOCAL_EXCLUDE: &str = ".git/info/exclude";

/// The local ignore file of the line of this command (`set_line_exclude`). A command often scans a
/// snapshot of the line folder, which has no `.git` or an empty one (a nested subvolume on btrfs, the
/// upper folder on OverlayFS), so the rules of a tree without its own `.git/info` read this one.
static LINE_EXCLUDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Name the local ignore file of the line of this command (`LINE_EXCLUDE`).
pub fn set_line_exclude(path: Option<PathBuf>) {
    if let Ok(mut l) = LINE_EXCLUDE.lock() {
        *l = path;
    }
}

pub struct Rules {
    root: PathBuf,
    files: &'static [&'static str],
    /// For `.layrattributes`: the attribute whose patterns this matcher holds.
    attribute: Option<&'static str>,
    cache: RefCell<HashMap<String, Option<Arc<Gitignore>>>>,
    /// `local_exclude`, looked up once.
    local: std::cell::OnceCell<Option<PathBuf>>,
    /// The folders (relative to root, "" is the root) whose rule files are read. None: every folder.
    dirs: Option<std::collections::HashSet<String>>,
    /// The layers of a thin state: rule files are read from the layer that holds them.
    #[cfg(target_os = "macos")]
    layers: Option<crate::thin::Layered>,
    #[cfg(not(target_os = "macos"))]
    source: Option<crate::source::Source>,
}

impl Rules {
    pub fn new(root: &Path) -> Rules {
        Rules {
            root: root.to_path_buf(),
            files: &[".gitignore", ".layrignore"],
            attribute: None,
            cache: RefCell::new(HashMap::new()),
            local: std::cell::OnceCell::new(),
            dirs: None,
            #[cfg(target_os = "macos")]
            layers: None,
            #[cfg(not(target_os = "macos"))]
            source: None,
        }
    }

    /// The ignore rules (`attribute` None) or the rules of `attribute`, read only in the folders
    /// `dirs`. A walk that lists the folders knows which hold rule files, so no other folder costs
    /// a stat for them.
    pub fn limited(root: &Path, attribute: Option<&'static str>, dirs: std::collections::HashSet<String>) -> Rules {
        Rules {
            root: root.to_path_buf(),
            files: if attribute.is_some() { &[".layrattributes"] } else { &[".gitignore", ".layrignore"] },
            attribute,
            cache: RefCell::new(HashMap::new()),
            local: std::cell::OnceCell::new(),
            dirs: Some(dirs),
            #[cfg(target_os = "macos")]
            layers: None,
            #[cfg(not(target_os = "macos"))]
            source: None,
        }
    }

    /// The rules of a tree that hot paths read (`repo::Snap`).
    #[cfg(target_os = "macos")]
    pub fn of(tree: &crate::thin::Layered) -> Rules {
        let mut r = Rules::new(tree.top());
        if !tree.is_complete() {
            r.layers = Some(tree.clone());
        }
        r
    }
    #[cfg(not(target_os = "macos"))]
    pub fn of(tree: &crate::source::Source) -> Rules {
        use crate::fsutil::Src;
        let mut rules = Rules::new(tree.folder().unwrap_or_else(|| tree.objects().unwrap().root()));
        rules.source = Some(tree.clone());
        rules
    }

    fn file(&self, rel: &str) -> PathBuf {
        #[cfg(not(target_os = "macos"))]
        if let Some(source) = &self.source {
            return source.path(rel).unwrap_or_else(|_| self.root.join(rel));
        }
        #[cfg(target_os = "macos")]
        if let Some(l) = &self.layers {
            return l.path(rel).unwrap_or_else(|_| self.root.join(rel));
        }
        #[cfg(target_os = "macos")]
        crate::storage::note_read(&self.root, [rel]);
        self.root.join(rel)
    }

    fn is_file(&self, rel: &str) -> bool {
        #[cfg(not(target_os = "macos"))]
        if let Some(source) = &self.source {
            return crate::fsutil::lmeta(source, rel).map(|m| m.is_file()).unwrap_or(false);
        }
        #[cfg(target_os = "macos")]
        if let Some(l) = &self.layers {
            return crate::fsutil::lmeta(l, rel).map(|m| m.is_file()).unwrap_or(false);
        }
        crate::fsutil::lmeta(&self.root, rel).map(|m| m.is_file()).unwrap_or(false)
    }

    /// The paths that `.layrattributes` files give the attribute `name`: `ignored` is true for them.
    pub fn attribute(root: &Path, name: &'static str) -> Rules {
        Rules {
            root: root.to_path_buf(),
            files: &[".layrattributes"],
            attribute: Some(name),
            cache: RefCell::new(HashMap::new()),
            local: std::cell::OnceCell::new(),
            dirs: None,
            #[cfg(target_os = "macos")]
            layers: None,
            #[cfg(not(target_os = "macos"))]
            source: None,
        }
    }

    /// The local exclude file of the working folder, when it has one (`LOCAL_EXCLUDE`). It is read
    /// from the folder itself: a state, and the layers of one, have no `.git`.
    fn local_exclude(&self) -> Option<&PathBuf> {
        self.local
            .get_or_init(|| {
                // An empty file (each line's `.git` has one) adds no rule and no matcher.
                let rules = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0);
                let own = self.root.join(LOCAL_EXCLUDE);
                // A snapshot keeps a nested `.git` subvolume as an empty folder: only a `.git/info`
                // folder makes the tree's own file the one to read.
                let path = if self.root.join(".git/info").is_dir() {
                    own
                } else {
                    LINE_EXCLUDE.lock().ok().and_then(|l| l.clone()).unwrap_or(own)
                };
                (self.attribute.is_none() && rules(&path)).then_some(path)
            })
            .as_ref()
    }

    fn matcher(&self, dir: &str) -> Option<Arc<Gitignore>> {
        if self.dirs.as_ref().is_some_and(|d| !d.contains(dir)) && !(dir.is_empty() && self.local_exclude().is_some()) {
            return None;
        }
        if let Some(m) = self.cache.borrow().get(dir) {
            return m.clone();
        }
        let base = if dir.is_empty() { self.root.clone() } else { self.root.join(dir) };
        let mut b = GitignoreBuilder::new(&base);
        let mut any = false;
        // The local excludes of a working folder (`.git/info/exclude`), as git reads them: below the
        // root's rule files, which come after them and win. A state has no `.git`.
        if let Some(path) = dir.is_empty().then(|| self.local_exclude()).flatten() {
            b.add(path);
            any = true;
        }
        for name in self.files {
            let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
            if self.is_file(&rel) {
                match self.attribute {
                    None => {
                        b.add(self.file(&rel));
                    }
                    Some(attribute) => {
                        let unset = format!("-{attribute}");
                        for line in std::fs::read_to_string(self.file(&rel)).unwrap_or_default().lines() {
                            let mut words = line.split_whitespace();
                            let pattern = match words.next() {
                                Some(p) if !p.starts_with('#') => p,
                                _ => continue,
                            };
                            for word in words {
                                if word == attribute {
                                    let _ = b.add_line(None, pattern);
                                } else if word == unset {
                                    let _ = b.add_line(None, &format!("!{pattern}"));
                                }
                            }
                        }
                    }
                }
                any = true;
            }
        }
        let m = if any { b.build().ok().map(Arc::new) } else { None };
        self.cache.borrow_mut().insert(dir.to_string(), m.clone());
        m
    }

    /// Ignore state of one entry, checking only the rules in its parent folders.
    fn direct(&self, path: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = path.split('/').collect();
        // Deeper files take precedence.
        for depth in (0..comps.len()).rev() {
            let dir = comps[..depth].join("/");
            if let Some(m) = self.matcher(&dir) {
                let rel = comps[depth..].join("/");
                let full = if dir.is_empty() { self.root.join(&rel) } else { self.root.join(&dir).join(&rel) };
                let r = m.matched(&full, is_dir);
                if r.is_ignore() {
                    return true;
                }
                if r.is_whitelist() {
                    return false;
                }
            }
        }
        false
    }

    pub fn ignored(&self, path: &str, is_dir: bool) -> bool {
        if path == ".git" || path.starts_with(".git/") {
            return true;
        }
        let comps: Vec<&str> = path.split('/').collect();
        for i in 1..comps.len() {
            if self.direct(&comps[..i].join("/"), true) {
                return true;
            }
        }
        self.direct(path, is_dir)
    }
}
