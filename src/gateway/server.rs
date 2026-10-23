//! Gateway routes and grants. Storage only receives validated relative object names.

use super::{
    auth,
    config::{ConfigFile, Project},
};
use crate::http;
use anyhow::{bail, Result};
use base64::Engine;
use serde_json::json;
use std::sync::Arc;

pub const OBJECT_LIMIT: usize = 32 * 1024 * 1024;
pub const SLOT_LIMIT: usize = 16 * 1024;
pub const JSON_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Reader,
    Writer,
    Gc,
}

pub struct Object {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_range: Option<String>,
}

pub struct Entry {
    pub key: String,
    pub modified: i64,
}

pub trait Storage: Send + Sync {
    fn get(&self, project: &Project, key: &str, range: Option<http::Range>, limit: usize) -> Result<Option<Object>>;
    fn list(&self, project: &Project, prefix: &str, role: Role) -> Result<Vec<Entry>>;
    fn put(&self, project: &Project, key: &str, body: Vec<u8>) -> Result<bool>;
    fn delete(&self, project: &Project, key: &str) -> Result<()>;
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn bytes(status: u16, body: Vec<u8>, media: &str) -> Self {
        Self {
            status,
            body,
            headers: vec![
                ("Content-Type".into(), media.into()),
                ("Cache-Control".into(), "no-store".into()),
                (
                    "Content-Security-Policy".into(),
                    "default-src 'none'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".into(),
                ),
                ("Strict-Transport-Security".into(), "max-age=63072000".into()),
                ("X-Content-Type-Options".into(), "nosniff".into()),
                ("X-Frame-Options".into(), "DENY".into()),
                ("Referrer-Policy".into(), "no-referrer".into()),
                ("Cross-Origin-Opener-Policy".into(), "same-origin".into()),
            ],
        }
    }
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self::bytes(status, value.to_string().into_bytes(), "application/json")
    }
    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, json!({"error":message}))
    }
}

pub struct Gateway {
    pub config: Arc<ConfigFile>,
    pub proof: auth::Proof,
    pub storage: Arc<dyn Storage>,
    pub listen: std::net::SocketAddr,
}

fn decode(raw: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = raw.as_bytes().iter().copied();
    while let Some(b) = input.next() {
        bytes.push(if b == b'%' {
            let a = (input.next()? as char).to_digit(16)?;
            let b = (input.next()? as char).to_digit(16)?;
            (a * 16 + b) as u8
        } else {
            b
        });
    }
    String::from_utf8(bytes).ok()
}

