//! The Layr Cloud API v1 client against an in-process fake server (layr-cloud docs/api.md).

use super::api::{Api, Cas, RefUpdate};
use super::backend::Backend;
use super::{writer, Options, Store};
use crate::ctx::{Caller, Ctx};
use crate::model::{LineChange, LineState, Record, StateRec};
use crate::repo::Repo;
use layr_objects::Kind;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "machine.token";

type Upload = (String, String, BTreeMap<usize, Vec<u8>>);

#[derive(Default)]
struct FakeState {
    project_id: Option<String>,
    objects: BTreeMap<String, (String, Vec<u8>)>,
    /// Upload id to the object id, its type and the parts by number.
    uploads: BTreeMap<String, Upload>,
    segments: BTreeMap<(String, String, u64, u64), Vec<u8>>,
    /// A state: its tree and times table.
    states: BTreeMap<(String, String), (String, String)>,
    /// Object id to the pack that holds it (from the pack indexes).
    located: BTreeMap<String, String>,
    /// Pack id to its index.
    indexes: BTreeMap<String, Vec<u8>>,
    refs: BTreeMap<(String, String), (String, u64)>,
    machines: BTreeMap<String, String>,
    minimum: Option<String>,
    part_size: usize,
    login_polls: usize,
    request_pages: BTreeMap<String, serde_json::Value>,
    log: Vec<String>,
}

struct Fake {
    origin: String,
    state: Arc<Mutex<FakeState>>,
}

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query =
        query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (k.to_string(), v.replace("%2F", "/"))).collect();
    Some(Request { method, path: path.to_string(), query, headers, body })
}

fn reply(stream: &mut TcpStream, status: u16, body: &[u8], extra: &[(&str, &str)]) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nLayr-Server: fake-1\r\n", body.len());
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn json(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

impl Fake {
    fn start(project_id: Option<&str>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(FakeState {
            project_id: project_id.map(str::to_owned),
            part_size: 8 << 20,
            ..Default::default()
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().unwrap();
                    let mut reader = BufReader::new(stream);
                    while let Some(request) = read_request(&mut reader) {
                        let (status, body, extra) = Fake::handle(&state, request);
                        let extra: Vec<(&str, &str)> = extra.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                        reply(&mut writer, status, &body, &extra);
                    }
                });
            }
        });
        Fake { origin, state }
    }

    fn api(&self) -> Api {
        Api::for_test(&self.origin, "p", Some(TOKEN))
    }

    fn handle(state: &Mutex<FakeState>, request: Request) -> (u16, Vec<u8>, Vec<(String, String)>) {
        let mut s = state.lock().unwrap();
        s.log.push(format!("{} {}", request.method, request.path));
        assert!(request.headers.contains_key("layr-client"), "every request names the client version");
        if let Some(minimum) = &s.minimum {
            return (426, json(serde_json::json!({"error":{"code":"client_too_old","minimum":minimum}})), Vec::new());
        }
        let authorized = request.headers.get("authorization").map(String::as_str) == Some(&format!("Bearer {TOKEN}"));
        let write = request.method != "GET";
        if request.path.starts_with("/api/v1/login") {
            if request.path == "/api/v1/login" {
                return (200, json(serde_json::json!({"code":"c1","link":"/#/login?code=c1","expires":0})), Vec::new());
            }
            s.login_polls += 1;
            if s.login_polls < 2 {
                return (202, Vec::new(), Vec::new());
            }
            return (200, json(serde_json::json!({"login":"ann@example.com","token":TOKEN,"expires":0})), Vec::new());
        }
        if write && !authorized {
            return (401, json(serde_json::json!({"error":{"code":"unauthorized","message":"no token"}})), Vec::new());
        }
        if let Some(machine) = request.path.strip_prefix("/api/v1/machines/") {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let key = body["public_key"].as_str().unwrap().to_string();
            return match s.machines.get(machine) {
                Some(old) if old != &key => (409, Vec::new(), Vec::new()),
                _ => {
                    s.machines.insert(machine.into(), key);
                    (200, Vec::new(), Vec::new())
                }
            };
        }
        let Some(rest) = request.path.strip_prefix("/api/v1/projects/p") else { return (404, Vec::new(), Vec::new()) };
        if rest.is_empty() {
            return (200, json(serde_json::json!({"name":"p","mode":"private","role":"admin","id":s.project_id})), Vec::new());
        }
        let rest = rest.strip_prefix('/').unwrap();
        let (scope, rest) = match rest.strip_prefix("overlays/").and_then(|r| r.split_once('/')) {
            Some((name, rest)) => (format!("overlays/{name}/"), rest),
            None => (String::new(), rest),
        };
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let parts: Vec<&str> = rest.split('/').collect();
        match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["person-requests"]) => {
                let cursor = request.query.get("after").map(String::as_str).unwrap_or("");
                match s.request_pages.get(cursor) {
                    Some(page) => (200, json(page.clone()), Vec::new()),
                    None => (404, Vec::new(), Vec::new()),
                }
            }
            ("POST", ["objects", "missing"]) => {
                let missing: Vec<&str> = body["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|id| !s.objects.contains_key(*id) && !s.located.contains_key(*id))
                    .collect();
                (200, json(serde_json::json!({ "missing": missing })), Vec::new())
            }
            ("POST", ["objects", "locate"]) => {
                let packs: BTreeMap<&str, &String> = body["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|id| s.located.get(id).map(|pack| (id, pack)))
                    .collect();
                (200, json(serde_json::json!({ "packs": packs })), Vec::new())
            }
            ("PUT", ["packs", pack]) => {
                let Some((_, data)) = s.objects.get(*pack) else { return (400, Vec::new(), Vec::new()) };
                let Ok(index) = layr_objects::pack::Index::decode(&request.body, data.len() as u64) else {
                    return (400, Vec::new(), Vec::new());
                };
                if index.pack.hex() != *pack {
                    return (400, Vec::new(), Vec::new());
                }
                for entry in &index.entries {
                    s.located.insert(entry.id.hex(), pack.to_string());
                }
                s.indexes.insert(pack.to_string(), request.body);
                (200, Vec::new(), Vec::new())
            }
            ("GET", ["packs", pack]) => match s.indexes.get(*pack) {
                Some(index) => (200, index.clone(), Vec::new()),
                None => (404, Vec::new(), Vec::new()),
            },
            ("PUT", ["objects", id]) => {
                let kind = request.headers.get("layr-object-type").cloned().unwrap_or_default();
                if request.body.len() > super::api::SINGLE_PUT_LIMIT || typed(&kind, &request.body) != *id {
                    return (400, json(serde_json::json!({"error":{"code":"invalid"}})), Vec::new());
                }
                let new = s.objects.insert(id.to_string(), (kind, request.body)).is_none();
                (if new { 201 } else { 200 }, Vec::new(), Vec::new())
            }
            ("GET", ["objects", id]) => match s.objects.get(*id) {
                Some((_, bytes)) => (200, bytes.clone(), Vec::new()),
                None => (404, Vec::new(), Vec::new()),
            },
            ("POST", ["objects", id, "uploads"]) => {
                let kind = request.headers.get("layr-object-type").cloned().unwrap_or_default();
                let upload = format!("u{}", s.uploads.len() + 1);
                s.uploads.insert(upload.clone(), (id.to_string(), kind, BTreeMap::new()));
                let size = s.part_size;
                (200, json(serde_json::json!({"upload":upload,"part_size":size})), Vec::new())
            }
            ("PUT", ["objects", _, "uploads", upload, n]) => {
                let n: usize = n.parse().unwrap();
                s.uploads.get_mut(*upload).unwrap().2.insert(n, request.body);
                (200, Vec::new(), Vec::new())
            }
            ("POST", ["objects", id, "uploads", upload, "complete"]) => {
                let (object, kind, parts) = s.uploads.remove(*upload).unwrap();
                let bytes: Vec<u8> = parts.into_values().flatten().collect();
                if object != *id || typed(&kind, &bytes) != *id {
                    return (400, json(serde_json::json!({"error":{"code":"invalid"}})), Vec::new());
                }
                s.objects.insert(object, (kind, bytes));
                (201, Vec::new(), Vec::new())
            }
            ("GET", ["records"]) => {
                let list: Vec<serde_json::Value> = s
                    .segments
                    .keys()
                    .filter(|(sc, ..)| sc == &scope)
                    .map(|(_, w, a, b)| serde_json::json!({"writer":w,"first":a,"last":b,"sha256":"","size":0}))
                    .collect();
                (200, json(serde_json::json!({"segments":list,"next":null})), Vec::new())
            }
            ("POST", ["records", writer, range]) => {
                let (first, last) = range.split_once('-').unwrap();
                let (first, last): (u64, u64) = (first.parse().unwrap(), last.parse().unwrap());
                let held =
                    s.segments.keys().filter(|(sc, w, ..)| sc == &scope && w == writer).map(|(.., b)| *b).max().unwrap_or(0);
                if first != held + 1 {
                    return (409, json(serde_json::json!({"error":{"code":"conflict"},"last":held})), Vec::new());
                }
                s.segments.insert((scope, writer.to_string(), first, last), request.body);
                (201, Vec::new(), Vec::new())
            }
            ("GET", ["records", writer, range]) => {
                let (first, last) = range.split_once('-').unwrap();
                let key = (scope, writer.to_string(), first.parse().unwrap(), last.parse().unwrap());
                match s.segments.get(&key) {
                    Some(bytes) => (200, bytes.clone(), Vec::new()),
                    None => (404, Vec::new(), Vec::new()),
                }
            }
            ("PUT", ["states", state]) => {
                let (Some(tree), Some(times)) = (body["tree"].as_str(), body["times"].as_str()) else {
                    return (400, json(serde_json::json!({"error":{"code":"invalid","message":"tree and times"}})), Vec::new());
                };
                // Both must be in a pack of the project.
                if !s.located.contains_key(tree) || !s.located.contains_key(times) {
                    return (400, json(serde_json::json!({"error":{"code":"invalid"}})), Vec::new());
                }
                let body = (tree.to_string(), times.to_string());
                match s.states.get(&(scope.clone(), state.to_string())) {
                    Some(old) if old != &body => (409, Vec::new(), Vec::new()),
                    Some(_) => (200, Vec::new(), Vec::new()),
                    None => {
                        s.states.insert((scope, state.to_string()), body);
                        (201, Vec::new(), Vec::new())
                    }
                }
            }
            ("POST", ["states", "missing"]) => {
                let missing: Vec<&str> = body["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|id| !s.states.contains_key(&(scope.clone(), id.to_string())))
                    .collect();
                (200, json(serde_json::json!({ "missing": missing })), Vec::new())
            }
            ("GET", ["states", state]) => match s.states.get(&(scope, state.to_string())) {
                Some((tree, times)) => (200, json(serde_json::json!({ "times": times, "tree": tree })), Vec::new()),
                None => (404, Vec::new(), Vec::new()),
            },
            ("GET", ["refs"]) => {
                let prefix = request.query.get("prefix").cloned().unwrap_or_default();
                let refs: Vec<serde_json::Value> = s
                    .refs
                    .iter()
                    .filter(|((sc, name), _)| sc == &scope && name.starts_with(&prefix))
                    .map(|((_, name), (target, version))| serde_json::json!({"name":name,"target":target,"version":version}))
                    .collect();
                (200, json(serde_json::json!({ "refs": refs })), Vec::new())
            }
            ("POST", ["refs", "cas"]) => {
                let updates = body["updates"].as_array().unwrap().clone();
                let mut conflicts = Vec::new();
                for update in &updates {
                    assert!(update["record"].as_str().is_some_and(|r| !r.is_empty()), "each move names its record");
                    let name = update["name"].as_str().unwrap().to_string();
                    let actual = s.refs.get(&(scope.clone(), name.clone())).map(|(t, _)| t.clone());
                    if actual.as_deref() != update["expect"].as_str() {
                        conflicts.push(serde_json::json!({"name":name,"actual":actual}));
                    }
                }
                if !conflicts.is_empty() {
                    return (409, json(serde_json::json!({"error":{"code":"conflict"},"conflicts":conflicts})), Vec::new());
                }
                let mut moved = Vec::new();
                for update in &updates {
                    let name = update["name"].as_str().unwrap().to_string();
                    match update["target"].as_str() {
                        Some(target) => {
                            let version = s.refs.get(&(scope.clone(), name.clone())).map_or(0, |(_, v)| v + 1);
                            s.refs.insert((scope.clone(), name.clone()), (target.to_string(), version));
                            moved.push(serde_json::json!({"name":name,"target":target,"version":version}));
                        }
                        None => {
                            s.refs.remove(&(scope.clone(), name));
                        }
                    }
                }
                (200, json(serde_json::json!({ "refs": moved })), Vec::new())
            }
            _ => (404, json(serde_json::json!({"error":{"code":"not_found"}})), Vec::new()),
        }
    }

    fn count(&self, prefix: &str) -> usize {
        self.state.lock().unwrap().log.iter().filter(|l| l.starts_with(prefix)).count()
    }
}

