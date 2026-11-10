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

/// A remote URL without its transport and user: `git@host:path`, `ssh://git@host/path` and `https://host/path`
/// give `host/path`, with no `.git` and no slash at the end. A local path stays as it is.
fn repo_key(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let (rest, scheme) = match u.split_once("://") {
        Some((_, r)) => (r, true),
        None => (u, false),
    };
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    if scheme {
        return match rest.split_once('/') {
            Some((host, path)) => format!("{}/{path}", host.to_lowercase()),
            None => rest.to_lowercase(),
        };
    }
    // The scp form `host:path` of ssh; a local path has no `:` before its first `/`.
    match rest.split_once(':') {
        Some((host, path)) if !host.contains('/') && !host.is_empty() => format!("{}/{}", host.to_lowercase(), path),
        _ => rest.to_string(),
    }
}
/// The results of the passes, in the project meta folder. Local to this machine.
const RESULTS: &str = "follow.json";

#[derive(Default, Deserialize, Serialize)]
struct StoreCaches {
    remote: String,
    head: String,
    folders: BTreeMap<String, SeenCaches>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum SeenCaches {
    One(String),
    Many(BTreeSet<String>),
}

impl SeenCaches {
    fn contains(&self, id: &str) -> bool {
        match self {
            Self::One(value) => value == id,
            Self::Many(values) => values.contains(id),
        }
    }

    fn insert(&mut self, id: String) {
        match self {
            Self::One(value) => *self = Self::Many(BTreeSet::from([value.clone(), id])),
            Self::Many(values) => {
                values.insert(id);
            }
        }
    }
}

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

const NO_DAEMON: &str = "warning: no layr daemon runs for this store; follow passes run only on `layr follow --run`";
const OLDER_DAEMON: &str = "warning: the layr daemon runs an older build; restart it";
const OTHER_BUILD: &str = "warning: the layr daemon runs another layr build than this one; restart it";

/// The warning for a line of the store `root` when no daemon of this user serves that store, or
/// None. A busy daemon is not reported. A daemon that does not report its store root is an older
/// build, and its store is unknown. A daemon of another build than this binary (after an install
/// without a daemon restart) gets a restart warning.
pub fn daemon_warning(root: &Path) -> Option<&'static str> {
    match crate::service::hello(&crate::service::user_daemon_socket()) {
        crate::service::Hello::Busy => None,
        crate::service::Hello::Absent => Some(NO_DAEMON),
        crate::service::Hello::Running { root: None, .. } => Some(OLDER_DAEMON),
        crate::service::Hello::Running { root: Some(served), version } => {
            if !same_store(&served, root) {
                Some(NO_DAEMON)
            } else if version.is_some_and(|v| v != crate::store_version::binary()) {
                Some(OTHER_BUILD)
            } else {
                None
            }
        }
    }
}

fn same_store(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// A remote state that a pass could not merge into a line head. The daemon tick does not try
/// the same pair again: it tries when the remote state or the line head changes, or on an
/// explicit `layr follow`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    /// The fetched remote state.
    pub remote: String,
    /// The line head at the failed pass.
    pub line: String,
}

/// The result of the last pass of a line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Last {
    /// Milliseconds since the epoch.
    pub time: i64,
    pub ok: bool,
    /// What the pass did, or the error. Example: `fast-forward to 1a2b3c4d`.
    pub text: String,
    /// Set when the merge failed: the pair that the next tick does not retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refused>,
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

/// Unsaved work: a change or a conflict in the working folder. Untracked files are not unsaved work: the
/// merge keeps them and refuses a state that writes one of their paths. Ignored files (build output and
/// caches) are not unsaved work either.
fn has_unsaved(st: &Status) -> bool {
    !(st.staged.is_empty() && st.unstaged.is_empty() && st.conflicts.is_empty())
}

/// The build is useful only when the head stays unchanged for the whole build.
fn build_for_stable_head(
    head: impl Fn() -> Result<String>,
    mut build: impl FnMut() -> bool,
    mut moved: impl FnMut(&str),
) -> Result<Option<String>> {
    let mut before = head()?;
    for _ in 0..3 {
        let built = build();
        let after = head()?;
        if before == after {
            return Ok(built.then_some(after));
        }
        moved(&after);
        before = after;
    }
    Ok(None)
}

