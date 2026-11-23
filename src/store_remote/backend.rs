//! Immutable objects in a folder or an S3 bucket.

use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub use super::download::TransferStats;
pub use layr_store::s3::Object;

/// An object that the store answered for and that cannot be used: its bytes do not decrypt or decode. Unlike a failed
/// read, reading it again gives the same answer.
#[derive(Debug)]
pub struct Undecodable(pub String);

impl Undecodable {
    pub fn of(e: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self(format!("{e:#}")))
    }
}

impl std::fmt::Display for Undecodable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Undecodable {}

/// An object that the store says is absent, by key. A listing that shows it again means a read that may succeed later.
#[derive(Debug)]
pub struct Missing(pub String);

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing remote object {}", self.0)
    }
}

impl std::error::Error for Missing {}

#[derive(Clone)]
pub enum Backend {
    File(PathBuf, FileIdentity),
    S3(super::s3::S3),
    Gateway(super::gateway::Gateway),
    /// Layr Cloud API v1 (Public and Private projects).
    Api(super::api::Api),
    Prefix(Box<Backend>, String),
    Epoch(Box<EpochBackend>),
}

/// File backends start with the process's rights; request backends use the resolved caller.
#[derive(Clone)]
pub enum FileIdentity {
    Process,
    Caller(std::sync::Arc<crate::privs::FsIdentity>),
}

impl FileIdentity {
    fn run<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        match self {
            Self::Process => f(),
            Self::Caller(identity) => crate::privs::as_user_fs(identity, f),
        }
    }
}

#[derive(Clone)]
pub struct EpochBackend {
    pub root: Backend,
    pub current: Backend,
    pub history: Vec<(Backend, String)>,
}
fn epoch_key(key: &str) -> std::borrow::Cow<'_, str> {
    if let Some(id) = key.strip_prefix("requests/claims/") {
        format!("claims/{id}").into()
    } else {
        key.into()
    }
}
fn historical_object(key: &str) -> bool {
    ["states/", "files/", "requests/", "claims/"].iter().any(|p| key.starts_with(p))
}
fn epoch_object(key: &str) -> bool {
    key == "store.json"
        || key == "public.json"
        || ["records/", "states/", "files/", "slots/", "history/", "index/", "requests/", "claims/"]
            .iter()
            .any(|p| key.starts_with(p))
}
pub(super) type ObjectRead = Result<Option<Vec<u8>>>;
type ReadVisitor<'a> = dyn FnMut(usize, ObjectRead, &str) -> Result<()> + 'a;

