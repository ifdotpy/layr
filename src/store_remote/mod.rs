//! Encrypted immutable remotes, independent of the local snapshot backend.

pub(crate) mod api;
#[cfg(test)]
mod api_tests;
pub(crate) mod backend;
pub(crate) mod browse;
pub(crate) mod chunked;
mod cloud_objects;
mod download;
pub(crate) mod epochs;
pub(crate) mod gateway;
mod gitpack;
mod index;
mod listing_cache;
pub(crate) mod overlays;
mod public;
mod push_cache;
mod push_journal;
pub(crate) mod reader;
mod release;
pub(crate) mod sweep;
#[cfg(test)]
mod sync_tests;
mod writer;
// The large-block allocator counter, for the tests of other modules.
#[cfg(test)]
pub(crate) use writer::tests::{large_blocks, large_blocks_of};

pub(crate) use layr_store::{canonical, format, keys, s3, zst};

use crate::ctx::Ctx;
use crate::repo::Repo;
use crate::sync_local::fetch;
use anyhow::{bail, Context, Result};
use backend::Backend;
use format::{Descriptor, Mode};
use layr_hashers::{ContentAddress, Digest};
use layr_store::sync::RemoteSync;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
// Only metadata and the portable port cross into a main-layer transfer engine.
pub(crate) type Replica<'a> =
    dyn layr_store::sync::LocalReplica<State = crate::model::StateRec, Line = crate::model::Line, Selection = Options> + 'a;

#[derive(Clone, Default)]
pub struct Options {
    pub public: bool,
    pub auto: bool,
    pub caches: bool,
    /// Leave ordinary cache cleanup to a later push or daemon sweep.
    pub no_cache_sweep: bool,
    pub paper_key: Option<String>,
    /// `--caches-of <line>`: restore only the cache states that `line new X <line>` takes for these lines.
    pub caches_of: Vec<String>,
    /// Fetch caches saved with this state, including its automatic save states.
    pub(crate) caches_of_state: Option<String>,
    /// `--heads`: restore only the heads of the lines; the other states stay on the remote and come on first read.
    pub heads: bool,
    /// Clone only this line's working folder and head; other states stay on the remote.
    pub only_line: Option<String>,
    /// The cache states of `caches_of` in one cache overlay, set by the overlay fetch (None: every usable one).
    pub only_caches: Option<std::collections::BTreeSet<String>>,
    /// `fetch --line <name>`: only the history of these lines (docs/overlays.md, a view is f(line, overlays)).
    /// Empty: every line.
    pub lines: Vec<String>,
    /// `fetch --overlay <name>`: only these layers; `main` names the bottom layer. None: every layer.
    pub overlays: Option<std::collections::BTreeSet<String>>,
    /// The states of one overlay that `lines` need, set by the overlay fetch (None: every state).
    pub(crate) only_states: Option<std::collections::BTreeSet<String>>,
    /// A plain fetch of one cache overlay, set by the overlay fetch: no cache state comes, and the fetch names the
    /// count of the states that stay on the remote (`takes_cache_states`).
    pub(crate) plain_cache_fetch: bool,
}

impl Options {
    /// Whether `fetch --overlay` takes the layer `name`.
    pub(crate) fn takes_layer(&self, name: &str) -> bool {
        self.overlays.as_ref().map_or(true, |only| only.contains(name))
    }

    /// Whether a fetch or push leaves out overlay `name` because its `platform` is another
    /// platform's (docs/overlays.md, section 1a, slice 3). `--overlay <name>` takes it all the same.
    pub(crate) fn other_platform(&self, name: &str, platform: Option<&str>) -> bool {
        crate::overlay::other_platform_overlay(platform)
            && !self.overlays.as_ref().is_some_and(|only| only.iter().any(|o| o == name))
    }

    /// Whether a fetch or clone takes the states of the cache overlay `name` (`Repo::cache_data_overlay`) as the
    /// caller chose them: `--overlay` names it, or `--line` or a cache selection asks for the caches of lines. Else it
    /// is a plain fetch: only the records come, and a command that needs a cache fetches it on demand
    /// (`fetch_caches_on_demand`, docs/remotes.md).
    pub(crate) fn takes_cache_states(&self, name: &str) -> bool {
        self.overlays.as_ref().is_some_and(|only| only.contains(name))
            || !self.lines.is_empty()
            || !self.caches_of.is_empty()
            || self.caches_of_state.is_some()
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// One authenticated head snapshot for clone restoration and working folders.
    #[serde(skip)]
    pub clone_lines: Option<Vec<crate::model::Line>>,
    pub records: usize,
    pub states: usize,
    /// Git packs (`git/`) that a push wrote or a fetch installed.
    pub git_packs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<download::GatewayReport>,
    #[serde(skip)]
    pub state_ids: Vec<String>,
    #[serde(skip)]
    pub overlay_transfers: Vec<(String, crate::model::Transfer)>,
    pub files: usize,
    pub bytes: u64,
    pub omitted: Vec<String>,
    pub overlaps: usize,
    pub gaps: Vec<String>,
    pub missing_lines: Vec<String>,
    /// One line per overlay that moved: "<name>: N records, M states".
    pub overlays: Vec<String>,
}

#[derive(Default)]
pub struct ConfigureOptions {
    pub paper_key_out: Option<PathBuf>,
    pub paper_key_tty: bool,
    /// The mode of a new store. Private and Public stores need no paper key.
    pub mode: Mode,
}

#[derive(Debug, Serialize)]
pub struct Configured {
    pub created: bool,
    pub recovery_key_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StorePin {
    pub project: String,
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paper_recipient: Option<String>,
    /// The project mode first seen; a pin without it is a Secret store.
    #[serde(default, skip_serializing_if = "Mode::is_secret")]
    pub mode: Mode,
}

impl StorePin {
    fn check(&self, descriptor: &Descriptor) -> Result<()> {
        if self.project != descriptor.project {
            bail!("store project changed: pinned {}, store.json {}", self.project, descriptor.project);
        }
        // Public and Private differ only in the server access list; a change between Secret and plain is refused.
        if self.mode.is_secret() != descriptor.mode.is_secret() {
            bail!("store mode changed: pinned {}, store.json {}", self.mode.name(), descriptor.mode.name());
        }
        if self.recipient != descriptor.recipient {
            bail!(
                "store recipient changed: pinned {}, store.json {}; use layr remote trust-store <name> --recipient <age1…>",
                self.recipient,
                descriptor.recipient
            );
        }
        Ok(())
    }
}

pub struct Store {
    backend: Backend,
    url: String,
    project: String,
    machine: String,
    key: ed25519_dalek::SigningKey,
    dir: PathBuf,
    paper_key_text: Option<String>,
    person: Option<crate::person::Access>,
    pin: Option<StorePin>,
    aws_profile: Option<String>,
    layer: Option<overlays::LayerAccess>,
    epoch: Option<epochs::Current>,
    epoch_public: bool,
    publishing_epoch: bool,
    force_public: bool,
    public_clone: bool,
    public_reader: std::sync::atomic::AtomicBool,
    /// The project mode chosen for this remote; None accepts the mode in store.json.
    mode: Option<Mode>,
}

/// Opening a store performs no remote I/O. Configure it with the caller's environment next.
pub fn open(url: &str, repo: &Repo) -> Result<Store> {
    let mut pin: Option<StorePin> = None;
    for (key, _) in repo.p.db.config_all()? {
        let Some(name) = key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")) else { continue };
        if repo.p.config_value(&key)?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() != Some(url) {
            continue;
        }
        if let Some(value) = repo.p.config_value(&format!("cloud.remote.{name}.store"))? {
            let stored: StorePin = serde_json::from_value(value).context("invalid store pin")?;
            if pin.as_ref().is_some_and(|pin| pin.project != stored.project || pin.recipient != stored.recipient) {
                bail!("conflicting local pins for store {url}");
            }
            pin = Some(stored);
        }
    }
    let mut store = open_with_pin(url, repo, pin)?;
    for (key, _) in repo.p.db.config_all()? {
        let Some(name) = key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")) else { continue };
        if repo.p.config_value(&key)?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(url) {
            store.aws_profile =
                repo.p.config_value(&format!("remote.{name}.aws-profile"))?.and_then(|v| v.as_str().map(str::to_owned));
            store.public_clone = repo.p.config_value(&format!("cloud.remote.{name}.public"))? == Some(serde_json::json!(true));
            store.mode = configured_mode(repo, name)?;
            store.choose_transport(store.mode)?;
            break;
        }
    }
    Ok(store)
}

/// The mode recorded by `remote add --mode` for a remote, if any.
pub(crate) fn configured_mode(repo: &Repo, name: &str) -> Result<Option<Mode>> {
    repo.p
        .config_value(&format!("cloud.remote.{name}.mode"))?
        .and_then(|v| v.as_str().map(str::to_owned))
        .map(|v| Mode::parse(&v))
        .transpose()
}

impl Store {
    /// Require this project mode in store.json (`remote add --mode`).
    pub fn require_mode(&mut self, mode: Option<Mode>) -> Result<()> {
        self.mode = mode;
        self.choose_transport(mode)
    }

    /// A `layr+https://` remote talks to the Layr Cloud API v1 unless it is Secret: a Secret project keeps the
    /// encrypted store routes of the gateway. No mode is Private, the default of a new store.
    pub fn choose_transport(&mut self, mode: Option<Mode>) -> Result<()> {
        if !self.url.starts_with("layr+https://") {
            return Ok(());
        }
        // A pin without a mode is an existing encrypted store. Keep using its gateway until a
        // verified migration changes the pin; never silently downgrade a pinned plain store.
        let effective_mode = mode.or_else(|| self.pin.as_ref().map(|pin| pin.mode));
        let api = !effective_mode.is_some_and(|m| m.is_secret());
        if api != matches!(self.backend.root(), Backend::Api(_)) {
            self.backend = if api { Backend::Api(api::Api::open(&self.url)?) } else { Backend::open(&self.url)? };
        }
        Ok(())
    }

    /// `layr login`: the device sign-in of a person (docs/remotes.md, "Person login"). The session goes to the
    /// caller's `~/.config/layr/cloud/<host>.json`, and this machine's key is registered with it.
    pub fn login(&self, ctx: &Ctx, show: impl Fn(&str)) -> Result<String> {
        let mut api = self.backend.api().context("layr login is for Layr Cloud remotes (layr+https, Public or Private)")?.clone();
        api.set_token(None);
        let start = api.login_start()?;
        show(api.login_link(&start.link)?.as_str());
        let deadline = if start.expires > 0 { start.expires } else { crate::ids::now_ms() + 600_000 };
        let session = loop {
            if let Some(session) = api.login_poll(&start.code)? {
                break session;
            }
            if crate::ids::now_ms() >= deadline {
                bail!("the sign-in was not finished in time; run layr login again");
            }
            std::thread::sleep(std::time::Duration::from_millis(if cfg!(test) { 10 } else { 2000 }));
        };
        let path = api::session_file(&ctx.caller.home, api.host());
        crate::privs::as_caller_fs(ctx, || api::save_session(&path, &session))?;
        api.set_token(Some(session.token.clone()));
        api.register_machine(&self.machine, &self.key.verifying_key())?;
        Ok(session.login)
    }

    /// True for a remote of the Layr Cloud API v1.
    pub fn is_api(&self) -> bool {
        self.backend.api().is_some()
    }

