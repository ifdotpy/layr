//! On-demand path overlaps between live lines. Nothing is written to permanent history.
//!
//! The changes of our live folder against a merge base are the combination of two parts: the
//! compare of the base with our head (a saved state compare, cached under the owning project's
//! `meta/` folder) and one compare of our head with the live folder. The live compare is saved per
//! line for the scan of the folder it read (`live_compare`), so an unchanged folder costs no compare.
//! Renames take part as their two paths, and our side counts each path on its own. A path that
//! both parts changed is checked again against the base. When a rule file changed between our head
//! and the live folder, the parts left out different paths: each path that the other line changed
//! is then compared on its own (`OurChanges::Checked`). Inside a saved compare, the
//! ignore rules of the saved state skip ignored paths before any content compare. The caller's
//! ignore rules and hidden paths run after the cache on every call.

use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil::{self, Src};
use crate::ignore_rules::Rules;
use crate::model::Line;
use crate::overlay::{Layer, OpenOverlay};
use crate::repo::{skip_path, Repo, Snap, Temp};
use crate::status_cache::FileFingerprint;
use anyhow::{anyhow, Result};
use layr_hashers::{Digest, GitCompatible};
use serde::{Deserialize, Serialize};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Format of the saved state compares. Bump it when `changes::candidates`,
/// `changes::really_changed`, `skip_path` or the ignore rules inside the compare change their
/// results: older entries then miss. 2: the ignore rules of the saved state skip paths.
/// 3: a change records whether its path is missing in the parent (`added`) or the child (`removed`).
/// 4: a rename to an ignored path keeps the removal of its old path; no `added` and `removed`.
const COMPARE_FORMAT: u32 = 4;
/// The folder under `meta/` that holds the saved state compares. `gc` removes the entries whose
/// states are gone.
const COMPARE_DIR: &str = "overlap-compares";
/// The folder under `meta/` that holds the saved compares of a line's head with its live folder.
const LIVE_DIR: &str = "overlap-live";
/// Format of the saved live compares. Bump it when `compare` changes its results or `live_digest` its key.
/// 4: the key of a capture leaves out its device and root (`capture_relative`), and holds the working folder;
/// the rule files that the compare read are saved with it. 5: those rule files are the working folder's.
const LIVE_FORMAT: u32 = 5;
/// Threads that compare saved states at once.

#[derive(Debug, Serialize)]
pub struct Overlap {
    pub path: String,
    pub line: String,
    pub owner: u32,
    /// Unix time in milliseconds, from the other line's file or latest saved state.
    pub changed_at: i64,
}

/// A change between two trees, before the caller's ignore and hidden filters.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Found {
    pub(crate) path: String,
    old_path: Option<String>,
    is_dir: bool,
    /// Milliseconds of the new file, 0 when there is none.
    time: i64,
}

#[derive(Serialize, Deserialize)]
struct SavedCompare {
    format: u32,
    base: String,
    theirs: String,
    /// The hash of the local ignore policy that the compare left paths out by (`local_digest`).
    local: String,
    found: Vec<Found>,
}

/// The compare of a line's head with its live folder, saved for the scan it was made from.
#[derive(Serialize, Deserialize)]
struct SavedLive {
    format: u32,
    head: String,
    digest: String,
    /// The rule files that the compare read (`Rules::rule_files`). The scan does not observe all of
    /// them: a nested tree is not scanned, but the rules in it can classify a path of our head.
    rules: BTreeMap<String, Option<Vec<u8>>>,
    live: LiveChanges,
}

/// A compare of our head with the live folder (`compare_live`): the changes, and whether a rule
/// file changed (`rule_change`), which the changes leave out when the rules ignore it.
#[derive(Clone, Serialize, Deserialize)]
struct LiveChanges {
    found: Vec<Found>,
    rules_changed: bool,
}

/// Our working folder as the overlap compares read it.
struct Ours {
    /// The capture that `root` is (btrfs, OverlayFS), removed on drop. `None`: the live folder (APFS).
    capture: Option<Temp>,
    root: PathBuf,
    /// The ignore rules of the working folder, as status reads them: a nested volume is an empty
    /// folder in a capture, but its rule files classify our head's paths in it.
    rules: Rules,
    /// The ignore rules of `root`, which the overlay layers are compared with.
    layer_rules: Rules,
    /// The working folder's device and inode: a new folder is another source.
    source: String,
    /// The clock before `root` was read.
    started: u64,
}

impl Ours {
    fn new(working: &Path, capture: Option<Temp>, source: String, started: u64) -> Ours {
        let root = capture.as_ref().map_or_else(|| working.to_path_buf(), |c| c.path.clone());
        Ours { capture, rules: Rules::new(working).recording(), layer_rules: Rules::new(&root), root, source, started }
    }
}

/// `compare_live` of `head` (the state `head_id`) with our folder, saved in `file` for the folder
/// it read. A saved compare is used only when the state, `live_digest` and the rule files it read
/// (looked up as the rules read them) are the same: the compare reads only those, so the result is
/// the same. A capture cannot change, so one digest serves; the live folder is observed again
/// after the compare.
fn saved_live(file: &Path, head_id: &str, head: &Snap, ours: &Ours) -> Result<LiveChanges> {
    let local = local_digest(&ours.rules);
    let digest = || -> Result<Option<String>> {
        let _t = crate::trace::span("overlap scan digest");
        let Some(local) = &local else { return Ok(None) };
        scan_digest(ours, local)
    };
    let before = digest()?;
    if let Some(digest) = &before {
        let saved = std::fs::read(file).ok().and_then(|raw| serde_json::from_slice::<SavedLive>(&raw).ok());
        let same = |s: &SavedLive| {
            s.format == LIVE_FORMAT && s.head == head_id && s.digest == *digest && ours.rules.same_rule_files(&s.rules)
        };
        if let Some(saved) = saved.filter(same) {
            let _t = crate::trace::span("overlap live saved");
            return Ok(saved.live);
        }
    }
    let found = {
        let _t = crate::trace::span("overlap live compare");
        compare_live(head, &ours.root, &ours.rules)?
    };
    if let Some(key) = before {
        if ours.capture.is_some() || digest().ok().flatten().as_ref() == Some(&key) {
            // A file that cannot be written only costs the next status a compare.
            let rules = ours.rules.rule_files();
            let saved = SavedLive { format: LIVE_FORMAT, head: head_id.to_string(), digest: key, rules, live: found.clone() };
            let _ = write_live(file, &saved);
        }
    }
    Ok(found)
}

fn write_live(file: &Path, saved: &SavedLive) -> Result<()> {
    std::fs::create_dir_all(file.parent().ok_or_else(|| anyhow!("live compare folder"))?)?;
    crate::fsutil::cache_write(file, &serde_json::to_vec(saved)?, 0o600)
}

/// `live_digest` of our folder as it is now, with the local policy hash `local`.
fn scan_digest(ours: &Ours, local: &str) -> Result<Option<String>> {
    let scan = crate::fs::observe_tree(&ours.root)?;
    if ours.capture.is_none() {
        // Mount points are resolved paths. A list that cannot be read gives no key either.
        let root = std::fs::canonicalize(&ours.root);
        if root.and_then(|root| Ok(mounted_inside(&root, &crate::sys::mount_points()?))).unwrap_or(true) {
            return Ok(None);
        }
        return live_digest(&scan, local, &ours.source, ours.started);
    }
    // The key of a capture leaves out its root, but not the names in it: a volume nested there is
    // a folder of another device, which the scan leaves out and the compare reads.
    let mut names = std::fs::read_dir(&ours.root)?
        .map(|entry| entry?.file_name().into_string().map_err(|_| anyhow!("non-UTF-8 file name")))
        .filter(|name| !name.as_ref().is_ok_and(|name| skip_path(name)))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    live_digest(&capture_relative(scan), local, &format!("{}\0{}", ours.source, names.join("\0")), ours.started)
}

/// Whether one of `mounts` is inside `root`. The scan leaves out what is on another device; a
/// compare reads a file mounted there (FUSE takes a file as a mount point), and a folder mounted
/// there can go away while it runs. Each digest checks again, so a compare that starts or ends
/// with a mount inside is not saved. A capture holds no mounts. O(mounts).
fn mounted_inside(root: &Path, mounts: &[PathBuf]) -> bool {
    mounts.iter().any(|m| m != root && m.starts_with(root))
}

/// The observations of a capture as they are the same in each capture of an unchanged folder: each
/// capture is a new device and gives its root new times; an entry keeps its inode, times and
/// evidence. The live folder keeps both: its root records a top-level entry made and removed, and
/// its devices tell a volume mounted inside it. O(n) in the entries.
fn capture_relative(mut scan: BTreeMap<String, FileFingerprint>) -> BTreeMap<String, FileFingerprint> {
    scan.remove("");
    for entry in scan.values_mut() {
        if let FileFingerprint::Verified { stat, .. } = entry {
            stat.dev = 0;
        }
    }
    scan
}