fn safe_part(part: &str) -> bool {
    !part.is_empty()
        && !matches!(part, "." | "..")
        && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

pub fn object_name(name: &str) -> bool {
    name.len() <= 1024
        && matches!(name.split('/').next(), Some("records" | "states" | "slots"))
        && name.contains('/')
        && name.split('/').all(safe_part)
}

pub fn gc_name(name: &str) -> bool {
    object_name(name) && name.starts_with("states/") && name.split('/').count() == 3 && name.ends_with(".data.age")
}

fn list_prefix(prefix: &str) -> bool {
    prefix.is_empty()
        || (prefix.len() <= 1024
            && matches!(prefix.split('/').next(), Some("records" | "states" | "slots"))
            && prefix.trim_end_matches('/').split('/').all(safe_part))
}

impl Gateway {
    pub fn handle(&self, request: http::Request) -> Reply {
        match self.route(request) {
            Ok(reply) => reply,
            Err(error) => {
                if let Some(error) = error.downcast_ref::<http::Error>() {
                    return Reply::error(error.status, error.message);
                }
                eprintln!("layr gateway: request failed: {error:#}");
                Reply::error(502, "storage request failed")
            }
        }
    }

    fn route(&self, request: http::Request) -> Result<Reply> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let config = self.config.get()?;
        if matches!(request.method.as_str(), "GET" | "DELETE") && !request.body.is_empty() {
            return Ok(Reply::error(400, "this method has no body"));
        }
        if config.listen != self.listen {
            return Ok(Reply::error(503, "restart the gateway after a listen change"));
        }
        let host = request.header("host").unwrap_or("");
        let given = host.split_once(':').map(|(name, port)| (name, port.parse::<u16>().ok()));
        let host_ok = match given {
            Some((name, Some(port))) => name.eq_ignore_ascii_case(&config.host) && port == self.listen.port(),
            Some(_) => false,
            None => host.eq_ignore_ascii_case(&config.host),
        };
        if !host_ok {
            return Ok(Reply::error(421, "wrong host name"));
        }
        let (path, query) = request.target.split_once('?').unwrap_or((&request.target, ""));
        let parts = path.strip_prefix('/').unwrap_or("").split('/').map(decode).collect::<Option<Vec<_>>>();
        let Some(parts) = parts else {
            return Ok(Reply::error(400, "invalid path encoding"));
        };
        if parts.iter().any(|p| !safe_part(p)) {
            return Ok(Reply::error(400, "invalid path"));
        }
        if parts.len() < 4 || parts[0] != "v1" || parts[1] != "p" {
            return Ok(Reply::error(404, "no such route"));
        }
        let name = &parts[2];
        let Some(project) = config.projects.get(name) else {
            return Ok(Reply::error(404, "no such project"));
        };
        let action = &parts[3];
        let root = parts.len() == 4;
        let binding = project.binding(name);
        let token = request.header("authorization").and_then(|s| s.strip_prefix("Bearer ")).unwrap_or("");
        let at = auth::now();
        let read = self.proof.valid("read", &binding, token, at);
        let write = auth::grant(token, project, false, at);
        let gc = auth::grant(token, project, true, at);
        let empty = |status| Reply::bytes(status, Vec::new(), "application/json");
        if action != "objects" && !query.is_empty() {
            return Ok(Reply::error(400, "unexpected query"));
        }
        match (request.method.as_str(), action.as_str(), root) {
            ("GET", "store.json", true) => {
                return Ok(match self.storage.get(project, "store.json", None, JSON_LIMIT)? {
                    Some(object) => Reply::bytes(200, object.body, "application/json"),
                    None => Reply::error(404, "store is not initialized"),
                });
            }
            ("GET", "slots", true) => {
                let entries = self.storage.list(project, "slots/", Role::Reader)?;
                if entries.len() > 1024 {
                    return Ok(Reply::error(502, "too many store slots"));
                }
                let mut slots = Vec::new();
                let mut size = 0;
                for entry in entries {
                    if std::time::Instant::now() >= deadline {
                        return Ok(Reply::error(408, "request timed out"));
                    }
                    if !object_name(&entry.key) || !entry.key.starts_with("slots/") {
                        bail!("invalid slot key from storage");
                    }
                    if let Some(object) = self.storage.get(project, &entry.key, None, SLOT_LIMIT)? {
                        size += object.body.len();
                        if size > 4 * 1024 * 1024 {
                            bail!("store slots are too large");
                        }
                        slots.push(json!({"name":entry.key.strip_prefix("slots/").unwrap().strip_suffix(".age").unwrap_or(entry.key.strip_prefix("slots/").unwrap()),
                                          "data":base64::engine::general_purpose::STANDARD.encode(object.body)}));
                    }
                }
                return Ok(Reply::json(200, json!({"slots":slots})));
            }
            ("GET", "challenge", true) => {
                return Ok(Reply::json(200, json!({"challenge":self.proof.challenge(&binding, &project.recipient, at)?})));
            }
            ("POST", "token", true) => {
                if request.body.len() > JSON_LIMIT {
                    return Ok(Reply::error(413, "token request is too large"));
                }
                let value: serde_json::Value = match serde_json::from_slice(&request.body) {
                    Ok(value) => value,
                    Err(_) => return Ok(Reply::error(400, "invalid token request")),
                };
                let proof = value.get("challenge").and_then(|v| v.as_str()).unwrap_or("");
                return Ok(match self.proof.exchange(&binding, proof, at) {
                    Some((token, expires)) => Reply::json(200, json!({"token":token,"expires":expires})),
                    None => Reply::error(401, "invalid challenge proof"),
                });
            }
            ("GET", "objects", true) => {
                let prefix = match query.strip_prefix("prefix=").and_then(decode) {
                    Some(p) if !query.contains('&') && list_prefix(&p) => p,
                    None if query.is_empty() => String::new(),
                    _ => return Ok(Reply::error(400, "invalid object prefix")),
                };
                let role = if read {
                    Role::Reader
                } else if write && (prefix.starts_with("records/") || prefix.starts_with("states/")) {
                    Role::Writer
                } else {
                    return Ok(Reply::error(401, "a read challenge token or scoped write token is required"));
                };
                let objects = self.storage.list(project, &prefix, role)?;
                if objects.len() > 10_000 {
                    bail!("too many listed objects");
                }
                let objects: Vec<_> = objects.into_iter().map(|e| json!({"key":e.key,"modified":e.modified})).collect();
                return Ok(Reply::json(200, json!({"objects":objects})));
            }
            ("GET" | "PUT" | "DELETE", "objects", false) => {}
            _ => return Ok(Reply::error(405, "method or route is not supported")),
        }
        if !query.is_empty() {
            return Ok(Reply::error(400, "object requests have no query"));
        }
        let key = parts[4..].join("/");
        if !object_name(&key) && !(request.method == "GET" && key == "store.json") {
            return Ok(Reply::error(400, "invalid object name"));
        }
        match request.method.as_str() {
            "GET" => {
                if !read {
                    return Ok(Reply::error(401, "a read challenge token is required"));
                }
                let range = request.header("range").map(http::Range::parse).transpose()?;
                Ok(match self.storage.get(project, &key, range, OBJECT_LIMIT)? {
                    Some(object) => {
                        let mut reply = Reply::bytes(object.status, object.body, "application/octet-stream");
                        reply.headers.push(("Accept-Ranges".into(), "bytes".into()));
                        if let Some(range) = object.content_range {
                            reply.headers.push(("Content-Range".into(), range));
                        }
                        reply
                    }
                    None => Reply::error(404, "no such object"),
                })
            }
            "PUT" => {
                if key.starts_with("slots/") {
                    if !read {
                        return Ok(Reply::error(403, "slot writes require a read challenge token"));
                    }
                    if request.body.len() > SLOT_LIMIT {
                        return Ok(Reply::error(413, "slot is too large"));
                    }
                } else if !write {
                    return Ok(Reply::error(403, "a write token is required"));
                }
                if request.header("content-length").is_none() {
                    return Ok(Reply::error(411, "Content-Length is required"));
                }
                Ok(if self.storage.put(project, &key, request.body)? {
                    empty(201)
                } else {
                    Reply::error(412, "object already exists")
                })
            }
            "DELETE" => {
                if !gc || !gc_name(&key) {
                    return Ok(Reply::error(403, "GC can delete only state data parts"));
                }
                self.storage.delete(project, &key)?;
                Ok(empty(204))
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);
    impl Storage for Memory {
        fn get(&self, _: &Project, key: &str, range: Option<http::Range>, _: usize) -> Result<Option<Object>> {
            let objects = self.0.lock().unwrap();
            let Some(body) = objects.get(key) else {
                return Ok(None);
            };
            Ok(Some(match range {
                Some(range) => {
                    let (start, end) =
                        range.resolve(body.len() as u64).ok_or(http::Error { status: 416, message: "outside object" })?;
                    Object {
                        status: 206,
                        body: body[start as usize..=end as usize].to_vec(),
                        content_range: Some(format!("bytes {start}-{end}/{}", body.len())),
                    }
                }
                None => Object { status: 200, body: body.clone(), content_range: None },
            }))
        }
        fn list(&self, _: &Project, prefix: &str, _: Role) -> Result<Vec<Entry>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|s| s.starts_with(prefix))
                .map(|s| Entry { key: s.clone(), modified: 0 })
                .collect())
        }
        fn put(&self, _: &Project, key: &str, body: Vec<u8>) -> Result<bool> {
            let mut objects = self.0.lock().unwrap();
            if objects.contains_key(key) {
                return Ok(false);
            }
            objects.insert(key.into(), body);
            Ok(true)
        }
        fn delete(&self, _: &Project, key: &str) -> Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }
    struct Fixture {
        gateway: Gateway,
        identity: age::x25519::Identity,
        config: super::super::config::Config,
        path: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let identity = age::x25519::Identity::generate();
            let mut config = super::super::config::sample();
            config.projects.get_mut("example").unwrap().recipient = identity.to_public().to_string();
            let path = std::env::temp_dir().join(format!("layr-gateway-routes-{:016x}.json", rand::random::<u64>()));
            std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
            let gateway = Gateway {
                config: Arc::new(ConfigFile::new(path.clone())),
                proof: auth::Proof::new(),
                listen: config.listen,
                storage: Arc::new(Memory(Mutex::new(BTreeMap::new()))),
            };
            Self { gateway, identity, config, path }
        }
        fn request(&self, method: &str, path: &str, token: &str, body: &[u8], range: Option<&str>) -> Reply {
            let mut headers: BTreeMap<String, String> =
                [("host".into(), "localhost".into()), ("content-length".into(), body.len().to_string())].into();
            if !token.is_empty() {
                headers.insert("authorization".into(), format!("Bearer {token}"));
            }
            if let Some(range) = range {
                headers.insert("range".into(), range.into());
            }
            self.gateway.handle(http::Request { method: method.into(), target: path.into(), headers, body: body.to_vec() })
        }
        fn token(&mut self, gc: bool) -> String {
            let token = auth::issue(gc, 60_000, auth::now()).unwrap();
            let project = self.config.projects.get_mut("example").unwrap();
            if gc {
                project.gc_token_hashes.push(auth::hash(&token));
            } else {
                project.write_token_hashes.push(auth::hash(&token));
            }
            std::fs::write(&self.path, serde_json::to_vec(&self.config).unwrap()).unwrap();
            token
        }
        fn read_token(&self) -> String {
            let reply = self.request("GET", "/v1/p/example/challenge", "", b"", None);
            assert_eq!(reply.status, 200);
            let value: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
            let bytes = base64::engine::general_purpose::STANDARD.decode(value["challenge"].as_str().unwrap()).unwrap();
            let proof = String::from_utf8(age::decrypt(&self.identity, &bytes).unwrap()).unwrap();
            let body = json!({"challenge":proof}).to_string();
            let reply = self.request("POST", "/v1/p/example/token", "", body.as_bytes(), None);
            assert_eq!(reply.status, 200);
            serde_json::from_slice::<serde_json::Value>(&reply.body).unwrap()["token"].as_str().unwrap().into()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn grants_are_separate_and_put_is_immutable() {
        let mut f = Fixture::new();
        let write = f.token(false);
        let gc = f.token(true);
        let read = f.read_token();
        let path = "/v1/p/example/objects/records/machine/1-1.age";
        assert_eq!(f.request("PUT", path, &read, b"first", None).status, 403);
        assert_eq!(f.request("PUT", path, &gc, b"first", None).status, 403);
        assert_eq!(f.request("PUT", path, &write, b"first", None).status, 201);
        assert_eq!(f.request("PUT", path, &write, b"second", None).status, 412);
        assert_eq!(f.request("GET", path, &write, b"", None).status, 401);
        for prefix in ["records/", "records/machine/", "states/", "states/state/"] {
            assert_eq!(f.request("GET", &format!("/v1/p/example/objects?prefix={prefix}"), &write, b"", None).status, 200);
        }
        for prefix in ["", "slots/", "records", "states"] {
            assert_ne!(f.request("GET", &format!("/v1/p/example/objects?prefix={prefix}"), &write, b"", None).status, 200);
        }

        assert_eq!(f.request("GET", path, &read, b"", None).body, b"first");
        let slot = "/v1/p/example/objects/slots/machine-id.age";
        assert_eq!(f.request("PUT", slot, &write, b"slot", None).status, 403);
        assert_eq!(f.request("PUT", slot, &read, b"slot", None).status, 201);
        assert_eq!(f.request("PUT", "/v1/p/example/objects/store.json", &write, b"{}", None).status, 400);
        assert_eq!(f.request("DELETE", path, &gc, b"", None).status, 403);
        let data = "/v1/p/example/objects/states/state/attempt.0001.data.age";
        assert_eq!(f.request("PUT", data, &write, b"data", None).status, 201);
        assert_eq!(f.request("DELETE", data, &write, b"", None).status, 403);
        assert_eq!(f.request("DELETE", data, &gc, b"", None).status, 204);
        assert_eq!(f.request("GET", data, &read, b"", None).status, 404);
    }
    #[test]
    fn encoded_paths_ranges_and_config_changes_do_not_bypass_grants() {
        let mut f = Fixture::new();
        let write = f.token(false);
        let read = f.read_token();
        for name in ["records/%2e%2e/secret", "records/a%2fb/secret", "slots/%5csecret", "slots/%00", "states//x", "states/%xx"] {
            assert_eq!(f.request("PUT", &format!("/v1/p/example/objects/{name}"), &write, b"x", None).status, 400, "{name}");
        }
        let path = "/v1/p/example/objects/states/state/attempt.0001.data.age";
        assert_eq!(f.request("PUT", path, &write, b"abcdef", None).status, 201);
        let reply = f.request("GET", path, &read, b"", Some("bytes=1-3"));
        assert_eq!(reply.status, 206);
        assert_eq!(reply.body, b"bcd");
        assert!(reply.headers.contains(&("Content-Range".into(), "bytes 1-3/6".into())));
        assert_eq!(f.request("GET", path, &read, b"", Some("bytes=9-")).status, 416);
        assert_eq!(f.request("GET", path, &read, b"", Some("bytes=0-1,3-4")).status, 416);
        f.config.projects.get_mut("example").unwrap().write_token_hashes.clear();
        std::fs::write(&f.path, serde_json::to_vec(&f.config).unwrap()).unwrap();
        assert_eq!(f.request("PUT", path, &write, b"new", None).status, 403);
        f.config.projects.get_mut("example").unwrap().recipient = age::x25519::Identity::generate().to_public().to_string();
        std::fs::write(&f.path, serde_json::to_vec(&f.config).unwrap()).unwrap();
        assert_eq!(f.request("GET", path, &read, b"", None).status, 401);
    }
}