/// The typed id of the server: `type` from the `Layr-Object-Type` header.
fn typed(kind: &str, bytes: &[u8]) -> String {
    use layr_hashers::Digest;
    let mut hasher = layr_hashers::ContentAddress::new();
    hasher.update(format!("{kind} {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = vec![0; n];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

#[test]
fn person_requests_follow_pages_and_reject_repeated_ids() {
    let fake = Fake::start(None);
    let first = "01a1b000-0000-7000-9000-000000000001";
    let second = "01a1b000-0000-7000-9000-000000000002";
    {
        let mut state = fake.state.lock().unwrap();
        state.request_pages.insert("".into(), serde_json::json!({"requests":[{"id":first,"modified":1}],"next":first}));
        state.request_pages.insert(first.into(), serde_json::json!({"requests":[{"id":second,"modified":2}],"next":null}));
    }
    let rows = fake.api().list("requests/").unwrap();
    assert_eq!(
        rows.iter().map(|item| item.key.as_str()).collect::<Vec<_>>(),
        [format!("requests/{first}.json"), format!("requests/{second}.json")]
    );
    fake.state
        .lock()
        .unwrap()
        .request_pages
        .insert(first.into(), serde_json::json!({"requests":[{"id":first,"modified":1}],"next":null}));
    assert!(fake.api().list("requests/").unwrap_err().to_string().contains("repeated"));
    fake.state
        .lock()
        .unwrap()
        .request_pages
        .insert(first.into(), serde_json::json!({"requests":[{"id":second,"modified":2}],"next":first}));
    assert!(fake.api().list("requests/").unwrap_err().to_string().contains("cursor"));
}

#[test]
fn request_visitor_stops_before_fetching_the_next_page() {
    let fake = Fake::start(None);
    let first = "01a1b000-0000-7000-9000-000000000001";
    let second = "01a1b000-0000-7000-9000-000000000002";
    {
        let mut state = fake.state.lock().unwrap();
        state.request_pages.insert("".into(), serde_json::json!({"requests":[{"id":first,"modified":1}],"next":first}));
        state.request_pages.insert(first.into(), serde_json::json!({"requests":[{"id":second,"modified":2}],"next":null}));
    }
    let api = fake.api();
    let mut visited = Vec::new();
    api.visit_requests(|item| {
        visited.push(item.key);
        Ok(false)
    })
    .unwrap();
    assert_eq!(visited, [format!("requests/{first}.json")]);
    assert_eq!(fake.count("GET /api/v1/projects/p/person-requests"), 1);
    visited.clear();
    api.visit_requests(|item| {
        visited.push(item.key);
        Ok(true)
    })
    .unwrap();
    assert_eq!(visited, [format!("requests/{first}.json"), format!("requests/{second}.json")]);
    assert_eq!(fake.count("GET /api/v1/projects/p/person-requests"), 3);
    fake.state
        .lock()
        .unwrap()
        .request_pages
        .insert(first.into(), serde_json::json!({"requests":[{"id":first,"modified":1}],"next":null}));
    visited.clear();
    assert!(api
        .visit_requests(|item| {
            visited.push(item.key);
            Ok(true)
        })
        .is_err());
    assert_eq!(visited, [format!("requests/{first}.json")]);
}

#[test]
fn an_object_over_64_mib_goes_up_in_parts_and_a_held_object_is_not_sent_again() {
    let fake = Fake::start(None);
    let api = fake.api();
    let bytes = random_bytes((64 << 20) + 4096);
    let (id, new) = api.put_object(Kind::Opaque, &bytes).unwrap();
    assert!(new);
    assert_eq!(id, typed("opaque", &bytes));
    assert_eq!(fake.count("POST /api/v1/projects/p/objects/missing"), 1, "a large upload asks first");
    assert_eq!(fake.count(&format!("POST /api/v1/projects/p/objects/{id}/uploads")), 2, "start and complete");
    assert_eq!(fake.count(&format!("PUT /api/v1/projects/p/objects/{id}/uploads/u1/")), 9, "8 MiB parts");
    assert_eq!(fake.count(&format!("PUT /api/v1/projects/p/objects/{id}")), 9, "no single PUT over 64 MiB");
    assert_eq!(api.get_object(Kind::Opaque, &id).unwrap().unwrap(), bytes);
    // The project holds it now: the second upload sends nothing.
    let (_, new) = api.put_object(Kind::Opaque, &bytes).unwrap();
    assert!(!new);
    assert_eq!(fake.count(&format!("POST /api/v1/projects/p/objects/{id}/uploads")), 2);
    // The type is part of the id, and a read checks the bytes against the id.
    assert_ne!(typed("blob", b"x"), typed("opaque", b"x"));
    assert!(api.get_object(Kind::Blob, &id).unwrap_err().to_string().contains("does not match its id"));
    // A missing object is missing, and `objects/missing` names it.
    let absent = typed("opaque", b"absent");
    assert_eq!(api.get_object(Kind::Opaque, &absent).unwrap(), None);
    assert_eq!(api.objects_missing(&[id.clone(), absent.clone()]).unwrap(), vec![absent]);
}

#[test]
fn a_client_that_the_server_finds_too_old_stops_with_the_minimum_version() {
    let fake = Fake::start(None);
    fake.state.lock().unwrap().minimum = Some("9.9.9".into());
    let error = fake.api().project_info().unwrap_err().to_string();
    assert!(error.contains("needs layr 9.9.9 or newer"), "{error}");
    assert_eq!(fake.api().server_version(), None);
    let api = fake.api();
    let _ = api.project_info();
    assert_eq!(api.server_version().as_deref(), Some("fake-1"));
}

#[test]
fn a_write_without_a_token_names_the_login() {
    let fake = Fake::start(None);
    let api = Api::for_test(&fake.origin, "p", None);
    let error = api.put_object(Kind::Opaque, b"bytes").unwrap_err().to_string();
    assert!(error.contains("HTTP 401") && error.contains("layr login"), "{error}");
}

fn context(root: &Path) -> Ctx {
    Ctx {
        store: crate::store::Store::open(&root.join("local")).unwrap(),
        caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
        cwd: root.to_path_buf(),
        env: HashMap::new(),
        author: Default::default(),
        out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
    }
}

const PROJECT: &str = "01a12290-0000-7000-8000-0000000000a1";

fn api_store(root: &Path, ctx: &Ctx, fake: &Fake) -> Store {
    api_store_with(root, ctx, fake.api())
}

fn api_store_with(root: &Path, ctx: &Ctx, api: Api) -> Store {
    Store {
        backend: Backend::Api(api),
        url: "layr+https://cloud.test/p".into(),
        project: PROJECT.into(),
        machine: ctx.store.machine.id.clone(),
        key: ctx.store.machine.key.clone(),
        dir: root.join("local/meta/remote"),
        paper_key_text: None,
        person: None,
        pin: None,
        aws_profile: None,
        layer: None,
        epoch: None,
        epoch_public: false,
        publishing_epoch: false,
        force_public: false,
        public_clone: false,
        public_reader: std::sync::atomic::AtomicBool::new(false),
        mode: None,
    }
}

/// A project with three states (the third repeats the first file and adds a large one) and one line at the head.
fn project<'a>(ctx: &'a Ctx) -> (Repo<'a>, Vec<String>, String) {
    let dir = ctx.store.project_dir("p").unwrap();
    std::fs::create_dir_all(dir.join("meta/db")).unwrap();
    std::fs::create_dir_all(dir.join("states")).unwrap();
    let project = crate::store::Project {
        store: &ctx.store,
        name: "p".into(),
        dir: dir.clone(),
        id: PROJECT.into(),
        db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
    };
    let mut record = Record::new("project");
    // These are plain fixture folders, not read-only native snapshots. Record their v2 objects
    // before publication so API transport tests do not require a privileged Btrfs send.
    record.data = serde_json::json!({"id":PROJECT,"name":"p","config":{"objects.enabled":true}});
    project.append(record, &ctx.actor()).unwrap();
    let mut states = Vec::new();
    for n in 0..3 {
        let id = crate::ids::new_id();
        let path = project.state_path(&id);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("stable"), "unchanged content\n".repeat(100)).unwrap();
        std::fs::write(path.join("payload"), format!("version {}\n", n % 2)).unwrap();
        if n == 2 {
            std::fs::create_dir(path.join("dir")).unwrap();
            std::fs::write(path.join("dir/large"), random_bytes(3 << 20)).unwrap();
        }
        let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":"commit",
            "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
            "parents":states.last().cloned().into_iter().collect::<Vec<_>>(),"time":crate::ids::now_ms()}))
        .unwrap();
        let mut record = Record::new("state");
        record.states.push(state);
        let saved = project.append(record, &ctx.actor()).unwrap();
        assert!(saved.states[0].tree.is_some() && saved.states[0].times.is_some(), "the API fixture uses v2 objects");
        states.push(id);
    }
    let line = crate::ids::new_id();
    move_line(&project, ctx, &line, None, &states[2]);
    (Repo { p: project, ctx: None }, states, line)
}

fn add_state(project: &crate::store::Project, ctx: &Ctx, parent: &str) -> String {
    let id = crate::ids::new_id();
    std::fs::create_dir(project.state_path(&id)).unwrap();
    std::fs::write(project.state_path(&id).join("payload"), "next\n").unwrap();
    let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":"commit",
        "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
        "parents":[parent],"time":crate::ids::now_ms()}))
    .unwrap();
    let mut record = Record::new("state");
    record.states.push(state);
    project.append(record, &ctx.actor()).unwrap();
    id
}

fn move_line(project: &crate::store::Project, ctx: &Ctx, line: &str, before: Option<&str>, head: &str) {
    let state = |head: &str| LineState {
        name: "main".into(),
        head: head.into(),
        owner: ctx.caller.uid,
        machine: ctx.store.machine.id.clone(),
    };
    let mut record = Record::new(if before.is_some() { "save" } else { "line.create" });
    record.lines.insert(
        line.into(),
        LineChange { before: before.map(state), after: Some(state(head)), working_before: None, working_after: None },
    );
    record.data = serde_json::json!({"name":"main"});
    project.append(record, &ctx.actor()).unwrap();
}

fn push(store: &Store, ctx: &Ctx, repo: &Repo) -> anyhow::Result<super::Report> {
    store.private_dir()?;
    let mut report = writer::push(store, ctx, repo, &Options::default())?;
    super::push_refs(store, repo, &mut report)?;
    Ok(report)
}