impl Backend {
    pub fn set_epoch(&self, epoch: Option<&str>) -> Result<()> {
        match self {
            Self::Gateway(g) => g.set_epoch(epoch),
            Self::Prefix(base, _) => base.set_epoch(epoch),
            Self::Epoch(e) => {
                e.root.set_epoch(epoch)?;
                e.current.set_epoch(epoch)?;
                for (backend, _) in &e.history {
                    backend.set_epoch(epoch)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub fn prove_reader(&self) -> Result<()> {
        match self {
            Self::Gateway(g) => g.prove_reader(),
            Self::Prefix(base, _) => base.prove_reader(),
            Self::Epoch(epoch) => epoch.current.prove_reader(),
            _ => Ok(()),
        }
    }

    /// One long-poll of the project events of a Layr Cloud remote.
    pub fn events(&self, after: Option<&str>, wait: u64) -> Result<serde_json::Value> {
        match self {
            Self::Gateway(g) => g.events(after, wait),
            Self::Api(a) => a.events(after, wait),
            Self::Prefix(base, _) => base.events(after, wait),
            Self::Epoch(epoch) => epoch.current.events(after, wait),
            _ => anyhow::bail!("layr wait needs a Layr Cloud remote (layr+https://)"),
        }
    }

    pub fn root(&self) -> &Backend {
        if let Self::Epoch(epoch) = self {
            &epoch.root
        } else {
            self
        }
    }
    fn namespace_epoch(&self) -> &str {
        if let Self::Prefix(_, scope) = self {
            if let Some((epoch, _)) = scope.strip_prefix("project/epochs/").and_then(|s| s.split_once('/')) {
                return epoch;
            }
        }
        "root"
    }
    /// Return the object's actual namespace and its key, binding unsigned outer routing
    /// fields to authenticated epoch discovery before the request parser runs.
    pub fn request_object(&self, key: &str, identity: &str) -> Result<(Vec<u8>, String, String)> {
        if let Self::Epoch(epoch) = self {
            if let Some(bytes) = epoch.current.get(key)? {
                return Ok((bytes, epoch.current.namespace_epoch().into(), identity.into()));
            }
            for (backend, old_identity) in &epoch.history {
                if let Some(bytes) = backend.get(key)? {
                    return Ok((bytes, backend.namespace_epoch().into(), old_identity.clone()));
                }
            }
            bail!("request disappeared");
        }
        Ok((self.get(key)?.context("request disappeared")?, self.namespace_epoch().into(), identity.into()))
    }
    pub fn decrypted(&self, key: &str, identity: &str) -> Result<Vec<u8>> {
        if let Self::Epoch(epoch) = self {
            if key.starts_with("states/") {
                if let Some(bytes) = epoch.current.get(key)? {
                    return super::keys::decrypt(identity, &bytes).map_err(Undecodable::of);
                }
                for (backend, old_identity) in &epoch.history {
                    if let Some(bytes) = backend.get(key)? {
                        return super::keys::decrypt(old_identity, &bytes).map_err(Undecodable::of);
                    }
                }
                bail!(Missing(key.to_string()));
            }
        }
        let bytes = self.get(key)?.ok_or_else(|| Missing(key.to_string()))?;
        super::keys::decrypt(identity, &bytes).map_err(Undecodable::of)
    }

    /// Stream a release object and select the authenticated identity of its epoch.
    pub fn release_stream(&self, key: &str, identity: &str) -> Result<(Box<dyn Read>, String)> {
        if !super::format::release_file(key) {
            bail!("invalid release object");
        }
        if let Self::Epoch(epoch) = self {
            if let Some(stream) = epoch.current.get_stream(key)? {
                return Ok((stream, identity.into()));
            }
            for (backend, old_identity) in &epoch.history {
                if let Some(stream) = backend.get_stream(key)? {
                    return Ok((stream, old_identity.clone()));
                }
            }
            bail!("missing release object {key}");
        }
        Ok((self.get_stream(key)?.with_context(|| format!("missing release object {key}"))?, identity.into()))
    }

    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn Read>>> {
        check_key(key)?;
        match self {
            Self::Epoch(_) => bail!("select an epoch before streaming"),
            Self::Prefix(base, prefix) => base.get_stream(&format!("{prefix}{key}")),
            Self::S3(s) => Ok(s.get_stream(key)?.map(|r| Box::new(r) as Box<dyn Read>)),
            Self::Gateway(g) => Ok(g.release_stream(key)?.map(|r| Box::new(r) as Box<dyn Read>)),
            Self::Api(a) if super::format::release_file(key) => a.release_stream(key),
            Self::Api(a) => Ok(a.get(key)?.map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn Read>)),
            Self::File(root, identity) => identity.run(|| {
                let path = crate::fsutil::safe_join(root, key)?;
                match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
                    Ok(file) => {
                        if !file.metadata()?.is_file() {
                            bail!("remote object is not a file");
                        }
                        Ok(Some(Box::new(file) as Box<dyn Read>))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }),
        }
    }

    pub fn put_file_new(&self, key: &str, mut source: fs::File) -> Result<bool> {
        check_key(key)?;
        match self {
            // A public release file goes to the root of the store, as `put_new` does with objects outside the epochs.
            Self::Epoch(epoch) if !epoch_object(key) => epoch.root.put_file_new(key, source),
            Self::Epoch(epoch) => epoch.current.put_file_new(key, source),
            Self::Prefix(base, prefix) => base.put_file_new(&format!("{prefix}{key}"), source),
            Self::S3(s) => s.put_file_new(key, source),
            Self::Gateway(g) => g.put_file_new(key, source),
            Self::Api(a) if super::format::release_file(key) => a.put_release_file(key, source),
            Self::Api(a) => {
                let mut body = Vec::new();
                source.read_to_end(&mut body)?;
                a.put_new(key, body)
            }
            Self::File(root, identity) => identity.run(|| {
                fs::create_dir_all(root)?;
                let path = crate::fsutil::safe_join(root, key)?;
                let parent = path.parent().context("object parent")?;
                fs::create_dir_all(parent)?;
                crate::fsutil::safe_join(root, key)?;
                let temporary = parent.join(format!(".put-{}", crate::ids::new_id()));
                let result = (|| {
                    let mut out = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                    std::io::copy(&mut source, &mut out)?;
                    out.sync_all()?;
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {
                            fs::File::open(parent)?.sync_all()?;
                            Ok(true)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                        Err(e) => Err(e.into()),
                    }
                })();
                let _ = fs::remove_file(temporary);
                result
            }),
        }
    }

    pub fn scoped(&self, prefix: &str) -> Result<Self> {
        check_key(prefix)?;
        Ok(Self::Prefix(Box::new(self.clone()), format!("{prefix}/")))
    }
    /// The Layr Cloud API client under this backend, if it is one (also through a prefix).
    pub fn api(&self) -> Option<&super::api::Api> {
        match self {
            Self::Api(api) => Some(api),
            Self::Prefix(base, _) => base.api(),
            _ => None,
        }
    }
    pub fn file(root: PathBuf) -> Self {
        Self::File(root, FileIdentity::Process)
    }
    pub fn as_caller(&mut self, ctx: &crate::ctx::Ctx) -> Result<()> {
        if let Self::File(_, identity) = self {
            *identity = FileIdentity::Caller(ctx.caller.fs_identity()?);
        }
        Ok(())
    }

    pub fn open(url: &str) -> Result<Self> {
        if url.starts_with("layr+https://") {
            return Ok(Self::Gateway(super::gateway::Gateway::open(url)?));
        }
        if url.starts_with("file://") {
            let parsed = reqwest::Url::parse(url).context("invalid file remote URL")?;
            if parsed.query().is_some() || parsed.fragment().is_some() {
                bail!("a file remote cannot have a query or fragment");
            }
            let path = parsed.to_file_path().map_err(|_| anyhow::anyhow!("a file remote needs an absolute local path"))?;
            return Ok(Self::file(path));
        }
        if let Some(path) = url.strip_prefix("s3://") {
            let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
            if bucket.is_empty() || bucket.contains(['@', ':', '?', '#']) || prefix.contains(['?', '#']) {
                bail!("invalid S3 bucket");
            }
            return Ok(Self::S3(super::s3::S3::unconnected(bucket, prefix)?));
        }
        bail!("store URL must start with file://, s3:// or layr+https://")
    }

    /// Read-only clients use caller environment without a local project or service context.
    pub fn for_reader(url: &str, env: &std::collections::HashMap<String, String>) -> Result<Self> {
        let mut backend = Self::open(url)?;
        match &mut backend {
            Self::S3(s3) => {
                let path = url.strip_prefix("s3://").expect("S3 URL checked");
                let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
                *s3 = super::s3::S3::with_env(bucket, prefix, env)?;
            }
            Self::Gateway(gateway) => gateway.connect_reader(env)?,
            Self::File(..) | Self::Prefix(..) | Self::Epoch(..) | Self::Api(_) => {}
        }
        Ok(backend)
    }

    pub fn unlock_reader(&self, identity: &str) -> Result<()> {
        match self {
            Self::Epoch(epoch) => {
                epoch.root.unlock_reader(identity)?;
                epoch.current.unlock_reader(identity)?;
                for (backend, _) in &epoch.history {
                    backend.unlock_reader(identity)?;
                }
                Ok(())
            }
            Self::Prefix(base, _) => base.unlock_reader(identity),
            Self::Gateway(gateway) => gateway.set_identity(identity),
            Self::File(..) | Self::S3(_) | Self::Api(_) => Ok(()),
        }
    }

    pub fn can_read(&self) -> bool {
        match self {
            Self::Gateway(gateway) => gateway.has_read_identity(),
            Self::Epoch(epoch) => epoch.root.can_read(),
            Self::Prefix(base, _) => base.can_read(),
            _ => true,
        }
    }

    pub(super) fn public_manifest(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Gateway(gateway) => gateway.public_get("public.json"),
            _ => self.get("public.json"),
        }
    }

    /// A backend that sends its own requests (not a view of another backend): its calls are counted.
    fn is_leaf(&self) -> bool {
        matches!(self, Self::S3(_) | Self::Gateway(_) | Self::Api(_) | Self::File(..))
    }

    /// Enforced ciphertext ceiling, when every route uses the Gateway's bounded reader.
    /// File/S3 bodies have no transport ceiling and must be prefetched one at a time.
    pub(super) fn read_limit(&self) -> Option<usize> {
        match self {
            Self::Gateway(_) => Some(super::gateway::OBJECT_LIMIT),
            Self::Prefix(base, _) => base.read_limit(),
            Self::Epoch(epoch) => {
                let limit = epoch.current.read_limit()?;
                (epoch.root.read_limit() == Some(limit) && epoch.history.iter().all(|(b, _)| b.read_limit() == Some(limit)))
                    .then_some(limit)
            }
            _ => None,
        }
    }

    /// Objects in input order. The returned Vec retains all bodies; use `visit_many` for large inventories.
    #[cfg(test)]
    pub fn get_many(&self, keys: &[String]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut out = vec![None; keys.len()];
        self.visit_many(keys, &vec![None; keys.len()], |i, body| {
            out[i] = body;
            Ok(())
        })?;
        Ok(out)
    }

    /// Consume bounded batches as they complete. Indices refer to `keys`; completion order is unspecified. `sizes`,
    /// by key, are known from a listing or not: a known size reserves the bytes of its read, and its body may not
    /// exceed it.
    pub fn visit_many(
        &self,
        keys: &[String],
        sizes: &[Option<u64>],
        mut visit: impl FnMut(usize, Option<Vec<u8>>) -> Result<()>,
    ) -> Result<()> {
        self.visit_many_results(keys, sizes, |i, body| visit(i, body?))
    }

    /// Keep individual read errors for a consumer that processes objects in key order.
    pub(super) fn visit_many_results(
        &self,
        keys: &[String],
        sizes: &[Option<u64>],
        mut visit: impl FnMut(usize, ObjectRead) -> Result<()>,
    ) -> Result<()> {
        self.visit_reads(keys, sizes, "", &mut |i, body, _| visit(i, body))
    }

    /// Decrypt each body with the identity of the epoch that supplied it, including through Prefix views. `sizes` as
    /// in `visit_many`.
    pub fn visit_decrypted_many(
        &self,
        keys: &[String],
        sizes: &[Option<u64>],
        identity: &str,
        mut visit: impl FnMut(usize, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        self.visit_reads(keys, sizes, identity, &mut |i, body, selected| {
            let body = body?.with_context(|| format!("missing remote object {}", keys[i]))?;
            visit(i, super::keys::decrypt(selected, &body)?)
        })
    }

    #[cfg(test)]
    pub fn decrypted_many(&self, keys: &[String], identity: &str) -> Result<Vec<Vec<u8>>> {
        let mut out = vec![Vec::new(); keys.len()];
        self.visit_decrypted_many(keys, &vec![None; keys.len()], identity, |i, body| {
            out[i] = body;
            Ok(())
        })?;
        Ok(out)
    }

    pub fn transfer_stats(&self) -> TransferStats {
        self.gateway_transfer_stats().unwrap_or_default()
    }

    /// Shared across Gateway clones and views.
    pub(super) fn gateway_transfer_stats(&self) -> Option<TransferStats> {
        match self {
            Self::Gateway(g) => Some(g.transfer_stats()),
            Self::Prefix(base, _) => base.gateway_transfer_stats(),
            Self::Epoch(e) => e.root.gateway_transfer_stats(),
            _ => None,
        }
    }

    fn visit_reads(&self, keys: &[String], sizes: &[Option<u64>], identity: &str, visit: &mut ReadVisitor<'_>) -> Result<()> {
        for key in keys {
            check_key(key)?;
        }
        if sizes.len() != keys.len() {
            bail!("download size count does not match keys");
        }
        match self {
            Self::Prefix(base, prefix) => {
                let scoped: Vec<_> = keys.iter().map(|key| format!("{prefix}{key}")).collect();
                base.visit_reads(&scoped, sizes, identity, visit)
            }
            Self::Epoch(epoch) => {
                let (current, root): (Vec<_>, Vec<_>) = (0..keys.len()).partition(|&i| epoch_object(&keys[i]));
                let route = |backend: &Backend, indices: &[usize], mapped: bool, selected: &str, visit: &mut ReadVisitor<'_>| {
                    let names: Vec<_> = indices
                        .iter()
                        .map(|&i| if mapped { epoch_key(&keys[i]).into_owned() } else { keys[i].clone() })
                        .collect();
                    let lengths: Vec<_> = indices.iter().map(|&i| sizes[i]).collect();
                    backend.visit_reads(&names, &lengths, selected, &mut |j, body, id| visit(indices[j], body, id))
                };
                route(&epoch.root, &root, false, identity, visit)?;
                let mut missing = Vec::new();
                route(&epoch.current, &current, true, identity, &mut |i, body, id| {
                    if matches!(&body, Ok(None)) && historical_object(&keys[i]) {
                        missing.push(i);
                        Ok(())
                    } else {
                        visit(i, body, id)
                    }
                })?;
                for (backend, old_identity) in &epoch.history {
                    if missing.is_empty() {
                        break;
                    }
                    let mut next = Vec::new();
                    route(backend, &missing, false, old_identity, &mut |i, body, id| {
                        if matches!(&body, Ok(None)) {
                            next.push(i);
                            Ok(())
                        } else {
                            visit(i, body, id)
                        }
                    })?;
                    missing = next;
                }
                for i in missing {
                    visit(i, Ok(None), identity)?;
                }
                Ok(())
            }
            Self::Gateway(g) => g.visit_many(keys, sizes, |i, body| visit(i, body, identity)),
            // Listed sizes reserve their bytes, and a body may not exceed its size. A File or S3 body of unknown size
            // has no ceiling: its read runs alone, after the reads of known size, which share the pool. An API read
            // keeps a share of the budget.
            _ => {
                let unknown = match self {
                    Self::Api(_) => super::download::BYTE_BUDGET / super::download::JOBS as u64,
                    _ => super::download::BYTE_BUDGET,
                };
                let (known, open): (Vec<usize>, Vec<usize>) = (0..keys.len()).partition(|&i| sizes[i].is_some());
                let order = [known, open].concat();
                super::download::run(
                    keys.len(),
                    super::download::JOBS,
                    super::download::BYTE_BUDGET,
                    |j| sizes[order[j]].unwrap_or(unknown),
                    |j| self.get_within(&keys[order[j]], sizes[order[j]]),
                    |j, body| visit(order[j], Ok(body), identity),
                )
            }
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.get_within(key, None)
    }

    /// `get` of an immutable object of at most `limit` bytes (its listed size, or a bound of its format): a larger body
    /// fails before a File or S3 read takes in more than `limit` bytes of it.
    pub(super) fn get_within(&self, key: &str, limit: Option<u64>) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        let _io = Io::start("get", key, self.is_leaf());
        let exceeds = |limit: u64| anyhow::anyhow!("remote object {key} exceeds its size bound ({limit} bytes)");
        let bounded = |body: Option<Vec<u8>>| match (body, limit) {
            (Some(body), Some(limit)) if body.len() as u64 > limit => Err(exceeds(limit)),
            (body, _) => Ok(body),
        };
        match self {
            Self::Epoch(epoch) => {
                if !epoch_object(key) {
                    return epoch.root.get_within(key, limit);
                }
                let data = epoch.current.get_within(&epoch_key(key), limit)?;
                if data.is_some()
                    || !(key.starts_with("states/")
                        || key.starts_with("files/")
                        || key.starts_with("requests/")
                        || key.starts_with("claims/"))
                {
                    return Ok(data);
                }
                for (backend, _) in &epoch.history {
                    if let Some(bytes) = backend.get_within(key, limit)? {
                        return Ok(Some(bytes));
                    }
                }
                Ok(None)
            }
            Self::Prefix(base, prefix) => base.get_within(&format!("{prefix}{key}"), limit),
            Self::S3(s) => s.get_within(key, limit),
            // The gateway's reader has its own ceiling (`OBJECT_LIMIT`); a smaller bound stops it sooner.
            Self::Gateway(g) => {
                let ceiling = super::gateway::OBJECT_LIMIT;
                bounded(
                    g.get_limited(key, limit.map_or(ceiling, |limit| usize::try_from(limit).unwrap_or(ceiling).min(ceiling)))?,
                )
            }
            Self::Api(a) => bounded(a.get(key)?),
            Self::File(root, identity) => identity.run(|| {
                #[cfg(test)]
                record(root, "get", key);
                #[cfg(test)]
                if fault(root, "get", key) {
                    bail!("read remote object {key}: injected fault");
                }
                #[cfg(test)]
                if fault(root, "absent", key) {
                    return Ok(None);
                }
                #[cfg(test)]
                let counted = FILE_THREADS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(root)
                    .map(|threads| threads.insert(std::thread::current().id()))
                    .is_some();
                // A counted read lasts a moment, so that reads that may run together do overlap on a busy machine.
                #[cfg(test)]
                if counted {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let path = crate::fsutil::safe_join(root, key)?;
                let f = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e).with_context(|| format!("read remote object {key}")),
                };
                let metadata = f.metadata()?;
                if !metadata.is_file() {
                    bail!("remote object is not a file: {key}");
                }
                if let Some(limit) = limit.filter(|limit| metadata.len() > *limit) {
                    return Err(exceeds(limit));
                }
                // The file's length reserves its buffer; a file that grew since is cut one byte past the bound.
                let mut data = Vec::with_capacity(usize::try_from(metadata.len())?);
                f.take(limit.map_or(u64::MAX, |limit| limit + 1)).read_to_end(&mut data)?;
                bounded(Some(data))
            }),
        }
    }

    /// Publish a whole object or leave its destination absent. Never replace an object.
    pub fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        check_key(key)?;
        let created = match self {
            Self::Epoch(epoch) => {
                if epoch_object(key) {
                    epoch.current.put_new(&epoch_key(key), body)
                } else {
                    epoch.root.put_new(key, body)
                }
            }
            Self::Prefix(base, prefix) => base.put_new(&format!("{prefix}{key}"), body),
            Self::S3(s) => s.put_new(key, body),
            Self::Gateway(g) => g.put_new(key, body),
            Self::Api(a) => a.put_new(key, body),
            #[cfg(test)]
            Self::File(root, _) if fault(root, "put", key) => Ok(false),
            Self::File(root, identity) => identity.run(|| {
                fs::create_dir_all(root)?;
                let path = crate::fsutil::safe_join(root, key)?;
                let parent = path.parent().expect("object parent");
                fs::create_dir_all(parent)?;
                crate::fsutil::safe_join(root, key)?;
                let temporary = parent.join(format!(".put-{}", crate::ids::new_id()));
                let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                let result = (|| -> Result<bool> {
                    f.write_all(&body)?;
                    f.sync_all()?;
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {
                            fs::File::open(parent)?.sync_all()?;
                            Ok(true)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                        Err(e) => Err(e.into()),
                    }
                })();
                let _ = fs::remove_file(temporary);
                result
            }),
        };
        // Logged after the write ends: tests check what an upload waited for.
        #[cfg(test)]
        if let Self::File(root, _) = self {
            record(root, "put", key);
        }
        created
    }

    /// Visit requests in backend order. The callback can stop the pass.
    pub fn visit_requests(&self, mut visit: impl FnMut(Object) -> Result<bool>) -> Result<()> {
        if let Self::Api(api) = self {
            return api.visit_requests(visit);
        }
        for object in self.list("requests/")? {
            if !visit(object)? {
                break;
            }
        }
        Ok(())
    }

    /// Immediate child folders, without enumerating their objects. Prefixes end in `/`.
    fn folders(&self, prefix: &str) -> Result<Option<Vec<String>>> {
        check_key(prefix.trim_end_matches('/'))?;
        if !prefix.ends_with('/') {
            bail!("folder listing needs a directory prefix");
        }
        match self {
            Self::Epoch(epoch) => epoch.root.folders(prefix),
            Self::Prefix(base, scope) => match base.folders(&format!("{scope}{prefix}"))? {
                Some(folders) => Ok(Some(
                    folders
                        .into_iter()
                        .map(|folder| Ok(folder.strip_prefix(scope).context("folder is outside its namespace")?.to_string()))
                        .collect::<Result<_>>()?,
                )),
                None => Ok(None),
            },
            Self::Gateway(g) if prefix == "overlays/" => g.overlay_folders(),
            Self::Gateway(_) => Ok(None),
            Self::S3(s) => Ok(Some(s.folders(prefix)?)),
            Self::Api(_) => Ok(None),
            Self::File(root, identity) => identity.run(|| {
                let start = crate::fsutil::safe_join(root, prefix.trim_end_matches('/'))?;
                if fs::symlink_metadata(&start).is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!("symbolic link in remote store");
                }
                let entries = match fs::read_dir(&start) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(Vec::new())),
                    Err(e) => return Err(e.into()),
                };
                let mut folders = Vec::new();
                for entry in entries {
                    let entry = entry?;
                    let kind = entry.file_type()?;
                    if kind.is_symlink() {
                        bail!("symbolic link in remote store");
                    }
                    if kind.is_dir() {
                        let name = entry.file_name().into_string().map_err(|_| anyhow::anyhow!("remote folder is not UTF-8"))?;
                        folders.push(format!("{prefix}{name}/"));
                    }
                }
                folders.sort();
                Ok(Some(folders))
            }),
        }
    }

    /// The overlay key objects (`overlays/<id>/keys/...`), and for a backend without folder listings every object
    /// of `overlays/`. A gateway lists the overlay folders, then the keys of each: the full listing holds every
    /// chunk of every overlay, and it took a follow pass of project layr more than 10 minutes (2026-11-15).
    pub fn overlay_key_objects(&self) -> Result<Vec<Object>> {
        self.overlay_key_objects_filtered(|_| true)
    }

    /// Filter overlay ids before listing keys or reading manifests. Unknown ids can still be taken:
    /// a hidden overlay's name is only available after its manifest opens.
    pub(super) fn overlay_key_objects_filtered(&self, take: impl Fn(&str) -> bool) -> Result<Vec<Object>> {
        let folders = self.folders("overlays/")?;
        let Some(folders) = folders else {
            return Ok(self.list("overlays/")?.into_iter().filter(|o| take(o.key.split('/').nth(1).unwrap_or(""))).collect());
        };
        let mut objects = Vec::new();
        for folder in folders {
            if take(folder.trim_end_matches('/').rsplit('/').next().unwrap_or("")) {
                objects.extend(self.list(&format!("{folder}keys/"))?);
            }
        }
        Ok(objects)
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        if !prefix.is_empty() {
            check_key(prefix.trim_end_matches('/'))?;
        }
        let _io = Io::start("list", prefix, self.is_leaf());
        match self {
            Self::Epoch(epoch) => {
                if !epoch_object(prefix) {
                    return epoch.root.list(prefix);
                }
                let mut objects: std::collections::BTreeMap<String, Object> = std::collections::BTreeMap::new();
                if prefix.starts_with("states/")
                    || prefix.starts_with("files/")
                    || prefix.starts_with("requests/")
                    || prefix.starts_with("claims/")
                {
                    for (backend, _) in epoch.history.iter().rev() {
                        for object in backend.list(prefix)? {
                            objects.insert(object.key.clone(), object);
                        }
                    }
                }
                for object in epoch.current.list(prefix)? {
                    objects.insert(object.key.clone(), object);
                }
                Ok(objects.into_values().collect())
            }
            Self::Prefix(base, scope) => base
                .list(&format!("{scope}{prefix}"))?
                .into_iter()
                .map(|mut object| {
                    object.key = object.key.strip_prefix(scope).context("object is outside its namespace")?.to_string();
                    Ok(object)
                })
                .collect(),
            Self::S3(s) => s.list(prefix),
            Self::Gateway(g) => g.list(prefix),
            Self::Api(a) => a.list(prefix),
            Self::File(root, identity) => identity.run(|| {
                #[cfg(test)]
                record(root, "list", prefix);
                // A policy that denies listing some prefixes (an S3 bucket policy can).
                #[cfg(test)]
                if fault(root, "list", prefix) {
                    bail!("listing denied: {prefix}");
                }
                let mut out = Vec::new();
                if root.exists() {
                    // A state prefix must not enumerate every sibling state directory.
                    let directory = prefix.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
                    let start = if directory.is_empty() { root.clone() } else { crate::fsutil::safe_join(root, directory)? };
                    match fs::symlink_metadata(&start) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            bail!("symbolic link in remote store: {directory}")
                        }
                        Ok(meta) if meta.is_dir() => walk(root, &start, prefix, &mut out)?,
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                out.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(out)
            }),
        }
    }

    /// Temporary files of puts that stopped (`.put-*`) and are older than `cutoff` (Unix seconds).
    /// Nothing lists them, so only this finds them. Deletes them unless `dry`; returns their keys.
    pub fn stale_temporaries(&self, cutoff: i64, dry: bool) -> Result<Vec<String>> {
        let mut out = match self {
            Self::File(root, identity) => identity.run(|| {
                let mut out = Vec::new();
                if root.exists() {
                    sweep_temporaries(root, root, cutoff, dry, &mut out)?;
                }
                Ok(out)
            })?,
            // A prefix is a folder of the base store: its temporary files are there too.
            Self::Prefix(base, _) => base.stale_temporaries(cutoff, dry)?,
            Self::Epoch(epoch) => {
                let mut out = Vec::new();
                for backend in [&epoch.root, &epoch.current].into_iter().chain(epoch.history.iter().map(|(b, _)| b)) {
                    out.extend(backend.stale_temporaries(cutoff, dry)?);
                }
                out
            }
            Self::S3(_) | Self::Gateway(_) | Self::Api(_) => Vec::new(),
        };
        out.sort();
        out.dedup();
        Ok(out)
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match self {
            Self::Epoch(epoch) => {
                if key.starts_with("files/") {
                    epoch.current.delete(key)?;
                    for (backend, _) in &epoch.history {
                        backend.delete(key)?;
                    }
                    Ok(())
                } else if epoch_object(key) {
                    epoch.current.delete(&epoch_key(key))
                } else {
                    epoch.root.delete(key)
                }
            }
            Self::Prefix(base, prefix) => base.delete(&format!("{prefix}{key}")),
            Self::S3(s) => s.delete(key),
            Self::Gateway(g) => g.delete(key),
            Self::Api(_) => {
                bail!("Layr Cloud deletes objects itself (server-side garbage collection): {key}")
            }
            Self::File(root, identity) => identity.run(|| {
                let path = crate::fsutil::safe_join(root, key)?;
                match fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }),
        }
    }
}

