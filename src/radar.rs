//! On-demand path overlaps between live lines. Nothing is written to permanent history.
//!
//! The changes of our live folder against a merge base are the combination of two parts: the
//! compare of the base with our head (a saved state compare, cached under the owning project's
//! `meta/` folder) and one compare of our head with the live folder, which runs once per call.
//! A path that both parts changed is checked again against the base. Inside a saved compare, the
//! ignore rules of the saved state skip ignored paths before any content compare. The caller's
//! ignore rules and hidden paths run after the cache on every call.

use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil::{self, Src};
use crate::ignore_rules::Rules;
use crate::model::Line;
use crate::repo::{skip_path, Repo, Snap};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Format of the saved state compares. Bump it when `changes::candidates`,
/// `changes::really_changed`, `skip_path` or the ignore rules inside the compare change their
/// results: older entries then miss. 2: the ignore rules of the saved state skip paths.
/// 3: a change records whether its path is missing in the parent (`added`) or the child (`removed`).
const COMPARE_FORMAT: u32 = 3;
/// The folder under `meta/` that holds the saved state compares. `gc` removes the entries whose
/// states are gone.
const COMPARE_DIR: &str = "overlap-compares";
/// Threads that compare saved states at once.
const COMPARE_THREADS: usize = 4;

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
struct Found {
    path: String,
    old_path: Option<String>,
    is_dir: bool,
    /// Milliseconds of the new file, 0 when there is none.
    time: i64,
    /// The path is missing in the parent tree of the compare.
    added: bool,
    /// The path is missing in the child tree of the compare.
    removed: bool,
}

#[derive(Serialize, Deserialize)]
struct SavedCompare {
    format: u32,
    base: String,
    theirs: String,
    found: Vec<Found>,
}

/// The real changes between two trees. Paths that `skip_path` or `rules` ignore are left out
/// before the content compare, so ignored build output costs no content reads.
fn compare(base: &(impl Src + ?Sized), child: &(impl Src + ?Sized), rules: &Rules) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for change in changes::candidates(base, child)? {
        let is_dir = change.new.as_ref().or(change.old.as_ref()).is_some_and(|m| m.is_dir());
        if skip_path(&change.path) || rules.ignored(&change.path, is_dir) {
            continue;
        }
        if !changes::really_changed(base, child, &change)? {
            continue;
        }
        let time = change.new.as_ref().map(|m| m.mtime * 1000).unwrap_or(0);
        let (added, removed) = (change.old.is_none(), change.new.is_none());
        out.push(Found { path: change.path, old_path: change.old_path, is_dir, time, added, removed });
    }
    Ok(out)
}

/// The test of `compare` for one path: whether the path changed from `base` to `live`.
fn recheck(base: &(impl Src + ?Sized), live: &(impl Src + ?Sized), rules: &Rules, path: &str) -> Result<Option<Found>> {
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
    let (added, removed) = (old.is_none(), new.is_none());
    let change = changes::Change { path: path.to_string(), old_path: None, old, new, ranges: None };
    if !changes::really_changed(base, live, &change)? {
        return Ok(None);
    }
    Ok(Some(Found { path: path.to_string(), old_path: None, is_dir, time, added, removed }))
}

/// The changes of the live tree against the base, from the two parts. A path that the saved part
/// and the live part both changed is checked again with `recheck`, because it may have changed
/// back. `None` when a rename is in either part: renames pair files across the whole tree.
fn merge_changes(
    saved: &[Found],
    head_live: &[Found],
    mut recheck: impl FnMut(&str) -> Result<Option<Found>>,
) -> Result<Option<Vec<Found>>> {
    if saved.iter().chain(head_live).any(|f| f.old_path.is_some()) {
        return Ok(None);
    }
    let live_paths: BTreeSet<&str> = head_live.iter().map(|f| f.path.as_str()).collect();
    let saved_paths: BTreeSet<&str> = saved.iter().map(|f| f.path.as_str()).collect();
    let mut out: Vec<Found> = saved.iter().filter(|f| !live_paths.contains(f.path.as_str())).cloned().collect();
    for f in head_live {
        if saved_paths.contains(f.path.as_str()) {
            out.extend(recheck(&f.path)?);
        } else {
            out.push(f.clone());
        }
    }
    Ok(Some(out))
}