impl<'a> Repo<'a> {
    /// One follow pass of `line` (docs/follow.md). `ctx.cwd` must be the working folder of the line:
    /// the fetch and the cache save are the commands, which run in that folder. Takes the project
    /// lock only where it reads or writes records; the merge takes its own. The result is recorded
    /// and returned; an error of the pass is the result (`ok` false), not an `Err`.
    /// `explicit`: a user command. A tick (`explicit` false) does not retry a refused pair and
    /// then returns None, with nothing recorded and nothing to report.
    pub fn follow_pass(&self, ctx: &Ctx, line: &Line, explicit: bool) -> Result<Option<Last>> {
        if !ctx.cwd.starts_with(self.working(line)) {
            bail!("follow pass of '{}' runs in its working folder", line.name);
        }
        let refused = last_result(self, line)?.and_then(|l| l.refused);
        let mut pending = None;
        let outcome = match target(self, line)?.as_deref().and_then(split) {
            None => Err(anyhow::anyhow!("line '{}' follows no branch", line.name)),
            Some((remote, branch)) => self.follow_once(ctx, line, remote, branch, explicit, refused.as_ref(), &mut pending),
        };
        let last = match outcome {
            Ok(Some(text)) => Last { time: ids::now_ms(), ok: true, text, refused: None },
            Ok(None) => return Ok(None),
            Err(e) => Last { time: ids::now_ms(), ok: false, text: format!("{e:#}"), refused: pending },
        };
        let _lock = self.p.lock()?;
        let mut all = last_results(self)?;
        all.insert(line.id.clone(), last.clone());
        crate::fsutil::atomic_write(&results_path(self), &serde_json::to_vec(&all)?, 0o600)?;
        Ok(Some(last))
    }

    /// The text of a pass that ran, or None for a tick that skips a refused pair. `pending` receives
    /// the pair before the merge, so a failed merge can record it.
    #[allow(clippy::too_many_arguments)]
    fn follow_once(
        &self,
        ctx: &Ctx,
        line: &Line,
        remote: &str,
        branch: &str,
        explicit: bool,
        refused: Option<&Refused>,
        pending: &mut Option<Refused>,
    ) -> Result<Option<String>> {
        let rb = format!("{remote}/{branch}");
        let is_store = self.p.config_value(&format!("cloud.remote.{remote}.url"))?.is_some();
        let fetched = if is_store {
            if branch != "main" || remote::kind(&remote::remote_url(self, remote)?)? != remote::Kind::Store {
                bail!("Layr follow requires a store remote and main");
            }
            let code = remote::fetch(ctx, &[remote.to_string(), "--heads".into(), "-q".into()])?;
            if code != 0 {
                bail!("fetch of {remote} was incomplete");
            }
            crate::store_remote::open_named(remote, self)?.verified_main_head(ctx)?
        } else {
            remote::git::fetch(ctx, &[remote.to_string(), branch.to_string(), "-q".to_string()])?;
            revs::resolve(self, Some(line), &rb)?
        };
        let head = self.line(&line.id)?.head;
        if fetched == head || self.is_ancestor(&fetched, &head)? {
            // A pass that merged but saved no cache (it stopped, or an older layr ran it) leaves the head without
            // a cache, and new lines start cold. This pass runs the cache step again; once the caches are saved
            // with the head, the next passes do nothing.
            let now = self.line(&line.id)?;
            let behind = self.caches_behind_head(&now)?;
            if behind.is_empty() && !(is_store && fetched == head) {
                return Ok(Some("up to date".into()));
            }
            {
                let _lock = self.p.lock()?;
                if has_unsaved(&self.locked_status_report(&now)?) {
                    return Ok(Some("up to date; caches not saved: unsaved work".into()));
                }
            }
            let cache_result = if is_store && fetched == head {
                self.take_store_follow_caches(ctx, line, remote, &fetched)
            } else {
                Ok(Vec::new())
            };
            if behind.is_empty() && cache_result.as_ref().is_ok_and(Vec::is_empty) {
                return Ok(Some("up to date".into()));
            }
            let remote_taken = cache_result.as_ref().is_ok_and(|folders| !folders.is_empty());
            let cache_error = cache_result.err();
            if behind.is_empty() && cache_error.is_some() {
                return Ok(Some(format!("up to date; remote cache not taken: {:#}", cache_error.unwrap())));
            }
            return Ok(Some(match self.take_follow_caches(ctx, line, Vec::new(), Some(&head), true) {
                Ok(_) if cache_error.is_some() => format!("up to date; remote cache not taken: {:#}", cache_error.unwrap()),
                Ok(_) if remote_taken && behind.is_empty() => "up to date; remote caches taken and saved".into(),
                Ok(_) => format!("up to date; caches saved with the head ({})", behind.join(", ")),
                Err(e) => format!("up to date; caches not saved: {e:#}"),
            }));
        }
        if !explicit && refused.is_some_and(|r| r.remote == fetched && r.line == head) {
            return Ok(None);
        }
        {
            let _lock = self.p.lock()?;
            let st = self.locked_status_report(&self.line(&line.id)?)?;
            if has_unsaved(&st) {
                return Ok(Some("skipped: unsaved work".into()));
            }
        }
        *pending = Some(Refused { remote: fetched.clone(), line: head.clone() });
        let mut args = crate::args::Parsed::default();
        args.values.insert("--expect".into(), vec![fetched.clone()]);
        args.flags.insert("-q".into(), 1);
        args.flags.insert(mergecmd::REFUSE_CONFLICTS.into(), 1);
        let code = mergecmd::merge_rev(ctx, self, line, if is_store { &fetched } else { &rb }, &args)?;
        if code != 0 {
            bail!("merge of {rb} left conflicts");
        }
        let after = self.line(&line.id)?.head;
        let short = ids::short(&fetched);
        let mut text = if after == fetched { format!("fast-forward to {short}") } else { format!("merged {short}") };
        let cache_error =
            if is_store && after == fetched { self.take_store_follow_caches(ctx, line, remote, &fetched).err() } else { None };
        if let Some(e) = cache_error {
            text.push_str(&format!("; remote cache not taken: {e:#}"));
        }
        match self.follow_caches(ctx, line, remote, &head) {
            Ok((taken, Some(reason))) if taken.is_empty() => text.push_str(&format!("; no cache taken: {reason}")),
            Ok(_) => {}
            Err(e) => text.push_str(&format!("; caches not taken: {e:#}")),
        }
        Ok(Some(text))
    }