fn sweep_temporaries(root: &Path, dir: &Path, cutoff: i64, dry: bool, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let temporary = entry.file_name().to_string_lossy().starts_with(".put-");
        let m = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // A put in flight renames or unlinks its temporary file after the read of the folder.
            Err(error) if temporary && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if m.file_type().is_symlink() {
            continue;
        }
        if m.is_dir() {
            sweep_temporaries(root, &path, cutoff, dry, out)?;
            continue;
        }
        if !temporary {
            continue;
        }
        let modified = m.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        if modified >= cutoff {
            continue;
        }
        let key = path.strip_prefix(root)?.to_str().context("non-UTF-8 remote object")?.to_string();
        if dry || fs::remove_file(&path).is_ok() {
            out.push(key);
        }
    }
    Ok(())
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<Object>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        walk_entry(root, entry?, prefix, out)?;
    }
    Ok(())
}

fn walk_entry(root: &Path, entry: fs::DirEntry, prefix: &str, out: &mut Vec<Object>) -> Result<()> {
    let path = entry.path();
    let name = path.strip_prefix(root)?.to_str().context("non-UTF-8 remote object")?.to_string();
    // A sibling outside the prefix is not looked at: a listing of a shard (`states/01a1e`) must not stat every state.
    if !(name.starts_with(prefix) || prefix.starts_with(&format!("{name}/"))) {
        return Ok(());
    }
    let temporary = entry.file_name().to_string_lossy().starts_with(".put-");
    let m = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        // Publication unlinks its internal temporary file after creating the final
        // hard link. A listing may have already read that directory entry.
        Err(error) if temporary && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if m.file_type().is_symlink() {
        bail!("symbolic link in remote store: {name}");
    }
    if m.is_dir() {
        walk(root, &path, prefix, out)?;
    } else if name.starts_with(prefix) && !temporary {
        let modified = m.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        #[cfg(test)]
        if fault(root, "unsized", &name) {
            out.push(Object { key: name, modified, size: None });
            return Ok(());
        }
        out.push(Object { key: name, modified, size: Some(m.len()) });
    }
    Ok(())
}

