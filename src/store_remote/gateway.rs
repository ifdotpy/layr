//! HTTPS object transport. Read proofs and write grants exist only in memory.

use super::{
    backend::{Object, ObjectRead},
    gitpack::is_commit_list,
    keys,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    Method,
};
use serde::Deserialize;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) const OBJECT_LIMIT: usize = 32 << 20;
/// The request header by which a client says that it follows a signed CDN URL, and the type of that answer
/// (layr-gateway `cdn.rs`).
const CDN_CAPABILITY: &str = "x-layr-cdn";
const CDN_CONTENT_TYPE: &str = "application/vnd.layr.cdn+json";
/// The most keys of one `POST batch`, and the most bytes of its reply (32 MiB of bodies and the index).
pub(super) const BATCH_KEYS: usize = 256;
const BATCH_LIMIT: usize = (32 << 20) + (1 << 20);

enum BatchObject {
    Read(ObjectRead),
    Overflow,
    Direct,
}

struct BatchReply {
    items: Vec<BatchObject>,
    trailer_error: Option<anyhow::Error>,
}

impl From<Vec<BatchObject>> for BatchReply {
    fn from(items: Vec<BatchObject>) -> Self {
        Self { items, trailer_error: None }
    }
}

/// An error from a batch route. The status is kept separately from its diagnostic.
struct RouteFailure {
    status: Option<u64>,
    error: anyhow::Error,
}

impl RouteFailure {
    fn http(route: &str, status: u16) -> Self {
        Self { status: Some(u64::from(status)), error: anyhow!("gateway {route}: HTTP {status}") }
    }
}

impl From<anyhow::Error> for RouteFailure {
    fn from(error: anyhow::Error) -> Self {
        Self { status: None, error }
    }
}

fn transient_read_status(status: u64) -> bool {
    matches!(status, 408 | 429 | 502 | 503 | 504)
}

fn is_manifest(key: &str) -> bool {
    let key = epoch_path(key).map_or(key, |(_, inner)| inner);
    let key = key.strip_prefix("overlays/").and_then(|path| path.split_once('/')).map_or(key, |(_, inner)| {
        inner.strip_prefix("epochs/").and_then(|path| path.split_once('/')).map_or(inner, |(_, key)| key)
    });
    is_commit_list(key)
}

// Git pack consumers handle manifest failures in listing order. Other consumers stop the transfer.
fn manifest_read(key: &str, body: ObjectRead) -> Result<ObjectRead> {
    if is_manifest(key) {
        Ok(body)
    } else {
        body.map(Ok)
    }
}

fn get_status_error(status: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("gateway get: HTTP {status}")
}

fn is_missing_project(error: &str) -> bool {
    error.contains("no such project")
}

fn missing_project_error() -> anyhow::Error {
    anyhow!("gateway: HTTP 404 (no such project) for an open project; the gateway did not load it, try again")
}

/// Both batch routes send transient manifest failures to the authenticated object GET. A permanent failure
/// occupies the item's ordered result slot. Ordinary object reads still propagate their first failure.
fn failed_item(key: &str, status: Option<u64>, error: anyhow::Error) -> Result<BatchObject> {
    let error = match status.filter(|_| is_manifest(key)) {
        Some(status) if transient_read_status(status) => return Ok(BatchObject::Direct),
        Some(status) => get_status_error(status),
        None => error,
    };
    Ok(BatchObject::Read(manifest_read(key, Err(error))?))
}

fn failed_request(keys: &[(usize, &str)], failure: RouteFailure) -> Result<Vec<BatchObject>> {
    if keys.iter().any(|(_, key)| !is_manifest(key)) {
        return Err(failure.error);
    }
    let error: Arc<dyn std::error::Error + Send + Sync> = failure.error.into_boxed_dyn_error().into();
    keys.iter().map(|(_, key)| failed_item(key, failure.status, anyhow::Error::new(error.clone()))).collect()
}

/// Decode one batch envelope and retain ordered Git manifest item errors.
fn batch_objects(response: Response, keys: &[(usize, &str)], counters: Arc<super::download::Counters>) -> Result<BatchReply> {
    let count = keys.len();
    if response.content_length().is_some_and(|n| n > BATCH_LIMIT as u64) {
        bail!("batch response is too large");
    }
    let mut reader = super::download::Counted { inner: response, counters, payload: false, upload: false };
    let mut header = [0; 4];
    reader.read_exact(&mut header).context("short batch reply")?;
    let length = u32::from_be_bytes(header) as usize;
    if length > 1 << 20 {
        bail!("batch index exceeds 1 MiB");
    }
    let mut index = vec![0; length];
    reader.read_exact(&mut index).context("short batch index")?;
    let index: serde_json::Value = serde_json::from_slice(&index)?;
    let items = index["objects"].as_array().context("batch index has no objects")?;
    if items.len() != count {
        bail!("batch reply names {} objects for {count} keys", items.len());
    }
    reader.payload = true;
    let mut total = 0;
    let mut out = Vec::with_capacity(count);
    for (item, (_, key)) in items.iter().zip(keys) {
        let object = match item["status"].as_u64() {
            Some(200) => match batch_body(&mut reader, item, OBJECT_LIMIT - total) {
                Ok(body) => {
                    total += body.len();
                    BatchObject::Read(Ok(Some(body)))
                }
                Err(error) => {
                    let (completed, unread) = keys.split_at(out.len());
                    if completed.iter().any(|(_, key)| !is_manifest(key)) {
                        return Err(error);
                    }
                    out.extend(failed_request(unread, error.into())?);
                    return Ok(out.into());
                }
            },
            Some(404) if item["error"].as_str().is_some_and(is_missing_project) => {
                let error = if is_manifest(key) { missing_project_error() } else { anyhow!("gateway batch: no such project") };
                failed_item(key, None, error)?
            }
            Some(404) => BatchObject::Read(Ok(None)),
            Some(501 | 507) => BatchObject::Overflow,
            status => failed_item(key, status, anyhow!("gateway batch object: HTTP {status:?}"))?,
        };
        out.push(object);
    }
    let error = match reader.read(&mut [0]) {
        Ok(0) => return Ok(out.into()),
        Ok(_) => anyhow!("batch reply has bytes after its bodies"),
        Err(error) => error.into(),
    };
    if keys.iter().any(|(_, key)| !is_manifest(key)) {
        return Err(error);
    }
    Ok(BatchReply { items: out, trailer_error: Some(error) })
}
fn batch_body(reader: &mut impl Read, item: &serde_json::Value, remaining: usize) -> Result<Vec<u8>> {
    let n = usize::try_from(item["length"].as_u64().context("batch entry has no length")?)?;
    if n > remaining {
        bail!("batch bodies exceed 32 MiB");
    }
    let mut body = vec![0; n];
    reader.read_exact(&mut body).context("short batch body")?;
    Ok(body)
}

const SLOTS_LIMIT: usize = 8 << 20;
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
/// A cached read proof is renewed when it expires within this time: a long push or a CI clock that runs behind the
/// gateway by some seconds must not send a proof that the gateway sees as expired.
const PROOF_RENEW_MS: i64 = 60_000;

fn valid_token_expiry(expires: i64, now: i64, lifetime: i64) -> bool {
    expires > now && expires <= now + lifetime + CLOCK_SKEW_MS
}

struct PersonSession {
    login: String,
    key: ed25519_dalek::SigningKey,
    token: Option<SessionToken>,
}
#[derive(Deserialize)]
struct SessionToken {
    token: String,
    expires: i64,
    login: String,
}

pub struct Gateway {
    http: Client,
    pub(super) deadline: Option<Instant>,
    base: reqwest::Url,
    token: Option<String>,
    machine: Option<String>,
    identity: Mutex<Option<String>>,
    epoch: Mutex<Option<String>>,
    read: Mutex<Option<(String, i64)>>,
    public: std::sync::atomic::AtomicBool,
    /// The CDN failed once in this command: later reads go through the gateway.
    cdn_off: Arc<std::sync::atomic::AtomicBool>,
    /// The gateway has no `POST batch` (an older one): later reads go one by one.
    batch_off: Arc<std::sync::atomic::AtomicBool>,
    person: Arc<Mutex<Option<PersonSession>>>,
    counters: Arc<super::download::Counters>,
    cdn_batch_off: Arc<std::sync::atomic::AtomicBool>,
    batch_ready: Arc<std::sync::atomic::AtomicBool>,
    batch_probe: Arc<Mutex<()>>,
    pub(super) listing_cache_dir: Option<std::path::PathBuf>,
}

impl Clone for Gateway {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            deadline: self.deadline,
            base: self.base.clone(),
            token: self.token.clone(),
            machine: self.machine.clone(),
            identity: Mutex::new(self.identity.lock().expect("gateway identity lock").clone()),
            epoch: Mutex::new(self.epoch.lock().expect("gateway epoch lock").clone()),
            read: Mutex::new(self.read.lock().expect("gateway read lock").clone()),
            public: std::sync::atomic::AtomicBool::new(self.is_public()),
            cdn_off: self.cdn_off.clone(),
            batch_off: self.batch_off.clone(),
            person: self.person.clone(),
            counters: self.counters.clone(),
            cdn_batch_off: self.cdn_batch_off.clone(),
            batch_ready: self.batch_ready.clone(),
            batch_probe: self.batch_probe.clone(),
            listing_cache_dir: self.listing_cache_dir.clone(),
        }
    }
}

/// Tries of a request that the gateway refuses as busy (503 or 429) before the error goes to the caller.
const BUSY_TRIES: u32 = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(150);

thread_local! {
    /// Set by `quick`: a fetch on demand that a command waits for (`fetch_caches_on_demand`).
    static QUICK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with quick connections: a connection that fails is not tried again, and a client made in `f` waits at
/// most 2 s for a connection (`connect_timeout`). A fetch on demand of a cache must not stop `line new` for half a
/// minute while the remote is away: the folder starts cold.
pub(crate) fn quick<T>(f: impl FnOnce() -> T) -> T {
    let before = QUICK.with(|q| q.replace(true));
    let out = f();
    QUICK.with(|q| q.set(before));
    out
}

pub(super) fn is_quick() -> bool {
    QUICK.with(|q| q.get())
}

/// The connect timeout of a new client: `normal`, or at most 2 s inside `quick`.
pub(crate) fn connect_timeout(normal: Duration) -> Duration {
    if is_quick() {
        normal.min(Duration::from_secs(2))
    } else {
        normal
    }
}

/// A request that did not reach the server: no connection, no address for the name, or no answer in time.
/// The message is the one that the caller showed before.
#[derive(Debug)]
pub struct Unreachable(pub String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

/// The error of a request that `send` gave: a request that did not reach the server is `Unreachable`.
pub(crate) fn send_error(what: &str, e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() || e.is_timeout() {
        anyhow::Error::new(Unreachable(format!("{what}: {e}")))
    } else {
        anyhow::anyhow!("{what}: {e}")
    }
}

/// Whether `e` says that the server could not be reached (connection, name or time), not that it answered.
pub fn unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<Unreachable>().is_some()
            || cause.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_connect() || r.is_timeout())
    })
}

pub(super) fn body_read_error(message: String, error: std::io::Error, deadline: Option<Instant>) -> anyhow::Error {
    if deadline.is_some()
        && (error.kind() == std::io::ErrorKind::TimedOut
            || error.get_ref().and_then(|e| e.downcast_ref::<reqwest::Error>()).is_some_and(reqwest::Error::is_timeout))
    {
        std::io::Error::new(std::io::ErrorKind::TimedOut, message).into()
    } else {
        anyhow!(message)
    }
}

fn remaining(deadline: Option<Instant>) -> Result<Option<Duration>> {
    let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    if left == Some(Duration::ZERO) {
        return Err(Unreachable("remote request deadline reached".into()).into());
    }
    Ok(left)
}

pub(super) fn limit_request(request: RequestBuilder, deadline: Option<Instant>, timeout: Duration) -> Result<RequestBuilder> {
    Ok(match remaining(deadline)? {
        Some(left) => request.timeout(left.min(timeout)),
        None => request,
    })
}

/// Sends with `send` again while the gateway answers busy (503, 429): it waits `Retry-After` (at most 30 s) or a
/// doubling backoff from 0.5 s to 8 s, at most `BUSY_TRIES` times. A refused request did nothing on the gateway, so
/// a GET, a conditional PUT and a DELETE may go again. Other answers return at once.
/// `send_busy` for a request that a repeat cannot harm (GET, HEAD, a create-only PUT that answers 412 when the first
/// one landed): it is sent again also on 502 and 504 and when the connection fails, as when the gateway restarts
/// for a release or a project change (about 2 s). Authenticated Git manifest GETs also retry a deadline (408).
pub(super) fn send_again(
    idempotent: bool,
    deadline: Option<Instant>,
    send: impl FnMut() -> Result<Response>,
) -> Result<Response> {
    send_again_with(idempotent, false, deadline, send, |_| {})
}

fn send_again_with(
    idempotent: bool,
    manifest_get: bool,
    deadline: Option<Instant>,
    mut send: impl FnMut() -> Result<Response>,
    mut discard: impl FnMut(Response),
) -> Result<Response> {
    let mut wait = Duration::from_millis(500);
    let mut attempt = 1;
    loop {
        remaining(deadline)?;
        let delay = match send() {
            Ok(response) => {
                let status = response.status().as_u16();
                let again = matches!(status, 429 | 503)
                    || (idempotent && matches!(status, 502 | 504))
                    || (manifest_get && transient_read_status(u64::from(status)));
                if !again || attempt >= BUSY_TRIES {
                    return Ok(response);
                }
                let after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(|s| Duration::from_secs(s.min(30)));
                discard(response);
                after.unwrap_or(wait)
            }
            Err(_) if idempotent && attempt < BUSY_TRIES && !is_quick() => wait,
            Err(e) => return Err(e),
        };
        std::thread::sleep(remaining(deadline)?.map_or(delay, |left| delay.min(left)));
        wait = (wait * 2).min(Duration::from_secs(8));
        attempt += 1;
    }
}

fn ca_file(path: &std::path::Path) -> Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open gateway CA file")?;
    if !file.metadata()?.is_file() {
        bail!("gateway CA must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        bail!("gateway CA file is too large");
    }
    Ok(bytes)
}

impl Gateway {
    pub fn transfer_stats(&self) -> super::download::TransferStats {
        self.counters.snapshot()
    }

    fn retry(&self, idempotent: bool, send: impl FnMut() -> Result<Response>) -> Result<Response> {
        self.retry_with(idempotent, false, send)
    }

    fn retry_with(&self, idempotent: bool, manifest_get: bool, send: impl FnMut() -> Result<Response>) -> Result<Response> {
        send_again_with(idempotent, manifest_get, self.deadline, send, |response| {
            let _ = self.bytes(response, 1 << 20);
        })
    }

    /// Count requests, and buffered request bodies in full, at the attempt boundary. The body stays the buffer
    /// that every try shares: counting must not copy an upload. A streamed body counts as HTTP reads it.
    fn execute(&self, builder: reqwest::blocking::RequestBuilder, upload: bool) -> Result<Response> {
        let request = limit_request(builder, self.deadline, REQUEST_TIMEOUT)?.build().map_err(reqwest::Error::without_url)?;
        if let Some(length) = request.body().and_then(reqwest::blocking::Body::as_bytes).map(<[u8]>::len) {
            let counter = if upload { &self.counters.uploaded } else { &self.counters.control_sent };
            counter.fetch_add(length as u64, Ordering::Relaxed);
        }
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let _attempt = crate::trace::named(|| format!("gateway HTTP {} {}", request.method(), self.trace_route(request.url())));
        self.http.execute(request).map_err(reqwest::Error::without_url).map_err(Into::into)
    }