/// The key under which a compare of a folder can be reused: the observations of its entries, the
/// local ignore policy (outside the tree) and the identity of the working folder (`source`).
/// O(n) in the entries. `None`
/// when an entry is not verifiable (mapped files) or may have changed again within its time
/// stamp (`FileFingerprint::trusted` against `started`, the clock before the folder was read).
fn live_digest(scan: &BTreeMap<String, FileFingerprint>, local: &str, source: &str, started: u64) -> Result<Option<String>> {
    let mut hash = GitCompatible::new();
    hash.update(source);
    hash.update(local);
    for (path, entry) in scan {
        let FileFingerprint::Verified { stat, evidence } = entry else { return Ok(None) };
        if !entry.trusted(started) {
            return Ok(None);
        }
        hash.update(serde_json::to_vec(&(path, stat, evidence))?);
    }
    Ok(Some(hex::encode(hash.finalize())))
}

/// The hash of the local ignore policy of `rules` (`.git/info/exclude`, outside the trees). `None`
/// when it cannot be read: nothing made with it is saved.
fn local_digest(rules: &Rules) -> Option<String> {
    let mut hash = GitCompatible::new();
    hash.update(rules.local_policy().ok()?);
    Some(hex::encode(hash.finalize()))
}

/// Whether a change to `path` can change the ignore rules: a rule file in a folder that the rules
/// do not ignore. Rules inside an ignored folder only match paths that are ignored anyway.
fn rule_change(path: &str, rules: &Rules) -> bool {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    crate::ignore_rules::RULE_FILES.contains(&name) && (dir.is_empty() || !rules.ignored(dir, true))
}

/// The real changes between two trees. A change is left out before the content compare when
/// `skip_path` or `rules` leave out both of its sides, so ignored build output costs no content
/// reads.
pub(crate) fn compare(base: &(impl Src + ?Sized), child: &(impl Src + ?Sized), rules: &Rules) -> Result<Vec<Found>> {
    Ok(compare_live(base, child, rules)?.found)
}

/// `compare`, and whether any candidate, ignored or not, changed a rule file (`rule_change`).
fn compare_live(base: &(impl Src + ?Sized), child: &(impl Src + ?Sized), rules: &Rules) -> Result<LiveChanges> {
    let mut out = LiveChanges { found: Vec::new(), rules_changed: false };
    #[cfg(not(test))]
    let candidates = changes::candidates(base, child)?;
    #[cfg(test)]
    let candidates = test_probe::candidates(base, child)?;
    let kept =
        |path: &str, side: Option<&fsutil::Meta>| side.is_some_and(|m| !skip_path(path) && !rules.ignored(path, m.is_dir()));
    for change in candidates {
        let rule = rule_change(change.path(), rules) || change.old_path().is_some_and(|old| rule_change(old, rules));
        let is_dir = change.new().or(change.old()).is_some_and(|m| m.is_dir());
        let (old_kept, new_kept) = (kept(change.src(), change.old()), kept(change.path(), change.new()));
        if !old_kept && !new_kept && !rule || !changes::really_changed(base, child, &change)? {
            continue;
        }
        out.rules_changed |= rule;
        if !old_kept && !new_kept {
            continue;
        }
        if !new_kept {
            if let Some(old) = change.old_path() {
                // A rename to an ignored path still removes its old path.
                out.found.push(Found { path: old.clone(), old_path: None, is_dir, time: 0 });
                continue;
            }
        }
        let time = change.new().map(|m| m.mtime * 1000).unwrap_or(0);
        out.found.push(Found { path: change.path().clone(), old_path: change.old_path().cloned(), is_dir, time });
    }
    Ok(out)
}

/// The test of `compare` for one path: whether the path changed from `base` to `live`.
pub(crate) fn recheck(
    base: &(impl Src + ?Sized),
    live: &(impl Src + ?Sized),
    rules: &Rules,
    path: &str,
) -> Result<Option<Found>> {
    let old = fsutil::lmeta(base, path);
    let new = fsutil::lmeta(live, path);
    if old.is_none() && new.is_none() {
        return Ok(None);
    }
    let is_dir = new.as_ref().or(old.as_ref()).is_some_and(|m| m.is_dir());
    if skip_path(path) || rules.ignored(path, is_dir) {
        return Ok(None);
    }
    let time = new.as_ref().map(|m| m.mtime * 1000).unwrap_or(0);
    let Some(change) = changes::EntryChange::observed(path.to_string(), old, new, crate::compare::DataEvidence::Unknown) else {
        return Ok(None);
    };
    if !changes::really_changed(base, live, &change)? {
        return Ok(None);
    }
    Ok(Some(Found { path: path.to_string(), old_path: None, is_dir, time }))
}

/// A rename as the two path changes it is made of: its old path removed, its new path added.
fn endpoints(found: &[Found]) -> impl Iterator<Item = Found> + '_ {
    found.iter().flat_map(|f| {
        let removed = f.old_path.as_ref().map(|old| Found { path: old.clone(), old_path: None, is_dir: f.is_dir, time: 0 });
        removed.into_iter().chain([Found { old_path: None, ..f.clone() }])
    })
}

/// The changes of the live tree against the base, from the two parts. Renames take part as their
/// two paths (`endpoints`). A path that the saved part and the live part both changed is checked
/// again with `recheck`, because it may have changed back. Both parts must have left out the same
/// ignored paths (`OurBase::changes`). O(n log n) in the size of the parts.
fn merge_changes(
    saved: &[Found],
    head_live: &[Found],
    mut recheck: impl FnMut(&str) -> Result<Option<Found>>,
) -> Result<Vec<Found>> {
    let saved: BTreeMap<String, Found> = endpoints(saved).map(|f| (f.path.clone(), f)).collect();
    let live: BTreeMap<String, Found> = endpoints(head_live).map(|f| (f.path.clone(), f)).collect();
    let mut out: Vec<Found> = saved.iter().filter(|(path, _)| !live.contains_key(*path)).map(|(_, f)| f.clone()).collect();
    for (path, f) in live {
        if saved.contains_key(&path) {
            out.extend(recheck(&path)?);
        } else {
            out.push(f);
        }
    }
    Ok(out)
}

/// The paths of our changes that can overlap: `visible` of each path on its own, so a rename
/// counts both of its paths whatever route (`merge_changes`, the direct compare) produced it.
fn our_paths(found: &[Found], hidden: &BTreeSet<String>, ignored: &Rules) -> BTreeSet<String> {
    visible(&endpoints(found).collect::<Vec<_>>(), hidden, ignored).into_keys().collect()
}

/// Our changes against one merge base, as the paths that can overlap (`our_paths`), or, when the
/// two parts left out different ignored paths, each path asked for compared on its own (`recheck`)
/// and remembered. The other line's changed paths bound those checks.
enum OurChanges {
    Paths(BTreeSet<String>),
    Checked { base: Snap, seen: BTreeMap<String, bool> },
}

impl OurChanges {
    /// Keep the paths of `theirs` that we changed in `live` (`our_paths` semantics: hidden and
    /// ignored paths are not). Each checked path is compared once (`recheck`).
    fn retain_changed(
        &mut self,
        theirs: &mut BTreeMap<String, i64>,
        live: &Path,
        hidden: &BTreeSet<String>,
        rules: &Rules,
    ) -> Result<()> {
        let (base, seen) = match self {
            Self::Paths(paths) => return Ok(theirs.retain(|path, _| paths.contains(path))),
            Self::Checked { base, seen } => (base, seen),
        };
        for path in theirs.keys().filter(|p| !hidden.contains(*p)) {
            if !seen.contains_key(path) {
                seen.insert(path.clone(), recheck(base, live, rules, path)?.is_some());
            }
        }
        theirs.retain(|path, _| seen.get(path) == Some(&true));
        Ok(())
    }
}

/// The paths of `found` that the caller may see: not ignored, not hidden. A change with a hidden
/// old name is left out whole, as a rename of a hidden file is.
fn visible(found: &[Found], hidden: &BTreeSet<String>, ignored: &Rules) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for f in found {
        // Ignored files (build output, node_modules) are not work that two lines can conflict on.
        if ignored.ignored(&f.path, f.is_dir)
            || hidden.contains(&f.path)
            || f.old_path.as_ref().is_some_and(|p| hidden.contains(p))
        {
            continue;
        }
        out.insert(f.path.clone(), f.time);
        if let Some(old) = &f.old_path {
            out.insert(old.clone(), f.time);
        }
    }
    out
}

/// The compare of two saved states, from the cache or computed and then saved.
struct StateCompare {
    file: PathBuf,
    base_id: String,
    theirs_id: String,
    base: Snap,
    theirs: Snap,
}

impl StateCompare {
    fn new(dir: &Path, base_id: &str, theirs_id: &str, base: Snap, theirs: Snap) -> StateCompare {
        let mut h = GitCompatible::new();
        h.update(format!("{COMPARE_FORMAT}\n{base_id}\n{theirs_id}"));
        let file = dir.join(format!("{:x}.json", h.finalize()));
        StateCompare { file, base_id: base_id.to_string(), theirs_id: theirs_id.to_string(), base, theirs }
    }