/// The changes of `live` against `base` from the two parts (`merge_changes`). `None` when the
/// direct compare must decide: a rename, or a file that the parts show as added and as removed
/// (git pairs such files as renames when their content is similar).
fn combine(
    base: &(impl Src + ?Sized),
    live: &(impl Src + ?Sized),
    rules: &Rules,
    saved: &[Found],
    head_live: &[Found],
) -> Result<Option<Vec<Found>>> {
    let Some(merged) = merge_changes(saved, head_live, |path| recheck(base, live, rules, path))? else {
        return Ok(None);
    };
    let lone: Vec<changes::Change> = merged
        .iter()
        .filter(|f| !f.is_dir && (f.added || f.removed))
        .map(|f| changes::Change {
            path: f.path.clone(),
            old_path: None,
            old: if f.removed { fsutil::lmeta(base, &f.path) } else { None },
            new: if f.added { fsutil::lmeta(live, &f.path) } else { None },
            ranges: None,
        })
        .collect();
    if changes::with_renames(base, live, lone).iter().any(|c| c.old_path.is_some()) {
        return Ok(None);
    }
    Ok(Some(merged))
}

/// The changes of `live` against `base`, as `compare` gives them. With both parts (the saved
/// compare and the live compare of our head), they combine; otherwise the direct compare runs.
fn changes_vs_base(
    base: &(impl Src + ?Sized),
    live: &(impl Src + ?Sized),
    rules: &Rules,
    parts: Option<(&[Found], &[Found])>,
) -> Result<Vec<Found>> {
    if let Some((saved, head_live)) = parts {
        if let Some(found) = combine(base, live, rules, saved, head_live)? {
            return Ok(found);
        }
    }
    let _t = crate::trace::span("overlap live compare");
    compare(base, live, rules)
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
        let mut h = Sha1::new();
        h.update(format!("{COMPARE_FORMAT}\n{base_id}\n{theirs_id}"));
        let file = dir.join(format!("{:x}.json", h.finalize()));
        StateCompare { file, base_id: base_id.to_string(), theirs_id: theirs_id.to_string(), base, theirs }
    }

    fn run(&self) -> Result<Vec<Found>> {
        if let Some(found) = self.read() {
            return Ok(found);
        }
        let _t = crate::trace::span("overlap compare miss");
        // The ignore rules of the saved state: the tree is immutable, so the key stays valid.
        let found = compare(&self.base, &self.theirs, &Rules::of(&self.theirs))?;
        // A save that fails costs only time on the next call.
        let _ = self.write(&found);
        Ok(found)
    }

    /// `None` for a missing, unreadable or foreign entry: the compare runs again and rewrites it.
    fn read(&self) -> Option<Vec<Found>> {
        let saved: SavedCompare = serde_json::from_slice(&std::fs::read(&self.file).ok()?).ok()?;
        let same = saved.format == COMPARE_FORMAT && saved.base == self.base_id && saved.theirs == self.theirs_id;
        same.then_some(saved.found)
    }

    fn write(&self, found: &[Found]) -> Result<()> {
        let dir = self.file.parent().ok_or_else(|| anyhow!("compare folder"))?;
        std::fs::create_dir_all(dir)?;
        let saved = SavedCompare {
            format: COMPARE_FORMAT,
            base: self.base_id.clone(),
            theirs: self.theirs_id.clone(),
            found: found.to_vec(),
        };
        crate::fsutil::atomic_write(&self.file, &serde_json::to_vec(&saved)?, 0o600)
    }
}

