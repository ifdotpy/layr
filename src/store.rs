//! The store: machine identity, projects, their folders, the operation log and the index.
//!
//! ```text
//! <root>/machine/{id,name,key}        machine identity (0700)
//! <root>/projects/<name>/
//!   states/<state-id>                 read-only snapshots (0700)
//!   lines/<line-name>                 writable snapshots (0711, no listing)
//!   git/                              git object store for the remote bridge (0755)
//!   meta/log/<machine-id>.jsonl       operation log, one file per machine (0700)
//!   meta/index.sqlite                 query index, rebuilt from the logs
//!   meta/stage/<line-id>              staging lines (the index of `layr add`)
//!   meta/scan, meta/work              temporary snapshots
//! ```

use crate::db::Db;
use crate::ids;
use crate::model::{Actor, Record};
use crate::oplog;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub struct Machine {
    pub id: String,
    pub name: String,
    pub key: SigningKey,
}

pub struct Store {
    pub root: PathBuf,
    pub machine: Machine,
}

pub fn default_root() -> PathBuf {
    std::env::var_os("LAYR_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/studio"))
}

fn mkdir_mode(p: &Path, mode: u32) -> Result<()> {
    if !p.exists() {
        fs::create_dir_all(p).with_context(|| format!("mkdir {}", p.display()))?;
    }
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "machine".into())
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        let mdir = root.join("machine");
        if !mdir.join("key").exists() {
            if !root.exists() {
                mkdir_mode(root, 0o755)?;
            }
            mkdir_mode(&mdir, 0o700)?;
            let id = ids::new_id();
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            crate::fsutil::atomic_write(&mdir.join("id"), id.as_bytes(), 0o600)?;
            crate::fsutil::atomic_write(&mdir.join("name"), hostname().as_bytes(), 0o600)?;
            crate::fsutil::atomic_write(&mdir.join("key"), hex::encode(key.to_bytes()).as_bytes(), 0o600)?;
        }
        let id = fs::read_to_string(mdir.join("id"))?.trim().to_string();
        let name = fs::read_to_string(mdir.join("name")).unwrap_or_else(|_| hostname()).trim().to_string();
        let kb = hex::decode(fs::read_to_string(mdir.join("key"))?.trim())?;
        let arr: [u8; 32] = kb.try_into().map_err(|_| anyhow!("bad machine key"))?;
        let key = SigningKey::from_bytes(&arr);
        mkdir_mode(&root.join("projects"), 0o755)?;
        Ok(Store { root: root.to_path_buf(), machine: Machine { id, name, key } })
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn project_names(&self) -> Result<Vec<String>> {
        let mut v = Vec::new();
        for e in fs::read_dir(self.projects_dir())?.flatten() {
            if e.path().join("meta").join("log").is_dir() {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        v.sort();
        Ok(v)
    }
}

pub fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && name.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.ends_with(".lock")
        && !name.contains("..");
    if !ok {
        bail!("invalid {kind} name '{name}': use letters, digits, '-', '_' and '.'; start with a letter or digit");
    }
    Ok(())
}

pub struct Project<'a> {
    pub store: &'a Store,
    pub name: String,
    pub dir: PathBuf,
    pub id: String,
    pub db: Db,
}

/// An error after the record reached the log: the operation happened, its snapshots stay.
#[derive(Debug)]
pub struct Logged(pub String);

impl std::fmt::Display for Logged {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Logged {}

pub fn is_logged(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Logged>().is_some()
}

pub struct Lock {
    _f: fs::File,
}

impl<'a> Project<'a> {
    pub fn states_dir(&self) -> PathBuf {
        self.dir.join("states")
    }
    pub fn lines_dir(&self) -> PathBuf {
        self.dir.join("lines")
    }
    pub fn meta_dir(&self) -> PathBuf {
        self.dir.join("meta")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.meta_dir().join("log")
    }
    pub fn git_dir(&self) -> PathBuf {
        self.dir.join("git")
    }
    pub fn state_path(&self, id: &str) -> PathBuf {
        self.states_dir().join(id)
    }
    pub fn line_path(&self, name: &str) -> PathBuf {
        self.lines_dir().join(name)
    }
    pub fn stage_path(&self, line_id: &str) -> PathBuf {
        self.meta_dir().join("stage").join(line_id)
    }
    pub fn scan_dir(&self) -> PathBuf {
        self.meta_dir().join("scan")
    }
    pub fn work_dir(&self) -> PathBuf {
        self.meta_dir().join("work")
    }
    pub fn own_log(&self) -> PathBuf {
        self.log_dir().join(format!("{}.jsonl", self.store.machine.id))
    }

