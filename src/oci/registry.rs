//! Read-only OCI Distribution endpoints, served by the daemon's request processes.

use super::{Artifact, Cache, Export, MANIFEST};
use crate::ctx::{Caller, Ctx};
use crate::repo::Repo;
use crate::store::{Project, Store};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

/// Configuration is read at daemon start. Several projects can share a listener.
pub fn listeners(root: &Path) -> Result<Vec<(String, TcpListener)>> {
    let store = Store::open(root)?;
    let mut names = BTreeSet::new();
    for name in store.project_names()? {
        let p = Project::open(&store, &name)?;
        if let Some(v) = p.config_value("registry.listen")? {
            let addr = v.as_str().ok_or_else(|| anyhow!("registry.listen must be a socket address"))?;
            addr.parse::<SocketAddr>().context("registry.listen needs an IP address and port")?;
            names.insert(addr.to_string());
        }
    }
    let mut out = Vec::new();
    for name in names {
        let l = TcpListener::bind(&name).with_context(|| format!("registry.listen {name}"))?;
        l.set_nonblocking(true)?;
        eprintln!("layr registry: listening on {}", l.local_addr()?);
        out.push((name, l));
    }
    Ok(out)
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    artifact: Artifact,
    state: String,
    revision: String,
    restrictions: String,
}

fn index(cache: &Cache, runnable: &str, chart: bool) -> std::path::PathBuf {
    cache.root.join("manifests").join(if chart { "charts" } else { "images" }).join(runnable)
}