/// Reads `state` from the server as a fresh machine would: its body, the packs that hold its objects (in
/// rounds: trees first), then a checkout with its times into `dst`.
fn download(api: &Api, objects_dir: &Path, state: &str, dst: &Path) -> anyhow::Result<()> {
    let r = api.get_state("", state)?.ok_or_else(|| anyhow::anyhow!("no state {state}"))?;
    for _ in 0..8 {
        let objects = layr_objects::Store::open(objects_dir, layr_objects::Keying::Plain)?;
        let mut need: Vec<String> = layr_objects::transfer::missing_trees(&objects, &r.tree)?.iter().map(|i| i.hex()).collect();
        need.extend(layr_objects::transfer::missing_cloud_times(&objects, &r.times)?.iter().map(|i| i.hex()));
        if need.is_empty() {
            for (id, kind) in layr_objects::transfer::reachable(&objects, &r.tree)? {
                if kind == Kind::Blob && !objects.has(&id) {
                    need.push(id.hex());
                }
            }
        }
        if need.is_empty() {
            let times = layr_objects::transfer::cloud_times_to_folder(&objects, &r.tree, &r.times)?;
            layr_objects::checkout_with(&objects, &r.tree, Some(&times), dst)?;
            return Ok(());
        }
        let located = api.locate(&need)?;
        anyhow::ensure!(located.len() == need.len(), "the server lacks objects");
        for pack in located.values().collect::<std::collections::BTreeSet<_>>() {
            let data = api.get_object(Kind::Opaque, pack)?.ok_or_else(|| anyhow::anyhow!("no pack {pack}"))?;
            layr_objects::transfer::add_cloud_pack(&objects, &data, &api.get_pack_index(pack)?)?;
        }
    }
    anyhow::bail!("objects did not come")
}

#[test]
fn a_reader_fetches_a_legacy_cloud_times_table() {
    use layr_objects::{pack, Keying, Node, Tree};
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let source = layr_objects::Store::open(&root.join("source"), Keying::Plain).unwrap();
    let (blob, size) = source.put_cold_bytes(b"old cloud file", None).unwrap();
    let tree = source
        .put_tree(&Tree {
            entries: vec![layr_objects::Entry {
                name: b"file".to_vec(),
                mode: 0o644,
                node: Node::File { size, blob },
                xattrs: vec![],
            }],
            links: vec![],
        })
        .unwrap();
    let packs = layr_objects::transfer::build_packs(&source, &[(tree, Kind::Tree), (blob, Kind::Blob)]).unwrap();
    let old_times = [1, 2, 20, 2, 6, 0]; // root 10 ns, file 13 ns
    let times = Keying::Plain.id(Kind::Times, &old_times);
    let compressed =
        layr_objects::zstdx::compress(&old_times, &layr_objects::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
    let mut data = pack::DATA_MAGIC.to_vec();
    data.extend_from_slice(&compressed);
    let pack_id = Keying::Plain.id(Kind::Opaque, &data);
    let index = pack::Index {
        pack: pack_id,
        blocks: vec![pack::Block { offset: 4, compressed: compressed.len() as u32, plain: old_times.len() as u32 }],
        entries: vec![pack::Entry {
            id: times,
            kind: Kind::Times,
            form: pack::RAW,
            block: 0,
            offset: 0,
            length: old_times.len() as u32,
        }],
    }
    .encode();
    let fake = Fake::start(Some(PROJECT));
    let state = crate::ids::new_id();
    {
        let mut server = fake.state.lock().unwrap();
        for pack in packs {
            let index = pack::Index::decode(&pack.index, pack.data.len() as u64).unwrap();
            for entry in index.entries {
                server.located.insert(entry.id.hex(), pack.id.hex());
            }
            server.indexes.insert(pack.id.hex(), pack.index);
            server.objects.insert(pack.id.hex(), ("opaque".into(), pack.data));
        }
        server.located.insert(times.hex(), pack_id.hex());
        server.indexes.insert(pack_id.hex(), index);
        server.objects.insert(pack_id.hex(), ("opaque".into(), data));
        server.states.insert((String::new(), state.clone()), (tree.hex(), times.hex()));
    }
    let dst = root.join("checkout");
    download(&fake.api(), &root.join("destination"), &state, &dst).unwrap();
    assert_eq!(std::fs::read(dst.join("file")).unwrap(), b"old cloud file");
    assert_eq!(layr_objects::times::ns_of(&std::fs::metadata(dst.join("file")).unwrap()), 13);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_push_sends_trees_times_and_packs_records_and_one_ref_move_and_a_reader_gets_the_files_back() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let ctx = context(&root);
    let fake = Fake::start(Some(PROJECT));
    let store = api_store(&root, &ctx, &fake);
    let (repo, states, line) = project(&ctx);
    let report = push(&store, &ctx, &repo).unwrap();
    assert_eq!(report.states, 3);
    {
        let server = fake.state.lock().unwrap();
        // The machine key went up before its records, and only this machine's segments.
        assert_eq!(server.machines.get(&store.machine), Some(&hex::encode(store.key.verifying_key().to_bytes())));
        assert!(server.segments.keys().all(|(_, writer, ..)| writer == &store.machine));
        // Every object is an `opaque` pack data file with its index; every state names its tree and times.
        assert!(!server.indexes.is_empty());
        for (id, (kind, bytes)) in &server.objects {
            assert_eq!(kind, "opaque");
            assert_eq!(&typed(kind, bytes), id);
            assert!(server.indexes.contains_key(id), "pack {id} has its index");
        }
        for state in &states {
            let (tree, times) = &server.states[&(String::new(), state.clone())];
            assert!(server.located.contains_key(tree) && server.located.contains_key(times));
        }
        assert_eq!(server.refs[&(String::new(), format!("lines/{line}"))].0, states[2]);
    }
    // The local object store holds the states as `layr objects` does.
    let objects = layr_objects::Store::open(&repo.p.meta_dir().join("objects"), layr_objects::Keying::Plain).unwrap();
    for state in &states {
        assert!(objects.state_ref(state).unwrap().is_some());
    }
    // Another machine reads each state back from the server, with its times.
    for (n, state) in states.iter().enumerate() {
        let dst = root.join(format!("read-{n}"));
        download(&fake.api(), &root.join("reader-objects"), state, &dst).unwrap();
        let local = repo.p.state_path(state);
        assert!(layr_objects::compare(&local, &dst).unwrap().differences.is_empty(), "state {n} reads back equal");
        assert_eq!(std::fs::read(dst.join("payload")).unwrap(), format!("version {}\n", n % 2).as_bytes());
    }
    // A second push sends no object, no state and no ref move, and asks for the states in one batch.
    let gets = fake.count("GET /api/v1/projects/p/states/");
    let batches = fake.count("POST /api/v1/projects/p/states/missing");
    let puts = fake.count("PUT ");
    let cas = fake.count("POST /api/v1/projects/p/refs/cas");
    // A new command: a new client that knows nothing about the server yet.
    let store = api_store(&root, &ctx, &fake);
    let again = push(&store, &ctx, &repo).unwrap();
    assert_eq!((again.states, again.records), (0, 0));
    assert_eq!(fake.count("PUT "), puts);
    assert_eq!(fake.count("POST /api/v1/projects/p/refs/cas"), cas);
    assert_eq!(fake.count("GET /api/v1/projects/p/states/"), gets, "no request per state");
    assert_eq!(fake.count("POST /api/v1/projects/p/states/missing"), batches + 1);
    // A new state sends only the objects that the server lacks: its tree path and the new blob.
    let packs = fake.state.lock().unwrap().indexes.len();
    let next = add_state(&repo.p, &ctx, &states[2]);
    move_line(&repo.p, &ctx, &line, Some(&states[2]), &next);
    let store = api_store(&root, &ctx, &fake);
    push(&store, &ctx, &repo).unwrap();
    let server = fake.state.lock().unwrap();
    assert_eq!(server.indexes.len(), packs + 1, "one new pack");
    let index = server.indexes.values().find(|i| {
        let decoded = layr_objects::pack::Index::decode(i, server.objects[&hex::encode(&i[4..36])].1.len() as u64).unwrap();
        decoded.entries.iter().any(|e| e.kind == Kind::Times && server.states[&(String::new(), next.clone())].1 == e.id.hex())
    });
    let index = index.expect("the new pack holds the new times table");
    let pack = &server.objects[&hex::encode(&index[4..36])].1;
    let decoded = layr_objects::pack::Index::decode(index, pack.len() as u64).unwrap();
    assert!(decoded.entries.len() <= 4, "a root tree, a times table and the changed blob: {}", decoded.entries.len());
    drop(server);
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_first_api_cache_fetch_selects_after_importing_metadata() {
    for (exact_state, selected) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let fake = Fake::start(Some(PROJECT));
        let store = api_store(&root, &ctx, &fake);
        let (repo, parent_states, _) = project(&ctx);
        let opts = if exact_state {
            Options {
                caches_of_state: Some(parent_states[2].clone()),
                heads: true,
                ..Default::default()
            }
        } else {
            Options {
                caches_of: vec!["main".into()],
                heads: true,
                ..Default::default()
            }
        };
        check_first_cache_fetch(
            &root,
            &ctx,
            &repo,
            &store,
            "cache.target",
            &opts,
            selected.then_some(parent_states[2].as_str()),
        );
        assert_eq!(
            fake.count("GET /api/v1/projects/p/objects/") > 0,
            selected,
            "only a selected cache downloads an object pack"
        );
        crate::free::finish(&ctx);
        drop(repo);
        delete_cache_fixture(&root);
    }
}

fn check_first_cache_fetch(
    root: &Path,
    ctx: &Ctx,
    repo: &Repo,
    store: &Store,
    name: &str,
    opts: &Options,
    cache_of: Option<&str>,
) {
    let mut declaration = Record::new("overlay.create");
    declaration.data = serde_json::json!({"name":name,"config":{
            format!("overlay.{name}"):{"owner":ctx.caller.uid,"root":"target","mode":"follow"},
            format!("overlay.{name}.cache"):true}});
    repo.p.append(declaration, &ctx.actor()).unwrap();
    let dir = ctx.store.project_dir(&format!("p%{name}")).unwrap();
    std::fs::create_dir_all(dir.join("meta/db")).unwrap();
    std::fs::create_dir_all(dir.join("states")).unwrap();
    let project_id = crate::ids::new_id();
    let cache = crate::store::Project {
        store: &ctx.store,
        name: format!("p%{name}"),
        dir: dir.clone(),
        id: project_id.clone(),
        db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
    };
    let mut creation = Record::new("project");
    creation.data =
        serde_json::json!({"id":project_id,"name":name,"config":{"objects.enabled":true}});
    cache.append(creation, &ctx.actor()).unwrap();
    let state = crate::ids::new_id();
    std::fs::create_dir(cache.state_path(&state)).unwrap();
    std::fs::write(
        cache.state_path(&state).join("unrelated"),
        random_bytes(300000),
    )
    .unwrap();
    let mut saved = Record::new("commit");
    saved.states.push(serde_json::from_value(serde_json::json!({"id":state,"kind":"commit",
            "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
            "cache_of":cache_of.map(str::to_owned).unwrap_or_else(crate::ids::new_id),"cache_by":ctx.caller.uid,"time":crate::ids::now_ms()})).unwrap());
    cache.append(saved, &ctx.actor()).unwrap();
    move_line(&cache, &ctx, &crate::ids::new_id(), None, &state);
    let mut layer = api_store_with(root, ctx, store.backend.api().unwrap().clone());
    layer.backend = layer.backend.scoped(&format!("overlays/{name}")).unwrap();
    layer.project = project_id;
    layer.dir = root.join(format!("layer-remote-{name}"));
    push(
        &layer,
        &ctx,
        &Repo {
            p: cache,
            ctx: None,
        },
    )
    .unwrap();
    std::fs::rename(&dir, root.join(format!("original-{name}"))).unwrap();
    let selected = cache_of.is_some();
    let mut report = super::Report::default();
    super::overlays::api_fetch(store, ctx, repo, opts, &mut report).unwrap();
    assert!(
        report
            .omitted
            .iter()
            .all(|item| item.contains("not for --caches-of")),
        "cache fetch errors: {:?}",
        report.omitted
    );
    let copied = repo.overlay_repo(name).unwrap();
    assert!(
        copied.p.db.state(&state).unwrap().is_some(),
        "the metadata is imported"
    );
    assert_eq!(
        report.states,
        usize::from(selected),
        "only selected cache bodies arrive: {:?}",
        report.omitted
    );
    assert_eq!(copied.p.has_state(&state), selected);
    if selected {
        assert_eq!(
            std::fs::read(copied.p.state_path(&state).join("unrelated")).unwrap(),
            std::fs::read(
                root.join(format!("original-{name}/states"))
                    .join(&state)
                    .join("unrelated")
            )
            .unwrap()
        );
    }
}


fn delete_cache_fixture(root: &Path) {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    fn open_subvolumes(path: &Path) {
        // The fixture owns these snapshots. Ordinary Btrfs deletion needs them writable.
        if crate::storage::is_subvolume(path) {
            crate::storage::set_writable(path).unwrap();
        }
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                open_subvolumes(&entry.path());
            }
        }
    }
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    open_subvolumes(root);
    crate::storage::delete_tree(root).unwrap();
}

