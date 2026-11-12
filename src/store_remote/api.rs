//! Layr Cloud API v1 client for Public and Private remotes (layr-cloud docs/api.md, layr docs/remotes.md).
//!
//! The record code reads and writes keys of the store layout (`records/…`); [`Api`] maps them to `/records`.
//! States and objects use the object format of docs/objects.md (trees, times tables, blobs in packs) through
//! direct calls (`cloud_objects`), as do refs, the machine key and the person login. Keys of the Secret
//! layout (slots, epochs, public manifests) do not exist here.

use super::backend::Object;
use super::format;
use anyhow::{anyhow, bail, Context, Result};
use layr_objects::{Id, Keying, Kind, StateRef};
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    Method, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The largest object that goes up with one `PUT /objects/<id>`; a larger one goes up in parts.
pub(crate) const SINGLE_PUT_LIMIT: usize = 64 << 20;
/// The largest object that a read takes into memory (a part holds at most one large frame).
const OBJECT_READ_LIMIT: u64 = 4 << 30;
const JSON_LIMIT: u64 = 64 << 20;

/// The project as `GET /api/v1/projects/<p>` gives it.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ProjectInfo {
    #[serde(default)]
    pub id: Option<String>,
    pub mode: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Segment {
    pub writer: String,
    pub first: u64,
    pub last: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Ref {
    pub name: String,
    pub target: Option<String>,
    #[serde(default)]
    pub version: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RefUpdate {
    pub name: String,
    pub expect: Option<String>,
    pub target: Option<String>,
    pub record: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cas {
    Done(Vec<Ref>),
    /// The refs that moved on the server: name and the target there.
    Conflict(Vec<(String, Option<String>)>),
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginStart {
    pub code: String,
    pub link: String,
    #[serde(default)]
    pub expires: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Session {
    pub login: String,
    pub token: String,
    #[serde(default)]
    pub expires: i64,
}

pub struct Api {
    http: Client,
    /// `https://<host>/`.
    origin: reqwest::Url,
    project: String,
    token: Option<String>,
    /// The local project id: a server project without records has no id yet, and the first push names it.
    local_project: Mutex<String>,
    /// `Layr-Server` of the last answer.
    server: Arc<Mutex<Option<String>>>,
    /// Record segments listed in this command, by scope; a segment upload clears them.
    segments: Arc<Mutex<std::collections::BTreeMap<String, Vec<Segment>>>>,
    /// The project as this command first read it.
    info: Arc<Mutex<Option<ProjectInfo>>>,
    /// Whether the server holds a state (`<scope><state>`), as this command learned it.
    held: Arc<Mutex<std::collections::BTreeMap<String, bool>>>,
    single_put_limit: usize,
}

impl Clone for Api {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            origin: self.origin.clone(),
            project: self.project.clone(),
            token: self.token.clone(),
            local_project: Mutex::new(self.local_project.lock().expect("api project lock").clone()),
            server: self.server.clone(),
            segments: self.segments.clone(),
            info: self.info.clone(),
            held: self.held.clone(),
            single_put_limit: self.single_put_limit,
        }
    }
}

/// The scope of a store key: `overlays/<name>/` for an overlay layer, empty for the project.
fn split_scope(key: &str) -> Result<(String, &str)> {
    if let Some(rest) = key.strip_prefix("overlays/") {
        let (name, rest) = rest.split_once('/').context("overlay key without a path")?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
            bail!("invalid overlay name in a Layr Cloud key");
        }
        return Ok((format!("overlays/{name}/"), rest));
    }
    Ok((String::new(), key))
}

/// Map the legacy request object names to the plain Cloud API routes.
fn person_request_path(key: &str) -> Option<String> {
    let rest = key.strip_prefix("requests/")?;
    let (claim, file) = match rest.strip_prefix("claims/") {
        Some(file) => (true, file),
        None => (false, rest),
    };
    let id = file.strip_suffix(".json")?;
    if !layr_store::request::request_id(id) {
        return None;
    }
    Some(if claim { format!("person-requests/{id}/claim") } else { format!("person-requests/{id}") })
}

/// The keys of the Secret layout: they have no route here, a read finds nothing and a listing is empty.
fn secret_layout(rest: &str) -> bool {
    rest == "public.json"
        || ["slots/", "keys/", "epochs/", "project/", "history/", "index/", "requests/", "claims/", "git/", "chunks/"]
            .iter()
            .any(|p| rest.starts_with(p))
}

fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

impl Api {
    /// Upload a release file with bounded memory and bind its existing file key to the object.
    pub(crate) fn put_release_file(&self, key: &str, mut file: std::fs::File) -> Result<bool> {
        use layr_hashers::{ContentAddress, Digest};
        let name = key.strip_prefix("files/").filter(|_| format::release_file(key)).context("invalid release file key")?;
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(0))?;
        let mut hasher = ContentAddress::new();
        hasher.update(format!("release {size}\0").as_bytes());
        let mut read = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            read += n as u64;
        }
        if read != size {
            bail!("release file changed during hashing");
        }
        let id = hex::encode(hasher.finalize());
        file.seek(SeekFrom::Start(0))?;
        if size <= self.single_put_limit as u64 {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let url = self.project_url(&format!("objects/{id}"))?;
            let response = self.send(true, || {
                self.request(Method::PUT, url.clone())
                    .header("Layr-Object-Type", "release")
                    .header("Content-Type", "application/octet-stream")
                    .body(bytes.clone())
            })?;
            if !response.status().is_success() {
                return Err(self.failure("release upload", response));
            }
        } else if !self.objects_missing(std::slice::from_ref(&id))?.is_empty() {
            let url = self.project_url(&format!("objects/{id}/uploads"))?;
            let body = serde_json::json!({ "size": size });
            let response =
                self.send(false, || self.request(Method::POST, url.clone()).header("Layr-Object-Type", "release").json(&body))?;
            #[derive(Deserialize)]
            struct Upload {
                upload: String,
                part_size: usize,
            }
            let upload: Upload = self.json("multipart start", response)?;
            if upload.part_size != 32 << 20
                || upload.upload.is_empty()
                || !upload.upload.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                bail!("Layr Cloud multipart start: invalid answer");
            }
            let mut left = size;
            let mut index = 1;
            while left > 0 {
                let length = (left as usize).min(upload.part_size);
                let mut part = vec![0; length];
                file.read_exact(&mut part)?;
                let url = self.project_url(&format!("objects/{id}/uploads/{}/{index}", upload.upload))?;
                let response = self.send(true, || {
                    self.request(Method::PUT, url.clone()).header("Content-Type", "application/octet-stream").body(part.clone())
                })?;
                if !response.status().is_success() {
                    return Err(self.failure("release upload part", response));
                }
                left -= length as u64;
                index += 1;
            }
            let url = self.project_url(&format!("objects/{id}/uploads/{}/complete", upload.upload))?;
            let response = self.send(true, || self.request(Method::POST, url.clone()).json(&serde_json::json!({})))?;
            if !response.status().is_success() {
                return Err(self.failure("release upload completion", response));
            }
        }
        let url = self.project_url(&format!("release-files/{name}"))?;
        let body = serde_json::json!({"id":id});
        let response = self.send(true, || self.request(Method::PUT, url.clone()).json(&body))?;
        match response.status().as_u16() {
            201 => Ok(true),
            200 => Ok(false),
            _ => Err(self.failure("release file binding", response)),
        }
    }

    /// Stream a bound release object; the signed release record verifies its size and SHA-256.
    pub(crate) fn release_stream(&self, key: &str) -> Result<Option<Box<dyn Read>>> {
        let name = key.strip_prefix("files/").filter(|_| format::release_file(key)).context("invalid release file key")?;
        let url = self.project_url(&format!("release-files/{name}"))?;
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Binding {
            id: String,
        }
        let id = self.json::<Binding>("release file binding", response)?.id;
        Id::parse(&id)?;
        let url = self.project_url(&format!("objects/{id}"))?;
        let mut response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status().is_redirection() {
            let location =
                response.headers().get("location").and_then(|v| v.to_str().ok()).context("redirect without a location")?;
            let target = url.join(location)?;
            if target.scheme() != "https" && !cfg!(test) {
                bail!("Layr Cloud redirected an object to a URL that is not HTTPS");
            }
            response = super::gateway::send_again(true, || {
                self.http.get(target.clone()).send().map_err(|e| anyhow!("Layr Cloud object URL: {e}"))
            })?;
        }
        if !response.status().is_success() {
            return Err(self.failure("release object", response));
        }
        Ok(Some(Box::new(response)))
    }
    pub fn open(url: &str) -> Result<Self> {
        let address = url.strip_prefix("layr+https://").context("a Layr Cloud remote URL starts with layr+https://")?;
        let (host, project) = address.split_once('/').context("the Layr Cloud URL needs a project name")?;
        Self::with_origin(&format!("https://{host}/"), project)
    }

    fn with_origin(origin: &str, project: &str) -> Result<Self> {
        if project.is_empty()
            || project.len() > 44
            || !project.as_bytes()[0].is_ascii_alphanumeric()
            || !project.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            bail!("invalid Layr Cloud project name {project:?}");
        }
        let origin = reqwest::Url::parse(origin).map_err(|_| anyhow!("invalid Layr Cloud URL"))?;
        if origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.path() != "/"
        {
            bail!("the Layr Cloud URL needs a host and one project name, without credentials or query");
        }
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            http,
            origin,
            project: project.into(),
            token: None,
            local_project: Mutex::new(String::new()),
            server: Arc::new(Mutex::new(None)),
            segments: Arc::new(Mutex::new(Default::default())),
            info: Arc::new(Mutex::new(None)),
            held: Arc::new(Mutex::new(Default::default())),
            single_put_limit: SINGLE_PUT_LIMIT,
        })
        .and_then(|mut api| {
            // The CA of the process, as for the gateway client; `connect` sets the command's own after it.
            if let Some(path) = std::env::var_os("LAYR_GATEWAY_CA_FILE").filter(|p| !p.is_empty()) {
                api.use_ca(&std::fs::read(&path).context("read LAYR_GATEWAY_CA_FILE")?)?;
            }
            Ok(api)
        })
    }

    /// Trust the certificates of `pem` for the server (`LAYR_GATEWAY_CA_FILE`).
    fn use_ca(&mut self, pem: &[u8]) -> Result<()> {
        let certificates = reqwest::Certificate::from_pem_bundle(pem).context("invalid CA file")?;
        if certificates.is_empty() {
            bail!("the CA file has no certificates");
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600));
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        self.http = builder.build()?;
        Ok(())
    }

    /// A client of a test server over plain HTTP.
    #[cfg(test)]
    pub(crate) fn for_test(origin: &str, project: &str, token: Option<&str>) -> Self {
        let mut api = Self::with_origin(origin, project).unwrap();
        api.token = token.map(str::to_owned);
        api
    }

    pub fn host(&self) -> &str {
        self.origin.host_str().unwrap_or_default()
    }

    /// The token of this command: `LAYR_GATEWAY_TOKEN_FILE` (a machine or job token), else the session that
    /// `layr login` saved for this host.
    pub fn connect(&mut self, ctx: &crate::ctx::Ctx, local_project: &str, anonymous: bool) -> Result<()> {
        *self.local_project.lock().map_err(|_| anyhow!("api project lock"))? = local_project.into();
        // A private CA for a test or an internal server (the same setting as the gateway).
        if let Some(path) = ctx.env("LAYR_GATEWAY_CA_FILE") {
            let pem = crate::privs::as_caller_fs(ctx, || Ok(std::fs::read(ctx.cwd.join(path))?))?;
            self.use_ca(&pem)?;
        }
        if anonymous {
            self.token = None;
            return Ok(());
        }
        if let Some(path) = ctx.env("LAYR_GATEWAY_TOKEN_FILE") {
            self.token = Some(crate::privs::as_caller_fs(ctx, || token_file(&ctx.cwd.join(path), ctx.caller.uid))?);
            return Ok(());
        }
        let path = session_file(&ctx.caller.home, self.host());
        self.token = crate::privs::as_caller_fs(ctx, || -> Result<Option<String>> {
            if !path.exists() {
                return Ok(None);
            }
            let session: Session = serde_json::from_str(&token_file_text(&path, ctx.caller.uid, 8192)?)?;
            if session.expires != 0 && session.expires <= crate::ids::now_ms() {
                return Ok(None);
            }
            Ok(Some(session.token))
        })?;
        Ok(())
    }

    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    pub fn server_version(&self) -> Option<String> {
        self.server.lock().ok().and_then(|s| s.clone())
    }

    fn project_url(&self, path: &str) -> Result<reqwest::Url> {
        let path = if path.is_empty() {
            format!("api/v1/projects/{}", self.project)
        } else {
            format!("api/v1/projects/{}/{path}", self.project)
        };
        Ok(self.origin.join(&path)?)
    }

    fn request(&self, method: Method, url: reqwest::Url) -> RequestBuilder {
        let mut request = self.http.request(method, url).header("Layr-Client", client_version());
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
    }

    /// Sends a request again while the server is busy (and on a lost connection for an idempotent one), then
    /// turns `426` into the minimum version the server needs.
    fn send(&self, idempotent: bool, build: impl Fn() -> RequestBuilder) -> Result<Response> {
        let response = super::gateway::send_again(idempotent, || build().send().map_err(|e| anyhow!("Layr Cloud: {e}")))?;
        if let Some(version) = response.headers().get("layr-server").and_then(|v| v.to_str().ok()) {
            if let Ok(mut server) = self.server.lock() {
                *server = Some(version.to_string());
            }
        }
        if response.status() == StatusCode::UPGRADE_REQUIRED {
            let body: serde_json::Value = serde_json::from_slice(&read_limited(response, 4096)?).unwrap_or_default();
            let minimum = body["error"]["minimum"].as_str().unwrap_or("a newer version");
            bail!("Layr Cloud needs layr {minimum} or newer; this is layr {}: update layr", client_version());
        }
        Ok(response)
    }

    /// An error for an answer that the caller did not expect.
    fn failure(&self, what: &str, response: Response) -> anyhow::Error {
        let status = response.status().as_u16();
        let body: serde_json::Value =
            serde_json::from_slice(&read_limited(response, 65536).unwrap_or_default()).unwrap_or_default();
        let code = body["error"]["code"].as_str().unwrap_or("");
        let message = body["error"]["message"].as_str().unwrap_or("");
        let server = self.server_version().map(|v| format!(", server {v}")).unwrap_or_default();
        let hint = match status {
            401 => "; sign in with layr login <remote> or set LAYR_GATEWAY_TOKEN_FILE",
            403 => "; the token has no right for this action",
            _ => "",
        };
        anyhow!("Layr Cloud {what}: HTTP {status} {code} {message}{server}{hint}")
    }

    fn json<T: serde::de::DeserializeOwned>(&self, what: &str, response: Response) -> Result<T> {
        if !response.status().is_success() {
            return Err(self.failure(what, response));
        }
        serde_json::from_slice(&read_limited(response, JSON_LIMIT)?).with_context(|| format!("Layr Cloud {what}: invalid JSON"))
    }

    /// The project, read once per command while it has an id (a project without records gets one at its
    /// first push).
    pub(crate) fn project_info(&self) -> Result<Option<ProjectInfo>> {
        if let Some(info) = self.info.lock().map_err(|_| anyhow!("api info lock"))?.clone() {
            return Ok(Some(info));
        }
        let url = self.project_url("")?;
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let info: ProjectInfo = self.json("project", response)?;
        if info.id.as_ref().is_some_and(|id| !id.is_empty()) {
            *self.info.lock().map_err(|_| anyhow!("api info lock"))? = Some(info.clone());
        }
        Ok(Some(info))
    }

    /// The older read API also serves project info. Its records route reports `no such route`;
    /// only that exact response permits legacy discovery. Other errors must not select a protocol.
    pub(crate) fn has_v1_records(&self) -> Result<bool> {
        let mut url = self.project_url("records")?;
        url.query_pairs_mut().append_pair("limit", "1");
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status() == StatusCode::NOT_FOUND {
            let body: serde_json::Value = serde_json::from_slice(&read_limited(response, 65536)?).unwrap_or_default();
            if body["error"]["code"] == "not_found" && body["error"]["message"] == "no such route" {
                return Ok(false);
            }
            bail!("Layr Cloud records: HTTP 404: project or records are not accessible");
        }
        #[derive(Deserialize)]
        struct Page {
            segments: Vec<Segment>,
            next: Option<String>,
        }
        let page: Page = self.json("records", response)?;
        let _ = (page.segments, page.next);
        Ok(true)
    }

    /// `store.json` as the rest of the store code reads it: the plain descriptor of the project mode.
    fn descriptor(&self) -> Result<Option<Vec<u8>>> {
        let Some(info) = self.project_info()? else { return Ok(None) };
        let mode = format::Mode::parse(&info.mode)?;
        if mode.is_secret() {
            bail!("Layr Cloud project {} is Secret; add the remote with --mode secret", self.project);
        }
        let local = self.local_project.lock().map_err(|_| anyhow!("api project lock"))?.clone();
        let project = match info.id.filter(|id| !id.is_empty()) {
            Some(id) => id,
            None if !local.is_empty() => local,
            None => return Ok(None),
        };
        let descriptor = format::Descriptor { format: 2, project, recipient: super::keys::PLAIN.into(), mode };
        Ok(Some(serde_json::to_vec(&descriptor)?))
    }

    pub(crate) fn objects_missing(&self, ids: &[String]) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for batch in ids.chunks(1000) {
            let url = self.project_url("objects/missing")?;
            let body = serde_json::json!({ "ids": batch });
            let response = self.send(true, || self.request(Method::POST, url.clone()).json(&body))?;
            #[derive(Deserialize)]
            struct Missing {
                missing: Vec<String>,
            }
            missing.extend(self.json::<Missing>("objects/missing", response)?.missing);
        }
        Ok(missing)
    }

    /// Uploads one object: true when it is new to the project. An object over 64 MiB goes up in parts, after
    /// a check that the project does not hold it yet.
    pub(crate) fn put_object(&self, kind: Kind, bytes: &[u8]) -> Result<(String, bool)> {
        let id = Keying::Plain.id(kind, bytes).hex();
        let kind = kind.name();
        if bytes.len() > self.single_put_limit {
            if self.objects_missing(std::slice::from_ref(&id))?.is_empty() {
                return Ok((id, false));
            }
            self.put_parts(kind, &id, bytes)?;
            return Ok((id, true));
        }
        let url = self.project_url(&format!("objects/{id}"))?;
        let response = self.send(true, || {
            self.request(Method::PUT, url.clone())
                .header("Layr-Object-Type", kind)
                .header("Content-Type", "application/octet-stream")
                .body(bytes.to_vec())
        })?;
        match response.status().as_u16() {
            201 => Ok((id, true)),
            200 => Ok((id, false)),
            _ => Err(self.failure(&format!("upload of object {id}"), response)),
        }
    }

    fn put_parts(&self, kind: &str, id: &str, bytes: &[u8]) -> Result<()> {
        let url = self.project_url(&format!("objects/{id}/uploads"))?;
        let body = serde_json::json!({ "size": bytes.len() });
        let response =
            self.send(false, || self.request(Method::POST, url.clone()).header("Layr-Object-Type", kind).json(&body))?;
        #[derive(Deserialize)]
        struct Upload {
            upload: String,
            part_size: usize,
        }
        let upload: Upload = self.json("multipart start", response)?;
        if upload.part_size == 0 || !upload.upload.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
            bail!("Layr Cloud multipart start: invalid answer");
        }
        for (index, part) in bytes.chunks(upload.part_size).enumerate() {
            let url = self.project_url(&format!("objects/{id}/uploads/{}/{}", upload.upload, index + 1))?;
            let response = self.send(true, || {
                self.request(Method::PUT, url.clone()).header("Content-Type", "application/octet-stream").body(part.to_vec())
            })?;
            if !response.status().is_success() {
                return Err(self.failure(&format!("upload of part {} of object {id}", index + 1), response));
            }
        }
        let url = self.project_url(&format!("objects/{id}/uploads/{}/complete", upload.upload))?;
        let response = self.send(true, || self.request(Method::POST, url.clone()).json(&serde_json::json!({})))?;
        if !response.status().is_success() {
            return Err(self.failure(&format!("multipart completion of object {id}"), response));
        }
        Ok(())
    }

    /// The bytes of an object, checked against its typed id. A `302` goes to a signed URL without the token.
    pub(crate) fn get_object(&self, kind: Kind, id: &str) -> Result<Option<Vec<u8>>> {
        let want = Id::parse(id)?;
        let url = self.project_url(&format!("objects/{id}"))?;
        let mut response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status().is_redirection() {
            let location =
                response.headers().get("location").and_then(|v| v.to_str().ok()).context("redirect without a location")?;
            let target = url.join(location)?;
            if target.scheme() != "https" && !cfg!(test) {
                bail!("Layr Cloud redirected an object to a URL that is not HTTPS");
            }
            response = super::gateway::send_again(true, || {
                self.http.get(target.clone()).send().map_err(|e| anyhow!("Layr Cloud object URL: {e}"))
            })?;
        }
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.failure(&format!("read of object {id}"), response));
        }
        let bytes = read_limited(response, OBJECT_READ_LIMIT)?;
        if Keying::Plain.id(kind, &bytes) != want {
            bail!("Layr Cloud object {id} does not match its id");
        }
        Ok(Some(bytes))
    }

    pub(crate) fn segments(&self, scope: &str) -> Result<Vec<Segment>> {
        if let Some(listed) = self.segments.lock().map_err(|_| anyhow!("api listing lock"))?.get(scope) {
            return Ok(listed.clone());
        }
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = self.project_url(&format!("{scope}records"))?;
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(after) = &cursor {
                url.query_pairs_mut().append_pair("after", after);
            }
            let response = self.send(true, || self.request(Method::GET, url.clone()))?;
            #[derive(Deserialize)]
            struct Page {
                segments: Vec<Segment>,
                next: Option<String>,
            }
            let page: Page = self.json("records", response)?;
            all.extend(page.segments);
            match page.next {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                Some(_) => bail!("Layr Cloud records: the listing cursor repeats"),
                None => break,
            }
        }
        for segment in &all {
            if !crate::model::is_uuid(&segment.writer) || segment.first == 0 || segment.first > segment.last {
                bail!("Layr Cloud records: invalid segment");
            }
        }
        self.segments.lock().map_err(|_| anyhow!("api listing lock"))?.insert(scope.into(), all.clone());
        Ok(all)
    }

    fn segment_url(&self, scope: &str, writer: &str, first: u64, last: u64) -> Result<reqwest::Url> {
        self.project_url(&format!("{scope}records/{writer}/{first}-{last}"))
    }

    pub(crate) fn get_segment(&self, scope: &str, writer: &str, first: u64, last: u64) -> Result<Option<Vec<u8>>> {
        let url = self.segment_url(scope, writer, first, last)?;
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.failure("record segment", response));
        }
        Ok(Some(read_limited(response, JSON_LIMIT)?))
    }

    /// Uploads a segment: true when the server took it, false when it holds these records already.
    pub(crate) fn post_segment(&self, scope: &str, writer: &str, first: u64, last: u64, body: Vec<u8>) -> Result<bool> {
        let url = self.segment_url(scope, writer, first, last)?;
        let response = self.send(false, || {
            self.request(Method::POST, url.clone()).header("Content-Type", "application/octet-stream").body(body.clone())
        })?;
        self.segments.lock().map_err(|_| anyhow!("api listing lock"))?.remove(scope);
        match response.status().as_u16() {
            200 | 201 => Ok(true),
            409 => {
                let answer: serde_json::Value = serde_json::from_slice(&read_limited(response, 65536)?).unwrap_or_default();
                let held = answer["last"].as_u64().or_else(|| answer["error"]["last"].as_u64()).unwrap_or(0);
                if held >= last {
                    return Ok(false);
                }
                bail!("Layr Cloud holds records of machine {writer} up to {held}; segment {first}-{last} leaves a gap")
            }
            _ => Err(self.failure(&format!("upload of record segment {first}-{last}"), response)),
        }
    }

    /// The states of `ids` that the server lacks: `POST /states/missing` (at most 1000 per request).
    pub(crate) fn states_missing(&self, scope: &str, ids: &[String]) -> Result<Vec<String>> {
        let mut missing = Vec::new();
        for batch in ids.chunks(1000) {
            let url = self.project_url(&format!("{scope}states/missing"))?;
            let body = serde_json::json!({ "ids": batch });
            let response = self.send(true, || self.request(Method::POST, url.clone()).json(&body))?;
            #[derive(Deserialize)]
            struct Missing {
                missing: Vec<String>,
            }
            missing.extend(self.json::<Missing>("states/missing", response)?.missing);
        }
        Ok(missing)
    }

    /// Learns with one batch call which of `ids` the server holds, so the reads after ask nothing per state.
    pub(crate) fn learn_states(&self, scope: &str, ids: &[String]) -> Result<()> {
        let unknown: Vec<String> = {
            let held = self.held.lock().map_err(|_| anyhow!("api state lock"))?;
            ids.iter().filter(|id| !held.contains_key(&format!("{scope}{id}"))).cloned().collect()
        };
        if unknown.is_empty() {
            return Ok(());
        }
        let missing: std::collections::BTreeSet<String> = self.states_missing(scope, &unknown)?.into_iter().collect();
        let mut held = self.held.lock().map_err(|_| anyhow!("api state lock"))?;
        for id in unknown {
            let has = !missing.contains(&id);
            held.insert(format!("{scope}{id}"), has);
        }
        Ok(())
    }

    /// Whether the server holds a state: from `learn_states`, else one `GET /states/<state>`.
    pub(crate) fn state_held(&self, scope: &str, state: &str) -> Result<bool> {
        if let Some(has) = self.held.lock().map_err(|_| anyhow!("api state lock"))?.get(&format!("{scope}{state}")) {
            return Ok(*has);
        }
        let has = self.get_state(scope, state)?.is_some();
        self.held.lock().map_err(|_| anyhow!("api state lock"))?.insert(format!("{scope}{state}"), has);
        Ok(has)
    }

    /// Uploads the index of a pack whose data went up as an `opaque` object (`PUT /packs/<id>`). The server
    /// checks the index against the data and every entry against its id.
    pub(crate) fn put_pack_index(&self, pack: &str, index: &[u8]) -> Result<()> {
        let url = self.project_url(&format!("packs/{pack}"))?;
        let response = self.send(true, || {
            self.request(Method::PUT, url.clone()).header("Content-Type", "application/octet-stream").body(index.to_vec())
        })?;
        if !response.status().is_success() {
            return Err(self.failure(&format!("index of pack {pack}"), response));
        }
        Ok(())
    }

    pub(crate) fn get_pack_index(&self, pack: &str) -> Result<Vec<u8>> {
        let url = self.project_url(&format!("packs/{pack}"))?;
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if !response.status().is_success() {
            return Err(self.failure(&format!("index of pack {pack}"), response));
        }
        read_limited(response, JSON_LIMIT)
    }

    /// The pack that holds each object of `ids` (`POST /objects/locate`, at most 1000 per request): object id
    /// to pack id. An id that no pack of the project holds is left out.
    pub(crate) fn locate(&self, ids: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
        let mut found = std::collections::BTreeMap::new();
        for batch in ids.chunks(1000) {
            let url = self.project_url("objects/locate")?;
            let body = serde_json::json!({ "ids": batch });
            let response = self.send(true, || self.request(Method::POST, url.clone()).json(&body))?;
            #[derive(Deserialize)]
            struct Located {
                packs: std::collections::BTreeMap<String, String>,
            }
            for (object, pack) in self.json::<Located>("objects/locate", response)?.packs {
                Id::parse(&object)?;
                Id::parse(&pack)?;
                found.insert(object, pack);
            }
        }
        Ok(found)
    }

    /// Publishes a state: `PUT /states/<state> {"times","tree"}` (docs/objects.md, "States"). True when the
    /// server took it, false when it holds another body for the state.
    pub(crate) fn put_state(&self, scope: &str, state: &str, r: &StateRef) -> Result<bool> {
        let url = self.project_url(&format!("{scope}states/{state}"))?;
        let body = serde_json::json!({ "times": r.times.hex(), "tree": r.tree.hex() });
        let response = self.send(true, || self.request(Method::PUT, url.clone()).json(&body))?;
        let done = match response.status().as_u16() {
            200 | 201 => true,
            409 => false,
            _ => return Err(self.failure(&format!("state {state}"), response)),
        };
        self.held.lock().map_err(|_| anyhow!("api state lock"))?.insert(format!("{scope}{state}"), true);
        Ok(done)
    }

    pub(crate) fn get_state(&self, scope: &str, state: &str) -> Result<Option<StateRef>> {
        let url = self.project_url(&format!("{scope}states/{state}"))?;
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct State {
            tree: String,
            times: String,
        }
        let body = self.json::<State>(&format!("state {state}"), response)?;
        Ok(Some(StateRef { tree: Id::parse(&body.tree)?, times: Id::parse(&body.times)? }))
    }

    pub(crate) fn refs(&self, scope: &str, prefix: &str) -> Result<Vec<Ref>> {
        let mut url = self.project_url(&format!("{scope}refs"))?;
        url.query_pairs_mut().append_pair("prefix", prefix);
        let response = self.send(true, || self.request(Method::GET, url.clone()))?;
        #[derive(Deserialize)]
        struct Refs {
            refs: Vec<Ref>,
        }
        Ok(self.json::<Refs>("refs", response)?.refs)
    }

    pub(crate) fn cas(&self, scope: &str, updates: &[RefUpdate], message: &str) -> Result<Cas> {
        let url = self.project_url(&format!("{scope}refs/cas"))?;
        let body = serde_json::json!({ "updates": updates, "message": message });
        let response = self.send(false, || self.request(Method::POST, url.clone()).json(&body))?;
        match response.status().as_u16() {
            200 => {
                #[derive(Deserialize)]
                struct Refs {
                    refs: Vec<Ref>,
                }
                Ok(Cas::Done(self.json::<Refs>("refs/cas", response)?.refs))
            }
            409 => {
                let answer: serde_json::Value = serde_json::from_slice(&read_limited(response, JSON_LIMIT)?).unwrap_or_default();
                let list = answer["conflicts"]
                    .as_array()
                    .or_else(|| answer["error"]["conflicts"].as_array())
                    .cloned()
                    .unwrap_or_default();
                Ok(Cas::Conflict(
                    list.iter()
                        .map(|c| (c["name"].as_str().unwrap_or("").to_string(), c["actual"].as_str().map(str::to_owned)))
                        .collect(),
                ))
            }
            403 => {
                let answer: serde_json::Value = serde_json::from_slice(&read_limited(response, JSON_LIMIT)?).unwrap_or_default();
                let rule = answer["rule"].as_str().or_else(|| answer["error"]["rule"].as_str()).unwrap_or("role");
                let missing =
                    answer["missing"].as_array().or_else(|| answer["error"]["missing"].as_array()).cloned().unwrap_or_default();
                let missing: Vec<String> =
                    missing.iter().map(|m| m.as_str().map(str::to_owned).unwrap_or_else(|| m.to_string())).collect();
                bail!("Layr Cloud refused the ref move: protection rule {rule} (missing: {})", missing.join(", "))
            }
            _ => Err(self.failure("refs/cas", response)),
        }
    }

    /// Registers the public signing key of a machine for this token (docs/remotes.md, "Machine key").
    pub(crate) fn register_machine(&self, machine: &str, key: &ed25519_dalek::VerifyingKey) -> Result<()> {
        let url = self.origin.join(&format!("api/v1/machines/{machine}"))?;
        let body = serde_json::json!({ "public_key": hex::encode(key.to_bytes()) });
        let response = self.send(true, || self.request(Method::PUT, url.clone()).json(&body))?;
        match response.status().as_u16() {
            200 | 201 => Ok(()),
            409 => bail!("Layr Cloud has another key for machine {machine}"),
            _ => Err(self.failure("machine key registration", response)),
        }
    }

    /// The sign-in link to show: a relative link joins the remote origin; an absolute one must name the same origin
    /// over HTTPS, so a server answer cannot send the person to another site.
    pub(crate) fn login_link(&self, link: &str) -> Result<reqwest::Url> {
        let url = self.origin.join(link).map_err(|_| anyhow!("Layr Cloud gave an invalid sign-in link"))?;
        let https = url.scheme() == "https" || (cfg!(test) && url.scheme() == self.origin.scheme());
        if !https
            || url.host_str() != self.origin.host_str()
            || url.port_or_known_default() != self.origin.port_or_known_default()
        {
            bail!("Layr Cloud gave a sign-in link outside {}; nothing is shown", self.origin);
        }
        Ok(url)
    }

    pub(crate) fn login_start(&self) -> Result<LoginStart> {
        let url = self.origin.join("api/v1/login")?;
        let response = self.send(false, || self.request(Method::POST, url.clone()))?;
        self.json("login", response)
    }

    /// The session once the person signed in, None while the server waits for the person.
    pub(crate) fn login_poll(&self, code: &str) -> Result<Option<Session>> {
        if code.is_empty() || !code.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
            bail!("invalid login code");
        }
        let url = self.origin.join(&format!("api/v1/login/{code}"))?;
        let response = self.send(true, || self.request(Method::POST, url.clone()))?;
        if response.status() == StatusCode::ACCEPTED {
            return Ok(None);
        }
        Ok(Some(self.json("login", response)?))
    }

    // ----- the store key space -----

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (scope, rest) = split_scope(key)?;
        if rest == "store.json" {
            return if scope.is_empty() { self.descriptor() } else { Ok(None) };
        }
        if let Some((writer, first, last)) = format::parse_segment_name(rest) {
            return self.get_segment(&scope, &writer, first, last);
        }
        if let Some(path) = person_request_path(rest).filter(|_| scope.is_empty()) {
            let url = self.project_url(&path)?;
            let response = self.send(true, || self.request(Method::GET, url.clone()))?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !response.status().is_success() {
                return Err(self.failure("person request", response));
            }
            return Ok(Some(read_limited(response, layr_store::request::MAX_BYTES as u64)?));
        }
        if scope.is_empty() && format::public_release_file(rest) {
            let path = rest.strip_prefix("public/release/").unwrap();
            let url = self.project_url(&format!("public-release/{path}"))?;
            let response = self.send(true, || self.request(Method::GET, url.clone()))?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !response.status().is_success() {
                return Err(self.failure("public release file", response));
            }
            return Ok(Some(read_limited(response, 32 << 20)?));
        }
        if secret_layout(rest) {
            return Ok(None);
        }
        bail!("{key} has no Layr Cloud API route")
    }

    pub fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let (scope, rest) = split_scope(key)?;
        if let Some((writer, first, last)) = format::parse_segment_name(rest) {
            return self.post_segment(&scope, &writer, first, last, body);
        }
        if let Some(path) = person_request_path(rest).filter(|_| scope.is_empty()) {
            let url = self.project_url(&path)?;
            let response = self.send(true, || {
                self.request(Method::PUT, url.clone()).header("Content-Type", "application/json").body(body.clone())
            })?;
            return match response.status().as_u16() {
                200 | 201 => Ok(true),
                409 => Ok(false),
                _ => Err(self.failure("person request", response)),
            };
        }
        if scope.is_empty() && format::public_release_file(rest) {
            let path = rest.strip_prefix("public/release/").unwrap();
            let url = self.project_url(&format!("public-release/{path}"))?;
            let response = self.send(true, || {
                self.request(Method::PUT, url.clone()).header("Content-Type", "application/octet-stream").body(body.clone())
            })?;
            return match response.status().as_u16() {
                201 => Ok(true),
                200 => Ok(false),
                _ => Err(self.failure("public release file", response)),
            };
        }
        bail!("{key} cannot be written to Layr Cloud; the server keeps it")
    }

    /// Visit accepted requests in order. A false return stops before the next request or page.
    pub fn visit_requests(&self, mut visit: impl FnMut(Object) -> Result<bool>) -> Result<()> {
        #[derive(Deserialize)]
        struct Item {
            id: String,
            modified: i64,
        }
        #[derive(Deserialize)]
        struct Listing {
            requests: Vec<Item>,
            next: Option<String>,
        }
        let seen = crate::db::Db::scratch()?;
        seen.execute_batch("CREATE TABLE seen(id TEXT PRIMARY KEY) WITHOUT ROWID; BEGIN;")?;
        let mut insert = seen.prepare_cached("INSERT OR IGNORE INTO seen(id) VALUES(?1)")?;
        let mut after: Option<String> = None;
        for _ in 0..10_000 {
            let mut url = self.project_url("person-requests")?;
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(cursor) = &after {
                url.query_pairs_mut().append_pair("after", cursor);
            }
            let response = self.send(true, || self.request(Method::GET, url.clone()))?;
            let listing: Listing = self.json("person requests", response)?;
            for item in &listing.requests {
                if !layr_store::request::request_id(&item.id) || insert.execute([&item.id])? == 0 {
                    bail!("Layr Cloud returned an invalid or repeated person request id");
                }
            }
            if let Some(next) = &listing.next {
                if listing.requests.last().is_none_or(|item| &item.id != next) || after.as_ref() == Some(next) {
                    bail!("Layr Cloud returned an invalid person request cursor");
                }
            }
            for item in listing.requests {
                if !visit(Object { key: format!("requests/{}.json", item.id), modified: item.modified })? {
                    return Ok(());
                }
            }
            match listing.next {
                Some(cursor) => after = Some(cursor),
                None => return Ok(()),
            }
        }
        bail!("Layr Cloud person request listing exceeds 10000 pages")
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        let (scope, rest) = split_scope(prefix)?;
        if scope.is_empty() && rest == "requests/" {
            let mut result = Vec::new();
            self.visit_requests(|object| {
                result.push(object);
                Ok(true)
            })?;
            return Ok(result);
        }
        if rest == "records/" || rest.starts_with("records/") {
            let writer = rest.strip_prefix("records/").unwrap_or("").trim_end_matches('/');
            return Ok(self
                .segments(&scope)?
                .into_iter()
                .filter(|s| writer.is_empty() || s.writer == writer)
                .map(|s| Object { key: format!("{scope}{}", format::segment_name(&s.writer, s.first, s.last)), modified: 0 })
                .collect());
        }
        if rest.is_empty() || rest.starts_with("states/") || rest.starts_with("objects/") {
            bail!("Layr Cloud keeps states and objects by id (docs/objects.md); {prefix:?} has no listing");
        }
        if secret_layout(rest) || rest.starts_with("files/") {
            return Ok(Vec::new());
        }
        bail!("{prefix} has no Layr Cloud API listing")
    }
}