pub fn remember(e: &Export, revision: &str) -> Result<()> {
    for (chart, artifact) in [(false, Some(&e.image)), (true, e.chart.as_ref())] {
        if let Some(a) = artifact {
            let dir = index(&e.cache, &e.runnable, chart);
            fs::create_dir_all(&dir)?;
            let stored = Stored {
                artifact: a.clone(),
                state: e.state.clone(),
                revision: revision.into(),
                restrictions: e.restrictions.clone(),
            };
            crate::fsutil::atomic_write(&dir.join(super::digest_hex(&a.manifest.digest)?), &serde_json::to_vec(&stored)?, 0o600)?;
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
pub struct TokenRecord {
    pub uid: u32,
    pub machine: String,
    pub until: i64,
}

fn token_uid(p: &Project, token: &str) -> Result<Option<u32>> {
    let tokens: BTreeMap<String, TokenRecord> =
        p.config_value("registry.tokens")?.map(serde_json::from_value).transpose()?.unwrap_or_default();
    let hash = super::digest(token.as_bytes());
    Ok(tokens.get(&hash).filter(|r| r.until > crate::ids::now_ms() && r.machine == p.store.machine.id).map(|r| r.uid))
}

struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
}

fn request(stream: &TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.take(16 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 3 || !["HTTP/1.1", "HTTP/1.0"].contains(&parts[2]) {
        bail!("invalid request line");
    }
    let (method, target) = (parts[0].into(), parts[1].into());
    let mut authorization = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("incomplete HTTP headers");
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("authorization") {
                if authorization.is_some() {
                    bail!("duplicate authorization header");
                }
                authorization = Some(value.trim().into());
            }
        } else {
            bail!("invalid HTTP header");
        }
    }
    Ok(Request { method, target, authorization })
}

fn token(header: Option<&str>, basic: bool) -> Option<String> {
    let h = header?;
    if let Some(t) = h.strip_prefix("Bearer ") {
        return Some(t.into());
    }
    if basic {
        let raw = base64::engine::general_purpose::STANDARD.decode(h.strip_prefix("Basic ")?).ok()?;
        let text = String::from_utf8(raw).ok()?;
        return Some(text.split_once(':')?.1.into());
    }
    None
}

fn context(root: &Path, uid: u32) -> Result<Ctx> {
    Ok(Ctx {
        store: Store::open(root)?,
        caller: Caller::from_uid(uid),
        cwd: "/".into(),
        env: HashMap::new(),
        out: std::cell::RefCell::new(Box::new(std::io::sink())),
    })
}

fn enabled(p: &Project, listen: &str) -> Result<bool> {
    Ok(p.config_value("registry.listen")?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(listen))
}

fn anonymous(p: &Project) -> bool {
    let mut policy = crate::access::Policy::load(p);
    // Anonymous access has only the '*' principal, without a local uid or groups.
    policy.access.retain(|p, _| p == "*");
    policy.grants.retain(|p, _| p == "*");
    policy.deny.retain(|p, _| p == "*");
    p.config_bool("registry.anonymous", false)
        && policy.check(&crate::access::Who { uid: u32::MAX, groups: vec![] }, "read").is_ok()
}

fn principal(p: &Project, req: &Request, basic: bool) -> Result<Option<u32>> {
    match token(req.authorization.as_deref(), basic) {
        Some(t) => token_uid(p, &t),
        None if anonymous(p) => Ok(Some(u32::MAX)),
        None => Ok(None),
    }
}

fn response(
    stream: &mut TcpStream,
    code: u16,
    body: &[u8],
    media: &str,
    digest: Option<&str>,
    extra: &[(&str, String)],
    head: bool,
) -> Result<()> {
    headers(stream, code, body.len() as u64, media, digest, extra)?;
    if !head {
        stream.write_all(body)?;
    }
    Ok(())
}

fn headers(
    stream: &mut TcpStream,
    code: u16,
    len: u64,
    media: &str,
    digest: Option<&str>,
    extra: &[(&str, String)],
) -> Result<()> {
    let reason = match code {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    write!(stream, "HTTP/1.1 {code} {reason}\r\nContent-Length: {len}\r\nContent-Type: {media}\r\nDocker-Distribution-Api-Version: registry/2.0\r\nConnection: close\r\n")?;
    if let Some(d) = digest {
        write!(stream, "Docker-Content-Digest: {d}\r\n")?;
    }
    for (k, v) in extra {
        if v.contains(['\r', '\n']) {
            bail!("invalid response header");
        }
        write!(stream, "{k}: {v}\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    Ok(())
}

fn error(stream: &mut TcpStream, code: u16, name: &str, head: bool) -> Result<()> {
    response(
        stream,
        code,
        &serde_json::to_vec(&json!({"errors": [{"code": name, "message": name}]}))?,
        "application/json",
        None,
        &[],
        head,
    )
}

fn challenge(stream: &mut TcpStream, realm: &str, repository: Option<&str>, head: bool) -> Result<()> {
    let mut value = format!("Bearer realm=\"{realm}\",service=\"layr\"");
    if let Some(r) = repository {
        value.push_str(&format!(",scope=\"repository:{r}:pull\""));
    }
    response(
        stream,
        401,
        b"{\"errors\":[{\"code\":\"UNAUTHORIZED\"}]}",
        "application/json",
        None,
        &[("WWW-Authenticate", value)],
        head,
    )
}

fn realm(p: Option<&Project>, address: SocketAddr) -> Result<String> {
    let base = p
        .map(|p| p.config_value("registry.public_url"))
        .transpose()?
        .flatten()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("http://{address}"));
    let u = reqwest::Url::parse(&base)?;
    if !["https", "http"].contains(&u.scheme())
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("registry.public_url must be an HTTP(S) origin");
    }
    Ok(format!("{}/token", base.trim_end_matches('/')))
}

/// Ref read authorization stays here when Core gains narrower ref policy.
fn authorize_ref(repo: &Repo, ctx: &Ctx, _revision: &str) -> Result<()> {
    repo.can_read(ctx)
}

/// Core integration hook: append env/release refs when Db::refs lands.
fn tags(repo: &Repo) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    for l in repo.lines()? {
        let tag = format!("line-{}", l.name);
        if super::valid_tag(&tag) {
            names.insert(tag);
        }
        names.insert(format!("state-{}", l.head));
    }
    Ok(names.into_iter().collect())
}

pub fn handle(mut stream: TcpStream, root: &Path, listen: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let result = (|| -> Result<()> {
        let req = match request(&stream) {
            Ok(r) => r,
            Err(_) => return error(&mut stream, 400, "DENIED", false),
        };
        let head = req.method == "HEAD";
        if req.method != "GET" && !head {
            return error(&mut stream, 405, "UNSUPPORTED", false);
        }
        let target = reqwest::Url::parse(&format!("http://registry{}", req.target))?;
        let address = stream.local_addr()?;
        let store = Store::open(root)?;
        if target.path() == "/token" || target.path().trim_end_matches('/') == "/v2" {
            let scope = target.query_pairs().find(|(k, _)| k == "scope").map(|(_, v)| v.into_owned());
            let project = scope
                .as_deref()
                .and_then(|s| s.strip_prefix("repository:"))
                .and_then(|s| s.split(':').next())
                .and_then(|s| s.split('/').next());
            let mut token_realm = realm(None, address)?;
            for name in store.project_names()? {
                if project.is_some() && project != Some(name.as_str()) {
                    continue;
                }
                let p = Project::open(&store, &name)?;
                if !enabled(&p, listen)? {
                    continue;
                }
                token_realm = realm(Some(&p), address)?;
                if let Some(uid) = principal(&p, &req, true)? {
                    let ctx = context(root, uid)?;
                    if Repo::open(&ctx, &name).is_err() {
                        continue;
                    }
                    let body = if target.path() == "/token" {
                        json!({"token": token(req.authorization.as_deref(), true).unwrap_or_default(), "expires_in": 60})
                    } else {
                        json!({})
                    };
                    return response(&mut stream, 200, &serde_json::to_vec(&body)?, "application/json", None, &[], head);
                }
            }
            return challenge(&mut stream, &token_realm, None, head);
        }
        let path = match target.path().strip_prefix("/v2/") {
            Some(p) => p,
            None => return error(&mut stream, 404, "NAME_UNKNOWN", head),
        };
        let parts: Vec<_> = path.split('/').collect();
        let (project, runnable, chart, endpoint, reference) = match parts.as_slice() {
            [p, r, e, v] => (*p, *r, false, *e, *v),
            [p, "charts", r, e, v] => (*p, *r, true, *e, *v),
            _ => return error(&mut stream, 404, "NAME_UNKNOWN", head),
        };
        if super::valid_component(project).is_err() || super::valid_component(runnable).is_err() {
            return error(&mut stream, 404, "NAME_UNKNOWN", head);
        }
        let p = match Project::open(&store, project) {
            Ok(p) if enabled(&p, listen)? => p,
            _ => return error(&mut stream, 404, "NAME_UNKNOWN", head),
        };
        let repo_path = if chart { format!("{project}/charts/{runnable}") } else { format!("{project}/{runnable}") };
        let uid = match principal(&p, &req, false)? {
            Some(u) => u,
            None => return challenge(&mut stream, &realm(Some(&p), address)?, Some(&repo_path), head),
        };
        let ctx = context(root, uid)?;
        let repo = match Repo::open(&ctx, project) {
            Ok(r) => r,
            Err(_) => return error(&mut stream, 403, "DENIED", head),
        };
        let cache = Cache::open(&repo.p.meta_dir().join("oci"))?;
        if endpoint == "tags" && reference == "list" {
            let mut list = tags(&repo)?;
            list.retain(|t| super::tag_revision(t).map(|r| authorize_ref(&repo, &ctx, &r).is_ok()).unwrap_or(false));
            // OCI pagination uses lexical order and excludes the last returned tag.
            if let Some(last) = target.query_pairs().find(|(k, _)| k == "last").map(|(_, v)| v.into_owned()) {
                list.retain(|s| s > &last);
            }
            let n = target.query_pairs().find(|(k, _)| k == "n").and_then(|(_, v)| v.parse::<usize>().ok());
            let mut extra = Vec::new();
            if let Some(n) = n {
                if n > 0 && list.len() > n {
                    list.truncate(n);
                    extra
                        .push(("Link", format!("</v2/{repo_path}/tags/list?n={n}&last={}>; rel=\"next\"", list.last().unwrap())));
                }
            }
            return response(
                &mut stream,
                200,
                &serde_json::to_vec(&json!({"name": repo_path, "tags": list}))?,
                "application/json",
                None,
                &extra,
                head,
            );
        }
        if endpoint == "manifests" {
            let a = if reference.starts_with("sha256:") {
                let path = match super::digest_hex(reference) {
                    Ok(h) => index(&cache, runnable, chart).join(h),
                    Err(_) => return error(&mut stream, 404, "MANIFEST_UNKNOWN", head),
                };
                let st: Stored = match fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                    Some(s) => s,
                    None => return error(&mut stream, 404, "MANIFEST_UNKNOWN", head),
                };
                if authorize_ref(&repo, &ctx, &st.revision).is_err()
                    || super::restriction_key(&repo, &st.state).map(|key| st.restrictions != key).unwrap_or(true)
                {
                    return error(&mut stream, 403, "DENIED", head);
                }
                st.artifact
            } else {
                let revision = match super::tag_revision(reference) {
                    Ok(r) => r,
                    Err(_) => return error(&mut stream, 404, "MANIFEST_UNKNOWN", head),
                };
                if authorize_ref(&repo, &ctx, &revision).is_err() {
                    return error(&mut stream, 403, "DENIED", head);
                }
                let state = match super::resolve_tag(&repo, reference) {
                    Ok(s) => s,
                    Err(_) => return error(&mut stream, 404, "MANIFEST_UNKNOWN", head),
                };
                let source_line = repo.p.db.line_by_name(&revision)?;
                let e = match super::export(&repo, &state, source_line.as_ref(), Some(runnable)) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("layr registry: export failed: {e:#}");
                        return error(&mut stream, 404, "MANIFEST_UNKNOWN", head);
                    }
                };
                remember(&e, &revision)?;
                if chart {
                    match e.chart {
                        Some(c) => c,
                        None => return error(&mut stream, 404, "MANIFEST_UNKNOWN", head),
                    }
                } else {
                    e.image
                }
            };
            return response(&mut stream, 200, &cache.read(&a.manifest)?, MANIFEST, Some(&a.manifest.digest), &[], head);
        }
        if endpoint == "blobs" {
            let hex = match super::digest_hex(reference) {
                Ok(h) => h,
                Err(_) => return error(&mut stream, 404, "BLOB_UNKNOWN", head),
            };
            // A global blob cache is never an authorization source. Require a manifest in this repository.
            let mut descriptor = None;
            if let Ok(files) = fs::read_dir(index(&cache, runnable, chart)) {
                for file in files.flatten() {
                    let st: Stored = match fs::read(file.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                        Some(s) => s,
                        None => continue,
                    };
                    if authorize_ref(&repo, &ctx, &st.revision).is_err()
                        || super::restriction_key(&repo, &st.state).map(|key| st.restrictions != key).unwrap_or(true)
                    {
                        continue;
                    }
                    for d in st.artifact.layers.iter().chain(std::iter::once(&st.artifact.config)) {
                        if d.digest == reference {
                            descriptor = Some(d.clone());
                            break;
                        }
                    }
                    if descriptor.is_some() {
                        break;
                    }
                }
            }
            let d = match descriptor {
                Some(d) => d,
                None => return error(&mut stream, 404, "BLOB_UNKNOWN", head),
            };
            let mut f = match File::open(cache.root.join("blobs").join(hex)) {
                Ok(f) => f,
                Err(_) => return error(&mut stream, 404, "BLOB_UNKNOWN", head),
            };
            headers(&mut stream, 200, f.metadata()?.len(), &d.media_type, Some(reference), &[])?;
            if !head {
                std::io::copy(&mut f, &mut stream)?;
            }
            return Ok(());
        }
        error(&mut stream, 404, "NAME_UNKNOWN", head)
    })();
    if let Err(e) = result {
        eprintln!("layr registry: request failed: {e:#}");
    }
}