    fn take_store_follow_caches(&self, ctx: &Ctx, line: &Line, remote: &str, head: &str) -> Result<Vec<String>> {
        let mut repo = Repo::open(ctx, &self.p.name)?;
        let mut store = crate::store_remote::open_named(remote, &repo)?;
        store.configure(ctx, &crate::store_remote::ConfigureOptions::default())?;
        let opts = crate::store_remote::Options {
            auto: true,
            caches: true,
            heads: true,
            caches_of_state: Some(head.to_string()),
            ..Default::default()
        };
        let report = store.fetch(ctx, &mut repo, &opts)?;
        if !report.missing_lines.is_empty() {
            bail!("fetch of {remote} was incomplete");
        }
        let _line_lock = self.line_lock(line)?;
        let now = {
            let _lock = self.p.lock()?;
            let now = self.line(&line.id)?;
            if now.head != head || has_unsaved(&self.locked_status_report(&now)?) {
                bail!("line changed during cache fetch");
            }
            now
        };
        let path = self.p.meta_dir().join("line-locks").join(format!("{}.follow-caches", line.id));
        let mut recorded: StoreCaches = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("reading follow cache state")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreCaches::default(),
            Err(e) => return Err(e.into()),
        };
        if recorded.remote != remote || recorded.head != head {
            recorded = StoreCaches { remote: remote.to_string(), head: head.to_string(), folders: BTreeMap::new() };
        }
        let overlays = self.folder_overlays()?;
        let all: BTreeSet<String> = overlays.iter().map(|(_, folder)| folder.clone()).collect();
        let mut folders = Vec::new();
        let source = Source::ExactState(head, &line.id, None);
        for (name, folder) in overlays {
            let key = self.cache_key(ctx, &name, &self.working(&now), self.owner_ids(&now).0)?;
            let Some(id) = self.cache_source(&name, &source, self.owner_ids(&now).0, &key)? else { continue };
            if recorded.folders.get(&folder).is_some_and(|seen| seen.contains(&id)) {
                continue;
            }
            let skip = all.iter().filter(|f| *f != &folder).cloned().collect();
            let exact = Source::ExactState(head, &line.id, Some(&id));
            let taken = self.pull_caches(ctx, &now, &exact, &skip)?;
            if !taken.is_empty() {
                recorded.folders.entry(folder.clone()).and_modify(|seen| seen.insert(id.clone())).or_insert(SeenCaches::One(id));
                crate::fsutil::atomic_write(&path, &serde_json::to_vec(&recorded)?, 0o600)?;
                folders.push(folder);
            }
        }
        crate::source_times::apply(&self.working(line), &folders)?;
        Ok(folders)
    }

    /// The caches after a merge (docs/cache-overlays.md): the candidates of `remote`, newest first.
    fn follow_caches(
        &self,
        ctx: &Ctx,
        line: &Line,
        remote: &str,
        before: &str,
    ) -> Result<(Vec<(String, String)>, Option<String>)> {
        self.take_follow_caches(ctx, line, self.cache_sources(remote)?, Some(before), true)
    }

    /// Takes the caches of `sources` (newest first) into `line`, then saves them as `save --caches` does (prune,
    /// then save). The follow pass and a local merge both use it. `warm`: the adapters build the standard commands
    /// into each cache folder before the prune (the follow pass; a change from the remote may come without a
    /// cache). `before` is the head before the merge: the rule
    /// of a protected line is checked against it. Returns the folder and the state of each cache taken, and the reason
    /// a candidate was refused (the last one).
    pub(crate) fn take_follow_caches(
        &self,
        ctx: &Ctx,
        line: &Line,
        sources: Vec<Line>,
        before: Option<&str>,
        warm: bool,
    ) -> Result<(Vec<(String, String)>, Option<String>)> {
        // Cache saves wait for this build, while ordinary head saves can trigger a retry.
        let _cache_lock = if warm { Some(self.cache_build_lock(line)?) } else { None };
        let preparation_lock = self.line_lock(line)?;
        let line = &self.line(&line.id)?;
        // A crash at any later step must leave a followed line due for another build.
        if target(self, line)?.is_some() {
            self.set_unbuilt(line, true)?;
        }
        let folders: BTreeSet<String> = self.folder_overlays()?.into_iter().map(|(_, f)| f).collect();
        let mut done = BTreeSet::new();
        let mut taken = Vec::new();
        let mut refusal = None;
        for cand in sources {
            if done.len() == folders.len() {
                break;
            }
            if cand.id == line.id {
                continue;
            }
            // An error of one candidate (its state is not on this machine, say) refuses only that candidate: the
            // others, the build and the save still run.
            let name = cand.name.clone();
            match self.take_candidate_caches(ctx, line, cand, before, &done) {
                Ok(Ok(pulled)) => {
                    for (folder, state) in pulled {
                        done.insert(folder.clone());
                        taken.push((folder, state));
                    }
                }
                Ok(Err(reason)) => refusal = Some(reason),
                Err(e) => refusal = Some(format!("{name}: {e:#}")),
            }
        }
        // The merge wrote the changed files with a new time; the ones the taken caches were built from get their time
        // back (docs/cache-overlays.md, section 20), before the prune reads them.
        let folders_taken: Vec<String> = done.iter().cloned().collect();
        if let Err(e) = crate::source_times::apply(&self.working(line), &folders_taken) {
            eprintln!("warning: source file times of the caches not set: {e:#}");
        }
        // A change without a cache (a merge on the remote of a branch with no line here) leaves units to build. The
        // build runs once here, so the lines from this line start warm (docs/follow.md).
        // Cache transfer is complete. Allow a head save during the build and check it again.
        if warm {
            drop(preparation_lock);
        }
        let mut paused = false;
        let built_head = if warm {
            build_for_stable_head(
                || Ok(self.line(&line.id)?.head),
                || {
                    let built = folders.iter().fold(true, |all, folder| self.warm_cache_folder(line, folder) & all);
                    if !paused {
                        if let Some(ms) =
                            std::env::var("LAYR_TEST_FOLLOW_AFTER_BUILD_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok())
                        {
                            eprintln!("layr: test pause after follow build");
                            std::thread::sleep(std::time::Duration::from_millis(ms));
                        }
                        paused = true;
                    }
                    built
                },
                |head| eprintln!("layr: line {} moved to {head} during the cache build; building again", line.name),
            )?
        } else {
            None
        };
        // Prunes each taken folder, then saves the caches of the line.
        // The follow pass runs in the background, so its prune may take long on a loaded machine.
        let save = || admin::save(ctx, &["--caches".to_string(), "-q".to_string()]);
        if warm {
            if let Some(ms) = std::env::var("LAYR_TEST_FOLLOW_BEFORE_PUBLISH_PAUSE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                eprintln!("layr: test pause before follow cache publish");
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            // Hold the line lock through the final comparison, cache save and mark update. A
            // save on the line cannot move the head between this check and publishing the cache.
            let _publication_lock = self.line_lock(line)?;
            for variable in ["LAYR_TEST_FOLLOW_CACHE_BARRIER", "LAYR_TEST_FOLLOW_MARK_BARRIER"] {
                if let Some(root) = std::env::var_os(variable) {
                    let root = std::path::PathBuf::from(root);
                    std::fs::write(root.join("ready"), b"ready")?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    while !root.join("release").exists() {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "follow cache test barrier timed out");
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
            let now = self.line(&line.id)?;
            if built_head.as_deref() != Some(now.head.as_str()) {
                self.set_unbuilt(&now, true)?;
                bail!("cache build did not finish for the current head; the next follow pass will retry");
            }
            self.set_unbuilt(&now, true)?;
            Repo::with_prune_limit(crate::cache_hook::BACKGROUND_PRUNE_LIMIT, || {
                admin::save_expected_head(ctx, &["--caches".to_string(), "-q".to_string()], Some(&now.head))
            })?;
            self.set_unbuilt(&now, false)?;
            return Ok((taken, refusal));
        }
        save()?;
        // A local merge saves without a build. A followed line must build again on its next pass.
        if target(self, line)?.is_some() {
            self.set_unbuilt(line, true)?;
        }
        Ok((taken, refusal))
    }

    /// The caches of one candidate: the folders pulled, or the reason it is refused. The rule of a protected line and
    /// the squash-safe content check (#133) must pass.
    fn take_candidate_caches(
        &self,
        ctx: &Ctx,
        line: &Line,
        cand: Line,
        before: Option<&str>,
        done: &BTreeSet<String>,
    ) -> Result<std::result::Result<Vec<(String, String)>, String>> {
        let theirs = self.localize(cand)?;
        let refused = {
            let _lock = self.p.lock()?;
            let now = self.line(&line.id)?;
            let mut reason = None;
            if let Some(rule) = self.policy().rule_for(&now.name) {
                if !mergecmd::merge_met_rule(self, &rule, before.unwrap_or(&now.head), &now.head, &theirs.head)? {
                    reason = Some("checks and approvals do not allow them".to_string());
                }
            }
            match reason {
                Some(r) => Some(r),
                None => mergecmd::caches_contained(self, &now, &theirs)?,
            }
        };
        if let Some(r) = refused {
            return Ok(Err(r));
        }
        let now = self.line(&line.id)?;
        let _t = crate::trace::span("merge caches");
        Ok(Ok(self.pull_caches(ctx, &now, &Source::Line(&theirs), done)?))
    }

    /// The remotes of the repository of `remote`: `remote` itself and each remote with the same URL key
    /// (`repo_key`). A push over https and a follow over ssh name one repository.
    fn same_repo_remotes(&self, remote: &str) -> Result<BTreeSet<String>> {
        let mut urls = Vec::new();
        for (k, val) in self.p.db.config_all()? {
            if let Some(n) = k.strip_prefix("remote.").and_then(|x| x.strip_suffix(".url")) {
                let url: String = serde_json::from_str(&val).unwrap_or(val.clone());
                urls.push((n.to_string(), repo_key(&url)));
            }
        }
        let mut out = BTreeSet::from([remote.to_string()]);
        if let Some((_, key)) = urls.iter().find(|(n, _)| n == remote) {
            out.extend(urls.iter().filter(|(_, k)| k == key).map(|(n, _)| n.clone()));
        }
        Ok(out)
    }

    /// Lines with an export to `remote` (or to a remote of the same repository) in the last 14 days, newest
    /// export first. An export names a state, and the state names its line.
    fn cache_sources(&self, remote: &str) -> Result<Vec<Line>> {
        let since = ids::now_ms() - CACHE_SOURCE_MS;
        let remotes = self.same_repo_remotes(remote)?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for rec in self.p.db.records_since(since)?.into_iter().rev() {
            if rec.op != "export" || !rec.data["remote"].as_str().is_some_and(|r| remotes.contains(r)) {
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
                repo.follow_pass(ctx, &line, false)
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
        author: Default::default(),
        out: std::cell::RefCell::new(Box::new(std::io::sink())),
    };
    let repo = Repo::open(&ctx, name)?;
    f(&repo, &ctx)
}

#[cfg(test)]
mod tests {
    use super::{build_for_stable_head, repo_key, StoreCaches};
    use crate::repo::Repo;
    use std::time::Duration;

    #[test]
    fn a_follow_build_retries_a_moved_head_and_stops_after_three_moves() {
        use std::cell::{Cell, RefCell};
        let heads = RefCell::new(["a", "b", "b"].into_iter());
        let builds = Cell::new(0);
        let mut moved = Vec::new();
        let result = build_for_stable_head(
            || Ok(heads.borrow_mut().next().unwrap().to_string()),
            || {
                builds.set(builds.get() + 1);
                true
            },
            |head| moved.push(head.to_string()),
        )
        .unwrap();
        assert_eq!(result.as_deref(), Some("b"));
        assert_eq!(builds.get(), 2);
        assert_eq!(moved, ["b"]);
        let n = Cell::new(0);
        let result = build_for_stable_head(
            || {
                n.set(n.get() + 1);
                Ok(n.get().to_string())
            },
            || true,
            |_| {},
        )
        .unwrap();
        assert_eq!(result, None);
        assert_eq!(n.get(), 4);
        assert_eq!(build_for_stable_head(|| Ok("same".into()), || false, |_| {}).unwrap(), None);
    }

    #[test]
    fn a_retried_build_replaces_the_cache_for_the_moved_head() {
        use std::cell::{Cell, RefCell};
        let head = RefCell::new("first".to_string());
        let cache = RefCell::new(String::new());
        let calls = Cell::new(0);
        let built = build_for_stable_head(
            || Ok(head.borrow().clone()),
            || {
                *cache.borrow_mut() = head.borrow().clone();
                if calls.get() == 0 {
                    *head.borrow_mut() = "second".to_string();
                }
                calls.set(calls.get() + 1);
                true
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(built.as_deref(), Some("second"));
        assert_eq!(&*cache.borrow(), "second");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn follow_cache_history_keeps_the_old_single_cache_format() {
        let mut state: StoreCaches =
            serde_json::from_str(r#"{"remote":"origin","head":"head","folders":{"target":"old"}}"#).unwrap();
        let seen = state.folders.get_mut("target").unwrap();
        seen.insert("new".into());
        let saved = serde_json::to_vec(&state).unwrap();
        let restored: StoreCaches = serde_json::from_slice(&saved).unwrap();
        assert!(restored.folders["target"].contains("old"));
        assert!(restored.folders["target"].contains("new"));
        assert!(!restored.folders["target"].contains("other"));
    }

    #[test]
    fn the_background_prune_limit_holds_inside_and_is_restored_after() {
        let inside =
            Repo::with_prune_limit(Duration::from_secs(600), || Repo::with_prune_limit(Duration::from_secs(5), || 1) + 1);
        assert_eq!(inside, 2);
        assert_eq!(Repo::prune_limit(), crate::cache_hook::HOOK_LIMIT);
        assert_eq!(Repo::with_prune_limit(Duration::from_secs(600), Repo::prune_limit), Duration::from_secs(600));
    }

    #[test]
    fn ssh_and_https_remotes_of_one_repository_match() {
        let k = "github.com/ifdotpy/layr-studio";
        assert_eq!(repo_key("git@github.com:ifdotpy/layr-studio.git"), k);
        assert_eq!(repo_key("https://github.com/ifdotpy/layr-studio.git"), k);
        assert_eq!(repo_key("https://user@GitHub.com/ifdotpy/layr-studio/"), k);
        assert_eq!(repo_key("ssh://git@github.com/ifdotpy/layr-studio"), k);
        assert_ne!(repo_key("https://github.com/ifdotpy/layr"), k);
        assert_eq!(repo_key("/tmp/remote.git"), "/tmp/remote");
    }
}