fn read_limited(response: Response, limit: u64) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit) {
        bail!("Layr Cloud answer is too large");
    }
    let mut data = Vec::new();
    response.take(limit + 1).read_to_end(&mut data).map_err(|e| anyhow!("Layr Cloud body read failed: {e}"))?;
    if data.len() as u64 > limit {
        bail!("Layr Cloud answer is too large");
    }
    Ok(data)
}

/// The session file that `layr login` writes for a host.
pub(crate) fn session_file(home: &std::path::Path, host: &str) -> std::path::PathBuf {
    home.join(".config/layr/cloud").join(format!("{host}.json"))
}

fn token_file_text(path: &std::path::Path, uid: u32, limit: u64) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open token file {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o600 {
        bail!("token file {} must be a regular file owned by the caller with mode 0600", path.display());
    }
    let mut text = String::new();
    file.take(limit + 1).read_to_string(&mut text).map_err(|_| anyhow!("invalid token file"))?;
    if text.len() as u64 > limit {
        bail!("token file is too large");
    }
    Ok(text)
}

fn token_file(path: &std::path::Path, uid: u32) -> Result<String> {
    let text = token_file_text(path, uid, 4096)?;
    let token = text.trim_end_matches(['\r', '\n']);
    if token.is_empty()
        || !token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~' | b'+' | b'/' | b'='))
    {
        bail!("invalid token file");
    }
    Ok(token.into())
}

/// Writes the session of `layr login`, owner-only.
pub(crate) fn save_session(path: &std::path::Path, session: &Session) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = path.parent().context("session folder")?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    crate::fsutil::atomic_write(path, &serde_json::to_vec(session)?, 0o600)
}
