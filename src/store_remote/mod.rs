//! Encrypted immutable remotes, independent of the local snapshot backend.

pub(crate) mod api;
#[cfg(test)]
mod api_tests;
pub(crate) mod backend;
pub(crate) mod browse;
pub(crate) mod chunked;
mod cloud_objects;
pub(crate) mod epochs;
pub(crate) mod gateway;
mod gitpack;
mod index;
pub(crate) mod overlays;
mod public;
mod push_cache;
mod push_journal;
pub(crate) mod reader;
mod release;
pub(crate) mod sweep;
mod writer;

pub(crate) use layr_store::{canonical, format, keys, s3, zst};

use crate::ctx::Ctx;
use crate::repo::Repo;
use anyhow::{bail, Context, Result};
use backend::Backend;
use format::{Descriptor, Mode};
use layr_hashers::{ContentAddress, Digest};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    /// `fetch --overlay <name>`: only these layers; `base` names the base. None: every layer.
    pub overlays: Option<std::collections::BTreeSet<String>>,
    /// The states of one overlay that `lines` need, set by the overlay fetch (None: every state).
    pub(crate) only_states: Option<std::collections::BTreeSet<String>>,
    /// A plain fetch of one cache overlay, set by the overlay fetch: no cache state comes, and the fetch names the
    /// count of the states that stay on the remote (`takes_cache_states`).
    pub(crate) plain_cache_fetch: bool,
}

/// The name of the base layer in `fetch --overlay`.
pub const BASE_LAYER: &str = "base";

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
    let url = crate::cmd::remote::remote_url(repo, name)?;
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
        if data["result"] != "success" || !matches!(crate::cmd::remote::kind(url), Ok(crate::cmd::remote::Kind::Store)) {
            continue;
        }
        if crate::cmd::remote::remote_url(repo, name).ok().as_deref() != Some(url) || !store_dir(repo, url).join("read").is_file()
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
        installed += gitpack::fetch(&store, repo, &identity)?;
    }
    Ok(installed)
}

/// The states that a `--heads` clone or fetch left on a store remote (`states-left` in its local folder).
pub(super) fn states_left_file(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("states-left")
}

