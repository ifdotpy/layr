//! `layr follow`: a line keeps a remote branch (docs/follow.md). This module owns the config key,
//! one pass, its result record and the daemon tick. Merges, fetches and cache saves are the
//! commands' own functions.

use crate::cache_overlay::Source;
use crate::cmd::{admin, mergecmd, remote};
use crate::ctx::{Caller, Ctx};
use crate::ids;
use crate::model::Line;
use crate::repo::{Repo, Status};
use crate::revs;
use crate::store::Store;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Config key prefix: `follow.<line id>` = `"<remote>/<branch>"`.
const KEY_PREFIX: &str = "follow.";
/// Only lines with an export to the remote this recent are cache sources.
const CACHE_SOURCE_MS: i64 = 14 * 24 * 3600 * 1000;
/// The results of the passes, in the project meta folder. Local to this machine.
const RESULTS: &str = "follow.json";

/// The config key of a line's follow target. The only code that builds the key.
pub fn config_key(line_id: &str) -> String {
    format!("{KEY_PREFIX}{line_id}")
}

/// The line id of a follow config key, if it is one.
pub fn line_of_key(key: &str) -> Option<&str> {
    key.strip_prefix(KEY_PREFIX)
}

/// Split `<remote>/<branch>` at the first slash. Branch names may hold slashes; remote names may not.
pub fn split(target: &str) -> Option<(&str, &str)> {
    let (remote, branch) = target.split_once('/')?;
    (!remote.is_empty() && !branch.is_empty()).then_some((remote, branch))
}

/// The result of the last pass of a line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Last {
    /// Milliseconds since the epoch.
    pub time: i64,
    pub ok: bool,
    /// What the pass did, or the error. Example: `fast-forward to 1a2b3c4d`.
    pub text: String,
}

/// The follow target of a line, `<remote>/<branch>`.
pub fn target(repo: &Repo, line: &Line) -> Result<Option<String>> {
    Ok(repo.p.config_value(&config_key(&line.id))?.and_then(|v| v.as_str().map(str::to_owned)))
}

/// The lines of the project with a follow target, and their target.
pub fn followed(repo: &Repo) -> Result<Vec<(Line, String)>> {
    let mut out = Vec::new();
    for (key, _) in repo.p.db.config_all()? {
        let Some(id) = line_of_key(&key) else { continue };
        // A line deleted after its target was set keeps no pass.
        let Ok(line) = repo.line(id) else { continue };
        if let Some(t) = target(repo, &line)? {
            out.push((line, t));
        }
    }
    Ok(out)
}

fn results_path(repo: &Repo) -> PathBuf {
    repo.p.meta_dir().join(RESULTS)
}

