//! `.gitignore` rules, read from one snapshot root. A path below an ignored folder is ignored.
//! `.layrattributes` files give paths attributes with the same patterns: a line is a pattern and
//! its attributes, such as `node_modules regenerable`; `-regenerable` unsets one, and the last
//! matching line wins.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The local ignore file of a working folder: not versioned, read by git and layr. The project
/// setting `ignore.local` fills it in the `.git` folder of each line.
pub const LOCAL_EXCLUDE: &str = ".git/info/exclude";
/// The names of the files in a tree that hold ignore rules for their folder.
pub(crate) const RULE_FILES: &[&str] = &[".gitignore", ".layrignore"];

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

/// Exact bytes consumed by a cached-status classification, including absent rule files.
struct CapturedPolicy {
    local: Vec<u8>,
    files: BTreeMap<String, Option<Vec<u8>>>,
}

/// The path of a local exclude file and its bytes, or the error that reading them gave.
type LocalExclude = (PathBuf, Result<Vec<u8>, std::io::ErrorKind>);

pub struct Rules {
    root: PathBuf,
    captured: Option<CapturedPolicy>,
    files: &'static [&'static str],
    /// For `.layrattributes`: the attribute whose patterns this matcher holds.
    attribute: Option<&'static str>,
    cache: RefCell<HashMap<String, Option<Arc<Gitignore>>>>,
    /// `local_exclude` and its bytes, read once: the matcher and `local_policy` use the same bytes.
    local: std::cell::OnceCell<Option<LocalExclude>>,
    /// When recording (`recording`): the rule files read, with their bytes (`None`: no file
    /// there). The matcher and `same_rule_files` read each of them once.
    read: Option<RefCell<BTreeMap<String, Option<Vec<u8>>>>>,
    /// The folders (relative to root, "" is the root) whose rule files are read. None: every folder.
    dirs: Option<std::collections::HashSet<String>>,
    /// Read rules from the logical source without exporting object or layer entries.
    source: Option<crate::source::Source>,
}

impl Rules {
    pub fn new(root: &Path) -> Rules {
        Rules {
            root: root.to_path_buf(),
            captured: None,
            files: RULE_FILES,
            attribute: None,
            cache: RefCell::new(HashMap::new()),
            local: std::cell::OnceCell::new(),
            read: None,
            dirs: None,
            source: None,
        }
    }

    /// The ignore rules (`attribute` None) or the rules of `attribute`, read only in the folders
    /// `dirs`. A walk that lists the folders knows which hold rule files, so no other folder costs
    /// a stat for them.
    pub fn limited(root: &Path, attribute: Option<&'static str>, dirs: std::collections::HashSet<String>) -> Rules {
        Rules {
            root: root.to_path_buf(),
            captured: None,
            files: if attribute.is_some() { &[".layrattributes"] } else { RULE_FILES },
            attribute,
            cache: RefCell::new(HashMap::new()),
            local: std::cell::OnceCell::new(),
            read: None,
            dirs: Some(dirs),
            source: None,
        }
    }

    pub fn of(source: &crate::source::Source) -> Rules {
        let mut rules = Rules::new(source.root_hint());
        rules.source = Some(source.clone());
        rules
    }
    fn is_file(&self, relative: &str) -> bool {
        match &self.source {
            Some(source) => crate::fsutil::lmeta(source, relative),
            None => crate::fsutil::lmeta(&self.root, relative),
        }
        .is_some_and(|metadata| metadata.is_file())
    }
    fn read_rule(&self, relative: &str) -> Vec<u8> {
        match &self.source {
            Some(source) => crate::fsutil::read_file(source, relative).unwrap_or_default(),
            None => std::fs::read(self.root.join(relative)).unwrap_or_default(),
        }
    }
    /// The rule file at `relative` as the matcher reads it (`None`: no file there). On a volume
    /// that ignores case `.GITIGNORE` is not `.gitignore`, and a path through a link names none.
    fn rule_file(&self, relative: &str) -> Option<Vec<u8>> {
        self.is_file(relative).then(|| self.read_rule(relative))
    }
    /// `rule_file`, read once when recording.
    fn recorded_rule_file(&self, relative: &str) -> Option<Vec<u8>> {
        let Some(read) = &self.read else { return self.rule_file(relative) };
        if let Some(bytes) = read.borrow().get(relative) {
            return bytes.clone();
        }
        let bytes = self.rule_file(relative);
        read.borrow_mut().insert(relative.to_string(), bytes.clone());
        bytes
    }