#[test]
fn a_line_that_moved_on_the_server_fails_the_push_until_a_fetch_reads_it() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let ctx = context(&root);
    let fake = Fake::start(Some(PROJECT));
    let store = api_store(&root, &ctx, &fake);
    let (repo, states, line) = project(&ctx);
    push(&store, &ctx, &repo).unwrap();
    let name = format!("lines/{line}");
    // Another writer moves the line on the server.
    fake.state.lock().unwrap().refs.insert((String::new(), name.clone()), (states[0].clone(), 7));
    let next = add_state(&repo.p, &ctx, &states[2]);
    move_line(&repo.p, &ctx, &line, Some(&states[2]), &next);
    let error = push(&store, &ctx, &repo).unwrap_err().to_string();
    assert!(error.contains(&name) && error.contains("layr fetch"), "{error}");
    assert_eq!(fake.state.lock().unwrap().refs[&(String::new(), name.clone())].0, states[0], "the server ref stays");
    // The fetch reads the server ref; the next push expects it and moves the line.
    let mut report = super::Report::default();
    super::fetch_refs(&store, &repo, &mut report).unwrap();
    assert!(report.omitted.iter().any(|o| o.contains("Layr Cloud has")), "{:?}", report.omitted);
    push(&store, &ctx, &repo).unwrap();
    assert_eq!(fake.state.lock().unwrap().refs[&(String::new(), name.clone())].0, next);
    // A head that does not contain the server's target is refused before any request, as a Git push without force.
    let cas = fake.count("POST /api/v1/projects/p/refs/cas");
    move_line(&repo.p, &ctx, &line, Some(&next), &states[1]);
    let error = push(&store, &ctx, &repo).unwrap_err().to_string();
    assert!(error.contains("does not contain it"), "{error}");
    assert_eq!(fake.count("POST /api/v1/projects/p/refs/cas"), cas);
    assert_eq!(fake.state.lock().unwrap().refs[&(String::new(), name)].0, next);
    // A direct compare and swap with a stale expectation is a conflict that names the actual target.
    let update = RefUpdate {
        name: "lines/x".into(),
        expect: Some(states[0].clone()),
        target: Some(states[1].clone()),
        record: "r".into(),
    };
    assert_eq!(fake.api().cas("", &[update], "").unwrap(), Cas::Conflict(vec![("lines/x".into(), None)]));
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_missing_pack_fails_the_read_and_names_the_pack() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let ctx = context(&root);
    let fake = Fake::start(Some(PROJECT));
    let store = api_store(&root, &ctx, &fake);
    let (repo, states, _) = project(&ctx);
    push(&store, &ctx, &repo).unwrap();
    let pack = fake.state.lock().unwrap().indexes.keys().next().unwrap().clone();
    fake.state.lock().unwrap().objects.remove(&pack);
    let error = format!("{:#}", download(&fake.api(), &root.join("reader"), &states[2], &root.join("dst")).unwrap_err());
    assert!(error.contains(&pack), "{error}");
    // A pack whose index the server lost: `locate` no longer names its objects, so the read names them.
    fake.state.lock().unwrap().located.clear();
    let error = format!("{:#}", download(&fake.api(), &root.join("reader2"), &states[2], &root.join("dst2")).unwrap_err());
    assert!(error.contains("lacks objects"), "{error}");
    // A state that the server lacks.
    assert!(fake.api().get_state("", &crate::ids::new_id()).unwrap().is_none());
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn login_polls_until_the_person_signs_in_then_saves_the_session_and_registers_the_machine() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let mut ctx = context(&root);
    ctx.caller.home = root.join("home");
    let fake = Fake::start(Some(PROJECT));
    let store = api_store(&root, &ctx, &fake);
    let shown = std::cell::RefCell::new(String::new());
    let login = store.login(&ctx, |link| *shown.borrow_mut() = link.into()).unwrap();
    assert_eq!(login, "ann@example.com");
    // The relative link joins the remote origin.
    assert_eq!(*shown.borrow(), format!("{}#/login?code=c1", fake.origin));
    // A link to another host or with another scheme is refused.
    let api = fake.api();
    for bad in ["https://evil.test/#/login?code=c1", "javascript:alert(1)", "//evil.test/login", "file:///etc/passwd"] {
        assert!(api.login_link(bad).is_err(), "{bad}");
    }
    assert_eq!(api.login_link("login/c2").unwrap().as_str(), format!("{}login/c2", fake.origin));
    assert!(fake.state.lock().unwrap().machines.contains_key(&store.machine));
    let path = super::api::session_file(&ctx.caller.home, "127.0.0.1");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // The next command reads the saved session as its token.
    let mut api = Api::for_test(&fake.origin, "p", None);
    api.connect(&ctx, PROJECT, false).unwrap();
    api.put_object(Kind::Opaque, b"with the session").unwrap();
    // Explicit public access does not load the saved person session.
    let mut public = Api::for_test(&fake.origin, "p", None);
    public.connect(&ctx, PROJECT, true).unwrap();
    let error = format!("{:#}", public.put_object(Kind::Opaque, b"anonymous").unwrap_err());
    assert!(error.contains("HTTP 401"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn records_that_a_server_serves_come_in_without_widening_local_trust_and_a_rekeyed_writer_is_refused() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let ctx = context(&root);
    let (repo, _, _) = project(&ctx);
    // A machine that only the server vouches for: its records come in, its trust of a third machine has no effect.
    let key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
    let third = ed25519_dalek::SigningKey::from_bytes(&[6; 32]);
    let machine = "01a12290-0000-7000-8000-0000000000c1";
    let other = "01a12290-0000-7000-8000-0000000000c2";
    let sign = |seq: u64, op: &str, data: serde_json::Value| {
        let mut record = Record::new(op);
        record.id = format!("01a12290-0000-7000-8000-{:012x}", 0xd00 + seq);
        record.machine = machine.into();
        record.project = PROJECT.into();
        record.seq = seq;
        record.time = crate::ids::now_ms();
        record.data = data;
        crate::oplog::sign_line(&record, &key).unwrap()
    };
    let lines = vec![
        sign(1, "machine", serde_json::json!({"name":"server-writer","public_key":hex::encode(key.verifying_key().to_bytes())})),
        sign(2, "tag", serde_json::json!({"name":"from-server","state":null,"message":"m"})),
        sign(3, "machine.trust", serde_json::json!({"machine":other,"public_key":hex::encode(third.verifying_key().to_bytes())})),
    ];
    // `Trust::Known` refuses this batch; `Trust::Server` takes it.
    assert!(repo.p.import_lines(&lines, crate::store::Trust::Known).is_err());
    assert_eq!(repo.p.import_lines(&lines, crate::store::Trust::Server).unwrap(), 3);
    assert!(repo.p.db.record_line(machine, 3).unwrap().is_some(), "every record is stored");
    assert_eq!(repo.p.db.trusted(machine).unwrap(), None, "the server's writer is not trusted here");
    assert_eq!(repo.p.db.trusted(other).unwrap(), None, "its trust record has no effect");
    // The writer key is pinned per remote on first sight; another key for the same machine is refused.
    let dir = root.join("remote");
    let id = machine.to_string();
    super::reader::pin_writer_keys(&dir, &BTreeMap::from([(id.clone(), key.verifying_key())])).unwrap();
    super::reader::pin_writer_keys(&dir, &BTreeMap::from([(id.clone(), key.verifying_key())])).unwrap();
    let error = super::reader::pin_writer_keys(&dir, &BTreeMap::from([(id, third.verifying_key())])).unwrap_err().to_string();
    assert!(error.contains("another key"), "{error}");
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}

/// Run through the real gateway fixture, supplied by its matching interoperability test.
#[test]
#[ignore = "requires the local gateway interoperability fixture"]
fn real_gateway_object_round_trip() {
    use layr_objects::{Keying, StateRef};
    // Check runs pass --include-ignored; without the fixture there is nothing to run against.
    let Ok(origin) = std::env::var("LAYR_INTEROP_ORIGIN") else {
        eprintln!("skipped: LAYR_INTEROP_ORIGIN is not set (the gateway interoperability fixture sets it)");
        return;
    };
    let token = std::env::var("LAYR_INTEROP_TOKEN").expect("gateway token");
    let api = Api::for_test(&origin, "example", Some(&token));
    if std::env::var_os("LAYR_INTEROP_PAGINATION").is_some() {
        let requests = api.list("requests/").unwrap();
        assert_eq!(requests.len(), 1001);
        for (n, item) in requests.iter().enumerate() {
            assert_eq!(item.key, format!("requests/01a1b000-0000-7000-9000-{:012x}.json", n + 1));
        }
    }
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(root.join("input/nested")).unwrap();
    std::fs::write(root.join("input/nested/file"), b"real gateway\n").unwrap();
    let source = layr_objects::Store::open(&root.join("source"), Keying::Plain).unwrap();
    let options = layr_objects::WriteOptions {
        temperature: layr_objects::Temperature::Cold,
        previous: None,
        exclude: Vec::new(),
        similar: false,
    };
    let written = layr_objects::write_tree(&source, &root.join("input"), &options).unwrap();
    let state = StateRef { tree: written.tree.unwrap(), times: written.times.unwrap() };
    let mut ids = source.times_ids(&state.times).unwrap().into_iter().map(|id| (id, Kind::Times)).collect::<Vec<_>>();
    ids.extend(layr_objects::transfer::reachable(&source, &state.tree).unwrap());
    let missing = api.objects_missing(&ids.iter().map(|(id, _)| id.hex()).collect::<Vec<_>>()).unwrap();
    assert_eq!(missing.len(), ids.len());
    for pack in layr_objects::transfer::build_packs(&source, &ids).unwrap() {
        let (uploaded, _) = api.put_object(Kind::Opaque, &pack.data).unwrap();
        assert_eq!(uploaded, pack.id.hex());
        api.put_pack_index(&uploaded, &pack.index).unwrap();
    }
    let state_id = crate::ids::new_id();
    assert!(api.put_state("", &state_id, &state).unwrap());
    let got = api.get_state("", &state_id).unwrap().unwrap();
    assert_eq!((got.tree, got.times), (state.tree, state.times));
    let output = root.join("output");
    download(&api, &root.join("receiver"), &state_id, &output).unwrap();
    assert_eq!(std::fs::read(output.join("nested/file")).unwrap(), b"real gateway\n");
    assert!(layr_objects::compare(&root.join("input"), &output).unwrap().differences.is_empty());

    // The gateway also accepts its older whole-tree times layout.
    use layr_objects::pack;
    let legacy = [1, 3, 20, 3, 2, 2, 4, 0];
    let legacy_id = Keying::Plain.id(Kind::Times, &legacy);
    let compressed =
        layr_objects::zstdx::compress(&legacy, &layr_objects::zstdx::Codec { level: 3, long: false, dict: None }).unwrap();
    let mut data = pack::DATA_MAGIC.to_vec();
    data.extend_from_slice(&compressed);
    let pack_id = Keying::Plain.id(Kind::Opaque, &data);
    let index = pack::Index {
        pack: pack_id,
        blocks: vec![pack::Block { offset: 4, compressed: compressed.len() as u32, plain: legacy.len() as u32 }],
        entries: vec![pack::Entry {
            id: legacy_id,
            kind: Kind::Times,
            form: pack::RAW,
            block: 0,
            offset: 0,
            length: legacy.len() as u32,
        }],
    }
    .encode();
    api.put_object(Kind::Opaque, &data).unwrap();
    api.put_pack_index(&pack_id.hex(), &index).unwrap();
    let old_state = StateRef { tree: state.tree, times: legacy_id };
    let old_id = crate::ids::new_id();
    assert!(api.put_state("", &old_id, &old_state).unwrap());
    let old_output = root.join("old-output");
    download(&api, &root.join("old-receiver"), &old_id, &old_output).unwrap();
    assert_eq!(std::fs::read(old_output.join("nested/file")).unwrap(), b"real gateway\n");
    assert_eq!(layr_objects::times::ns_of(&std::fs::metadata(old_output.join("nested/file")).unwrap()), 13);

    // Push records, states and a line ref through the same API as a normal client.
    let mut ctx = context(&root);
    use std::os::unix::fs::PermissionsExt;
    let source_token = root.join("source-token");
    std::fs::write(&source_token, &token).unwrap();
    std::fs::set_permissions(&source_token, std::fs::Permissions::from_mode(0o600)).unwrap();
    ctx.env.insert("LAYR_GATEWAY_TOKEN_FILE".into(), source_token.to_string_lossy().into_owned());
    let mut store = api_store_with(&root, &ctx, api.clone());
    let (mut repo, states, line) = project(&ctx);
    let mut config = Record::new("config");
    config.data = serde_json::json!({"config":{"access":{format!("user:{}", ctx.caller.uid):"admin"}}});
    repo.p.append(config, &ctx.actor()).unwrap();
    let report = store.push(&ctx, &mut repo, &Options::default()).expect("push through the real gateway");
    assert_eq!(report.states, 3);
    let refs = api.refs("", "lines/").unwrap();
    assert!(refs.iter().any(|r| r.name == format!("lines/{line}") && r.target.as_deref() == Some(&states[2])));
    let pushed_output = root.join("pushed-output");
    download(&api, &root.join("pushed-receiver"), &states[2], &pushed_output).unwrap();
    assert_eq!(std::fs::read(pushed_output.join("payload")).unwrap(), b"version 0\n");
    assert_eq!(
        std::fs::read(pushed_output.join("dir/large")).unwrap(),
        std::fs::read(repo.p.state_path(&states[2]).join("dir/large")).unwrap()
    );
    // Import the signed records and restore the states into a separate empty project.
    let receiving_root = root.join("receiving-user");
    std::fs::create_dir_all(&receiving_root).unwrap();
    let mut receiver_ctx = context(&receiving_root);
    let token_file = receiving_root.join("gateway-token");
    std::fs::write(&token_file, &token).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    receiver_ctx.env.insert("LAYR_GATEWAY_TOKEN_FILE".into(), token_file.to_string_lossy().into_owned());
    let dir = receiver_ctx.store.project_dir("p").unwrap();
    std::fs::create_dir_all(dir.join("meta/db")).unwrap();
    std::fs::create_dir_all(dir.join("meta/work")).unwrap();
    std::fs::create_dir_all(dir.join("states")).unwrap();
    let project = crate::store::Project {
        store: &receiver_ctx.store,
        name: "p".into(),
        dir: dir.clone(),
        id: String::new(),
        db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
    };
    let mut receiver_repo = Repo { p: project, ctx: None };
    let mut receiver_store = api_store_with(&receiving_root, &receiver_ctx, api.clone());
    let fetched =
        receiver_store.fetch(&receiver_ctx, &mut receiver_repo, &Options::default()).expect("fetch through the real gateway");
    assert_eq!(fetched.states, 3);
    assert!(fetched.records > 0);
    assert!(receiver_repo.p.db.lines().unwrap().iter().any(|entry| entry.id == line && entry.head == states[2]));
    assert_eq!(std::fs::read(receiver_repo.p.state_path(&states[2]).join("payload")).unwrap(), b"version 0\n");
    assert_eq!(
        std::fs::read(receiver_repo.p.state_path(&states[2]).join("dir/large")).unwrap(),
        std::fs::read(repo.p.state_path(&states[2]).join("dir/large")).unwrap()
    );
    // First cache copies select after importing metadata on the actual Gateway too.
    for (n, (exact_state, selected)) in [(false, false), (true, false), (false, true), (true, true)].into_iter().enumerate() {
        let opts = if exact_state {
            Options {caches_of_state:Some(states[2].clone()),heads:true,..Default::default()}
        } else {
            Options {caches_of:vec!["main".into()],heads:true,..Default::default()}
        };
        check_first_cache_fetch(&root, &ctx, &repo, &store, &format!("cache.selection{n}"), &opts,
            selected.then_some(states[2].as_str()));
    }
    crate::free::finish(&ctx);
    crate::free::finish(&receiver_ctx);
    drop(receiver_repo);
    drop(repo);
    delete_cache_fixture(&root);
    real_gateway_cli_queue(&origin, &token);
}

/// Exercise records made by the real CLI, including a non-fast-forward queue result.
fn real_gateway_cli_queue(origin: &str, token: &str) {
    use std::io::{BufRead, Write};
    use std::os::unix::fs::PermissionsExt;
    struct Browser {
        child: std::process::Child,
        output: std::io::BufReader<std::process::ChildStdout>,
    }
    impl Browser {
        fn read(&mut self) -> serde_json::Value {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0, "browser fixture ended");
            serde_json::from_str(&line).expect("browser fixture JSON")
        }
        fn call(&mut self, value: serde_json::Value) -> serde_json::Value {
            let input = self.child.stdin.as_mut().unwrap();
            writeln!(input, "{value}").unwrap();
            input.flush().unwrap();
            self.read()
        }
        fn finish(&mut self) {
            let input = self.child.stdin.as_mut().unwrap();
            writeln!(input, "{{\"op\":\"close\"}}").unwrap();
            input.flush().unwrap();
            assert!(self.child.wait().unwrap().success(), "browser fixture did not close");
        }
    }
    impl Drop for Browser {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    let binary = std::env::var_os("LAYR_INTEROP_BIN").expect("Layr CLI executable");
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(root.join("source")).unwrap();
    std::fs::write(root.join("source/base.txt"), b"base\n").unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new(&binary)
            .args(args)
            .current_dir(&root)
            .env("LAYR_ROOT", root.join("local"))
            .env("LAYR_PERSON_DIR", root.join("person"))
            .env("LAYR_SOCKET", root.join("isolated.sock"))
            .env("LAYR_DIRECT", "1")
            .env("LAYR_AUTHOR_NAME", "Queue test")
            .env("LAYR_AUTHOR_EMAIL", "queue@example.com")
            .env_remove("LAYR_PROJECT")
            .env_remove("LAYR_LINE")
            .output()
            .unwrap();
        assert!(output.status.success(), "layr {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    run(&["init", "p", "--from", root.join("source").to_str().unwrap(), "-q"]);
    run(&["--project", "p", "protect", "main", "--remove"]);
    run(&["--project", "p", "line", "new", "-q", "topic"]);
    std::fs::write(root.join("local/projects/p/lines/topic/source.txt"), b"source\n").unwrap();
    run(&["--project", "p", "--line", "topic", "add", "source.txt"]);
    run(&["--project", "p", "--line", "topic", "save", "-qm", "source"]);
    std::fs::write(root.join("local/projects/p/lines/main/target.txt"), b"target\n").unwrap();
    run(&["--project", "p", "--line", "main", "add", "target.txt"]);
    run(&["--project", "p", "--line", "main", "save", "-qm", "target"]);
    run(&["--project", "p", "protect", "main", "--no-direct"]);
    run(&["--project", "p", "--line", "topic", "queue", "add", "head", "-m", "fix: source"]);
    let mut ctx = context(&root);
    let token_file = root.join("token");
    std::fs::write(&token_file, token).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    ctx.env.insert("LAYR_GATEWAY_TOKEN_FILE".into(), token_file.to_string_lossy().into_owned());
    let api = Api::for_test(origin, "queue", Some(token));
    let mut store = api_store_with(&root, &ctx, api.clone());
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        store.project = repo.p.id.clone();
        store.push(&ctx, &mut repo, &Options::default()).expect("push queued change");
    }
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let platform_config = serde_json::json!({"gate":platform}).to_string();
    let mut focused_store = api_store_with(&root, &ctx, Api::for_test(origin, "cli-queue", Some(token)));
    focused_store.project = store.project.clone();
    focused_store.dir = root.join("local/meta/focused-remote");
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        focused_store.push(&ctx, &mut repo, &Options::default()).expect("publish CLI queue fixture");
    }
    run(&["--project", "p", "config", "check.gate", "true"]);
    run(&["--project", "p", "config", "checks.changes", "[\"gate\"]"]);
    run(&["--project", "p", "config", "checks.platforms", &platform_config]);
    run(&["--project", "p", "protect", "main", "--check", "gate", "--no-direct"]);
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        focused_store.push(&ctx, &mut repo, &Options::default()).expect("publish required check policy");
    }
    // Use the actual CLI and its HTTPS transport for a focused clone and fetch.
    let focused_root = root.join("focused");
    std::fs::create_dir_all(&focused_root).unwrap();
    let focused_remote = std::env::var("LAYR_INTEROP_TLS_REMOTE").expect("TLS fixture remote");
    let ca_file = std::env::var("LAYR_INTEROP_CA_FILE").expect("TLS fixture CA");
    let focused_ctx = context(&focused_root);
    let focused_token =
        reqwest::blocking::get(format!("{origin}__fixture/merge-token/cli-queue/{}", focused_ctx.store.machine.id))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
    let focused_token_file = focused_root.join("job-token");
    std::fs::write(&focused_token_file, &focused_token).unwrap();
    std::fs::set_permissions(&focused_token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let focused = |args: &[&str]| {
        std::process::Command::new(&binary)
            .args(args)
            .current_dir(&focused_root)
            .env("LAYR_ROOT", focused_root.join("local"))
            .env("LAYR_PERSON_DIR", focused_root.join("person"))
            .env("LAYR_SOCKET", focused_root.join("isolated.sock"))
            .env("LAYR_GATEWAY_TOKEN_FILE", &focused_token_file)
            .env("LAYR_GATEWAY_CA_FILE", &ca_file)
            .env("LAYR_DIRECT", "1")
            .env("LAYR_AUTHOR_NAME", "Focused clone")
            .env("LAYR_AUTHOR_EMAIL", "focused@example.com")
            .env_remove("LAYR_PROJECT")
            .env_remove("LAYR_LINE")
            .output()
            .unwrap()
    };
    let run_focused = |args: &[&str]| {
        let output = focused(args);
        assert!(output.status.success(), "focused layr {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    run_focused(&["clone", &focused_remote, "p", "--heads", "--only-line", "main"]);
    if let Some(script) = std::env::var_os("LAYR_INTEROP_CI_SCRIPT") {
        let output = std::process::Command::new(
            std::env::var_os("LAYR_INTEROP_CI_PYTHON").unwrap_or_else(|| "python3".into()),
        )
        .arg(script)
        .env("LAYR_CI_INTEROP_ROOT", focused_root.join("local"))
        .env("LAYR_CI_INTEROP_URL", &focused_remote)
        .env("LAYR_CI_INTEROP_TOKEN_FILE", &focused_token_file)
        .env("LAYR_CI_INTEROP_CA_FILE", &ca_file)
        .env("LAYR_CI_INTEROP_BIN", &binary)
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "CI store interoperability failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    // The reviewer has an independent signed-in person identity.
    let reviewer_token = std::env::var("LAYR_INTEROP_REVIEW_TOKEN").expect("reviewer session");
    let reviewer_token_file = focused_root.join("reviewer-token");
    std::fs::write(&reviewer_token_file, &reviewer_token).unwrap();
    std::fs::set_permissions(&reviewer_token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let reviewer_dir = focused_root.join("reviewer");
    let reviewer_paper = focused_root.join("reviewer-paper");
    let person = |args: &[&str], unlock: bool| {
        let mut command = std::process::Command::new(&binary);
        command
            .args(args)
            .current_dir(&focused_root)
            .env("LAYR_ROOT", focused_root.join("local"))
            .env("LAYR_SOCKET", focused_root.join("isolated.sock"))
            .env("LAYR_PERSON_DIR", &reviewer_dir)
            .env("LAYR_PERSON_LOGIN", "writer2@x.y")
            .env("LAYR_GATEWAY_TOKEN_FILE", &reviewer_token_file)
            .env("LAYR_GATEWAY_CA_FILE", &ca_file)
            .env("LAYR_DIRECT", "1")
            .env_remove("LAYR_PROJECT")
            .env_remove("LAYR_LINE");
        if unlock {
            command.env("LAYR_PERSON_KEY_FILE", &reviewer_paper);
        } else {
            command.env_remove("LAYR_PERSON_KEY_FILE");
        }
        command.output().unwrap()
    };
    let created = person(&["person", "keys", "--paper-key-out", reviewer_paper.to_str().unwrap()], false);
    assert!(created.status.success(), "person keys: {}", String::from_utf8_lossy(&created.stderr));
    let shown = person(&["person", "show", "--json"], true);
    assert!(shown.status.success(), "person show: {}", String::from_utf8_lossy(&shown.stderr));
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["login"], "writer2@x.y");
    assert!(shown["signing_key"].as_str().is_some_and(|key| key.len() == 64));
    assert!(shown["stores"].is_array());
    let reviewer_public = shown["signing_key"].as_str().unwrap();
    let mut browser = std::env::var_os("LAYR_INTEROP_BROWSER_SCRIPT").map(|script| {
        let mut child = std::process::Command::new("node")
            .arg(script)
            .env("LAYR_INTEROP_ORIGIN", origin)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start browser fixture");
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        Browser { child, output }
    });
    let browser_public = browser.as_mut().map(|browser| browser.read()["publicKey"].as_str().unwrap().to_string());
    run(&["--project", "p", "access", "person-key", "writer2@x.y", reviewer_public]);
    run(&["--project", "p", "access", "grant", "person:writer2@x.y", "maintainer"]);
    if let Some(public) = &browser_public {
        run(&["--project", "p", "access", "person-key", "browser@x.y", public]);
        run(&["--project", "p", "access", "grant", "person:browser@x.y", "maintainer"]);
    }
    let approvers = if browser.is_some() { "" } else { "person:writer2@x.y" };
    run(&["--project", "p", "protect", "main", "--check", "gate", "--approvals", "1", "--approvers", approvers, "--no-direct"]);
    let authors = serde_json::json!({focused_ctx.store.machine.id.clone(): []}).to_string();
    run(&["--project", "p", "config", "review.machine-authors", &authors]);
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        focused_store.push(&ctx, &mut repo, &Options::default()).expect("publish reviewer policy");
    }
    run_focused(&["--project", "p", "fetch", "origin", "--heads", "--only-line", "main"]);
    let original_topic = {
        let repo = Repo::open(&focused_ctx, "p").unwrap();
        let lines = repo.p.db.lines().unwrap();
        let main = lines.iter().find(|line| line.name == "main").unwrap();
        let topic = lines.iter().find(|line| line.name == "topic").unwrap();
        assert!(repo.p.has_state(&main.head));
        assert!(!repo.p.has_state(&topic.head), "focused clone restored topic state");
        assert!(!focused_root.join("local/projects/p/lines/topic").exists());
        topic.head.clone()
    };
    std::fs::write(root.join("local/projects/p/lines/topic/source.txt"), b"new topic\n").unwrap();
    run(&["--project", "p", "--line", "topic", "save", "-qam", "new topic"]);
    let new_topic = {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        focused_store.push(&ctx, &mut repo, &Options::default()).expect("push newer CLI topic");
        repo.p.db.lines().unwrap().into_iter().find(|line| line.name == "topic").unwrap().head
    };
    assert_ne!(new_topic, original_topic);
    run_focused(&["--project", "p", "fetch", "origin", "--heads", "--only-line", "main"]);
    {
        let repo = Repo::open(&focused_ctx, "p").unwrap();
        let topic = repo.p.db.lines().unwrap().into_iter().find(|line| line.name == "topic").unwrap();
        assert_eq!(topic.head, new_topic, "focused fetch did not import new records");
        assert!(!repo.p.has_state(&topic.head), "focused fetch restored topic state");
    }
    run_focused(&["--project", "p", "access", "person-key", "writer2@x.y", reviewer_public]);
    run_focused(&["--project", "p", "access", "grant", "person:writer2@x.y", "maintainer"]);
    if let Some(public) = &browser_public {
        run_focused(&["--project", "p", "access", "person-key", "browser@x.y", public]);
        run_focused(&["--project", "p", "access", "grant", "person:browser@x.y", "maintainer"]);
    }
    run_focused(&["--project", "p", "config", "check.gate", "true"]);
    run_focused(&["--project", "p", "config", "checks.platforms", &platform_config]);
    run_focused(&[
        "--project",
        "p",
        "protect",
        "main",
        "--check",
        "gate",
        "--approvals",
        "1",
        "--approvers",
        approvers,
        "--no-direct",
    ]);
    let queue: serde_json::Value =
        serde_json::from_str(&run_focused(&["--project", "p", "queue", "list", "--into", "main", "--json"])).unwrap();
    let entry = &queue[0];
    let entry_id = entry["id"].as_str().unwrap();
    let source = entry["state"].as_str().unwrap();
    assert_eq!(source, original_topic);
    let head = run_focused(&["--project", "p", "--line", "main", "rev-parse", "main"]);
    let candidate_args = [
        "--project",
        "p",
        "queue",
        "candidate",
        entry_id,
        "--into",
        "main",
        "--expect-head",
        head.trim(),
        "--expect-state",
        source,
        "--json",
    ];
    let missing_review = focused(&candidate_args);
    assert!(!missing_review.status.success(), "candidate was prepared without review");
    assert!(
        String::from_utf8_lossy(&missing_review.stderr).contains("approval"),
        "unexpected candidate refusal: {}",
        String::from_utf8_lossy(&missing_review.stderr)
    );
    let submit_and_apply = |action: &str| {
        let request =
            person(&["--project", "p", "--line", "main", "request", action, &original_topic, "--person", "--json"], true);
        assert!(request.status.success(), "review request: {}", String::from_utf8_lossy(&request.stderr));
        let sent: serde_json::Value = serde_json::from_slice(&request.stdout).unwrap();
        let status_url = format!("{origin}api/v1/projects/cli-queue/person-requests/{}/status", sent["id"].as_str().unwrap());
        let status = || {
            reqwest::blocking::Client::new()
                .get(&status_url)
                .bearer_auth(&reviewer_token)
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<serde_json::Value>()
                .unwrap()
        };
        assert_eq!(status()["status"], "pending");
        let applied = run_focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
        let applied: serde_json::Value = serde_json::from_str(&applied).unwrap();
        let result = applied.as_array().unwrap().iter().find(|item| item["id"] == sent["id"]).unwrap();
        assert_eq!(result["status"], "applied", "review request was refused: {result}");
        assert_eq!(status()["status"], "pending", "unpublished evidence appeared remotely");
        run_focused(&["--project", "p", "--line", "main", "push", "origin"]);
        assert_eq!(status()["status"], "applied");
    };
    submit_and_apply("approve");
    let replay = run_focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
    let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
    assert!(replay.as_array().unwrap().iter().all(|item| item["status"] == "ignored"));
    submit_and_apply("withdraw");
    let withdrawn = focused(&candidate_args);
    assert!(!withdrawn.status.success(), "candidate was prepared after review withdrawal");
    assert!(
        String::from_utf8_lossy(&withdrawn.stderr).contains("approval"),
        "unexpected withdrawal refusal: {}",
        String::from_utf8_lossy(&withdrawn.stderr)
    );
    if let Some(browser) = browser.as_mut() {
        let sent = browser.call(serde_json::json!({"op":"approve","project":store.project,"state":original_topic}));
        assert_eq!(sent["status"], "pending", "browser did not submit: {sent}");
        assert!(sent.get("reason").is_none(), "browser delivery is uncertain: {sent}");
        let request_id = sent["id"].as_str().unwrap();
        let status =
            |browser: &mut Browser| browser.call(serde_json::json!({"op":"status","project":store.project,"id":request_id}));
        assert_eq!(status(browser)["status"], "pending");
        let applied = run_focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
        let applied: serde_json::Value = serde_json::from_str(&applied).unwrap();
        assert!(applied.as_array().unwrap().iter().any(|row| row["id"] == request_id && row["status"] == "applied"));
        assert_eq!(status(browser)["status"], "pending");
        run_focused(&["--project", "p", "--line", "main", "push", "origin"]);
        assert_eq!(status(browser)["status"], "applied");
        browser.finish();
    } else {
        submit_and_apply("approve");
    }
    let prepared = run_focused(&[
        "--project",
        "p",
        "queue",
        "candidate",
        entry_id,
        "--into",
        "main",
        "--expect-head",
        head.trim(),
        "--expect-state",
        source,
        "--json",
    ]);
    let prepared: serde_json::Value = serde_json::from_str(&prepared).unwrap();
    let candidate = prepared["candidate"].as_str().unwrap();
    run_focused(&["--project", "p", "--line", "main", "push", "origin"]);
    let before_check = run_focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "checks",
        "pending",
        "--state",
        candidate,
        "--passing",
        "--required-checks",
        "gate",
        "--json",
    ]);
    let pending: serde_json::Value = serde_json::from_str(&before_check).unwrap();
    assert_eq!(pending["pending"].as_array().unwrap().len(), 1, "candidate has no missing check");
    let waiting = run_focused(&[
        "--project",
        "p",
        "queue",
        "run",
        "--into",
        "main",
        "--checked-entry",
        entry_id,
        "--checked-head",
        head.trim(),
        "--checked-state",
        source,
        "--checked-candidate",
        candidate,
        "--json",
    ]);
    let waiting: serde_json::Value = serde_json::from_str(&waiting).unwrap();
    assert!(waiting.as_array().is_some_and(|entries| entries.is_empty()), "unchecked queue did not wait: {waiting}");
    assert_eq!(run_focused(&["--project", "p", "--line", "main", "rev-parse", "main"]), head);
    // macOS requires a maintainer's approval before it runs a check outside main.
    if cfg!(target_os = "macos") {
        {
            let mut repo = Repo::open(&ctx, "p").unwrap();
            focused_store.fetch(&ctx, &mut repo, &Options::default()).expect("fetch candidate for macOS approval");
        }
        run(&["--project", "p", "--line", "main", "approve", "-q", candidate]);
        {
            let mut repo = Repo::open(&ctx, "p").unwrap();
            focused_store.push(&ctx, &mut repo, &Options::default()).expect("publish macOS check approval");
        }
    }
    let runner_root = root.join("runner");
    std::fs::create_dir_all(&runner_root).unwrap();
    let runner_ctx = context(&runner_root);
    let runner_token = reqwest::blocking::get(format!("{origin}__fixture/check-token/cli-queue/{}", runner_ctx.store.machine.id))
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .unwrap();
    let runner_token_file = runner_root.join("token");
    std::fs::write(&runner_token_file, runner_token).unwrap();
    std::fs::set_permissions(&runner_token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let run_runner = |args: &[&str]| {
        let output = std::process::Command::new(&binary)
            .args(args)
            .current_dir(&runner_root)
            .env("LAYR_ROOT", runner_root.join("local"))
            .env("LAYR_PERSON_DIR", runner_root.join("person"))
            .env("LAYR_SOCKET", runner_root.join("isolated.sock"))
            .env("LAYR_GATEWAY_TOKEN_FILE", &runner_token_file)
            .env("LAYR_GATEWAY_CA_FILE", &ca_file)
            .env("LAYR_DIRECT", "1")
            .env("LAYR_AUTHOR_NAME", "Check runner")
            .env("LAYR_AUTHOR_EMAIL", "runner@example.com")
            .env_remove("LAYR_PROJECT")
            .env_remove("LAYR_LINE")
            .output()
            .unwrap();
        assert!(output.status.success(), "runner layr {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    run_runner(&["clone", &focused_remote, "p", "--heads", "--only-line", "main"]);
    run_runner(&["--project", "p", "config", "--local", "checks.runner", "true"]);
    let checked = run_runner(&[
        "--project",
        "p",
        "--line",
        "main",
        "checks",
        "run",
        "--state",
        candidate,
        "--required-checks",
        "gate",
        "--json",
    ]);
    let checked: serde_json::Value = serde_json::from_str(&checked).unwrap();
    assert_eq!(checked["count"], 1, "the runner did not execute the candidate check: {checked}");
    assert_eq!(checked["checks"][0]["exit"], 0);
    run_runner(&["--project", "p", "--line", "main", "push", "origin"]);
    run_focused(&["--project", "p", "fetch", "origin", "--heads", "--only-line", "main"]);
    let after_check = run_focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "checks",
        "pending",
        "--state",
        candidate,
        "--passing",
        "--required-checks",
        "gate",
        "--json",
    ]);
    let pending: serde_json::Value = serde_json::from_str(&after_check).unwrap();
    assert!(pending["pending"].as_array().unwrap().is_empty(), "candidate check was not imported: {pending}");
    run_focused(&[
        "--project",
        "p",
        "queue",
        "run",
        "--into",
        "main",
        "--checked-entry",
        entry_id,
        "--checked-head",
        head.trim(),
        "--checked-state",
        source,
        "--checked-candidate",
        candidate,
    ]);
    crate::free::finish(&runner_ctx);
    drop(runner_ctx);
    assert_eq!(std::fs::read(focused_root.join("local/projects/p/lines/main/source.txt")).unwrap(), b"source\n");
    assert_eq!(std::fs::read(focused_root.join("local/projects/p/lines/main/target.txt")).unwrap(), b"target\n");
    run_focused(&["--project", "p", "--line", "main", "push", "origin"]);
    let (main_id, main_head) = {
        let repo = Repo::open(&focused_ctx, "p").unwrap();
        let main = repo.p.db.lines().unwrap().into_iter().find(|line| line.name == "main").unwrap();
        (main.id, main.head)
    };
    let focused_api = Api::for_test(origin, "cli-queue", Some(token));
    assert!(focused_api.refs("", "lines/").unwrap().iter().any(|r|
        r.name == format!("lines/{main_id}") && r.target.as_deref() == Some(&main_head)));
    // The creator names the merge machine and gives it the merge role (docs/ci-trust.md), not full trust.
    run(&["--project", "p", "config", "queue.merge-machine", &focused_ctx.store.machine.id]);
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        focused_store.push(&ctx, &mut repo, &Options::default()).unwrap();
    }
    // A release of the checked main must work through the same V1 gateway.
    run_focused(&["--project", "p", "fetch", "origin", "--heads", "--only-line", "main"]);
    run_focused(&["--project", "p", "--line", "main", "ref", "set", "release/v9.9.9", "main"]);
    run_focused(&["--project", "p", "--line", "main", "push", "origin"]);
    let untrusted = focused(&["--project", "p", "--line", "main", "release", "verify-main", "release/v9.9.9", "--expect", "main"]);
    assert!(!untrusted.status.success(), "an untrusted merge machine's release ref was accepted");
    assert!(String::from_utf8_lossy(&untrusted.stderr).contains("release ref does not exist on the remote"),
        "unexpected refusal: {}", String::from_utf8_lossy(&untrusted.stderr));
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        crate::cmd::replicate::trust(&ctx, &repo.p, &focused_ctx.store.machine.id,
            &hex::encode(focused_ctx.store.machine.key.verifying_key().to_bytes()), Some(&serde_json::json!({"role": "merge"}))).unwrap();
        focused_store.push(&ctx, &mut repo, &Options::default()).unwrap();
    }
    run_focused(&["--project", "p", "fetch", "origin", "--heads", "--only-line", "main"]);
    run_focused(&["--project", "p", "--line", "main", "release", "verify-main", "release/v9.9.9", "--expect", "main"]);
    // The merge role sets no other ref: the creator's fetch takes the release ref and ignores `env/prod`.
    run_focused(&["--project", "p", "--line", "main", "ref", "set", "env/prod", "main"]);
    run_focused(&["--project", "p", "--line", "main", "push", "origin"]);
    {
        let mut repo = Repo::open(&ctx, "p").unwrap();
        focused_store.fetch(&ctx, &mut repo, &Options::default()).unwrap();
        let now = crate::ids::now_ms();
        assert!(repo.p.db.ref_value("release/v9.9.9", now).unwrap().is_some(), "the merge machine's release ref is missing");
        assert!(repo.p.db.ref_value("env/prod", now).unwrap().is_none(), "a merge machine changed a ref outside its role");
    }
    let release_file = focused_root.join("release-fixture.txt");
    std::fs::write(&release_file, b"release fixture\n").unwrap();
    run_focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "release",
        "attach",
        "release/v9.9.9",
        release_file.to_str().unwrap(),
        "--expect",
        "main",
    ]);
    let downloads = focused_root.join("release-downloads");
    run_focused(&["--project", "p", "--line", "main", "release", "get", "release/v9.9.9", "-o", downloads.to_str().unwrap()]);
    assert_eq!(std::fs::read(downloads.join("release-fixture.txt")).unwrap(), b"release fixture\n");
    let large = focused_root.join("large-release.bin");
    std::fs::File::create(&large).unwrap().set_len(65 << 20).unwrap();
    run_focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "release",
        "attach",
        "release/v9.9.9",
        large.to_str().unwrap(),
        "--expect",
        "main",
    ]);
    let large_download = focused_root.join("large-release-downloads");
    run_focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "release",
        "get",
        "release/v9.9.9",
        "-o",
        large_download.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::metadata(large_download.join("large-release.bin")).unwrap().len(), 65 << 20);
    let published = run_focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "release",
        "attach",
        "release/v9.9.9",
        release_file.to_str().unwrap(),
        "--expect",
        "main",
        "--public-files",
        "--json",
    ]);
    let published: serde_json::Value = serde_json::from_str(&published).unwrap();
    assert_eq!(published[0]["name"], "release-fixture.txt");
    let listed = run_focused(&["--project", "p", "--line", "main", "release", "files", "release/v9.9.9", "--json"]);
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed[0]["sha256"], published[0]["sha256"]);
    let public_url = format!("{origin}p/cli-queue/release/v9.9.9/release-fixture.txt");
    let public = reqwest::blocking::get(&public_url).unwrap().error_for_status().unwrap().bytes().unwrap();
    assert_eq!(&public[..], b"release fixture\n");
    let manifest: serde_json::Value = reqwest::blocking::get(format!("{origin}p/cli-queue/release/v9.9.9"))
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(manifest["files"][0]["sha256"], published[0]["sha256"]);
    let forged = reqwest::blocking::Client::new()
        .put(format!("{origin}api/v1/projects/cli-queue/public-release/v9.9.9/release-fixture.txt"))
        .bearer_auth(&focused_token)
        .header("Content-Type", "application/octet-stream")
        .body("wrong bytes")
        .send()
        .unwrap();
    assert_eq!(forged.status().as_u16(), 409);
    let mut wrong_manifest = manifest.clone();
    wrong_manifest["files"][0]["sha256"] = serde_json::json!("0".repeat(64));
    let forged = reqwest::blocking::Client::new()
        .put(format!("{origin}api/v1/projects/cli-queue/public-release/v9.9.9/files.json"))
        .bearer_auth(&focused_token)
        .header("Content-Type", "application/octet-stream")
        .body(wrong_manifest.to_string())
        .send()
        .unwrap();
    assert_eq!(forged.status().as_u16(), 409);
    run_focused(&["--project", "p", "--line", "main", "release", "publish-public", "release/v9.9.9", "--expect", "main"]);
    std::fs::write(&release_file, b"changed release\n").unwrap();
    let conflict = focused(&[
        "--project",
        "p",
        "--line",
        "main",
        "release",
        "attach",
        "release/v9.9.9",
        release_file.to_str().unwrap(),
        "--expect",
        "main",
        "--public-files",
    ]);
    assert!(!conflict.status.success(), "a published file cannot be replaced");
    let after: serde_json::Value =
        serde_json::from_str(&run_focused(&["--project", "p", "--line", "main", "release", "files", "release/v9.9.9", "--json"]))
            .unwrap();
    assert_eq!(after, listed, "a conflicting public attach changed the signed list");
    assert_eq!(
        &reqwest::blocking::get(&public_url).unwrap().error_for_status().unwrap().bytes().unwrap()[..],
        b"release fixture\n"
    );
    // An interrupted, different executor has claimed a withdrawal. Its key is registered at
    // the gateway but was never published as a record to this clone.
    let withheld =
        person(&["--project", "p", "--line", "main", "request", "withdraw", &original_topic, "--person", "--json"], true);
    assert!(withheld.status.success(), "withdraw request: {}", String::from_utf8_lossy(&withheld.stderr));
    let withheld: serde_json::Value = serde_json::from_slice(&withheld.stdout).unwrap();
    let withheld_id = withheld["id"].as_str().unwrap();
    let abandoned_root = root.join("abandoned");
    std::fs::create_dir_all(&abandoned_root).unwrap();
    let abandoned_ctx = context(&abandoned_root);
    let abandoned_token =
        reqwest::blocking::get(format!("{origin}__fixture/merge-token/cli-queue/{}", abandoned_ctx.store.machine.id))
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .unwrap();
    let abandoned_api = Api::for_test(origin, "cli-queue", Some(&abandoned_token));
    abandoned_api.register_machine(&abandoned_ctx.store.machine.id, &abandoned_ctx.store.machine.key.verifying_key()).unwrap();
    let claim = layr_store::request::Claim {
        format: 1,
        project: store.project.clone(),
        request: withheld_id.into(),
        machine: abandoned_ctx.store.machine.id.clone(),
    };
    assert!(abandoned_api
        .put_new(
            &format!("requests/claims/{withheld_id}.json"),
            claim.sign(&abandoned_ctx.store.machine.key).unwrap().to_bytes().unwrap()
        )
        .unwrap());
    reqwest::blocking::get(format!("{origin}__fixture/merge-token/cli-queue/{}", focused_ctx.store.machine.id))
        .unwrap()
        .error_for_status()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let later = person(&["--project", "p", "--line", "main", "request", "approve", &original_topic, "--person", "--json"], true);
    assert!(later.status.success());
    let later: serde_json::Value = serde_json::from_slice(&later.stdout).unwrap();
    let later_id = later["id"].as_str().unwrap();
    assert!(withheld_id < later_id, "fixture requests are out of order");
    let page_check = if std::env::var_os("LAYR_INTEROP_SMALL_PAGES").is_some() {
        let listed = focused_api.list("requests/").unwrap();
        let expected = listed.iter().position(|item| item.key == format!("requests/{withheld_id}.json")).unwrap() + 1;
        let before = reqwest::blocking::get(format!("{origin}__fixture/person-list-count"))
            .unwrap()
            .text()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        Some((expected, before))
    } else {
        None
    };
    let pending = focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
    assert_eq!(pending.status.code(), Some(2), "a waiting claim must stop the CI job");
    let pending: serde_json::Value = serde_json::from_slice(&pending.stdout).unwrap();
    let pending = pending.as_array().unwrap();
    let first = pending.iter().find(|row| row["id"] == withheld_id).unwrap();
    assert_eq!(first["status"], "claimed", "an interrupted claim must not become a refusal: {first}");
    assert!(!pending.iter().any(|row| row["id"] == later_id), "later review overtook a claimed request");
    if let Some((expected, before)) = page_check {
        let after = reqwest::blocking::get(format!("{origin}__fixture/person-list-count"))
            .unwrap()
            .text()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(after - before, expected, "the blocked pass fetched a later page");
    }
    {
        let repo = Repo::open(&focused_ctx, "p").unwrap();
        assert!(repo
            .p
            .db
            .records(usize::MAX >> 1)
            .unwrap()
            .iter()
            .all(|record| record.data["request_id"] != withheld_id && record.data["request_id"] != later_id));
    }
    // Restart the original claimant with the same fixture-owned machine identity. It must
    // finish the old claim before the later request; another clone then sees both as done.
    reqwest::blocking::get(format!("{origin}__fixture/merge-token/cli-queue/{}", abandoned_ctx.store.machine.id))
        .unwrap()
        .error_for_status()
        .unwrap();
    let abandoned_token_file = abandoned_root.join("token");
    std::fs::write(&abandoned_token_file, &abandoned_token).unwrap();
    std::fs::set_permissions(&abandoned_token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let run_abandoned = |args: &[&str]| {
        let output = std::process::Command::new(&binary)
            .args(args)
            .current_dir(&abandoned_root)
            .env("LAYR_ROOT", abandoned_root.join("local"))
            .env("LAYR_PERSON_DIR", abandoned_root.join("person"))
            .env("LAYR_SOCKET", abandoned_root.join("isolated.sock"))
            .env("LAYR_GATEWAY_TOKEN_FILE", &abandoned_token_file)
            .env("LAYR_GATEWAY_CA_FILE", &ca_file)
            .env("LAYR_DIRECT", "1")
            .env("LAYR_AUTHOR_NAME", "Restarted merge job")
            .env("LAYR_AUTHOR_EMAIL", "restart@example.com")
            .env_remove("LAYR_PROJECT")
            .env_remove("LAYR_LINE")
            .output()
            .unwrap();
        assert!(output.status.success(), "restart layr {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    run_abandoned(&["clone", &focused_remote, "p", "--heads", "--only-line", "main"]);
    run_abandoned(&["--project", "p", "access", "person-key", "writer2@x.y", reviewer_public]);
    run_abandoned(&["--project", "p", "access", "grant", "person:writer2@x.y", "maintainer"]);
    let resumed = run_abandoned(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
    let resumed: serde_json::Value = serde_json::from_str(&resumed).unwrap();
    let resumed = resumed.as_array().unwrap();
    assert!(
        resumed.iter().any(|row| row["id"] == withheld_id && row["status"] == "applied"),
        "old claim not resumed: {resumed:?}"
    );
    assert!(
        resumed.iter().any(|row| row["id"] == later_id && row["status"] == "applied"),
        "later request not applied: {resumed:?}"
    );
    run_abandoned(&["--project", "p", "--line", "main", "push", "origin"]);
    reqwest::blocking::get(format!("{origin}__fixture/merge-token/cli-queue/{}", focused_ctx.store.machine.id))
        .unwrap()
        .error_for_status()
        .unwrap();
    run_focused(&["--project", "p", "fetch", "origin", "--heads", "--only-line", "main"]);
    let replay = run_focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
    let replay: serde_json::Value = serde_json::from_str(&replay).unwrap();
    for id in [withheld_id, later_id] {
        assert!(
            replay.as_array().unwrap().iter().any(|row| row["id"] == id && row["status"] == "ignored"),
            "a completed request was not ignored: {replay}"
        );
    }
    // The gateway preserves acceptance order even when valid UUIDv7 IDs sort in reverse.
    let mut reviewer_env = HashMap::new();
    reviewer_env.insert("LAYR_PERSON_DIR".into(), reviewer_dir.to_string_lossy().into_owned());
    reviewer_env.insert("LAYR_PERSON_KEY_FILE".into(), reviewer_paper.to_string_lossy().into_owned());
    reviewer_env.insert("LAYR_PERSON_LOGIN".into(), "writer2@x.y".into());
    let access =
        crate::person::load(&reviewer_env, &focused_ctx.caller.home, &focused_root, focused_ctx.caller.uid, Some("writer2@x.y"))
            .unwrap()
            .unwrap();
    let reviewer_key =
        crate::person::signing_key_at(&reviewer_env, &focused_ctx.caller.home, focused_ctx.caller.uid, &access).unwrap();
    let reviewer_api = Api::for_test(origin, "cli-queue", Some(&reviewer_token));
    let first_id = "01800000-0000-7000-8000-000000000002";
    let second_id = "01700000-0000-7000-8000-000000000001";
    assert!(first_id > second_id);
    for (id, action) in [
        (first_id, layr_store::request::Action::Withdraw { state: original_topic.clone(), into: "main".into() }),
        (second_id, layr_store::request::Action::Approve { state: original_topic.clone(), into: "main".into() }),
    ] {
        let request = layr_store::request::Request::new(
            id.into(),
            store.project.clone(),
            "writer2@x.y".into(),
            crate::ids::now_ms(),
            action,
        )
        .unwrap();
        assert!(reviewer_api
            .put_new(&format!("requests/{id}.json"), request.sign(&reviewer_key).unwrap().to_bytes().unwrap())
            .unwrap());
    }
    let ordered = run_focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--no-run-queue", "--json"]);
    let ordered: serde_json::Value = serde_json::from_str(&ordered).unwrap();
    let rows = ordered.as_array().unwrap();
    let first = rows.iter().position(|row| row["id"] == first_id && row["status"] == "applied").unwrap();
    let second = rows.iter().position(|row| row["id"] == second_id && row["status"] == "applied").unwrap();
    assert!(first < second, "withdrawal and approval were applied out of acceptance order");
    {
        let repo = Repo::open(&focused_ctx, "p").unwrap();
        assert!(repo.read_model().unwrap().person_approvals(&original_topic).iter().any(|(login, _)| login == "writer2@x.y"));
    }
    // A queue-add request still starts the configured target after result spooling.
    run_focused(&["--project", "p", "config", "queue.run", "main"]);
    let queued = person(&["--project", "p", "--line", "main", "request", "queue-add", &new_topic, "--person", "--json"], true);
    assert!(queued.status.success(), "queue request: {}", String::from_utf8_lossy(&queued.stderr));
    let queued: serde_json::Value = serde_json::from_slice(&queued.stdout).unwrap();
    let applied = run_focused(&["--project", "p", "requests", "apply", "--remote", "origin", "--json"]);
    let applied: serde_json::Value = serde_json::from_str(&applied).unwrap();
    assert!(applied.as_array().unwrap().iter().any(|row| row["id"] == queued["id"] && row["status"] == "applied"));
    let queue: serde_json::Value =
        serde_json::from_str(&run_focused(&["--project", "p", "queue", "list", "--into", "main", "--json"])).unwrap();
    let entry = queue.as_array().unwrap().iter().find(|row| row["id"] == queued["id"]).unwrap();
    assert_eq!(entry["status"], "failed", "the configured queue did not run: {entry}");
    crate::free::finish(&abandoned_ctx);
    drop(abandoned_ctx);
    crate::free::finish(&focused_ctx);
    drop(focused_ctx);
    // A separate store gets a real merge-job token from the local fixture and clones the
    // queued change. Its own machine signs the adopt and queue.result records.
    let merger_root = root.join("merger");
    std::fs::create_dir_all(&merger_root).unwrap();
    let mut merger_ctx = context(&merger_root);
    let machine = merger_ctx.store.machine.id.clone();
    let job_token = reqwest::blocking::get(format!("{origin}__fixture/merge-token/{machine}"))
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .unwrap();
    let job_token_file = merger_root.join("token");
    std::fs::write(&job_token_file, &job_token).unwrap();
    std::fs::set_permissions(&job_token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    merger_ctx.env.insert("LAYR_GATEWAY_TOKEN_FILE".into(), job_token_file.to_string_lossy().into_owned());
    let job_api = Api::for_test(origin, "queue", Some(&job_token));
    let mut job_store = api_store_with(&merger_root, &merger_ctx, job_api);
    job_store.project = store.project.clone();
    {
        let project = crate::store::Project::create_empty(&merger_ctx.store, "p").unwrap();
        let mut repo = Repo { p: project, ctx: Some(&merger_ctx) };
        let report = job_store.clone_into(&merger_ctx, &mut repo, &Options::default()).expect("clone for merge job");
        assert!(report.missing_lines.is_empty(), "clone omitted lines: {:?}", report.missing_lines);
        // Match the configuration records written after clone by the real CLI.
        for (key, value) in [
            ("cloud.remote.origin.url", serde_json::json!("layr+https://cloud.test/queue")),
            ("cloud.remote.origin.mode", serde_json::json!("private")),
            ("access", {
                let mut access = repo.policy().access_json();
                access[format!("user:{}", merger_ctx.caller.uid)] = serde_json::json!("admin");
                access
            }),
        ] {
            let mut config = Record::new("config");
            config.data = serde_json::json!({"config": {key: value}});
            repo.commit_record(&merger_ctx, config).unwrap();
        }
    }
    let run_job = |args: &[&str]| {
        let output = std::process::Command::new(&binary)
            .args(args)
            .current_dir(&merger_root)
            .env("LAYR_ROOT", merger_root.join("local"))
            .env("LAYR_PERSON_DIR", merger_root.join("person"))
            .env("LAYR_SOCKET", merger_root.join("isolated.sock"))
            .env("LAYR_DIRECT", "1")
            .env("LAYR_AUTHOR_NAME", "Merge job")
            .env("LAYR_AUTHOR_EMAIL", "merge@example.com")
            .env_remove("LAYR_PROJECT")
            .env_remove("LAYR_LINE")
            .output()
            .unwrap();
        assert!(output.status.success(), "merge job layr {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    run_job(&["--project", "p", "queue", "run"]);
    let head = run_job(&["--project", "p", "--line", "main", "rev-parse", "head"]);
    assert_eq!(std::fs::read(merger_root.join("local/projects/p/lines/main/source.txt")).unwrap(), b"source\n");
    assert_eq!(std::fs::read(merger_root.join("local/projects/p/lines/main/target.txt")).unwrap(), b"target\n");
    let (main_id, main_head) = {
        let mut repo = Repo::open(&merger_ctx, "p").unwrap();
        job_store.push(&merger_ctx, &mut repo, &Options::default()).expect("merge job push queue result");
        let main = repo.p.db.lines().unwrap().into_iter().find(|line| line.name == "main").unwrap();
        (main.id, main.head)
    };
    assert_eq!(crate::ids::display(&main_head), head.trim());
    crate::free::finish(&merger_ctx);
    drop(merger_ctx);
    let refs = api.refs("", "lines/").unwrap();
    assert!(
        refs.iter().any(|r| r.name == format!("lines/{main_id}") && r.target.as_deref() == Some(&main_head)),
        "merged main head was not published: {refs:?}"
    );
    crate::free::finish(&ctx);
    drop(ctx);
    let errors = crate::store::empty_all(&root).unwrap();
    assert!(errors.is_empty(), "temporary cleanup: {errors:?}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_new_clone_uses_published_server_refs_and_refuses_missing_target_records() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let ctx = context(&root);
    let fake = Fake::start(Some(PROJECT));
    let store = api_store(&root, &ctx, &fake);
    let (repo, states, line) = project(&ctx);
    let name = (String::new(), format!("lines/{line}"));
    fake.state.lock().unwrap().refs.insert(name.clone(), (states[1].clone(), 7));
    let selected = super::reader::clone_lines(&store, &repo).unwrap();
    assert_eq!(selected.iter().find(|row| row.id == line).unwrap().head, states[1]);
    assert_eq!(repo.p.db.line_by_id(&line).unwrap().unwrap().head, states[2], "selection does not change the local line");
    fake.state.lock().unwrap().refs.insert(name, (crate::ids::new_id(), 8));
    assert!(super::reader::clone_lines(&store, &repo).unwrap_err().to_string().contains("no fetched record"));
    fake.state.lock().unwrap().refs.clear();
    assert!(super::reader::clone_lines(&store, &repo).unwrap().is_empty(), "unpublished record heads are not server refs");
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
}