/// The last pass of each line of this project (line id to result).
pub fn last_results(repo: &Repo) -> Result<BTreeMap<String, Last>> {
    match std::fs::read(results_path(repo)) {
        Ok(data) => serde_json::from_slice(&data).context("reading the follow results"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

/// The last pass of a line, if any.
pub fn last_result(repo: &Repo, line: &Line) -> Result<Option<Last>> {
    Ok(last_results(repo)?.remove(&line.id))
}

/// Unsaved work: a change, an untracked file or a conflict in the working folder. Ignored files
/// (build output and caches) are not unsaved work.
fn has_unsaved(st: &Status) -> bool {
    !(st.staged.is_empty()
        && st.unstaged.is_empty()
        && st.untracked.is_empty()
        && st.untracked_dirs.is_empty()
        && st.conflicts.is_empty())
}

impl<'a> Repo<'a> {
    /// One follow pass of `line` (docs/follow.md). `ctx.cwd` must be the working folder of the line:
    /// the fetch and the cache save are the commands, which run in that folder. Takes the project
    /// lock only where it reads or writes records; the merge takes its own. The result is recorded
    /// and returned; an error of the pass is the result (`ok` false), not an `Err`.
    pub fn follow_pass(&self, ctx: &Ctx, line: &Line) -> Result<Last> {
        if !ctx.cwd.starts_with(self.working(line)) {
            bail!("follow pass of '{}' runs in its working folder", line.name);
        }
        let outcome = match target(self, line)?.as_deref().and_then(split) {
            None => Err(anyhow::anyhow!("line '{}' follows no branch", line.name)),
            Some((remote, branch)) => self.follow_once(ctx, line, remote, branch),
        };
        let last = match outcome {
            Ok(text) => Last { time: ids::now_ms(), ok: true, text },
            Err(e) => Last { time: ids::now_ms(), ok: false, text: format!("{e:#}") },
        };
        let _lock = self.p.lock()?;
        let mut all = last_results(self)?;
        all.insert(line.id.clone(), last.clone());
        crate::fsutil::atomic_write(&results_path(self), &serde_json::to_vec(&all)?, 0o600)?;
        Ok(last)
    }

    fn follow_once(&self, ctx: &Ctx, line: &Line, remote: &str, branch: &str) -> Result<String> {
        let rb = format!("{remote}/{branch}");
        remote::git::fetch(ctx, &[remote.to_string(), branch.to_string(), "-q".to_string()])?;
        let fetched = revs::resolve(self, Some(line), &rb)?;
        let head = self.line(&line.id)?.head;
        if fetched == head || self.is_ancestor(&fetched, &head)? {
            return Ok("up to date".into());
        }
        {
            let _lock = self.p.lock()?;
            let st = self.locked_status(&self.line(&line.id)?)?.0;
            if has_unsaved(&st) {
                return Ok("skipped: unsaved work".into());
            }
        }
        let mut args = crate::args::Parsed::default();
        args.values.insert("--expect".into(), vec![fetched.clone()]);
        args.flags.insert("-q".into(), 1);
        args.flags.insert(mergecmd::REFUSE_CONFLICTS.into(), 1);
        let code = mergecmd::merge_rev(ctx, self, line, &rb, &args)?;
        if code != 0 {
            bail!("merge of {rb} left conflicts");
        }
        let after = self.line(&line.id)?.head;
        let short = ids::short(&fetched);
        let mut text = if after == fetched { format!("fast-forward to {short}") } else { format!("merged {short}") };
        if let Err(e) = self.follow_caches(ctx, line, remote) {
            text.push_str(&format!("; caches not taken: {e:#}"));
        }
        Ok(text)
    }

    /// The caches after a merge (docs/cache-overlays.md). Per folder overlay, the newest candidate
    /// whose work the line now holds takes it. Then `prune_cache` per folder and `save --caches`.
    fn follow_caches(&self, ctx: &Ctx, line: &Line, remote: &str) -> Result<()> {
        let folders: BTreeSet<String> = self.folder_overlays()?.into_iter().map(|(_, f)| f).collect();
        let mut done = BTreeSet::new();
        for cand in self.cache_sources(remote)? {
            if done.len() == folders.len() {
                break;
            }
            if cand.id == line.id {
                continue;
            }
            let theirs = self.localize(cand)?;
            // The rule of a protected line and the squash-safe content check (#133) must pass.
            let refused = {
                let _lock = self.p.lock()?;
                let now = self.line(&line.id)?;
                let mut reason = None;
                if let Some(rule) = self.policy().rule_for(&now.name) {
                    if !mergecmd::merge_met_rule(self, &rule, &now.head, &now.head, &theirs.head)? {
                        reason = Some("checks and approvals do not allow them".to_string());
                    }
                }
                match reason {
                    Some(r) => Some(r),
                    None => mergecmd::caches_contained(self, &now, &theirs)?,
                }
            };
            if refused.is_some() {
                continue;
            }
            let now = self.line(&line.id)?;
            for (_, folder) in self.pull_caches(ctx, &now, &Source::Line(&theirs), &done)? {
                done.insert(folder);
            }
        }
        for folder in &folders {
            self.prune_cache(folder)?;
        }
        // The caches of the line as cache overlays, as `layr save --caches` makes them.
        admin::save(ctx, &["--caches".to_string(), "-q".to_string()])?;
        Ok(())
    }

    /// Lines with an export to `remote` in the last 14 days, newest export first. An export names
    /// a state, and the state names its line.
    fn cache_sources(&self, remote: &str) -> Result<Vec<Line>> {
        let since = ids::now_ms() - CACHE_SOURCE_MS;
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for rec in self.p.db.records_since(since)?.into_iter().rev() {
            if rec.op != "export" || rec.data["remote"].as_str() != Some(remote) {
                continue;
            }
            let Some(state) = rec.data["state"].as_str() else { continue };
            let Some(id) = self.p.db.state(state)?.and_then(|s| s.line) else { continue };
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Ok(line) = self.line(&id) {
                out.push(line);
            }
        }
        Ok(out)
    }
}

/// The daemon tick: one follow pass for each followed line of each project. Each pass takes the
/// project lock itself, one pass runs at a time, and an error in one line does not stop the others.
/// The caller is the daemon's user (macOS) or root (Linux).
pub fn tick(root: &Path, caller: Caller) {
    let names = match Store::open(root).and_then(|s| s.project_names()) {
        Ok(names) => names,
        Err(e) => {
            eprintln!("layr follow: cannot read the store {}: {e:#}", root.display());
            return;
        }
    };
    for name in names {
        let lines = match with_repo(root, &caller, PathBuf::from("/"), &name, |repo, _| {
            Ok(followed(repo)?.into_iter().map(|(l, _)| l).collect::<Vec<_>>())
        }) {
            Ok(lines) => lines,
            Err(_) => continue,
        };
        for line in lines {
            let working = match with_repo(root, &caller, PathBuf::from("/"), &name, |repo, _| Ok(repo.working(&line))) {
                Ok(w) => w,
                Err(_) => continue,
            };
            let pass = with_repo(root, &caller, working, &name, |repo, ctx| {
                let line = repo.line(&line.id)?;
                // A line of another machine has no working folder here: no pass.
                if line.machine != repo.p.store.machine.id {
                    return Ok(None);
                }
                repo.follow_pass(ctx, &line).map(Some)
            });
            match pass {
                Ok(Some(last)) if !last.ok => eprintln!("layr follow ({name}/{}): {}", line.name, last.text),
                Ok(_) => {}
                Err(e) => eprintln!("layr follow ({name}/{}): {e:#}", line.name),
            }
        }
    }
}

/// Open the store and the project with a context of `cwd`, and run `f`. The context has no output.
fn with_repo<T>(root: &Path, caller: &Caller, cwd: PathBuf, name: &str, f: impl FnOnce(&Repo, &Ctx) -> Result<T>) -> Result<T> {
    let ctx = Ctx {
        store: Store::open(root)?,
        caller: caller.clone(),
        cwd,
        env: HashMap::new(),
        out: std::cell::RefCell::new(Box::new(std::io::sink())),
    };
    let repo = Repo::open(&ctx, name)?;
    f(&repo, &ctx)
}