/// Run the state compares on up to `COMPARE_THREADS` threads. The results keep the order of `jobs`.
fn run_compares(jobs: &[StateCompare]) -> Result<Vec<Vec<Found>>> {
    let next = AtomicUsize::new(0);
    let worker = || -> Result<Vec<(usize, Vec<Found>)>> {
        let mut done = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(job) = jobs.get(i) else { return Ok(done) };
            done.push((i, job.run()?));
        }
    };
    let mut out: Vec<Option<Vec<Found>>> = jobs.iter().map(|_| None).collect();
    std::thread::scope(|s| -> Result<()> {
        let handles: Vec<_> = (0..COMPARE_THREADS.min(jobs.len())).map(|_| s.spawn(worker)).collect();
        for h in handles {
            let done = h.join().map_err(|_| anyhow!("status overlap compare panicked"))??;
            for (i, found) in done {
                out[i] = Some(found);
            }
        }
        Ok(())
    })?;
    out.into_iter().map(|f| f.ok_or_else(|| anyhow!("status overlap compare missing"))).collect()
}

/// The state compares and live compares of one `overlaps` call. Each one runs once.
#[derive(Default)]
struct Work {
    /// The saved compares: base -> their head, our base -> head, and overlay layers.
    jobs: Vec<StateCompare>,
    /// Our saved compare per merge base: the index in `jobs`.
    ours_jobs: BTreeMap<String, usize>,
    /// Our head -> live folder. `Some(None)`: our head state cannot be read.
    head_live: Option<Option<Vec<Found>>>,
    /// The saved compare of an overlay: base -> our head of the overlay, by (overlay, base).
    layer_jobs: BTreeMap<(String, String), usize>,
    /// Our overlay head -> live folder, once per overlay. `Some(None)`: the head cannot be read.
    layer_live: BTreeMap<String, Option<Vec<Found>>>,
}

/// What one other line contributes, with its state compare in `Work::jobs`.
struct OtherPlan {
    name: String,
    owner: u32,
    /// The merge base with this line. `our_job` is our compare against it.
    base: String,
    our_job: Option<usize>,
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
    our_job: Option<usize>,
    state_time: i64,
    job: usize,
}

