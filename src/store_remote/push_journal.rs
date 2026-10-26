//! Durable progress for one push. Removed after successful completion.
use super::{
    format::{Entry, Link},
    zst,
};
use anyhow::Result;
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Completed {
    pub state: String,
    pub attempt: String,
    pub depth: usize,
}
#[derive(Default, Serialize)]
pub(super) struct Stats {
    pub peak_bytes: u64,
    pub commits: usize,
    pub commit_seconds: f64,
    pub state_commits: usize,
    pub state_commit_seconds: f64,
    pub seed_states: usize,
    pub seed_pending_parts: usize,
    pub seed_reader_gets: usize,
    pub seed_seconds: f64,
    pub seed_started_ms: i64,
    pub seed_finished_ms: i64,
}
pub(super) struct Pending {
    pub object: String,
    pub entries: Vec<Entry>,
    pub created: i64,
}
pub(super) struct Journal {
    db: Connection,
    path: PathBuf,
    pub stats: Stats,
    warned: bool,
}
impl Journal {
    pub fn open(dir: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join("push-journal.sqlite3");
        std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&path)?;
        let db = Connection::open(&path)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS hashes (sha TEXT PRIMARY KEY, links BLOB NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS completed (state TEXT PRIMARY KEY, attempt TEXT NOT NULL, depth INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS pending (object TEXT PRIMARY KEY, state TEXT NOT NULL, attempt TEXT NOT NULL, entries BLOB NOT NULL, created INTEGER NOT NULL) WITHOUT ROWID;")?;
        let peak_bytes = std::fs::metadata(&path)?.len();
        Ok(Self { db, path, stats: Stats { peak_bytes, ..Stats::default() }, warned: false })
    }
    pub fn hashes(&self) -> Result<BTreeMap<String, Vec<Link>>> {
        let mut result = BTreeMap::new();
        let mut query = self.db.prepare("SELECT sha, links FROM hashes")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(1)?;
            result.insert(row.get(0)?, serde_json::from_slice(&zst::decompress_with_limit(&bytes, None, 1 << 20)?)?);
        }
        Ok(result)
    }
    pub fn completed(&self) -> Result<Vec<Completed>> {
        let mut query = self.db.prepare("SELECT state, attempt, depth FROM completed")?;
        let completed = query
            .query_map([], |row| Ok(Completed { state: row.get(0)?, attempt: row.get(1)?, depth: row.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(completed)
    }
    pub fn pending(&self) -> Result<Vec<Pending>> {
        let mut result = Vec::new();
        let mut query = self.db.prepare("SELECT object, entries, created FROM pending ORDER BY object")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(1)?;
            result.push(Pending {
                object: row.get(0)?,
                entries: serde_json::from_slice(&zst::decompress(&bytes, None)?)?,
                created: row.get(2)?,
            });
        }
        Ok(result)
    }
    fn transaction(&mut self, state: bool, apply: impl FnOnce(&Transaction<'_>) -> Result<()>) -> Result<()> {
        let tx = self.db.transaction()?;
        apply(&tx)?;
        let size = std::fs::metadata(&self.path)?.len()
            + std::fs::metadata(self.path.with_file_name("push-journal.sqlite3-journal")).map_or(0, |m| m.len());
        self.stats.peak_bytes = self.stats.peak_bytes.max(size);
        if self.stats.peak_bytes > 64 << 20 && !self.warned {
            eprintln!("warning: active push journal exceeds 64 MiB; it is removed after the push completes");
            self.warned = true;
        }
        let started = Instant::now();
        tx.commit()?;
        let elapsed = started.elapsed().as_secs_f64();
        self.stats.commits += 1;
        self.stats.commit_seconds += elapsed;
        if state {
            self.stats.state_commits += 1;
            self.stats.state_commit_seconds += elapsed;
        }
        Ok(())
    }
    fn insert_hashes(tx: &Transaction<'_>, hashes: &BTreeMap<String, Vec<Link>>) -> Result<()> {
        let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO hashes VALUES (?1,?2)")?;
        for (sha, links) in hashes {
            insert.execute(params![sha, zst::compress(&serde_json::to_vec(links)?, None)?])?;
        }
        Ok(())
    }
    pub fn prepare_part(&mut self, state: &str, attempt: &str, part: u32, entries: &[Entry]) -> Result<()> {
        let object = super::format::data_name(state, attempt, part);
        let bytes = zst::compress(&serde_json::to_vec(entries)?, None)?;
        self.transaction(false, |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO pending VALUES (?1,?2,?3,?4,?5)",
                params![object, state, attempt, bytes, crate::ids::now_ms()],
            )?;
            Ok(())
        })
    }
    pub fn resolve_part(&mut self, object: &str, hashes: &BTreeMap<String, Vec<Link>>) -> Result<()> {
        self.transaction(false, |tx| {
            Self::insert_hashes(tx, hashes)?;
            tx.execute("DELETE FROM pending WHERE object=?1", [object])?;
            Ok(())
        })
    }
    pub fn checkpoint(&mut self, state: &str, attempt: &str, depth: usize, hashes: &BTreeMap<String, Vec<Link>>) -> Result<()> {
        self.transaction(true, |tx| {
            Self::insert_hashes(tx, hashes)?;
            tx.execute("INSERT OR REPLACE INTO completed VALUES (?1,?2,?3)", params![state, attempt, depth])?;
            tx.execute("DELETE FROM pending WHERE state=?1 AND attempt=?2", params![state, attempt])?;
            Ok(())
        })
    }
    pub fn finish(self) -> Result<Stats> {
        let Self { db, path, stats, .. } = self;
        drop(db);
        std::fs::remove_file(&path)?;
        std::fs::File::open(path.parent().unwrap())?.sync_all()?;
        Ok(stats)
    }
}