pub fn restore_state(repo: &Repo, id: &str) -> Result<()> {
    let mut names = store_copies(repo)?.remove(id).unwrap_or_default();
    // A `--heads` clone or fetch left the state on the remote that it read.
    for (key, _) in repo.p.db.config_all()? {
        let Some(name) = key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")) else { continue };
        let Ok(url) = crate::cmd::remote::remote_url(repo, name) else { continue };
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

fn fetch_caches_inner(
    ctx: &Ctx,
    repo: &Repo,
    start: &str,
    source: Option<&crate::model::Line>,
    quiet: bool,
) -> Result<()> {
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
            Some(line) => overlays::caches_of_lines_for(repo, &orepo, std::slice::from_ref(&line.name), Some(ctx.caller.uid))?,
            None => overlays::caches_from_state(repo, &orepo, start, ctx.caller.uid)?,
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
        let url = crate::cmd::remote::remote_url(repo, &name)?;
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

/// The cache states of the overlay `orepo` of `repo` that a store remote of the project holds, for eviction
/// (`gc.evict on`, docs/cache-overlays.md, "Eviction"): a successful push record of the overlay, from this machine
/// or from the machine that made the state, names the state and a URL that a store remote of the project has now,
/// and this machine has read that store with its own key (`mark_read`). The overlay keeps no remotes of its own: the
/// URLs come from the project. A renamed remote with the same URL still counts; a changed or removed one does not.
pub fn cache_store_copies(repo: &Repo, orepo: &Repo) -> Result<std::collections::BTreeSet<String>> {
    let mut urls = std::collections::BTreeSet::new();
    for name in store_remote_names(repo)? {
        let url = crate::cmd::remote::remote_url(repo, &name)?;
        if store_dir(repo, &url).join("read").is_file() {
            urls.insert(layr_model::operations::public_url(&url));
            urls.insert(url);
        }
    }
    let mut held = std::collections::BTreeSet::new();
    if urls.is_empty() {
        return Ok(held);
    }
    for rec in orepo.p.db.records_with_op("push")? {
        let data = &rec.data;
        if data["result"] != "success" || !data["url"].as_str().is_some_and(|url| urls.contains(url)) {
            continue;
        }
        held.extend(data["state_ids_sent"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_owned));
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
            crate::cmd::remote::remote_url(repo, name)
                .is_ok_and(|url| matches!(crate::cmd::remote::kind(&url), Ok(crate::cmd::remote::Kind::Store)))
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
        crate::thin::Layered::of_state(&repo.p, id)?.layers.into_iter().map(|layer| layer.root).collect();
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
    pub fn write_index(&mut self, ctx: &Ctx, repo: &Repo) -> Result<index::Report> {
        self.connect(ctx)?;
        repo.require(ctx, "sync")?;
        // No project lock: the index only reads the records and writes create-only objects of the remote (heads take
        // the next free number), and it can take minutes on a large store.
        index::write(self, repo)
    }
    fn connect(&mut self, ctx: &Ctx) -> Result<()> {
        self.connect_transport(ctx)?;
        epochs::connect(self)
    }

    fn connect_transport(&mut self, ctx: &Ctx) -> Result<()> {
        self.backend = self.backend.root().clone();
        self.epoch = None;
        self.epoch_public = false;
        self.backend.as_caller(ctx);
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
            let info = api.project_info()?;
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
            self.backend = Backend::open(&self.url)?;
            self.backend.as_caller(ctx);
        }
        if let Backend::Gateway(gateway) = &mut self.backend {
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
        self.connect(ctx)?;
        let descriptor = self.descriptor()?;
        self.identity(&descriptor, self.paper_key_text.as_deref())?;
        Ok(&self.backend)
    }

    /// Current project vault recipient, pinned by the same descriptor as sync.
    pub fn request_recipient(&mut self, ctx: &Ctx) -> Result<String> {
        self.connect(ctx)?;
        Ok(self.descriptor()?.recipient)
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
    pub fn save_pin(&self, ctx: &Ctx, repo: &Repo, name: &str) -> Result<()> {
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
                        crate::cmd::admin::set_config(
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
                crate::cmd::admin::set_config(ctx, repo, &key, value)?;
            }
        }
        Ok(())
    }

    pub fn trust_store(&mut self, ctx: &Ctx, repo: &Repo, name: &str, recipient: &str) -> Result<()> {
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
        self.save_pin(ctx, repo, name)
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

    pub fn push(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
        repo.require(ctx, "sync")?;
        repo.require(ctx, "records")?;
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
            let records =
                reader::records_with_local(&self.backend, &identity, &descriptor.project, &local, Some(&reader::seen_file(&self.dir)))?;
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
        if self.is_api() {
            self.private_dir()?;
            let mut report = writer::push(self, ctx, repo, opts)?;
            push_refs(self, repo, &mut report)?;
            overlays::api_push(self, ctx, repo, opts, &mut report)?;
            return Ok(report);
        }
        let mut report = writer::push(self, ctx, repo, opts)?;
        overlays::push(self, ctx, repo, opts, &mut report)?;
        Ok(report)
    }

    pub fn fetch(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
        if !repo.p.id.is_empty() {
            repo.require(ctx, if self.public_clone { "read" } else { "sync" })?;
            if !self.public_clone {
                repo.require(ctx, "records")?;
            }
        }
        self.connect(ctx)?;
        let mut report = reader::fetch(self, ctx, repo, opts)?;
        if self.is_api() {
            self.private_dir()?;
            fetch_refs(self, repo, &mut report)?;
            overlays::api_fetch(self, ctx, repo, opts, &mut report)?;
        } else {
            overlays::fetch(self, ctx, repo, opts, &mut report)?;
        }
        let pending = self.dir.join("pending-lines.json");
        if pending.exists() {
            let ids: std::collections::BTreeSet<String> = serde_json::from_slice(&std::fs::read(pending)?)?;
            let lines = report.clone_lines.take();
            report.missing_lines = self.materialize_clone_lines(ctx, repo, Some(&ids), lines.as_deref())?;
        }
        Ok(report)
    }

    pub fn clone_into(&mut self, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
        self.force_public = opts.public;
        self.connect(ctx)?;
        if !repo.p.id.is_empty() {
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
        let lines = report.clone_lines.take().unwrap_or(repo.p.db.lines()?);
        let only = if let Some(name) = &opts.only_line {
            let line = lines.iter().find(|line| line.name == *name).with_context(|| format!("line '{name}' is not in the store"))?;
            Some(std::collections::BTreeSet::from([line.id.clone()]))
        } else {
            None
        };
        report.missing_lines = self.materialize_clone_lines(ctx, repo, only.as_ref(), Some(&lines))?;
        Ok(report)
    }

    fn materialize_clone_lines(
        &self,
        ctx: &Ctx,
        repo: &mut Repo,
        only: Option<&std::collections::BTreeSet<String>>,
        source_lines: Option<&[crate::model::Line]>,
    ) -> Result<Vec<String>> {
        self.private_dir()?;
        // The working folders are written with no project lock: each under its line's lock, with the
        // head pinned. Only the adopt record is written under the project lock, after the folder.
        let lines = match source_lines {
            Some(lines) => lines.to_vec(),
            None => repo.p.db.lines()?,
        };
        let mut pending = Vec::new();
        let mut missing = Vec::new();
        for line in lines {
            if only.is_some_and(|ids| !ids.contains(&line.id)) {
                continue;
            }
            let local = crate::model::Line { machine: repo.p.store.machine.id.clone(), owner: ctx.caller.uid, ..line.clone() };
            if repo.working(&local).exists() {
                continue;
            }
            if !repo.p.has_state(&line.head) {
                pending.push(line.id.clone());
                missing.push(format!(
                    "line {}: head state {} is not in the store yet; run `layr fetch <remote>` later",
                    line.name, line.head
                ));
                continue;
            }
            repo.hold(repo.pin(&[line.head.as_str()])?);
            let mut created = false;
            {
                let _line = repo.line_lock(&local)?;
                if !repo.working(&local).exists() {
                    repo.materialize(&local, &local.head, &repo.working(&local))?;
                    created = true;
                }
            }
            if created {
                let _lock = repo.p.lock()?;
                let mut record = crate::model::Record::new("line.adopt");
                record.lines.insert(
                    local.id.clone(),
                    crate::model::LineChange {
                        before: repo.p.db.line_by_id(&line.id)?.map(|before| repo.line_state(&before, &before.head)),
                        after: Some(repo.line_state(&local, &local.head)),
                        working_before: None,
                        working_after: None,
                    },
                );
                let shown = repo.readable_layers(&local)?;
                record.data = serde_json::json!({
                    "folder_view": repo.public_view_data(&local.id, &shown)?,
                });
                repo.commit_record(ctx, record)?;
            }
        }
        crate::fsutil::atomic_write(&self.dir.join("pending-lines.json"), &serde_json::to_vec(&pending)?, 0o600)?;
        Ok(missing)
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
        let _lock = repo.p.lock()?;
        let mut record = crate::model::Record::new("project.reader");
        record.data = serde_json::json!({"principal":format!("person:{login}"),"recipient":recipient,"store":anchor.recipient});
        repo.commit_record(ctx, record)?;
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

/// The CLI commands use the same named-remote configuration as push and fetch.
pub fn command(ctx: &Ctx, operation: &str, args: &[String]) -> Result<i32> {
    let args = &crate::cmd::overlay_args(operation, args, &[("--caches", "--overlays")])?;
    let spec = crate::args::Spec::new()
        .value("--remote")
        .flag("--files")
        .flag("--json")
        .flag("-n|--dry-run")
        .flag("-q|--quiet")
        .flag("--caches");
    let a = crate::args::parse(&spec, args)?;
    let name = a.get("--remote").context("name a remote with --remote <name>")?;
    let h = crate::cmd::here(ctx)?;
    h.repo.require(ctx, if operation == "gc" { "gc" } else { "fsck" })?;
    let mut store = open_named(name, &h.repo)?;
    store.connect(ctx)?;
    match operation {
        "fsck" if a.has("--caches") => {
            // Read only: what a sweep would delete (`sweep`).
            let (mut trees, mut bytes) = (0, 0);
            for (overlay, swept) in overlays::sweep_caches(&store, ctx, &h.repo, true)? {
                outln!(
                    ctx,
                    "remote {name} overlay {overlay}: {} unreachable cache state(s), {} part(s), {} bytes; {} unreachable chunk(s) in {} pack object(s), {} bytes; {} tree(s) with missing chunks; {} state(s) not known here kept",
                    swept.trees.len(),
                    swept.parts.len(),
                    swept.bytes,
                    swept.chunks,
                    swept.packs.len(),
                    swept.chunk_bytes,
                    swept.missing,
                    swept.unknown
                );
                trees += swept.trees.len() + swept.parts.len() + swept.packs.len() + swept.missing;
                bytes += swept.bytes + swept.chunk_bytes;
            }
            outln!(ctx, "remote {name}: unreachable cache data: {trees} object(s), {bytes} bytes");
            if trees > 0 {
                return Ok(1);
            }
        }
        "fsck" => {
            let report = store.verify(a.has("--files"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&report)?);
            } else {
                outln!(
                    ctx,
                    "remote {name}: {} records, {} states, {} verified files, {} overlaps",
                    report.records,
                    report.states,
                    report.files,
                    report.overlaps
                );
                for gap in &report.gaps {
                    outln!(ctx, "gap: {gap}");
                }
            }
            if !report.gaps.is_empty() {
                return Ok(1);
            }
        }
        "log" => {
            for line in store.log()? {
                if a.has("--json") {
                    outln!(ctx, "{line}");
                } else {
                    let record = crate::oplog::parse_line(&line, None)?.record;
                    for state in record.states {
                        outln!(ctx, "{} {} {}", crate::ids::short(&state.id), state.time, state.message);
                    }
                }
            }
        }
        "gc" => {
            for (overlay, swept) in overlays::sweep_caches(&store, ctx, &h.repo, a.has("-n"))? {
                if let Some(refused) = &swept.refused {
                    eprintln!("warning: overlay {overlay}: the remote refused a delete ({refused}); the rest stays");
                }
                if !a.has("-q") && (!swept.trees.is_empty() || !swept.packs.is_empty()) {
                    let verb = if a.has("-n") { "would delete" } else { "deleted" };
                    outln!(
                        ctx,
                        "overlay {overlay}: {verb} {} dead cache state(s), {} bytes; {} dead chunk(s) in {} pack object(s), {} bytes; {} dead chunk(s) wait for a later pass",
                        swept.trees.len(),
                        swept.bytes,
                        swept.chunks,
                        swept.packs.len(),
                        swept.chunk_bytes,
                        swept.chunks_waiting
                    );
                }
            }
            let deleted = store.gc(a.has("-n"))?;
            if a.has("--json") {
                outln!(ctx, "{}", serde_json::to_string(&deleted)?);
            } else if !a.has("-q") {
                outln!(ctx, "{} {} orphan remote parts", if a.has("-n") { "would delete" } else { "deleted" }, deleted.len());
            }
        }
        _ => bail!("unknown remote store operation"),
    }
    Ok(0)
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
                let url = crate::cmd::remote::remote_url(&repo, &name).ok()?;
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
    use crate::model::Record;

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
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        let dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let project_id = crate::ids::new_id();
        let mut repo = Repo {
            p: crate::store::Project {
                store: &ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: project_id.clone(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
            ctx: None,
        };
        let mut record = Record::new("project");
        record.data = serde_json::json!({"id":project_id,"name":"p","config":{}});
        repo.p.append(record, &ctx.actor()).unwrap();
        let url = format!("file://{}", root.join("remote").display());
        let mut store = open(&url, &repo).unwrap();
        store.configure(&ctx, &ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        crate::cmd::admin::set_config(&ctx, &repo, "cloud.remote.archive.url", serde_json::json!(url)).unwrap();
        store.save_pin(&ctx, &repo, "archive").unwrap();
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
    use crate::model::Record;

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
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        let dir = ctx.store.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let project_id = crate::ids::new_id();
        let mut repo = Repo {
            p: crate::store::Project {
                store: &ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: project_id.clone(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
            ctx: None,
        };
        let mut record = Record::new("project");
        record.data = serde_json::json!({"id":project_id,"name":"p","config":{}});
        repo.p.append(record, &ctx.actor()).unwrap();
        let url = format!("file://{}", root.join("remote").display());
        let mut store = open(&url, &repo).unwrap();
        store.configure(&ctx, &ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        crate::cmd::admin::set_config(&ctx, &repo, "cloud.remote.archive.url", serde_json::json!(url)).unwrap();
        store.save_pin(&ctx, &repo, "archive").unwrap();
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
        let record = crate::oplog::parse_line(line, None)?.record;
        if record.machine != machine {
            continue;
        }
        for (id, change) in &record.lines {
            let order = (record.time, record.seq);
            if moves.get(id).is_none_or(|(newest, ..)| order >= *newest) {
                moves.insert(id.clone(), (order, record.id.clone(), change.after.as_ref().map(|after| after.head.clone())));
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
            if !reader::state_ancestry(repo, target)?.contains(expect) {
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
    use crate::model::{LineChange, LineState, Record};

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
            before: None,
            after: Some(LineState { name: "main".into(), head: head.into(), owner: 501, machine: machine.into() }),
            working_before: None,
            working_after: None,
        };
        let mut creation = Record::new("project");
        creation.project = project.clone();
        creation.machine = creator.clone();
        creation.seq = 1;
        creation.time = 1;
        creation.data = serde_json::json!({"id":project,"name":"p","config":{}});
        creation.lines.insert(line.clone(), change(&old, &creator));
        let mut update = Record::new("commit");
        update.project = project.clone();
        update.machine = creator.clone();
        update.seq = 2;
        update.time = 2;
        update.lines.insert(line.clone(), change(&new, &creator));
        let mut adoption = Record::new("line.adopt");
        adoption.project = project.clone();
        adoption.machine = peer.clone();
        adoption.seq = 1;
        adoption.time = 3;
        adoption.lines.insert(line.clone(), change(&old, &peer));
        for mut record in [creation, update, adoption] {
            record.id = crate::ids::new_id();
            db.insert_record(&record, &crate::oplog::sign_line(&record, &key)?)?;
            db.derive(&record, &|_| false, &store.machine.id)?;
        }
        let mut repo = Repo { p: crate::store::Project { store: &store, name: "p".into(), dir, id: project, db }, ctx: None };
        assert_eq!(repo.p.db.line_by_id(&line)?.unwrap().head, old, "the foreign row reproduces the stale selection");
        let remote = open(&format!("file://{}", root.join("remote").display()), &repo)?;
        let selected = reader::clone_lines(&remote, &repo)?;
        assert_eq!(selected.iter().find(|row| row.id == line).unwrap().head, new, "the clone selects the authoritative head");
        assert!(repo.p.db.line_heads(&line)?.contains(&(peer, old)), "the foreign head remains available");
        let caller = crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw());
        let local_head = crate::ids::new_id();
        let mut own = Record::new("line.adopt");
        own.id = crate::ids::new_id();
        own.project = repo.p.id.clone();
        own.machine = store.machine.id.clone();
        own.seq = 1;
        own.time = 4;
        let mut own_change = change(&local_head, &store.machine.id);
        own_change.after.as_mut().unwrap().owner = caller.uid;
        own.lines.insert(line.clone(), own_change);
        repo.p.db.insert_record(&own, &crate::oplog::sign_line(&own, &store.machine.key)?)?;
        repo.p.db.derive(&own, &|_| false, &store.machine.id)?;
        let local = repo.p.db.line_by_id(&line)?.unwrap();
        let working = repo.working(&local);
        std::fs::create_dir_all(&working)?;
        std::fs::write(working.join("keep"), "local work")?;
        let ctx = crate::ctx::Ctx {
            store: crate::store::Store::open(&root.join("local"))?, caller,
            cwd: root.clone(), env: Default::default(), author: Default::default(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        };
        assert!(remote.materialize_clone_lines(&ctx, &mut repo, None, Some(&selected))?.is_empty());
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
        let signed = |machine: &str, seq: u64, time: i64, op: &str, data: Value, lines: Value, states: Value, key: &SigningKey| -> Result<String> {
            let record: Record = serde_json::from_value(json!({
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
            let mut repo = Repo { p: crate::store::Project { store: &store, name:"p".into(), dir:dir.clone(), id:PROJECT.into(), db:crate::db::Db::open(&dir.join("meta/db/layr.sqlite"))? }, ctx:None };
            let owner = SigningKey::from_bytes(&[7;32]);
            let merger = SigningKey::from_bytes(&[8;32]);
            let mut trust = json!({"machine":MERGER,"public_key":hex::encode(merger.verifying_key().to_bytes())});
            if let Some(role) = role { trust["role"] = json!(role); }
            let pending = json!({"id":QUEUE,"into":"main","target_line":LINE,"state":MERGED,"added_by":1,"time":4,"status":"pending"});
            let mut done = pending.clone();
            done["status"] = json!("done"); done["result"] = json!(MERGED);
            let records = vec![
                signed(OWNER,1,1,"machine",json!({"public_key":hex::encode(owner.verifying_key().to_bytes())}),json!({}),json!([]),&owner)?,
                signed(MERGER,1,1,"machine",json!({"public_key":hex::encode(merger.verifying_key().to_bytes())}),json!({}),json!([]),&merger)?,
                signed(OWNER,2,2,"project",json!({"id":PROJECT,"name":"p","config":{"queue.merge-machine":MERGER}}),json!({LINE:{"before":null,"after":head(BASE,OWNER)}}),json!([state(BASE,vec![])]),&owner)?,
                signed(OWNER,3,3,"machine.trust",trust,json!({}),json!([]),&owner)?,
                signed(OWNER,4,4,"queue.add",json!({"queue_change":{"id":QUEUE,"before":null,"after":pending}}),json!({}),json!([]),&owner)?,
                signed(MERGER,2,5,"queue.result",json!({"queue_change":{"id":QUEUE,"before":pending,"after":done}}),json!({LINE:{"before":head(BASE,OWNER),"after":head(MERGED,OWNER)}}),json!([state(MERGED,vec![BASE])]),&merger)?,
                signed(MERGER,3,6,"line.adopt",json!({"line":LINE}),json!({LINE:{"before":head(MERGED,OWNER),"after":head(BASE,MERGER)}}),json!([]),&merger)?,
            ];
            repo.p.import_lines(&records, crate::store::Trust::All)?;
            let remote = open(&format!("file://{}",root.join("remote").display()), &repo)?;
            let lines = reader::clone_lines(&remote, &repo)?;
            assert_eq!(lines.iter().find(|line| line.id==LINE).unwrap().head, MERGED, "role {role:?}");
            drop(repo);
            std::fs::remove_dir_all(root)?;
        }
        Ok(())
    }

}