    fn run(&self) -> Result<Vec<Found>> {
        if self.base_id == self.theirs_id {
            return Ok(Vec::new());
        }
        // The ignore rules of the saved state (the tree is immutable) and the local policy, which
        // lives outside it: an entry made under another local policy is a miss and is replaced.
        let rules = Rules::of(&self.theirs);
        let local = local_digest(&rules);
        if let Some(found) = local.as_deref().and_then(|local| self.read(local)) {
            return Ok(found);
        }
        let _t = crate::trace::span("overlap compare miss");
        let found = compare(&self.base, &self.theirs, &rules)?;
        // `rules` read the local policy once, so the compare used the one in `local`. A save
        // that fails costs only time on the next call.
        if let Some(local) = local {
            let _ = self.write(&local, &found);
        }
        Ok(found)
    }

    /// `None` for a missing, unreadable or foreign entry: the compare runs again and rewrites it.
    fn read(&self, local: &str) -> Option<Vec<Found>> {
        #[cfg(test)]
        test_probe::record(|p| p.cache_reads += 1);
        let saved: SavedCompare = serde_json::from_slice(&std::fs::read(&self.file).ok()?).ok()?;
        let same = saved.format == COMPARE_FORMAT
            && saved.base == self.base_id
            && saved.theirs == self.theirs_id
            && saved.local == local;
        same.then_some(saved.found)
    }

    fn write(&self, local: &str, found: &[Found]) -> Result<()> {
        #[cfg(test)]
        test_probe::record(|p| p.cache_writes += 1);
        let dir = self.file.parent().ok_or_else(|| anyhow!("compare folder"))?;
        std::fs::create_dir_all(dir)?;
        let saved = SavedCompare {
            format: COMPARE_FORMAT,
            base: self.base_id.clone(),
            theirs: self.theirs_id.clone(),
            local: local.to_string(),
            found: found.to_vec(),
        };
        crate::fsutil::cache_write(&self.file, &serde_json::to_vec(&saved)?, 0o600)
    }
}

/// Run state compares in order, using the current operation's pool when available.
fn run_compares(jobs: &[StateCompare]) -> Result<Vec<Vec<Found>>> {
    let Some(width) = std::num::NonZeroUsize::new(jobs.len()) else {
        return Ok(Vec::new());
    };
    crate::pools::run(crate::pools::Priority::Foreground, crate::pools::Width::Items(width), || {
        use rayon::prelude::*;
        jobs.par_iter().map(StateCompare::run).collect::<Vec<_>>().into_iter().collect()
    })?
}

/// The state compares and live compares of one `overlaps` call. Each one runs once, and each
/// state tree is read once for all the lines (`Repo::tree`).
#[derive(Default)]
struct Work {
    /// The trees read, by project and state.
    trees: BTreeMap<(String, String), Snap>,
    /// The saved compares: base -> their head, our base -> head, and overlay layers.
    jobs: Vec<StateCompare>,
    /// Our saved compare per merge base: the index in `jobs`.
    ours_jobs: BTreeMap<String, usize>,
    /// Our head -> live folder. `Some(None)`: our head state cannot be read.
    head_live: Option<Option<LiveChanges>>,
    /// The saved compare of an overlay: base -> our head of the overlay, by (overlay, base).
    layer_jobs: BTreeMap<(String, String), usize>,
    /// Our overlay head -> live folder, once per overlay. `Some(None)`: the head cannot be read.
    layer_live: BTreeMap<String, Option<LiveChanges>>,
}

/// Our merge base relative to the head used for the live compare. Identity is a state-ID
/// property, never inferred from an empty saved diff (which can omit metadata changes).
#[derive(Clone, Copy, Debug)]
enum OurBase {
    AtHead,
    /// The queued saved compare, or `None` when the head is unreadable.
    Diverged(Option<usize>),
}

impl OurBase {
    fn plan(base: &str, head: &str, queue: impl FnOnce() -> Result<Option<usize>>) -> Result<Self> {
        if base == head {
            Ok(Self::AtHead)
        } else {
            queue().map(Self::Diverged)
        }
    }

    /// Our changes against `base`. With both parts (the saved compare and the live compare of our
    /// head), they combine, unless a rule file changed between them: the parts then left out
    /// different ignored paths, and the paths are checked one by one. Without them, the direct
    /// compare runs.
    fn changes(
        self,
        base: &Snap,
        live: &Path,
        rules: &Rules,
        hidden: &BTreeSet<String>,
        found: &[Vec<Found>],
        head_live: Option<&LiveChanges>,
    ) -> Result<OurChanges> {
        let paths = |changed: &[Found]| OurChanges::Paths(our_paths(changed, hidden, rules));
        match (self, head_live) {
            (Self::AtHead, Some(head_live)) => Ok(paths(&head_live.found)),
            (Self::Diverged(Some(job)), Some(head_live)) => {
                #[cfg(test)]
                test_probe::record(|p| p.combines += 1);
                let _t = crate::trace::span("overlap combine changes");
                if head_live.rules_changed {
                    let _t = crate::trace::span("overlap rule change checks paths");
                    return Ok(OurChanges::Checked { base: base.clone(), seen: BTreeMap::new() });
                }
                Ok(paths(&merge_changes(&found[job], &head_live.found, |path| recheck(base, live, rules, path))?))
            }
            _ => {
                #[cfg(test)]
                test_probe::record(|p| p.fallbacks += 1);
                let _t = crate::trace::span("overlap direct base compare");
                Ok(paths(&compare(base, live, rules)?))
            }
        }
    }
}

/// What one other line contributes, with its state compare in `Work::jobs`.
struct OtherPlan {
    name: String,
    owner: u32,
    /// The merge base with this line (its tree is the base of `job`). `our_base` plans our compare against it.
    base: String,
    our_base: OurBase,
    their_private: BTreeSet<String>,
    hidden: BTreeSet<String>,
    state_time: i64,
    job: usize,
    layers: Vec<LayerPlan>,
}

/// One overlay layer that both lines hold, with its state compares in `Work::jobs`.
struct LayerPlan {
    overlay: String,
    base: String,
    our_base: OurBase,
    state_time: i64,
    job: usize,
}

impl<'a> Repo<'a> {
    /// The saved state compares of this project (the states live in this project's store).
    fn compare_dir(&self) -> PathBuf {
        self.p.meta_dir().join(COMPARE_DIR)
    }

