//! On-demand path overlaps between live lines. Nothing is written to permanent history.
//!
//! Two trees are compared in two steps. The compare of two saved states (immutable) is cached
//! under the owning project's `meta/` folder. Inside that compare, the ignore rules of the saved
//! state skip ignored paths before any content compare. The caller's ignore rules and hidden
//! paths run after the cache on every call. The live folder is compared each time.

use crate::changes;
use crate::ctx::Ctx;
use crate::fsutil::Src;
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
const COMPARE_FORMAT: u32 = 2;
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
        out.push(Found { path: change.path, old_path: change.old_path, is_dir, time });
    }
    Ok(out)
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

/// A compare with a live folder of another line. The rules and hidden sets stay with the caller.
struct LayerPlan {
    /// Our visible changes against the base of this overlay layer.
    ours: BTreeMap<String, i64>,
    rules: Rules,
    state_time: i64,
    job: usize,
}

/// What one other line contributes, with its state compare in `jobs`.
struct OtherPlan {
    name: String,
    owner: u32,
    /// Our visible changes against the merge base with this line.
    our_paths: BTreeMap<String, i64>,
    their_private: BTreeSet<String>,
    hidden: BTreeSet<String>,
    state_time: i64,
    job: usize,
    layers: Vec<LayerPlan>,
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

    pub fn overlaps(&self, ctx: &Ctx, line: &Line) -> Result<Vec<Overlap>> {
        self.can_read(ctx)?;
        // Taking the working folder tree is costly; do it only when another line needs it.
        let mut ours: Option<(Option<crate::repo::Temp>, PathBuf)> = None;
        let ignored = Rules::new(&self.working(line));
        let our_hidden = self.radar_hidden(ctx, line)?;
        let our_private = self.private_paths(line)?;
        let mut ours_by_base: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        // Step 1, on this thread: the live parts and the state lookups (`Repo` is not `Sync`).
        let mut jobs: Vec<StateCompare> = Vec::new();
        let mut plans: Vec<OtherPlan> = Vec::new();
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
            if !ours_by_base.contains_key(&base) {
                let found = compare(&root, ours_root, &ignored)?;
                ours_by_base.insert(base.clone(), visible(&found, &our_private, &ignored));
            }
            let our_paths = ours_by_base[&base].clone();
            let hidden = self.radar_hidden(ctx, &other)?;
            // show <state>:<path> and try use project read access for both head and auto states.
            let head = self.state(&other.head)?;
            let state = self.last_auto(&other)?.filter(|s| s.time >= head.time).unwrap_or(head);
            let theirs = self.state_src(&state.id)?;
            let their_private = self.private_paths(&other)?;
            let job = jobs.len();
            jobs.push(StateCompare::new(&self.compare_dir(), &base, &state.id, root, theirs));
            let layers = self.overlay_plans(ctx, line, &other, ours_root, &our_hidden, &mut jobs)?;
            plans.push(OtherPlan {
                name: other.name.clone(),
                owner: other.owner,
                our_paths,
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
            run_compares(&jobs)?
        };
        // Step 3, on this thread: the caller's filters, in the order of the lines.
        let mut overlaps = Vec::new();
        for p in &plans {
            for (path, time) in visible(&found[p.job], &p.their_private, &ignored) {
                if p.our_paths.contains_key(&path) && !our_hidden.contains(&path) && !p.hidden.contains(&path) {
                    overlaps.push(Overlap {
                        path,
                        line: p.name.clone(),
                        owner: p.owner,
                        changed_at: if time == 0 { p.state_time } else { time },
                    });
                }
            }
            for layer in &p.layers {
                for (path, time) in visible(&found[layer.job], &p.hidden, &layer.rules) {
                    if layer.ours.contains_key(&path) {
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
        overlaps.sort_by(|a, b| (&a.path, &a.line).cmp(&(&b.path, &b.line)));
        overlaps.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        Ok(overlaps)
    }

    /// The overlay layers that both lines hold: our live changes now, the state compare as a job.
    fn overlay_plans(
        &self,
        ctx: &Ctx,
        line: &Line,
        other: &Line,
        ours: &Path,
        our_hidden: &BTreeSet<String>,
        jobs: &mut Vec<StateCompare>,
    ) -> Result<Vec<LayerPlan>> {
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
            let rules = Rules::new(ours);
            let ours_changes = visible(&compare(&base_root, ours, &rules)?, our_hidden, &rules);
            let state = repo.state(&theirs.state)?;
            let job = jobs.len();
            jobs.push(StateCompare::new(&repo.compare_dir(), &base, &theirs.state, base_root, their_root));
            plans.push(LayerPlan { ours: ours_changes, rules, state_time: state.time, job });
        }
        Ok(plans)
    }
}