    /// These rules, recording the rule files they read (`rule_files`).
    pub(crate) fn recording(mut self) -> Self {
        self.read = Some(RefCell::default());
        self
    }

    /// The paths that `.layrattributes` files give the attribute `name`: `ignored` is true for them.
    pub fn attribute(root: &Path, name: &'static str) -> Rules {
        Rules {
            root: root.to_path_buf(),
            captured: None,
            files: &[".layrattributes"],
            attribute: Some(name),
            cache: RefCell::new(HashMap::new()),
            local: std::cell::OnceCell::new(),
            read: None,
            dirs: None,
            source: None,
        }
    }

    /// The local exclude file of the working folder, when it has one (`LOCAL_EXCLUDE`), and its
    /// bytes. It is read from the folder itself: a state, and the layers of one, have no `.git`.
    fn local_exclude(&self) -> Option<&LocalExclude> {
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
                (self.attribute.is_none() && rules(&path)).then(|| {
                    let bytes = std::fs::read(&path).map_err(|e| e.kind());
                    (path, bytes)
                })
            })
            .as_ref()
    }

    /// Bytes of the effective local ignore policy, including an external line exclude: the bytes
    /// that the rules match with.
    pub(crate) fn local_policy(&self) -> std::io::Result<Vec<u8>> {
        match self.local_exclude() {
            Some((_, bytes)) => bytes.clone().map_err(std::io::Error::from),
            None => Ok(Vec::new()),
        }
    }

    /// Freeze the ignore inputs of these paths. Classification never reopens them.
    pub(crate) fn captured<'a>(root: &Path, paths: impl Iterator<Item = &'a str>, local: &[u8]) -> std::io::Result<Self> {
        let mut files = BTreeMap::new();
        for path in paths {
            for dir in std::iter::once("").chain(path.match_indices('/').map(|(i, _)| &path[..i])) {
                for name in RULE_FILES {
                    let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                    if !files.contains_key(&rel) {
                        let bytes = optional_rule(&root.join(&rel))?;
                        files.insert(rel, bytes);
                    }
                }
            }
        }
        let mut rules = Self::new(root);
        rules.captured = Some(CapturedPolicy { local: local.to_vec(), files });
        Ok(rules)
    }

    /// Validate AFTER classification, including rules absent when inputs were captured.
    pub(crate) fn captured_matches(&self) -> std::io::Result<bool> {
        let Some(policy) = &self.captured else { return Ok(false) };
        if Self::new(&self.root).local_policy()? != policy.local {
            return Ok(false);
        }
        for (rel, bytes) in &policy.files {
            if optional_rule(&self.root.join(rel))? != *bytes {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The rule files that the matcher read so far, with their bytes (`None`: no file there). A
    /// result classified by these rules is the same while `same_rule_files` holds for them.
    pub(crate) fn rule_files(&self) -> BTreeMap<String, Option<Vec<u8>>> {
        self.read.as_ref().map(|read| read.borrow().clone()).unwrap_or_default()
    }

    /// Whether the matcher reads `files` (`rule_files`). When recording, the matcher then
    /// classifies with the bytes checked here, not a later version. O(files).
    pub(crate) fn same_rule_files(&self, files: &BTreeMap<String, Option<Vec<u8>>>) -> bool {
        files.iter().all(|(rel, bytes)| self.recorded_rule_file(rel) == *bytes)
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
        if let Some(policy) = &self.captured {
            let local = dir.is_empty().then_some((LOCAL_EXCLUDE.to_string(), policy.local.as_slice()));
            let files = self.files.iter().filter_map(|name| {
                let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                policy.files.get(&rel).and_then(Option::as_ref).map(|bytes| (rel, bytes.as_slice()))
            });
            for (rel, bytes) in local.into_iter().chain(files) {
                add_ignore_bytes(&mut b, self.root.join(&rel), bytes);
                any = true;
            }
        } else {
            // The local excludes of a working folder (`.git/info/exclude`), as git reads them: below the
            // root's rule files, which come after them and win. A state has no `.git`.
            if let Some((path, bytes)) = dir.is_empty().then(|| self.local_exclude()).flatten() {
                add_ignore_bytes(&mut b, path.clone(), bytes.as_deref().unwrap_or_default());
                any = true;
            }
            for name in self.files {
                let rel = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                if let Some(bytes) = self.recorded_rule_file(&rel) {
                    match self.attribute {
                        None => {
                            add_ignore_bytes(&mut b, self.root.join(&rel), &bytes);
                        }
                        Some(attribute) => {
                            let unset = format!("-{attribute}");
                            let text = std::str::from_utf8(&bytes).unwrap_or_default();
                            for line in text.lines() {
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

fn optional_rule(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => std::fs::read(path).map(Some),
        Ok(_) => Ok(None),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Match GitignoreBuilder::add, including BOM stripping and partial parsing before the
/// first invalid UTF-8 line. Both live and captured readers consume the same bytes.
fn add_ignore_bytes(builder: &mut GitignoreBuilder, origin: PathBuf, bytes: &[u8]) {
    use std::io::BufRead;
    for (index, line) in bytes.lines().map_while(Result::ok).enumerate() {
        let line = if index == 0 { line.trim_start_matches('\u{feff}') } else { &line };
        let _ = builder.add_line(Some(origin.clone()), line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_attributes_are_rejected_as_a_whole_for_live_and_logical_readers() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-attribute-bytes-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("nested"))?;
        for dir in ["", "nested"] {
            let rule = root.join(dir).join(".layrattributes");
            let path = if dir.is_empty() { "cache".to_owned() } else { format!("{dir}/cache") };
            for (bytes, matched) in [
                (b"cache regenerable\n".as_slice(), true),
                (b"cache regenerable\n\xff\n".as_slice(), false),
                (b"\xff\ncache regenerable\n".as_slice(), false),
            ] {
                std::fs::write(&rule, bytes)?;
                let live = Rules::attribute(&root, "regenerable");
                let logical = Rules::of(&crate::source::Source::native(&root));
                let logical = Rules { files: &[".layrattributes"], attribute: Some("regenerable"), ..logical };
                assert_eq!(live.ignored(&path, true), matched, "live: {path}: {bytes:?}");
                assert_eq!(logical.ignored(&path, true), matched, "logical: {path}: {bytes:?}");
            }
            std::fs::remove_file(rule)?;
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn live_captured_and_logical_ignore_bytes_match_gitignore_bom_and_invalid_utf8() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-ignore-bytes-{}", crate::ids::new_id()));
        std::fs::create_dir(&root)?;
        // A local info directory prevents another test's external line policy from applying.
        std::fs::create_dir_all(root.join(".git/info"))?;
        for file in [".gitignore", ".layrignore", "nested/.gitignore", LOCAL_EXCLUDE] {
            let rule = root.join(file);
            std::fs::create_dir_all(rule.parent().unwrap())?;
            std::fs::write(&rule, b"\xef\xbb\xbfignored\r\nlater\ninvalid_\xff\npost\n")?;
            let dir = if file.starts_with("nested/") { "nested/" } else { "" };
            let paths = [format!("{dir}ignored"), format!("{dir}later"), format!("{dir}invalid_\u{fffd}"), format!("{dir}post")];
            let live = Rules::new(&root);
            let captured = Rules::captured(&root, paths.iter().map(String::as_str), &live.local_policy()?)?;
            let logical = Rules::of(&crate::source::Source::native(&root));
            let base = if dir.is_empty() { root.clone() } else { root.join("nested") };
            let mut expected = GitignoreBuilder::new(&base);
            expected.add(&rule);
            let expected = expected.build()?;
            for (index, path) in paths.iter().enumerate() {
                let ignored = expected.matched(root.join(path), false).is_ignore();
                assert_eq!(ignored, index < 2, "reference: {file}: {path}");
                assert_eq!(live.ignored(path, false), ignored, "live: {file}: {path}");
                assert_eq!(captured.ignored(path, false), ignored, "captured: {file}: {path}");
                assert_eq!(logical.ignored(path, false), ignored, "logical: {file}: {path}");
            }
            std::fs::remove_file(rule)?;
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// The rules match with the local exclude bytes that `local_policy` returns, even when the file
    /// changes in between: a key made from the policy then names the rules that were used.
    #[test]
    fn rules_match_with_the_local_policy_they_return() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-local-policy-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join(".git/info"))?;
        std::fs::write(root.join(LOCAL_EXCLUDE), "first\n")?;
        let rules = Rules::new(&root);
        assert_eq!(rules.local_policy()?, b"first\n");
        std::fs::write(root.join(LOCAL_EXCLUDE), "second\n")?;
        assert!(rules.ignored("first", false) && !rules.ignored("second", false));
        assert_eq!(rules.local_policy()?, b"first\n");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// The rule files read are checked as the matcher reads them: one under another case (a
    /// volume that ignores case finds it) or reached through a link is none for it.
    #[test]
    fn rule_files_are_checked_as_the_matcher_reads_them() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-rule-files-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("d/sub"))?;
        std::fs::write(root.join("d/sub/.gitignore"), "a\n")?;
        let rules = Rules::new(&root).recording();
        assert!(rules.ignored("d/sub/a", false));
        let files = rules.rule_files();
        assert_eq!(files.get("d/sub/.gitignore"), Some(&Some(b"a\n".to_vec())));
        assert!(Rules::new(&root).same_rule_files(&files));
        std::fs::rename(root.join("d/sub/.gitignore"), root.join("d/sub/.GITIGNORE"))?;
        assert!(!Rules::new(&root).ignored("d/sub/a", false) && !Rules::new(&root).same_rule_files(&files));
        std::fs::rename(root.join("d/sub/.GITIGNORE"), root.join("d/sub/.gitignore"))?;
        std::fs::rename(root.join("d"), root.join("real"))?;
        std::os::unix::fs::symlink("real", root.join("d"))?;
        assert!(!Rules::new(&root).ignored("d/sub/a", false) && !Rules::new(&root).same_rule_files(&files));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// Recording rules classify with the rule bytes that `same_rule_files` checked, though the file
    /// changes after; rules that do not record keep nothing.
    #[test]
    fn checked_rule_files_are_the_ones_the_rules_match_with() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("layr-rule-files-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join(".gitignore"), "a\n")?;
        let plain = Rules::new(&root);
        assert!(plain.ignored("a", false) && plain.rule_files().is_empty());
        let first = Rules::new(&root).recording();
        assert!(first.ignored("a", false));
        let files = first.rule_files();
        assert_eq!(files.get(".gitignore"), Some(&Some(b"a\n".to_vec())));
        let rules = Rules::new(&root).recording();
        assert!(rules.same_rule_files(&files));
        std::fs::write(root.join(".gitignore"), "b\n")?;
        assert!(rules.ignored("a", false) && !rules.ignored("b", false));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