impl Repo<'_> {
    /// The saved state compares of this project (the states live in this project's store).
    fn compare_dir(&self) -> PathBuf {
        self.p.meta_dir().join(COMPARE_DIR)
    }

    /// Remove the saved compares whose states are gone, and the unreadable ones. Called by `gc`.
    pub fn prune_compares(&self) -> Result<usize> {
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

    fn radar_hidden(&self, ctx: &Ctx, line: &Line) -> Result<BTreeSet<String>> {
        let mut hidden = BTreeSet::new();
        for layer in self.layers(line)? {
            if !self.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                // Include earlier and newer private paths, so deletions cannot expose names.
                hidden.extend(self.private_paths(line)?);
            }
        }
        for (path, overlay) in self.pending_moves(line) {
            if !self.reads_overlay(&overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                hidden.insert(path);
            }
        }
        Ok(hidden)
    }

    /// The tree of our working folder to compare. APFS compares the live folder, as status does:
    /// a copy would clone every file, cache folders included.
    fn radar_tree(&self, line: &Line) -> Result<(Option<crate::repo::Temp>, PathBuf)> {
        #[cfg(target_os = "macos")]
        return Ok((None, self.working(line)));
        #[cfg(not(target_os = "macos"))]
        {
            let t = self.scan(line)?;
            let path = t.path.clone();
            Ok((Some(t), path))
        }
    }

    /// Our head -> live folder compare: one per call, shared by all merge bases.
    fn our_head_live(&self, line: &Line, live: &Path, rules: &Rules, work: &mut Work) -> Result<()> {
        if work.head_live.is_none() {
            work.head_live = Some(match self.state_src(&line.head) {
                Ok(head) => {
                    let _t = crate::trace::span("overlap live compare");
                    Some(compare(&head, live, rules)?)
                }
                Err(_) => None,
            });
        }
        Ok(())
    }

    /// The saved compare of a merge base -> our head, once per base. `None` when our head is
    /// unreadable: the changes then come from the direct compare.
    fn our_base_job(&self, line: &Line, base: &str, work: &mut Work) -> Result<Option<usize>> {
        if let Some(job) = work.ours_jobs.get(base) {
            return Ok(Some(*job));
        }
        if !matches!(work.head_live, Some(Some(_))) {
            return Ok(None);
        }
        let job = work.jobs.len();
        let state = StateCompare::new(&self.compare_dir(), base, &line.head, self.state_src(base)?, self.state_src(&line.head)?);
        work.jobs.push(state);
        work.ours_jobs.insert(base.to_string(), job);
        Ok(Some(job))
    }

    pub fn overlaps(&self, ctx: &Ctx, line: &Line) -> Result<Vec<Overlap>> {
        self.can_read(ctx)?;
        // Taking the working folder tree is costly; do it only when another line needs it.
        let mut ours: Option<(Option<crate::repo::Temp>, PathBuf)> = None;
        let ignored = Rules::new(&self.working(line));
        let our_hidden = self.radar_hidden(ctx, line)?;
        let our_private = self.private_paths(line)?;
        let mut work = Work::default();
        let mut plans: Vec<OtherPlan> = Vec::new();
        // Step 1, on this thread: the live parts and the state lookups (`Repo` is not `Sync`).
        for other in self.lines()? {
            if other.id == line.id || other.machine != self.p.store.machine.id || !self.working(&other).exists() {
                continue;
            }
            let base = match self.merge_base(&line.head, &other.head)? {
                Some(base) => base,
                None => continue,
            };
            let root = match self.state_src(&base) {
                Ok(root) => root,
                Err(_) => continue,
            };
            if ours.is_none() {
                ours = Some(self.radar_tree(line)?);
            }
            let ours_root = &ours.as_ref().expect("scanned above").1;
            self.our_head_live(line, ours_root, &ignored, &mut work)?;
            let our_job = self.our_base_job(line, &base, &mut work)?;
            // show <state>:<path> and try use project read access for both head and auto states.
            let head = self.state(&other.head)?;
            let state = self.last_auto(&other)?.filter(|s| s.time >= head.time).unwrap_or(head);
            let theirs = self.state_src(&state.id)?;
            let hidden = self.radar_hidden(ctx, &other)?;
            let their_private = self.private_paths(&other)?;
            let job = work.jobs.len();
            work.jobs.push(StateCompare::new(&self.compare_dir(), &base, &state.id, root, theirs));
            let layers = self.overlay_plans(ctx, line, &other, ours_root, &mut work)?;
            plans.push(OtherPlan {
                name: other.name.clone(),
                owner: other.owner,
                base,
                our_job,
                their_private,
                hidden,
                state_time: state.time,
                job,
                layers,
            });
        }
        // Step 2: the saved state compares, in parallel.
        let found = {
            let _t = crate::trace::span("status overlap compares");
            run_compares(&work.jobs)?
        };
        // Step 3, on this thread: our changes per base and the caller's filters, in the order of the lines.
        let mut overlaps = Vec::new();
        if let Some((_, ours_root)) = ours.as_ref() {
            let overlay_rules = Rules::new(ours_root);
            let mut ours_keys: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
            let mut layer_keys: BTreeMap<(String, String), BTreeMap<String, i64>> = BTreeMap::new();
            for p in &plans {
                if !ours_keys.contains_key(&p.base) {
                    let base_src = self.state_src(&p.base)?;
                    let parts = match (p.our_job, work.head_live.as_ref().and_then(|h| h.as_ref())) {
                        (Some(j), Some(live)) => Some((found[j].as_slice(), live.as_slice())),
                        _ => None,
                    };
                    let changed = changes_vs_base(&base_src, ours_root, &ignored, parts)?;
                    ours_keys.insert(p.base.clone(), visible(&changed, &our_private, &ignored));
                }
                let our_paths = &ours_keys[&p.base];
                for (path, time) in visible(&found[p.job], &p.their_private, &ignored) {
                    if our_paths.contains_key(&path) && !our_hidden.contains(&path) && !p.hidden.contains(&path) {
                        overlaps.push(Overlap {
                            path,
                            line: p.name.clone(),
                            owner: p.owner,
                            changed_at: if time == 0 { p.state_time } else { time },
                        });
                    }
                }
                for layer in &p.layers {
                    let key = (layer.overlay.clone(), layer.base.clone());
                    if !layer_keys.contains_key(&key) {
                        let repo = self.overlay_repo(&layer.overlay)?;
                        let base_src = repo.state_src(&layer.base)?;
                        let parts = match (layer.our_job, work.layer_live.get(&layer.overlay).and_then(|l| l.as_ref())) {
                            (Some(j), Some(live)) => Some((found[j].as_slice(), live.as_slice())),
                            _ => None,
                        };
                        let changed = changes_vs_base(&base_src, ours_root, &overlay_rules, parts)?;
                        layer_keys.insert(key.clone(), visible(&changed, &our_hidden, &overlay_rules));
                    }
                    let ours_layer = &layer_keys[&key];
                    for (path, time) in visible(&found[layer.job], &p.hidden, &overlay_rules) {
                        if ours_layer.contains_key(&path) {
                            overlaps.push(Overlap {
                                path,
                                line: p.name.clone(),
                                owner: p.owner,
                                changed_at: if time == 0 { layer.state_time } else { time },
                            });
                        }
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
    fn overlay_plans(&self, ctx: &Ctx, line: &Line, other: &Line, ours: &Path, work: &mut Work) -> Result<Vec<LayerPlan>> {
        let mut plans = Vec::new();
        let their_layers = self.layers(other)?;
        for layer in self.layers(line)? {
            if !self.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                continue;
            }
            let theirs = match their_layers.iter().find(|other| other.overlay == layer.overlay) {
                Some(l) => l,
                None => continue,
            };
            let repo = self.overlay_repo(&layer.overlay)?;
            // Use the same policy as show/try for this overlay's immutable states.
            if repo.can_read(ctx).is_err() {
                continue;
            }
            let base = match repo.merge_base(&layer.state, &theirs.state)? {
                Some(base) => base,
                None => continue,
            };
            let base_root = match repo.state_src(&base) {
                Ok(root) => root,
                Err(_) => continue,
            };
            let their_root = match repo.state_src(&theirs.state) {
                Ok(root) => root,
                Err(_) => continue,
            };
            if !work.layer_live.contains_key(&layer.overlay) {
                let live = match repo.state_src(&layer.state) {
                    Ok(head) => {
                        let _t = crate::trace::span("overlap live compare");
                        Some(compare(&head, ours, &Rules::new(ours))?)
                    }
                    Err(_) => None,
                };
                work.layer_live.insert(layer.overlay.clone(), live);
            }
            let key = (layer.overlay.clone(), base.clone());
            let our_job = match work.layer_jobs.get(&key) {
                Some(job) => Some(*job),
                None if matches!(work.layer_live.get(&layer.overlay), Some(Some(_))) => {
                    let job = work.jobs.len();
                    let state = StateCompare::new(
                        &repo.compare_dir(),
                        &base,
                        &layer.state,
                        repo.state_src(&base)?,
                        repo.state_src(&layer.state)?,
                    );
                    work.jobs.push(state);
                    work.layer_jobs.insert(key, job);
                    Some(job)
                }
                None => None,
            };
            let state = repo.state(&theirs.state)?;
            let job = work.jobs.len();
            work.jobs.push(StateCompare::new(&repo.compare_dir(), &base, &theirs.state, base_root, their_root));
            plans.push(LayerPlan { overlay: layer.overlay.clone(), base, our_job, state_time: state.time, job });
        }
        Ok(plans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str) -> Found {
        Found { path: path.into(), old_path: None, is_dir: false, time: 1, added: false, removed: false }
    }

    /// A path that both parts changed goes to the recheck; the other paths of both parts stay.
    #[test]
    fn a_path_changed_in_both_parts_is_checked_again() {
        let saved = vec![f("a"), f("b")];
        let live = vec![f("b"), f("c")];
        let merged = merge_changes(&saved, &live, |p| Ok(if p == "b" { None } else { Some(f(p)) }));
        let paths: Vec<String> = merged.unwrap().unwrap().into_iter().map(|x| x.path).collect();
        assert_eq!(paths, ["a", "c"]);
    }

    /// A rename in either part needs the direct compare.
    #[test]
    fn a_rename_needs_the_direct_compare() {
        let mut moved = f("new");
        moved.old_path = Some("old".into());
        assert!(merge_changes(&[moved], &[], |_| Ok(None)).unwrap().is_none());
    }
}