    /// The saved live compares of lines that are gone. Called by `gc`.
    fn prune_live(&self) -> Result<usize> {
        let Ok(entries) = std::fs::read_dir(self.p.meta_dir().join(LIVE_DIR)) else { return Ok(0) };
        let lines: BTreeSet<String> = self.p.db.lines()?.into_iter().map(|l| format!("{}.json", l.id)).collect();
        let mut removed = 0;
        for e in entries.flatten() {
            if !lines.contains(&e.file_name().to_string_lossy().to_string()) && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Remove the saved compares whose states are gone, and the unreadable ones. Called by `gc`.
    pub fn prune_compares(&self) -> Result<usize> {
        let _ = self.prune_live();
        let Ok(entries) = std::fs::read_dir(self.compare_dir()) else { return Ok(0) };
        let mut removed = 0;
        for e in entries.flatten() {
            let kept = std::fs::read(e.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<SavedCompare>(&b).ok())
                .is_some_and(|s| s.format == COMPARE_FORMAT && self.p.has_state(&s.base) && self.p.has_state(&s.theirs));
            if !kept && std::fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The paths of `line` that the caller must not see: its private paths when one of its `layers` is an
    /// overlay that the caller cannot read, and its pending moves to such overlays.
    fn radar_hidden(
        &self,
        ctx: &Ctx,
        line: &Line,
        layers: &[Layer],
        private: &BTreeSet<String>,
        open: &[OpenOverlay<'a>],
    ) -> BTreeSet<String> {
        let mut hidden = BTreeSet::new();
        if layers.iter().any(|layer| !self.caller_reads(ctx, &layer.overlay, open)) {
            // Include earlier and newer private paths, so deletions cannot expose names.
            hidden.extend(private.iter().cloned());
        }
        for (path, overlay) in self.pending_moves(line) {
            // A move to `main` is a move to the public layer: its reader is every reader of the project.
            if overlay != crate::overlay::MAIN && !self.caller_reads(ctx, &overlay, open) {
                hidden.insert(path);
            }
        }
        hidden
    }

    /// Whether the caller reads the overlay `name` (`reads_overlay`; an error reads nothing). Asked
    /// again at every use, through the projects in `open`.
    fn caller_reads(&self, ctx: &Ctx, name: &str, open: &[OpenOverlay<'a>]) -> bool {
        self.reads_open_overlay(open, name, ctx.caller.uid, ctx.caller.gid).unwrap_or(false)
    }

    /// `state_src` of a state of this project, read once per call. An error is not kept: the next
    /// call reads again.
    fn tree(&self, id: &str, work: &mut Work) -> Result<Snap> {
        let key = (self.p.name.clone(), id.to_string());
        if let Some(tree) = work.trees.get(&key) {
            return Ok(tree.clone());
        }
        let tree = {
            let _t = crate::trace::span("overlap state read");
            self.state_src(id)?
        };
        work.trees.insert(key, tree.clone());
        Ok(tree)
    }

    /// The tree of our working folder to compare. APFS compares the live folder, as status does:
    /// a copy would clone every file, cache folders included. A fast line's live folder is its
    /// shadow (`working_src`): its mount has no APFS file identities.
    fn radar_tree(&self, line: &Line) -> Result<Ours> {
        let working = self.working_src(line)?;
        let w = std::fs::symlink_metadata(&working)?;
        let (source, started) = (format!("{}|{}", w.dev(), w.ino()), crate::status_cache::clock()?);
        let capture = if crate::fs::prefers_captured_comparison(&working) { Some(self.scan(line)?) } else { None };
        Ok(Ours::new(&working, capture, source, started))
    }

    /// Our head -> live folder compare: one per call, shared by all merge bases, saved per line
    /// (`saved_live`).
    fn our_head_live(&self, line: &Line, ours: &Ours, work: &mut Work) -> Result<()> {
        if work.head_live.is_none() {
            let file = self.p.meta_dir().join(LIVE_DIR).join(format!("{}.json", line.id));
            work.head_live = Some(match self.tree(&line.head, work) {
                Ok(head) => Some(saved_live(&file, &line.head, &head, ours)?),
                Err(_) => None,
            });
        }
        Ok(())
    }

    /// Plan our merge base -> head compare once per base. An identical head needs no job;
    /// an unreadable head leaves the diverged plan to the direct compare.
    fn our_base_job(&self, line: &Line, base: &str, work: &mut Work) -> Result<OurBase> {
        OurBase::plan(base, &line.head, || {
            if let Some(job) = work.ours_jobs.get(base) {
                return Ok(Some(*job));
            }
            if !matches!(work.head_live, Some(Some(_))) {
                return Ok(None);
            }
            let job = work.jobs.len();
            let state =
                StateCompare::new(&self.compare_dir(), base, &line.head, self.tree(base, work)?, self.tree(&line.head, work)?);
            work.jobs.push(state);
            work.ours_jobs.insert(base.to_string(), job);
            Ok(Some(job))
        })
    }

    pub fn overlaps(&self, ctx: &Ctx, line: &Line) -> Result<Vec<Overlap>> {
        self.can_read(ctx)?;
        // The lines are read first, and their heads pinned for the command: the compares read those
        // heads and their merge bases (ancestors of the heads). A free step of another command
        // cannot remove them while the compares run.
        // The compared lines: other lines of this machine with a working folder, read before the pins.
        let lines: Vec<Line> = self
            .lines()?
            .into_iter()
            .filter(|o| o.id != line.id && o.machine == self.p.store.machine.id && self.working(o).exists())
            .collect();
        let mut heads: Vec<&str> = lines.iter().map(|l| l.head.as_str()).collect();
        heads.push(line.head.as_str());
        self.hold(self.pin(&heads)?);
        // Taking the working folder tree is costly; do it only when another line needs it.
        let mut ours: Option<Ours> = None;
        // The hidden overlays are read once for all lines: reading them reads every overlay setting.
        // The other overlays of the layers join them when first seen (`open_layers`).
        let mut open = self.hidden_overlays()?;
        let our_layers = self.layers_in(line, &open)?;
        self.open_layers(&mut open, &our_layers);
        let our_private = self.private_paths_in(line, &our_layers, &open)?;
        let mut work = Work::default();
        let our_hidden = self.radar_hidden(ctx, line, &our_layers, &our_private, &open);
        let mut plans: Vec<OtherPlan> = Vec::new();
        // The merge bases of all lines come from one read of the history.
        let history = if lines.is_empty() { None } else { Some(self.history()?) };
        // Step 1, on this thread: the live parts and the state lookups (`Repo` is not `Sync`).
        let step1 = crate::trace::span("overlap step 1");
        let source = self.source_id(&line.head)?;
        // The time of each kind of lookup, summed over the lines.
        let (mut t_bases, mut t_trees, mut t_states) = (
            crate::trace::sum("overlap merge bases"),
            crate::trace::sum("overlap state trees"),
            crate::trace::sum("overlap their states"),
        );
        let (mut t_layers, mut t_plans) = (crate::trace::sum("overlap their layers"), crate::trace::sum("overlap overlay plans"));
        for other in lines {
            let found = t_bases.time(|| -> Result<_> {
                let other_source = self.source_id(&other.head)?;
                match history.as_ref().and_then(|h| h.merge_base(&source, &other_source)) {
                    Some(found) => Ok(found),
                    None => self.merge_base(&source, &other_source),
                }
            })?;
            let Some(base) = found else { continue };
            let root = match t_trees.time(|| self.tree(&base, &mut work)) {
                Ok(root) => root,
                Err(_) => continue,
            };
            // A line whose head cannot be read (#452) is left out, as an unreadable base is.
            let state = t_states.time(|| -> Result<_> {
                let head = self.state(&other.head)?;
                Ok(self.last_auto_since(&other, head.time)?.unwrap_or(head))
            })?;
            let Ok(theirs) = t_trees.time(|| self.tree(&state.id, &mut work)) else { continue };
            if ours.is_none() {
                let _t = crate::trace::span("overlap capture");
                ours = Some(self.radar_tree(line)?);
            }
            let our = ours.as_ref().expect("scanned above");
            self.our_head_live(line, our, &mut work)?;
            let our_base = self.our_base_job(line, &base, &mut work)?;
            // show <state>:<path> and try use project read access for both head and auto states.
            let (their_layers, their_private, hidden) = t_layers.time(|| -> Result<_> {
                let their_layers = self.layers_in(&other, &open)?;
                self.open_layers(&mut open, &their_layers);
                let their_private = self.private_paths_in(&other, &their_layers, &open)?;
                let hidden = self.radar_hidden(ctx, &other, &their_layers, &their_private, &open);
                Ok((their_layers, their_private, hidden))
            })?;
            let job = work.jobs.len();
            work.jobs.push(StateCompare::new(&self.compare_dir(), &base, &state.id, root, theirs));
            let layers = t_plans.time(|| self.overlay_plans(ctx, &open, &our_layers, &their_layers, our, &mut work))?;
            plans.push(OtherPlan {
                name: other.name.clone(),
                owner: other.owner,
                base,
                our_base,
                their_private,
                hidden,
                state_time: state.time,
                job,
                layers,
            });
        }
        drop((t_bases, t_trees, t_states, t_layers, t_plans));
        drop(step1);
        // Step 2: the saved state compares, in parallel.
        let found = {
            let _t = crate::trace::span("status overlap compares");
            run_compares(&work.jobs)?
        };
        // Step 3, on this thread: our changes per base and the caller's filters, in the order of the lines.
        let _step3 = crate::trace::span("overlap step 3");
        let mut overlaps = Vec::new();
        if let Some(our) = ours.as_ref() {
            let (ours_root, ignored) = (&our.root, &our.rules);
            let mut ours_keys: BTreeMap<String, OurChanges> = BTreeMap::new();
            let mut layer_keys: BTreeMap<(String, String), OurChanges> = BTreeMap::new();
            for p in &plans {
                let ours = match ours_keys.entry(p.base.clone()) {
                    Entry::Occupied(known) => known.into_mut(),
                    Entry::Vacant(new) => {
                        let _t = crate::trace::span("overlap ours changes");
                        let head_live = work.head_live.as_ref().and_then(Option::as_ref);
                        new.insert(p.our_base.changes(
                            &work.jobs[p.job].base,
                            ours_root,
                            ignored,
                            &our_private,
                            &found,
                            head_live,
                        )?)
                    }
                };
                let mut theirs = visible(&found[p.job], &p.their_private, ignored);
                theirs.retain(|path, _| !our_hidden.contains(path) && !p.hidden.contains(path));
                ours.retain_changed(&mut theirs, ours_root, &our_private, ignored)?;
                for (path, time) in theirs {
                    let changed_at = if time == 0 { p.state_time } else { time };
                    overlaps.push(Overlap { path, line: p.name.clone(), owner: p.owner, changed_at });
                }
                for layer in &p.layers {
                    let ours_layer = match layer_keys.entry((layer.overlay.clone(), layer.base.clone())) {
                        Entry::Occupied(known) => known.into_mut(),
                        Entry::Vacant(new) => {
                            let layer_live = work.layer_live.get(&layer.overlay).and_then(Option::as_ref);
                            new.insert(layer.our_base.changes(
                                &work.jobs[layer.job].base,
                                ours_root,
                                &our.layer_rules,
                                &our_hidden,
                                &found,
                                layer_live,
                            )?)
                        }
                    };
                    let mut theirs = visible(&found[layer.job], &p.hidden, &our.layer_rules);
                    ours_layer.retain_changed(&mut theirs, ours_root, &our_hidden, &our.layer_rules)?;
                    for (path, time) in theirs {
                        let changed_at = if time == 0 { layer.state_time } else { time };
                        overlaps.push(Overlap { path, line: p.name.clone(), owner: p.owner, changed_at });
                    }
                }
            }
        }
        overlaps.sort_by(|a, b| (&a.path, &a.line).cmp(&(&b.path, &b.line)));
        overlaps.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        Ok(overlaps)
    }

    /// The overlay layers that both lines hold: the state compares as jobs, and our overlay head
    /// compared with the live folder once per overlay.
    /// `open`: the overlays open for the call, whose projects are not opened again.
    fn overlay_plans(
        &self,
        ctx: &Ctx,
        open: &[OpenOverlay<'a>],
        our_layers: &[Layer],
        their_layers: &[Layer],
        ours: &Ours,
        work: &mut Work,
    ) -> Result<Vec<LayerPlan>> {
        let mut plans = Vec::new();
        for layer in our_layers {
            if !self.caller_reads(ctx, &layer.overlay, open) {
                continue;
            }
            let theirs = match their_layers.iter().find(|other| other.overlay == layer.overlay) {
                Some(l) => l,
                None => continue,
            };
            let opened;
            let repo = match open.iter().find(|(name, _, _)| *name == layer.overlay) {
                Some((_, _, repo)) => repo,
                None => {
                    opened = self.overlay_repo(&layer.overlay)?;
                    &opened
                }
            };
            // Use the same policy as show/try for this overlay's immutable states.
            if repo.can_read(ctx).is_err() {
                continue;
            }
            let base = match repo.merge_base(&repo.source_id(&layer.state)?, &repo.source_id(&theirs.state)?)? {
                Some(base) => base,
                None => continue,
            };
            let base_root = match repo.tree(&base, work) {
                Ok(root) => root,
                Err(_) => continue,
            };
            let their_root = match repo.tree(&theirs.state, work) {
                Ok(root) => root,
                Err(_) => continue,
            };
            if !work.layer_live.contains_key(&layer.overlay) {
                let live = match repo.tree(&layer.state, work) {
                    Ok(head) => {
                        let _t = crate::trace::span("overlap live compare");
                        Some(compare_live(&head, &ours.root, &ours.layer_rules)?)
                    }
                    Err(_) => None,
                };
                work.layer_live.insert(layer.overlay.clone(), live);
            }
            let key = (layer.overlay.clone(), base.clone());
            let our_base = OurBase::plan(&base, &layer.state, || {
                Ok(match work.layer_jobs.get(&key) {
                    Some(job) => Some(*job),
                    None if matches!(work.layer_live.get(&layer.overlay), Some(Some(_))) => {
                        let job = work.jobs.len();
                        let state = StateCompare::new(
                            &repo.compare_dir(),
                            &base,
                            &layer.state,
                            base_root.clone(),
                            repo.tree(&layer.state, work)?,
                        );
                        work.jobs.push(state);
                        work.layer_jobs.insert(key, job);
                        Some(job)
                    }
                    None => None,
                })
            })?;
            let state = repo.state(&theirs.state)?;
            let job = work.jobs.len();
            work.jobs.push(StateCompare::new(&repo.compare_dir(), &base, &theirs.state, base_root, their_root));
            plans.push(LayerPlan { overlay: layer.overlay.clone(), base, our_base, state_time: state.time, job });
        }
        Ok(plans)
    }
}

/// Observe the candidate/send boundary without requiring a privileged btrfs send in unit tests.
/// Fixtures replace only candidate discovery; content checks and caller filters still run.
#[cfg(test)]
mod test_probe {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[derive(Default)]
    pub(super) struct Probe {
        pub candidates: usize,
        pub combines: usize,
        pub fallbacks: usize,
        pub cache_reads: usize,
        pub cache_writes: usize,
        pub streams: VecDeque<Vec<changes::EntryChange>>,
    }

    thread_local! {
        static PROBE: RefCell<Probe> = RefCell::new(Probe::default());
    }

    pub(super) fn record(f: impl FnOnce(&mut Probe)) {
        PROBE.with(|p| f(&mut p.borrow_mut()));
    }

    pub(super) fn candidates(base: &(impl Src + ?Sized), child: &(impl Src + ?Sized)) -> Result<Vec<changes::EntryChange>> {
        let mut stream = None;
        record(|p| {
            p.candidates += 1;
            stream = p.streams.pop_front();
        });
        match stream {
            Some(stream) => Ok(stream),
            None => changes::candidates(base, child),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str) -> Found {
        Found { path: path.into(), old_path: None, is_dir: false, time: 1 }
    }

    /// A path that both parts changed goes to the recheck; the other paths of both parts stay.
    #[test]
    fn a_path_changed_in_both_parts_is_checked_again() {
        let saved = vec![f("a"), f("b")];
        let live = vec![f("b"), f("c")];
        let merged = merge_changes(&saved, &live, |p| Ok((p != "b").then(|| f(p))));
        let paths: Vec<String> = merged.unwrap().into_iter().map(|x| x.path).collect();
        assert_eq!(paths, ["a", "c"]);
    }

    fn moved(old: &str, new: &str) -> Found {
        Found { old_path: Some(old.into()), ..f(new) }
    }

    /// The merged paths, sorted, and the rechecked paths.
    fn merged(saved: &[Found], live: &[Found], rechecked: &[&str]) -> Result<(Vec<String>, Vec<String>)> {
        let mut asked = Vec::new();
        let out = merge_changes(saved, live, |p| {
            asked.push(p.to_string());
            Ok(rechecked.contains(&p).then(|| f(p)))
        })?;
        let mut paths: Vec<String> = out.into_iter().map(|f| f.path).collect();
        paths.sort();
        Ok((paths, asked))
    }

    fn meta(root: &Path, path: &str) -> Result<fsutil::Meta> {
        fsutil::lmeta(root, path).ok_or_else(|| anyhow!("no {path}"))
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Renames combine without a direct compare, as their two paths; only the paths that both
    /// parts changed are checked again.
    #[test]
    fn renames_in_either_part_combine() -> Result<()> {
        // Saved o -> x, live x -> n: o and n; x is checked and gone in both trees.
        assert_eq!(merged(&[moved("o", "x")], &[moved("x", "n")], &[])?, (strings(&["n", "o"]), strings(&["x"])));
        // Saved o -> x, then x edited: x is checked again.
        assert_eq!(merged(&[moved("o", "x")], &[f("x")], &["x"])?, (strings(&["o", "x"]), strings(&["x"])));
        // Saved o -> x, then x deleted.
        assert_eq!(merged(&[moved("o", "x")], &[f("x")], &[])?, (strings(&["o"]), strings(&["x"])));
        // Saved o -> x, live x -> o: both paths are checked, and nothing changed.
        assert_eq!(merged(&[moved("o", "x")], &[moved("x", "o")], &[])?, (strings(&[]), strings(&["o", "x"])));
        // Renames of different paths in the two parts need no check.
        assert_eq!(merged(&[moved("a", "b")], &[moved("c", "d")], &[])?, (strings(&["a", "b", "c", "d"]), strings(&[])));
        Ok(())
    }

    /// The paths of combined changes; `None` when they are checked one by one.
    fn paths(ours: &OurChanges) -> Option<&BTreeSet<String>> {
        match ours {
            OurChanges::Paths(paths) => Some(paths),
            OurChanges::Checked { .. } => None,
        }
    }

    /// A rule file changed in the live compare, ignored or not, marks it; a rule file in an ignored
    /// folder cannot change what matters. The marked compare does not combine and no direct base
    /// compare runs: the paths are checked one by one.
    #[test]
    fn a_rule_file_change_checks_paths_instead_of_combining() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        std::fs::create_dir_all(live.join("sub"))?;
        std::fs::create_dir_all(live.join("build"))?;
        std::fs::write(live.join(".gitignore"), "build/\n.gitignore\n")?;
        let rules = Rules::new(&live);
        for (changed, marked) in [(".gitignore", true), ("sub/.layrignore", true), ("build/.gitignore", false), ("a.txt", false)]
        {
            if !live.join(changed).exists() {
                std::fs::write(live.join(changed), "x")?;
            }
            reset_probe(vec![vec![changes::EntryChange::Add { path: changed.into(), new: meta(&live, changed)? }]]);
            let head_live = compare_live(&head, &live, &rules)?;
            assert_eq!(head_live.rules_changed, marked, "{changed}");
            reset_probe(Vec::new());
            let ours = OurBase::Diverged(Some(0)).changes(
                &crate::repo::snap(&head),
                &live,
                &rules,
                &BTreeSet::new(),
                &[Vec::new()],
                Some(&head_live),
            )?;
            assert_eq!(paths(&ours).is_none(), marked, "{changed}");
            test_probe::record(|p| assert_eq!((p.candidates, p.combines, p.fallbacks), (0, 1, 0), "{changed}"));
        }
        // The old path of a rename counts too.
        let renamed = changes::EntryChange::Rename {
            path: "kept".into(),
            old_path: ".layrignore".into(),
            old: meta(&live, "a.txt")?,
            new: meta(&live, "a.txt")?,
            data: crate::compare::DataEvidence::Unknown,
        };
        reset_probe(vec![vec![renamed]]);
        assert!(compare_live(&head, &live, &rules)?.rules_changed);
        Ok(())
    }

    /// Without combined changes, each path asked for is compared on its own, once: hidden and
    /// ignored paths are not ours.
    #[test]
    fn checked_changes_compare_each_asked_path() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        for (path, old, new) in [("changed", "1", "2"), ("same", "1", "1"), ("hidden", "1", "2"), ("ignored", "1", "2")] {
            std::fs::write(head.join(path), old)?;
            std::fs::write(live.join(path), new)?;
        }
        std::fs::write(live.join(".gitignore"), "ignored\n")?;
        let rules = Rules::new(&live);
        let hidden = BTreeSet::from(["hidden".to_string()]);
        let mut ours = OurChanges::Checked { base: crate::repo::snap(&head), seen: BTreeMap::new() };
        let mut theirs: BTreeMap<String, i64> =
            ["changed", "same", "hidden", "ignored", "absent"].into_iter().map(|p| (p.to_string(), 1)).collect();
        ours.retain_changed(&mut theirs, &live, &hidden, &rules)?;
        assert_eq!(theirs.into_keys().collect::<Vec<_>>(), ["changed"]);
        std::fs::write(live.join("same"), "3")?;
        let mut theirs = BTreeMap::from([("same".to_string(), 1)]);
        ours.retain_changed(&mut theirs, &live, &hidden, &rules)?;
        assert!(theirs.is_empty(), "a path is compared once per status");
        Ok(())
    }

    /// `compare` keeps a change when either side is not left out: a file replaced by an ignored
    /// folder (the merge then checks the path again). A rule file that its own rules ignore is no
    /// change for callers that read the changes (`mergecmd::changes_contained`); only the live
    /// compare marks it.
    #[test]
    fn compare_keeps_a_change_with_one_visible_side_and_marks_ignored_rule_files() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        std::fs::write(head.join("b"), "file")?;
        std::fs::create_dir(live.join("b"))?;
        std::fs::write(live.join(".gitignore"), "b/\n.gitignore\n")?;
        let rules = Rules::new(&live);
        assert!(rules.ignored("b", true) && !rules.ignored("b", false) && rules.ignored(".gitignore", false));
        let stream = vec![
            changes::EntryChange::Add { path: ".gitignore".into(), new: meta(&live, ".gitignore")? },
            changes::EntryChange::ReplaceType { path: "b".into(), old: meta(&head, "b")?, new: meta(&live, "b")? },
        ];
        reset_probe(vec![stream.clone()]);
        let found = compare(&head, &live, &rules)?;
        assert_eq!(found.iter().map(|f| (f.path.as_str(), f.is_dir)).collect::<Vec<_>>(), [("b", true)]);
        assert!(our_paths(&found, &BTreeSet::new(), &rules).is_empty());
        // Only an ignored rule file changed: the shared result is empty, the live compare marks it.
        reset_probe(vec![stream[..1].to_vec()]);
        assert!(compare(&head, &live, &rules)?.is_empty());
        reset_probe(vec![stream[..1].to_vec()]);
        assert!(compare_live(&head, &live, &rules)?.rules_changed);
        Ok(())
    }

    /// A rename to an ignored path is still the removal of its old path, so a saved rename to
    /// that old path is checked again.
    #[test]
    fn a_rename_to_an_ignored_path_keeps_its_old_path() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        std::fs::write(head.join("b"), "moved")?;
        std::fs::create_dir(live.join("ignored"))?;
        std::fs::write(live.join("ignored/c"), "moved")?;
        std::fs::write(live.join(".gitignore"), "ignored/\n")?;
        let rules = Rules::new(&live);
        assert!(rules.ignored("ignored/c", false));
        reset_probe(vec![vec![changes::EntryChange::Rename {
            path: "ignored/c".into(),
            old_path: "b".into(),
            old: meta(&head, "b")?,
            new: meta(&live, "ignored/c")?,
            data: crate::compare::DataEvidence::Unknown,
        }]]);
        let head_live = compare(&head, &live, &rules)?;
        assert_eq!(merged(&[], &head_live, &[])?, (strings(&["b"]), strings(&[])));
        // Saved a -> b, then b -> ignored/c: b is gone in both trees, only a is left.
        assert_eq!(merged(&[moved("a", "b")], &head_live, &[])?, (strings(&["a"]), strings(&["b"])));
        Ok(())
    }

    /// Our changes count both paths of a rename on their own: a hidden or ignored path leaves
    /// out only itself, so the two routes to our changes (a rename, or its two paths) agree.
    #[test]
    fn our_paths_count_each_rename_path_alone() {
        let rules = Rules::new(Path::new("/nonexistent"));
        let found = [moved("old", "new")];
        let split: Vec<Found> = endpoints(&found).collect();
        for hidden in [BTreeSet::new(), BTreeSet::from(["old".into()]), BTreeSet::from(["new".into()])] {
            assert_eq!(our_paths(&found, &hidden, &rules), our_paths(&split, &hidden, &rules));
        }
        let kept = our_paths(&found, &BTreeSet::from(["old".into()]), &rules);
        assert_eq!(kept, BTreeSet::from(["new".to_string()]));
    }

    #[test]
    fn visible_changes_keep_both_rename_paths_and_filter_hidden_names() {
        let mut moved = f("new");
        moved.old_path = Some("old".into());
        let root = Path::new("/nonexistent");
        let rules = Rules::new(root);
        let found = [moved];
        let paths = visible(&found, &BTreeSet::new(), &rules);
        assert_eq!(paths.keys().map(String::as_str).collect::<Vec<_>>(), ["new", "old"]);
        assert!(visible(&found, &BTreeSet::from(["old".into()]), &rules).is_empty());
    }

    fn unmarked(found: Vec<Found>) -> LiveChanges {
        LiveChanges { found, rules_changed: false }
    }

    #[test]
    fn combined_changes_do_not_need_a_direct_compare() -> Result<()> {
        let root = Path::new("/nonexistent");
        let live = unmarked(vec![f("live")]);
        // A direct compare cannot read this path, so success proves the parts sufficed.
        let ours = OurBase::Diverged(Some(0)).changes(
            &crate::repo::snap(root),
            root,
            &Rules::new(root),
            &BTreeSet::new(),
            &[vec![f("saved")]],
            Some(&live),
        )?;
        assert_eq!(paths(&ours), Some(&BTreeSet::from(["live".to_string(), "saved".to_string()])));
        Ok(())
    }

    struct Trees(PathBuf);

    impl Trees {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("layr-radar-{}", crate::ids::new_id()));
            std::fs::create_dir_all(root.join("head")).unwrap();
            std::fs::create_dir_all(root.join("live")).unwrap();
            Self(root)
        }

        fn head(&self) -> PathBuf {
            self.0.join("head")
        }
        fn live(&self) -> PathBuf {
            self.0.join("live")
        }
    }

    impl Drop for Trees {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reset_probe(streams: Vec<Vec<changes::EntryChange>>) {
        test_probe::record(|p| *p = test_probe::Probe { streams: streams.into(), ..Default::default() });
    }

    fn bytes(found: &[Found]) -> Vec<u8> {
        serde_json::to_vec(found).unwrap_or_default()
    }

    #[test]
    fn at_head_reuses_renames_without_a_job_combine_or_send() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        std::fs::write(head.join("old"), "rename content")?;
        std::fs::write(live.join("new"), "rename content")?;
        std::fs::write(live.join("private"), "hidden content")?;
        let stream = vec![
            changes::EntryChange::Rename {
                path: "new".into(),
                old_path: "old".into(),
                old: meta(&head, "old")?,
                new: meta(&live, "new")?,
                data: crate::compare::DataEvidence::Unknown,
            },
            changes::EntryChange::Add { path: "private".into(), new: meta(&live, "private")? },
        ];
        let rules = Rules::new(&live);
        reset_probe(vec![stream]);
        let head_live = compare_live(&head, &live, &rules)?;
        let base = crate::repo::snap(&head);
        // Both main and overlay callers pass the head of their own immutable tree. An overlay
        // state deliberately differs from the main head, so using the main ID would queue.
        for own_head in ["main-head", "overlay-state"] {
            for hidden in [BTreeSet::new(), BTreeSet::from(["private".into()]), BTreeSet::from(["old".into()])] {
                reset_probe(Vec::new());
                let plan = OurBase::plan(own_head, own_head, || panic!("AtHead queued a saved send"))?;
                let fast = plan.changes(&base, &live, &rules, &hidden, &[], Some(&head_live))?;
                test_probe::record(|p| {
                    assert_eq!((p.candidates, p.combines, p.fallbacks), (0, 0, 0));
                    assert_eq!((p.cache_reads, p.cache_writes), (0, 0));
                });
                let slow = OurBase::Diverged(Some(0)).changes(&base, &live, &rules, &hidden, &[Vec::new()], Some(&head_live))?;
                // A rename combines as its two paths: no candidate stream and no direct compare.
                test_probe::record(|p| assert_eq!((p.candidates, p.combines, p.fallbacks), (0, 1, 0)));
                assert_eq!(paths(&fast), paths(&slow));
                assert_eq!(paths(&fast), Some(&our_paths(&head_live.found, &hidden, &rules)));
            }
        }
        Ok(())
    }

    #[test]
    fn diverged_empty_saved_diff_is_not_identity_and_missing_live_still_compares() -> Result<()> {
        let root = Path::new("/nonexistent");
        let (base, rules, hidden) = (crate::repo::snap(root), Rules::new(root), BTreeSet::new());
        let live = unmarked(vec![f("mtime-can-differ")]);
        reset_probe(Vec::new());
        let mut queued = 0;
        let plan = OurBase::plan("base", "head", || {
            queued += 1;
            Ok(Some(0))
        })?;
        assert_eq!(queued, 1);
        let ours = plan.changes(&base, root, &rules, &hidden, &[Vec::new()], Some(&live))?;
        assert_eq!(paths(&ours), Some(&BTreeSet::from(["mtime-can-differ".to_string()])));
        test_probe::record(|p| assert_eq!(p.combines, 1));
        // An unreadable head cannot supply a live diff, even when the IDs match.
        reset_probe(vec![Vec::new()]);
        let ours = OurBase::AtHead.changes(&base, root, &rules, &hidden, &[], None)?;
        assert!(paths(&ours).is_some_and(BTreeSet::is_empty));
        test_probe::record(|p| assert_eq!((p.candidates, p.fallbacks), (1, 1)));
        Ok(())
    }

    /// An entry made under another local ignore policy is a miss, and the compare replaces it in
    /// the same file: one entry per compared pair, whatever the policies were.
    #[test]
    fn a_saved_compare_of_another_local_policy_is_replaced() -> Result<()> {
        let trees = Trees::new();
        let (base, theirs) = (crate::repo::snap(&trees.head()), crate::repo::snap(&trees.live()));
        let job = StateCompare::new(&trees.0.join("cache"), "base", "theirs", base, theirs);
        job.write("another policy", &[f("stale")])?;
        reset_probe(vec![Vec::new()]);
        assert!(job.run()?.is_empty());
        test_probe::record(|p| assert_eq!((p.candidates, p.cache_writes), (1, 1)));
        assert_eq!(std::fs::read_dir(trees.0.join("cache"))?.count(), 1);
        reset_probe(Vec::new());
        assert!(job.run()?.is_empty());
        test_probe::record(|p| assert_eq!((p.candidates, p.cache_reads, p.cache_writes), (0, 1, 0)));
        Ok(())
    }

    #[test]
    fn identical_states_skip_send_and_cache_even_when_a_cache_entry_exists() {
        let trees = Trees::new();
        let snap = crate::repo::snap(&trees.head());
        let job = StateCompare::new(&trees.0.join("cache"), "same-state", "same-state", snap.clone(), snap);
        // Poison the otherwise-valid cache to prove identity is answered before reading it.
        job.write(&local_digest(&Rules::of(&job.theirs)).unwrap_or_default(), &[f("must-not-be-read")]).unwrap();
        let before = std::fs::read(&job.file).unwrap();
        reset_probe(Vec::new());
        assert!(job.run().unwrap().is_empty());
        assert_eq!(std::fs::read(&job.file).unwrap(), before);
        test_probe::record(|p| assert_eq!((p.candidates, p.cache_reads, p.cache_writes), (0, 0, 0)));
        std::fs::remove_dir_all(job.file.parent().unwrap()).unwrap();
        assert!(job.run().unwrap().is_empty());
        assert!(!job.file.parent().unwrap().exists());
        test_probe::record(|p| assert_eq!((p.candidates, p.cache_reads, p.cache_writes), (0, 0, 0)));
        // The previous cold path on the same immutable input also returns no changes, but sends.
        reset_probe(vec![Vec::new()]);
        let slow = compare(&job.base, &job.theirs, &Rules::of(&job.theirs)).unwrap();
        assert_eq!(bytes(&job.run().unwrap()), bytes(&slow));
        test_probe::record(|p| assert_eq!(p.candidates, 1));
    }

    fn stat(dev: u64, ino: u64, time: i64) -> FileFingerprint {
        let t = crate::status_cache::Timestamp { seconds: time, nanos: 0 };
        let stat = crate::status_cache::FileStat {
            dev,
            ino,
            size: 1,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            nlink: 1,
            mtime: t.clone(),
            ctime: t,
        };
        FileFingerprint::Verified { stat, evidence: None }
    }

    /// Two captures of one folder differ in the device of every entry and the times of the root;
    /// in the live folder, and any other difference, they are a new key. An entry that cannot be
    /// trusted gives none.
    #[test]
    fn the_live_key_is_the_same_for_each_capture_of_an_unchanged_folder() -> Result<()> {
        let scan = |dev: u64, ino: u64, root_time: i64| -> BTreeMap<String, FileFingerprint> {
            BTreeMap::from([("".to_string(), stat(dev, 256, root_time)), ("a".to_string(), stat(dev, ino, 10))])
        };
        let key = |scan: &BTreeMap<String, FileFingerprint>| live_digest(scan, "local", "source", 100);
        let first = key(&scan(1, 257, 10))?;
        assert!(first.is_some());
        assert_ne!(key(&scan(2, 257, 10))?, first);
        assert_ne!(key(&scan(1, 257, 20))?, first);
        let captured = key(&capture_relative(scan(1, 257, 10)))?;
        assert!(captured.is_some());
        assert_eq!(key(&capture_relative(scan(2, 257, 20)))?, captured);
        assert_ne!(key(&capture_relative(scan(1, 258, 10)))?, captured);
        assert_ne!(key(&scan(1, 258, 10))?, first);
        assert_ne!(live_digest(&scan(1, 257, 10), "other local", "source", 100)?, first);
        assert_ne!(live_digest(&scan(1, 257, 10), "local", "other source", 100)?, first);
        // Written within the second the folder was read in: it may change again with the same stamp.
        assert_eq!(live_digest(&scan(1, 257, 10), "local", "source", 10)?, None);
        // 10.999 and a clock of 11 can be a millisecond apart: a same-size rewrite keeps the stamps.
        assert_eq!(live_digest(&scan(1, 257, 10), "local", "source", 11)?, None);
        assert!(live_digest(&scan(1, 257, 10), "local", "source", 12)?.is_some());
        let mut mapped = scan(1, 257, 10);
        mapped.insert("b".to_string(), FileFingerprint::Unverifiable);
        assert_eq!(key(&mapped)?, None);
        Ok(())
    }

    /// A file or folder mounted inside the live folder gives no key.
    #[test]
    fn a_mount_inside_the_live_folder_gives_no_key() -> Result<()> {
        let trees = Trees::new();
        let live = trees.live();
        std::fs::create_dir(live.join("dir"))?;
        std::fs::write(live.join("file"), "1")?;
        let outside = [PathBuf::from("/"), live.clone(), trees.0.join("outside"), trees.0.join("live2")];
        assert!(!mounted_inside(&live, &outside));
        for inside in [live.join("dir"), live.join("file"), live.join("gone")] {
            assert!(mounted_inside(&live, &[outside.as_slice(), &[inside]].concat()));
        }
        // The folder through a link: the mount points name the resolved path.
        std::os::unix::fs::symlink(&trees.0, trees.0.join("link"))?;
        let ours = Ours::new(&trees.0.join("link/live"), None, "work".into(), crate::status_cache::clock()? + 2);
        assert!(scan_digest(&ours, "local")?.is_some());
        let resolved = std::fs::canonicalize(&ours.root)?;
        assert!(!mounted_inside(&ours.root, &[resolved.join("file")]));
        assert!(mounted_inside(&resolved, &[resolved.join("file")]));
        Ok(())
    }

    /// The live folder (no capture) is observed again after the compare: unchanged, its compare is
    /// saved and the next status reuses it. Its root stays in the key: a top-level file made and
    /// removed between two reads changes it.
    #[test]
    fn an_unchanged_live_folder_reuses_its_compare_and_keeps_its_root_in_the_key() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        std::fs::write(head.join("a"), "1")?;
        std::fs::write(live.join("a"), "2")?;
        // As root on btrfs, an entry's evidence is the transaction of its inode item: the first
        // read after a write moves the access time, and a delayed inode item reaches the tree in
        // any commit, so the key can change while the files do not. Access times ahead of the
        // other times stay on read (relatime); the commit puts them in the tree before the scan.
        let ahead = std::fs::FileTimes::new().set_accessed(std::time::SystemTime::now() + std::time::Duration::from_secs(3600));
        for path in [&live, &live.join("a")] {
            std::fs::File::open(path)?.set_times(ahead)?;
        }
        #[cfg(target_os = "linux")]
        rustix::fs::syncfs(std::fs::File::open(&live)?)?;
        let started = crate::status_cache::clock()? + 2;
        let ours = Ours::new(&live, None, "work".into(), started);
        let (file, head) = (trees.0.join("live.json"), crate::repo::snap(&head));
        reset_probe(Vec::new());
        let cold = saved_live(&file, "head", &head, &ours)?;
        assert_eq!(bytes(&saved_live(&file, "head", &head, &ours)?.found), bytes(&cold.found));
        test_probe::record(|p| assert_eq!(p.candidates, 1));
        let before = scan_digest(&ours, "local")?;
        assert!(before.is_some());
        std::fs::write(live.join(".gitignore"), "a\n")?;
        std::fs::remove_file(live.join(".gitignore"))?;
        assert_ne!(scan_digest(&ours, "local")?, before);
        Ok(())
    }

    /// A rule file inside a nested tree, which the scan leaves out, classifies a path of our head:
    /// edited in place, the scan is the same, and the saved compare is not used.
    #[test]
    fn a_changed_rule_file_that_the_scan_leaves_out_compares_again() -> Result<()> {
        let trees = Trees::new();
        let (head, live) = (trees.head(), trees.live());
        if !crate::fs::host().namespace.supported(&live) {
            return Ok(());
        }
        std::fs::create_dir(head.join("d"))?;
        std::fs::write(head.join("d/a"), "1")?;
        let nested = live.join("d");
        crate::fs::create(&nested)?;
        struct Nested(PathBuf);
        impl Drop for Nested {
            fn drop(&mut self) {
                let _ = crate::fs::remove(&self.0);
            }
        }
        let _nested = Nested(nested.clone());
        std::fs::write(nested.join(".gitignore"), "a\n")?;
        // As in the test above: on btrfs the first read after a write moves the access time, and its
        // inode item can reach the tree between the two scans (integration on 5d1bf07, 2026-11-23).
        // Access times ahead of the other times stay on read; the commit puts them in the tree first.
        let ahead = std::fs::FileTimes::new().set_accessed(std::time::SystemTime::now() + std::time::Duration::from_secs(3600));
        for path in [&live, &nested] {
            std::fs::File::open(path)?.set_times(ahead)?;
        }
        #[cfg(target_os = "linux")]
        rustix::fs::syncfs(std::fs::File::open(&live)?)?;
        let ours = || -> Result<Ours> {
            let started = crate::status_cache::clock()? + 2;
            Ok(Ours::new(&live, None, "work".into(), started))
        };
        let (file, head) = (trees.0.join("live.json"), crate::repo::snap(&head));
        let removed = |live: &LiveChanges| live.found.iter().any(|f| f.path == "d/a");
        let first = ours()?;
        let key = scan_digest(&first, "local")?;
        assert!(key.is_some() && !crate::fs::observe_tree(&live)?.contains_key("d/.gitignore"));
        assert!(!removed(&saved_live(&file, "head", &head, &first)?));
        std::fs::write(nested.join(".gitignore"), "")?;
        let second = ours()?;
        assert_eq!(scan_digest(&second, "local")?, key);
        assert!(removed(&saved_live(&file, "head", &head, &second)?));
        Ok(())
    }

    /// A private folder with the folder `head`, the volume `work` and the volumes nested in it,
    /// removed on drop.
    struct Volumes(PathBuf, Vec<PathBuf>);

    impl Volumes {
        /// `None` where this filesystem makes no volumes.
        fn new() -> Result<Option<Volumes>> {
            if !crate::fs::host().namespace.supported(&std::env::temp_dir()) {
                return Ok(None);
            }
            let volumes = Volumes(std::env::temp_dir().join(format!("layr-radar-live-{}", crate::ids::new_id())), Vec::new());
            std::fs::create_dir_all(volumes.head())?;
            // A frozen capture is published only into a private folder.
            std::fs::set_permissions(&volumes.0, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            crate::fs::create(&volumes.work())?;
            Ok(Some(volumes))
        }
        fn head(&self) -> PathBuf {
            self.0.join("head")
        }
        fn work(&self) -> PathBuf {
            self.0.join("work")
        }
        /// A volume at `rel` in `work`.
        fn nest(&mut self, rel: &str) -> Result<PathBuf> {
            let path = self.work().join(rel);
            crate::fs::create(&path)?;
            self.1.push(path.clone());
            Ok(path)
        }
        /// Our folder as a status two seconds from now reads it: the files are a full second older
        /// than its clock.
        fn capture(&self) -> Result<Ours> {
            let at = self.0.join(crate::store::temp_name());
            let capture = Temp::capture(&self.work(), at, crate::fs::Protection::Frozen, crate::fs::CapturePurpose::Scan)?;
            Ok(Ours::new(&self.work(), Some(capture), "work".into(), crate::status_cache::clock()? + 2))
        }
    }

    impl Drop for Volumes {
        fn drop(&mut self) {
            for nested in self.1.iter().rev() {
                let _ = crate::fs::remove(nested);
            }
            let _ = crate::fs::remove(&self.work());
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A nested volume is an empty folder in a capture, but its rule files, which status reads in
    /// the working folder, hide our head's paths in it. Changed, the next status compares again.
    #[test]
    fn rules_in_a_nested_volume_of_the_working_folder_classify_its_capture() -> Result<()> {
        let Some(mut volumes) = Volumes::new()? else { return Ok(()) };
        std::fs::create_dir(volumes.head().join("d"))?;
        std::fs::write(volumes.head().join("d/a"), "1")?;
        let nested = volumes.nest("d")?;
        std::fs::write(nested.join(".gitignore"), "a\n")?;
        let mut ours = volumes.capture()?;
        if ours.root.join("d/.gitignore").exists() {
            // A capture by copy holds the nested tree: the rules are the same in both.
            return Ok(());
        }
        let (file, head) = (volumes.0.join("live.json"), crate::repo::snap(&volumes.head()));
        let removed = |live: &LiveChanges| live.found.iter().any(|f| f.path == "d/a");
        reset_probe(Vec::new());
        assert!(!removed(&saved_live(&file, "head", &head, &ours)?));
        // The next statuses read the rules again; one capture keeps the scan the same (a new
        // capture can give the empty folder a new inode).
        ours.rules = Rules::new(&volumes.work()).recording();
        assert!(!removed(&saved_live(&file, "head", &head, &ours)?));
        test_probe::record(|p| assert_eq!(p.candidates, 1));
        std::fs::write(nested.join(".gitignore"), "")?;
        ours.rules = Rules::new(&volumes.work()).recording();
        assert!(removed(&saved_live(&file, "head", &head, &ours)?));
        test_probe::record(|p| assert_eq!(p.candidates, 2));
        Ok(())
    }

    /// A volume nested at the top of the folder is left out of the scan (another device) and the
    /// capture's root is left out of its key; its name is in the key.
    #[test]
    fn a_volume_nested_at_the_top_changes_the_key_of_a_capture() -> Result<()> {
        let Some(mut volumes) = Volumes::new()? else { return Ok(()) };
        std::fs::write(volumes.work().join("a"), "1")?;
        let first = volumes.capture()?;
        if std::fs::symlink_metadata(first.root.join("a"))?.ino() != std::fs::symlink_metadata(volumes.work().join("a"))?.ino() {
            // A capture by copy (APFS) has new inodes: there is nothing to reuse.
            return Ok(());
        }
        let key = scan_digest(&first, "local")?;
        assert!(key.is_some() && scan_digest(&volumes.capture()?, "local")? == key);
        volumes.nest("n")?;
        let nested = volumes.capture()?;
        assert!(!crate::fs::observe_tree(&nested.root)?.contains_key("n"));
        assert_ne!(scan_digest(&nested, "local")?, key);
        Ok(())
    }

    /// Captures of an unchanged folder reuse the saved compare of the first one; a new local
    /// exclude, which the observations leave out (`.git`), compares again.
    #[test]
    fn a_second_capture_of_an_unchanged_folder_reuses_the_saved_live_compare() -> Result<()> {
        let Some(volumes) = Volumes::new()? else { return Ok(()) };
        let (head, work) = (volumes.head(), volumes.work());
        std::fs::create_dir_all(work.join(".git/info"))?;
        for (path, old, new) in [("a", "1", "2"), ("b", "1", "2"), ("same", "1", "1")] {
            std::fs::write(head.join(path), old)?;
            std::fs::write(work.join(path), new)?;
        }
        let capture = |exclude: &str| -> Result<Ours> {
            std::fs::write(work.join(crate::ignore_rules::LOCAL_EXCLUDE), exclude)?;
            volumes.capture()
        };
        let (first, second) = (capture("b\n")?, capture("b\n")?);
        let ino = |root: &Path| std::fs::symlink_metadata(root.join("a")).map(|m| m.ino());
        if ino(&first.root)? != ino(&work)? {
            // A capture by copy (APFS) has new inodes: there is nothing to reuse.
            return Ok(());
        }
        let (file, head) = (volumes.0.join("live.json"), crate::repo::snap(&head));
        let changed = |live: &LiveChanges| live.found.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
        reset_probe(Vec::new());
        let cold = saved_live(&file, "head", &head, &first)?;
        assert_eq!(changed(&cold), ["a"]);
        let warm = saved_live(&file, "head", &head, &second)?;
        assert_eq!(bytes(&warm.found), bytes(&cold.found));
        test_probe::record(|p| assert_eq!(p.candidates, 1));
        let third = capture("a\n")?;
        assert_eq!(changed(&saved_live(&file, "head", &head, &third)?), ["b"]);
        test_probe::record(|p| assert_eq!(p.candidates, 2));
        Ok(())
    }
}