pub(super) fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.split('/').any(|p| p.is_empty() || p == "." || p == "..") || key.contains(['\\', '\0']) {
        bail!("invalid remote object key: {key:?}");
    }
    crate::fsutil::check_rel(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlay_discovery_does_not_walk_file_cache_data_or_lose_scoped_keys() {
        let root = std::env::temp_dir().join(format!("layr-overlay-listing-{}", crate::ids::new_id()));
        let backend = Backend::file(root.clone());
        let scoped = backend.scoped("project/epochs/current").unwrap();
        scoped.put_new("overlays/a/keys/public.json", Vec::new()).unwrap();
        scoped.put_new("overlays/b/keys/hidden.age", Vec::new()).unwrap();
        let data = root.join("project/epochs/current/overlays/a/epochs/old/chunks");
        fs::create_dir_all(&data).unwrap();
        // A recursive listing must fail if it reaches the cache payload namespace.
        std::os::unix::fs::symlink("missing", data.join("poison")).unwrap();
        assert!(scoped.list("overlays/").is_err());
        let objects = scoped.overlay_key_objects().unwrap();
        assert_eq!(
            objects.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            ["overlays/a/keys/public.json", "overlays/b/keys/hidden.age"]
        );
        assert!(matches!(backend.folders("absent/").unwrap(), Some(f) if f.is_empty()));
        std::os::unix::fs::symlink("project/epochs/current/overlays", root.join("linked")).unwrap();
        assert!(backend.folders("linked/").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reader_hooks_use_s3_caller_credentials_and_leave_file_reads_local() {
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut bytes = Vec::new();
                    let mut byte = [0];
                    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 8192 {
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        bytes.push(byte[0]);
                    }
                    let request = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                    let valid = request.contains("credential=disk-reader-test/")
                        && request.contains("x-amz-security-token: reader-session-marker");
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = tx.send(valid);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let env = std::collections::HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "disk-reader-test".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "reader-secret-marker".into()),
            ("AWS_SESSION_TOKEN".into(), "reader-session-marker".into()),
            ("AWS_ENDPOINT_URL_S3".into(), endpoint),
        ]);
        let backend = Backend::for_reader("s3://reader-test/prefix", &env).unwrap();
        backend.unlock_reader("file and S3 need no reader identity").unwrap();
        assert!(backend.get("store.json").unwrap().is_none());
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("store.json"), b"local").unwrap();
        let backend = Backend::for_reader(&format!("file://{}", dir.display()), &env).unwrap();
        backend.unlock_reader("not used").unwrap();
        assert_eq!(backend.get("store.json").unwrap().unwrap(), b"local");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scoped_objects_use_the_same_exclusive_publication_and_stay_in_their_namespace() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let backend = Backend::file(root.clone());
        let overlay = backend.scoped("overlays/one").unwrap();
        assert!(overlay.put_new("states/a", b"overlay".to_vec()).unwrap());
        assert!(!overlay.put_new("states/a", b"replacement".to_vec()).unwrap());
        assert!(backend.put_new("states/a", b"shared".to_vec()).unwrap());
        assert_eq!(overlay.get("states/a").unwrap().unwrap(), b"overlay");
        assert_eq!(backend.get("states/a").unwrap().unwrap(), b"shared");
        assert_eq!(overlay.list("states/").unwrap()[0].key, "states/a");
        assert_eq!(overlay.list("").unwrap().len(), 1);
        assert!(backend.scoped("../bad").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_tolerates_only_a_vanished_publication_temporary() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        fs::create_dir(&root).unwrap();
        let mut objects = Vec::new();
        for (name, tolerated) in [(".put-published", true), ("published-object", false)] {
            let path = root.join(name);
            fs::write(&path, b"publication bytes").unwrap();
            let entry = fs::read_dir(&root).unwrap().next().unwrap().unwrap();
            // Force the interval between read_dir and stat: the uploader removes
            // its temporary name once the public hard link exists.
            fs::remove_file(&path).unwrap();
            assert_eq!(walk_entry(&root, entry, "", &mut objects).is_ok(), tolerated);
        }
        for name in ["public-link", ".put-link"] {
            let path = root.join(name);
            std::os::unix::fs::symlink("missing-target", &path).unwrap();
            assert!(Backend::file(root.clone()).list("").is_err());
            fs::remove_file(path).unwrap();
        }
        assert!(objects.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_listing_visits_only_the_requested_directory_and_refuses_its_links() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let backend = Backend::file(root.clone());
        backend.put_new("states/one/a.age", b"one".to_vec()).unwrap();
        backend.put_new("states/two/b.age", b"two".to_vec()).unwrap();
        // This sibling must not be inspected for an exact state-directory prefix.
        std::os::unix::fs::symlink("/missing", root.join("states/two/link")).unwrap();
        let files = backend.list("states/one/").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].key, "states/one/a.age");
        assert_eq!(backend.list("states/one/a").unwrap().len(), 1);
        // A partial name lists its matches and does not look at the siblings (a link among them is not seen).
        std::os::unix::fs::symlink("/missing", root.join("states/zz-link")).unwrap();
        assert_eq!(backend.list("states/on").unwrap().len(), 1);
        assert!(backend.list("states/zz").is_err());
        std::fs::remove_file(root.join("states/zz-link")).unwrap();
        assert!(backend.list("states/missing/").unwrap().is_empty());
        assert!(backend.list("states/two/").is_err());
        std::os::unix::fs::symlink("one", root.join("states/link")).unwrap();
        assert!(backend.list("states/link/").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_is_exclusive_and_never_changes_bytes() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let s = Backend::file(root.clone());
        assert!(s.put_new("states/a", b"first".to_vec()).unwrap());
        assert!(!s.put_new("states/a", b"second".to_vec()).unwrap());
        assert_eq!(s.get("states/a").unwrap().unwrap(), b"first");
        assert_eq!(s.list("states/").unwrap().len(), 1);
        assert!(s.put_new("../bad", vec![]).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod epoch_tests {
    use super::*;

    #[test]
    fn a_public_release_file_goes_to_the_root_and_a_release_file_to_the_epoch() {
        let path = std::env::temp_dir().join(crate::ids::new_id());
        let root = Backend::file(path.clone());
        let current = root.scoped("project/epochs/new").unwrap();
        let backend =
            Backend::Epoch(Box::new(EpochBackend { root: root.clone(), current: current.clone(), history: Vec::new() }));
        let file = |body: &[u8]| {
            let p = path.join(format!("in-{}", crate::ids::new_id()));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(&p, body).unwrap();
            std::fs::File::open(p).unwrap()
        };
        assert!(backend.put_file_new("public/release/v1.0.0/a.tar.gz", file(b"public")).unwrap());
        assert_eq!(root.get("public/release/v1.0.0/a.tar.gz").unwrap().unwrap(), b"public");
        assert!(current.get("public/release/v1.0.0/a.tar.gz").unwrap().is_none());
        assert!(backend.put_file_new("files/01a1ae00-0000-7000-8000-000000000001.age", file(b"sealed")).unwrap());
        assert!(current.get("files/01a1ae00-0000-7000-8000-000000000001.age").unwrap().is_some());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn historical_states_use_sealed_history_keys_but_slots_never_fall_back() {
        let path = std::env::temp_dir().join(crate::ids::new_id());
        let root = Backend::file(path.clone());
        let current = root.scoped("project/epochs/new").unwrap();
        let old = super::super::keys::generate_identity();
        let new = super::super::keys::generate_identity();
        root.put_new(
            "states/old",
            super::super::keys::encrypt(&super::super::keys::recipient_of(&old).unwrap(), b"old bytes").unwrap(),
        )
        .unwrap();
        root.put_new("slots/removed.age", b"old slot".to_vec()).unwrap();
        let backend = Backend::Epoch(Box::new(EpochBackend {
            root: root.clone(),
            current: current.clone(),
            history: vec![(root.clone(), old)],
        }));
        assert_eq!(backend.decrypted("states/old", &new).unwrap(), b"old bytes");
        current
            .put_new(
                "states/old",
                super::super::keys::encrypt(&super::super::keys::recipient_of(&new).unwrap(), b"current bytes").unwrap(),
            )
            .unwrap();
        assert_eq!(backend.decrypted("states/old", &new).unwrap(), b"current bytes");
        assert_eq!(backend.decrypted("states/missing", &new).unwrap_err().to_string(), "missing remote object states/missing");
        assert!(backend.get("slots/removed.age").unwrap().is_none());
        backend.put_new("requests/one.json", b"sealed request".to_vec()).unwrap();
        backend.put_new("requests/claims/one.json", b"claim".to_vec()).unwrap();
        assert_eq!(current.get("requests/one.json").unwrap().unwrap(), b"sealed request");
        assert_eq!(current.get("claims/one.json").unwrap().unwrap(), b"claim");
        assert!(root.get("requests/one.json").unwrap().is_none());
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(test)]
mod temporary_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("layr-put-{name}-{}", crate::ids::new_id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn temporaries(folder: &Path) -> usize {
        fs::read_dir(folder).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".put-")).count()
    }

    /// The source cannot be read, so the copy fails after the temporary file exists.
    #[test]
    fn reads_of_known_size_share_the_pool_before_the_reads_of_unknown_size() {
        let root = dir("mixed-sizes");
        let backend = Backend::file(root.clone());
        let keys: Vec<String> = (0..8).map(|n| format!("obj/{n}")).collect();
        for key in &keys {
            backend.put_new(key, vec![1; 10]).unwrap();
        }
        // Every other size is known.
        let sizes: Vec<Option<u64>> = (0..keys.len()).map(|n| (n % 2 == 0).then_some(10)).collect();
        FILE_REQUESTS.lock().unwrap().insert(root.clone(), Vec::new());
        let mut seen = vec![false; keys.len()];
        backend
            .visit_many(&keys, &sizes, |i, body| {
                assert_eq!(body.as_deref(), Some(&[1; 10][..]));
                seen[i] = true;
                Ok(())
            })
            .unwrap();
        assert!(seen.iter().all(|seen| *seen));
        let gets = FILE_REQUESTS.lock().unwrap().remove(&root).unwrap();
        let known: Vec<bool> = gets.iter().map(|(_, key)| sizes[keys.iter().position(|k| k == key).unwrap()].is_some()).collect();
        // An unknown read runs alone: one before a known read would hold back the reads of known size after it.
        assert_eq!(known, [[true; 4], [false; 4]].concat(), "{gets:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_put_leaves_no_temporary_file() {
        let root = dir("failed");
        let backend = Backend::file(root.clone());
        let unreadable = fs::File::options().write(true).create(true).truncate(true).open(root.join("source")).unwrap();
        assert!(backend.put_file_new("obj/one", unreadable).is_err());
        assert_eq!(temporaries(&root.join("obj")), 0);
        fs::remove_dir_all(root).unwrap();
    }

    /// Temporary files older than the cutoff go; young ones and objects stay.
    #[test]
    fn stale_temporary_files_go_after_a_day() {
        let root = dir("stale");
        fs::create_dir_all(root.join("a")).unwrap();
        let old = root.join("a/.put-old");
        fs::write(&old, b"x").unwrap();
        let file = fs::File::options().write(true).open(&old).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(25 * 3600))).unwrap();
        fs::write(root.join("a/.put-new"), b"x").unwrap();
        fs::write(root.join("a/object"), b"x").unwrap();
        let cutoff = chrono::Utc::now().timestamp() - 86400;
        assert_eq!(Backend::file(root.clone()).stale_temporaries(cutoff, true).unwrap(), vec!["a/.put-old".to_string()]);
        assert!(old.exists(), "a dry run changes nothing");
        assert_eq!(Backend::file(root.clone()).stale_temporaries(cutoff, false).unwrap(), vec!["a/.put-old".to_string()]);
        assert!(!old.exists());
        assert!(root.join("a/.put-new").exists() && root.join("a/object").exists());
        fs::remove_dir_all(root).unwrap();
    }
}

/// The requests of the File backends whose roots a test registered: `(operation, key or prefix)` in order.
#[cfg(test)]
pub(super) static FILE_REQUESTS: std::sync::Mutex<std::collections::BTreeMap<PathBuf, Vec<(&'static str, String)>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The threads that read objects of the File backends whose roots a test registered.
#[cfg(test)]
pub(super) static FILE_THREADS: std::sync::Mutex<
    std::collections::BTreeMap<PathBuf, std::collections::HashSet<std::thread::ThreadId>>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Faults of the File backends whose roots a test registered: `(operation, key prefix)`. A `get` of such a key fails, an
/// `absent` one reads as absent, an `unsized` one lists without its size, and a `put` of one creates nothing (`put_new`
/// returns false).
#[cfg(test)]
pub(super) static FILE_FAULTS: std::sync::Mutex<std::collections::BTreeMap<PathBuf, Vec<(&'static str, String)>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
fn fault(root: &Path, operation: &str, key: &str) -> bool {
    let faults = FILE_FAULTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    faults.get(root).is_some_and(|faults| faults.iter().any(|(op, prefix)| *op == operation && key.starts_with(prefix.as_str())))
}

#[cfg(test)]
fn record(root: &Path, operation: &'static str, key: &str) {
    if let Some(log) = FILE_REQUESTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(root) {
        log.push((operation, key.to_string()));
    }
}

/// The requests of this process by kind (`get records`, `list chunks`, ...): count and milliseconds, for `LAYR_TRACE`.
static IO: std::sync::Mutex<std::collections::BTreeMap<String, (u64, f64)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// One counted request; it adds its time when it ends.
struct Io(Option<(String, std::time::Instant)>);

impl Io {
    fn start(op: &str, key: &str, leaf: bool) -> Self {
        if !leaf || !crate::trace::enabled() {
            return Self(None);
        }
        // The kind is the folder that names the object: `records`, `states`, `chunks`, `keys`, `slots`, `git`.
        let kind = ["records", "states", "chunks", "keys", "slots", "git", "files", "claims", "requests"]
            .into_iter()
            .find(|k| key.split('/').any(|part| part == *k))
            .unwrap_or_else(|| key.split('/').next().unwrap_or(""));
        Self(Some((format!("{op} {kind}"), std::time::Instant::now())))
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        if let Some((kind, start)) = self.0.take() {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let mut io = IO.lock().unwrap_or_else(|e| e.into_inner());
            let entry = io.entry(kind).or_default();
            entry.0 += 1;
            entry.1 += ms;
        }
    }
}

/// With `LAYR_TRACE`, prints the requests of this process by kind, and forgets them.
pub fn trace_requests() {
    if !crate::trace::enabled() {
        return;
    }
    let mut io = IO.lock().unwrap_or_else(|e| e.into_inner());
    for (kind, (count, ms)) in std::mem::take(&mut *io) {
        eprintln!("trace {ms:>8.1} ms  remote {kind}: {count} request(s)");
    }
}