    /// The route scope of this store on the Layr Cloud API: `overlays/<name>/` for a plain overlay layer.
    pub(crate) fn api_scope(&self) -> String {
        match &self.backend {
            Backend::Prefix(_, scope) => scope.clone(),
            _ => String::new(),
        }
    }
}

pub fn open_named(name: &str, repo: &Repo) -> Result<Store> {
    let url = crate::remote_config::remote_url(repo, name)?;
    let pin = repo
        .p
        .config_value(&format!("cloud.remote.{name}.store"))?
        .map(serde_json::from_value)
        .transpose()
        .context("invalid store pin")?;
    let mut store = open_with_pin(&url, repo, pin)?;
    store.mode = configured_mode(repo, name)?;
    store.choose_transport(store.mode)?;
    store.aws_profile = repo.p.config_value(&format!("remote.{name}.aws-profile"))?.and_then(|v| v.as_str().map(str::to_owned));
    store.public_clone = repo.p.config_value(&format!("cloud.remote.{name}.public"))? == Some(serde_json::json!(true));
    Ok(store)
}

/// The store remotes whose successful push records name each state: state id to remote names.
/// A state a store holds is recoverable from it (`restore_state`). A copy counts only while the
/// remote under the recorded name still has the recorded URL, and this machine has read that store
/// with its own key (`mark_read`). Removing or changing the remote ends the guarantee.
pub fn store_copies(repo: &Repo) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    let mut copies: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for rec in repo.p.db.records_with_op("push")? {
        let data = &rec.data;
        let (Some(name), Some(url)) = (data["remote"].as_str(), data["url"].as_str()) else { continue };
        if data["result"] != "success" || !matches!(crate::remote_config::kind(url), Ok(crate::remote_config::Kind::Store)) {
            continue;
        }
        if crate::remote_config::remote_url(repo, name).ok().as_deref() != Some(url)
            || !store_dir(repo, url).join("read").is_file()
        {
            continue;
        }
        for id in data["state_ids_sent"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
            let names = copies.entry(id.to_string()).or_default();
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    Ok(copies)
}

/// Restore a state that is not on this machine from a store remote that holds it. Fails with the
/// reason when no remote gives it (for example, when the remote is offline).
/// Install the Git objects that the store remotes of the project hold (`gitpack`). Used when the
/// local Git cache is lost: an imported state has no tree to rebuild its commits from. Returns the
/// number of packs installed.
pub fn restore_git_objects(ctx: &Ctx, repo: &Repo) -> Result<usize> {
    let mut names: Vec<String> = repo
        .p
        .db
        .config_all()?
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")).map(str::to_owned))
        .collect();
    names.sort();
    names.dedup();
    let mut installed = 0;
    for name in names {
        let mut store = open_named(&name, repo)?;
        store.connect(ctx)?;
        let descriptor = store.descriptor()?;
        let identity = store.identity(&descriptor, None)?;
        installed += gitpack::fetch(&store, &crate::sync_local::Local::read(ctx, repo), &identity)?;
    }
    Ok(installed)
}

/// The states that a `--heads` clone or fetch left on a store remote (`states-left` in its local folder).
pub(super) fn states_left_file(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("states-left")
}

#[cfg(test)]
thread_local! {
    static REMOTE_STATE_RESTORES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub fn restore_state(repo: &Repo, id: &str) -> Result<()> {
    if !repo.restore_from_remote {
        bail!("state {} is not on this machine (run layr fetch <remote>)", crate::ids::short(id));
    }
    #[cfg(test)]
    REMOTE_STATE_RESTORES.with(|count| count.set(count.get() + 1));
    if repo.p.is_overlay() {
        return restore_overlay_state(repo, id);
    }
    let mut names = store_copies(repo)?.remove(id).unwrap_or_default();
    // A `--heads` clone or fetch left the state on the remote that it read.
    for (key, _) in repo.p.db.config_all()? {
        let Some(name) = key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")) else { continue };
        let Ok(url) = crate::remote_config::remote_url(repo, name) else { continue };
        let left = std::fs::read_to_string(states_left_file(&store_dir(repo, &url))).unwrap_or_default();
        if left.lines().any(|line| line == id) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        bail!("state {} is not on this machine (run layr fetch <remote>)", crate::ids::short(id));
    }
    let ctx = repo.ctx.context("restore needs a command context")?;
    restore_from_remotes(
        ctx,
        &names,
        id,
        |name| open_named(name, repo),
        |store, ctx| store.connect(ctx),
        |store| reader::restore_states(store, repo, &[id.to_string()]).map(|_| ()),
    )
}

/// `restore_state` for a state of an overlay project (`<project>%<overlay>`): a cache state that `gc` evicted
/// (`cmd::admin::evict_caches`). The overlay keeps no remotes; the store remotes of the project are tried in order.
fn restore_overlay_state(orepo: &Repo, id: &str) -> Result<()> {
    let ctx = orepo.ctx.context("restore needs a command context")?;
    let (project, name) = orepo.p.name.split_once('%').context("overlay project name")?;
    let parent = Repo::open(ctx, project)?;
    let names = store_remote_names(&parent)?;
    if names.is_empty() {
        bail!("state {} is not on this machine, and project {project} has no store remote", crate::ids::short(id));
    }
    restore_from_remotes(
        ctx,
        &names,
        id,
        |remote| Ok(remote.to_string()),
        |_, _| Ok(()),
        |remote| overlays::restore_overlay_state(ctx, &parent, orepo, name, remote, id),
    )
}

/// Fetch on demand: a plain fetch, pull or clone takes only the records of a cache overlay (owner decision
/// 2026-11-13), so the cache that a new line from the line `source` (or from the state `start`) takes can be on a store
/// remote only. This fetches those cache states from the store remotes of the project, before the line or the copy of
/// a check takes its caches (`take_caches`, `pull_caches`, `copy_caches`, `take_follow_caches`). It reads only the
/// records and the named cache states, and nothing when every such cache is here. The caller holds no project lock.
/// A failure leaves the folder cold, with a warning; the command goes on. `quiet` leaves out the progress line.
///
/// It must not make each command wait: connections are quick (`gateway::quick`), a remote that failed is not asked
/// again for `FAILED_FOR`, and a cache that a remote did not give is not asked of it again for `ABSENT_FOR`
/// (`on-demand.json` in the remote's local folder).
pub fn fetch_caches_on_demand(ctx: &Ctx, repo: &Repo, start: &str, source: Option<&crate::model::Line>, quiet: bool) {
    if let Err(e) = gateway::quick(|| fetch_caches_inner(ctx, repo, start, source, quiet)) {
        eprintln!("layr: caches not fetched from the store remote: {e:#}");
    }
}

const FAILED_FOR: i64 = 10 * 60 * 1000;
const ABSENT_FOR: i64 = 60 * 60 * 1000;

/// The marks of the fetch on demand for one store remote: when it last failed, and the cache states that it did not
/// give (state id to time, in ms).
#[derive(Default, Deserialize, Serialize)]
struct OnDemandMarks {
    #[serde(default)]
    failed_at: Option<i64>,
    #[serde(default)]
    absent: std::collections::BTreeMap<String, i64>,
}

fn on_demand_marks_path(repo: &Repo, url: &str) -> PathBuf {
    store_dir(repo, url).join("on-demand.json")
}

fn read_on_demand_marks(repo: &Repo, url: &str) -> OnDemandMarks {
    std::fs::read(on_demand_marks_path(repo, url)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_on_demand_marks(repo: &Repo, url: &str, marks: &mut OnDemandMarks) -> Result<()> {
    let now = crate::ids::now_ms();
    marks.absent.retain(|_, at| now - *at < ABSENT_FOR);
    let path = on_demand_marks_path(repo, url);
    std::fs::create_dir_all(path.parent().context("remote folder")?)?;
    crate::fsutil::atomic_write(&path, &serde_json::to_vec(marks)?, 0o600)
}

fn fetch_caches_inner(ctx: &Ctx, repo: &Repo, start: &str, source: Option<&crate::model::Line>, quiet: bool) -> Result<()> {
    if repo.p.is_overlay() {
        return Ok(());
    }
    let absent_here = |orepo: &Repo, id: &str| -> Result<bool> {
        Ok(!orepo.p.has_state(id) && crate::objstore::verified(&orepo.p, id)?.is_none())
    };
    // The caches that the line takes, from the records: absent here, and usable by the caller (`usable_cache`).
    let mut want: std::collections::BTreeMap<String, (String, Repo)> = std::collections::BTreeMap::new();
    for (name, _folder) in repo.folder_overlays()? {
        let Ok(orepo) = repo.overlay_repo(&name) else { continue };
        let ids = match source {
            Some(line) => fetch::caches_of_lines_for(repo, &orepo, std::slice::from_ref(&line.name), Some(ctx.caller.uid))?,
            None => fetch::caches_from_state(repo, &orepo, start, ctx.caller.uid)?,
        };
        for id in ids {
            if absent_here(&orepo, &id)? {
                want.insert(id, (name.clone(), repo.overlay_repo(&name)?));
            }
        }
    }
    if want.is_empty() {
        return Ok(());
    }
    let mut errors = Vec::new();
    for name in store_remote_names(repo)? {
        let url = crate::remote_config::remote_url(repo, &name)?;
        let mut marks = read_on_demand_marks(repo, &url);
        let now = crate::ids::now_ms();
        if marks.failed_at.is_some_and(|at| now - at < FAILED_FOR) {
            errors.push(format!("{name}: it failed less than {} minutes ago", FAILED_FOR / 60_000));
            continue;
        }
        let mut asked = std::collections::BTreeSet::new();
        for (id, (_, orepo)) in &want {
            if absent_here(orepo, id)? && !marks.absent.get(id).is_some_and(|at| now - *at < ABSENT_FOR) {
                asked.insert(id.clone());
            }
        }
        if asked.is_empty() {
            continue;
        }
        let overlays: std::collections::BTreeSet<String> = asked.iter().map(|id| want[id].0.clone()).collect();
        if !quiet {
            eprintln!("layr: fetching the cache of {} from {name}", overlays.iter().cloned().collect::<Vec<_>>().join(", "));
        }
        let opts = Options {
            caches: true,
            heads: true,
            // Only the cache overlays with an absent cache, and no base state.
            overlays: Some(overlays),
            only_caches: Some(asked.clone()),
            only_states: Some(std::collections::BTreeSet::new()),
            ..Default::default()
        };
        let attempt = (|| -> Result<()> {
            let mut fresh = Repo::open(ctx, &repo.p.name)?;
            let mut store = open_named(&name, &fresh)?;
            store.configure(ctx, &ConfigureOptions::default())?;
            store.fetch(ctx, &mut fresh, &opts)?;
            Ok(())
        })();
        match attempt {
            Err(e) => {
                marks.failed_at = Some(now);
                errors.push(format!("{name}: {e:#}"));
            }
            Ok(()) => {
                marks.failed_at = None;
                for id in &asked {
                    if absent_here(&want[id].1, id)? {
                        marks.absent.insert(id.clone(), now);
                        errors.push(format!("{name}: cache state {} is not there", crate::ids::short(id)));
                    }
                }
            }
        }
        write_on_demand_marks(repo, &url, &mut marks)?;
        let mut left = false;
        for (id, (_, orepo)) in &want {
            left |= absent_here(orepo, id)?;
        }
        if !left {
            return Ok(());
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}

/// Whether the cache state `id` of the overlay `orepo` is on a store remote of the project, so that a command can
/// fetch it on demand (`fsck`). A state of another machine came here as a record only, from a remote that holds it;
/// a state of this machine counts only when a successful push to a store remote that the project still has named it.
pub fn cache_on_store_remote(repo: &Repo, orepo: &Repo, id: &str) -> Result<bool> {
    let remotes = store_remote_names(repo)?;
    if remotes.is_empty() {
        return Ok(false);
    }
    if orepo.p.db.state_machine(id)?.is_some_and(|machine| machine != repo.p.store.machine.id) {
        return Ok(true);
    }
    for rec in orepo.p.db.records_with_op("push")? {
        let data = &rec.data;
        let named = data["state_ids_sent"].as_array().into_iter().flatten().any(|v| v.as_str() == Some(id));
        if named && data["result"] == "success" && data["remote"].as_str().is_some_and(|r| remotes.iter().any(|n| n == r)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A store remote of the project as eviction sees it (`cache_stores`): the URL that push records name, the store
/// recipient that the project pins, and the key epoch that the store publishes now for each overlay (by name). None:
/// the store has no key epochs (a plain layer of Layr Cloud).
pub struct CacheStore {
    url: String,
    recipient: String,
    epochs: Option<std::collections::BTreeMap<String, String>>,
}

/// The store remotes of the project whose copies eviction may count (`gc.evict on`, `cmd::admin::evict_caches`):
/// this machine has read the store with its own key (`mark_read`), the project pins its recipient, and the key
/// listing of the store reads now. The listing is read once per store, before the project lock: the epoch that
/// counts is the one the store publishes, not the newest key record here (another machine may have changed the key
/// and deleted the older epochs since this machine read its records). A store that cannot be read counts nothing,
/// with a warning.
pub fn cache_stores(ctx: &Ctx, repo: &Repo) -> Vec<CacheStore> {
    let mut out = Vec::new();
    let Ok(names) = store_remote_names(repo) else { return out };
    for name in names {
        let read = (|| -> Result<Option<CacheStore>> {
            let url = crate::remote_config::remote_url(repo, &name)?;
            if !store_dir(repo, &url).join("read").is_file() {
                return Ok(None);
            }
            let Some(pin) = repo
                .p
                .config_value(&format!("cloud.remote.{name}.store"))?
                .and_then(|v| serde_json::from_value::<StorePin>(v).ok())
            else {
                return Ok(None);
            };
            let mut store = open_named(&name, repo)?;
            store.configure(ctx, &ConfigureOptions::default())?;
            let epochs = if store.is_api() {
                None
            } else {
                let descriptor = store.root_descriptor()?;
                anyhow::ensure!(descriptor.recipient == pin.recipient, "the store has another recipient than its pin");
                Some(overlays::published_epochs(&store, repo, &descriptor.recipient)?)
            };
            Ok(Some(CacheStore { url: layr_model::operations::public_url(&url), recipient: pin.recipient, epochs }))
        })();
        match read {
            Ok(Some(found)) => out.push(found),
            Ok(None) => {}
            Err(e) => eprintln!("warning: gc: store remote {name} cannot be read ({e:#}); no cache counts as held by it"),
        }
    }
    out
}

/// The cache states of the overlay `orepo` (overlay `name` of `repo`) that a store remote of the project holds, for
/// eviction (`gc.evict on`, docs/cache-overlays.md, "Eviction"). A state counts when a successful push record of the
/// overlay names it, and the record:
/// - is of this machine or of the machine that made the state;
/// - names the URL of a store in `stores` (`cache_stores`);
/// - names the store by its recipient, and that is the recipient that the project pins for the remote
///   (`cloud.remote.<name>.store`): a store made again at the same URL, or a path that names another disk, differs;
/// - names the key epoch that the store publishes now, and that is also the newest key here with no key change
///   waiting (`overlays::settled_epoch`): a key change deletes the trees of the older epochs.
///
/// The overlay keeps no remotes of its own: the URLs and pins come from the project.
pub fn cache_store_copies(
    repo: &Repo,
    orepo: &Repo,
    name: &str,
    stores: &[CacheStore],
) -> Result<std::collections::BTreeSet<String>> {
    // URL (as a record writes it) to the store recipient and the epoch that count there.
    let mut counted: std::collections::BTreeMap<&str, (&str, Option<String>)> = std::collections::BTreeMap::new();
    for store in stores {
        let published = match &store.epochs {
            None => None,
            Some(epochs) => match epochs.get(name) {
                Some(epoch) => Some(epoch.clone()),
                None => continue,
            },
        };
        if overlays::settled_epoch(orepo, &store.recipient)? != Some(published.clone()) {
            continue;
        }
        counted.insert(&store.url, (&store.recipient, published));
    }
    let mut held = std::collections::BTreeSet::new();
    if counted.is_empty() {
        return Ok(held);
    }
    let here = &repo.p.store.machine.id;
    for rec in orepo.p.db.records_with_op("push")? {
        let data = &rec.data;
        let Some((recipient, epoch)) = data["url"].as_str().and_then(|url| counted.get(url)) else { continue };
        if data["result"] != "success" || data["store"].as_str() != Some(*recipient) || data["epoch"].as_str() != epoch.as_deref()
        {
            continue;
        }
        for id in data["state_ids_sent"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
            let maker = orepo.p.db.state_machine(id)?;
            if rec.machine == *here || maker.as_deref() == Some(rec.machine.as_str()) {
                held.insert(id.to_owned());
            }
        }
    }
    Ok(held)
}

/// The names of the store remotes of the project (`cloud.remote.<name>.url` of a store), sorted.
pub fn store_remote_names(repo: &Repo) -> Result<Vec<String>> {
    let mut remotes: Vec<String> = repo
        .p
        .db
        .config_all()?
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")).map(str::to_owned))
        .filter(|name| {
            crate::remote_config::remote_url(repo, name)
                .is_ok_and(|url| matches!(crate::remote_config::kind(&url), Ok(crate::remote_config::Kind::Store)))
        })
        .collect();
    remotes.sort();
    remotes.dedup();
    Ok(remotes)
}

/// Try each remote that holds the state, in order. Each one is opened, connected with the same
/// path as fetch (`connect` sets the credentials, the gateway user and the epoch) and read.
fn restore_from_remotes<S, C>(
    ctx: &C,
    names: &[String],
    id: &str,
    mut open: impl FnMut(&str) -> Result<S>,
    mut connect: impl FnMut(&mut S, &C) -> Result<()>,
    mut read: impl FnMut(&S) -> Result<()>,
) -> Result<()> {
    let mut errors = Vec::new();
    for name in names {
        let attempt = open(name).and_then(|mut store| {
            connect(&mut store, ctx)?;
            read(&store)
        });
        match attempt {
            Ok(()) => return Ok(()),
            Err(e) => errors.push(format!("{name}: {e:#}")),
        }
    }
    bail!(
        "state {} is not on this machine; store remote {} holds it but cannot give it ({}); check the remote and run the command again",
        crate::ids::short(id),
        names.join(", "),
        errors.join("; ")
    )
}

/// The folder of local state for the store at `url` (the same folder `open_with_pin` uses).
fn store_dir(repo: &Repo, url: &str) -> PathBuf {
    repo.p.meta_dir().join("remote-stores").join(hex::encode(ContentAddress::digest(url.as_bytes())))
}

/// The text that names the store at `url` whichever way the URL spells it: without a trailing slash, `.` or `..`, with
/// the host in lower case and without a default port, and for a file store the canonical path when it exists.
fn store_identity(url: &str) -> String {
    if let Some(address) = url.strip_prefix("layr+https://") {
        if let Ok(parsed) = reqwest::Url::parse(&format!("https://{address}")) {
            return format!("layr+{}", parsed.as_str().trim_end_matches('/'));
        }
    } else if url.starts_with("file://") {
        if let Some(path) = reqwest::Url::parse(url).ok().and_then(|parsed| parsed.to_file_path().ok()) {
            let path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.components().collect());
            return format!("file://{}", path.display());
        }
    } else if let Some(path) = url.strip_prefix("s3://") {
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        return format!("s3://{bucket}/{}", prefix.trim_matches('/'));
    }
    url.to_string()
}

/// Records that this machine read the store with its own key. `store_copies` counts a copy only
/// for a store this machine can read.
fn mark_read(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("read"), b"")?;
    Ok(())
}

/// True when the store format restores the tree of this state exactly (`writer::format_gap`).
/// Retention evicts only such a state. It checks the layers on disk and does not build a complete
/// tree (`thin::complete_tree` would copy one), so a thin state is checked layer by layer.
pub fn restorable(repo: &Repo, id: &str) -> Result<bool> {
    #[cfg(target_os = "macos")]
    let roots: Vec<std::path::PathBuf> =
        crate::thin::Layered::of_state(&repo.p, id)?.layers().iter().map(|layer| layer.root.clone()).collect();
    #[cfg(not(target_os = "macos"))]
    let roots = vec![repo.p.state_path(id)];
    for root in roots {
        if writer::format_gap(&root)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn open_with_pin(url: &str, repo: &Repo, pin: Option<StorePin>) -> Result<Store> {
    let mut store = Store {
        backend: Backend::open(url)?,
        url: url.to_string(),
        project: repo.p.id.clone(),
        machine: repo.p.store.machine.id.clone(),
        key: repo.p.store.machine.key.clone(),
        dir: store_dir(repo, url),
        paper_key_text: None,
        person: None,
        pin,
        aws_profile: None,
        layer: None,
        epoch: None,
        epoch_public: false,
        publishing_epoch: false,
        force_public: false,
        public_clone: false,
        public_reader: std::sync::atomic::AtomicBool::new(false),
        mode: None,
    };
    store.choose_transport(None)?;
    Ok(store)
}

impl Store {
    pub(crate) fn gateway_report(&self) -> Option<download::GatewayReport> {
        self.backend.gateway_transfer_stats().map(Into::into)
    }

    /// The folder whose lock lets one push or sweep of this store (or of a layer of it) run at a time on this machine:
    /// `dir`, under the store's identity rather than its URL, so that two URLs of one store share it. The local state of
    /// the store stays under its URL.
    pub(super) fn lock_dir(&self) -> PathBuf {
        let raw = hex::encode(ContentAddress::digest(self.url.as_bytes()));
        let Some(top) = self.dir.ancestors().find(|dir| dir.file_name().is_some_and(|name| *name == *raw)) else {
            return self.dir.clone();
        };
        let identity = hex::encode(ContentAddress::digest(store_identity(&self.url).as_bytes()));
        top.with_file_name(identity).join(self.dir.strip_prefix(top).unwrap_or(std::path::Path::new("")))
    }

    pub fn write_index(&mut self, ctx: &Ctx, repo: &Repo) -> Result<index::Report> {
        self.connect(ctx)?;
        repo.require(ctx, "sync")?;
        // No project lock: the index only reads the records and writes create-only objects of the remote (heads take
        // the next free number), and it can take minutes on a large store.
        index::write(self, repo)
    }
    pub(crate) fn connect(&mut self, ctx: &Ctx) -> Result<()> {
        self.connect_transport(ctx)?;
        epochs::connect(self)
    }

    /// Set the deadline on an events remote before connecting it.
    pub(crate) fn set_wait_deadline(&mut self, deadline: std::time::Instant) -> Result<()> {
        match &mut self.backend {
            Backend::Gateway(gateway) => gateway.deadline = Some(deadline),
            Backend::Api(api) => api.deadline = Some(deadline),
            _ => bail!(backend::EVENTS_REMOTE_REQUIRED),
        }
        Ok(())
    }

    fn connect_transport(&mut self, ctx: &Ctx) -> Result<()> {
        const EPOCHS_REFUSED: &str = "does not open projects with access epochs";
        self.backend = self.backend.root().clone();
        self.epoch = None;
        self.epoch_public = false;
        self.backend.as_caller(ctx)?;
        let public_only = self.force_public
            || (self.public_clone
                && ctx.env("LAYR_GATEWAY_TOKEN_FILE").is_none()
                && ctx.env("LAYR_PERSON_KEY_FILE").is_none()
                && ctx.env("LAYR_PAPER_KEY_FILE").is_none());
        // A new, unpinned URL may still name a legacy gateway. Project info also exists on the
        // older read API, so check the V1 records route before selecting the protocol.
        let mut legacy_probe = false;
        if let Backend::Api(api) = &mut self.backend {
            api.connect(ctx, &self.project, public_only)?;
            // The server reader of Layr Cloud 0.2.0 refuses a project with access epochs (HTTP 400). Its data is
            // on the gateway routes, as for an older server: after a rotation on 2026-11-24 every new clone of
            // project layr failed here, CI jobs too.
            let info = match api.project_info() {
                Err(error) if format!("{error:#}").contains(EPOCHS_REFUSED) => None,
                info => info?,
            };
            let may_probe = self.pin.is_none() && self.mode.is_none();
            let legacy = may_probe
                && match info {
                    None => true,
                    Some(_) => !api.has_v1_records()?,
                };
            if !legacy {
                self.person = None;
                self.paper_key_text = None;
                return Ok(());
            }
            legacy_probe = true;
            let mut gateway = gateway::Gateway::open(&self.url)?;
            gateway.deadline = api.deadline;
            self.backend = Backend::Gateway(gateway);
            self.backend.as_caller(ctx)?;
        }
        if let Backend::Gateway(gateway) = &mut self.backend {
            gateway.listing_cache_dir = Some(self.dir.clone());
            if public_only {
                gateway.connect_public(&ctx.env)?;
                gateway.use_public_access();
            } else {
                gateway.connect(ctx)?;
            }
        }
        if self.force_public {
            self.person = None;
            self.paper_key_text = None;
        } else {
            self.person = if ctx.env("LAYR_PERSON_KEY_FILE").is_some() {
                let login = match &self.backend {
                    Backend::Gateway(g) => Some(crate::person::gateway_login(g)?),
                    _ => None,
                };
                crate::privs::as_caller_fs(ctx, || {
                    crate::person::load(&ctx.env, &ctx.caller.home, &ctx.cwd, ctx.caller.uid, login.as_deref())
                })?
            } else {
                None
            };
            if let (Backend::Gateway(gateway), Some(person)) = (&self.backend, &mut self.person) {
                crate::person::gateway_access(gateway, person)?;
            }
            self.paper_key_text = ctx
                .env("LAYR_PAPER_KEY_FILE")
                .map(|path| {
                    crate::privs::as_caller_fs(ctx, || {
                        use std::io::Read;
                        let mut text = String::new();
                        std::fs::File::open(ctx.cwd.join(path))?.take(4097).read_to_string(&mut text)?;
                        if text.len() > 4096 {
                            bail!("invalid paper key file");
                        }
                        Ok(text)
                    })
                })
                .transpose()?;
        }
        if let Some(path) = self.url.strip_prefix("s3://") {
            let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
            let mut env = ctx.env.clone();
            if !env.get("AWS_PROFILE").is_some_and(|v| !v.is_empty()) {
                if let Some(profile) = &self.aws_profile {
                    env.insert("AWS_PROFILE".into(), profile.clone());
                }
            }
            env.entry("HOME".into()).or_insert_with(|| ctx.caller.home.to_string_lossy().into_owned());
            self.backend = crate::privs::as_caller_fs(ctx, || {
                let mut s3 = s3::S3::with_env(bucket, prefix, &env)?;
                s3.resolve_credentials()?;
                Ok(Backend::S3(s3))
            })?;
        }
        if legacy_probe {
            // Fail before configure can create a store. The descriptor also checks the project,
            // recipient, mode and any person pin against the same origin.
            self.descriptor().context("V1 project was not found and the legacy store could not be verified")?;
        }
        Ok(())
    }

    pub fn use_public_access(&mut self, public: bool) {
        self.force_public = public;
    }

    pub fn use_aws_profile(&mut self, profile: Option<String>) {
        self.aws_profile = profile;
    }

    pub fn request_person(&mut self, ctx: &Ctx) -> Result<Option<crate::person::Access>> {
        self.connect(ctx)?;
        if self.is_api() {
            return crate::privs::as_caller_fs(ctx, || {
                crate::person::load(&ctx.env, &ctx.caller.home, &ctx.cwd, ctx.caller.uid, None)
            });
        }
        Ok(self.person.clone())
    }

    /// Register the executor before it signs a request claim. A fresh clone has not pushed yet.
    pub fn register_request_executor(&self) -> Result<()> {
        if let Some(api) = self.backend.api() {
            api.register_machine(&self.machine, &self.key.verifying_key())?;
        }
        Ok(())
    }

    /// Current request encryption target; the root epoch retains the legacy trust anchor.
    pub fn request_epoch(&mut self, ctx: &Ctx) -> Result<(String, String)> {
        self.connect(ctx)?;
        Ok((self.epoch.as_ref().map(|e| e.key.epoch.clone()).unwrap_or_else(|| "root".into()), self.descriptor()?.recipient))
    }

    /// Authenticated immutable-object access for person requests and executor claims.
    pub fn request_backend(&mut self, ctx: &Ctx) -> Result<&Backend> {
        self.prepare_requests(ctx)?;
        Ok(self.backend())
    }

    pub(crate) fn prepare_requests(&mut self, ctx: &Ctx) -> Result<()> {
        self.connect(ctx)?;
        let descriptor = self.descriptor()?;
        self.identity(&descriptor, self.paper_key_text.as_deref())?;
        Ok(())
    }

    pub(crate) fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Project vault identity for opening person requests on the trusted executor.
    pub fn request_identity(&mut self, ctx: &Ctx) -> Result<String> {
        self.connect(ctx)?;
        let descriptor = self.descriptor()?;
        self.identity(&descriptor, self.paper_key_text.as_deref())
    }

    pub fn configure(&mut self, ctx: &Ctx, opts: &ConfigureOptions) -> Result<Configured> {
        if opts.paper_key_tty && opts.paper_key_out.is_some() {
            bail!("use either --paper-key-tty or --paper-key-out, not both");
        }
        // Establish and verify the immutable root pin before epoch discovery.
        // The actual operation resolves current epochs after configuration.
        self.connect_transport(ctx)?;
        if self.backend.get("store.json")?.is_some() {
            if opts.paper_key_tty {
                bail!("the store already exists; its paper key cannot be shown again");
            }
            self.pin_descriptor()?;
            return Ok(Configured { created: false, recovery_key_file: None });
        }
        if self.is_api() {
            bail!(
                "Layr Cloud has no project at {}, or this token cannot read it; create the project on the site first",
                self.url
            );
        }
        if self.project.is_empty() {
            bail!("the remote store is empty");
        }
        if !opts.mode.is_secret() {
            if opts.paper_key_tty || opts.paper_key_out.is_some() {
                bail!("a {} store has no paper key; drop --paper-key-out and --paper-key-tty", opts.mode.name());
            }
            writer::initialize_plain(self, opts.mode)?;
            self.pin_descriptor()?;
            return Ok(Configured { created: true, recovery_key_file: None });
        }
        let (file, paper_recipient) = if opts.paper_key_tty {
            (None, writer::initialize_tty(self, ctx)?)
        } else {
            let path = opts.paper_key_out.as_ref().context("a new store requires --paper-key-tty or --paper-key-out <path>")?;
            let (file, recipient) = writer::initialize(self, ctx, path)?;
            (Some(file), recipient)
        };
        // The paper key that this machine just made opens the store: it is read access (`mark_read`).
        mark_read(&self.dir)?;
        self.pin_descriptor()?;
        self.pin.as_mut().expect("initialized store pin").paper_recipient = Some(paper_recipient);
        Ok(Configured { created: true, recovery_key_file: file })
    }

    fn pin_descriptor(&mut self) -> Result<()> {
        let descriptor = self.root_descriptor()?;
        if let Some(pin) = &self.pin {
            pin.check(&descriptor)?;
        }
        if let Some(pin) = self.person.as_ref().and_then(|person| person.index.pin(&descriptor.project)) {
            pin.check_store(&descriptor)?;
        }
        let paper_recipient = self.pin.as_ref().and_then(|pin| pin.paper_recipient.clone());
        self.pin = Some(StorePin {
            mode: descriptor.mode,
            project: descriptor.project,
            recipient: descriptor.recipient,
            paper_recipient,
        });
        Ok(())
    }

    /// Caller holds the project lock. Config records carry the machine signature.
    pub fn save_pin(&self, locked: &crate::store::LockedProject<'_>, ctx: &Ctx, repo: &Repo, name: &str) -> Result<()> {
        if let Some(pin) = &self.pin {
            let mut pin = pin.clone();
            // Learn the public paper recipient once. LAYR_PAPER_KEY_FILE may hold the paper key of
            // another store: a key that does not open this store's paper slot is not this store's
            // paper key, so nothing is recorded.
            if let (None, Some(paper)) = (&pin.paper_recipient, &self.paper_key_text) {
                let opener = keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
                let root = self.backend.root();
                // Configuration pins the root descriptor before resolving epochs. Open the
                // machine/person slot first so a gateway paper-slot read has its own proof.
                if !root.can_read() {
                    let identity = self.own_identity(&self.root_descriptor()?, None)?;
                    root.unlock_reader(&identity)?;
                }
                if let Some(slot) = root.get("slots/paper.age")? {
                    if let Ok(identity) = keys::open_slot(&slot, &opener) {
                        if keys::recipient_of(&identity)? != pin.recipient {
                            bail!("paper slot does not match store recipient");
                        }
                        pin.paper_recipient = Some(keys::recipient_of(&opener)?);
                    }
                }
            }
            // A public clone becomes an ordinary remote once the owner grants this
            // machine its own slot. Never create a slot to perform this transition.
            if repo.p.config_value(&format!("cloud.remote.{name}.public"))? == Some(serde_json::json!(true)) {
                if let Some(slot) = self.backend.get(&format!("slots/machine-{}.age", self.machine))? {
                    let current_recipient = self.descriptor()?.recipient;
                    if keys::open_machine_slot(&slot, &self.key)
                        .ok()
                        .is_some_and(|identity| keys::recipient_of(&identity).ok().as_deref() == Some(current_recipient.as_str()))
                    {
                        crate::project_config::set_config(
                            locked,
                            ctx,
                            repo,
                            &format!("cloud.remote.{name}.public"),
                            serde_json::json!(false),
                        )?;
                    }
                }
            }
            let key = format!("cloud.remote.{name}.store");
            let value = serde_json::to_value(&pin)?;
            if repo.p.config_value(&key)?.as_ref() != Some(&value) {
                crate::project_config::set_config(locked, ctx, repo, &key, value)?;
            }
        }
        Ok(())
    }

    pub fn trust_store(
        &mut self,
        locked: &crate::store::LockedProject<'_>,
        ctx: &Ctx,
        repo: &Repo,
        name: &str,
        recipient: &str,
    ) -> Result<()> {
        recipient.parse::<age::x25519::Recipient>().map_err(|e| anyhow::anyhow!("invalid store recipient: {e}"))?;
        let previous = self.pin.take();
        let descriptor = self.connect(ctx).and_then(|_| self.root_descriptor());
        self.pin = previous;
        let descriptor = descriptor?;
        if let Some(pin) = &self.pin {
            if pin.project != descriptor.project {
                bail!("store project changed: pinned {}, store.json {}", pin.project, descriptor.project);
            }
        }
        if descriptor.recipient != recipient {
            bail!("store recipient differs: requested {}, store.json {}", recipient, descriptor.recipient);
        }
        self.pin = Some(StorePin {
            mode: descriptor.mode,
            project: descriptor.project,
            recipient: descriptor.recipient,
            paper_recipient: None,
        });
        self.save_pin(locked, ctx, repo, name)
    }

    fn root_descriptor(&self) -> Result<Descriptor> {
        let descriptor: Descriptor =
            serde_json::from_slice(&self.backend.root().get("store.json")?.context("missing root descriptor")?)?;
        if descriptor.format != 2 || !crate::model::is_uuid(&descriptor.project) {
            bail!("invalid root store descriptor");
        }
        descriptor.check_mode()?;
        if descriptor.mode.is_secret() {
            descriptor
                .recipient
                .parse::<age::x25519::Recipient>()
                .map_err(|_| anyhow::anyhow!("invalid root store recipient"))?;
        }
        self.check_mode(&descriptor)?;
        if !self.project.is_empty() && self.project != descriptor.project {
            bail!("store project differs: local {}, store.json {}", self.project, descriptor.project);
        }
        Ok(descriptor)
    }

    fn descriptor(&self) -> Result<Descriptor> {
        if let Some(epoch) = &self.epoch {
            return Ok(Descriptor {
                format: 2,
                project: epoch.key.project.clone(),
                recipient: epoch.key.recipient.clone(),
                mode: Default::default(),
            });
        }
        if let Some(layer) = &self.layer {
            return Ok(Descriptor {
                format: 2,
                project: self.project.clone(),
                recipient: layer.recipient.clone(),
                mode: Default::default(),
            });
        }
        // A plain overlay layer of a Layr Cloud project (`overlays/<name>/`): no keys, the project's object store.
        if matches!(self.backend, Backend::Prefix(..)) && self.is_api() {
            return Ok(Descriptor {
                format: 2,
                project: self.project.clone(),
                recipient: keys::PLAIN.into(),
                mode: Mode::Private,
            });
        }
        let data = self
            .backend
            .get("store.json")?
            .context("remote store is not initialized; use layr remote add with --paper-key-out")?;
        let descriptor: Descriptor = serde_json::from_slice(&data)?;
        if descriptor.format != 2 {
            bail!("unsupported store format {}", descriptor.format);
        }
        if !crate::model::is_uuid(&descriptor.project) {
            bail!("store has an invalid project id");
        }
        if let Some(pin) = &self.pin {
            pin.check(&descriptor)?;
        }
        if let Some(person) = &self.person {
            if let Some(pin) = person.index.pin(&descriptor.project) {
                pin.check_store(&descriptor)?;
            }
        }
        if !self.project.is_empty() && descriptor.project != self.project {
            bail!("store project differs: local {}, store.json {}", self.project, descriptor.project);
        }
        descriptor.check_mode()?;
        if descriptor.mode.is_secret() {
            descriptor
                .recipient
                .parse::<age::x25519::Recipient>()
                .map_err(|e| anyhow::anyhow!("invalid store recipient: {e}"))?;
        }
        self.check_mode(&descriptor)?;
        Ok(descriptor)
    }

    /// The mode this machine chose for the remote (`remote add --mode`) must be the store's mode: a server
    /// must not turn a Secret project into a plain one, nor the reverse.
    fn check_mode(&self, descriptor: &Descriptor) -> Result<()> {
        if let Some(mode) = self.mode {
            if mode.is_secret() != descriptor.mode.is_secret() || (!self.is_api() && mode != descriptor.mode) {
                bail!("store mode differs: this remote is {}, store.json says {}", mode.name(), descriptor.mode.name());
            }
        }
        Ok(())
    }

    /// The store mode from store.json; Secret until the descriptor is read.
    pub fn store_mode(&self) -> Result<Mode> {
        Ok(self.descriptor()?.mode)
    }

    fn identity(&self, descriptor: &Descriptor, paper: Option<&str>) -> Result<String> {
        // A Private or Public store has no keys: the server's access list decides who reads and writes.
        if !descriptor.mode.is_secret() {
            self.public_reader.store(self.force_public, std::sync::atomic::Ordering::SeqCst);
            return Ok(keys::PLAIN.to_string());
        }
        if let Some(epoch) = &self.epoch {
            self.public_reader.store(self.epoch_public, std::sync::atomic::Ordering::SeqCst);
            return Ok(epoch.identity.clone());
        }
        self.public_reader.store(false, std::sync::atomic::Ordering::SeqCst);
        if !self.force_public || self.layer.is_some() {
            match self.own_identity(descriptor, paper) {
                Ok(identity) => {
                    mark_read(&self.dir)?;
                    return Ok(identity);
                }
                Err(error) => {
                    if self.layer.is_some() {
                        return Err(error);
                    }
                    if let Some(identity) = public::identity(&self.backend, descriptor)? {
                        self.public_reader.store(true, std::sync::atomic::Ordering::SeqCst);
                        return Ok(identity);
                    }
                    return Err(error);
                }
            }
        }
        let identity = public::identity(&self.backend, descriptor)?.context("the store is not public")?;
        self.public_reader.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(identity)
    }

    pub fn is_public_reader(&self) -> bool {
        self.public_reader.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn require_private_access(&self) -> Result<()> {
        if self.force_public || self.public_reader.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("public store access is read only; a trusted writer slot is required to write");
        }
        Ok(())
    }

    fn own_identity(&self, descriptor: &Descriptor, paper: Option<&str>) -> Result<String> {
        if let Some(layer) = &self.layer {
            return layer.identity.clone().context("this machine has no overlay slot");
        }
        let machine = format!("slots/machine-{}.age", self.machine);
        if let Some(slot) = self.backend.get(&machine)? {
            let identity = keys::open_machine_slot(&slot, &self.key)?;
            if keys::recipient_of(&identity)? != descriptor.recipient {
                bail!("machine slot does not match store recipient");
            }
            if let Backend::Gateway(gateway) = &self.backend {
                gateway.set_identity(&identity)?;
            }
            return Ok(identity);
        }
        if let Some(person) = &self.person {
            let identity = person.open(&self.backend, descriptor)?;
            self.backend.unlock_reader(&identity)?;
            return Ok(identity);
        }
        let paper = paper.map(str::to_string).or_else(|| self.paper_key_text.clone());
        if let Some(paper) = paper {
            let opener = keys::identity_from_paper(&paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
            let slot = self.backend.get("slots/paper.age")?.context("remote has no paper slot")?;
            let identity = keys::open_slot(&slot, &opener)?;
            if keys::recipient_of(&identity)? != descriptor.recipient {
                bail!("paper slot does not match store recipient");
            }
            if let Backend::Gateway(gateway) = &self.backend {
                gateway.set_identity(&identity)?;
            }
            return Ok(identity);
        }
        bail!("this machine has no remote slot; set LAYR_PAPER_KEY_FILE or use layr remote trust")
    }

    fn private_dir(&self) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir)?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    // Fetch/clone share one orchestration body, preserving their distinct preconditions and final selections.
    fn fetch_into(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options, clone: bool) -> Result<Report> {
        if clone {
            self.force_public = opts.public;
        } else if !repo.p.id.is_empty() {
            repo.require(ctx, if self.public_clone { "read" } else { "sync" })?;
            if !self.public_clone {
                repo.require(ctx, "records")?;
            }
        }
        self.connect(ctx)?;
        if clone && !repo.p.id.is_empty() {
            bail!("clone needs an empty project");
        }
        let mut report = reader::fetch(self, ctx, repo, opts)?;
        if self.is_api() {
            self.private_dir()?;
            fetch_refs(self, repo, &mut report)?;
            overlays::api_fetch(self, ctx, repo, opts, &mut report)?;
        } else {
            overlays::fetch(self, ctx, repo, opts, &mut report)?;
        }
        let pending = self.dir.join("pending-lines.json");
        let (only, lines) = if clone {
            let lines = report.clone_lines.take().unwrap_or(repo.p.db.lines()?);
            let only = if let Some(name) = &opts.only_line {
                let line =
                    lines.iter().find(|line| line.name == *name).with_context(|| format!("line '{name}' is not in the store"))?;
                Some(std::collections::BTreeSet::from([line.id.clone()]))
            } else {
                None
            };
            (only, Some(lines))
        } else if pending.exists() {
            let ids: std::collections::BTreeSet<String> = serde_json::from_slice(&std::fs::read(pending)?)?;
            (Some(ids), report.clone_lines.take())
        } else {
            return Ok(report);
        };
        self.private_dir()?;
        report.missing_lines = fetch::materialize_clone_lines(ctx, repo, only.as_ref(), lines.as_deref(), &self.dir, opts.heads)?;
        Ok(report)
    }

    pub fn verify(&self, files: bool) -> Result<Report> {
        reader::verify(self, files)
    }
    pub fn log(&self) -> Result<Vec<String>> {
        reader::log(self)
    }
    pub fn gc(&self, dry_run: bool) -> Result<Vec<String>> {
        reader::gc(self, dry_run)
    }

    pub fn grant(
        &mut self,
        ctx: &Ctx,
        repo: &Repo,
        login: &str,
        recipient: &str,
        invite_out: Option<&std::path::Path>,
    ) -> Result<bool> {
        use base64::Engine;
        use layr_store::person;
        if !person::login_name(login) {
            bail!("invalid person login; use a tailnet login such as ann@example.com");
        }
        if !keys::valid_recipient(recipient) {
            bail!("the recipient is not an age recipient");
        }
        self.connect(ctx)?;
        let own = match &self.backend {
            Backend::Gateway(g) => crate::person::gateway_login(g)?,
            _ => {
                let own = ctx.env("LAYR_PERSON_LOGIN").context("set LAYR_PERSON_LOGIN for a file:// or s3:// person grant")?;
                if !person::login_name(own) {
                    bail!("invalid LAYR_PERSON_LOGIN");
                }
                own.into()
            }
        };
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        self.require_private_access()?;
        let granted = self.backend.list("slots/")?;
        let first = !granted.iter().any(|slot| slot.key.strip_prefix("slots/").and_then(person::slot_login).is_some());
        if first && own != login {
            bail!("grant yourself first: the first person slot must name your login {own}; nothing is written");
        }
        let slot = format!("slots/{}", person::slot_name(login, recipient)?);
        let existing = granted.iter().any(|object| object.key == slot);
        let anchor: Descriptor =
            serde_json::from_slice(&self.backend.root().get("store.json")?.context("missing root descriptor")?)?;
        let invite = person::seal_invite(recipient, &person::Pin::of_store(&anchor))?;
        // Refuse an occupied output path before publishing a grant.
        if let Some(path) = invite_out {
            crate::privs::as_caller_fs(ctx, || crate::person::create(path, &invite))?;
        }
        let created = !existing && self.backend.put_new(&slot, keys::seal_slot(&identity, recipient)?)?;
        let _lock = repo.p.lock_maintenance()?;
        let mut record = crate::model::DraftRecord::new("project.reader");
        record.data = serde_json::json!({"principal":format!("person:{login}"),"recipient":recipient,"store":anchor.recipient});
        repo.commit_record(&_lock, ctx, record)?;
        if let Backend::Gateway(g) = &self.backend {
            g.person_call(reqwest::Method::POST,&format!("/api/people/{login}/inbox"),None,Some(&serde_json::json!({"data":base64::engine::general_purpose::STANDARD.encode(invite)})))
                .context("the person slot is in place, but the invite did not reach the inbox; register the person key and run grant again")?;
        }
        Ok(created)
    }

    pub fn trust(&self, repo: &Repo, machine: &str, public_key: Option<&str>) -> Result<()> {
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        self.require_private_access()?;
        let id = if crate::model::is_uuid(machine) {
            machine.to_string()
        } else {
            repo.p.db.machine_by_name(machine)?.context("unknown machine")?
        };
        let first_key = repo.p.db.machine_key(&id)?;
        let public = public_key
            .map(str::to_string)
            .or_else(|| first_key.clone())
            .context("machine has no key; give --public-key <key>")?;
        if first_key.as_ref().is_some_and(|first| first != &public) {
            bail!("the public key differs from the machine's first record");
        }
        if repo.p.db.trusted(&id)?.as_ref().is_some_and(|trusted| trusted != &public) {
            bail!("the public key differs from the trusted machine key");
        }
        let bytes: [u8; 32] = hex::decode(public)?.try_into().map_err(|_| anyhow::anyhow!("invalid machine key"))?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&bytes)?;
        // A Private or Public store has no vault key: the signed trust record is all a reader needs.
        if !descriptor.mode.is_secret() {
            return Ok(());
        }
        let slot = keys::machine_slot(&identity, &key)?;
        let name = format!("slots/machine-{id}.age");
        // The signed trust path binds this machine id to one key. A retry keeps its slot.
        self.backend.put_new(&name, slot)?;
        Ok(())
    }
}

impl<'a> RemoteSync<Ctx, Repo<'a>, Options> for Store {
    type Report = Report;
    fn push(&mut self, ctx: &Ctx, repo: &mut Repo<'a>, opts: &Options) -> Result<Report> {
        repo.require(ctx, "sync")?;
        repo.require(ctx, "records")?;
        let stage = crate::trace::span("store push access");
        self.connect(ctx)?;
        if repo.p.is_overlay() {
            bail!("private overlay projects cannot be pushed to a store");
        }
        // Recheck the descriptor pin before discovery or any write-related request.
        let descriptor = self.descriptor()?;
        // Check before the writer can publish any object, even when the caller is local root.
        let public_manifest = self.backend.get("public.json")?;
        let public = public_manifest.is_some();
        if let Some(bytes) = public_manifest {
            let manifest = layr_store::public::Manifest::parse(&bytes, &descriptor)?;
            let identity = self.identity(&descriptor, opts.paper_key.as_deref())?;
            self.require_private_access()?;
            // The segments that an earlier fetch or push read and checked are not read again (`seen-records`): a
            // runner that pushes after each check reads only the new segments, not the whole log each time.
            let local: Vec<String> = repo.p.export_jsonl()?.lines().map(str::to_string).collect();
            let records = reader::records_with_local(
                &self.backend,
                &identity,
                &descriptor.project,
                &local,
                Some((&reader::seen_file(&self.dir), &self.key)),
            )?;
            manifest.verify_signer(&layr_store::public::trusted_writers_checked(&descriptor.project, &records.lines)?)?;
        } else if self.force_public {
            self.require_private_access()?;
        }
        // Private gateway listings require the machine read proof, including before
        // the writer's first discovery request when there is no public manifest.
        if let Backend::Gateway(gateway) = &self.backend {
            if !gateway.has_read_identity() {
                self.identity(&descriptor, opts.paper_key.as_deref())?;
            }
        }
        let mode = if public {
            layr_store::epochs::Mode::Public
        } else if self.backend.get("slots/service.age")?.is_some() {
            layr_store::epochs::Mode::Service
        } else {
            layr_store::epochs::Mode::Private
        };
        for name in repo.visible_overlay_names(ctx)? {
            if let Ok(overlay) = repo.overlay_repo(&name) {
                if epochs::overlay_mode(&overlay)? > mode {
                    bail!(
                        "overlay {name} is more open than remote project mode {}; change the remote mode before pushing",
                        mode.name()
                    );
                }
            }
        }
        drop(stage);
        if self.is_api() {
            self.private_dir()?;
            let mut report = writer::push(self, ctx, repo, opts)?;
            push_refs(self, repo, &mut report)?;
            overlays::api_push(self, ctx, repo, opts, &mut report)?;
            return Ok(report);
        }
        let mut report = writer::push(self, ctx, repo, opts)?;
        let _overlays = crate::trace::span("store push overlays");
        overlays::push(self, ctx, repo, opts, &mut report)?;
        Ok(report)
    }

    fn fetch(&mut self, ctx: &Ctx, repo: &mut Repo<'a>, opts: &Options) -> Result<Report> {
        self.fetch_into(ctx, repo, opts, false)
    }
    fn clone_into(&mut self, ctx: &Ctx, repo: &mut Repo<'a>, opts: &Options) -> Result<Report> {
        self.fetch_into(ctx, repo, opts, true)
    }
}

/// The daemon's pass (docs/cache-overlays.md): for each project of the store at `root` and each of its
/// store remotes, delete the dead cache states, so a remote that nobody pushes to loses them too. A
/// remote that cannot be reached now is tried at the next pass.
pub fn sweep_tick(root: &std::path::Path, caller: crate::ctx::Caller) {
    let Ok(names) = crate::store::Store::open(root).and_then(|s| s.project_names()) else { return };
    for project in names.into_iter().filter(|n| !n.contains('%')) {
        let ctx = Ctx {
            store: match crate::store::Store::open(root) {
                Ok(s) => s,
                Err(_) => return,
            },
            caller: caller.clone(),
            cwd: std::path::PathBuf::from("/"),
            env: std::collections::HashMap::new(),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let Ok(repo) = Repo::open(&ctx, &project) else { continue };
        let remotes: Vec<String> = repo
            .p
            .db
            .config_all()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, _)| {
                // A setting of this machine is `<key>@<machine>`.
                let name = k.split('@').next()?.strip_prefix("cloud.remote.")?.strip_suffix(".url")?.to_string();
                let url = crate::remote_config::remote_url(&repo, &name).ok()?;
                ["file://", "s3://", "layr+https://"].iter().any(|s| url.starts_with(s)).then_some(name)
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for remote in remotes {
            let swept = open_named(&remote, &repo).and_then(|mut store| {
                store.connect(&ctx)?;
                overlays::sweep_caches(&store, &ctx, &repo, false)
            });
            match swept {
                Ok(list) => {
                    for (overlay, refused) in list.iter().filter_map(|(o, s)| Some((o, s.refused.as_ref()?))) {
                        eprintln!("layr daemon: {project} remote {remote} overlay {overlay}: the remote refused a delete ({refused}); the rest stays");
                    }
                    for (overlay, s) in list.into_iter().filter(|(_, s)| !s.trees.is_empty() || !s.packs.is_empty()) {
                        eprintln!(
                            "layr daemon: {project} remote {remote} overlay {overlay}: deleted {} dead cache state(s), {} bytes; {} dead chunk(s) in {} pack object(s), {} bytes",
                            s.trees.len(),
                            s.bytes,
                            s.chunks,
                            s.packs.len(),
                            s.chunk_bytes
                        );
                    }
                }
                Err(e) => eprintln!("layr daemon: {project} remote {remote}: cache sweep skipped: {e:#}"),
            }
        }
    }
}

/// Validate the gateway URL before a remote is written to project configuration.
pub(crate) fn validate_gateway_url(url: &str) -> Result<()> {
    gateway::Gateway::open(url).map(|_| ())
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use crate::ctx::{Caller, Ctx};

    #[test]
    fn changed_file_descriptor_stops_writer_fetch_clone_and_mount_before_encryption_or_decryption() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = Ctx {
            store: crate::store::Store::open(&root.join("local")).unwrap(),
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.clone(),
            env: Default::default(),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        let dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let project_id = crate::ids::new_id();
        let mut repo = Repo {
            restore_from_remote: true,
            p: crate::store::Project {
                lease: Default::default(),
                store: &ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: project_id.clone(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
            ctx: None,
        };
        let mut record = crate::model::DraftRecord::new("project");
        record.data = serde_json::json!({"id":project_id,"name":"p","config":{}});
        repo.p.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
        let url = format!("file://{}", root.join("remote").display());
        let mut store = open(&url, &repo).unwrap();
        store.configure(&ctx, &ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        crate::project_config::set_config(
            &repo.p.lock_maintenance().unwrap(),
            &ctx,
            &repo,
            "cloud.remote.archive.url",
            serde_json::json!(url),
        )
        .unwrap();
        store.save_pin(&repo.p.lock_maintenance().unwrap(), &ctx, &repo, "archive").unwrap();
        let mut store = open(&url, &repo).unwrap();
        let pin = store.pin.clone().unwrap();
        let attacker = keys::generate_identity();
        let recipient = keys::recipient_of(&attacker).unwrap();
        std::fs::write(
            root.join("remote/store.json"),
            serde_json::to_vec(&Descriptor {
                format: 2,
                project: project_id.clone(),
                recipient: recipient.clone(),
                mode: Default::default(),
            })
            .unwrap(),
        )
        .unwrap();
        // No slot: this machine can only write to the public recipient.
        std::fs::remove_dir_all(root.join("remote/slots")).unwrap();
        let before = store.backend.list("").unwrap().len();
        let error = writer::push(&store, &ctx, &repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        assert_eq!(store.backend.list("").unwrap().len(), before);
        let error = reader::fetch(&mut store, &ctx, &mut repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        // The descriptor must win over the missing slot error in clone and mount too.
        repo.p.id.clear();
        store.project.clear();
        let error = store.clone_into(&ctx, &mut repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        let access = browse::Access { pin: Some(pin.clone()), ..Default::default() };
        let error = browse::Browse::open(&url, &Default::default(), &access).err().unwrap().to_string();
        assert!(error.contains(&pin.recipient) && error.contains(&recipient), "{error}");
        let other_project = crate::ids::new_id();
        std::fs::write(
            root.join("remote/store.json"),
            serde_json::to_vec(&Descriptor {
                format: 2,
                project: other_project.clone(),
                recipient: pin.recipient.clone(),
                mode: Default::default(),
            })
            .unwrap(),
        )
        .unwrap();
        let error = store.descriptor().err().unwrap().to_string();
        assert!(error.contains(&project_id) && error.contains(&other_project), "{error}");
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod mode_pin_tests {
    use super::*;
    use crate::ctx::{Caller, Ctx};

    #[test]
    fn a_pinned_secret_store_rewritten_as_plain_stops_fetch_and_push() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = Ctx {
            store: crate::store::Store::open(&root.join("local")).unwrap(),
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.clone(),
            env: Default::default(),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        let dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let project_id = crate::ids::new_id();
        let mut repo = Repo {
            restore_from_remote: true,
            p: crate::store::Project {
                lease: Default::default(),
                store: &ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: project_id.clone(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
            ctx: None,
        };
        let mut record = crate::model::DraftRecord::new("project");
        record.data = serde_json::json!({"id":project_id,"name":"p","config":{}});
        repo.p.lock_maintenance().unwrap().append(record, &ctx.actor()).unwrap();
        let url = format!("file://{}", root.join("remote").display());
        let mut store = open(&url, &repo).unwrap();
        store.configure(&ctx, &ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        crate::project_config::set_config(
            &repo.p.lock_maintenance().unwrap(),
            &ctx,
            &repo,
            "cloud.remote.archive.url",
            serde_json::json!(url),
        )
        .unwrap();
        store.save_pin(&repo.p.lock_maintenance().unwrap(), &ctx, &repo, "archive").unwrap();
        let mut store = open(&url, &repo).unwrap();
        assert_eq!(store.pin.as_ref().unwrap().mode, Mode::Secret);
        // Someone with write access to the folder rewrites store.json as a plain Private store.
        let plain = Descriptor { format: 2, project: project_id.clone(), recipient: keys::PLAIN.into(), mode: Mode::Private };
        std::fs::write(root.join("remote/store.json"), serde_json::to_vec(&plain).unwrap()).unwrap();
        let error = reader::fetch(&mut store, &ctx, &mut repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains("store mode changed: pinned secret, store.json private"), "{error}");
        let error = writer::push(&store, &ctx, &repo, &Options::default()).unwrap_err().to_string();
        assert!(error.contains("store mode changed"), "{error}");
        // A pin saved before modes existed has no mode field and still means Secret.
        let old: StorePin = serde_json::from_value(serde_json::json!({"project":project_id,"recipient":"age1x"})).unwrap();
        assert_eq!(old.mode, Mode::Secret);
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod restore_tests {
    use super::*;
    use std::cell::RefCell;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn restore_connects_before_it_reads() -> Result<()> {
        let log = RefCell::new(Vec::new());
        restore_from_remotes(
            &(),
            &names(&["archive"]),
            "0123abcd",
            |name| Ok(name.to_string()),
            |store, _ctx| {
                log.borrow_mut().push(format!("connect {store}"));
                Ok(())
            },
            |store| {
                log.borrow_mut().push(format!("read {store}"));
                Ok(())
            },
        )?;
        assert_eq!(*log.borrow(), ["connect archive", "read archive"]);
        Ok(())
    }

    #[test]
    fn a_failed_connect_skips_the_read_and_tries_the_next_remote() -> Result<()> {
        let reads = RefCell::new(Vec::new());
        restore_from_remotes(
            &(),
            &names(&["first", "second"]),
            "0123abcd",
            |name| Ok(name.to_string()),
            |store, _ctx| if store == "first" { bail!("no credentials") } else { Ok(()) },
            |store| {
                reads.borrow_mut().push(store.clone());
                Ok(())
            },
        )?;
        assert_eq!(*reads.borrow(), ["second"]);
        Ok(())
    }

    #[test]
    fn every_failed_remote_is_named_in_the_error() {
        let err = restore_from_remotes(
            &(),
            &names(&["first", "second"]),
            "0123abcd",
            |name| Ok(name.to_string()),
            |store, _ctx| bail!("offline {store}"),
            |_| Ok(()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("first: offline first") && err.contains("second: offline second"), "{err}");
        assert!(err.contains("not on this machine"), "{err}");
    }
}

/// The line refs that this machine last read from Layr Cloud (`refs.json` of the remote folder): the `expect` of
/// the next compare and swap.
fn tracked_refs(dir: &std::path::Path) -> Result<std::collections::BTreeMap<String, String>> {
    match std::fs::read(dir.join("refs.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid refs.json")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_tracked_refs(dir: &std::path::Path, refs: &std::collections::BTreeMap<String, String>) -> Result<()> {
    crate::fsutil::atomic_write(&dir.join("refs.json"), &serde_json::to_vec(refs)?, 0o600)
}

/// The newest record of this machine that moved each line: line id to the record id and the head after it
/// (None for a deleted line).
fn own_line_moves(repo: &Repo, machine: &str) -> Result<std::collections::BTreeMap<String, (String, Option<String>)>> {
    /// The order of a record (time, sequence), its id and the head after it.
    type Move = ((i64, u64), String, Option<String>);
    let mut moves: std::collections::BTreeMap<String, Move> = Default::default();
    for line in repo.p.export_jsonl()?.lines() {
        let record = crate::oplog::parse_line(line)?.record;
        if record.machine != machine {
            continue;
        }
        for (id, change) in &record.lines {
            let order = (record.time, record.seq);
            if moves.get(id).is_none_or(|(newest, ..)| order >= *newest) {
                moves.insert(id.clone(), (order, record.id.clone(), change.after().map(|after| after.head.clone())));
            }
        }
    }
    Ok(moves.into_iter().map(|(id, (_, record, head))| (id, (record, head))).collect())
}

/// Moves the line refs on Layr Cloud to this machine's line heads with one compare and swap
/// (docs/remotes.md, "Records and refs"). A line that another writer moved on the server since this machine
/// read it fails the push: fetch, then push again.
fn push_refs(store: &Store, repo: &Repo, report: &mut Report) -> Result<()> {
    let api = store.backend.api().context("not a Layr Cloud remote")?;
    let scope = store.api_scope();
    let mut tracked = tracked_refs(&store.dir)?;
    let present: std::collections::BTreeSet<String> = repo.p.db.lines()?.into_iter().map(|line| line.id).collect();
    let mut updates = Vec::new();
    for (line, (record, head)) in own_line_moves(repo, &store.machine)? {
        // A line that exists now must point where the database says; the newest record names its head.
        let head = if present.contains(&line) { head } else { None };
        let name = format!("lines/{line}");
        let expect = tracked.get(&name).cloned();
        if expect == head {
            continue;
        }
        // A state that this push left out (an automatic state without --auto) cannot be a ref target.
        if let Some(target) = &head {
            if !report.state_ids.contains(target) && !api.state_held(&scope, target)? {
                report.omitted.push(format!("line {line}: head {} is not on Layr Cloud", crate::ids::short(target)));
                continue;
            }
        }
        // Like a Git push without force: the new head must contain what the server had, or a move of another
        // writer would be lost. The person merges it first.
        if let (Some(expect), Some(target)) = (&expect, &head) {
            if !fetch::state_ancestry(repo, target)?.contains(expect) {
                bail!(
                    "Layr Cloud: {name} is at {} there, and head {} here does not contain it; merge it into the line, then push",
                    crate::ids::short(expect),
                    crate::ids::short(target)
                );
            }
        }
        updates.push(api::RefUpdate { name, expect, target: head, record });
    }
    if updates.is_empty() {
        return Ok(());
    }
    for _ in 0..2 {
        match api.cas(&scope, &updates, &format!("layr push from machine {}", store.machine))? {
            api::Cas::Done(refs) => {
                for update in &updates {
                    match &update.target {
                        Some(target) => tracked.insert(update.name.clone(), target.clone()),
                        None => tracked.remove(&update.name),
                    };
                }
                for reference in refs {
                    match reference.target {
                        Some(target) => tracked.insert(reference.name, target),
                        None => tracked.remove(&reference.name),
                    };
                }
                save_tracked_refs(&store.dir, &tracked)?;
                return Ok(());
            }
            api::Cas::Conflict(conflicts) => {
                // A ref that is already at its target (an earlier push moved it, but this machine did not keep the
                // answer) is no conflict: keep it and send the rest again.
                let (done, moved): (Vec<_>, Vec<_>) = conflicts
                    .into_iter()
                    .partition(|(name, actual)| updates.iter().any(|u| &u.name == name && &u.target == actual));
                if !moved.is_empty() || done.is_empty() {
                    let names: Vec<String> = moved.iter().map(|(name, _)| name.clone()).collect();
                    save_tracked_refs(&store.dir, &tracked)?;
                    bail!(
                        "Layr Cloud: {} moved on the server since this machine read it; run layr fetch, then push again",
                        if names.is_empty() { "a line".to_string() } else { names.join(", ") }
                    );
                }
                for (name, actual) in done {
                    if let Some(target) = actual {
                        tracked.insert(name.clone(), target);
                    }
                    updates.retain(|u| u.name != name);
                }
                if updates.is_empty() {
                    save_tracked_refs(&store.dir, &tracked)?;
                    return Ok(());
                }
            }
        }
    }
    bail!("Layr Cloud: the refs changed again during the push; push again")
}

/// Reads the line refs of Layr Cloud: they are the `expect` of the next push. A line whose head here differs
/// from the server is named in the report: the server ref is the truth for its position.
fn fetch_refs(store: &Store, repo: &Repo, report: &mut Report) -> Result<()> {
    let api = store.backend.api().context("not a Layr Cloud remote")?;
    let refs = api.refs(&store.api_scope(), "lines/")?;
    let mut tracked = std::collections::BTreeMap::new();
    let lines: std::collections::BTreeMap<String, String> =
        repo.p.db.lines()?.into_iter().map(|line| (line.id, line.head)).collect();
    for reference in refs {
        let Some(target) = reference.target else { continue };
        if let Some(line) = reference.name.strip_prefix("lines/") {
            if lines.get(line).is_some_and(|head| head != &target) {
                report.omitted.push(format!(
                    "line {line}: Layr Cloud has {}, the records here {}",
                    crate::ids::short(&target),
                    crate::ids::short(&lines[line])
                ));
            }
        }
        tracked.insert(reference.name, target);
    }
    save_tracked_refs(&store.dir, &tracked)
}

#[cfg(test)]
mod clone_head_tests {
    use super::*;
    use crate::model::{LineChange, LineState};

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    #[ignore = "requires a Btrfs TMPDIR"]
    fn heads_materialization_sync_restores_history_on_demand() -> Result<()> {
        heads_materialization_sync(false, false, false)
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    #[ignore = "requires a Btrfs TMPDIR"]
    fn heads_materialization_fetch_then_sync() -> Result<()> {
        heads_materialization_sync(true, false, false)
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    #[ignore = "requires a Btrfs TMPDIR"]
    fn heads_materialization_unborn_sync() -> Result<()> {
        heads_materialization_sync(false, true, false)
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    #[ignore = "requires a Btrfs TMPDIR"]
    fn heads_materialization_offline_save_then_fetch_and_sync() -> Result<()> {
        heads_materialization_sync(true, false, true)
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    #[test]
    #[ignore = "requires a Btrfs TMPDIR"]
    fn heads_materialization_unborn_offline_save_then_fetch_and_sync() -> Result<()> {
        heads_materialization_sync(true, true, true)
    }

    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    fn heads_materialization_sync(fetch_first: bool, unborn: bool, save_offline: bool) -> Result<()> {
        use serde_json::json;
        use std::process::Command;
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&root)?;
        let ctx = Ctx {
            store: crate::store::Store::open(&root.join("local"))?,
            caller: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.clone(),
            env: [("LAYR_PROJECT".into(), "p".into()), ("LAYR_LINE".into(), "main".into())].into(),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        let p = crate::store::Project::create(
            &ctx.store,
            "p",
            &ctx.actor(),
            json!({"access": {format!("user:{}", ctx.caller.uid): "admin"}}),
        )?;
        let mut repo = Repo { restore_from_remote: true, p, ctx: Some(&ctx) };
        let source = root.join("git-source");
        let git = |dir: &std::path::Path, args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid"])
                .args(args)
                .output()?;
            anyhow::ensure!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        std::fs::create_dir(&source)?;
        git(&source, &["init", "-q"])?;
        std::fs::write(source.join("base-file"), "base")?;
        git(&source, &["add", "."])?;
        git(&source, &["commit", "-qm", "base"])?;
        let commit = git(&source, &["rev-parse", "HEAD"])?;
        git(&source, &["clone", "--bare", "-q", ".", repo.p.git_dir().to_str().unwrap()])?;
        let base = crate::ids::new_id();
        let omitted = crate::ids::new_id();
        let mut head = crate::ids::new_id();
        let line_id = crate::ids::new_id();
        let mut record = crate::model::DraftRecord::new("export");
        for (id, parents) in [(&base, vec![]), (&omitted, if unborn { vec![] } else { vec![&base] }), (&head, vec![&omitted])] {
            record.states.push(serde_json::from_value(json!({
                "id":id,"kind":"commit","parents":parents,"subvol":"",
                "author":{"name":"Test","email":"test@example.invalid"},"time":1700000000000i64
            }))?);
        }
        record.data = json!({"state":base,"commit":commit,"commits":{base.clone():commit}});
        record.lines.insert(
            line_id.clone(),
            LineChange {
                transition: crate::model::LineTransition::Create {
                    after: LineState {
                        name: "main".into(),
                        head: head.clone(),
                        owner: ctx.caller.uid,
                        machine: ctx.store.machine.id.clone(),
                    },
                },
                working_before: None,
                working_after: None,
            },
        );
        repo.p.lock_maintenance()?.append(record, &ctx.actor())?;
        crate::fs::create(&repo.p.state_path(&head))?;
        std::fs::write(repo.p.state_path(&head).join("head-file"), "head")?;
        crate::fs::set_readonly(&repo.p.state_path(&head))?;
        // Portable endpoints keep export comparisons independent of privileged Btrfs send.
        // The middle state has no objects here and must still come from the archive.
        let objects = crate::objstore::open(&repo.p)?;
        let mut trees = serde_json::Map::new();
        for (state, path) in [(&base, source.clone()), (&head, repo.p.state_path(&head))] {
            let written = layr_objects::write_tree(
                &objects,
                &path,
                &layr_objects::WriteOptions {
                    temperature: layr_objects::Temperature::Hot,
                    previous: None,
                    exclude: vec![b".git".to_vec()],
                    similar: false,
                },
            )?;
            trees.insert(state.clone(), json!({"tree":written.tree.unwrap().hex(),"times":written.times.unwrap().hex()}));
        }
        let mut record = crate::model::DraftRecord::new("state.tree");
        record.data = json!({"states":trees});
        repo.p.lock_maintenance()?.append(record, &ctx.actor())?;
        let url = format!("file://{}", root.join("offline-remote").display());
        let remote = open(&url, &repo)?;
        crate::cmd::admin::set_config(&repo.p.lock_maintenance()?, &ctx, &repo, "cloud.remote.archive.url", json!(&url))?;
        remote.private_dir()?;
        std::fs::write(states_left_file(&remote.dir), format!("{base}\n{omitted}\n"))?;
        let line = repo.p.db.line_by_id(&line_id)?.unwrap();
        REMOTE_STATE_RESTORES.with(|count| count.set(0));
        assert!(fetch::materialize_clone_lines(&ctx, &mut repo, None, Some(std::slice::from_ref(&line)), &remote.dir, true)?
            .is_empty());
        assert_eq!(REMOTE_STATE_RESTORES.with(|count| count.get()), 0, "Git must not try remote restoration");
        assert!(!repo.p.has_state(&omitted));
        assert!(repo.working(&line).join("head-file").is_file());
        let fallback = if unborn { "ref: refs/heads/main" } else { &commit };
        assert_eq!(std::fs::read_to_string(repo.working(&line).join(".git/HEAD"))?.trim(), fallback);
        assert!(repo.restore_from_remote, "normal reads must be restored after materialization");
        assert!(repo.state_src(&omitted).is_err(), "the remote is offline");
        assert!(REMOTE_STATE_RESTORES.with(|count| count.get()) > 0, "an explicit history read still tries restoration");
        // Publish history to a local encrypted archive, then discard the temporary source trees.
        // The command below must use its normal preflight and restore the missing history itself.
        for state in [&base, &omitted] {
            std::fs::create_dir(repo.p.state_path(state))?;
            std::fs::write(repo.p.state_path(state).join("base-file"), "base")?;
            repo.p.lock_maintenance()?.write(|tx| tx.set_present(state, true))?;
        }
        let mut remote = open(&url, &repo)?;
        remote.configure(&ctx, &ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })?;
        writer::push(&remote, &ctx, &repo, &Options::default())?;
        for state in [&base, &omitted] {
            std::fs::remove_dir_all(repo.p.state_path(state))?;
            repo.p.lock_maintenance()?.write(|tx| tx.set_present(state, false))?;
        }
        if save_offline {
            // Match the CLI reproduction: the archive is unavailable while save moves the head.
            let archive = root.join("offline-remote");
            let unavailable = root.join("unavailable-remote");
            std::fs::rename(&archive, &unavailable)?;
            let result =
                crate::cmd::admin::save(&ctx, &["--allow-empty".into(), "-m".into(), "offline save".into(), "-q".into()]);
            std::fs::rename(&unavailable, &archive)?;
            assert_eq!(result?, 0);
            let moved = repo.p.db.line_by_id(&line_id)?.unwrap();
            assert_ne!(moved.head, head, "save must persist a new head");
            head = moved.head;
            assert!(!repo.p.has_state(&omitted), "save must succeed without restoring history");
            // The original line state must no longer authorize carrying this marker forward.
            crate::gitbridge::advance_fallback_head(&repo, &line, &omitted)?;
            assert_eq!(
                std::fs::read_to_string(repo.working(&line).join(".git/layr-fallback"))?,
                format!("{head}\n{fallback}\n"),
                "failed refresh must carry the verified fallback to the saved head"
            );
        }
        let line = repo.p.db.line_by_id(&line_id)?.unwrap();
        if save_offline {
            let archive = root.join("offline-remote");
            let unavailable = root.join("unavailable-remote");
            std::fs::rename(&archive, &unavailable)?;
            crate::fsutil::git_writable(&repo.working(&line), true);
            git(&repo.working(&line), &["commit", "--allow-empty", "-qm", "external after offline save"])?;
            let marker = std::fs::read(repo.working(&line).join(".git/layr-fallback"))?;
            let error =
                crate::cmd::admin::save(&ctx, &["--allow-empty".into(), "-m".into(), "rejected save".into(), "-q".into()])
                    .unwrap_err();
            assert!(error.to_string().contains("A git commit, merge, reset or checkout ran in this line"), "{error:#}");
            assert_eq!(repo.p.db.line_by_id(&line_id)?.unwrap().head, head);
            // Even the failed-refresh hook must not bless a HEAD changed outside Layr.
            repo.after_head_moved(&line, &omitted);
            assert_eq!(std::fs::read(repo.working(&line).join(".git/layr-fallback"))?, marker);
            std::fs::rename(&unavailable, &archive)?;
            assert_eq!(crate::cmd::remote::fetch(&ctx, &["archive".into(), "--overlay".into(), "main".into(), "-q".into()])?, 0);
            let error = crate::cmd::remote::git::sync(&ctx, &["-q".into()]).unwrap_err();
            assert!(error.to_string().contains("A git commit, merge, reset or checkout ran in this line"), "{error:#}");
            if unborn {
                git(&repo.working(&line), &["update-ref", "-d", "refs/heads/main"])?;
            } else {
                git(&repo.working(&line), &["reset", "--soft", &commit])?;
            }
            crate::fsutil::git_writable(&repo.working(&line), false);
        }
        if fetch_first {
            assert_eq!(crate::cmd::remote::fetch(&ctx, &["archive".into(), "--overlay".into(), "main".into(), "-q".into()])?, 0);
            assert!(repo.p.has_state(&omitted));
        }
        assert_eq!(
            std::fs::read_to_string(repo.working(&line).join(".git/HEAD"))?.trim(),
            fallback,
            "fetch leaves the existing HEAD alone"
        );
        if fetch_first || unborn {
            // An external commit is rejected while the fallback marker still exists.
            // For an unborn branch Git leaves the symbolic HEAD text unchanged.
            crate::fsutil::git_writable(&repo.working(&line), true);
            git(&repo.working(&line), &["commit", "--allow-empty", "-qm", "external"])?;
            let error = crate::cmd::remote::git::sync(&ctx, &["-q".into()]).unwrap_err();
            assert!(error.to_string().contains("A git commit, merge, reset or checkout ran in this line"), "{error:#}");
            assert!(repo.head_record(&ctx, "reset", &line, &line.head, vec![], None, json!({})).is_err());
            if unborn {
                git(&repo.working(&line), &["update-ref", "-d", "refs/heads/main"])?;
            } else {
                git(&repo.working(&line), &["reset", "--soft", &commit])?;
            }
            crate::fsutil::git_writable(&repo.working(&line), false);
        }
        // The same preflight protects later head moves, even before Git history can be exported.
        let _ = repo.head_record(&ctx, "reset", &line, &line.head, vec![], None, json!({}))?;
        REMOTE_STATE_RESTORES.with(|count| count.set(0));
        assert_eq!(crate::cmd::remote::git::sync(&ctx, &["-q".into()])?, 0);
        assert!(repo.p.has_state(&omitted));
        assert_eq!(REMOTE_STATE_RESTORES.with(|count| count.get()) > 0, !fetch_first);
        let real_head = git(&repo.working(&line), &["rev-parse", "HEAD"])?;
        assert_ne!(real_head, commit);
        assert_eq!(Some(real_head), repo.p.db.git_commit_of(&head)?);
        assert_eq!(git(&repo.working(&line), &["show", "HEAD:head-file"])?, "head");
        assert!(!repo.working(&line).join(".git/layr-fallback").exists());
        // A real external reset to the former fallback must now fail the complete command preflight.
        crate::fsutil::git_writable(&repo.working(&line), true);
        git(&repo.working(&line), &["reset", "--soft", &commit])?;
        let error = crate::cmd::remote::git::sync(&ctx, &["-q".into()]).unwrap_err();
        assert!(error.to_string().contains("A git commit, merge, reset or checkout ran in this line"), "{error:#}");
        assert_eq!(git(&repo.working(&line), &["rev-parse", "HEAD"])?, commit);
        crate::fsutil::git_writable(&repo.working(&line), true);
        crate::free::finish(&ctx);
        drop(repo);
        let mut directories = vec![root.clone()];
        while let Some(path) = directories.pop() {
            if crate::fs::host().namespace.managed(&path) {
                crate::fs::set_writable(&path)?;
            }
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    directories.push(entry.path());
                }
            }
        }
        if let Err(error) = crate::fs::remove(&root) {
            // Some Btrfs mounts forbid unprivileged subvolume deletion.
            eprintln!("fixture cleanup {}: {error:#}", root.display());
        }
        Ok(())
    }
    #[test]
    fn clone_uses_the_creator_head_after_a_later_foreign_adoption() -> Result<()> {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root)?;
        let store = crate::store::Store::open(&root.join("local"))?;
        let dir = store.project_dir("p")?;
        std::fs::create_dir_all(dir.join("meta/db"))?;
        let project = crate::ids::new_id();
        let db = crate::db::Db::open(&dir.join("meta/db/layr.sqlite"))?;
        let creator = crate::ids::new_id();
        let peer = crate::ids::new_id();
        let line = crate::ids::new_id();
        let old = crate::ids::new_id();
        let new = crate::ids::new_id();
        let key = ed25519_dalek::SigningKey::from_bytes(&[4; 32]);
        let change = |head: &str, machine: &str| LineChange {
            transition: crate::model::LineTransition::Create {
                after: LineState { name: "main".into(), head: head.into(), owner: 501, machine: machine.into() },
            },
            working_before: None,
            working_after: None,
        };
        let mut creation = crate::model::Record::untrusted_preview("project");
        creation.project = project.clone();
        creation.machine = creator.clone();
        creation.seq = 2;
        creation.time = 1;
        creation.data = serde_json::json!({"id":project,"name":"p","config":{}});
        creation.lines.insert(line.clone(), change(&old, &creator));
        let mut update = crate::model::Record::untrusted_preview("commit");
        update.project = project.clone();
        update.machine = creator.clone();
        update.seq = 3;
        update.time = 2;
        update.lines.insert(line.clone(), change(&new, &creator));
        let mut adoption = crate::model::Record::untrusted_preview("line.adopt");
        adoption.project = project.clone();
        adoption.machine = peer.clone();
        adoption.seq = 2;
        adoption.time = 3;
        adoption.lines.insert(line.clone(), change(&old, &peer));
        for machine in [&creator, &peer] {
            let mut announcement = crate::model::Record::untrusted_preview("machine");
            announcement.id = crate::ids::new_id();
            announcement.project = project.clone();
            announcement.machine = machine.clone();
            announcement.seq = 1;
            announcement.data = serde_json::json!({"public_key":hex::encode(key.verifying_key().to_bytes())});
            db.test_insert_record(&announcement, &crate::oplog::sign_line(&announcement, &key)?)?;
            db.test_derive(&announcement, &|_| false, &store.machine.id)?;
        }
        for mut record in [creation, update, adoption] {
            record.id = crate::ids::new_id();
            db.test_insert_record(&record, &crate::oplog::sign_line(&record, &key)?)?;
            db.test_derive(&record, &|_| false, &store.machine.id)?;
        }
        let mut repo = Repo {
            restore_from_remote: true,
            p: crate::store::Project { lease: Default::default(), store: &store, name: "p".into(), dir, id: project, db },
            ctx: None,
        };
        assert_eq!(repo.p.db.line_by_id(&line)?.unwrap().head, old, "the foreign row reproduces the stale selection");
        let remote = open(&format!("file://{}", root.join("remote").display()), &repo)?;
        let selected = reader::clone_lines(&remote, &repo)?;
        assert_eq!(selected.iter().find(|row| row.id == line).unwrap().head, new, "the clone selects the authoritative head");
        assert!(repo.p.db.line_heads(&line)?.contains(&(peer, old)), "the foreign head remains available");
        let caller = crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw());
        let local_head = crate::ids::new_id();
        let mut own = crate::model::Record::untrusted_preview("line.adopt");
        own.id = crate::ids::new_id();
        own.project = repo.p.id.clone();
        own.machine = store.machine.id.clone();
        own.seq = 1;
        own.time = 4;
        let mut own_change = change(&local_head, &store.machine.id);
        (match &mut own_change.transition {
            crate::model::LineTransition::Move { after, .. } | crate::model::LineTransition::Create { after } => after,
            _ => panic!("fixture needs after"),
        })
        .owner = caller.uid;
        own.lines.insert(line.clone(), own_change);
        repo.p.db.test_insert_record(&own, &crate::oplog::sign_line(&own, &store.machine.key)?)?;
        repo.p.db.test_derive(&own, &|_| false, &store.machine.id)?;
        let local = repo.p.db.line_by_id(&line)?.unwrap();
        let working = repo.working(&local);
        std::fs::create_dir_all(&working)?;
        std::fs::write(working.join("keep"), "local work")?;
        let ctx = crate::ctx::Ctx {
            store: crate::store::Store::open(&root.join("local"))?,
            caller,
            cwd: root.clone(),
            env: Default::default(),
            author: Default::default(),
            captures: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        remote.private_dir()?;
        assert!(fetch::materialize_clone_lines(&ctx, &mut repo, None, Some(&selected), &remote.dir, false)?.is_empty());
        assert_eq!(repo.p.db.line_by_id(&line)?.unwrap().head, local_head);
        assert_eq!(std::fs::read_to_string(working.join("keep"))?, "local work");
        drop(repo);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn legacy_clone_keeps_a_valid_configured_merge_after_private_adoption() -> Result<()> {
        use ed25519_dalek::SigningKey;
        use serde_json::{json, Value};
        const PROJECT: &str = "01a1c000-0000-7000-8000-000000000003";
        const OWNER: &str = "01a1c000-0000-7000-8000-000000000001";
        const MERGER: &str = "01a1c000-0000-7000-8000-000000000002";
        const BASE: &str = "01a1c000-0000-7000-8000-000000000004";
        const MERGED: &str = "01a1c000-0000-7000-8000-000000000005";
        const LINE: &str = "01a1c000-0000-7000-8000-000000000006";
        const QUEUE: &str = "01a1c000-0000-7000-8000-000000000007";
        let signed = |machine: &str,
                      seq: u64,
                      time: i64,
                      op: &str,
                      data: Value,
                      lines: Value,
                      states: Value,
                      key: &SigningKey|
         -> Result<String> {
            let record: crate::model::Record = serde_json::from_value(json!({
                "id":crate::ids::new_id(),"project":PROJECT,"machine":machine,"seq":seq,"time":time,"op":op,
                "actor":{"uid":1,"name":"fixture"},"data":data,"lines":lines,"states":states,
            }))?;
            crate::oplog::sign_line(&record, key)
        };
        let state = |id: &str, parents: Vec<&str>| json!({"id":id,"kind":"commit","parents":parents,"subvol":"","author":{"name":"fixture","email":"fixture@example.invalid"},"time":if id==BASE {2} else {5}});
        let head = |state: &str, machine: &str| json!({"name":"main","head":state,"owner":1,"machine":machine});
        for role in [None, Some("merge")] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root)?;
            let store = crate::store::Store::open(&root.join("local"))?;
            let dir = store.project_dir("p")?;
            std::fs::create_dir_all(dir.join("meta/db"))?;
            let mut repo = Repo {
                restore_from_remote: true,
                p: crate::store::Project {
                    lease: Default::default(),
                    store: &store,
                    name: "p".into(),
                    dir: dir.clone(),
                    id: PROJECT.into(),
                    db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite"))?,
                },
                ctx: None,
            };
            let owner = SigningKey::from_bytes(&[7; 32]);
            let merger = SigningKey::from_bytes(&[8; 32]);
            let mut trust = json!({"machine":MERGER,"public_key":hex::encode(merger.verifying_key().to_bytes())});
            if let Some(role) = role {
                trust["role"] = json!(role);
            }
            let pending =
                json!({"id":QUEUE,"into":"main","target_line":LINE,"state":MERGED,"added_by":1,"time":4,"status":"pending"});
            let mut done = pending.clone();
            done["status"] = json!("done");
            done["result"] = json!(MERGED);
            let records = vec![
                signed(
                    OWNER,
                    1,
                    1,
                    "machine",
                    json!({"public_key":hex::encode(owner.verifying_key().to_bytes())}),
                    json!({}),
                    json!([]),
                    &owner,
                )?,
                signed(
                    MERGER,
                    1,
                    1,
                    "machine",
                    json!({"public_key":hex::encode(merger.verifying_key().to_bytes())}),
                    json!({}),
                    json!([]),
                    &merger,
                )?,
                signed(
                    OWNER,
                    2,
                    2,
                    "project",
                    json!({"id":PROJECT,"name":"p","config":{"queue.merge-machine":MERGER}}),
                    json!({LINE:{"before":null,"after":head(BASE,OWNER)}}),
                    json!([state(BASE, vec![])]),
                    &owner,
                )?,
                signed(OWNER, 3, 3, "machine.trust", trust, json!({}), json!([]), &owner)?,
                signed(
                    OWNER,
                    4,
                    4,
                    "queue.add",
                    json!({"queue_change":{"id":QUEUE,"before":null,"after":pending}}),
                    json!({}),
                    json!([]),
                    &owner,
                )?,
                signed(
                    MERGER,
                    2,
                    5,
                    "queue.result",
                    json!({"queue_change":{"id":QUEUE,"before":pending,"after":done}}),
                    json!({LINE:{"before":head(BASE,OWNER),"after":head(MERGED,OWNER)}}),
                    json!([state(MERGED, vec![BASE])]),
                    &merger,
                )?,
                signed(
                    MERGER,
                    3,
                    6,
                    "line.adopt",
                    json!({"line":LINE}),
                    json!({LINE:{"before":head(MERGED,OWNER),"after":head(BASE,MERGER)}}),
                    json!([]),
                    &merger,
                )?,
            ];
            repo.p.lock_maintenance().unwrap().import_lines(&records, crate::store::Trust::All)?;
            let remote = open(&format!("file://{}", root.join("remote").display()), &repo)?;
            let lines = reader::clone_lines(&remote, &repo)?;
            assert_eq!(lines.iter().find(|line| line.id == LINE).unwrap().head, MERGED, "role {role:?}");
            drop(repo);
            std::fs::remove_dir_all(root)?;
        }
        Ok(())
    }
}