    // Route names only: never print the host, identifiers, or signed URLs.
    fn trace_route(&self, url: &reqwest::Url) -> &'static str {
        let path = url.path();
        if path.starts_with("/o/") {
            return "CDN body";
        }
        let action = path.strip_prefix(self.base.path()).unwrap_or(path);
        match action {
            "objects" => match url.query_pairs().find(|(key, _)| key == "prefix") {
                Some((_, prefix)) => match epoch_path(&prefix).map_or(prefix.as_ref(), |(_, inner)| inner).split('/').next() {
                    Some("states") => "list states",
                    Some("records") => "list records",
                    Some("git") => "list Git",
                    _ => "list",
                },
                _ => "list",
            },
            "batch" => "batch",
            "cdn" => "CDN URLs",
            "challenge" | "token" => "proof",
            "store.json" | "public.json" => "descriptor",
            _ if action.strip_prefix("objects/").is_some_and(is_manifest) => "Git manifest",
            _ if action.ends_with(".pack.age") => "Git pack",
            _ => "object or metadata",
        }
    }

    fn read_body(&self, response: Response, limit: usize, payload: bool) -> Result<Vec<u8>> {
        let known = response.content_length();
        if known.is_some_and(|n| n > limit as u64) {
            bail!("gateway response is too large");
        }
        // Reserve once: geometric growth could allocate twice the limit the download scheduler charged.
        // The extra byte lets read_to_end detect an oversized body without growing.
        let mut data = Vec::with_capacity(known.map_or(limit, |n| n as usize) + 1);
        super::download::Counted { inner: response, counters: self.counters.clone(), payload, upload: false }
            .take(limit as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| body_read_error("gateway body read failed".into(), e, self.deadline))?;
        if data.len() > limit {
            bail!("gateway response is too large");
        }
        Ok(data)
    }
    fn bytes(&self, response: Response, limit: usize) -> Result<Vec<u8>> {
        self.read_body(response, limit, false)
    }
    fn json<T: serde::de::DeserializeOwned>(&self, response: Response, limit: usize) -> Result<T> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let _ = self.bytes(response, limit);
            bail!("gateway: HTTP {status}");
        }
        serde_json::from_slice(&self.bytes(response, limit)?).map_err(|_| anyhow!("invalid gateway JSON response"))
    }

    fn absent<T>(&self, response: Response) -> Result<Option<T>> {
        let body = self.bytes(response, 4096)?;
        if is_missing_project(&String::from_utf8_lossy(&body)) {
            return Err(missing_project_error());
        }
        Ok(None)
    }

    pub fn open(url: &str) -> Result<Self> {
        let address = url.strip_prefix("layr+https://").context("invalid gateway URL scheme")?;
        let (_, project_text) = address.split_once('/').context("gateway URL needs a project name")?;
        if project_text.is_empty() || !project_text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
            bail!("invalid gateway project path");
        }
        let mut base = reqwest::Url::parse(&format!("https://{address}")).map_err(|_| anyhow!("invalid gateway URL"))?;
        let project = base.path().strip_prefix('/').unwrap_or("");
        if base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !(1..=44).contains(&project.len())
            || !project.as_bytes()[0].is_ascii_alphanumeric()
            || !project.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            bail!("gateway URL needs a host and one project name, without credentials or query");
        }
        let path = format!("/v1/p/{project}/");
        base.set_path(&path);
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout(Duration::from_secs(5)))
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            deadline: None,
            base,
            token: None,
            machine: None,
            identity: Mutex::new(None),
            epoch: Mutex::new(None),
            read: Mutex::new(None),
            public: std::sync::atomic::AtomicBool::new(false),
            cdn_off: Arc::default(),
            batch_off: Arc::default(),
            person: Arc::new(Mutex::new(None)),
            counters: Arc::default(),
            cdn_batch_off: Arc::default(),
            batch_ready: Arc::default(),
            batch_probe: Arc::default(),
            listing_cache_dir: None,
        })
        .and_then(|mut gateway| {
            // Every client of the gateway uses the CA of the process, also one that never connects with a
            // command's environment (the daemon's cache sweep); `connect` sets the command's own after it.
            if let Some(path) = std::env::var_os("LAYR_GATEWAY_CA_FILE").filter(|p| !p.is_empty()) {
                gateway.configure_ca(&ca_file(std::path::Path::new(&path))?)?;
            }
            Ok(gateway)
        })
    }

    pub fn connect(&mut self, ctx: &crate::ctx::Ctx) -> Result<()> {
        self.machine = Some(ctx.store.machine.id.clone());
        self.token = ctx
            .env("LAYR_GATEWAY_TOKEN_FILE")
            .map(|path| crate::privs::as_caller_fs(ctx, || token_file(&ctx.cwd.join(path), ctx.caller.uid)))
            .transpose()?;
        if let Some(path) = ctx.env("LAYR_GATEWAY_CA_FILE") {
            let pem = crate::privs::as_caller_fs(ctx, || ca_file(&ctx.cwd.join(path)))?;
            self.configure_ca(&pem)?;
        }
        crate::privs::as_caller_fs(ctx, || self.connect_person(&ctx.env, &ctx.caller.home, &ctx.cwd, ctx.caller.uid))?;
        Ok(())
    }

    /// A standalone reader never loads a write or GC token.
    pub fn connect_reader(&mut self, env: &std::collections::HashMap<String, String>) -> Result<()> {
        self.connect_public(env)?;
        let uid = rustix::process::getuid().as_raw();
        self.connect_person(env, &crate::ctx::Caller::from_uid(uid).home, &std::env::current_dir()?, uid)?;
        Ok(())
    }

    /// Explicit anonymous access must not open person keys or exchange a session.
    pub fn connect_public(&mut self, env: &std::collections::HashMap<String, String>) -> Result<()> {
        self.token = None;
        self.machine = None;
        *self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))? = None;
        if let Some(path) = env.get("LAYR_GATEWAY_CA_FILE") {
            self.configure_ca(&ca_file(std::path::Path::new(path))?)?;
        }
        Ok(())
    }

    fn connect_person(
        &self,
        env: &std::collections::HashMap<String, String>,
        home: &std::path::Path,
        cwd: &std::path::Path,
        uid: u32,
    ) -> Result<()> {
        if let Some(access) = crate::person::load(env, home, cwd, uid, None)? {
            let key = crate::person::signing_key_at(env, home, uid, &access)?;
            self.set_person(&access.login, key)?;
        }
        Ok(())
    }

    pub fn set_person(&self, login: &str, key: ed25519_dalek::SigningKey) -> Result<()> {
        if !layr_store::person::login_name(login) {
            bail!("invalid person login");
        }
        let mut person = self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?;
        if !person.as_ref().is_some_and(|p| p.login == login && p.key == key) {
            *person = Some(PersonSession { login: login.into(), key, token: None });
        }
        Ok(())
    }

    fn session_token(&self, renew: bool) -> Result<String> {
        let mut person = self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?;
        let person = person.as_mut().context("set LAYR_PERSON_KEY_FILE to sign in to the gateway")?;
        if !renew {
            if let Some(token) = person.token.as_ref().filter(|t| t.expires > crate::ids::now_ms() + 10_000) {
                return Ok(token.token.clone());
            }
        }
        person.token = None;
        let mut url = self.base.join("/api/session/challenge")?;
        url.query_pairs_mut().append_pair("login", &person.login);
        #[derive(Deserialize)]
        struct Challenge {
            challenge: String,
            expires: i64,
        }
        let challenge: Challenge = self.json(self.execute(self.http.get(url), false)?, 4096)?;
        let now = crate::ids::now_ms();
        if !valid_token_expiry(challenge.expires, now, 60_000) {
            bail!("invalid or expired gateway session challenge");
        }
        let signature = layr_store::session::sign(&person.login, &challenge.challenge, &person.key)?;
        let response = self.execute(
            self.http
                .post(self.base.join("/api/session")?)
                .json(&serde_json::json!({"login":person.login,"challenge":challenge.challenge,"signature":signature})),
            false,
        )?;
        let token: SessionToken = self.json(response, 4096)?;
        let now = crate::ids::now_ms();
        if token.login != person.login
            || token.token.is_empty()
            || token.token.len() > 256
            || !token.token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'/' | b'='))
            || !valid_token_expiry(token.expires, now, 43_200_000)
        {
            bail!("invalid gateway person session");
        }
        let result = token.token.clone();
        person.token = Some(token);
        Ok(result)
    }

    fn configure_ca(&mut self, pem: &[u8]) -> Result<()> {
        let certificates = reqwest::Certificate::from_pem_bundle(pem).context("invalid gateway CA file")?;
        if certificates.is_empty() {
            bail!("gateway CA file has no certificates");
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout(Duration::from_secs(5)))
            .timeout(REQUEST_TIMEOUT);
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        self.http = builder.build()?;
        Ok(())
    }

    /// Desk person API on the same authenticated HTTPS origin.
    pub fn person_call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let send = |bearer: &str| -> Result<Response> {
            let mut request = self
                .http
                .request(method.clone(), self.base.join(path)?)
                .header("content-type", "application/json")
                .bearer_auth(bearer);
            if let Some(body) = body {
                request = request.json(body);
            }
            self.retry(false, || self.execute(request.try_clone().context("gateway request cannot be sent again")?, false))
        };
        if let Some(token) = token {
            return self.json(send(token)?, 512 << 10);
        }
        let response = send(&self.session_token(false)?)?;
        if response.status().as_u16() != 401 {
            return self.json(response, 512 << 10);
        }
        // A rejected session has not authorized the operation. Renew once, including after restart.
        let _ = self.bytes(response, 1 << 20);
        self.json(send(&self.session_token(true)?)?, 512 << 10)
    }

    pub fn person_slot_at(&self, project: &str, login: &str, recipient: &str) -> Result<Vec<u8>> {
        if !layr_store::person::project_name(project) {
            bail!("invalid gateway project name");
        }
        let slot = layr_store::person::slot_name(login, recipient)?;
        let url = self.base.join(&format!("/v1/p/{project}/slots/{slot}"))?;
        let response = self.execute(self.http.get(url.clone()).bearer_auth(self.session_token(false)?), false)?;
        let response = if response.status().as_u16() == 401 {
            let _ = self.bytes(response, 1 << 20);
            self.execute(self.http.get(url).bearer_auth(self.session_token(true)?), false)?
        } else {
            response
        };
        if !response.status().is_success() {
            bail!("gateway person slot: HTTP {}", response.status().as_u16());
        }
        self.bytes(response, 4096)
    }

    pub fn person_view(&self) -> Result<serde_json::Value> {
        let record = self.person_call(Method::GET, "/api/person", None, None)?;
        let person = self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?;
        if record["login"].as_str() != person.as_ref().map(|p| p.login.as_str()) {
            bail!("gateway person record differs from the session login");
        }
        Ok(record)
    }

    pub fn set_epoch(&self, epoch: Option<&str>) -> Result<()> {
        let mut current = self.epoch.lock().map_err(|_| anyhow!("gateway epoch lock failed"))?;
        if current.as_deref() != epoch {
            *current = epoch.map(str::to_owned);
            drop(current);
            *self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))? = None;
        }
        Ok(())
    }
    pub fn set_identity(&self, identity: &str) -> Result<()> {
        *self.identity.lock().map_err(|_| anyhow!("gateway identity lock failed"))? = Some(identity.into());
        Ok(())
    }

    fn send(
        &self,
        method: Method,
        action: &str,
        bearer: Option<&str>,
        query: Option<(&str, &str)>,
        data: Option<Vec<u8>>,
    ) -> Result<Response> {
        self.send_query(method, action, bearer, query.as_slice(), data)
    }

    fn send_query(
        &self,
        method: Method,
        action: &str,
        bearer: Option<&str>,
        query: &[(&str, &str)],
        data: Option<Vec<u8>>,
    ) -> Result<Response> {
        self.send_query_headers(method, action, bearer, query, data, &[])
    }

    /// `send_query` with additional headers, retained across retries and session renewal.
    fn send_query_headers(
        &self,
        method: Method,
        action: &str,
        bearer: Option<&str>,
        query: &[(&str, &str)],
        data: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<Response> {
        let mut url = self.base.clone();
        url.set_path(&format!("{}{action}", self.base.path()));
        for (key, value) in query {
            url.query_pairs_mut().append_pair(key, value);
        }
        let person = bearer.is_none() && self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?.is_some();
        // The body moves into the request once; each try and the session renewal share that buffer.
        let mut template = self.http.request(method.clone(), url);
        for (name, value) in headers {
            template = template.header(*name, *value);
        }
        if let Some(data) = data {
            let content_type =
                if matches!(action, "token" | "cdn" | "batch") { "application/json" } else { "application/octet-stream" };
            template = template.header("content-type", content_type).body(data);
        }
        let send = |token: Option<&str>| -> Result<Response> {
            let mut request = template.try_clone().context("gateway request cannot be sent again")?;
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            let idempotent = matches!(method, Method::GET | Method::HEAD | Method::PUT | Method::DELETE)
                || (method == Method::POST && matches!(action, "batch" | "cdn"));
            let manifest_get = method == Method::GET && action.strip_prefix("objects/").is_some_and(is_manifest);
            self.retry_with(idempotent, manifest_get, || {
                self.execute(
                    request.try_clone().context("gateway request cannot be sent again")?,
                    method == Method::PUT && action.starts_with("objects/"),
                )
            })
        };
        if person {
            let response = send(Some(&self.session_token(false)?))?;
            if response.status().as_u16() == 401 {
                let _ = self.bytes(response, 1 << 20);
                return send(Some(&self.session_token(true)?));
            }
            Ok(response)
        } else {
            let bootstrap = self.bootstrap_action(&method, action)
                && (query.is_empty()
                    || query.len() == 1
                        && query.first().is_some_and(|(name, epoch)| {
                            *name == "epoch" && super::format::is_uuid(epoch) && matches!(action, "challenge" | "token")
                        }));
            let response = send(bearer.or_else(|| if bootstrap { self.token.as_deref() } else { None }))?;
            if bootstrap && response.status().as_u16() == 404 && (action != "store.json" || self.token.is_some()) {
                bail!("gateway bootstrap: HTTP 404 (no such project or machine slot); check the project name and a missing or wrong LAYR_GATEWAY_TOKEN_FILE grant");
            }
            Ok(response)
        }
    }

    fn machine_slot(&self) -> Option<String> {
        self.machine.as_ref().map(|id| format!("slots/machine-{id}.age"))
    }

    fn bootstrap_action(&self, method: &Method, action: &str) -> bool {
        let logical = epoch_path(action).map(|(_, key)| key).unwrap_or(action);
        (*method == Method::GET
            && (matches!(action, "store.json" | "challenge") || self.machine_slot().as_deref() == Some(logical)))
            || (*method == Method::POST && action == "token")
    }

    fn proof(&self) -> Result<String> {
        let mut cached = self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))?;
        if let Some((token, _)) = cached.as_ref().filter(|(_, expires)| *expires > crate::ids::now_ms() + PROOF_RENEW_MS) {
            return Ok(token.clone());
        }
        let identity = self
            .identity
            .lock()
            .map_err(|_| anyhow!("gateway identity lock failed"))?
            .clone()
            .context("gateway reads require a machine slot or paper key")?;
        #[derive(Deserialize)]
        struct Challenge {
            challenge: String,
        }
        let epoch = self.epoch.lock().map_err(|_| anyhow!("gateway epoch lock failed"))?.clone();
        let query = epoch.as_deref().map(|e| ("epoch", e));
        let challenge: Challenge = self.json(self.send(Method::GET, "challenge", None, query, None)?, 16 << 10)?;
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(challenge.challenge)
            .map_err(|_| anyhow!("invalid gateway challenge encoding"))?;
        let plain =
            keys::decrypt(&identity, &sealed).map_err(|_| anyhow!("gateway challenge does not match the store identity"))?;
        let proof = String::from_utf8(plain).map_err(|_| anyhow!("invalid gateway challenge"))?;
        #[derive(Deserialize)]
        struct Token {
            token: String,
            expires: i64,
        }
        let token: Token = self.json(
            self.send(Method::POST, "token", None, query, Some(serde_json::to_vec(&serde_json::json!({"challenge":proof}))?))?,
            16 << 10,
        )?;
        let now = crate::ids::now_ms();
        if token.token.len() > 256
            || !valid_token_expiry(token.expires, now, 600_000)
            || !token.token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        {
            bail!("invalid gateway read token");
        }
        *cached = Some((token.token.clone(), token.expires));
        Ok(token.token)
    }

    fn read_request(&self, action: &str, query: Option<(&str, &str)>) -> Result<Response> {
        self.read_request_query(action, query.as_slice(), &[])
    }

    fn read_request_query(&self, action: &str, query: &[(&str, &str)], headers: &[(&str, &str)]) -> Result<Response> {
        self.proof_request(Method::GET, action, query, None, headers)
    }

    /// Sends each request that carries a read proof (reads, listings, slot writes). A 401 means that the gateway
    /// refused the proof: a restart discarded its in-memory HMAC key, or the proof expired (clock skew). The client
    /// then exchanges one new proof and sends the request once again. A second 401 is an error. Other answers, a 404
    /// too, go to the caller at once.
    fn proof_request(
        &self,
        method: Method,
        action: &str,
        query: &[(&str, &str)],
        data: Option<Vec<u8>>,
        headers: &[(&str, &str)],
    ) -> Result<Response> {
        // An object read may come from the CDN: the gateway then answers with a signed URL, not the bytes.
        let cdn =
            method == Method::GET && action.starts_with("objects/") && query.is_empty() && !self.cdn_off.load(Ordering::SeqCst);
        let mut headers = headers.to_vec();
        if cdn {
            headers.push((CDN_CAPABILITY, "1"));
        }
        let token = self.proof().with_context(|| format!("gateway {method} {action}"))?;
        let response = self.send_query_headers(method.clone(), action, Some(&token), query, data.clone(), &headers)?;
        if response.status().as_u16() != 401 {
            return self.follow_cdn(response, method, action, &token, query, data);
        }
        let _ = self.bytes(response, 1 << 20);
        {
            let mut cached = self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))?;
            if cached.as_ref().is_some_and(|(current, _)| current == &token) {
                *cached = None;
            }
        }
        let token = self.proof().with_context(|| format!("gateway {method} {action}: renew the read proof after HTTP 401"))?;
        let response = self.send_query_headers(method.clone(), action, Some(&token), query, data.clone(), &headers)?;
        if response.status().as_u16() != 401 {
            return self.follow_cdn(response, method, action, &token, query, data);
        }
        if response.status().as_u16() == 401 {
            let body = self.bytes(response, 4096).unwrap_or_default();
            return Err(super::download::ProofRejected(format!(
                "gateway {method} {action}: HTTP 401 again with a new read proof ({}); check the clock of this machine and the project grant",
                String::from_utf8_lossy(&body).trim()
            ))
            .into());
        }
        Ok(response)
    }

    /// A gateway answer with a signed CDN URL (`application/vnd.layr.cdn+json`): the object comes from that URL,
    /// with no credentials of this client. The URL must be on the host of the gateway. When the CDN does not
    /// answer 200, the object comes through the gateway, and later reads of this command skip the CDN.
    fn follow_cdn(
        &self,
        response: Response,
        method: Method,
        action: &str,
        token: &str,
        query: &[(&str, &str)],
        data: Option<Vec<u8>>,
    ) -> Result<Response> {
        let is_cdn = response.status().as_u16() == 200
            && response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next().is_some_and(|t| t.trim() == CDN_CONTENT_TYPE));
        if !is_cdn {
            return Ok(response);
        }
        #[derive(Deserialize)]
        struct Signed {
            url: String,
        }
        let signed: Signed = self.json(response, 16 << 10)?;
        let _t = crate::trace::span("gateway object from the CDN");
        let fetched = self.cdn_fetch(&signed.url)?;
        match fetched {
            Some(r) => Ok(r),
            None => {
                let _t = crate::trace::span("gateway object: the CDN failed, read through the gateway");
                self.send_query_headers(method, action, Some(token), query, data, &[])
            }
        }
    }

    /// One long-poll of the project events (`GET /v1/p/<project>/events`, docs/remotes.md, "Wait for a
    /// change"): a read proof when this client has a read identity, else a person session or no token
    /// (a Public project).
    pub fn events(&self, after: Option<&str>, wait: u64) -> Result<serde_json::Value> {
        let wait = wait.min(25).to_string();
        let mut query = vec![("wait", wait.as_str())];
        if let Some(after) = after {
            query.push(("after", after));
        }
        let response = if self.has_read_identity() && !self.is_public() {
            self.proof_request(Method::GET, "events", &query, None, &[])?
        } else {
            self.send_query(Method::GET, "events", None, &query, None)?
        };
        self.json(response, 4 << 20).context("gateway events")
    }

    pub fn prove_reader(&self) -> Result<()> {
        self.proof().map(|_| ())
    }

    pub fn has_read_identity(&self) -> bool {
        self.identity.lock().map(|identity| identity.is_some()).unwrap_or(false)
    }

    pub(super) fn use_public_access(&self) {
        self.public.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_public(&self) -> bool {
        self.public.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn public_request(&self, action: &str, prefix: Option<&str>) -> Result<Response> {
        self.public_request_query(action, prefix.map(|value| ("prefix", value)).as_slice(), &[])
    }

    fn public_request_query(&self, action: &str, query: &[(&str, &str)], headers: &[(&str, &str)]) -> Result<Response> {
        let mut url = self.base.clone();
        let path = self.base.path().strip_prefix("/v1").context("invalid gateway base")?;
        url.set_path(&format!("{path}{action}"));
        for (key, value) in query {
            url.query_pairs_mut().append_pair(key, value);
        }
        // Anonymous routes are GET only and never carry a token or private read proof.
        self.retry(true, || {
            let mut request = self.http.get(url.clone());
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            self.execute(request, false)
        })
    }

    /// A side metadata request must not turn a writer into a public-only reader.
    pub(super) fn public_metadata(&self) -> Result<Option<Vec<u8>>> {
        self.public_object("public.json", false)
    }

    pub(super) fn public_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.public_object(key, true)
    }

    fn public_object(&self, key: &str, reader: bool) -> Result<Option<Vec<u8>>> {
        self.public_object_limited(key, reader, OBJECT_LIMIT)
    }

    fn public_object_limited(&self, key: &str, reader: bool, limit: usize) -> Result<Option<Vec<u8>>> {
        let logical = epoch_path(key).map(|(_, k)| k).unwrap_or(key);
        if !key.starts_with("keys/")
            && !logical.starts_with("history/")
            && !matches!(logical, "store.json" | "public.json" | "slots/public.age")
            && super::format::parse_segment_name(logical).is_none()
            && super::format::parse_tree_name(logical).is_none()
            && super::format::parse_data_name(logical).is_none()
            && !super::format::release_file(logical)
        {
            return Ok(None);
        }
        let response = self.public_request(key, None)?;
        match response.status().as_u16() {
            200 => {
                let body = self.read_body(
                    response,
                    if matches!(logical, "store.json" | "public.json" | "slots/public.age") { 16 << 10 } else { limit },
                    !matches!(logical, "store.json" | "public.json" | "slots/public.age"),
                )?;
                if reader && key == "public.json" {
                    self.public.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(Some(body))
            }
            404 if matches!(logical, "store.json" | "public.json") => Ok(None),
            404 => self.absent(response),
            status => {
                let _ = self.bytes(response, 4096);
                bail!("gateway public object: HTTP {status}");
            }
        }
    }

    #[cfg(test)]
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.get_limited(key, OBJECT_LIMIT)
    }

    pub(super) fn get_limited(&self, key: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        if key == "vault.json" {
            return Ok(None);
        }
        if key == "public.json" {
            return self.public_metadata();
        }
        if epoch_path(key).is_some_and(|(_, k)| k == "public.json") {
            return self.public_object(key, false);
        }
        if self.is_public() {
            return self.public_object_limited(key, true, limit);
        }
        let (slot_key, slot_epoch) = if let Some((epoch, inner)) = epoch_path(key) { (inner, Some(epoch)) } else { (key, None) };
        if slot_key.starts_with("slots/") && self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?.is_none() {
            let response = if self.machine_slot().as_deref() == Some(slot_key) {
                self.send(Method::GET, key, None, None, None)?
            } else {
                self.read_request(&format!("objects/{key}"), None)?
            };
            return match response.status().as_u16() {
                200 => Ok(Some(self.bytes(response, 16 << 10)?)),
                404 => Ok(None),
                status => {
                    let _ = self.bytes(response, 4096);
                    bail!("gateway machine slot: HTTP {status}");
                }
            };
        }
        if slot_key.strip_prefix("slots/").is_some_and(|name| layr_store::person::slot_login(name).is_some()) {
            let response = self.send(Method::GET, key, None, None, None)?;
            return match response.status().as_u16() {
                200 => Ok(Some(self.bytes(response, 4096)?)),
                404 => Ok(None),
                status => {
                    let _ = self.bytes(response, 4096);
                    bail!("gateway person slot: HTTP {status}");
                }
            };
        }
        if slot_key.starts_with("slots/") {
            #[derive(Deserialize)]
            struct Slot {
                name: String,
                data: String,
            }
            #[derive(Deserialize)]
            struct Slots {
                slots: Vec<Slot>,
            }
            let slots: Slots =
                self.json(self.send(Method::GET, "slots", None, slot_epoch.map(|e| ("epoch", e)), None)?, SLOTS_LIMIT)?;
            if slots.slots.len() > 1024 {
                bail!("too many gateway slots");
            }
            let name = slot_key.strip_prefix("slots/").unwrap().strip_suffix(".age").context("invalid slot name")?;
            let mut found = None;
            for slot in slots.slots {
                if slot.name == name {
                    if found.is_some() {
                        bail!("duplicate gateway slot");
                    }
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(slot.data)
                        .map_err(|_| anyhow!("invalid gateway slot encoding"))?;
                    if data.len() > 16 << 10 {
                        bail!("gateway slot is too large");
                    }
                    found = Some(data);
                }
            }
            return Ok(found);
        }
        let response = if key.starts_with("keys/")
            || epoch_path(key).is_some_and(|(_, k)| k == "store.json")
            || (overlay_keys_path(key) && !self.has_read_identity())
        {
            self.send(Method::GET, &format!("objects/{key}"), self.token.as_deref(), None, None)?
        } else if key == "store.json" {
            self.send(Method::GET, "store.json", None, None, None)?
        } else {
            self.read_request(&format!("objects/{key}"), None)?
        };
        if key == "store.json"
            && matches!(response.status().as_u16(), 401 | 403 | 404)
            && self.token.is_none()
            && self.public_get("public.json")?.is_some()
        {
            return self.public_get("store.json");
        }
        match response.status().as_u16() {
            404 if key == "store.json" && self.machine.is_some()
                && self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?.is_none() => bail!("gateway bootstrap: HTTP 404 (no such project); check the project name and a missing or wrong LAYR_GATEWAY_TOKEN_FILE grant"),
            404 if key == "store.json" => Ok(None),
            404 => self.absent(response),
            200 => Ok(Some(self.read_body(response, if key == "store.json" { 16 << 10 } else { limit }, key != "store.json")?)),
            status => { let _ = self.bytes(response, 4096); Err(get_status_error(status)) },
        }
    }

    /// Whether `key` is read with the machine read proof through `objects/<key>` (`get`), so a batch can read it.
    fn batchable(&self, key: &str) -> bool {
        let inner = epoch_path(key).map_or(key, |(_, k)| k);
        !self.is_public()
            && !key.starts_with("keys/")
            && !inner.starts_with("slots/")
            && !matches!(inner, "store.json" | "public.json" | "vault.json")
            && !overlay_keys_path(key)
            && !super::format::release_file(inner)
    }

    pub(super) fn visit_many(
        &self,
        keys: &[String],
        sizes: &[Option<u64>],
        mut visit: impl FnMut(usize, ObjectRead) -> Result<()>,
    ) -> Result<()> {
        for key in keys {
            super::backend::check_key(key)?;
        }
        let oversized = |size: Option<u64>| size.is_some_and(|n| n > OBJECT_LIMIT as u64);
        if sizes.len() != keys.len() || keys.iter().zip(sizes).any(|(key, &size)| oversized(size) && !is_manifest(key)) {
            bail!("invalid download size bounds (maximum 32 MiB per object)");
        }
        // Cap bookkeeping and URL answers too. Completed bodies go straight to the consumer.
        for (window, names) in keys.chunks(BATCH_KEYS * 2).enumerate() {
            let offset = window * BATCH_KEYS * 2;
            let lengths = &sizes[offset..offset + names.len()];
            let mut done = vec![false; names.len()];
            let mut direct = vec![false; names.len()];
            let mut trailer_error = None;
            let mut groups: std::collections::BTreeMap<&str, Vec<(usize, &str)>> = Default::default();
            for (i, key) in names.iter().enumerate() {
                if oversized(lengths[i]) {
                    done[i] = true;
                    visit(offset + i, manifest_read(key, Err(anyhow!("{key} exceeds the 32 MiB gateway object limit")))?)?;
                    continue;
                }
                if self.batchable(key) && !self.batch_off.load(Ordering::SeqCst) {
                    let (epoch, inner) = epoch_path(key).unwrap_or(("root", key));
                    groups.entry(epoch).or_default().push((i, inner));
                }
            }
            let batches: Vec<_> =
                groups.iter().flat_map(|(epoch, items)| items.chunks(BATCH_KEYS).map(move |chunk| (*epoch, chunk))).collect();
            super::download::run(
                batches.len(),
                2,
                (BATCH_LIMIT as u64) * 2,
                |_| BATCH_LIMIT as u64,
                |i| {
                    // Serialize only the first capability probe, so an old server gets one refused POST.
                    let probe = if self.batch_ready.load(Ordering::SeqCst) {
                        None
                    } else {
                        Some(self.batch_probe.lock().map_err(|_| anyhow!("gateway batch probe lock failed"))?)
                    };
                    if self.batch_ready.load(Ordering::SeqCst) {
                        drop(probe);
                    }
                    if self.batch_off.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    let (epoch, items) = batches[i];
                    let keys: Vec<_> = items.iter().map(|(_, key)| *key).collect();
                    let body = serde_json::to_vec(&serde_json::json!({"epoch":epoch,"keys":keys}))?;
                    let response = match self.proof_request(Method::POST, "batch", &[], Some(body), &[]) {
                        // A gateway can refuse a route it lacks with 401, before routing, also after an earlier
                        // batch went through (a rolling deploy, a new epoch). Each object read that follows still
                        // authorizes itself, so nothing is granted by falling back.
                        Err(error) if error.is::<super::download::ProofRejected>() => {
                            self.batch_off.store(true, Ordering::SeqCst);
                            return Ok(None);
                        }
                        Ok(response) => response,
                        Err(error) => return Ok(Some(failed_request(items, error.into())?.into())),
                    };
                    match response.status().as_u16() {
                        200 => {
                            self.batch_ready.store(true, Ordering::SeqCst);
                            // Parse directly from the response: no second 32 MiB copy of the envelope.
                            Ok(Some(batch_objects(response, items, self.counters.clone())?))
                        }
                        404 | 405 => {
                            // A project disappearing after restart is not an old route.
                            let body = match self.bytes(response, 4096) {
                                Ok(body) => body,
                                Err(error) => return Ok(Some(failed_request(items, error.into())?.into())),
                            };
                            if String::from_utf8_lossy(&body).contains("no such project") {
                                return Ok(Some(failed_request(items, anyhow!("gateway batch: no such project").into())?.into()));
                            }
                            self.batch_off.store(true, Ordering::SeqCst);
                            Ok(None)
                        }
                        status => {
                            let _ = self.bytes(response, 4096);
                            Ok(Some(failed_request(items, RouteFailure::http("batch", status))?.into()))
                        }
                    }
                },
                |batch, reply| {
                    if let Some(reply) = reply {
                        if trailer_error.is_none() {
                            trailer_error = reply.trailer_error;
                        }
                        for ((i, _), object) in batches[batch].1.iter().zip(reply.items) {
                            match object {
                                BatchObject::Read(body) => {
                                    let body = match body {
                                        Ok(Some(bytes)) if lengths[*i].is_some_and(|bound| bytes.len() as u64 > bound) => {
                                            manifest_read(
                                                &names[*i],
                                                Err(anyhow!("gateway batch: {} exceeds its listed size", names[*i])),
                                            )?
                                        }
                                        body => body,
                                    };
                                    done[*i] = true;
                                    visit(offset + *i, body)?;
                                }
                                BatchObject::Direct => direct[*i] = true,
                                BatchObject::Overflow => {}
                            }
                        }
                    }
                    Ok(())
                },
            )?;
            let mut pending: Vec<_> = (0..names.len()).filter(|&i| !done[i]).collect();
            // Large known bodies and overflow entries share the same whole-object scheduler.
            let urls = if self.batch_off.load(Ordering::SeqCst) && lengths.iter().all(Option::is_none) {
                vec![None; names.len()]
            } else {
                self.cdn_urls(names, pending.iter().copied().filter(|&i| !direct[i]), |i, body| {
                    done[i] = true;
                    visit(offset + i, body)
                })?
            };
            pending.retain(|&i| !done[i]);
            super::download::run(
                pending.len(),
                super::download::JOBS,
                super::download::BYTE_BUDGET,
                |j| lengths[pending[j]].unwrap_or(OBJECT_LIMIT as u64).max(1),
                |j| {
                    let i = pending[j];
                    let read = || {
                        let limit = lengths[i].map_or(Ok(OBJECT_LIMIT), usize::try_from)?;
                        // A CDN failure in an earlier job sends the rest of this window straight to the gateway.
                        if let Some(url) = urls[i].as_deref().filter(|_| !self.cdn_off.load(Ordering::SeqCst)) {
                            if let Some(response) = self.cdn_fetch(url)? {
                                return Ok(Some(self.read_body(response, limit, true)?));
                            }
                        }
                        self.get_limited(&names[i], limit)
                    };
                    manifest_read(&names[i], read())
                },
                |j, body| visit(offset + pending[j], body),
            )?;
            if let Some(error) = trailer_error {
                return Err(error);
            }
        }
        Ok(())
    }

    fn cdn_fetch(&self, signed: &str) -> Result<Option<Response>> {
        let url = reqwest::Url::parse(signed).context("invalid CDN URL from the gateway")?;
        let fetched = if url.scheme() == "https"
            && url.host_str() == self.base.host_str()
            && url.path().starts_with("/o/")
            && url.username().is_empty()
            && url.password().is_none()
        {
            match self.retry(true, || {
                self.execute(self.http.get(url.clone()).header(reqwest::header::ACCEPT_ENCODING, "identity"), false)
            }) {
                Ok(response) if response.status().as_u16() == 200 => Some(response),
                Ok(response) => {
                    let _ = self.bytes(response, 1 << 20);
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };
        if fetched.is_some() {
            self.counters.cdn_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.cdn_fallbacks.fetch_add(1, Ordering::Relaxed);
            self.cdn_off.store(true, Ordering::SeqCst);
        }
        Ok(fetched)
    }

    /// Optional POST cdn returns metadata only. Public access never probes authenticated batch routes.
    fn cdn_urls(
        &self,
        keys: &[String],
        pending: impl IntoIterator<Item = usize>,
        mut finished: impl FnMut(usize, ObjectRead) -> Result<()>,
    ) -> Result<Vec<Option<String>>> {
        let mut urls = vec![None; keys.len()];
        if self.cdn_off.load(Ordering::SeqCst) || self.cdn_batch_off.load(Ordering::SeqCst) || self.is_public() {
            return Ok(urls);
        }
        let mut groups: std::collections::BTreeMap<&str, Vec<(usize, &str)>> = Default::default();
        for i in pending {
            if self.batchable(&keys[i]) {
                let (epoch, inner) = epoch_path(&keys[i]).unwrap_or(("root", &keys[i]));
                groups.entry(epoch).or_default().push((i, inner));
            }
        }
        for (epoch, items) in groups {
            for chunk in items.chunks(BATCH_KEYS) {
                let objects = match self.cdn_objects(epoch, chunk) {
                    Ok(Some(objects)) => objects,
                    Ok(None) => return Ok(urls),
                    Err(failure) => {
                        for ((i, _), item) in chunk.iter().zip(failed_request(chunk, failure)?) {
                            if let BatchObject::Read(body) = item {
                                finished(*i, body)?;
                            }
                        }
                        continue;
                    }
                };
                for ((i, key), object) in chunk.iter().zip(objects) {
                    match object["status"].as_u64() {
                        Some(200) => urls[*i] = object["url"].as_str().map(str::to_owned),
                        Some(404) if is_manifest(key) && object["error"].as_str().is_some_and(is_missing_project) => {
                            finished(*i, Err(missing_project_error()))?;
                        }
                        Some(404 | 501) => {}
                        status => {
                            if let BatchObject::Read(body) =
                                failed_item(key, status, anyhow!("gateway cdn object: HTTP {status:?}"))?
                            {
                                finished(*i, body)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(urls)
    }

    fn cdn_objects(
        &self,
        epoch: &str,
        items: &[(usize, &str)],
    ) -> std::result::Result<Option<Vec<serde_json::Value>>, RouteFailure> {
        let keys: Vec<_> = items.iter().map(|(_, key)| *key).collect();
        let body = serde_json::to_vec(&serde_json::json!({"epoch":epoch,"keys":keys})).context("encode CDN request")?;
        let response = match self.proof_request(Method::POST, "cdn", &[], Some(body), &[]) {
            Err(error) if error.is::<super::download::ProofRejected>() => {
                self.cdn_batch_off.store(true, Ordering::SeqCst);
                return Ok(None);
            }
            response => response?,
        };
        match response.status().as_u16() {
            404 | 405 => {
                let body = self.bytes(response, 4096)?;
                if String::from_utf8_lossy(&body).contains("no such project") {
                    return Err(anyhow!("gateway cdn: no such project").into());
                }
                self.cdn_batch_off.store(true, Ordering::SeqCst);
                Ok(None)
            }
            200 => {
                let mut reply: serde_json::Value = self.json(response, 1 << 20)?;
                let Some(serde_json::Value::Array(objects)) = reply.get_mut("objects").map(serde_json::Value::take) else {
                    return Err(anyhow!("CDN reply has no objects").into());
                };
                if objects.len() != items.len() {
                    return Err(anyhow!("CDN reply has wrong object count").into());
                }
                Ok(Some(objects))
            }
            status => {
                let _ = self.bytes(response, 4096);
                Err(RouteFailure::http("cdn", status))
            }
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        use super::listing_cache::Entry;
        #[derive(Deserialize)]
        struct Objects {
            objects: Vec<Entry>,
            #[serde(default)]
            continuation: Option<String>,
        }
        let has_identity = self.identity.lock().map_err(|_| anyhow!("gateway identity lock failed"))?.is_some();
        if self.is_public() {
            let logical = epoch_path(prefix).map(|(_, k)| k).unwrap_or(prefix);
            if !prefix.starts_with("keys/")
                && !logical.starts_with("records/")
                && !logical.starts_with("states/")
                && !logical.starts_with("files/")
            {
                return Ok(Vec::new());
            }
        }
        let cache_path = self.listing_cache_path(prefix, false);
        let cached = cache_path.as_deref().and_then(super::listing_cache::load::<Vec<Entry>>).filter(|cached| {
            let mut found = std::collections::BTreeSet::new();
            super::listing_cache::validator(prefix, false, &cached.listing).as_ref() == Some(&cached.etag)
                && cached.listing.len() <= 100_000
                && cached.listing.iter().all(|entry| {
                    super::backend::check_key(&entry.key).is_ok() && entry.key.starts_with(prefix) && found.insert(&entry.key)
                })
        });
        let mut etag = None;
        let mut found = std::collections::BTreeSet::new();
        let mut cursors = std::collections::BTreeSet::new();
        let mut continuation: Option<String> = None;
        let mut objects = Vec::new();
        let mut legacy = false;
        let mut restarted = false;
        loop {
            let mut query = vec![("prefix", prefix)];
            if !legacy {
                query.push(("limit", "1000"));
            }
            if let Some(cursor) = &continuation {
                query.push(("continuation", cursor));
            }
            let conditional = cached.as_ref().filter(|_| continuation.is_none());
            let headers = conditional.map(|cached| ("if-none-match", cached.etag.as_str()));
            let response = if self.is_public() {
                self.public_request_query("objects", &query, headers.as_slice())?
            } else if !has_identity
                && (prefix.starts_with("records/")
                    || prefix.starts_with("states/")
                    || prefix == "keys/"
                    || overlay_keys_prefix(prefix))
            {
                self.send_query_headers(Method::GET, "objects", self.token.as_deref(), &query, None, headers.as_slice())?
            } else {
                self.read_request_query("objects", &query, headers.as_slice())?
            };
            if response.status().as_u16() == 400 && continuation.is_none() && !legacy {
                // Older servers may reject the opt-in pagination query. Retry only the first
                // page with the legacy prefix query and the same authentication boundary.
                legacy = true;
                continue;
            }
            if response.status().as_u16() == 400 && continuation.is_some() && !legacy {
                // Listing cursors expire and are bound to the Gateway process. Discard the
                // partial result before one restart, without changing the access mode.
                let body = self.bytes(response, 4096)?;
                let invalid_cursor = serde_json::from_slice::<serde_json::Value>(&body).ok().is_some_and(|value| {
                    value.get("error").and_then(|error| error.as_str()) == Some("invalid listing continuation")
                });
                if !invalid_cursor || restarted {
                    bail!("gateway: HTTP 400");
                }
                restarted = true;
                objects.clear();
                found.clear();
                cursors.clear();
                continuation = None;
                continue;
            }
            if response.status().as_u16() == 304 {
                let cached = conditional.context("gateway returned 304 without a cached listing")?;
                let mut objects: Vec<_> = cached
                    .listing
                    .iter()
                    .map(|entry| Object { key: entry.key.clone(), modified: entry.modified / 1000, size: entry.size })
                    .collect();
                objects.sort_by(|a, b| a.key.cmp(&b.key));
                return Ok(objects);
            }
            if continuation.is_none() {
                etag = super::listing_cache::etag(&response);
            }
            // Older gateways may omit the cursor and return their bounded complete listing.
            // New gateways return at most 1,000 objects per page, each at most 8 MiB.
            let listing: Objects = self.json(response, 8 << 20)?;
            if objects.len() + listing.objects.len() > 100_000 {
                bail!("too many gateway objects");
            }
            for entry in listing.objects {
                super::backend::check_key(&entry.key)?;
                if !entry.key.starts_with(prefix) || !found.insert(entry.key.clone()) {
                    bail!("invalid gateway object listing");
                }
                objects.push(entry);
            }
            let Some(cursor) = listing.continuation else { break };
            if cursor.is_empty() || cursor.len() > 8192 || !cursors.insert(cursor.clone()) || cursors.len() > 1024 {
                bail!("invalid gateway listing continuation");
            }
            continuation = Some(cursor);
        }
        if let Some(path) = cache_path {
            let verified = etag.filter(|tag| super::listing_cache::validator(prefix, false, &objects).as_ref() == Some(tag));
            super::listing_cache::store(&path, verified, &objects);
        }
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(objects
            .into_iter()
            .map(|entry| Object { key: entry.key, modified: entry.modified / 1000, size: entry.size })
            .collect())
    }

    /// Bind the cache to the canonical host/project, public route, full epoch prefix and listing mode.
    fn listing_cache_path(&self, prefix: &str, folders: bool) -> Option<std::path::PathBuf> {
        let mut url = self.base.clone();
        let path = if self.is_public() { self.base.path().strip_prefix("/v1")? } else { self.base.path() };
        url.set_path(&format!("{path}objects"));
        url.query_pairs_mut()
            .append_pair("prefix", prefix)
            .append_pair(if folders { "delimiter" } else { "limit" }, if folders { "/" } else { "1000" });
        Some(super::listing_cache::path(self.listing_cache_dir.as_deref()?, url.as_str()))
    }

    /// The overlay folders (`overlays/<id>/`) of the store, from one folder listing (`delimiter=/`). None when
    /// the gateway does not list folders (an older gateway, or a public reader): the caller then lists
    /// `overlays/` in full.
    pub fn overlay_folders(&self) -> Result<Option<Vec<String>>> {
        if self.is_public() {
            return Ok(None);
        }
        let cache_path = self.listing_cache_path("overlays/", true);
        let cached = cache_path.as_deref().and_then(super::listing_cache::load::<Vec<String>>).filter(|cached| {
            super::listing_cache::folder_validator("overlays/", &cached.listing).as_ref() == Some(&cached.etag)
                && cached.listing.len() <= 1000
                && cached.listing.iter().all(|prefix| {
                    prefix.strip_prefix("overlays/").and_then(|p| p.strip_suffix('/')).is_some_and(crate::model::is_uuid)
                })
        });
        let headers = cached.as_ref().map(|cached| ("if-none-match", cached.etag.as_str()));
        let response = self.read_request_query("objects", &[("prefix", "overlays/"), ("delimiter", "/")], headers.as_slice())?;
        if response.status().as_u16() == 304 {
            return Ok(Some(cached.context("gateway returned 304 without cached folders")?.listing));
        }
        if !response.status().is_success() {
            return Ok(None);
        }
        let etag = super::listing_cache::etag(&response);
        let folders = overlay_folders(&self.bytes(response, 1 << 20)?);
        if let (Some(path), Some(folders)) = (cache_path, folders.as_ref()) {
            let verified = etag.filter(|tag| super::listing_cache::folder_validator("overlays/", folders).as_ref() == Some(tag));
            super::listing_cache::store(&path, verified, folders);
        }
        Ok(folders)
    }

    pub(super) fn release_stream(&self, key: &str) -> Result<Option<super::download::Counted<Response>>> {
        let logical = epoch_path(key).map(|(_, key)| key).unwrap_or(key);
        if !super::format::release_file(logical) {
            bail!("invalid release object");
        }
        let response =
            if self.is_public() { self.public_request(key, None)? } else { self.read_request(&format!("objects/{key}"), None)? };
        match response.status().as_u16() {
            200 => {
                if response.content_length().is_some_and(|n| n > super::format::RELEASE_OBJECT_LIMIT) {
                    bail!("release object is too large");
                }
                Ok(Some(super::download::Counted {
                    inner: response,
                    counters: self.counters.clone(),
                    payload: true,
                    upload: false,
                }))
            }
            404 => self.absent(response),
            status => {
                let _ = self.bytes(response, 4096);
                Err(get_status_error(status))
            }
        }
    }

    pub fn put_file_new(&self, key: &str, file: std::fs::File) -> Result<bool> {
        let logical = epoch_path(key).map(|(_, key)| key).unwrap_or(key);
        // A public release file lives at the root of the store, outside the epochs.
        let public = epoch_path(key).is_none() && super::format::public_release_file(key);
        if self.is_public() || !(super::format::release_file(logical) || public) {
            bail!("invalid release write");
        }
        let length = file.metadata()?.len();
        if length > super::format::RELEASE_OBJECT_LIMIT {
            bail!("release object is too large");
        }
        let token = self.token.as_deref().context("set LAYR_GATEWAY_TOKEN_FILE to a private write token file")?;
        let url = self.base.join(&format!("objects/{key}"))?;
        // A create-only PUT may go again (a gateway restart): each try sends the file from its start. A try after one
        // that landed gets 412.
        let response = self.retry(true, || {
            use std::io::Seek;
            let mut body = file.try_clone()?;
            body.seek(std::io::SeekFrom::Start(0))?;
            self.execute(
                self.http
                    .put(url.clone())
                    .bearer_auth(token)
                    .timeout(Duration::from_secs(7200))
                    .header("content-type", "application/octet-stream")
                    .body(reqwest::blocking::Body::sized(
                        super::download::Counted { inner: body, counters: self.counters.clone(), payload: true, upload: true },
                        length,
                    )),
                true,
            )
        })?;
        match response.status().as_u16() {
            201 => Ok(true),
            412 => Ok(false),
            status => {
                let _ = self.bytes(response, 4096);
                bail!("gateway put: HTTP {status}");
            }
        }
    }

    pub fn put_new(&self, key: &str, data: Vec<u8>) -> Result<bool> {
        if self.is_public() {
            bail!("public gateway access is read only");
        }
        if key == "store.json" {
            bail!("create the store with owner credentials before adding a gateway remote");
        }
        if data.len() > OBJECT_LIMIT {
            bail!("gateway object exceeds 32 MiB");
        }
        let logical = epoch_path(key).map(|(_, key)| key).unwrap_or(key);
        let person_request =
            logical.strip_prefix("requests/").and_then(|s| s.strip_suffix(".json")).is_some_and(layr_store::request::request_id);
        if person_request {
            let action = format!("objects/{key}");
            let response = self.send(Method::PUT, &action, Some(&self.session_token(false)?), None, Some(data.clone()))?;
            let response = if response.status().as_u16() == 401 {
                let _ = self.bytes(response, 1 << 20);
                self.send(Method::PUT, &action, Some(&self.session_token(true)?), None, Some(data))?
            } else {
                response
            };
            return match response.status().as_u16() {
                201 => Ok(true),
                412 => Ok(false),
                status => {
                    let _ = self.bytes(response, 4096);
                    bail!("gateway put: HTTP {status}");
                }
            };
        }
        // Pending epoch slots are staged before discovery exists, so they cannot
        // obtain a read proof yet. A project writer uses its grant for those PUTs.
        let action = format!("objects/{key}");
        let response = if logical.starts_with("slots/") && (epoch_path(key).is_none() || self.token.is_none()) {
            self.proof_request(Method::PUT, &action, &[], Some(data), &[])?
        } else {
            let token = self.token.as_deref().context("set LAYR_GATEWAY_TOKEN_FILE to a private write token file")?;
            self.send(Method::PUT, &action, Some(token), None, Some(data))?
        };
        match response.status().as_u16() {
            201 => Ok(true),
            412 => Ok(false),
            status => {
                let _ = self.bytes(response, 4096);
                bail!("gateway put: HTTP {status}");
            }
        }
    }
    pub fn delete(&self, key: &str) -> Result<()> {
        if self.is_public() {
            bail!("public gateway access is read only");
        }
        let token = self.token.as_deref().context("set LAYR_GATEWAY_TOKEN_FILE to a private gc token file")?;
        let response = self.send(Method::DELETE, &format!("objects/{key}"), Some(token), None, None)?;
        match response.status().as_u16() {
            204 => Ok(()),
            // The gateway answers 204 for an object that is gone already, so any other answer is an error: a 404 says
            // that the route or the project is unknown.
            status => {
                let body = self.bytes(response, 4096).unwrap_or_default();
                bail!("gateway delete {key}: HTTP {status}: {}", String::from_utf8_lossy(&body).trim())
            }
        }
    }
}

/// The `prefixes` of a folder listing of `overlays/`, each `overlays/<uuid>/`. None for any other answer
/// (an older gateway answers with objects and no `prefixes`).
fn overlay_folders(body: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value.get("objects").and_then(|o| o.as_array()).is_some_and(|o| !o.is_empty()) {
        return None;
    }
    let mut folders = Vec::new();
    for prefix in value.get("prefixes")?.as_array()? {
        let prefix = prefix.as_str()?;
        let id = prefix.strip_prefix("overlays/")?.strip_suffix('/')?;
        if !crate::model::is_uuid(id) {
            return None;
        }
        folders.push(prefix.to_string());
    }
    Some(folders)
}

fn overlay_keys_prefix(prefix: &str) -> bool {
    let parts: Vec<_> = prefix.split('/').collect();
    parts.len() == 4 && parts[0] == "overlays" && crate::model::is_uuid(parts[1]) && parts[2] == "keys" && parts[3].is_empty()
}

fn overlay_keys_path(key: &str) -> bool {
    let mut parts = key.split('/');
    parts.next() == Some("overlays")
        && parts.next().is_some_and(crate::model::is_uuid)
        && parts.next() == Some("keys")
        && parts.next().is_some_and(|name| name.ends_with(".json"))
        && parts.next().is_none()
}

fn token_file(path: &std::path::Path, uid: u32) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open gateway token file")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o600 {
        bail!("gateway token file must be a regular file owned by the caller with mode 0600");
    }
    let mut text = String::new();
    file.take(257).read_to_string(&mut text).map_err(|_| anyhow!("invalid gateway token file"))?;
    let token = text.trim_end_matches(['\r', '\n']);
    if text.len() > 256 || token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
        bail!("invalid gateway token file");
    }
    Ok(token.into())
}

fn epoch_path(key: &str) -> Option<(&str, &str)> {
    let (epoch, inner) = key.strip_prefix("project/epochs/")?.split_once('/')?;
    super::format::is_uuid(epoch).then_some((epoch, inner))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn an_overlay_folder_listing_names_only_overlay_folders() {
        let a = "01a1d0ab-1111-7000-8000-000000000001";
        let b = "01a1d0ab-2222-7000-8000-000000000002";
        let body = serde_json::json!({"objects": [], "prefixes": [format!("overlays/{a}/"), format!("overlays/{b}/")]});
        assert_eq!(
            overlay_folders(&serde_json::to_vec(&body).unwrap()),
            Some(vec![format!("overlays/{a}/"), format!("overlays/{b}/")])
        );
        // An older gateway ignores the delimiter and returns objects: the caller lists in full.
        let old = serde_json::json!({"objects": [{"key": format!("overlays/{a}/keys/x.json"), "modified": 1}]});
        assert_eq!(overlay_folders(&serde_json::to_vec(&old).unwrap()), None);
        for bad in ["overlays/not-a-uuid/", "records/x/", "overlays/../", &format!("overlays/{a}")] {
            let body = serde_json::json!({"objects": [], "prefixes": [bad]});
            assert_eq!(overlay_folders(&serde_json::to_vec(&body).unwrap()), None, "{bad}");
        }
        assert_eq!(overlay_folders(b"not json"), None);
    }
    use std::os::unix::fs::{symlink, PermissionsExt};

    // macOS check sandboxes deny TCP. The same HTTP fixtures use a private Unix socket there;
    // ordinary test runs retain TCP, including its connection and retry behavior.
    pub(super) enum TestListener {
        Tcp(std::net::TcpListener),
        Unix(std::os::unix::net::UnixListener),
    }

    pub(super) enum TestStream {
        Tcp(std::net::TcpStream),
        Unix(std::os::unix::net::UnixStream),
    }

    pub(in crate::store_remote) enum TestEndpoint {
        Tcp(std::net::SocketAddr),
        Unix(std::path::PathBuf),
    }

    impl Drop for TestEndpoint {
        fn drop(&mut self) {
            if let Self::Unix(path) = self {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    impl TestListener {
        fn accept(&self) -> std::io::Result<(TestStream, ())> {
            match self {
                Self::Tcp(listener) => listener.accept().map(|(stream, _)| (TestStream::Tcp(stream), ())),
                Self::Unix(listener) => listener.accept().map(|(stream, _)| (TestStream::Unix(stream), ())),
            }
        }
    }

    impl TestStream {
        fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            match self {
                Self::Tcp(stream) => stream.set_read_timeout(timeout),
                Self::Unix(stream) => stream.set_read_timeout(timeout),
            }
        }
    }

    impl std::io::Read for TestStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self {
                Self::Tcp(stream) => stream.read(buf),
                Self::Unix(stream) => stream.read(buf),
            }
        }
    }

    impl std::io::Write for TestStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Tcp(stream) => stream.write(buf),
                Self::Unix(stream) => stream.write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                Self::Tcp(stream) => stream.flush(),
                Self::Unix(stream) => stream.flush(),
            }
        }
    }

    impl TestEndpoint {
        fn configure(&self, gateway: &mut Gateway) {
            let host = match self {
                Self::Tcp(address) => address.to_string(),
                Self::Unix(path) => {
                    gateway.http = Client::builder()
                        .no_proxy()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(connect_timeout(Duration::from_secs(5)))
                        .timeout(Duration::from_secs(150))
                        .unix_socket(path.clone())
                        .build()
                        .unwrap();
                    "localhost".into()
                }
            };
            gateway.base = reqwest::Url::parse(&format!("http://{host}/v1/p/project/")).unwrap();
        }

        fn connect(&self) -> std::io::Result<TestStream> {
            match self {
                Self::Tcp(address) => std::net::TcpStream::connect(address).map(TestStream::Tcp),
                Self::Unix(path) => std::os::unix::net::UnixStream::connect(path).map(TestStream::Unix),
            }
        }
    }

    /// The request head up to and including the blank line, read byte by byte so the body stays in the stream.
    fn read_head(stream: &mut impl std::io::Read) -> Vec<u8> {
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        head
    }

    /// A loopback gateway for `puts` PUTs, one thread per connection, that answers 201 once `together` request bodies
    /// have arrived. It reads the bodies in small pieces and keeps none of them.
    #[allow(clippy::unwrap_used)]
    pub(in crate::store_remote) fn put_fixture(
        puts: usize,
        together: usize,
    ) -> (Gateway, TestEndpoint, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let (listener, endpoint) = test_listener();
        let arrived = std::sync::Arc::new(std::sync::Barrier::new(together));
        let server = std::thread::spawn(move || {
            let connections: Vec<_> = (0..puts)
                .map(|_| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let arrived = arrived.clone();
                    std::thread::spawn(move || {
                        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                        let head = String::from_utf8(read_head(&mut stream)).unwrap().to_ascii_lowercase();
                        let mut left: usize = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap();
                        let mut piece = [0; 1 << 16];
                        while left > 0 {
                            let read = stream.read(&mut piece[..left.min(1 << 16)]).unwrap();
                            assert!(read > 0, "the body ended early");
                            left -= read;
                        }
                        arrived.wait();
                        write!(stream, "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    })
                })
                .collect();
            for connection in connections {
                connection.join().unwrap();
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        endpoint.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        (gateway, endpoint, server)
    }

    fn test_listener() -> (TestListener, TestEndpoint) {
        if std::env::var_os("LAYR_TEST_GATEWAY_UNIX").as_deref() == Some(std::ffi::OsStr::new("1")) {
            let path = std::env::temp_dir().join(format!("lg-{:016x}.sock", crate::ids::random_u64()));
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            (TestListener::Unix(listener), TestEndpoint::Unix(path))
        } else {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            (TestListener::Tcp(listener), TestEndpoint::Tcp(address))
        }
    }

    // Scripted HTTP replies make request and body counts independent of backend-operation tracing.
    fn conditional_fixture(
        replies: Vec<(u16, Option<&str>, serde_json::Value)>,
    ) -> (Gateway, TestEndpoint, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let (listener, endpoint) = test_listener();
        let replies: Vec<_> = replies.into_iter().map(|(status, etag, body)| (status, etag.map(str::to_owned), body)).collect();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let mut body_bytes = 0;
            for (status, etag, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let head = read_head(&mut stream);
                let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                stream.read_exact(&mut vec![0; length]).unwrap();
                requests.push(head);
                let body = if status == 304 { String::new() } else { body.to_string() };
                body_bytes += body.len();
                write!(stream, "HTTP/1.1 {status} response\r\nContent-Length: {}\r\nConnection: close\r\n", body.len()).unwrap();
                if let Some(etag) = etag {
                    write!(stream, "ETag: {etag}\r\n").unwrap();
                }
                write!(stream, "\r\n{body}").unwrap();
            }
            eprintln!("conditional listing fixture: {} HTTP requests, {body_bytes} response-body bytes", requests.len());
            requests
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        endpoint.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        gateway.listing_cache_dir = Some(std::env::temp_dir().join(format!("layr-listing-{}", crate::ids::new_id())));
        (gateway, endpoint, server)
    }

    fn listing_page(keys: &[&str], cursor: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "objects": keys.iter().map(|key| serde_json::json!({"key": key, "modified": 12345})).collect::<Vec<_>>(),
            "continuation": cursor,
        })
    }

    fn listing_tag(keys: &[&str]) -> String {
        let entries: Vec<super::super::listing_cache::Entry> =
            serde_json::from_value(listing_page(keys, None)["objects"].clone()).unwrap();
        super::super::listing_cache::validator("states/", false, &entries).unwrap()
    }

    #[test]
    fn listing_cache_keeps_descriptor_reads_live_in_the_client_and_its_clones() {
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, None, serde_json::json!({"recipient":"first"})),
            (200, None, serde_json::json!({"recipient":"second"})),
            (200, None, serde_json::json!({"recipient":"third"})),
        ]);
        for (client, expected) in [(&gateway, "first"), (&gateway.clone(), "second"), (&gateway, "third")] {
            let body = client.get("store.json").unwrap().unwrap();
            let descriptor: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(descriptor["recipient"], expected);
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.starts_with("get /v1/p/project/store.json ")));
        assert!(requests.iter().all(|request| !request.contains("if-none-match:")));
        assert!(!gateway.listing_cache_dir.unwrap().exists());
    }

    #[test]
    fn conditional_listing_aba_never_persists_pages_that_do_not_match_the_first_tag() {
        let tag = r#""5c322c1df96524a93cac9b79edb2e62b1a6dd2bc1ebb701611b5e45689d275db""#;
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(tag), listing_page(&["states/a"], Some("next"))),
            (200, None, listing_page(&["states/z-transient"], None)),
            // The transient entry was removed: the server is back at the original tag.
            (200, Some(tag), listing_page(&["states/a"], None)),
            (304, Some(tag), serde_json::Value::Null),
        ]);
        assert_eq!(gateway.list("states/").unwrap().len(), 2);
        let path = gateway.listing_cache_path("states/", false).unwrap();
        let mismatched_was_cached = path.exists();
        assert_eq!(gateway.list("states/").unwrap().len(), 1);
        assert_eq!(gateway.list("states/").unwrap().len(), 1);
        let requests = server.join().unwrap();
        assert!(!mismatched_was_cached, "ABA pages must not be stored under the original validator");
        assert!(!requests[2].contains("if-none-match:"));
        assert!(requests[3].contains("if-none-match:"));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_reuses_a_complete_listing_in_one_request() {
        let v1 = listing_tag(&["states/a", "states/z"]);
        for public in [false, true] {
            let (gateway, endpoint, server) = conditional_fixture(vec![
                (200, Some(&v1), listing_page(&["states/a"], Some("next"))),
                (200, Some("\"ignored-continuation-tag\""), listing_page(&["states/z"], None)),
                (304, Some(&v1), serde_json::Value::Null),
            ]);
            if public {
                gateway.use_public_access();
            }
            let first = gateway.list("states/").unwrap();
            // A fresh client uses the on-disk cache (not an operation-lifetime snapshot).
            let mut fresh = Gateway::open("layr+https://localhost/project").unwrap();
            endpoint.configure(&mut fresh);
            fresh.token = gateway.token.clone();
            fresh.listing_cache_dir = gateway.listing_cache_dir.clone();
            if public {
                fresh.use_public_access();
            }
            let second = fresh.list("states/");
            let requests = server.join().unwrap();
            let second = second.unwrap();
            assert_eq!(
                first.iter().map(|o| (&o.key, o.modified)).collect::<Vec<_>>(),
                second.iter().map(|o| (&o.key, o.modified)).collect::<Vec<_>>()
            );
            assert_eq!(second.len(), 2);
            assert_eq!(second[0].modified, 12);
            assert_eq!(requests.len(), 3, "two cold pages, then exactly one warm request");
            eprintln!("listing measurement (public={public}): cold=2 requests/140 body bytes, warm=1 request/0 body bytes");
            assert!(!requests[0].contains("if-none-match:"));
            assert!(!requests[1].contains("if-none-match:"));
            assert!(requests[2].contains(&format!("if-none-match: {v1}\r\n")));
            assert!(!requests[2].contains("continuation="));
            assert_eq!(requests[2].contains("authorization: bearer fixture.write"), !public);
            std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
        }
    }

    #[test]
    fn conditional_listing_changes_repage_and_only_complete_passes_replace_the_cache() {
        let v1 = listing_tag(&["states/old"]);
        let v2 = listing_tag(&["states/new"]);
        let v3 = listing_tag(&["states/a", "states/z"]);
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&v1), listing_page(&["states/old"], None)),
            (200, Some(&v2), listing_page(&["states/new"], Some("next"))),
            (403, None, serde_json::json!({"error": "interrupted"})),
            (200, Some(&v3), listing_page(&["states/a"], Some("next"))),
            (200, None, listing_page(&["states/z"], None)),
            (304, Some(&v3), serde_json::Value::Null),
        ]);
        gateway.list("states/").unwrap();
        let path = gateway.listing_cache_path("states/", false).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(gateway.list("states/").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let changed = gateway.list("states/").unwrap();
        assert_eq!(changed.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(), ["states/a", "states/z"]);
        assert_eq!(gateway.list("states/").unwrap().len(), 2);
        let requests = server.join().unwrap();
        for n in [1, 3] {
            assert!(requests[n].contains(&format!("if-none-match: {v1}")));
        }
        assert!(requests[5].contains(&format!("if-none-match: {v3}")));
        for n in [2, 4] {
            assert!(!requests[n].contains("if-none-match:"));
        }
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_without_an_etag_stores_nothing_and_corruption_lists_in_full() {
        let v1 = listing_tag(&["states/a"]);
        let v4 = listing_tag(&["states/d"]);
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&v1), listing_page(&["states/a"], None)),
            (200, None, listing_page(&["states/b"], None)),
            (200, None, listing_page(&["states/c"], None)),
            (200, Some(&v4), listing_page(&["states/d"], None)),
        ]);
        gateway.list("states/").unwrap();
        let path = gateway.listing_cache_path("states/", false).unwrap();
        std::fs::write(&path, b"interrupted JSON").unwrap();
        assert_eq!(gateway.list("states/").unwrap()[0].key, "states/b");
        assert!(!path.exists());
        gateway.list("states/").unwrap();
        assert!(!path.exists());
        // A valid JSON cache with keys outside its prefix is also discarded.
        std::fs::write(&path, br#"{"etag":"\"bad\"","listing":[{"key":"records/wrong","modified":0}]}"#).unwrap();
        assert_eq!(gateway.list("states/").unwrap()[0].key, "states/d");
        let requests = server.join().unwrap();
        assert!(requests.iter().all(|r| !r.contains("if-none-match:")));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_drops_an_old_validator_when_the_gateway_stops_sending_etags() {
        let v1 = listing_tag(&["states/a"]);
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&v1), listing_page(&["states/a"], None)),
            (200, None, listing_page(&["states/b"], None)),
            (200, None, listing_page(&["states/c"], None)),
        ]);
        gateway.list("states/").unwrap();
        gateway.list("states/").unwrap();
        assert!(!gateway.listing_cache_path("states/", false).unwrap().exists());
        gateway.list("states/").unwrap();
        let requests = server.join().unwrap();
        assert!(requests[1].contains(&format!("if-none-match: {v1}")));
        assert!(!requests[2].contains("if-none-match:"));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_valid_json_corruption_and_metadata_mismatch_are_not_cached() {
        let page = serde_json::json!({"objects": [{"key":"states/a", "modified":12345, "size":3, "etag":"storage-v1"}]});
        let entries = serde_json::from_value::<Vec<super::super::listing_cache::Entry>>(page["objects"].clone()).unwrap();
        let tag = super::super::listing_cache::validator("states/", false, &entries).unwrap();
        let mut changed = page.clone();
        changed["objects"][0]["etag"] = serde_json::json!("storage-v2");
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&tag), page.clone()),
            (200, Some(&tag), page.clone()),
            (200, Some(&tag), changed),
            (200, Some(&tag), page),
        ]);
        gateway.list("states/").unwrap();
        let path = gateway.listing_cache_path("states/", false).unwrap();
        let mut cache: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        cache["listing"][0]["size"] = serde_json::json!(4);
        std::fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        gateway.list("states/").unwrap();
        gateway.list("states/").unwrap();
        assert!(!path.exists(), "same keys/times with a different storage ETag must not be cached");
        gateway.list("states/").unwrap();
        let requests = server.join().unwrap();
        assert!(!requests[1].contains("if-none-match:"), "valid JSON with a wrong digest is corrupt");
        assert!(requests[2].contains("if-none-match:"));
        assert!(!requests[3].contains("if-none-match:"));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_cache_is_bound_to_remote_project_epoch_prefix_and_mode() {
        let mut gateway = Gateway::open("layr+https://example.test/project").unwrap();
        gateway.listing_cache_dir = Some(std::env::temp_dir().join("binding-only"));
        let original = gateway.listing_cache_path("states/", false).unwrap();
        let canonical = Gateway::open("layr+https://EXAMPLE.test:443/project").unwrap();
        gateway.base = canonical.base;
        assert_eq!(gateway.listing_cache_path("states/", false).unwrap(), original);
        let mut paths = std::collections::BTreeSet::from([original]);
        for prefix in ["records/", "project/epochs/one/states/", "project/epochs/two/states/"] {
            assert!(paths.insert(gateway.listing_cache_path(prefix, false).unwrap()));
        }
        assert!(paths.insert(gateway.listing_cache_path("states/", true).unwrap()));
        gateway.use_public_access();
        assert!(paths.insert(gateway.listing_cache_path("states/", false).unwrap()));
        gateway.base.set_host(Some("other.test")).unwrap();
        assert!(paths.insert(gateway.listing_cache_path("states/", false).unwrap()));
        gateway.base.set_path("/v1/p/other/");
        assert!(paths.insert(gateway.listing_cache_path("states/", false).unwrap()));
    }

    #[test]
    fn conditional_listing_rejects_unsolicited_and_continuation_304_responses() {
        let v1 = listing_tag(&["states/a"]);
        let v2 = listing_tag(&["states/b"]);
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (304, Some(&v1), serde_json::Value::Null),
            (200, Some(&v1), listing_page(&["states/a"], None)),
            (200, Some(&v2), listing_page(&["states/b"], Some("next"))),
            (304, Some(&v1), serde_json::Value::Null),
        ]);
        assert!(gateway.list("states/").unwrap_err().to_string().contains("304 without"));
        gateway.list("states/").unwrap();
        let path = gateway.listing_cache_path("states/", false).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(gateway.list("states/").unwrap_err().to_string().contains("304 without"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        server.join().unwrap();
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_restart_keeps_only_the_new_complete_pass_and_etag() {
        let v1 = listing_tag(&["states/old"]);
        let v2 = listing_tag(&["states/new"]);
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&v1), listing_page(&["states/old"], Some("expired"))),
            (400, None, serde_json::json!({"error": "invalid listing continuation"})),
            (200, Some(&v2), listing_page(&["states/new"], None)),
            (304, Some(&v2), serde_json::Value::Null),
        ]);
        assert_eq!(gateway.list("states/").unwrap()[0].key, "states/new");
        assert_eq!(gateway.list("states/").unwrap()[0].key, "states/new");
        let requests = server.join().unwrap();
        assert!(requests[3].contains(&format!("if-none-match: {v2}")));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_retains_the_etag_when_renewing_a_read_proof() {
        let v1 = listing_tag(&["states/a"]);
        let identity = keys::generate_identity();
        let sealed = keys::encrypt(&keys::recipient_of(&identity).unwrap(), b"challenge.proof").unwrap();
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&v1), listing_page(&["states/a"], None)),
            (401, None, serde_json::json!({"error": "expired proof"})),
            (200, None, serde_json::json!({"challenge": base64::engine::general_purpose::STANDARD.encode(sealed)})),
            (200, None, serde_json::json!({"token": "new.proof", "expires": crate::ids::now_ms() + 600_000})),
            (304, Some(&v1), serde_json::Value::Null),
        ]);
        gateway.set_identity(&identity).unwrap();
        *gateway.read.lock().unwrap() = Some(("old.proof".into(), crate::ids::now_ms() + 300_000));
        assert_eq!(gateway.list("states/").unwrap().len(), 1);
        assert_eq!(gateway.list("states/").unwrap().len(), 1);
        let requests = server.join().unwrap();
        for n in [1, 4] {
            assert!(requests[n].contains(&format!("if-none-match: {v1}")));
        }
        assert!(requests[1].contains("authorization: bearer old.proof"));
        assert!(requests[4].contains("authorization: bearer new.proof"));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    #[test]
    fn conditional_listing_folders_revalidate_with_the_read_proof() {
        let folder = "overlays/01a1d0ab-1111-7000-8000-000000000001/";
        let tag = super::super::listing_cache::folder_validator("overlays/", &[folder.into()]).unwrap();
        let (gateway, _endpoint, server) = conditional_fixture(vec![
            (200, Some(&tag), serde_json::json!({"objects": [], "prefixes": [folder]})),
            (304, Some(&tag), serde_json::Value::Null),
        ]);
        *gateway.read.lock().unwrap() = Some(("fixture.proof".into(), crate::ids::now_ms() + 600_000));
        assert_eq!(gateway.overlay_folders().unwrap(), Some(vec![folder.into()]));
        assert_eq!(gateway.clone().overlay_folders().unwrap(), Some(vec![folder.into()]));
        let requests = server.join().unwrap();
        assert!(requests[1].contains("delimiter=%2f"));
        assert!(requests[1].contains(&format!("if-none-match: {tag}")));
        assert!(requests.iter().all(|r| r.contains("authorization: bearer fixture.proof")));
        std::fs::remove_dir_all(gateway.listing_cache_dir.unwrap()).unwrap();
    }

    fn listing_pages(pages: Vec<serde_json::Value>, public: bool) -> Result<Vec<Object>> {
        use std::io::Write;
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            for (number, page) in pages.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let request = read_head(&mut stream);
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with(if public { "get /p/project/objects?" } else { "get /v1/p/project/objects?" }));
                assert!(request.contains("limit=1000"));
                assert_eq!(request.contains("continuation="), number > 0);
                assert_eq!(request.contains("authorization: bearer fixture.write"), !public);
                let body = serde_json::to_vec(&page).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                let _ = stream.write_all(&body);
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        if public {
            gateway.use_public_access();
        }
        let result = gateway.list("states/");
        server.join().unwrap();
        result
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn descriptor_reads_observe_replacements_in_the_same_client_and_clones() {
        use std::io::{Read, Write};
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            for body in ["first", "second", "third", "fourth"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(request.starts_with(b"GET /v1/p/project/store.json "));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        assert_eq!(gateway.get("store.json").unwrap().unwrap(), b"first");
        assert_eq!(gateway.get("store.json").unwrap().unwrap(), b"second");
        assert_eq!(gateway.clone().get("store.json").unwrap().unwrap(), b"third");
        let mut fresh = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut fresh);
        assert_eq!(fresh.get("store.json").unwrap().unwrap(), b"fourth");
        server.join().unwrap();
    }

    #[test]
    fn json_requests_and_object_uploads_send_their_content_types() {
        use std::io::{Read, Write};
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let head = read_head(&mut stream);
                let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
                let length: usize =
                    head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                assert_eq!(body, b"{}");
                requests.push(head);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            requests
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        for action in ["batch", "cdn", "token", "objects/states/test.age"] {
            let method = if action.starts_with("objects/") { Method::PUT } else { Method::POST };
            assert!(gateway.send(method, action, None, None, Some(b"{}".to_vec())).unwrap().status().is_success());
        }
        for (head, action) in server.join().unwrap().iter().zip(["batch", "cdn", "token", "objects/states/test.age"]) {
            let kind = if action.starts_with("objects/") { "application/octet-stream" } else { "application/json" };
            assert!(head.contains(&format!("content-type: {kind}\r\n")), "{action}: {head}");
        }
    }

    #[test]
    fn a_restarting_gateway_is_asked_again_for_a_read() {
        use std::io::Write;
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            for answer in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                read_head(&mut stream);
                match answer {
                    0 => write!(stream, "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap(),
                    // The gateway process stops: the connection closes before any answer.
                    1 => drop(stream),
                    _ => {
                        let body = serde_json::to_vec(&serde_json::json!({"objects": [], "continuation": null})).unwrap();
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                        let _ = stream.write_all(&body);
                    }
                }
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        let result = gateway.list("states/");
        server.join().unwrap();
        assert!(result.unwrap().is_empty(), "a 502 and a closed connection are asked again for a read");
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_release_file_goes_again_from_its_start_while_the_gateway_restarts() {
        use std::io::{Read, Write};
        let (listener, address) = test_listener();
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let expected = content.clone();
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for status in ["502 Bad Gateway", "201 Created"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let head = read_head(&mut stream);
                let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
                let length: usize =
                    head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                bodies.push(body);
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            bodies
        });
        let path = std::env::temp_dir().join(format!("layr-release-{}", crate::ids::new_id()));
        std::fs::write(&path, &content).unwrap();
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        let created = gateway.put_file_new("public/release/v1.0.0/layr.tar.gz", std::fs::File::open(&path).unwrap()).unwrap();
        let bodies = server.join().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(created);
        assert!(bodies.len() == 2 && bodies.iter().all(|b| *b == expected), "both tries send the whole file");
        assert_eq!(gateway.transfer_stats().requests, 2);
        assert_eq!(gateway.transfer_stats().uploaded, 400_000);
    }

    #[test]
    fn a_delete_error_names_the_key_and_the_answer() {
        use std::io::Write;
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            for (status, body) in [
                ("204 No Content", ""),
                ("404 Not Found", "no such route"),
                ("403 Forbidden", "GC can delete only state data parts"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let request = read_head(&mut stream);
                assert!(request.starts_with(b"DELETE "));
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        let key = "states/01a1af29-70f7-7566-ac60-2a4fc1b7b4d5/bf9b1e4e.tree.age";
        gateway.delete(key).unwrap();
        let error = format!("{:#}", gateway.delete(key).unwrap_err());
        assert!(error.contains("404") && error.contains("no such route") && error.contains(key), "{error}");
        let error = format!("{:#}", gateway.delete(key).unwrap_err());
        assert!(error.contains("403") && error.contains("only state data parts") && error.contains(key), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_missing_project_is_not_a_missing_object() {
        use std::io::Write;
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            for body in ["no such project", "no such object", "no such project", "no such object"] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                read_head(&mut stream);
                write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                    .unwrap();
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        // A key that the grant reads, then a data part that a public reader reads.
        for key in ["keys/fixture.age", "states/01a1aad8-a8ed-7385-8cda-05195b72afa3/681e275b.0000.data.age"] {
            if key.starts_with("states/") {
                gateway.use_public_access();
            }
            let error = gateway.get(key).unwrap_err().to_string();
            assert!(error.contains("no such project"), "{error}");
            assert!(gateway.get(key).unwrap().is_none(), "a 404 for the object itself is a missing object");
        }
        server.join().unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_busy_gateway_is_asked_again() {
        use std::io::Write;
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            for answer in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                read_head(&mut stream);
                if answer < 2 {
                    let status = if answer == 0 { "503 Service Unavailable" } else { "429 Too Many Requests" };
                    write!(stream, "HTTP/1.1 {status}\r\nRetry-After: 0\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy")
                        .unwrap();
                } else {
                    let body = serde_json::to_vec(&serde_json::json!({"objects": [], "continuation": null})).unwrap();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    let _ = stream.write_all(&body);
                }
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("fixture.write".into());
        let result = gateway.list("states/");
        server.join().unwrap();
        assert!(result.unwrap().is_empty(), "a 503 and a 429 are asked again");
        assert_eq!(gateway.transfer_stats().requests, 3);
        assert_eq!(gateway.transfer_stats().control_received, 8 + LISTING.len() as u64);
    }

    #[test]
    fn listings_follow_pages_above_ten_thousand_and_preserve_access_modes() {
        for public in [false, true] {
            let pages = (0..11).map(|page| serde_json::json!({
                "objects": (page*1000..(page+1)*1000).map(|n| serde_json::json!({"key":format!("states/{n:05}/data"),"modified":1000})).collect::<Vec<_>>(),
                "continuation": if page < 10 { Some(format!("page{page}")) } else { None }
            })).collect();
            assert_eq!(listing_pages(pages, public).unwrap().len(), 11_000);
        }
    }

    #[test]
    fn listings_restart_once_only_for_an_invalid_continuation() {
        use std::io::{Read, Write};
        for public in [false, true] {
            for failure in [None, Some("invalid listing continuation"), Some("invalid object prefix")] {
                let (listener, address) = test_listener();
                let server = std::thread::spawn(move || {
                    let mut requests = Vec::new();
                    for number in 0..4 {
                        let (mut stream, _) = listener.accept().unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let mut request = Vec::new();
                        let mut byte = [0];
                        while !request.ends_with(b"\r\n\r\n") {
                            if stream.read_exact(&mut byte).is_err() {
                                return requests;
                            }
                            request.push(byte[0]);
                        }
                        if request == b"\r\n\r\n" {
                            return requests;
                        }
                        let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                        assert!(request.starts_with(if public {
                            "get /p/project/objects?"
                        } else {
                            "get /v1/p/project/objects?"
                        }));
                        assert!(request.contains("prefix=states%2f"));
                        assert!(request.contains("limit=1000"));
                        assert_eq!(request.contains("continuation="), number % 2 == 1);
                        assert_eq!(request.contains("authorization: bearer fixture.write"), !public);
                        requests.push(request);
                        let (status, body) = if number == 1 || (number == 3 && failure.is_some()) {
                            (400, serde_json::json!({"error":failure.unwrap_or("invalid listing continuation")}))
                        } else {
                            let key = if number == 0 {
                                "states/stale"
                            } else if number == 2 {
                                "states/new"
                            } else {
                                "states/final"
                            };
                            (
                                200,
                                serde_json::json!({"objects":[{"key":key,"modified":1000}],
                                "continuation":if number % 2 == 0 { Some("cursor") } else { None }}),
                            )
                        };
                        let body = body.to_string();
                        write!(
                            stream,
                            "HTTP/1.1 {status} response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .unwrap();
                    }
                    requests
                });
                let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
                address.configure(&mut gateway);
                gateway.token = Some("fixture.write".into());
                if public {
                    gateway.use_public_access();
                }
                let result = gateway.list("states/");
                // Release a server waiting for the next request when the client refuses early.
                let mut wake = address.connect().ok();
                if let Some(stream) = &mut wake {
                    let _ = stream.write_all(b"\r\n\r\n");
                }
                let requests = server.join().unwrap();
                drop(wake);
                match failure {
                    None => {
                        let keys: Vec<_> = result.unwrap().into_iter().map(|object| object.key).collect();
                        assert_eq!(keys, ["states/final", "states/new"]);
                        assert_eq!(requests.len(), 4);
                    }
                    Some("invalid listing continuation") => {
                        assert!(result.unwrap_err().to_string().contains("HTTP 400"));
                        assert_eq!(requests.len(), 4);
                    }
                    Some(_) => {
                        assert!(result.unwrap_err().to_string().contains("HTTP 400"));
                        assert_eq!(requests.len(), 2);
                    }
                }
            }
        }
    }

    #[test]
    fn old_gateways_can_reject_pagination_and_return_a_bounded_legacy_listing() {
        use std::io::Write;
        for public in [false, true] {
            let (listener, address) = test_listener();
            let server = std::thread::spawn(move || {
                for first in [true, false] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let request = read_head(&mut stream);
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    assert!(request.starts_with(if public { "get /p/project/objects?" } else { "get /v1/p/project/objects?" }));
                    assert_eq!(request.contains("limit="), first);
                    assert_eq!(request.contains("authorization: bearer fixture.write"), !public);
                    let (status, body) =
                        if first { (400, "{}") } else { (200, "{\"objects\":[{\"key\":\"states/a\",\"modified\":1000}]}") };
                    write!(
                        stream,
                        "HTTP/1.1 {status} response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
            address.configure(&mut gateway);
            gateway.token = Some("fixture.write".into());
            if public {
                gateway.use_public_access();
            }
            assert_eq!(gateway.list("states/").unwrap().len(), 1);
            server.join().unwrap();
        }
    }

    #[test]
    fn listings_reject_duplicates_cursor_loops_bad_prefixes_and_excess_totals() {
        let entry = |key: &str| serde_json::json!({"key":key,"modified":1000});
        for pages in [
            vec![
                serde_json::json!({"objects":[entry("states/a")],"continuation":"next"}),
                serde_json::json!({"objects":[entry("states/a")]}),
            ],
            vec![
                serde_json::json!({"objects":[entry("states/a")],"continuation":"next"}),
                serde_json::json!({"objects":[entry("states/b")],"continuation":"next"}),
            ],
            vec![serde_json::json!({"objects":[entry("records/a")]})],
        ] {
            assert!(listing_pages(pages, false).is_err());
        }
        let pages =
            vec![serde_json::json!({"objects":[],"continuation":"next"}), serde_json::json!({"objects":[entry("states/a")]})];
        assert_eq!(listing_pages(pages, true).unwrap().len(), 1);
        let oversized = (0..100_001).map(|n| entry(&format!("states/{n}"))).collect::<Vec<_>>();
        assert!(listing_pages(vec![serde_json::json!({"objects":oversized})], false).is_err());
    }

    #[test]
    fn token_expiry_tolerates_a_client_clock_behind_the_gateway() {
        let gateway_now = 1_792_951_000_000;
        let client_now = gateway_now - 30_000;
        for lifetime in [600_000, 43_200_000] {
            assert!(valid_token_expiry(gateway_now + lifetime, client_now, lifetime));
            assert!(valid_token_expiry(client_now + lifetime + 300_000, client_now, lifetime));
            assert!(!valid_token_expiry(client_now + lifetime + 300_001, client_now, lifetime));
            assert!(!valid_token_expiry(client_now, client_now, lifetime));
            assert!(!valid_token_expiry(client_now - 1, client_now, lifetime));
        }
    }

    #[test]
    fn session_challenge_tolerates_a_gateway_clock_ahead_of_the_client() {
        let key = || ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let challenge = |expires: i64| {
            let bytes = base64::engine::general_purpose::STANDARD.encode([9; 32]);
            serde_json::json!({"challenge": bytes, "expires": expires})
        };
        let session = serde_json::json!({
            "token": "session.fixture",
            "expires": crate::ids::now_ms() + 43_200_000,
            "login": "ann@example.com",
        });

        let (gateway, _endpoint, server) =
            conditional_fixture(vec![(200, None, challenge(crate::ids::now_ms() + 60_000 + 1_000)), (200, None, session)]);
        gateway.set_person("ann@example.com", key()).unwrap();
        assert_eq!(gateway.session_token(true).unwrap(), "session.fixture");
        assert_eq!(server.join().unwrap().len(), 2);

        let (gateway, _endpoint, server) = conditional_fixture(vec![(200, None, challenge(crate::ids::now_ms()))]);
        gateway.set_person("ann@example.com", key()).unwrap();
        let error = gateway.session_token(true).unwrap_err().to_string();
        assert!(error.contains("invalid or expired gateway session challenge"), "{error}");
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn missing_descriptor_keeps_optional_person_reads_and_hints_machine_grants() {
        use std::io::{Read, Write};
        for mode in ["reader", "person", "machine", "grant"] {
            let (listener, address) = test_listener();
            let server = std::thread::spawn(move || {
                // Without a grant, automatic public discovery still probes public.json anonymously.
                for n in 0..if mode == "grant" { 1 } else { 2 } {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut request = Vec::new();
                    let mut part = [0; 2048];
                    while !request.windows(4).any(|p| p == b"\r\n\r\n") {
                        let n = stream.read(&mut part).unwrap();
                        assert!(n > 0);
                        request.extend_from_slice(&part[..n]);
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    if n == 0 {
                        assert!(request.starts_with("get /v1/p/project/store.json "));
                        assert_eq!(request.contains("authorization: bearer person.fixture"), mode == "person");
                        assert_eq!(request.contains("authorization: bearer grant.fixture"), mode == "grant");
                    } else {
                        assert!(request.starts_with("get /p/project/public.json "));
                        assert!(!request.contains("authorization:"));
                    }
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
                }
            });
            let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
            address.configure(&mut gateway);
            if mode != "reader" {
                gateway.machine = Some("test-machine".into());
            }
            if mode == "person" {
                *gateway.person.lock().unwrap() = Some(PersonSession {
                    login: "ann@example.com".into(),
                    key: ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
                    token: Some(SessionToken {
                        token: "person.fixture".into(),
                        expires: crate::ids::now_ms() + 60_000,
                        login: "ann@example.com".into(),
                    }),
                });
            }
            if mode == "grant" {
                gateway.token = Some("grant.fixture".into());
            }
            let result = gateway.get("store.json");
            if matches!(mode, "reader" | "person") {
                assert!(result.unwrap().is_none());
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("404") && error.contains("missing or wrong"));
                assert!(!error.contains("grant.fixture"));
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn epoch_slot_staging_uses_the_project_grant_before_a_new_proof_exists() {
        use std::io::{Read, Write};
        let (listener, address) = test_listener();
        let epoch = "01a12290-0000-7000-8000-000000000008";
        let server = std::thread::spawn(move || {
            for (path, bearer) in [
                (format!("project/epochs/{epoch}/slots/machine-test.age"), "project.grant"),
                ("slots/machine-test.age".into(), "old.read.proof"),
                (format!("project/epochs/{epoch}/history/root.age"), "project.grant"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut bytes = Vec::new();
                let mut part = [0; 2048];
                while !bytes.windows(4).any(|p| p == b"\r\n\r\n") {
                    let count = stream.read(&mut part).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&part[..count]);
                }
                let request = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
                assert!(request.starts_with(&format!("put /v1/p/project/objects/{path} ")));
                assert!(request.contains(&format!("authorization: bearer {bearer}\r\n")));
                write!(stream, "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.token = Some("project.grant".into());
        *gateway.read.lock().unwrap() = Some(("old.read.proof".into(), crate::ids::now_ms() + 300_000));
        for key in [
            format!("project/epochs/{epoch}/slots/machine-test.age"),
            "slots/machine-test.age".into(),
            format!("project/epochs/{epoch}/history/root.age"),
        ] {
            assert!(gateway.put_new(&key, b"sealed object".to_vec()).unwrap());
        }
        server.join().unwrap();
    }

    #[test]
    fn explicit_public_access_never_opens_person_keys() {
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        let env = std::collections::HashMap::from([("LAYR_PERSON_KEY_FILE".into(), "/missing/person-key".into())]);
        gateway.set_person("ann@example.com", ed25519_dalek::SigningKey::from_bytes(&[7; 32])).unwrap();
        gateway.connect_public(&env).unwrap();
        assert!(gateway.person.lock().unwrap().is_none());
    }

    #[test]
    fn reader_hooks_ignore_write_grants_set_identity_and_check_the_ca_file() {
        use super::super::backend::Backend;
        let mut env = std::collections::HashMap::from([("LAYR_GATEWAY_TOKEN_FILE".into(), "/missing/write-token".into())]);
        let backend = Backend::for_reader("layr+https://localhost/example", &env).unwrap();
        backend.unlock_reader("reader-test-identity").unwrap();
        let Backend::Gateway(gateway) = backend else { unreachable!() };
        assert!(gateway.token.is_none());
        assert!(gateway.identity.lock().unwrap().as_deref() == Some("reader-test-identity"));
        env.insert("LAYR_GATEWAY_CA_FILE".into(), "/missing/reader-ca".into());
        assert!(Backend::for_reader("layr+https://localhost/example", &env).is_err());
    }

    #[test]
    fn urls_refuse_credentials_queries_and_path_escapes() {
        assert!(Gateway::open("layr+https://cloud.example/project").is_ok());
        for url in [
            "layr+https://u:secret@cloud.example/p",
            "layr+https://cloud.example/p?token=secret",
            "layr+https://cloud.example/p#fragment",
            "layr+https://cloud.example/a/b",
            "layr+https://cloud.example/%2e%2e",
            "layr+https://cloud.example/",
        ] {
            let error = Gateway::open(url).err().expect("invalid URL").to_string();
            assert!(!error.contains("secret"));
        }
    }
    #[test]
    fn token_files_require_owner_private_mode_and_regular_no_follow_file() {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "v1.write.123.abc\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        assert_eq!(token_file(&path, uid).unwrap(), "v1.write.123.abc");
        assert!(token_file(&path, uid.wrapping_add(1)).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(token_file(&path, uid).is_err());
        symlink(&path, dir.join("link")).unwrap();
        assert!(token_file(&dir.join("link"), uid).is_err());
        assert!(token_file(&dir, uid).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A fake gateway that issues read proofs. `answer` gets "METHOD path" and the bearer of each object request and
    /// returns its status and body. It stops at a "GET /quit" request and returns the log of all requests before it.
    fn proof_gateway(
        recipient: String,
        mut answer: impl FnMut(&str, &str) -> (&'static str, String) + Send + 'static,
    ) -> (TestEndpoint, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let (listener, address) = test_listener();
        let server = std::thread::spawn(move || {
            let mut log = Vec::new();
            let mut issued = 0;
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let head = read_head(&mut stream);
                let head = String::from_utf8(head).unwrap();
                let lower = head.to_ascii_lowercase();
                let length: usize =
                    lower.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                let line: Vec<&str> = head.lines().next().unwrap().split(' ').collect();
                let request = format!("{} {}", line[0], line[1].split('?').next().unwrap());
                if request == "GET /quit" {
                    return log;
                }
                let bearer = head
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: Bearer ").or_else(|| l.strip_prefix("Authorization: Bearer ")))
                    .unwrap_or("-")
                    .trim()
                    .to_string();
                let cdn = if lower.lines().any(|l| l.trim() == "x-layr-cdn: 1") { " cdn" } else { "" };
                log.push(format!("{request} {bearer}{cdn}"));
                let (status, reply) = match request.as_str() {
                    "GET /v1/p/project/challenge" => {
                        let sealed = keys::encrypt(&recipient, b"challenge.proof").unwrap();
                        (
                            "200 OK",
                            serde_json::json!({"challenge": base64::engine::general_purpose::STANDARD.encode(sealed)})
                                .to_string(),
                        )
                    }
                    "POST /v1/p/project/token" => {
                        issued += 1;
                        let expires = crate::ids::now_ms() + 600_000;
                        ("200 OK", serde_json::json!({"token": format!("new.proof{issued}"), "expires": expires}).to_string())
                    }
                    _ => answer(&request, &bearer),
                };
                // A reply that names a signed URL has the CDN type, as layr-gateway `cdn.rs` sends it.
                let kind =
                    if reply.starts_with("{\"url\"") { format!("Content-Type: {CDN_CONTENT_TYPE}\r\n") } else { String::new() };
                // A client that stops at its size bound closes before the reply is all sent: the write fails then.
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\n{kind}Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (address, server)
    }

    /// A client with a read identity and a cached proof that expires in `expires_in` ms.
    fn proof_client(address: &TestEndpoint, identity: &str, expires_in: i64) -> Gateway {
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        address.configure(&mut gateway);
        gateway.set_identity(identity).unwrap();
        *gateway.read.lock().unwrap() = Some(("old.proof".into(), crate::ids::now_ms() + expires_in));
        gateway
    }

    fn quit(address: &TestEndpoint, server: std::thread::JoinHandle<Vec<String>>) -> Vec<String> {
        use std::io::Write;
        let mut stream = address.connect().unwrap();
        write!(stream, "GET /quit HTTP/1.1\r\n\r\n").unwrap();
        server.join().unwrap()
    }

    const LISTING: &str = r#"{"objects":[],"continuation":null}"#;

    #[test]
    fn a_read_proof_that_expires_during_a_push_is_renewed_before_the_request() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |_, bearer| {
            if bearer == "new.proof1" {
                ("200 OK", LISTING.into())
            } else {
                ("401 Unauthorized", "read proof expired".into())
            }
        });
        // 30 s is left: within the renew margin, as for a proof that a long push took 9 minutes ago.
        let gateway = proof_client(&address, &identity, 30_000);
        assert!(gateway.list("states/").unwrap().is_empty());
        let log = quit(&address, server);
        assert_eq!(
            log,
            ["GET /v1/p/project/challenge -", "POST /v1/p/project/token -", "GET /v1/p/project/objects new.proof1"],
            "the proof is renewed before it goes out, so the gateway never sees an expired proof"
        );
    }

    #[test]
    fn events_long_poll_with_the_read_proof_of_the_client() -> Result<()> {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity)?, |request, bearer| {
            if request == "GET /v1/p/project/events" && bearer == "old.proof" {
                ("200 OK", r#"{"project":"project","events":[],"cursor":"1.2.3"}"#.into())
            } else {
                ("404 Not Found", "{}".into())
            }
        });
        let gateway = proof_client(&address, &identity, 600_000);
        assert_eq!(gateway.events(Some("1.1.3"), 99)?["cursor"], "1.2.3");
        assert_eq!(quit(&address, server), ["GET /v1/p/project/events old.proof"]);
        Ok(())
    }

    #[test]
    fn an_events_retry_uses_the_remaining_request_deadline() -> Result<()> {
        let identity = keys::generate_identity();
        let mut attempts = 0;
        let (address, server) = proof_gateway(keys::recipient_of(&identity)?, move |_, bearer| {
            if bearer == "old.proof" {
                return ("401 Unauthorized", "{}".into());
            }
            attempts += 1;
            if attempts == 1 {
                ("503 Service Unavailable", "{}".into())
            } else {
                // The reply would fit a fresh 1 s timeout, but not after the 0.5 s retry delay.
                std::thread::sleep(Duration::from_millis(800));
                ("200 OK", r#"{"events":[],"cursor":"1.2.3"}"#.into())
            }
        });
        let mut gateway = proof_client(&address, &identity, 600_000);
        let started = Instant::now();
        gateway.deadline = Some(started + Duration::from_secs(1));
        let result = gateway.events(Some("1.1.3"), 1);
        assert!(result.as_ref().is_err_and(unreachable), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "elapsed: {:?}", started.elapsed());
        assert_eq!(
            quit(&address, server),
            [
                "GET /v1/p/project/events old.proof",
                "GET /v1/p/project/challenge -",
                "POST /v1/p/project/token -",
                "GET /v1/p/project/events new.proof1",
                "GET /v1/p/project/events new.proof1",
            ]
        );
        Ok(())
    }

    #[test]
    fn a_first_events_request_caps_retry_after_by_the_deadline() -> Result<()> {
        use std::io::Write;
        let (listener, endpoint) = test_listener();
        let server = std::thread::spawn(move || -> Result<_> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            let head = String::from_utf8(read_head(&mut stream))?;
            write!(stream, "HTTP/1.1 503 Busy\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            Ok(head)
        });
        let mut gateway = Gateway::open("layr+https://localhost/project")?;
        endpoint.configure(&mut gateway);
        let started = Instant::now();
        gateway.deadline = Some(started + Duration::from_secs(1));
        let result = gateway.clone().events(None, 0);
        let head = server.join().map_err(|_| anyhow!("server failed"))??;
        assert!(head.starts_with("GET /v1/p/project/events?wait=0 HTTP/"), "{head}");
        assert!(result.as_ref().is_err_and(|e| e.to_string().contains("deadline reached")), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "elapsed: {:?}", started.elapsed());
        Ok(())
    }

    #[test]
    fn only_a_finite_wait_classifies_a_stalled_events_body_as_timed_out() -> Result<()> {
        use std::io::Write;
        for finite in [false, true] {
            let (listener, endpoint) = test_listener();
            let (release, stall) = std::sync::mpsc::channel::<()>();
            let server = std::thread::spawn(move || -> Result<()> {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let _ = read_head(&mut stream);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 48\r\nConnection: close\r\n\r\n{{")?;
                let _ = stall.recv_timeout(Duration::from_secs(3));
                Ok(())
            });
            let mut gateway = Gateway::open("layr+https://localhost/project")?;
            endpoint.configure(&mut gateway);
            let budget = Duration::from_millis(250);
            let mut builder = Client::builder().no_proxy().timeout(budget);
            if let TestEndpoint::Unix(path) = &endpoint {
                builder = builder.unix_socket(path.clone());
            }
            gateway.http = builder.build()?;
            gateway.deadline = finite.then(|| Instant::now() + budget);
            let result = gateway.events(Some("1.1.3"), 1);
            drop(release);
            server.join().map_err(|_| anyhow!("server failed"))??;
            let Err(error) = result else { bail!("expected a body read failure") };
            assert_eq!(format!("{error:#}"), "gateway events: gateway body read failed");
            assert_eq!(
                error.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
                finite.then_some(std::io::ErrorKind::TimedOut)
            );
        }
        Ok(())
    }

    #[test]
    fn a_401_on_a_listing_renews_the_proof_once_and_sends_again() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |_, bearer| {
            if bearer == "new.proof1" {
                ("200 OK", LISTING.into())
            } else {
                ("401 Unauthorized", "read proof expired".into())
            }
        });
        // The client clock runs behind the gateway: the client sees 5 minutes left, the gateway sees an expired proof.
        let gateway = proof_client(&address, &identity, 300_000);
        assert!(gateway.list("states/").unwrap().is_empty());
        let log = quit(&address, server);
        assert_eq!(
            log,
            [
                "GET /v1/p/project/objects old.proof",
                "GET /v1/p/project/challenge -",
                "POST /v1/p/project/token -",
                "GET /v1/p/project/objects new.proof1"
            ]
        );
    }

    #[test]
    fn a_401_on_a_proof_write_renews_the_proof_once_and_sends_again() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |request, bearer| {
            assert_eq!(request, "PUT /v1/p/project/objects/slots/machine-test.age");
            if bearer == "new.proof1" {
                ("201 Created", String::new())
            } else {
                ("401 Unauthorized", "read proof expired".into())
            }
        });
        let gateway = proof_client(&address, &identity, 300_000);
        assert!(gateway.put_new("slots/machine-test.age", b"sealed slot".to_vec()).unwrap());
        let log = quit(&address, server);
        assert_eq!(
            log,
            [
                "PUT /v1/p/project/objects/slots/machine-test.age old.proof",
                "GET /v1/p/project/challenge -",
                "POST /v1/p/project/token -",
                "PUT /v1/p/project/objects/slots/machine-test.age new.proof1"
            ]
        );
    }

    #[test]
    fn a_gateway_refusing_the_batch_route_with_401_is_read_object_by_object() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |request, _| {
            if request.ends_with("/batch") {
                ("401 Unauthorized", "no session".into())
            } else {
                ("200 OK", "x".into())
            }
        });
        let gateway = proof_client(&address, &identity, 300_000);
        let keys = ["states/ab".to_string(), "states/cd".to_string()];
        let backend = super::super::backend::Backend::Gateway(gateway);
        assert_eq!(backend.get_many(&keys).unwrap(), vec![Some(b"x".to_vec()); 2]);
        backend.get_many(&keys[..1]).unwrap();
        let log = quit(&address, server);
        // One refused probe with its proof renewal, then no further batch requests.
        assert_eq!(log.iter().filter(|line| line.contains("/batch")).count(), 2, "{log:?}");
        assert_eq!(log.iter().filter(|line| line.contains("/objects/states/")).count(), 3, "{log:?}");
    }

    #[test]
    fn a_batch_route_refused_after_it_worked_is_read_object_by_object() {
        let identity = keys::generate_identity();
        let batches = std::sync::atomic::AtomicUsize::new(0);
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), move |request, _| {
            if !request.ends_with("/batch") {
                ("200 OK", "x".into())
            } else if batches.fetch_add(1, Ordering::SeqCst) == 0 {
                let envelope = transport_tests::envelope(vec![(200, b"x".to_vec())]);
                ("200 OK", String::from_utf8(envelope).unwrap())
            } else {
                // A rolling deploy: the next gateway refuses the route, also with a renewed proof.
                ("401 Unauthorized", "no session".into())
            }
        });
        let backend = super::super::backend::Backend::Gateway(proof_client(&address, &identity, 300_000));
        assert_eq!(backend.get_many(&["states/ab".to_string()]).unwrap(), vec![Some(b"x".to_vec())]);
        assert_eq!(backend.get_many(&["states/cd".to_string()]).unwrap(), vec![Some(b"x".to_vec())]);
        let log = quit(&address, server);
        assert_eq!(log.iter().filter(|line| line.contains("/objects/states/cd")).count(), 1, "{log:?}");
    }

    #[test]
    fn a_second_401_after_a_renewal_fails_clearly() {
        let identity = keys::generate_identity();
        let (address, server) =
            proof_gateway(keys::recipient_of(&identity).unwrap(), |_, _| ("401 Unauthorized", "read proof expired".into()));
        let gateway = proof_client(&address, &identity, 300_000);
        let error = format!("{:#}", gateway.list("states/").unwrap_err());
        let log = quit(&address, server);
        assert!(error.contains("HTTP 401 again with a new read proof") && error.contains("read proof expired"), "{error}");
        assert_eq!(log.len(), 4, "one renewal and one more try, no loop: {log:?}");
    }

    #[test]
    fn a_404_with_a_read_proof_is_not_sent_again() {
        let identity = keys::generate_identity();
        let (address, server) =
            proof_gateway(keys::recipient_of(&identity).unwrap(), |_, _| ("404 Not Found", "no such project".into()));
        let gateway = proof_client(&address, &identity, 300_000);
        let error = format!("{:#}", gateway.get("states/ab").unwrap_err());
        let log = quit(&address, server);
        assert!(error.contains("no such project"), "{error}");
        assert_eq!(log, ["GET /v1/p/project/objects/states/ab old.proof cdn"], "a 404 does not renew the proof or send again");
    }

    pub(crate) struct FileTransport {
        endpoint: TestEndpoint,
        server: Option<std::thread::JoinHandle<Vec<(String, serde_json::Value)>>>,
    }
    #[allow(clippy::expect_used)]
    impl FileTransport {
        pub(crate) fn finish(mut self) -> Vec<(String, serde_json::Value)> {
            transport_tests::stop_transport(&self.endpoint, self.server.take().expect("test peer is running"))
        }
    }
    impl Drop for FileTransport {
        fn drop(&mut self) {
            if let Some(server) = self.server.take() {
                transport_tests::stop_transport(&self.endpoint, server);
            }
        }
    }
    /// Reuse the binary peer and TCP/Unix selection for reader and end-to-end tests.
    pub(crate) fn file_transport(
        files: crate::store_remote::backend::Backend,
    ) -> (crate::store_remote::backend::Backend, FileTransport) {
        file_transport_with_status(files, |_, _| None)
    }

    pub(in crate::store_remote) fn file_transport_with_status(
        files: crate::store_remote::backend::Backend,
        status: impl Fn(&str, Option<&str>) -> Option<u16> + Send + 'static,
    ) -> (crate::store_remote::backend::Backend, FileTransport) {
        file_transport_with_batch_trailer(files, status, &[])
    }

    #[allow(clippy::unwrap_used)]
    pub(in crate::store_remote) fn file_transport_with_batch_trailer(
        files: crate::store_remote::backend::Backend,
        status: impl Fn(&str, Option<&str>) -> Option<u16> + Send + 'static,
        trailer: &'static [u8],
    ) -> (crate::store_remote::backend::Backend, FileTransport) {
        let (mut gateway, endpoint, server) = transport_tests::transport_fixture(move |request, value, body| {
            if let Some(key) = request.strip_prefix("put /v1/p/project/objects/") {
                let created = files.put_new(key, body.to_vec()).unwrap();
                return (if created { 201 } else { 412 }, Vec::new());
            }
            if request.ends_with("/batch") {
                if let Some(status) = status("batch", None) {
                    return (status, vec![]);
                }
                let items = value["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|name| {
                        let name = name.as_str().unwrap();
                        if let Some(status) = status("batch", Some(name)) {
                            return (status, vec![]);
                        }
                        files.get(name).unwrap().map_or((404, vec![]), |body| (200, body))
                    })
                    .collect();
                let mut reply = transport_tests::envelope(items);
                reply.extend_from_slice(trailer);
                return (200, reply);
            }
            let path = request.split_whitespace().nth(1).unwrap();
            let path = path.strip_prefix("/v1/p/project/").unwrap();
            let (action, query) = path.split_once('?').unwrap_or((path, ""));
            if action == "objects" {
                let prefix = reqwest::Url::parse(&format!("http://localhost/?{query}"))
                    .unwrap()
                    .query_pairs()
                    .find(|(key, _)| key == "prefix")
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                let objects = files
                    .list(&prefix)
                    .unwrap()
                    .into_iter()
                    .map(|o| serde_json::json!({"key":o.key,"modified":o.modified.saturating_mul(1000),"size":o.size}))
                    .collect::<Vec<_>>();
                return (200, serde_json::to_vec(&serde_json::json!({"objects":objects,"continuation":null})).unwrap());
            }
            if action == "cdn" {
                if let Some(status) = status("cdn", None) {
                    let body = if status == 200 { b"[]".to_vec() } else { Vec::new() };
                    return (status, body);
                }
                let items: Vec<_> =
                    value["keys"].as_array().unwrap().iter().map(|key| status("cdn", Some(key.as_str().unwrap()))).collect();
                if items.iter().any(Option::is_some) {
                    let objects: Vec<_> = items.iter().map(|&item| serde_json::json!({"status":item.unwrap_or(501)})).collect();
                    return (200, serde_json::to_vec(&serde_json::json!({"objects":objects})).unwrap());
                }
                return (404, b"unsupported".to_vec());
            }
            let name = action.strip_prefix("objects/").unwrap_or(action);
            files.get(name).unwrap().map_or((404, vec![]), |body| (200, body))
        });
        gateway.token = Some("fixture.write".into());
        (crate::store_remote::backend::Backend::Gateway(gateway), FileTransport { endpoint, server: Some(server) })
    }

    /// `file_transport` for a machine with a write grant and no read identity: it gets no read proof.
    #[allow(clippy::unwrap_used)]
    pub(crate) fn write_only_transport(
        files: crate::store_remote::backend::Backend,
    ) -> (crate::store_remote::backend::Backend, FileTransport) {
        let (backend, transport) = file_transport(files);
        if let crate::store_remote::backend::Backend::Gateway(gateway) = &backend {
            *gateway.identity.lock().unwrap() = None;
            *gateway.read.lock().unwrap() = None;
        }
        (backend, transport)
    }

    #[allow(clippy::unwrap_used)]
    pub(super) mod transport_tests {
        use super::*;
        // Unlike proof_gateway this fixture accepts arbitrary binary bodies and logs decoded POST requests.
        pub(super) fn transport_fixture(
            answer: impl Fn(&str, &serde_json::Value, &[u8]) -> (u16, Vec<u8>) + Send + 'static,
        ) -> (Gateway, TestEndpoint, std::thread::JoinHandle<Vec<(String, serde_json::Value)>>) {
            transport_fixture_with_length(answer, 0)
        }

        fn transport_fixture_with_length(
            answer: impl Fn(&str, &serde_json::Value, &[u8]) -> (u16, Vec<u8>) + Send + 'static,
            extra_length: usize,
        ) -> (Gateway, TestEndpoint, std::thread::JoinHandle<Vec<(String, serde_json::Value)>>) {
            use std::io::{Read, Write};
            let (listener, endpoint) = test_listener();
            let server = std::thread::spawn(move || {
                let mut requests = Vec::new();
                loop {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let head = read_head(&mut stream);
                    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
                    let request = head.lines().next().unwrap().rsplit_once(' ').unwrap().0.to_string();
                    if request == "get /quit" {
                        return requests;
                    }
                    let length: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|s| s.trim().parse().unwrap()))
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).unwrap();
                    let value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    let (status, reply) = answer(&request, &value, &body);
                    requests.push((request, value));
                    write!(
                        stream,
                        "HTTP/1.1 {status} Reply\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.len() + extra_length
                    )
                    .unwrap();
                    stream.write_all(&reply).unwrap();
                }
            });
            let identity = keys::generate_identity();
            (proof_client(&endpoint, &identity, 300_000), endpoint, server)
        }

        #[test]
        fn transport_public_reads_never_probe_authenticated_batch_routes() {
            let (gateway, endpoint, server) = transport_fixture(|request, _, _| {
                assert!(request.starts_with("get /p/project/records/"));
                (200, b"public".to_vec())
            });
            gateway.use_public_access();
            let keys = (1..=8)
                .map(|i| crate::store_remote::format::segment_name("01a12290-0000-7000-8000-000000000008", i, i))
                .collect::<Vec<_>>();
            assert_eq!(
                crate::store_remote::backend::Backend::Gateway(gateway.clone()).get_many(&keys).unwrap(),
                vec![Some(b"public".to_vec()); 8]
            );
            let log = stop_transport(&endpoint, server);
            assert_eq!(log.len(), 8);
            assert_eq!(gateway.transfer_stats().downloaded, 48);
        }

        #[test]
        fn transport_old_gateway_is_probed_once_across_multiple_batches_and_calls() {
            let (gateway, endpoint, server) = transport_fixture(|request, _, _| {
                if request.ends_with("/batch") {
                    (404, b"no such route".to_vec())
                } else {
                    (200, b"x".to_vec())
                }
            });
            let keys = (0..257).map(|i| format!("states/{i}")).collect::<Vec<_>>();
            assert_eq!(
                crate::store_remote::backend::Backend::Gateway(gateway.clone()).get_many(&keys).unwrap(),
                vec![Some(b"x".to_vec()); 257]
            );
            crate::store_remote::backend::Backend::Gateway(gateway.clone()).get_many(&keys[..1]).unwrap();
            let log = stop_transport(&endpoint, server);
            assert_eq!(log.len(), 259);
            assert_eq!(log.iter().filter(|(r, _)| r.ends_with("/batch")).count(), 1);
        }

        #[test]
        fn transport_retries_git_manifests_directly_after_a_batch_deadline_or_transient_item() {
            for deadline in [false, true] {
                let batches = std::sync::atomic::AtomicUsize::new(0);
                let gets = std::sync::atomic::AtomicUsize::new(0);
                let (gateway, endpoint, server) = transport_fixture(move |request, _, _| {
                    if request.ends_with("/batch") {
                        match batches.fetch_add(1, Ordering::Relaxed) {
                            0 => (502, b"busy".to_vec()),
                            1 if deadline => (408, b"deadline".to_vec()),
                            1 => (200, envelope(vec![(503, vec![])])),
                            _ => (200, envelope(vec![(200, b"manifest".to_vec())])),
                        }
                    } else if request.ends_with("/cdn") {
                        (200, br#"{"objects":[{"status":503}]}"#.to_vec())
                    } else {
                        assert!(request.rsplit_once("/objects/").is_some_and(|(_, key)| is_manifest(key)));
                        if gets.fetch_add(1, Ordering::Relaxed) == 0 {
                            (502, b"busy".to_vec())
                        } else {
                            (200, b"manifest".to_vec())
                        }
                    }
                });
                let backend = crate::store_remote::backend::Backend::Gateway(gateway);
                let key = "git/fixture.commits.age".to_owned();
                let mut got = None;
                let result = backend.visit_many(std::slice::from_ref(&key), &[Some(8)], |_, body| {
                    got = body;
                    Ok(())
                });
                let next = backend.get_many(std::slice::from_ref(&key));
                let log = stop_transport(&endpoint, server);
                result.unwrap();
                assert_eq!(got.as_deref(), Some(b"manifest".as_slice()));
                assert_eq!(next.unwrap(), [Some(b"manifest".to_vec())]);
                assert_eq!(
                    log.iter().map(|(request, _)| request.as_str()).collect::<Vec<_>>(),
                    [
                        "post /v1/p/project/batch",
                        "post /v1/p/project/batch",
                        &format!("get /v1/p/project/objects/{key}"),
                        &format!("get /v1/p/project/objects/{key}"),
                        "post /v1/p/project/batch",
                    ]
                );
                assert_eq!(backend.transfer_stats().requests, 5);
            }
        }

        #[test]
        fn transport_retries_a_manifest_after_batch_overflow_and_a_transient_cdn_item() {
            for cdn_deadline in [false, true] {
                let gets = std::sync::atomic::AtomicUsize::new(0);
                let (gateway, endpoint, server) = transport_fixture(move |request, _, _| {
                    if request.ends_with("/batch") {
                        (200, envelope(vec![(507, vec![])]))
                    } else if request.ends_with("/cdn") {
                        if cdn_deadline {
                            (408, vec![])
                        } else {
                            (200, br#"{"objects":[{"status":503}]}"#.to_vec())
                        }
                    } else if cdn_deadline && gets.fetch_add(1, Ordering::Relaxed) == 0 {
                        (408, vec![])
                    } else {
                        (200, b"manifest".to_vec())
                    }
                });
                let key = "git/test.commits.age".to_owned();
                let backend = crate::store_remote::backend::Backend::Gateway(gateway);
                let result = backend.get_many(std::slice::from_ref(&key));
                let log = stop_transport(&endpoint, server);
                assert_eq!(result.unwrap(), [Some(b"manifest".to_vec())]);
                let get = format!("get /v1/p/project/objects/{key}");
                let mut expected = vec!["post /v1/p/project/batch", "post /v1/p/project/cdn", &get];
                if cdn_deadline {
                    expected.push(&get);
                }
                assert_eq!(log.iter().map(|(request, _)| request.as_str()).collect::<Vec<_>>(), expected);
            }
        }

        #[test]
        fn a_release_filename_with_a_manifest_suffix_keeps_ordinary_get_and_size_rules() {
            let gets = std::sync::atomic::AtomicUsize::new(0);
            let (gateway, endpoint, server) = transport_fixture(move |request, _, _| {
                assert!(request.ends_with("public/release/v1.2.3/archive.commits.age"));
                if gets.fetch_add(1, Ordering::Relaxed) == 0 {
                    (408, vec![])
                } else {
                    (200, b"release".to_vec())
                }
            });
            let key = "public/release/v1.2.3/archive.commits.age".to_owned();
            let got = gateway.get(&key);
            let backend = crate::store_remote::backend::Backend::Gateway(gateway);
            let oversized = backend.visit_many(&[key], &[Some(33 << 20)], |_, _| Ok(()));
            let log = stop_transport(&endpoint, server);
            assert!(got.is_err_and(|error| error.to_string().contains("408")));
            assert!(oversized.is_err_and(|error| error.to_string().contains("invalid download size bounds")));
            assert_eq!(log.len(), 1);
        }

        #[test]
        fn transport_late_batch_body_failure_preserves_completed_manifests() {
            for (first, length, expected, bytes) in [
                ("git/a.commits.age", Some(6), "short batch body", &b"firstsec"[..]),
                ("git/a.commits.age", None, "batch entry has no length", &b"first"[..]),
                ("git/a.commits.age", Some(OBJECT_LIMIT), "batch bodies exceed 32 MiB", &b"first"[..]),
                ("states/a", Some(6), "short batch body", &b"firstsec"[..]),
            ] {
                let (gateway, endpoint, server) = transport_fixture(move |request, _, _| {
                    assert!(request.ends_with("/batch"));
                    let index = serde_json::to_vec(&serde_json::json!({"objects": [
                        {"status":200,"length":5}, {"status":200,"length":length}, {"status":200,"length":5}
                    ]}))
                    .unwrap();
                    let mut body = u32::try_from(index.len()).unwrap().to_be_bytes().to_vec();
                    body.extend(index);
                    body.extend_from_slice(bytes);
                    (200, body)
                });
                let backend = crate::store_remote::backend::Backend::Gateway(gateway);
                let names = [first.into(), "git/sub/b.commits.age".into(), "git/sub/c.commits.age".into()];
                let mut bodies: Vec<_> = (0..names.len()).map(|_| None).collect();
                let result = backend.visit_many_results(&names, &[None; 3], |i, body| {
                    bodies[i] = Some(body.map_err(|error| error.to_string()));
                    Ok(())
                });
                let log = stop_transport(&endpoint, server);
                if first == "states/a" {
                    assert_eq!(result.unwrap_err().to_string(), expected);
                    assert!(bodies.iter().all(Option::is_none));
                } else {
                    result.unwrap();
                    assert_eq!(bodies[0], Some(Ok(Some(b"first".to_vec()))));
                    assert_eq!(bodies[1], Some(Err(expected.into())));
                    assert_eq!(bodies[2], Some(Err(expected.into())));
                }
                assert_eq!(backend.transfer_stats().downloaded, bytes.len() as u64);
                assert_eq!(log.len(), 1);
            }
        }

        #[test]
        fn transport_batch_deadline_stops_ordinary_reads() {
            for keys in [vec!["states/a"], vec!["records/a"], vec!["states/a", "git/a.commits.age"]] {
                let (gateway, endpoint, server) =
                    transport_fixture(
                        |request, _, _| {
                            if request.ends_with("/batch") {
                                (408, vec![])
                            } else {
                                (200, b"object".to_vec())
                            }
                        },
                    );
                let backend = crate::store_remote::backend::Backend::Gateway(gateway);
                let keys: Vec<_> = keys.into_iter().map(str::to_owned).collect();
                let result = backend.get_many(&keys);
                let log = stop_transport(&endpoint, server);
                assert_eq!(result.unwrap_err().to_string(), "gateway batch: HTTP 408");
                assert_eq!(log.len(), 1);
                assert!(log[0].0.ends_with("/batch"));
            }
        }

        #[test]
        fn transport_manifest_batch_eof_error_follows_decoded_items() {
            for first in ["git/a.commits.age", "states/a"] {
                let (gateway, endpoint, server) =
                    transport_fixture_with_length(|_, _, _| (200, envelope(vec![(200, b"first".to_vec()), (403, vec![])])), 1);
                let backend = crate::store_remote::backend::Backend::Gateway(gateway);
                let keys = [first.into(), "git/b.commits.age".into()];
                let mut bodies = [None, None];
                let result = backend.visit_many_results(&keys, &[None; 2], |i, body| {
                    bodies[i] = Some(body.map_err(|error| error.to_string()));
                    Ok(())
                });
                let log = stop_transport(&endpoint, server);
                if first == "states/a" {
                    assert_eq!(bodies, [None, None]);
                } else {
                    assert_eq!(bodies, [Some(Ok(Some(b"first".to_vec()))), Some(Err("gateway get: HTTP 403".into()))]);
                }
                assert!(result.is_err());
                assert_eq!(log.len(), 1);
            }
        }

        #[test]
        fn transport_manifest_missing_project_matches_get_without_fallback() {
            for (cdn, missing) in [(false, true), (true, true), (false, false), (true, false)] {
                let (gateway, endpoint, server) = transport_fixture(move |request, _, _| {
                    let item = if missing {
                        serde_json::json!({"status":404,"error":"no such project"})
                    } else {
                        serde_json::json!({"status":404})
                    };
                    if request.ends_with("/batch") && cdn {
                        (200, envelope(vec![(507, vec![])]))
                    } else if request.ends_with("/batch") {
                        let index = serde_json::to_vec(&serde_json::json!({"objects":[item]})).unwrap();
                        let mut body = u32::try_from(index.len()).unwrap().to_be_bytes().to_vec();
                        body.extend(index);
                        (200, body)
                    } else if request.ends_with("/cdn") {
                        (200, serde_json::to_vec(&serde_json::json!({"objects":[item]})).unwrap())
                    } else if missing {
                        (404, b"no such project".to_vec())
                    } else {
                        (404, vec![])
                    }
                });
                let key = "git/a.commits.age";
                let expected = gateway.get(key).map_err(|error| error.to_string());
                let backend = crate::store_remote::backend::Backend::Gateway(gateway);
                let mut item = None;
                let result = backend.visit_many_results(&[key.into()], &[None], |_, body| {
                    item = Some(body.map_err(|error| error.to_string()));
                    Ok(())
                });
                let log = stop_transport(&endpoint, server);
                result.unwrap();
                assert_eq!(item, Some(expected));
                assert_eq!(log.len(), 2 + usize::from(cdn) + usize::from(cdn && !missing));
            }
        }

        #[test]
        fn transport_manifest_route_denial_matches_get() {
            let (gateway, endpoint, server) = transport_fixture(|_, _, _| (403, vec![]));
            let key = "overlays/fixture/epochs/version/git/sub/pack.commits.age";
            let direct = gateway.get(key);
            let backend = crate::store_remote::backend::Backend::Gateway(gateway);
            let batch = backend.get_many(&[key.into()]);
            let log = stop_transport(&endpoint, server);
            assert_eq!(direct.unwrap_err().to_string(), "gateway get: HTTP 403");
            assert_eq!(batch.unwrap_err().to_string(), "gateway get: HTTP 403");
            assert_eq!(log.len(), 2);
        }

        #[test]
        fn transport_batch_item_errors_do_not_fall_back_to_reads_or_history() {
            use crate::store_remote::backend::{Backend, EpochBackend};
            for status in [403, 503] {
                let (gateway, endpoint, server) = transport_fixture(move |request, _, _| {
                    assert!(request.ends_with("/batch"));
                    (200, envelope(vec![(status, vec![])]))
                });
                let base = Backend::Gateway(gateway);
                let backend = Backend::Epoch(Box::new(EpochBackend {
                    root: base.clone(),
                    current: base.scoped("project/epochs/01a12290-0000-7000-8000-000000000008").unwrap(),
                    history: vec![(base, keys::generate_identity())],
                }));
                assert!(backend.get_many(&["states/a".into()]).is_err());
                assert_eq!(stop_transport(&endpoint, server).len(), 1);
            }
        }

        pub(super) fn stop_transport(
            endpoint: &TestEndpoint,
            server: std::thread::JoinHandle<Vec<(String, serde_json::Value)>>,
        ) -> Vec<(String, serde_json::Value)> {
            use std::io::Write;
            let mut stream = endpoint.connect().unwrap();
            write!(stream, "GET /quit HTTP/1.1\r\n\r\n").unwrap();
            server.join().unwrap()
        }

        pub(super) fn envelope(items: Vec<(u16, Vec<u8>)>) -> Vec<u8> {
            let index = serde_json::json!({"objects":items.iter().map(|(status, body)| {
            serde_json::json!({"status":status,"length":body.len()})
        }).collect::<Vec<_>>()});
            let index = serde_json::to_vec(&index).unwrap();
            let mut out = u32::try_from(index.len()).unwrap().to_be_bytes().to_vec();
            out.extend(index);
            for (status, body) in items {
                if status == 200 {
                    out.extend(body);
                }
            }
            out
        }

        #[test]
        fn transport_257_keys_batch_through_nested_prefix_and_epoch() {
            use crate::store_remote::backend::{Backend, EpochBackend};
            let (gateway, endpoint, server) = transport_fixture(|request, value, _| {
                if request.ends_with("/batch") {
                    assert_eq!(value["epoch"], "01a12290-0000-7000-8000-000000000008");
                    let names = value["keys"].as_array().unwrap();
                    assert!(names.iter().all(|k| k.as_str().unwrap().starts_with("records/")));
                    (200, envelope(names.iter().map(|_| (200, b"x".to_vec())).collect()))
                } else {
                    (200, b"x".to_vec())
                }
            });
            let gateway = Backend::Gateway(gateway);
            let nested = gateway.scoped("project").unwrap().scoped("epochs/01a12290-0000-7000-8000-000000000008").unwrap();
            let backend = Backend::Epoch(Box::new(EpochBackend { root: gateway, current: nested, history: vec![] }));
            let keys: Vec<_> = (0..257).map(|i| format!("records/{i}")).collect();
            assert_eq!(backend.get_many(&keys).unwrap(), vec![Some(b"x".to_vec()); 257]);
            let stats = backend.transfer_stats();
            let log = stop_transport(&endpoint, server);
            assert_eq!(log.len(), 2, "257 keys require exactly two POSTs");
            assert!(log.iter().all(|(request, _)| request == "post /v1/p/project/batch"));
            assert_eq!(stats.requests, 2);
            assert_eq!(stats.downloaded, 257);
            assert_eq!(stats.uploaded, 0);
            let expected_sent: usize = log.iter().map(|(_, value)| serde_json::to_vec(value).unwrap().len()).sum();
            let expected_received: usize = log
                .iter()
                .map(|(_, value)| envelope(vec![(200, b"x".to_vec()); value["keys"].as_array().unwrap().len()]).len())
                .sum();
            let report = crate::store_remote::download::GatewayReport::from(stats);
            assert_eq!(report.request_body_bytes, expected_sent as u64);
            assert_eq!(report.response_body_bytes, expected_received as u64);
        }

        #[test]
        fn transport_epoch_partitions_root_objects_and_maps_current_claim_keys() {
            use crate::store_remote::backend::{Backend, EpochBackend};
            let (gateway, endpoint, server) = transport_fixture(|request, value, _| {
                assert!(request.ends_with("/batch"));
                (
                    200,
                    envelope(
                        value["keys"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|key| (200, key.as_str().unwrap().as_bytes().to_vec()))
                            .collect(),
                    ),
                )
            });
            let base = Backend::Gateway(gateway);
            let backend = Backend::Epoch(Box::new(EpochBackend {
                root: base.clone(),
                current: base.scoped("project/epochs/01a12290-0000-7000-8000-000000000008").unwrap(),
                history: vec![],
            }));
            let got = backend.get_many(&["requests/claims/id".into(), "git/x.pack.age".into(), "records/one".into()]).unwrap();
            assert_eq!(got, vec![Some(b"claims/id".to_vec()), Some(b"git/x.pack.age".to_vec()), Some(b"records/one".to_vec())]);
            let log = stop_transport(&endpoint, server);
            assert_eq!(log.len(), 2);
            assert_eq!(log[0].1["epoch"], "root");
            assert_eq!(log[0].1["keys"], serde_json::json!(["git/x.pack.age"]));
            assert_eq!(log[1].1["keys"], serde_json::json!(["claims/id", "records/one"]));
        }

        #[test]
        fn transport_epoch_retries_only_eligible_misses_and_decrypts_with_selected_identity() {
            use crate::store_remote::backend::{Backend, EpochBackend};
            let old = keys::generate_identity();
            let current = keys::generate_identity();
            let sealed_old = keys::encrypt(&keys::recipient_of(&old).unwrap(), b"old").unwrap();
            let sealed_current = keys::encrypt(&keys::recipient_of(&current).unwrap(), b"current").unwrap();
            let (gateway, endpoint, server) = transport_fixture(move |request, value, _| {
                assert!(request.ends_with("/batch"));
                let root = value["epoch"] == "root";
                let bodies = value["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|key| match (root, key.as_str().unwrap()) {
                        (false, "states/current") => (200, sealed_current.clone()),
                        (true, "states/old") => (200, sealed_old.clone()),
                        _ => (404, vec![]),
                    })
                    .collect();
                (200, envelope(bodies))
            });
            let base = Backend::Gateway(gateway);
            let backend = Backend::Epoch(Box::new(EpochBackend {
                root: base.clone(),
                current: base.scoped("project/epochs/01a12290-0000-7000-8000-000000000008").unwrap(),
                history: vec![(base, old)],
            }));
            let keys = vec!["states/old".into(), "states/current".into()];
            assert_eq!(backend.decrypted_many(&keys, &current).unwrap(), vec![b"old".to_vec(), b"current".to_vec()]);
            assert_eq!(backend.get_many(&["records/missing".into(), "states/missing".into()]).unwrap(), vec![None, None]);
            let log = stop_transport(&endpoint, server);
            assert_eq!(log.len(), 4);
            assert_eq!(log[1].1["keys"], serde_json::json!(["states/old"]));
            assert_eq!(log[3].1["keys"], serde_json::json!(["states/missing"]));
        }

        #[test]
        fn transport_overflow_falls_back_only_for_unresolved_objects_in_order() {
            let (gateway, endpoint, server) = transport_fixture(|request, _, _| {
                if request.ends_with("/batch") {
                    (200, envelope(vec![(200, b"a".to_vec()), (404, vec![]), (507, vec![]), (501, vec![])]))
                } else if request.ends_with("/cdn") {
                    (405, vec![])
                } else {
                    (200, request.as_bytes().last().unwrap().to_be_bytes().to_vec())
                }
            });
            let got = crate::store_remote::backend::Backend::Gateway(gateway.clone())
                .get_many(&["states/a".into(), "states/b".into(), "states/c".into(), "states/d".into()])
                .unwrap();
            assert_eq!(got, vec![Some(b"a".to_vec()), None, Some(b"c".to_vec()), Some(b"d".to_vec())]);
            let stats = gateway.transfer_stats();
            let log = stop_transport(&endpoint, server);
            assert_eq!(log.len(), 4);
            assert_eq!(stats.requests, 4);
            assert_eq!(stats.downloaded, 3);
        }

        #[test]
        fn transport_known_sizes_batch_cdn_metadata_and_reject_oversized_bodies() {
            let (gateway, endpoint, server) = transport_fixture(|request, value, _| {
                if request.ends_with("/cdn") {
                    let objects: Vec<_> = value["keys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .enumerate()
                        .map(|(i, _)| {
                            if i == 0 {
                                serde_json::json!({"status":200,"url":"https://elsewhere.example/o/untrusted","bytes":1})
                            } else {
                                serde_json::json!({"status":501})
                            }
                        })
                        .collect();
                    (200, serde_json::to_vec(&serde_json::json!({"objects":objects})).unwrap())
                } else if request.ends_with("/batch") {
                    // A gateway without the batch route: the reads go one at a time.
                    (404, Vec::new())
                } else {
                    (200, b"xx".to_vec())
                }
            });
            let backend = crate::store_remote::backend::Backend::Gateway(gateway.clone());
            let mut bodies = vec![None; 3];
            let keys = ["states/a".into(), "states/b".into(), "states/c".into()];
            backend
                .visit_decrypted_many(&keys, &[Some(2); 3], keys::PLAIN, |i, body| {
                    bodies[i] = Some(body);
                    Ok(())
                })
                .unwrap();
            assert_eq!(bodies, vec![Some(b"xx".to_vec()); 3]);
            assert!(backend.visit_decrypted_many(&["states/d".into()], &[Some(1)], keys::PLAIN, |_, _| Ok(())).is_err());
            let stats = gateway.transfer_stats();
            let log = stop_transport(&endpoint, server);
            // One refused batch (sized reads batch too), one URL batch, three direct bodies, one size-bound rejection.
            assert_eq!(log.len(), 6);
            assert_eq!(log.iter().filter(|(r, _)| r.ends_with("/batch")).count(), 1);
            assert_eq!(log.iter().filter(|(r, _)| r.ends_with("/cdn")).count(), 1);
            assert_eq!(stats.downloaded, 6);
            assert_eq!(stats.cdn_fallbacks, 1);
            assert_eq!(stats.cdn_hits, 0);
        }

        #[test]
        fn transport_fallback_has_two_body_reads_in_flight_under_the_unknown_size_budget() {
            use std::io::Write;
            let (listener, endpoint) = test_listener();
            let server = std::thread::spawn(move || {
                for _ in 0..2 {
                    // Neither request receives a response until both have arrived.
                    let mut streams = Vec::new();
                    for _ in 0..2 {
                        let (mut stream, _) = listener.accept().unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        let head = read_head(&mut stream);
                        assert!(head.starts_with(b"GET /v1/p/project/objects/states/"));
                        streams.push(stream);
                    }
                    for mut stream in streams {
                        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx").unwrap();
                    }
                }
            });
            let identity = keys::generate_identity();
            let gateway = proof_client(&endpoint, &identity, 300_000);
            gateway.batch_off.store(true, Ordering::SeqCst);
            let keys = (0..4).map(|i| format!("states/{i}")).collect::<Vec<_>>();
            assert_eq!(
                crate::store_remote::backend::Backend::Gateway(gateway.clone()).get_many(&keys).unwrap(),
                vec![Some(b"x".to_vec()); 4]
            );
            server.join().unwrap();
            assert_eq!(gateway.transfer_stats().requests, 4);
        }

        #[test]
        fn transport_parallel_reads_share_one_proof_renewal() {
            let identity = keys::generate_identity();
            let (endpoint, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |_, bearer| {
                assert_eq!(bearer, "new.proof1");
                ("200 OK", "x".into())
            });
            let gateway = proof_client(&endpoint, &identity, 30_000);
            gateway.batch_off.store(true, Ordering::SeqCst);
            let keys = (0..16).map(|i| format!("states/{i}")).collect::<Vec<_>>();
            assert_eq!(
                crate::store_remote::backend::Backend::Gateway(gateway.clone()).get_many(&keys).unwrap(),
                vec![Some(b"x".to_vec()); 16]
            );
            let log = quit(&endpoint, server);
            assert_eq!(log.iter().filter(|r| r.starts_with("GET /v1/p/project/challenge")).count(), 1);
            assert_eq!(log.iter().filter(|r| r.starts_with("POST /v1/p/project/token")).count(), 1);
            assert_eq!(gateway.transfer_stats().requests, 18);
        }
    }

    #[test]
    fn a_body_at_the_download_limit_is_read_without_a_larger_allocation() {
        // The download scheduler charges the limit; Vec doubling would hold twice that.
        let identity = keys::generate_identity();
        let limit = 1 << 20;
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), move |request, _| {
            assert!(request.starts_with("GET "), "{request}");
            ("200 OK", "x".repeat(limit))
        });
        let gateway = proof_client(&address, &identity, 300_000);
        let body = gateway.get_limited("states/ab", limit).unwrap().unwrap();
        assert_eq!(body.len(), limit);
        assert!(body.capacity() <= limit + 1, "capacity {}", body.capacity());
        quit(&address, server);
    }

    /// A read within a bound smaller than the gateway's ceiling (a chunk pack) stops at that bound.
    #[test]
    fn a_read_within_a_bound_takes_in_no_more_than_the_bound() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), move |request, _| {
            assert!(request.starts_with("GET "), "{request}");
            ("200 OK", "x".repeat(1 << 20))
        });
        let gateway = proof_client(&address, &identity, 300_000);
        let backend = crate::store_remote::backend::Backend::Gateway(gateway.clone());
        assert!(backend.get_within("states/ab", Some(1 << 10)).is_err());
        let downloaded = gateway.transfer_stats().downloaded;
        assert!(downloaded <= (1 << 10) + 1, "{downloaded} bytes read");
        quit(&address, server);
    }

    #[test]
    fn get_many_reads_the_objects_in_one_batch_request() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |request, _| {
            assert_eq!(request, "POST /v1/p/project/batch");
            let index = r#"{"objects":[{"status":200,"length":3},{"status":404,"error":"no such object"}]}"#;
            let mut reply = String::new();
            reply.push_str(std::str::from_utf8(&(index.len() as u32).to_be_bytes()).unwrap());
            reply.push_str(index);
            reply.push_str("abc");
            ("200 OK", reply)
        });
        let gateway = proof_client(&address, &identity, 300_000);
        let got = crate::store_remote::backend::Backend::Gateway(gateway.clone())
            .get_many(&["states/ab".into(), "states/cd".into()])
            .unwrap();
        assert_eq!(got, vec![Some(b"abc".to_vec()), None]);
        let log = quit(&address, server);
        assert_eq!(log, ["POST /v1/p/project/batch old.proof"], "one request for both objects");
    }

    #[test]
    fn get_many_reads_one_by_one_from_a_gateway_without_batch() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |request, _| match request {
            "POST /v1/p/project/batch" => ("404 Not Found", r#"{"error":"no such route"}"#.into()),
            _ => ("200 OK", "x".into()),
        });
        let gateway = proof_client(&address, &identity, 300_000);
        let got = crate::store_remote::backend::Backend::Gateway(gateway.clone())
            .get_many(&["states/ab".into(), "states/cd".into()])
            .unwrap();
        assert_eq!(got, vec![Some(b"x".to_vec()), Some(b"x".to_vec())]);
        let log = quit(&address, server);
        assert_eq!(log[0], "POST /v1/p/project/batch old.proof");
        assert_eq!(log.len(), 3, "after the 404 each object is read alone: {log:?}");
        assert!(gateway.batch_off.load(Ordering::SeqCst));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_refused_batch_grants_nothing_because_each_object_read_is_authorized_alone() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |_, _| {
            ("401 Unauthorized", r#"{"error":"no read grant"}"#.into())
        });
        let gateway = proof_client(&address, &identity, 300_000);
        let backend = super::super::backend::Backend::Gateway(gateway.clone());
        assert!(backend.get_many(&["states/ab".into(), "states/cd".into()]).is_err());
        assert!(gateway.batch_off.load(Ordering::SeqCst));
        let log = quit(&address, server);
        assert!(log.iter().any(|r| r.starts_with("GET /v1/p/project/objects/")), "{log:?}");
    }

    #[test]
    fn an_object_read_asks_for_the_cdn_and_takes_the_bytes_from_the_gateway_when_the_url_is_not_its_host() {
        let identity = keys::generate_identity();
        let (address, server) = proof_gateway(keys::recipient_of(&identity).unwrap(), |request, bearer| {
            assert_eq!(request, "GET /v1/p/project/objects/states/ab");
            assert_eq!(bearer, "old.proof");
            ("200 OK", r#"{"url":"https://elsewhere.example/o/project/states/ab","expires":1,"bytes":5}"#.into())
        });
        let gateway = proof_client(&address, &identity, 300_000);
        // The URL is on another host: the client never follows it. The second request asks for the bytes.
        let body = gateway.get("states/ab").unwrap().unwrap();
        assert!(body.starts_with(b"{\"url\""), "the test server answers the same both times");
        let log = quit(&address, server);
        assert_eq!(
            log,
            ["GET /v1/p/project/objects/states/ab old.proof cdn", "GET /v1/p/project/objects/states/ab old.proof"],
            "the first read asks for the CDN; after a CDN failure the read goes through the gateway"
        );
        assert!(gateway.cdn_off.load(Ordering::SeqCst), "later reads of the command skip the CDN");
        let stats = gateway.transfer_stats();
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.cdn_fallbacks, 1);
        assert_eq!(stats.cdn_hits, 0);
        assert_eq!(stats.downloaded, body.len() as u64);
        assert_eq!(stats.control_received, body.len() as u64);
    }
}