    pub fn create(store: &'a Store, name: &str, actor: &Actor, config: serde_json::Value) -> Result<Project<'a>> {
        check_name("project", name)?;
        let dir = store.projects_dir().join(name);
        if dir.join("meta").join("log").exists() {
            bail!("project '{name}' already exists");
        }
        if !crate::btrfs::is_btrfs(&store.projects_dir()) {
            bail!("{} is not on btrfs", store.projects_dir().display());
        }
        mkdir_mode(&dir, 0o755)?;
        mkdir_mode(&dir.join("states"), 0o700)?;
        mkdir_mode(&dir.join("lines"), 0o711)?;
        mkdir_mode(&dir.join("meta"), 0o700)?;
        for d in ["log", "stage", "scan", "work"] {
            mkdir_mode(&dir.join("meta").join(d), 0o700)?;
        }
        let id = ids::new_id();
        let db = Db::open(&dir.join("meta").join("index.sqlite"))?;
        let p = Project { store, name: name.to_string(), dir, id: id.clone(), db };
        let mut rec = Record::new("project");
        rec.data = serde_json::json!({"name": name, "id": id, "config": config});
        p.append(rec, actor)?;
        Ok(p)
    }

    /// Folders of a project without records (for a restore or a replica).
    pub fn create_empty(store: &'a Store, name: &str) -> Result<Project<'a>> {
        check_name("project", name)?;
        let dir = store.projects_dir().join(name);
        if dir.join("meta").join("log").exists() {
            bail!("project '{name}' already exists");
        }
        if !crate::btrfs::is_btrfs(&store.projects_dir()) {
            bail!("{} is not on btrfs", store.projects_dir().display());
        }
        mkdir_mode(&dir, 0o755)?;
        mkdir_mode(&dir.join("states"), 0o700)?;
        mkdir_mode(&dir.join("lines"), 0o711)?;
        mkdir_mode(&dir.join("meta"), 0o700)?;
        for d in ["log", "stage", "scan", "work"] {
            mkdir_mode(&dir.join("meta").join(d), 0o700)?;
        }
        let db = Db::open(&dir.join("meta").join("index.sqlite"))?;
        Ok(Project { store, name: name.to_string(), dir, id: String::new(), db })
    }

    /// Open a project that may not have its records yet.
    pub fn open_partial(store: &'a Store, name: &str) -> Result<Project<'a>> {
        let dir = store.projects_dir().join(name);
        let db = Db::open(&dir.join("meta").join("index.sqlite"))?;
        let mut p = Project { store, name: name.to_string(), dir, id: String::new(), db };
        p.catch_up()?;
        p.id = p.project_id().unwrap_or_default();
        Ok(p)
    }

    pub fn open(store: &'a Store, name: &str) -> Result<Project<'a>> {
        check_name("project", name)?;
        let dir = store.projects_dir().join(name);
        if !dir.join("meta").join("log").is_dir() {
            bail!("project '{name}' not found");
        }
        let db = Db::open(&dir.join("meta").join("index.sqlite"))?;
        let mut p = Project { store, name: name.to_string(), dir, id: String::new(), db };
        p.catch_up()?;
        p.id = p.project_id()?;
        Ok(p)
    }

    fn project_id(&self) -> Result<String> {
        let id: Option<String> = self
            .db
            .conn
            .query_row("SELECT json_extract(raw, '$.r.data.id') FROM records WHERE op='project' ORDER BY time LIMIT 1", [], |r| {
                r.get(0)
            })
            .ok();
        id.ok_or_else(|| anyhow!("project record missing"))
    }

    pub fn lock(&self) -> Result<Lock> {
        let f = fs::OpenOptions::new().create(true).write(true).truncate(false).open(self.meta_dir().join("lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Lock { _f: f })
    }

    fn log_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut v = Vec::new();
        for e in fs::read_dir(self.log_dir())?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(m) = n.strip_suffix(".jsonl") {
                v.push((m.to_string(), e.path()));
            }
        }
        Ok(v)
    }

    /// Apply log records that the index does not have yet.
    pub fn catch_up(&self) -> Result<()> {
        let mut pending: Vec<(oplog::Signed, String, u64)> = Vec::new();
        for (m, path) in self.log_files()? {
            let (_, off) = self.db.applied(&m)?;
            let (recs, end) = oplog::read_from(&path, off)?;
            if recs.is_empty() {
                continue;
            }
            let n = recs.len();
            for (i, r) in recs.into_iter().enumerate() {
                let e = if i + 1 == n { end } else { 0 };
                pending.push((r, m.clone(), e));
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        pending.sort_by(|a, b| {
            (a.0.record.time, &a.0.record.machine, a.0.record.seq).cmp(&(b.0.record.time, &b.0.record.machine, b.0.record.seq))
        });
        let mut last: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
        for (s, m, end) in &pending {
            self.apply(&s.record, &s.line)?;
            let e = last.entry(m.clone()).or_insert((0, 0));
            e.0 = e.0.max(s.record.seq);
            if *end > 0 {
                e.1 = *end;
            }
        }
        for (m, (seq, off)) in last {
            if off > 0 {
                self.db.set_applied(&m, seq, off)?;
            }
        }
        Ok(())
    }

    fn apply(&self, rec: &Record, line: &str) -> Result<()> {
        let sd = self.states_dir();
        self.db.apply(rec, line, &|id| sd.join(id).exists())
    }

    /// Drop the index and rebuild it from all logs.
    pub fn rebuild_index(&self) -> Result<()> {
        self.db.clear()?;
        self.catch_up()?;
        for (id, deleted) in self.db.present_states()? {
            if deleted || !self.state_path(&id).exists() {
                self.db.set_present(&id, false)?;
            }
        }
        let mut st = self.db.conn.prepare("SELECT id FROM states WHERE present=0 AND deleted=0")?;
        let missing: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        drop(st);
        for id in missing {
            if self.state_path(&id).exists() {
                self.db.set_present(&id, true)?;
            }
        }
        Ok(())
    }

    /// Append a record to this machine's log (`fsync`), then update the index.
    /// The caller holds the project lock and has created all snapshots the record names.
    pub fn append(&self, mut rec: Record, actor: &Actor) -> Result<Record> {
        self.catch_up()?;
        let own = self.own_log();
        let m = &self.store.machine;
        let (mut seq, _) = self.db.applied(&m.id)?;
        if seq == 0 && !own.exists() {
            let mut mr = Record::new("machine");
            mr.id = ids::new_id();
            mr.machine = m.id.clone();
            mr.seq = 1;
            mr.time = ids::now_ms();
            mr.project = if rec.op == "project" { rec.data["id"].as_str().unwrap_or("").to_string() } else { self.id.clone() };
            mr.actor = actor.clone();
            mr.data = serde_json::json!({"name": m.name, "public_key": hex::encode(m.key.verifying_key().to_bytes())});
            let line = oplog::sign_line(&mr, &m.key)?;
            oplog::append(&own, &line)?;
            seq = 1;
            self.apply(&mr, &line)?;
            self.db.set_applied(&m.id, 1, fs::metadata(&own)?.len())?;
        }
        rec.id = ids::new_id();
        rec.machine = m.id.clone();
        rec.seq = seq + 1;
        rec.time = ids::now_ms();
        rec.project = if rec.op == "project" { rec.data["id"].as_str().unwrap_or("").to_string() } else { self.id.clone() };
        rec.actor = actor.clone();
        let line = oplog::sign_line(&rec, &m.key)?;
        oplog::append(&own, &line)?;
        // From here the record is durable. An index failure does not undo it: the index is
        // rebuilt from the log on the next open.
        let indexed = self.apply(&rec, &line).and_then(|_| self.db.set_applied(&m.id, rec.seq, fs::metadata(&own)?.len()));
        if let Err(e) = indexed {
            return Err(anyhow::Error::new(Logged(format!(
                "the operation was recorded in the log, but the index update failed: {e:#}. The index is updated from the log on the next command."
            ))));
        }
        Ok(rec)
    }

    pub fn config_value(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.db.config(key)
    }

    pub fn config_u64(&self, key: &str, default: u64) -> u64 {
        self.db.config(key).ok().flatten().and_then(|v| v.as_u64()).unwrap_or(default)
    }

    pub fn config_bool(&self, key: &str, default: bool) -> bool {
        self.db.config(key).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(default)
    }

    pub fn config_strings(&self, key: &str) -> Vec<String> {
        self.db
            .config(key)
            .ok()
            .flatten()
            .and_then(|v| v.as_array().cloned())
            .map(|a| a.into_iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    }

    pub fn lead(&self) -> u32 {
        self.config_u64("lead", 0) as u32
    }
}
