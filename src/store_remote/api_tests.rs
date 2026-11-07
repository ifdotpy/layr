//! The Layr Cloud API v1 client against an in-process fake server (layr-cloud docs/api.md).

use super::api::{Api, Cas, RefUpdate};
use super::backend::Backend;
use super::{format, keys, reader, writer, Options, Store};
use crate::ctx::{Caller, Ctx};
use crate::model::{LineChange, LineState, Record, StateRec};
use crate::repo::Repo;
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
    states: BTreeMap<(String, String), String>,
    refs: BTreeMap<(String, String), (String, u64)>,
    machines: BTreeMap<String, String>,
    minimum: Option<String>,
    part_size: usize,
    login_polls: usize,
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
                return (
                    200,
                    json(serde_json::json!({"code":"c1","link":"https://cloud.test/login/c1","expires":0})),
                    Vec::new(),
                );
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
            ("POST", ["objects", "missing"]) => {
                let missing: Vec<&str> = body["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|id| !s.objects.contains_key(*id))
                    .collect();
                (200, json(serde_json::json!({ "missing": missing })), Vec::new())
            }
            ("PUT", ["objects", id]) => {
                let kind = request.headers.get("layr-object-type").cloned().unwrap_or_default();
                if request.body.len() > super::api::SINGLE_PUT_LIMIT || format::object_id(&kind, &request.body) != *id {
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
                if object != *id || format::object_id(&kind, &bytes) != *id {
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
                let tree = body["tree"].as_str().unwrap().to_string();
                if s.objects.get(&tree).is_none_or(|(kind, _)| kind != format::OBJECT_STATE_TREE) {
                    return (400, json(serde_json::json!({"error":{"code":"invalid"}})), Vec::new());
                }
                match s.states.get(&(scope.clone(), state.to_string())) {
                    Some(old) if old != &tree => (409, Vec::new(), Vec::new()),
                    Some(_) => (200, Vec::new(), Vec::new()),
                    None => {
                        s.states.insert((scope, state.to_string()), tree);
                        (201, Vec::new(), Vec::new())
                    }
                }
            }
            ("GET", ["states", state]) => match s.states.get(&(scope, state.to_string())) {
                Some(tree) => (200, json(serde_json::json!({ "tree": tree })), Vec::new()),
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

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut bytes = vec![0; n];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

#[test]
fn an_object_over_64_mib_goes_up_in_parts_and_a_held_object_is_not_sent_again() {
    let fake = Fake::start(None);
    let api = fake.api();
    let bytes = random_bytes((64 << 20) + 4096);
    let (id, new) = api.put_object(format::OBJECT_PART, &bytes).unwrap();
    assert!(new);
    assert_eq!(id, format::object_id("part", &bytes));
    assert_eq!(fake.count("POST /api/v1/projects/p/objects/missing"), 1, "a large upload asks first");
    assert_eq!(fake.count(&format!("POST /api/v1/projects/p/objects/{id}/uploads")), 2, "start and complete");
    assert_eq!(fake.count(&format!("PUT /api/v1/projects/p/objects/{id}/uploads/u1/")), 9, "8 MiB parts");
    assert_eq!(fake.count(&format!("PUT /api/v1/projects/p/objects/{id}")), 9, "no single PUT over 64 MiB");
    assert_eq!(api.get_object("part", &id).unwrap().unwrap(), bytes);
    // The project holds it now: the second upload sends nothing.
    let (_, new) = api.put_object(format::OBJECT_PART, &bytes).unwrap();
    assert!(!new);
    assert_eq!(fake.count(&format!("POST /api/v1/projects/p/objects/{id}/uploads")), 2);
    // The typed id keeps types apart, and a read checks the bytes against the id.
    assert_ne!(format::object_id("part", b"x"), format::object_id("state-tree", b"x"));
    assert!(api.get_object("state-tree", &id).unwrap_err().to_string().contains("does not match its id"));
    // A missing object is missing, and `objects/missing` names it.
    let absent = format::object_id("part", b"absent");
    assert_eq!(api.get_object("part", &absent).unwrap(), None);
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
    let error = api.put_object("part", b"bytes").unwrap_err().to_string();
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
    Store {
        backend: Backend::Api(fake.api()),
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
    record.data = serde_json::json!({"id":PROJECT,"name":"p","config":{}});
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
        project.append(record, &ctx.actor()).unwrap();
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

#[test]
fn a_push_sends_typed_objects_states_records_and_one_ref_move_and_a_reader_gets_the_files_back() {
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
        for state in &states {
            let tree = &server.states[&(String::new(), state.clone())];
            let (kind, bytes) = &server.objects[tree];
            assert_eq!(kind, format::OBJECT_STATE_TREE);
            let (header, _) = format::split_tree(bytes).unwrap();
            assert_eq!((header.tree.as_str(), header.attempt.as_str()), ("full", format::CONTENT_ATTEMPT), "complete trees");
        }
        for (id, (kind, bytes)) in &server.objects {
            assert_eq!(&format::object_id(kind, bytes), id);
            assert!(!bytes.starts_with(b"age-encryption.org/"), "plain objects");
        }
        assert_eq!(server.refs[&(String::new(), format!("lines/{line}"))].0, states[2]);
    }
    // A reader with the machine keys of the records reads each state back from the server.
    let identity = store.identity(&store.descriptor().unwrap(), None).unwrap();
    assert_eq!(identity, keys::PLAIN);
    let records = reader::records(&store.backend, &identity, PROJECT).unwrap();
    assert!(records.gaps.is_empty());
    let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
    for (n, state) in states.iter().enumerate() {
        let (_, _, tree) = source.tree(state, format::CONTENT_ATTEMPT).unwrap();
        assert!(tree.files.values().flat_map(|e| e.3.iter()).all(|link| link.is_content()), "links name parts by id");
        assert_eq!(source.read_file(&tree, "payload").unwrap(), format!("version {}\n", n % 2).as_bytes());
        let local = repo.p.state_path(state);
        assert_eq!(source.read_file(&tree, "stable").unwrap(), std::fs::read(local.join("stable")).unwrap());
        if n == 2 {
            assert_eq!(source.read_file(&tree, "dir/large").unwrap(), std::fs::read(local.join("dir/large")).unwrap());
        }
    }
    // A second push sends no object, no state and no ref move.
    let puts = fake.count("PUT ");
    let cas = fake.count("POST /api/v1/projects/p/refs/cas");
    let again = push(&store, &ctx, &repo).unwrap();
    assert_eq!((again.states, again.records), (0, 0));
    assert_eq!(fake.count("PUT "), puts);
    assert_eq!(fake.count("POST /api/v1/projects/p/refs/cas"), cas);
    drop(repo);
    std::fs::remove_dir_all(root).unwrap();
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
fn a_missing_object_fails_the_read_of_its_file_and_names_the_object() {
    let root = std::env::temp_dir().join(crate::ids::new_id());
    std::fs::create_dir_all(&root).unwrap();
    let ctx = context(&root);
    let fake = Fake::start(Some(PROJECT));
    let store = api_store(&root, &ctx, &fake);
    let (repo, states, _) = project(&ctx);
    push(&store, &ctx, &repo).unwrap();
    let records = reader::records(&store.backend, keys::PLAIN, PROJECT).unwrap();
    let mut source = reader::Reader::new(&store.backend, keys::PLAIN, &records.keys);
    let (_, _, tree) = source.tree(&states[2], format::CONTENT_ATTEMPT).unwrap();
    let part = tree.files["dir/large"].3[0].0.clone();
    fake.state.lock().unwrap().objects.remove(&part);
    let mut fresh = reader::Reader::new(&store.backend, keys::PLAIN, &records.keys);
    let error = format!("{:#}", fresh.read_file(&tree, "dir/large").unwrap_err());
    assert!(error.contains(&part), "{error}");
    // A state whose tree the server lacks is not listed.
    assert!(store.backend.list(&format!("states/{}/", crate::ids::new_id())).unwrap().is_empty());
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
    assert_eq!(*shown.borrow(), "https://cloud.test/login/c1");
    assert!(fake.state.lock().unwrap().machines.contains_key(&store.machine));
    let path = super::api::session_file(&ctx.caller.home, "127.0.0.1");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // The next command reads the saved session as its token.
    let mut api = Api::for_test(&fake.origin, "p", None);
    api.connect(&ctx, PROJECT).unwrap();
    api.put_object("part", b"with the session").unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
