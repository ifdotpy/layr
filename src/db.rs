//! The project database (SQLite). The `records` table holds the signed, immutable operation
//! records: the source of truth. All other tables are derived from the records. A record and
//! its derived rows are written in one transaction, so they cannot disagree after a crash. The
//! derived tables can be rebuilt from the records at any time.

use crate::ids;
use crate::model::{Attestation, Line, QueueChange, QueueEntry, Record, RefChange, RefState, StateKind, StateRec};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Cache only the schema query definition; every open still reads its database.
static SCHEMA_CHECK: LazyLock<(Vec<&'static str>, String)> = LazyLock::new(|| {
    let wanted: Vec<&str> =
        SCHEMA.lines().filter_map(|l| l.split(" IF NOT EXISTS ").nth(1)).filter_map(|l| l.split([' ', '(']).next()).collect();
    let sql = format!("SELECT COUNT(*) FROM sqlite_master WHERE name IN ({})", vec!["?"; wanted.len()].join(","));
    (wanted, sql)
});

/// Test hook: the next `with_tx` reports an error after its COMMIT succeeded (sqlite can report
/// an error after the commit point, on a full disk).
#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_AFTER_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enough room for the distinct database and ownership statements used in a replay.
pub(crate) const STATEMENT_CACHE_CAPACITY: usize = 256;

pub struct Db {
    pub conn: Connection,
    /// The derived tables are from an older schema: `rebuild` replaces them before use.
    pub stale: bool,
}

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS records (id TEXT PRIMARY KEY, machine TEXT, seq INTEGER, time INTEGER, op TEXT, actor_uid INTEGER, raw TEXT);
CREATE INDEX IF NOT EXISTS records_time ON records(time);
CREATE INDEX IF NOT EXISTS records_time_seq ON records(time, seq);
CREATE UNIQUE INDEX IF NOT EXISTS records_seq ON records(machine, seq);
CREATE INDEX IF NOT EXISTS records_git_import ON records(json_extract(raw, '$.r.data.state')) WHERE op='import';
CREATE INDEX IF NOT EXISTS records_git_export ON records(json_extract(raw, '$.r.data.state')) WHERE op='export';
-- kind 0: global push/sync; kind 1: exact line association. IDs link records; record_order preserves insertion ties.
CREATE TABLE IF NOT EXISTS record_lines (kind INTEGER NOT NULL, line_id TEXT NOT NULL, time INTEGER NOT NULL, seq INTEGER NOT NULL, record_order INTEGER NOT NULL, record_id TEXT NOT NULL, PRIMARY KEY(kind, line_id, time, seq, record_order)) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS records_index_lines AFTER INSERT ON records BEGIN
  INSERT OR IGNORE INTO record_lines SELECT 0, '', NEW.time, NEW.seq, NEW.rowid, NEW.id WHERE NEW.op IN ('push','sync');
  INSERT OR IGNORE INTO record_lines SELECT 1, key, NEW.time, NEW.seq, NEW.rowid, NEW.id FROM json_each(NEW.raw, '$.r.lines') WHERE NEW.op NOT IN ('push','sync');
  INSERT OR IGNORE INTO record_lines SELECT 1, value, NEW.time, NEW.seq, NEW.rowid, NEW.id FROM (SELECT value, type FROM json_each(NEW.raw, '$.r.data') WHERE key='line' ORDER BY id DESC LIMIT 1) WHERE NEW.op NOT IN ('push','sync') AND type='text';
END;
CREATE TABLE IF NOT EXISTS states (id TEXT PRIMARY KEY, display TEXT UNIQUE, kind TEXT, line TEXT, time INTEGER, rec TEXT, present INTEGER DEFAULT 1, deleted INTEGER DEFAULT 0, dropped INTEGER DEFAULT 0, machine TEXT);
CREATE INDEX IF NOT EXISTS states_line ON states(line, kind, time);
CREATE TABLE IF NOT EXISTS parents (state TEXT, parent TEXT, ord INTEGER, PRIMARY KEY(state, ord));
CREATE INDEX IF NOT EXISTS parents_parent ON parents(parent);
CREATE TABLE IF NOT EXISTS lines (id TEXT PRIMARY KEY, name TEXT, owner INTEGER, machine TEXT, head TEXT, deleted INTEGER DEFAULT 0, time INTEGER, ord TEXT);
CREATE TABLE IF NOT EXISTS line_heads (line TEXT, machine TEXT, head TEXT, time INTEGER, ord TEXT, PRIMARY KEY(line, machine));
CREATE TABLE IF NOT EXISTS tags (name TEXT PRIMARY KEY, state TEXT, message TEXT, time INTEGER, deleted INTEGER DEFAULT 0, ord TEXT);
CREATE TABLE IF NOT EXISTS remote_refs (name TEXT PRIMARY KEY, state TEXT, gcommit TEXT, time INTEGER, ord TEXT);
CREATE TABLE IF NOT EXISTS git_map (state TEXT, gcommit TEXT, PRIMARY KEY(state, gcommit));
CREATE INDEX IF NOT EXISTS git_map_commit ON git_map(gcommit);
CREATE TABLE IF NOT EXISTS exports (id TEXT PRIMARY KEY, state TEXT, remote TEXT, branch TEXT, gcommit TEXT, status TEXT, time INTEGER, ord TEXT);
CREATE TABLE IF NOT EXISTS machines (id TEXT PRIMARY KEY, name TEXT, public_key TEXT);
CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT, ord TEXT);
CREATE TABLE IF NOT EXISTS trusted (machine TEXT PRIMARY KEY, public_key TEXT, by TEXT);
CREATE TABLE IF NOT EXISTS approvals (id TEXT PRIMARY KEY, state TEXT, uid INTEGER, time INTEGER, withdrawn INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS refs (name TEXT PRIMARY KEY, target TEXT, expires_at INTEGER, deleted INTEGER DEFAULT 0, ord TEXT);
CREATE TABLE IF NOT EXISTS ref_history (id TEXT PRIMARY KEY, name TEXT, rec TEXT, ord TEXT);
CREATE INDEX IF NOT EXISTS ref_history_name ON ref_history(name, ord);
CREATE TABLE IF NOT EXISTS attestations (id TEXT PRIMARY KEY, subject TEXT, kind TEXT, rec TEXT, machine TEXT, uid INTEGER, local INTEGER DEFAULT 0, ord TEXT);
CREATE INDEX IF NOT EXISTS attestations_subject ON attestations(subject, kind, ord);
CREATE TABLE IF NOT EXISTS queue_entries (id TEXT PRIMARY KEY, rec TEXT, deleted INTEGER DEFAULT 0, ord TEXT);
CREATE TABLE IF NOT EXISTS attestation_revocations (id TEXT, target TEXT, machine TEXT, uid INTEGER, ord TEXT, PRIMARY KEY(id,target));
CREATE INDEX IF NOT EXISTS records_trust ON records(op) WHERE op IN ('project','machine.trust','machine.untrust','trust.delegate');
CREATE TABLE IF NOT EXISTS owners (state TEXT, kind TEXT, holder TEXT, PRIMARY KEY(state, kind, holder));
CREATE INDEX IF NOT EXISTS owners_kind_holder ON owners(kind, holder);
CREATE TABLE IF NOT EXISTS free_queue (state TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS keeps (line TEXT NOT NULL, name TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY(line, name));
CREATE TABLE IF NOT EXISTS bases (state TEXT PRIMARY KEY, base TEXT);
"#;

/// Version of the derived tables. Records never change shape; a database with an older version
/// drops its derived tables, creates them new and rebuilds them from the records.
pub const SCHEMA_VERSION: i64 = 10;

const DERIVED: &[&str] = &[
    "record_lines",
    "states",
    "parents",
    "lines",
    "line_heads",
    "tags",
    "remote_refs",
    "git_map",
    "exports",
    "machines",
    "config",
    "trusted",
    "approvals",
    "refs",
    "ref_history",
    "attestations",
    "attestation_revocations",
    "queue_entries",
    "owners",
    "bases",
];

/// Derived tables of older versions, dropped on migration.
const RETIRED: &[&str] = &["slots", "mirrors", "groups", "group_lines", "checks"];

/// Indexes of older versions on the records table, dropped on migration.
const RETIRED_INDEXES: &[&str] = &["records_legacy_evidence"];

/// Runs `step` again while SQLite reports a lock it gives up on at once: SQLITE_PROTOCOL (the WAL index lock,
/// under heavy load) and SQLITE_BUSY where no busy handler applies. Both are temporary; at most about 30 s.
fn retry_locked<T>(mut step: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let mut wait = std::time::Duration::from_millis(5);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match step() {
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::FileLockingProtocolFailed | rusqlite::ErrorCode::DatabaseBusy)
                ) && std::time::Instant::now() < until =>
            {
                std::thread::sleep(wait);
                wait = (wait * 2).min(std::time::Duration::from_millis(500));
            }
            other => return other,
        }
    }
}

impl Db {
    /// A disposable disk-backed database for bounded working sets. It has no durable data.
    pub fn scratch() -> Result<Connection> {
        let conn = Connection::open("")?;
        conn.execute_batch("PRAGMA cache_size=-2048; PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;")?;
        Ok(conn)
    }
    /// Open the database. Returns it and whether the derived tables must be rebuilt.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        // Before the pragmas: switching to WAL takes a lock too.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        retry_locked(|| conn.pragma_update(None, "journal_mode", "WAL"))?;
        // FULL: a committed record survives a power loss. The database has its own subvolume,
        // so this fsync costs a few milliseconds even right after a snapshot.
        retry_locked(|| conn.pragma_update(None, "synchronous", "FULL"))?;
        // A current schema needs no write lock: one read transaction decides, so read commands
        // do not wait for a writer.
        // An old schema with records is not touched here: its derived tables stay as they are,
        // complete, for other processes to read until `rebuild` commits the new ones.
        // One write transaction otherwise: another process (the service's periodic pass) may open
        // a project that `init` is creating. Without it, the other process saw the new tables
        // before the version and took the database for an old one: it dropped the derived
        // tables, and `init` failed with "no such table: states".
        let stale = if Self::schema_is_current(&conn)? {
            false
        } else if Self::is_old_with_records(&conn)? {
            true
        } else {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let opened = Self::prepare(&conn);
            match &opened {
                Ok(_) => conn.execute_batch("COMMIT")?,
                Err(_) => {
                    let _ = conn.execute_batch("ROLLBACK");
                }
            }
            opened.map_err(|e| e.context(path.display().to_string()))?
        };
        Ok(Db { conn, stale })
    }

    /// True when the version is current and every table and index of the schema exists.
    fn schema_is_current(conn: &Connection) -> Result<bool> {
        conn.execute_batch("BEGIN")?;
        let checked = (|| -> Result<bool> {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version != SCHEMA_VERSION {
                return Ok(false);
            }
            let (wanted, sql) = &*SCHEMA_CHECK;
            let have: i64 = conn.query_row(sql, rusqlite::params_from_iter(wanted.iter()), |r| r.get(0))?;
            Ok(have as usize == wanted.len())
        })();
        let _ = conn.execute_batch("COMMIT");
        checked
    }

    /// True when the schema version is older and the database holds records to rebuild from.
    /// A database that a concurrent `init` is creating has the tables but no record yet.
    fn is_old_with_records(conn: &Connection) -> Result<bool> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(version < SCHEMA_VERSION && Self::holds_records(conn)?)
    }

    fn holds_records(conn: &Connection) -> Result<bool> {
        let has_table: bool =
            conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='records'", [], |r| {
                r.get::<_, i64>(0)
            })? > 0;
        Ok(has_table && conn.query_row("SELECT EXISTS(SELECT 1 FROM records)", [], |r| r.get::<_, bool>(0))?)
    }

    /// Create or upgrade the schema inside the open transaction. Returns true when the database
    /// is old and holds records: the derived tables are left as they are for `rebuild`.
    fn prepare(conn: &Connection) -> Result<bool> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < SCHEMA_VERSION {
            if Self::holds_records(conn)? {
                return Ok(true);
            }
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version > SCHEMA_VERSION {
            anyhow::bail!("written by a newer layr (schema {version})");
        } else {
            conn.execute_batch(SCHEMA)?;
        }
        Ok(false)
    }

    /// Rebuild the derived tables in one write transaction: drop them, create them, `replay` the
    /// records, write the schema version, COMMIT. Other processes read the old tables until the
    /// COMMIT, and a failure or a crash rolls the whole rebuild back. With `only_older`, nothing
    /// is done when the database is current already (another process rebuilt it); returns false.
    pub fn rebuild(&self, only_older: bool, replay: impl FnOnce(&Db) -> Result<()>) -> Result<bool> {
        self.begin_rebuild()?;
        let done = (|| -> Result<bool> {
            let version: i64 = self.conn.prepare_cached("PRAGMA user_version")?.query_row([], |r| r.get(0))?;
            if only_older && version >= SCHEMA_VERSION {
                if version > SCHEMA_VERSION {
                    anyhow::bail!("written by a newer layr (schema {version})");
                }
                return Ok(false);
            }
            // Pins and removal reservations belong to running processes, not signed records.
            // Preserve them in this transaction so a replay cannot expose a reader's tree to GC.
            let has_owners: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='owners')",
                [],
                |row| row.get(0),
            )?;
            let mut local_owners = Vec::new();
            if has_owners {
                let mut query = self.conn.prepare_cached("SELECT state, kind, holder FROM owners WHERE kind=?1")?;
                for kind in crate::owners::LOCAL_OWNERS {
                    local_owners.extend(
                        query
                            .query_map([kind], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
            }
            self.conn.execute_batch("DROP TRIGGER IF EXISTS records_index_lines")?;
            for t in DERIVED.iter().chain(RETIRED) {
                self.conn.execute_batch(&format!("DROP TABLE IF EXISTS {t}"))?;
            }
            for i in RETIRED_INDEXES {
                self.conn.execute_batch(&format!("DROP INDEX IF EXISTS {i}"))?;
            }
            self.conn.execute_batch(SCHEMA)?;
            for (state, kind, holder) in local_owners {
                self.conn
                    .execute("INSERT OR IGNORE INTO owners(state,kind,holder) VALUES(?1,?2,?3)", params![state, kind, holder])?;
            }
            self.conn.execute_batch(r#"
                INSERT OR IGNORE INTO record_lines SELECT 0, '', time, seq, rowid, id FROM records WHERE op IN ('push','sync');
                INSERT OR IGNORE INTO record_lines SELECT 1, j.key, r.time, r.seq, r.rowid, r.id FROM records r, json_each(r.raw, '$.r.lines') j WHERE r.op NOT IN ('push','sync');
                INSERT OR IGNORE INTO record_lines SELECT 1, (SELECT value FROM json_each(records.raw, '$.r.data') WHERE key='line' ORDER BY id DESC LIMIT 1), time, seq, rowid, id FROM records WHERE op NOT IN ('push','sync') AND (SELECT type FROM json_each(records.raw, '$.r.data') WHERE key='line' ORDER BY id DESC LIMIT 1)='text';
            "#)?;
            // The replay fills the owners and frees nothing (the upgrade rule, docs/ownership.md).
            // Keeps come from no record: their table outlives the rebuild, and their owners return after the replay.
            crate::owners::fill_only(|| -> Result<()> {
                replay(self)?;
                self.restore_keeps(self)
            })?;
            // Test only (tests/t95-schema-rebuild-readers.sh): hold the transaction open.
            if let Some(ms) = std::env::var("LAYR_TEST_REBUILD_HOLD_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                eprintln!("test: rebuild held for {ms} ms");
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
            self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            Ok(true)
        })();
        let result = done.and_then(|rebuilt| {
            self.conn.execute_batch("COMMIT")?;
            Ok(rebuilt)
        });
        if result.as_ref().is_ok_and(|rebuilt| *rebuilt) {
            self.remove_upgrade_copies();
        }
        if result.is_err() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        result
    }

    /// Older layr versions copied the database before a schema upgrade (`<name>.schema-<version>.bak`
    /// next to it). The records of such a copy are all in the database, so the copies go.
    fn remove_upgrade_copies(&self) {
        let Some(db_path) = self.conn.path().map(PathBuf::from) else {
            return;
        };
        let (Some(dir), Some(name)) = (db_path.parent(), db_path.file_name()) else {
            return;
        };
        let prefix = format!("{}.schema-", name.to_string_lossy());
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            if file.starts_with(&prefix) && (file.ends_with(".bak") || file.ends_with(".tmp")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Start the write transaction of a rebuild. Another process may hold the write lock for
    /// minutes under load: wait and retry, and print one line after a few seconds.
    fn begin_rebuild(&self) -> Result<()> {
        self.conn.busy_timeout(std::time::Duration::from_secs(3))?;
        let mut told = false;
        let begun = loop {
            match self.conn.execute_batch("BEGIN IMMEDIATE") {
                Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) => {
                    if !told {
                        eprintln!("waiting for the database rebuild");
                        told = true;
                    }
                }
                other => break other,
            }
        };
        self.conn.busy_timeout(std::time::Duration::from_secs(30))?;
        begun.map_err(Into::into)
    }

    /// Run `f` in one write transaction (`BEGIN IMMEDIATE`): all or nothing.
    pub fn with_tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        retry_locked(|| self.conn.execute_batch("BEGIN IMMEDIATE"))?;
        match f() {
            Ok(v) => {
                // A failed COMMIT (a full disk) can leave the transaction open: roll it back so the
                // next write can begin. After a COMMIT that did happen, ROLLBACK does nothing.
                if let Err(e) = self.conn.execute_batch("COMMIT") {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
                #[cfg(test)]
                if FAIL_AFTER_COMMIT.with(|f| f.replace(false)) {
                    anyhow::bail!("injected: COMMIT reported an error after the commit point");
                }
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Insert a stored record line. Returns false if the record exists already.
    pub fn insert_record(&self, rec: &Record, line: &str) -> Result<bool> {
        let n = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO records(id, machine, seq, time, op, actor_uid, raw) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            )?
            .execute(params![rec.id, rec.machine, rec.seq as i64, rec.time, rec.op, rec.actor.uid as i64, line])?;
        Ok(n > 0)
    }

    pub fn last_seq(&self, machine: &str) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .prepare_cached("SELECT MAX(seq) FROM records WHERE machine=?1")?
            .query_row(params![machine], |r| r.get(0))?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// Stored lines of one machine after a sequence number, in order.
    pub fn lines_after(&self, machine: &str, seq: u64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT raw FROM records WHERE machine=?1 AND seq>?2 ORDER BY seq")?;
        let v = st.query_map(params![machine, seq as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn record_line(&self, machine: &str, seq: u64) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT raw FROM records WHERE machine=?1 AND seq=?2")?
            .query_row(params![machine, seq as i64], |r| r.get(0))
            .optional()?)
    }

    pub fn record_machines(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT DISTINCT machine FROM records ORDER BY machine")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The number of records and the newest row. Records are only added, so an equal mark means
    /// no record came in: everything derived from the records is unchanged.
    pub fn records_mark(&self) -> Result<(i64, i64)> {
        Ok(self
            .conn
            .prepare_cached("SELECT count(*), coalesce(max(rowid), 0) FROM records")?
            .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    /// All stored lines in apply order.
    pub fn all_lines(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT raw FROM records ORDER BY time, machine, seq")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Update the derived tables for one record. Runs inside the caller's transaction.
    ///
    /// The result does not depend on the order in which records arrive: every row that a
    /// later record can replace keeps the order key of the record that wrote it (time,
    /// machine, sequence number), and only a record with a larger key replaces it. Deletes
    /// leave a marked row for the same reason. Settings that act on this machine (admin,
    /// members, merge commands, remotes) and the lines that have their
    /// working folder here change only by this machine's records; the values written by other
    /// machines are kept under `<key>@<machine>`.
    pub fn derive(&self, rec: &Record, state_present: &dyn Fn(&str) -> bool, local: &str) -> Result<()> {
        self.derive_rows(rec, state_present, local)?;
        Self::publish_effects(rec);
        Ok(())
    }

    /// Release command cleanup notifications only after the writer has committed these rows.
    pub(crate) fn publish_effects(rec: &Record) {
        for (id, ch) in &rec.lines {
            if let Some(a) = &ch.after {
                crate::free::head_moved(&a.head);
            }
            match (&ch.before, &ch.after) {
                (Some(b), Some(a)) if b.head != a.head => crate::free::head_left(&b.head),
                (Some(b), None) => {
                    crate::free::head_left(&b.head);
                    crate::free::line_deleted(id);
                }
                _ => {}
            }
        }
    }

    /// Derive database rows without releasing effects from a transaction that may roll back.
    pub(crate) fn derive_rows(&self, rec: &Record, state_present: &dyn Fn(&str) -> bool, local: &str) -> Result<()> {
        let tx = &self.conn;
        let ord = crate::owners::record_ord(rec);
        let foreign = rec.machine != local;
        for s in &rec.states {
            // A state id names one state forever: a second record with the same id is refused
            // (the import checks this before; here it is a guard).
            let exists: bool =
                tx.prepare_cached("SELECT COUNT(*) FROM states WHERE id=?1")?.query_row(params![s.id], |r| r.get::<_, i64>(0))?
                    > 0;
            if exists {
                anyhow::bail!("state {} is named by two records", s.id);
            }
            let present = state_present(&s.id);
            tx.prepare_cached(
                "INSERT INTO states(id, display, kind, line, time, rec, present, machine) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            )?
            .execute(params![
                s.id,
                ids::display(&s.id),
                kind_str(s.kind),
                s.line,
                s.time,
                serde_json::to_string(s)?,
                present as i64,
                rec.machine
            ])?;
            // A migration record can arrive first, or sort before its source due to clock skew.
            let mappings: Vec<String> = tx
                .prepare_cached(
                    "SELECT raw FROM records WHERE op='state.tree' AND json_type(raw, '$.r.data.states.' || ?1)='object'",
                )?
                .query_map(params![s.id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for raw in mappings {
                let envelope: serde_json::Value = serde_json::from_str(&raw)?;
                self.apply_state_trees(&serde_json::from_value(envelope["r"].clone())?)?;
            }
            // A `state.parts` record of `layr overlay migrate-parts` can arrive first too.
            let parts: Vec<String> = tx
                .prepare_cached(
                    "SELECT raw FROM records WHERE op='state.parts' AND json_type(raw, '$.r.data.states.' || ?1)='object'",
                )?
                .query_map(params![s.id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for raw in parts {
                let envelope: serde_json::Value = serde_json::from_str(&raw)?;
                self.apply_state_parts(&serde_json::from_value::<Record>(envelope["r"].clone())?.data)?;
            }
            for p in &s.parents {
                if self.state(p)?.map(|p| p.kind.auxiliary()).unwrap_or(false) {
                    anyhow::bail!("source history cannot have a cache or derived parent");
                }
            }
            for (i, p) in s.parents.iter().enumerate() {
                tx.prepare_cached("INSERT OR IGNORE INTO parents(state, parent, ord) VALUES(?1,?2,?3)")?
                    .execute(params![s.id, p, i as i64])?;
            }
            // Owners may name the state before its record arrives: it holds its parents now.
            self.owners_state_made(&s.id)?;
        }
        for (id, ch) in &rec.lines {
            // Heads per machine: each machine's own records, in its order.
            if let Some(a) = &ch.after {
                if self.state(&a.head)?.map(|s| s.kind.auxiliary()).unwrap_or(false) {
                    anyhow::bail!("line head cannot be a cache or derived state");
                }
                tx.prepare_cached(
                    "INSERT INTO line_heads(line, machine, head, time, ord) VALUES(?1,?2,?3,?4,?5)
                     ON CONFLICT(line, machine) DO UPDATE SET head=?3, time=?4, ord=?5 WHERE excluded.ord > line_heads.ord",
                )?
                .execute(params![id, rec.machine, a.head, rec.time, ord])?;
            }
            let row_machine: Option<String> =
                tx.prepare_cached("SELECT machine FROM lines WHERE id=?1")?.query_row(params![id], |r| r.get(0)).optional()?;
            if foreign && row_machine.as_deref() == Some(local) {
                continue;
            }
            match &ch.after {
                Some(a) => {
                    tx.prepare_cached(
                        "INSERT INTO lines(id, name, owner, machine, head, deleted, time, ord) VALUES(?1,?2,?3,?4,?5,0,?6,?7)
                         ON CONFLICT(id) DO UPDATE SET name=?2, owner=?3, machine=?4, head=?5, deleted=0, time=?6, ord=?7
                         WHERE excluded.ord > lines.ord OR (lines.machine <> ?4 AND ?4 = ?8)",
                    )?
                    .execute(params![
                        id,
                        a.name,
                        a.owner as i64,
                        a.machine,
                        a.head,
                        rec.time,
                        ord,
                        local
                    ])?;
                }
                None => {
                    tx.prepare_cached(
                        "INSERT INTO lines(id, name, owner, machine, head, deleted, time, ord) VALUES(?1,'',0,?3,'',1,?2,?4)
                         ON CONFLICT(id) DO UPDATE SET deleted=1, time=?2, ord=?4 WHERE excluded.ord > lines.ord",
                    )?
                    .execute(params![id, rec.time, rec.machine, ord])?;
                }
            }
        }
        if let Some(value) = rec.data.get("queue_change") {
            let change: QueueChange = serde_json::from_value(value.clone())?;
            tx.prepare_cached(
                "INSERT INTO queue_entries(id,rec,deleted,ord) VALUES(?1,?2,?3,?4)
                ON CONFLICT(id) DO UPDATE SET rec=?2,deleted=?3,ord=?4 WHERE excluded.ord>queue_entries.ord",
            )?
            .execute(params![
                change.id,
                change.after.as_ref().map(serde_json::to_string).transpose()?,
                change.after.is_none() as i64,
                ord
            ])?;
        }
        self.derive_attestation(rec, !foreign)?;
        if !foreign {
            if let Some(a) = rec.data.get("attestation") {
                if a["kind"] == "revoke" {
                    for id in rec.data["revokes"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                        tx.prepare_cached("UPDATE approvals SET withdrawn=1 WHERE id=?1 AND (uid=?2 OR ?2=0)")?
                            .execute(params![id, rec.actor.uid as i64])?;
                    }
                }
                if a["kind"] == "approval"
                    && rec.data.get("declassify").is_none()
                    && !a["signer"].as_str().is_some_and(|s| s.starts_with("person:"))
                {
                    tx.prepare_cached("INSERT OR IGNORE INTO approvals(id,state,uid,time) VALUES(?1,?2,?3,?4)")?
                        .execute(params![rec.id, a["subject"]["id"].as_str(), rec.actor.uid as i64, rec.time])?;
                }
            }
        }
        if let Some(v) = rec.data.get("ref_change") {
            let ch: RefChange = serde_json::from_value(v.clone())?;
            tx.prepare_cached("INSERT OR IGNORE INTO ref_history(id,name,rec,ord) VALUES(?1,?2,?3,?4)")?.execute(params![
                rec.id,
                ch.name,
                serde_json::to_string(&ch)?,
                ord
            ])?;
            tx.prepare_cached(
                "INSERT INTO refs(name,target,expires_at,deleted,ord) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(name) DO UPDATE SET target=?2,expires_at=?3,deleted=?4,ord=?5 WHERE excluded.ord > refs.ord",
            )?
            .execute(params![
                ch.name,
                ch.after.as_ref().map(|v| &v.target),
                ch.after.as_ref().and_then(|v| v.expires_at),
                ch.after.is_none() as i64,
                ord
            ])?;
        }
        let d = &rec.data;
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
        match rec.op.as_str() {
            "machine" => {
                tx.prepare_cached("INSERT OR IGNORE INTO machines(id, name, public_key) VALUES(?1,?2,?3)")?.execute(params![
                    rec.machine,
                    s("name"),
                    s("public_key")
                ])?;
                if !foreign {
                    tx.prepare_cached("INSERT OR IGNORE INTO trusted(machine, public_key, by) VALUES(?1,?2,?1)")?
                        .execute(params![rec.machine, s("public_key")])?;
                }
            }
            "machine.trust" => {
                tx.prepare_cached("INSERT OR IGNORE INTO trusted(machine, public_key, by) VALUES(?1,?2,?3)")?
                    .execute(params![s("machine"), s("public_key"), rec.machine])?;
            }
            "project"
            | "config"
            | "overlay.create"
            | "overlay.readers"
            | "overlay.mode"
            | "overlay.use"
            | "overlay.show-to-project" => {
                if let Some(obj) = d.get("config").and_then(|v| v.as_object()) {
                    for (k, v) in obj {
                        let key = if foreign && local_key(k) { format!("{k}@{}", rec.machine) } else { k.clone() };
                        let val = if v.is_null() { None } else { Some(v.to_string()) };
                        tx.prepare_cached(
                            "INSERT INTO config(key, value, ord) VALUES(?1,?2,?3)
                             ON CONFLICT(key) DO UPDATE SET value=?2, ord=?3 WHERE excluded.ord > config.ord",
                        )?
                        .execute(params![key, val, ord])?;
                    }
                }
            }
            "tag" => {
                tx.prepare_cached("INSERT INTO tags(name, state, message, time, deleted, ord) VALUES(?1,?2,?3,?4,0,?5)
                     ON CONFLICT(name) DO UPDATE SET state=?2, message=?3, time=?4, deleted=0, ord=?5 WHERE excluded.ord > tags.ord")?.execute(
                    params![s("name"), s("state"), s("message"), rec.time, ord],
                )?;
            }
            "tag.delete" => {
                tx.prepare_cached(
                    "INSERT INTO tags(name, state, message, time, deleted, ord) VALUES(?1,NULL,NULL,?2,1,?3)
                     ON CONFLICT(name) DO UPDATE SET deleted=1, time=?2, ord=?3 WHERE excluded.ord > tags.ord",
                )?
                .execute(params![s("name"), rec.time, ord])?;
            }
            "remote.remove" => {
                // The refs of the removed remote become tombstones (state NULL), so an older import
                // record that arrives later cannot bring them back.
                if let Some(remote) = s("remote") {
                    tx.prepare_cached(
                        "UPDATE remote_refs SET state=NULL, gcommit=NULL, time=?2, ord=?3
                         WHERE substr(name, 1, length(?1)+1)=?1||'/' AND ord < ?3",
                    )?
                    .execute(params![remote, rec.time, ord])?;
                }
            }
            "import" | "remote.ref" => {
                if let (Some(name), Some(st)) = (s("ref"), s("state")) {
                    tx.prepare_cached("INSERT INTO remote_refs(name, state, gcommit, time, ord) VALUES(?1,?2,?3,?4,?5)
                         ON CONFLICT(name) DO UPDATE SET state=?2, gcommit=?3, time=?4, ord=?5 WHERE excluded.ord > remote_refs.ord")?.execute(
                        params![name, st, s("commit"), rec.time, ord],
                    )?;
                }
                if let (Some(st), Some(c)) = (s("state"), s("commit")) {
                    tx.prepare_cached("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)")?.execute(params![st, c])?;
                }
            }
            "export" => {
                tx.prepare_cached(
                    "INSERT INTO exports(id, state, remote, branch, gcommit, status, time, ord) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(id) DO UPDATE SET state=?2, remote=?3, branch=?4, gcommit=?5, status=?6, time=?7, ord=?8
                     WHERE excluded.ord > exports.ord",
                )?
                .execute(params![
                    s("export"),
                    s("state"),
                    s("remote"),
                    s("branch"),
                    s("commit"),
                    s("status"),
                    rec.time,
                    ord
                ])?;
                if let (Some(st), Some(c)) = (s("state"), s("commit")) {
                    tx.prepare_cached("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)")?.execute(params![st, c])?;
                }
                // The commits made for the ancestors of the exported state.
                if let Some(m) = d.get("commits").and_then(|v| v.as_object()) {
                    for (st, c) in m {
                        if let Some(c) = c.as_str() {
                            tx.prepare_cached("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)")?
                                .execute(params![st, c])?;
                        }
                    }
                }
                if s("status").as_deref() == Some("pushed") {
                    if let (Some(r), Some(b), Some(st), Some(c)) = (s("remote"), s("branch"), s("state"), s("commit")) {
                        tx.prepare_cached("INSERT INTO remote_refs(name, state, gcommit, time, ord) VALUES(?1,?2,?3,?4,?5)
                             ON CONFLICT(name) DO UPDATE SET state=?2, gcommit=?3, time=?4, ord=?5 WHERE excluded.ord > remote_refs.ord")?.execute(
                            params![format!("{r}/{b}"), st, c, rec.time, ord],
                        )?;
                    }
                }
            }
            "gc" => {
                if let Some(arr) = d.get("deleted").and_then(|v| v.as_array()) {
                    for id in arr.iter().filter_map(|v| v.as_str()) {
                        if !foreign && !state_present(id) {
                            tx.prepare_cached("UPDATE states SET deleted=1, present=0 WHERE id=?1")?.execute(params![id])?;
                        }
                    }
                }
            }
            "save.caches" | "save.layers" => {
                // Only the machine that made the state adds its caches (names and subvolume
                // UUIDs, which a receiver checks). A record adds to the caches of the state and
                // never drops one that an earlier record of the state gave it.
                if let Some(st) = s("state") {
                    let row: Option<(String, String)> = tx
                        .prepare_cached("SELECT rec, machine FROM states WHERE id=?1")?
                        .query_row(params![st], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    if let Some((raw, machine)) = row {
                        if machine == rec.machine {
                            let mut v: serde_json::Value = serde_json::from_str(&raw)?;
                            for (key, alias, field) in [("layers", "caches", "caches"), ("layer_uuids", "cache_uuids", "uuids")] {
                                let mut all =
                                    v.get(key).or_else(|| v.get(alias)).and_then(|x| x.as_object()).cloned().unwrap_or_default();
                                if let Some(new) = d
                                    .get(field)
                                    .or_else(|| if field == "caches" { d.get("layers") } else { None })
                                    .and_then(|x| x.as_object())
                                {
                                    all.extend(new.clone());
                                }
                                v.as_object_mut().unwrap().remove(alias);
                                v[key] = serde_json::Value::Object(all);
                            }
                            v.as_object_mut().unwrap().remove("cache_by");
                            v.as_object_mut().unwrap().remove("cache_shared");
                            v.as_object_mut().unwrap().remove("cache_platform");
                            // The platform of the newest save; all saves of a state are made on
                            // its own machine.
                            match d.get("platform").and_then(|x| x.as_str()) {
                                Some(p) => v["layer_platform"] = serde_json::json!(p),
                                None => {
                                    v.as_object_mut().unwrap().remove("layer_platform");
                                }
                            }
                            v["layer_by"] = serde_json::json!(rec.actor.uid);
                            v["layer_shared"] = serde_json::json!(d.get("shared").and_then(|x| x.as_bool()).unwrap_or(false));
                            tx.prepare_cached("UPDATE states SET rec=?2 WHERE id=?1")?.execute(params![st, v.to_string()])?;
                        }
                    }
                }
            }
            "stash.drop" => {
                tx.prepare_cached("UPDATE states SET dropped=1 WHERE id=?1")?.execute(params![s("state")])?;
            }
            layr_model::validation::STATE_TREE_OP => {
                // The tree and times table of states in the object store (`layr migrate`). The state
                // keeps its id and its signed save record; the row gets the two fields. The record
                // with the larger order key wins (`tree_ord` in the row), so the arrival order does
                // not matter. A record that arrives before the record of its state has no effect
                // until `layr reindex` replays both.
                for (st, tree, times) in layr_model::validation::state_trees(d)? {
                    let raw: Option<String> = tx
                        .prepare_cached("SELECT rec FROM states WHERE id=?1")?
                        .query_row(params![st], |r| r.get(0))
                        .optional()?;
                    let Some(raw) = raw else { continue };
                    let mut v: serde_json::Value = serde_json::from_str(&raw)?;
                    if v.get("tree_ord").and_then(|o| o.as_str()).is_some_and(|o| o >= ord.as_str()) {
                        continue;
                    }
                    v["tree"] = serde_json::json!(tree);
                    v["times"] = serde_json::json!(times);
                    v["tree_ord"] = serde_json::json!(ord);
                    tx.prepare_cached("UPDATE states SET rec=?2 WHERE id=?1")?.execute(params![st, v.to_string()])?;
                }
            }
            layr_model::validation::STATE_PARTS_OP => {
                // The parts of states saved before parts (docs/overlays.md, section 1a). The state
                // keeps its signed save record; the row gets the overlays it did not name.
                self.apply_state_parts(d)?;
            }
            _ => {}
        }
        // The overlays a line shows (`stack.<line id>`), from any record that changes them.
        if let Some(st) = d.get("stack") {
            if let (Some(line), Some(layers)) = (st.get("line").and_then(|v| v.as_str()), st.get("layers")) {
                let key = if foreign { format!("stack.{line}@{}", rec.machine) } else { format!("stack.{line}") };
                tx.prepare_cached(
                    "INSERT INTO config(key, value, ord) VALUES(?1,?2,?3)
                     ON CONFLICT(key) DO UPDATE SET value=?2, ord=?3 WHERE excluded.ord > config.ord",
                )?
                .execute(params![key, layers.to_string(), ord])?;
            }
        }
        // The overlays that the working folder of a line shows (`view.<line id>`, slice 5).
        if let Some(v) = d.get("folder_view") {
            if let (Some(line), Some(overlays)) = (v.get("line").and_then(|v| v.as_str()), v.get("overlays")) {
                let key = if foreign { format!("view.{line}@{}", rec.machine) } else { format!("view.{line}") };
                tx.prepare_cached(
                    "INSERT INTO config(key, value, ord) VALUES(?1,?2,?3)
                     ON CONFLICT(key) DO UPDATE SET value=?2, ord=?3 WHERE excluded.ord > config.ord",
                )?
                .execute(params![key, overlays.to_string(), ord])?;
            }
        }
        self.owners_after(rec, &ord, foreign)?;
        Ok(())
    }

    /// The subvolume UUID of a cache snapshot, from the signed save.caches record.
    pub fn cache_uuid(&self, state: &str, name: &str) -> Result<Option<String>> {
        let raw: Option<String> =
            self.conn.prepare_cached("SELECT rec FROM states WHERE id=?1")?.query_row(params![state], |r| r.get(0)).optional()?;
        Ok(raw
            .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
            .and_then(|v| v["layer_uuids"][name].as_str().or_else(|| v["cache_uuids"][name].as_str()).map(|s| s.to_string())))
    }

    pub fn trusted(&self, machine: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT public_key FROM trusted WHERE machine=?1")?
            .query_row(params![machine], |r| r.get(0))
            .optional()?)
    }

    fn apply_state_trees(&self, rec: &Record) -> Result<()> {
        let ord = crate::owners::record_ord(rec);
        for (state, tree, times) in layr_model::validation::state_trees(&rec.data)? {
            let raw: Option<String> = self
                .conn
                .prepare_cached("SELECT rec FROM states WHERE id=?1")?
                .query_row(params![state], |row| row.get(0))
                .optional()?;
            let Some(raw) = raw else { continue };
            let mut value: serde_json::Value = serde_json::from_str(&raw)?;
            if value.get("tree_ord").and_then(|o| o.as_str()).is_some_and(|o| o >= ord.as_str()) {
                continue;
            }
            value["tree"] = serde_json::json!(tree);
            value["times"] = serde_json::json!(times);
            value["tree_ord"] = serde_json::json!(ord);
            self.conn.prepare_cached("UPDATE states SET rec=?2 WHERE id=?1")?.execute(params![state, value.to_string()])?;
        }
        Ok(())
    }

    /// Add the parts of a `state.parts` record to the rows of its states. An overlay that a state
    /// names already keeps its part; a state that is not here yet gets them when it arrives.
    fn apply_state_parts(&self, data: &serde_json::Value) -> Result<()> {
        for (state, parts) in layr_model::validation::state_parts(data)? {
            let raw: Option<String> = self
                .conn
                .prepare_cached("SELECT rec FROM states WHERE id=?1")?
                .query_row(params![state], |row| row.get(0))
                .optional()?;
            let Some(raw) = raw else { continue };
            let mut value: serde_json::Value = serde_json::from_str(&raw)?;
            let mut all = value.get("parts").and_then(|x| x.as_object()).cloned().unwrap_or_default();
            let before = all.len();
            for (overlay, part) in parts {
                all.entry(overlay).or_insert(serde_json::json!(part));
            }
            if all.len() == before {
                continue;
            }
            value["parts"] = serde_json::Value::Object(all);
            self.conn.prepare_cached("UPDATE states SET rec=?2 WHERE id=?1")?.execute(params![state, value.to_string()])?;
        }
        Ok(())
    }

    pub fn state(&self, id: &str) -> Result<Option<StateRec>> {
        let raw: Option<String> =
            self.conn.prepare_cached("SELECT rec FROM states WHERE id=?1")?.query_row(params![id], |r| r.get(0)).optional()?;
        Ok(match raw {
            Some(r) => Some(serde_json::from_str(&r)?),
            None => None,
        })
    }

    /// Every state id, with whether it is auxiliary (cache or derived): one query for a history walk.
    pub fn state_auxiliary(&self) -> Result<Vec<(String, bool)>> {
        let mut st = self.conn.prepare_cached("SELECT id, kind IN (?1, ?2) FROM states")?;
        let v = st
            .query_map(params![kind_str(StateKind::Cache), kind_str(StateKind::Derived)], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Every parent edge (state, parent), each state's parents in order: one query for a history walk.
    pub fn parent_edges(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT state, parent FROM parents ORDER BY state, ord")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn state_flags(&self, id: &str) -> Result<Option<(bool, bool)>> {
        Ok(self
            .conn
            .prepare_cached("SELECT present, deleted FROM states WHERE id=?1")?
            .query_row(params![id], |r| Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)? != 0)))
            .optional()?)
    }

    pub fn set_present(&self, id: &str, present: bool) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE states SET present=?2, deleted=CASE WHEN ?2=1 THEN 0 ELSE deleted END WHERE id=?1")?
            .execute(params![id, present as i64])?;
        Ok(())
    }

    pub fn states_by_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM states WHERE display LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Users of this machine whose approval of the exact subject still stands.
    pub fn approvals(&self, state: &str) -> Result<Vec<(u32, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT a.uid,MAX(json_extract(a.rec,'$.time')) FROM attestations a
            WHERE (a.local=1 OR EXISTS (SELECT 1 FROM approvals p WHERE p.id=a.id)) AND a.subject=?1 AND a.kind='approval' AND json_extract(a.rec,'$.signer') NOT LIKE 'person:%'
            AND NOT EXISTS (SELECT 1 FROM records scoped WHERE scoped.id=a.id AND json_type(scoped.raw,'$.r.data.declassify') IS NOT NULL) AND NOT EXISTS
            (SELECT 1 FROM attestation_revocations r WHERE r.target=a.id AND r.machine=a.machine AND r.uid=a.uid AND r.ord>a.ord)
            GROUP BY a.machine,a.uid ORDER BY a.uid",
        )?;
        let v = st.query_map(params![state], |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The machine that made a state.
    pub fn state_machine(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT machine FROM states WHERE id=?1")?.query_row(params![id], |r| r.get(0)).optional()?)
    }

    /// The state whose signed record names subvolume `uuid`.
    pub fn state_by_subvol(&self, uuid: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM states WHERE json_extract(rec, '$.subvol')=?1 LIMIT 1")?
            .query_row(params![uuid], |r| r.get(0))
            .optional()?)
    }

    /// The states that the project still keeps (not deleted): what an object store prune keeps.
    pub fn kept_state_ids(&self) -> Result<std::collections::BTreeSet<String>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM states WHERE deleted=0")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<std::collections::BTreeSet<_>, _>>()?;
        Ok(v)
    }

    /// The states that are here only as objects (`layr migrate` retired their native copy): not present, not deleted,
    /// and their record names a tree.
    pub fn object_only_states(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id FROM states WHERE present=0 AND deleted=0 AND json_extract(rec, '$.tree') IS NOT NULL",
        )?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn present_states(&self) -> Result<Vec<(String, bool)>> {
        let mut st = self.conn.prepare_cached("SELECT id, deleted FROM states WHERE present=1")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The present states of a cache overlay that name the project state they were saved with
    /// (`cache_of`), newest first: (state, project state).
    /// `cache_states`, also of the states that are not on this machine yet (a fetch plans with them).
    pub fn cache_states_known(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, json_extract(rec, '$.cache_of') FROM states
             WHERE deleted=0 AND json_extract(rec, '$.cache_of') IS NOT NULL ORDER BY time DESC",
        )?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn cache_states(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, json_extract(rec, '$.cache_of') FROM states
             WHERE present=1 AND deleted=0 AND json_extract(rec, '$.cache_of') IS NOT NULL ORDER BY time DESC",
        )?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The newest local states, by time, whose folders the rollback check looks at.
    #[cfg_attr(not(all(target_os = "linux", not(feature = "overlayfs"))), allow(dead_code))]
    pub fn newest_present_states(&self, n: usize) -> Result<Vec<String>> {
        let mut st =
            self.conn.prepare_cached("SELECT id FROM states WHERE present=1 AND deleted=0 ORDER BY time DESC LIMIT ?1")?;
        let v = st.query_map(params![n as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    fn line_row(r: &rusqlite::Row) -> rusqlite::Result<Line> {
        Ok(Line { id: r.get(0)?, name: r.get(1)?, owner: r.get::<_, i64>(2)? as u32, machine: r.get(3)?, head: r.get(4)? })
    }

    pub fn line_by_name(&self, name: &str) -> Result<Option<Line>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT id, name, owner, machine, head FROM lines WHERE name=?1 AND deleted=0 ORDER BY time DESC LIMIT 1",
            )?
            .query_row(params![name], Self::line_row)
            .optional()?)
    }

    /// True when a deleted line of the project had the name `name` and no live line has it.
    pub fn lines_deleted_named(&self, name: &str) -> Result<bool> {
        let n: i64 = self.conn.prepare_cached("SELECT COUNT(*) FROM lines WHERE name=?1 AND deleted=1")?.query_row(params![name], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// True when the project line `id` is deleted (its row is kept with the deleted flag).
    pub fn line_deleted(&self, id: &str) -> Result<bool> {
        let deleted: Option<i64> =
            self.conn.prepare_cached("SELECT deleted FROM lines WHERE id=?1")?.query_row(params![id], |r| r.get(0)).optional()?;
        Ok(deleted == Some(1))
    }

    pub fn line_by_id(&self, id: &str) -> Result<Option<Line>> {
        Ok(self
            .conn
            .prepare_cached("SELECT id, name, owner, machine, head FROM lines WHERE id=?1")?
            .query_row(params![id], Self::line_row)
            .optional()?)
    }

    pub fn lines(&self) -> Result<Vec<Line>> {
        let mut st =
            self.conn.prepare_cached("SELECT id, name, owner, machine, head FROM lines WHERE deleted=0 ORDER BY name")?;
        let v = st.query_map([], Self::line_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn line_heads(&self, line: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT machine, head FROM line_heads WHERE line=?1")?;
        let v = st
            .query_map(params![line], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Park entries of older layr versions (`stash` states that are not dropped) with their line,
    /// oldest first. They have the `park` owner kind, which has an index: a cheap check.
    pub fn old_park_entries(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT o.state, o.holder FROM owners o JOIN states s ON s.id=o.state
             WHERE o.kind='park' AND s.deleted=0 AND s.dropped=0 ORDER BY s.time, s.id",
        )?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// States of a kind for a line, newest first.
    pub fn line_states(&self, line: &str, kind: StateKind) -> Result<Vec<StateRec>> {
        let mut st = self.conn.prepare_cached(
            "SELECT rec FROM states WHERE line=?1 AND kind=?2 AND deleted=0 AND dropped=0 ORDER BY time DESC, id DESC",
        )?;
        let v = st.query_map(params![line, kind_str(kind)], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn tag(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT state FROM tags WHERE name=?1 AND deleted=0")?
            .query_row(params![name], |r| r.get(0))
            .optional()?)
    }

    pub fn tags(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st = self.conn.prepare_cached("SELECT name, state, message FROM tags WHERE deleted=0 ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn remote_ref(&self, name: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .prepare_cached("SELECT state, gcommit FROM remote_refs WHERE name=?1 AND state IS NOT NULL")?
            .query_row(params![name], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn remote_refs(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st =
            self.conn.prepare_cached("SELECT name, state, gcommit FROM remote_refs WHERE state IS NOT NULL ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn git_commit_of(&self, state: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT gcommit FROM git_map WHERE state=?1 LIMIT 1")?
            .query_row(params![state], |r| r.get(0))
            .optional()?)
    }

    /// One Git commit of each state that has one.
    pub fn git_map(&self) -> Result<std::collections::HashMap<String, String>> {
        let mut st = self.conn.prepare_cached("SELECT state, gcommit FROM git_map")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (state, commit) = row?;
            map.entry(state).or_insert(commit);
        }
        Ok(map)
    }

    /// Every Git commit that `git_map` holds for a state, squash commits included.
    pub fn git_commits_of(&self, state: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT gcommit FROM git_map WHERE state=?1")?;
        let rows = st.query_map(params![state], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The squash commits (`push --squash`) named by the exports of a state. Records written
    /// before the `squash` flag name their squash commit by a parent; the flag is absent there.
    pub fn squash_commits_of(&self, state: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT json_extract(raw, '$.r.data.commit') FROM records WHERE op='export' AND json_extract(raw, '$.r.data.state')=?1
             AND (json_extract(raw, '$.r.data.squash')=1 OR json_type(raw, '$.r.data.parent')='text')",
        )?;
        let rows = st.query_map(params![state], |r| r.get::<_, Option<String>>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    pub fn has_git_history(&self) -> Result<bool> {
        Ok(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM git_map)")?.query_row([], |r| r.get(0))?)
    }

    /// Exact Git objects are part of the signed import record, never a machine-local cache.
    pub fn git_import(&self, state: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self.conn.prepare_cached("SELECT json_extract(raw, '$.r.data.git') FROM records WHERE op='import' AND json_extract(raw, '$.r.data.state')=?1 AND json_type(raw, '$.r.data.git')='object' LIMIT 1")?.query_row(
            params![state], |r| r.get(0),
        ).optional()?;
        raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }

    /// True when the record of an imported state holds its Git objects (`git_import`), without
    /// parsing them.
    pub fn has_git_import(&self, state: &str) -> Result<bool> {
        Ok(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM records WHERE op='import' AND json_extract(raw, '$.r.data.state')=?1 AND json_type(raw, '$.r.data.git')='object')")?.query_row(
            params![state],
            |r| r.get(0),
        )?)
    }

    /// The Git commits of the imported states, sorted.
    pub fn import_commits(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached(
            "SELECT DISTINCT g.gcommit FROM git_map g JOIN states s ON s.id=g.state WHERE s.kind='import' ORDER BY g.gcommit",
        )?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Fill the base rows of the layers on disk. Runs inside the rebuild transaction (`rebuild`),
    /// after the table was emptied: a row for every thin layer, and none for a missing one.
    pub fn fill_bases(&self, bases: &std::collections::BTreeMap<String, String>) -> Result<()> {
        for (state, base) in bases {
            self.conn.execute("INSERT OR REPLACE INTO bases(state, base) VALUES(?1, ?2)", rusqlite::params![state, base])?;
        }
        Ok(())
    }

    /// Every row of the `bases` table: layer state to base.
    pub fn bases_rows(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut st = self.conn.prepare("SELECT state, base FROM bases")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<std::collections::BTreeMap<_, _>>>()?)
    }

    /// The base of a thin layer, from its `bases` row.
    pub fn base_of_layer(&self, state: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare("SELECT base FROM bases WHERE state=?1")?;
        let mut rows = st.query_map(rusqlite::params![state], |r| r.get::<_, String>(0))?;
        Ok(match rows.next() {
            Some(b) => Some(b?),
            None => None,
        })
    }

    /// True when a thin layer is made over `base`.
    pub fn has_layer_child(&self, base: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM bases WHERE base=?1)", rusqlite::params![base], |r| r.get(0))?)
    }

    /// Put a dead state in the free queue (its last layer child went).
    pub fn queue_free(&self, state: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO free_queue(state) VALUES(?1)", rusqlite::params![state])?;
        Ok(())
    }

    /// Remove the base row of a thin layer that was removed from disk.
    pub fn forget_base(&self, state: &str) -> Result<()> {
        self.conn.execute("DELETE FROM bases WHERE state=?1", rusqlite::params![state])?;
        Ok(())
    }

    /// The base states of the thin layers that are on disk (`bases` rows of present layers).
    pub fn present_layer_bases(&self) -> Result<Vec<String>> {
        let mut st =
            self.conn.prepare_cached("SELECT DISTINCT b.base FROM bases b JOIN states s ON s.id=b.state WHERE s.present=1")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn state_of_commit(&self, prefix: &str) -> Result<Vec<String>> {
        if prefix.len() == 40 {
            let mut st = self.conn.prepare_cached("SELECT DISTINCT state FROM git_map WHERE gcommit=?1")?;
            return Ok(st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?);
        }
        let mut st = self.conn.prepare_cached("SELECT DISTINCT state FROM git_map WHERE gcommit LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn exports(&self) -> Result<Vec<(String, String, String, String, String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT id, state, remote, branch, gcommit, status FROM exports ORDER BY time")?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Active value declassifications, across all state subjects in this project.
    pub fn declassifications(&self) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare_cached("SELECT r.raw FROM attestations a JOIN records r ON r.id=a.id
            WHERE a.kind='approval' AND json_extract(r.raw,'$.r.data.declassify') IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM attestation_revocations v WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord)")?;
        let raw = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(crate::oplog::parse_line(&r, None)?.record)).collect()
    }

    pub fn checks(&self, state: &str) -> Result<Vec<(String, i64, i64, String)>> {
        let mut st = self.conn.prepare_cached("SELECT json_extract(r.raw,'$.r.data.check.command'),
            COALESCE(json_extract(r.raw,'$.r.data.check.exit'),-1),r.time,json_extract(r.raw,'$.r.actor.name')
            FROM attestations a JOIN records r ON r.id=a.id WHERE a.subject=?1 AND a.kind='check'
            AND NOT EXISTS (SELECT 1 FROM attestation_revocations v WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord)
            ORDER BY a.ord")?;
        let v = st
            .query_map(params![state], |r| {
                Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Match command arguments, not their display text. With no requested exit, use the newest result.
    pub fn check_exit(&self, state: &str, argv: &[String], wanted_exit: Option<i64>) -> Result<Option<i64>> {
        let mut statement=self.conn.prepare_cached("SELECT json_extract(r.raw,'$.r.data.check'),
            json_extract(r.raw,'$.r.data')
            FROM attestations a JOIN records r ON r.id=a.id WHERE a.subject=?1 AND a.kind='check'
            AND NOT EXISTS (SELECT 1 FROM attestation_revocations v WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord)
            ORDER BY a.ord DESC")?;
        let rows = statement
            .query_map(params![state], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (row, data) in rows {
            // A check attestation without a check result (`layr attest check`) names no command.
            let Some(row) = row else { continue };
            // A result whose digest differs from its signed payload digest counts as no result.
            if !layr_model::validation::check_digest_ok(&serde_json::from_str(&data)?) {
                continue;
            }
            let result: serde_json::Value = serde_json::from_str(&row)?;
            let matches = serde_json::from_value::<Vec<String>>(result["argv"].clone()).is_ok_and(|recorded| recorded == argv);
            if matches {
                if let Some(code) = result["exit"].as_i64() {
                    if wanted_exit.map(|wanted| wanted == code).unwrap_or(true) {
                        return Ok(Some(code));
                    }
                }
            }
        }
        Ok(None)
    }

    fn derive_attestation(&self, rec: &Record, local: bool) -> Result<()> {
        let ord = format!("{:016}:{}:{:012}", rec.time.max(0), rec.machine, rec.seq);
        if let Some(v) = rec.data.get("attestation") {
            let a = serde_json::from_value::<Attestation>(v.clone())?;
            let signer = if a.signer.starts_with("person:") { &a.signer } else { &rec.machine };
            self.conn.prepare_cached("INSERT INTO attestations(id,subject,kind,rec,machine,uid,ord,local) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET local=MAX(attestations.local,excluded.local)")?.execute(
                params![
                    a.attestation_id,
                    a.subject.id,
                    a.kind,
                    serde_json::to_string(&a)?,
                    signer,
                    rec.actor.uid as i64,
                    ord,
                    local as i64
                ],
            )?;
            if a.kind == "revoke" {
                for id in rec.data["revokes"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                    self.conn
                        // Root of the signing machine revokes for every user of it: the row takes the
                        // target's user, so the revocation matches it (only on the same machine).
                        .prepare_cached(
                            "INSERT OR IGNORE INTO attestation_revocations(id,target,machine,uid,ord)
                         SELECT ?1,?2,?3,CASE WHEN ?4=0 AND machine=?3 THEN uid ELSE ?4 END,?5 FROM attestations WHERE id=?2 AND subject=?6",
                        )?
                        .execute(params![a.attestation_id, id, signer, rec.actor.uid as i64, ord, a.subject.id])?;
                }
            }
        }
        Ok(())
    }

    pub fn attestation(&self, id: &str) -> Result<Option<Attestation>> {
        let raw: Option<String> = self
            .conn
            .prepare_cached("SELECT rec FROM attestations WHERE id=?1")?
            .query_row(params![id], |r| r.get(0))
            .optional()?;
        raw.map(|r| Ok(serde_json::from_str(&r)?)).transpose()
    }

    pub fn attestations(&self, subject: &str, kind: Option<&str>) -> Result<Vec<Attestation>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT rec FROM attestations WHERE subject=?1 AND (?2 IS NULL OR kind=?2) ORDER BY ord,id")?;
        let raw = st.query_map(params![subject, kind], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    /// The attestations of `subject` that `machine` made for `uid` (any user when None), but revokes.
    pub fn own_attestations(&self, subject: &str, machine: &str, uid: Option<u32>, kind: Option<&str>) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM attestations WHERE subject=?1 AND machine=?2 AND (?3 IS NULL OR uid=?3) AND (?4 IS NULL OR kind=?4) AND kind<>'revoke'")?;
        let v =
            st.query_map(params![subject, machine, uid.map(i64::from), kind], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn refs(&self, prefix: &str, now: i64) -> Result<Vec<RefState>> {
        let mut st = self.conn.prepare_cached("SELECT name,target,expires_at FROM refs WHERE deleted=0 AND substr(name,1,length(?1))=?1 AND (expires_at IS NULL OR expires_at>?2) ORDER BY name")?;
        let v = st
            .query_map(params![prefix, now], |r| Ok(RefState { name: r.get(0)?, target: r.get(1)?, expires_at: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn ref_value(&self, name: &str, now: i64) -> Result<Option<RefState>> {
        Ok(self.refs(name, now)?.into_iter().find(|r| r.name == name))
    }

    pub fn expired_refs(&self, now: i64) -> Result<Vec<RefState>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT name,target,expires_at FROM refs WHERE deleted=0 AND expires_at<=?1 ORDER BY name")?;
        let v = st
            .query_map(params![now], |r| Ok(RefState { name: r.get(0)?, target: r.get(1)?, expires_at: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn ref_log(&self, name: &str) -> Result<Vec<RefChange>> {
        let mut st = self.conn.prepare_cached("SELECT rec FROM ref_history WHERE name=?1 ORDER BY ord DESC,id DESC")?;
        let raw = st.query_map(params![name], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn queue_entries(&self, target: Option<&str>) -> Result<Vec<QueueEntry>> {
        let mut st=self.conn.prepare_cached("SELECT rec FROM queue_entries WHERE deleted=0 AND (?1 IS NULL OR json_extract(rec,'$.target_line')=?1) ORDER BY json_extract(rec,'$.time'),id")?;
        let rows = st.query_map(params![target], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|raw| Ok(serde_json::from_str(&raw)?)).collect()
    }

    pub fn queue_entry(&self, id: &str) -> Result<Option<QueueEntry>> {
        let raw: Option<String> = self
            .conn
            .prepare_cached("SELECT rec FROM queue_entries WHERE id=?1 AND deleted=0")?
            .query_row(params![id], |r| r.get(0))
            .optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub fn change_versions(&self, id: &str) -> Result<Vec<StateRec>> {
        let mut st =
            self.conn.prepare_cached("SELECT rec FROM states WHERE json_extract(rec,'$.change_id')=?1 ORDER BY time,id")?;
        let raw = st.query_map(params![id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn changes(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT DISTINCT json_extract(rec,'$.change_id') FROM states WHERE json_extract(rec,'$.change_id') IS NOT NULL ORDER BY 1")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn config(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let v: Option<String> = self
            .conn
            .prepare_cached("SELECT value FROM config WHERE key=?1")?
            .query_row(params![key], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten();
        Ok(match v {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub fn config_all(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT key, value FROM config WHERE value IS NOT NULL ORDER BY key")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Overlay policy and metadata follow their signed records across machines.
    pub fn overlay_config(&self, key: &str) -> Result<Option<serde_json::Value>> {
        // `key` and `key@<machine>`. The range ('@' to 'A') reads the primary key index; a
        // `substr` test read the whole table on every call.
        let value: Option<Option<String>> = self
            .conn
            .prepare_cached("SELECT value FROM config WHERE key=?1 OR (key>=?1||'@' AND key<?1||'A') ORDER BY ord DESC LIMIT 1")?
            .query_row(params![key], |row| row.get(0))
            .optional()?;
        value.flatten().map(|value| serde_json::from_str(&value).map_err(Into::into)).transpose()
    }

    pub fn machine_key(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT public_key FROM machines WHERE id=?1")?
            .query_row(params![id], |r| r.get(0))
            .optional()?)
    }

    /// The name in a machine's first record.
    pub fn machine_name(&self, id: &str) -> Result<Option<String>> {
        Ok(self.machines()?.into_iter().find(|(m, _)| m == id).map(|(_, name)| name))
    }

    /// A machine by its short id (the display form, random part first), full id or a unique name.
    pub fn machine_by_name(&self, name: &str) -> Result<Option<String>> {
        let all = self.machines()?;
        let by_id: Vec<&(String, String)> =
            all.iter().filter(|(id, _)| id == name || (name.len() >= 4 && ids::display(id).starts_with(name))).collect();
        if by_id.len() == 1 {
            return Ok(Some(by_id[0].0.clone()));
        }
        let by_name: Vec<&(String, String)> = all.iter().filter(|(_, n)| n == name).collect();
        if by_name.len() == 1 {
            return Ok(Some(by_name[0].0.clone()));
        }
        Ok(None)
    }

    pub fn machines(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT id, COALESCE(name, '') FROM machines ORDER BY id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn record(&self, id: &str) -> Result<Option<Record>> {
        let raw: Option<String> =
            self.conn.prepare_cached("SELECT raw FROM records WHERE id=?1")?.query_row(params![id], |r| r.get(0)).optional()?;
        match raw {
            Some(l) => Ok(Some(crate::oplog::parse_line(&l, None)?.record)),
            None => Ok(None),
        }
    }

    /// The head a line was created at, from its `line.create` record.
    pub fn line_start(&self, line: &str) -> Result<Option<String>> {
        let mut st = self.conn.prepare_cached("SELECT raw FROM records WHERE op='line.create' AND instr(raw, ?1) > 0")?;
        let v = st.query_map(params![line], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        for l in v {
            let rec = crate::oplog::parse_line(&l, None)?.record;
            if let Some(after) = rec.lines.get(line).and_then(|c| c.after.as_ref()) {
                return Ok(Some(after.head.clone()));
            }
        }
        Ok(None)
    }

    /// Records newest first.
    pub fn records(&self, limit: usize) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare_cached("SELECT raw FROM records ORDER BY time DESC, seq DESC LIMIT ?1")?;
        let v = st.query_map(params![limit as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    /// Copy the newest outcome per request into a temporary database without loading the log.
    pub fn copy_request_outcomes(&self, scratch: &Connection) -> Result<()> {
        let mut select = self.conn.prepare_cached("SELECT raw FROM records ORDER BY time DESC, seq DESC")?;
        let mut rows = select.query([])?;
        let mut insert = scratch.prepare_cached("INSERT OR IGNORE INTO request_outcomes(id,digest) VALUES(?1,?2)")?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let record = crate::oplog::parse_line(&raw, None)?.record;
            if let Some(id) = record.data["request_id"].as_str() {
                insert.execute(params![id, serde_json::to_string(&record.data["request_digest"])?])?;
            }
        }
        Ok(())
    }

    /// The newest operations visible on a line, or all operations when no line is supplied.
    /// SQLite merges two ordered index ranges and stops at `limit`; only selected rows are
    /// fetched and decoded. Global (`kind=0`) records are never in the line range (`kind=1`).
    pub fn log_records(&self, line: Option<&str>, limit: usize) -> Result<Vec<Record>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = if let Some(line) = line {
            let mut st = self.conn.prepare_cached(
                "SELECT records.raw FROM (
                 SELECT time, seq, record_order, record_id FROM record_lines WHERE kind=1 AND line_id=?1
                 UNION ALL
                 SELECT time, seq, record_order, record_id FROM record_lines WHERE kind=0 AND line_id=''
                 ORDER BY time DESC, seq DESC, record_order DESC LIMIT ?2
                 ) AS selected JOIN records ON records.id=selected.record_id
                 ORDER BY selected.time DESC, selected.seq DESC, selected.record_order DESC",
            )?;
            let rows = st.query_map(params![line, limit], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            rows
        } else {
            let mut st = self.conn.prepare_cached("SELECT raw FROM records ORDER BY time DESC, seq DESC, rowid DESC LIMIT ?1")?;
            let rows = st.query_map(params![limit], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            rows
        };
        rows.into_iter().map(|raw| Ok(crate::oplog::parse_line(&raw, None)?.record)).collect()
    }

    /// The newest records whose text names `needle` (a line id), newest first, at most `limit`.
    pub fn records_naming(&self, needle: &str, limit: usize) -> Result<Vec<Record>> {
        let mut st =
            self.conn.prepare_cached("SELECT raw FROM records WHERE instr(raw, ?1) > 0 ORDER BY time DESC, seq DESC LIMIT ?2")?;
        let v = st.query_map(params![needle, limit as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    /// The records with one operation name, oldest first.
    pub fn records_with_op(&self, op: &str) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare_cached("SELECT raw FROM records WHERE op=?1 ORDER BY time, machine, seq")?;
        let v = st.query_map(params![op], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    /// The project creator and the trust records (`machine.trust`, `machine.untrust`, `trust.delegate`), for
    /// `layr_store::trust`.
    pub fn trust_history(&self) -> Result<(Option<String>, Vec<Record>)> {
        let mut st = self.conn.prepare_cached(
            "SELECT raw FROM records WHERE op IN ('project','machine.trust','machine.untrust','trust.delegate') ORDER BY time, machine, seq",
        )?;
        let raw = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut creator = None;
        let mut records = Vec::new();
        for line in raw {
            let record = crate::oplog::parse_line(&line, None)?.record;
            if record.op == "project" {
                creator.get_or_insert(record.machine);
            } else {
                records.push(record);
            }
        }
        Ok((creator, records))
    }

    /// The machine whose record made line `id`.
    pub fn line_machine(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.prepare_cached("SELECT machine FROM lines WHERE id=?1")?.query_row(params![id], |r| r.get(0)).optional()?)
    }

    /// The trusted machines and their public keys (hex).
    pub fn trusted_keys(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached("SELECT machine, public_key FROM trusted")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The records from `time` on, oldest first (`layr follow` reads its exports of the last days).
    pub fn records_since(&self, time: i64) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare_cached("SELECT raw FROM records WHERE time>=?1 ORDER BY time")?;
        let v = st.query_map(params![time], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    pub fn max_seqs(&self) -> Result<Vec<(String, u64)>> {
        let mut st = self.conn.prepare_cached("SELECT machine, MAX(seq) FROM records GROUP BY machine")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn undone(&self, id: &str) -> Result<bool> {
        let n: i64 = self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM records WHERE op='undo' AND json_extract(raw, '$.r.data.undo')=?1")?
            .query_row(params![id], |r| r.get(0))?;
        Ok(n > 0)
    }
}

/// Every setting acts on one machine (admin, members, merge commands, remotes, retention,
/// push policy, caches): settings never come from another machine's records. Those
/// values stay as `<key>@<machine>`.
pub fn local_key(_k: &str) -> bool {
    true
}

pub fn kind_str(k: StateKind) -> &'static str {
    match k {
        StateKind::Commit => "commit",
        StateKind::Auto => "auto",
        StateKind::Stash => "stash",
        StateKind::Import => "import",
        StateKind::Cache => "cache",
        StateKind::Derived => "derived",
    }
}

pub fn need<T>(v: Option<T>, what: &str) -> Result<T> {
    v.ok_or_else(|| anyhow!("{what} not found"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn rebuild_preserves_active_reader_and_removal_reservations() -> anyhow::Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(super::SCHEMA)?;
        let db = super::Db { conn, stale: false };
        for (state, kind, holder) in [("read", "pin", "reader"), ("remove", "drop", "collector")] {
            db.conn.execute("INSERT INTO owners(state,kind,holder) VALUES(?1,?2,?3)", rusqlite::params![state, kind, holder])?;
        }
        let before = db.owner_rows()?;
        assert!(db.rebuild(false, |_| anyhow::bail!("replay failed")).is_err());
        assert_eq!(db.owner_rows()?, before, "a failed replay preserves the process owners");
        db.rebuild(false, |_| Ok(()))?;
        assert!(db.is_pinned("read")?, "a rebuild must not release an active reader");
        assert!(db.reserve_trees(&["read".into()], "other-collector")?.is_empty());
        assert!(db.pins_add(&["remove"], "other-reader").is_err(), "an active removal still refuses readers");
        db.pins_remove(&["read".into()], "reader")?;
        db.release_trees(&["remove".into()], &[], "collector")?;
        assert!(!db.is_pinned("read")?);
        assert!(db.pins_add(&["remove"], "other-reader").is_ok());
        Ok(())
    }

    use super::*;
    use crate::model::Subject;
    use crate::model::{Author, StateKind, StateRec};
    use layr_hashers::{ContentAddress, Digest};

    #[test]
    fn request_outcome_copy_preserves_newest_and_json_duplicate_semantics() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn, stale: false };
        for (n, request, digest) in [(1, "wanted", Some("old")), (2, "wanted", Some("new")), (3, "missing", None)] {
            let mut record = Record::new("request.refused");
            record.id = format!("r{n}");
            record.machine = "machine".into();
            record.seq = n;
            record.time = n as i64;
            record.data = serde_json::json!({"request_id":request,"request_digest":digest});
            let mut body = serde_json::to_string(&record)?;
            if n == 2 {
                body = body.replace(r#""request_id":"wanted""#, r#""request_id":"wrong","request_id":"wanted""#);
                assert!(body.contains(r#""request_id":"wrong","request_id":"wanted""#));
            }
            let line = format!(r#"{{"r":{body},"s":""}}"#);
            let parsed = crate::oplog::parse_line(&line, None)?.record;
            db.insert_record(&parsed, &line)?;
        }
        let scratch = Db::scratch()?;
        scratch
            .execute_batch("CREATE TABLE request_outcomes(id TEXT PRIMARY KEY, digest TEXT NOT NULL) WITHOUT ROWID; BEGIN;")?;
        db.copy_request_outcomes(&scratch)?;
        let find = |id: &str| -> Result<Option<String>> {
            Ok(scratch.query_row("SELECT digest FROM request_outcomes WHERE id=?1", [id], |row| row.get(0)).optional()?)
        };
        assert_eq!(find("wanted")?.as_deref(), Some("\"new\""));
        assert_eq!(find("missing")?.as_deref(), Some("null"));
        assert_eq!(find("wrong")?, None);
        Ok(())
    }

    #[test]
    fn scratch_database_handles_more_data_than_its_cache() -> Result<()> {
        let scratch = Db::scratch()?;
        scratch.execute_batch("CREATE TABLE seen(id TEXT PRIMARY KEY) WITHOUT ROWID; BEGIN;")?;
        {
            let mut insert = scratch.prepare("INSERT INTO seen(id) VALUES(?1)")?;
            for n in 0..100_000 {
                insert.execute([format!("01a1b000-0000-7000-9000-{n:012x}")])?;
            }
        }
        let count: i64 = scratch.query_row("SELECT COUNT(*) FROM seen", [], |row| row.get(0))?;
        let pages: i64 = scratch.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let size: i64 = scratch.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let cache: i64 = scratch.query_row("PRAGMA cache_size", [], |row| row.get(0))?;
        assert_eq!(count, 100_000);
        assert_eq!(cache, -2048);
        assert!(pages * size > 2 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn schema_check_observes_changes_between_opens() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-schema-check-{}", ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("layr.sqlite");
        let db = Db::open(&path)?;
        assert!(Db::schema_is_current(&db.conn)?);
        db.conn.execute_batch("DROP INDEX states_line")?;
        assert!(!Db::schema_is_current(&db.conn)?);
        drop(db);
        let reopened = Db::open(&path)?;
        assert!(Db::schema_is_current(&reopened.conn)?);
        reopened.conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1)?;
        assert!(!Db::schema_is_current(&reopened.conn)?);
        drop(reopened);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn log_records_keep_the_observed_insertion_order_for_fully_tied_records() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn, stale: false };
        for (id, op) in [("z-first", "save"), ("a-second", "sync"), ("m-third", "save")] {
            let mut rec = Record::new(op);
            rec.id = id.into();
            rec.machine = id.into();
            rec.time = 1;
            rec.seq = 1;
            rec.data = serde_json::json!({"line": "wanted"});
            db.insert_record(&rec, &serde_json::json!({"r": rec, "s": ""}).to_string())?;
        }
        // Installed 0.36's actual records(100000) caller returns newest insertion first.
        let expected = ["m-third", "a-second", "z-first"];
        for line in [None, Some("wanted"), Some("missing")] {
            for limit in [1, 2, 10] {
                let ids: Vec<_> = db.log_records(line, limit)?.into_iter().map(|r| r.id).collect();
                let visible: Vec<_> = expected
                    .iter()
                    .filter(|id| line != Some("missing") || **id == "a-second")
                    .take(limit)
                    .map(|id| id.to_string())
                    .collect();
                assert_eq!(ids, visible, "line={line:?}, limit={limit}");
            }
        }
        Ok(())
    }

    #[test]
    fn log_records_match_full_history_output() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn, stale: false };
        for i in 0..1000 {
            let mut rec = Record::new(if i % 17 == 0 { "push" } else { "save" });
            rec.id = format!("record-{i}");
            rec.machine = "machine".into();
            rec.seq = i as u64;
            rec.time = i / 3;
            if i % 5 == 0 {
                rec.data = serde_json::json!({"line": "wanted"});
            } else if i % 7 == 0 {
                rec.lines.insert(
                    "wanted".into(),
                    crate::model::LineChange { before: None, after: None, working_before: None, working_after: None },
                );
            } else {
                rec.data = serde_json::json!({"line": "other", "note": "wanted"});
            }
            let raw = serde_json::json!({"r": rec, "s": ""}).to_string();
            db.conn.execute(
                "INSERT INTO records(id,machine,seq,time,op,raw) VALUES(?1,?2,?3,?4,?5,?6)",
                params![rec.id, rec.machine, rec.seq, rec.time, rec.op, raw],
            )?;
            db.derive(&rec, &|_| true, "machine")?;
        }
        for line in [None, Some("wanted"), Some("missing")] {
            for limit in [1, 30, 200, 2000] {
                let expected: Vec<_> = db
                    .records(100000)?
                    .into_iter()
                    .filter(|r| {
                        line.is_none_or(|l| {
                            matches!(r.op.as_str(), "push" | "sync")
                                || r.lines.contains_key(l)
                                || r.data.get("line").and_then(|v| v.as_str()) == Some(l)
                        })
                    })
                    .take(limit)
                    .collect();
                let actual = db.log_records(line, limit)?;
                assert_eq!(serde_json::to_vec(&expected)?, serde_json::to_vec(&actual)?);
            }
        }
        Ok(())
    }

    #[test]
    fn opening_main_schema_nine_requires_and_backfills_the_line_index() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-log-main-nine-{}", ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("layr.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch(SCHEMA)?;
        // Main's schema 9 has no operation-log index. It must not be treated as current.
        conn.execute_batch("DROP TRIGGER records_index_lines; DROP TABLE record_lines; DROP INDEX records_time_seq")?;
        conn.pragma_update(None, "user_version", 9)?;
        let old = Db { conn, stale: true };
        let mut rec = Record::new("save");
        rec.id = "main-nine-record".into();
        rec.machine = "machine".into();
        rec.time = 1;
        rec.seq = 1;
        rec.data = serde_json::json!({"line": "wanted"});
        old.insert_record(&rec, &serde_json::json!({"r": rec, "s": ""}).to_string())?;
        drop(old);
        let db = Db::open(&path)?;
        assert!(db.stale, "main schema 9 must trigger a rebuild instead of creating an empty index");
        assert!(db.rebuild(true, |_| Ok(()))?);
        assert_eq!(db.log_records(Some("wanted"), 1)?[0].id, rec.id);
        assert_eq!(db.conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?, 10);
        drop(db);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn rebuilding_line_index_keeps_interleaved_log_order() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-log-migration-{}", ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("layr.sqlite"))?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", 9)?;
        let db = Db { conn, stale: true };
        for i in 0..400 {
            let mut rec = Record::new(if i % 13 == 0 { "sync" } else { "save" });
            rec.id = format!("record-{i}");
            rec.machine = "machine".into();
            rec.seq = i as u64;
            rec.time = i / 4;
            rec.data = serde_json::json!({"line": if i % 2 == 0 { "a" } else { "b" }});
            let raw = serde_json::json!({"r": rec, "s": ""}).to_string();
            db.insert_record(&rec, &raw)?;
        }
        assert!(db.rebuild(true, |db| {
            for raw in db.all_lines()? {
                let rec = crate::oplog::parse_line(&raw, None)?.record;
                db.derive(&rec, &|_| true, "machine")?;
            }
            Ok(())
        })?);
        let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        assert_eq!(version, SCHEMA_VERSION);
        for line in ["a", "b"] {
            let expected: Vec<_> = db
                .records(1000)?
                .into_iter()
                .filter(|r| matches!(r.op.as_str(), "push" | "sync") || r.data["line"].as_str() == Some(line))
                .take(75)
                .collect();
            assert_eq!(serde_json::to_vec(&expected)?, serde_json::to_vec(&db.log_records(Some(line), 75)?)?);
        }
        drop(db);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn duplicate_data_line_uses_last_value_on_insert_and_rebuild() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-log-migration-{}", ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("layr.sqlite"))?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn, stale: false };
        let ordered: Vec<String> = {
            let mut st = db.conn.prepare(
                "SELECT value FROM json_each('{\"line\":\"first\",\"line\":\"last\"}') WHERE key='line' ORDER BY id DESC",
            )?;
            let rows = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
            rows
        };
        assert_eq!(ordered, ["last", "first"]);
        for (i, data) in [(1, r#"{"line":"first","line":"last"}"#), (2, r#"{"line":"first","line":null}"#)] {
            let mut rec = Record::new("save");
            rec.id = format!("record-{i}");
            rec.machine = "machine".into();
            rec.seq = i;
            rec.time = i as i64;
            rec.data = serde_json::json!({"line": "placeholder"});
            let body = serde_json::to_string(&rec)?.replace(r#"{"line":"placeholder"}"#, data);
            let raw = format!(r#"{{"r":{body},"s":""}}"#);
            let parsed = crate::oplog::parse_line(&raw, None)?.record;
            db.insert_record(&parsed, &raw)?;
        }
        for rebuild in [false, true] {
            if rebuild {
                db.conn.pragma_update(None, "user_version", 8)?;
                db.rebuild(true, |_| Ok(()))?;
            }
            assert_eq!(db.log_records(Some("last"), 10)?.len(), 1);
            assert!(db.log_records(Some("first"), 10)?.is_empty());
            let raw: String = db.conn.query_row("SELECT raw FROM records WHERE id='record-1'", [], |r| r.get(0))?;
            assert!(raw.contains(r#"{"line":"first","line":"last"}"#));
        }
        drop(db);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn log_records_decode_only_the_requested_records() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn, stale: false };
        for i in 0..41 {
            let mut rec = Record::new("save");
            rec.id = format!("record-{i}");
            rec.machine = "machine".into();
            rec.seq = i;
            rec.time = i as i64;
            rec.data = serde_json::json!({"line": "wanted"});
            let mut raw = serde_json::json!({"r": rec, "s": ""});
            if i == 0 {
                // Valid JSON, but not a decodable record. Outside every requested window.
                raw["r"]["actor"] = serde_json::Value::Null;
            }
            db.insert_record(&rec, &raw.to_string())?;
        }
        for line in [None, Some("wanted")] {
            assert_eq!(db.log_records(line, 1)?[0].id, "record-40");
            assert_eq!(db.log_records(line, 30)?.len(), 30);
            assert!(db.log_records(line, 41).is_err());
        }
        Ok(())
    }

    #[test]
    fn opening_a_database_that_init_is_creating_keeps_its_tables() {
        // The state between the schema and the version of a fresh database: an opener must not
        // take it for an old database and drop the derived tables under the creating process.
        let dir = std::env::temp_dir().join(format!("layr-db-creating-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        let creating = Connection::open(&path).unwrap();
        creating.execute_batch(SCHEMA).unwrap();
        creating.execute("INSERT INTO states(id, display, kind) VALUES('s', 's', 'commit')", []).unwrap();
        let db = Db::open(&path).unwrap();
        assert!(!db.stale);
        let kept: i64 = db.conn.query_row("SELECT COUNT(*) FROM states", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1);
        let version: i64 = db.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_database_with_records_is_still_rebuilt() {
        let dir = std::env::temp_dir().join(format!("layr-db-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(SCHEMA).unwrap();
        old.execute("INSERT INTO records(id, raw) VALUES('r', '{}')", []).unwrap();
        old.execute("INSERT INTO states(id, display, kind) VALUES('s', 's', 'commit')", []).unwrap();
        drop(old);
        let db = Db::open(&path).unwrap();
        assert!(db.stale);
        let kept: i64 = db.conn.query_row("SELECT COUNT(*) FROM states", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1, "the old derived rows stay until the rebuild commits");
        assert!(db.rebuild(true, |_| Ok(())).unwrap());
        let kept: i64 = db.conn.query_row("SELECT COUNT(*) FROM states", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 0, "derived rows are rebuilt from the records");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An old-schema database (version 6) with one record, one state and one line.
    fn old_database(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("layr-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(SCHEMA).unwrap();
        old.execute("INSERT INTO records(id, raw) VALUES('r', '{}')", []).unwrap();
        old.execute("INSERT INTO states(id, display, kind) VALUES('s', 's', 'commit')", []).unwrap();
        old.execute("INSERT INTO lines(id, name) VALUES('l', 'main')", []).unwrap();
        old.pragma_update(None, "user_version", 6).unwrap();
        path
    }

    fn count_rows(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_failed_rebuild_keeps_the_old_tables_and_version() {
        let path = old_database("failed-rebuild");
        let db = Db::open(&path).unwrap();
        assert!(db.rebuild(true, |_| anyhow::bail!("replay failed")).is_err());
        let reader = Connection::open(&path).unwrap();
        assert_eq!((count_rows(&reader, "states"), count_rows(&reader, "lines")), (1, 1));
        let version: i64 = reader.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 6, "the next open retries the rebuild");
        assert!(Db::open(&path).unwrap().stale);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn readers_see_the_old_rows_while_the_rebuild_transaction_is_open() {
        let path = old_database("rebuild-readers");
        let db = Db::open(&path).unwrap();
        let reader = Connection::open(&path).unwrap();
        let mut seen = None;
        assert!(db
            .rebuild(true, |_| {
                seen = Some((count_rows(&reader, "states"), count_rows(&reader, "lines")));
                Ok(())
            })
            .unwrap());
        assert_eq!(seen, Some((1, 1)), "the reader sees the complete old tables");
        assert_eq!(count_rows(&reader, "states"), 0, "the rebuilt tables are visible after the COMMIT");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_rebuild_waits_for_another_writer_without_failing() {
        let path = old_database("rebuild-waits");
        let db = Db::open(&path).unwrap();
        let (locked, held) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let other = Connection::open(&writer_path).unwrap();
            other.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(4));
            other.execute_batch("COMMIT").unwrap();
        });
        held.recv().unwrap();
        assert!(db.rebuild(true, |_| Ok(())).unwrap(), "the rebuild runs after the other writer commits");
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn other_readers_keep_the_old_rows_until_the_rebuild_commits() {
        // Opening an old database must not drop its derived tables: another process reads them
        // until the rebuild commits (it would see empty lines and fail with "not in a layr line").
        let dir = std::env::temp_dir().join(format!("layr-db-readers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(SCHEMA).unwrap();
        old.execute("INSERT INTO records(id, raw) VALUES('r', '{}')", []).unwrap();
        old.execute("INSERT INTO states(id, display, kind) VALUES('s', 's', 'commit')", []).unwrap();
        old.execute("INSERT INTO lines(id, name) VALUES('l', 'main')", []).unwrap();
        old.pragma_update(None, "user_version", 6).unwrap();
        drop(old);
        let db = Db::open(&path).unwrap();
        assert!(db.stale);
        let reader = Connection::open(&path).unwrap();
        let count =
            |table: &str| -> i64 { reader.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap() };
        assert_eq!((count("states"), count("lines")), (1, 1), "the old rows stay visible");
        drop(reader);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn state_record(seq: u64, state: &str, parents: Vec<String>) -> Record {
        let mut r = Record::new("commit");
        r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
        r.machine = "01a12290-0000-7000-8000-00000000aaaa".into();
        r.seq = seq;
        r.time = seq as i64;
        r.states.push(StateRec {
            id: state.into(),
            kind: StateKind::Commit,
            change_id: None,
            derived_from: None,
            context: None,
            parents,
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: 0,
            running: false,
            conflicts: vec![],
            caches: Default::default(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
            cache_of: None,
            tree: None,
            times: None,
            cache_key_parts: Default::default(),
            parts: Default::default(),
        });
        r
    }

    #[test]
    fn restored_states_clear_local_gc_tombstones() {
        let path = std::env::temp_dir().join(format!("layr-db-restore-{}", crate::ids::new_id()));
        let db = Db::open(&path).unwrap();
        let state = "01a12290-0000-7000-8000-00000000c0c0";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let creation = state_record(1, state, vec![]);
        let mut gc = Record::new("gc");
        gc.machine = machine.into();
        gc.data = serde_json::json!({"deleted": [state]});
        db.derive(&creation, &|_| true, machine).unwrap();
        db.derive(&gc, &|_| false, machine).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((false, true)));
        db.set_present(state, true).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((true, false)));
        // Reindex replays an older gc record against the snapshots that exist now.
        db.derive(&gc, &|_| true, machine).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((true, false)));
        // Another machine's gc never deletes the local copy.
        gc.machine = "01a12290-0000-7000-8000-00000000bbbb".into();
        db.derive(&gc, &|_| false, machine).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((true, false)));
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn records_set_the_stack_of_a_line() {
        let dir = std::env::temp_dir().join(format!("layr-db-stack-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let line = "01a12290-0000-7000-8000-00000000eeee";
        let layers = |s: &str| serde_json::json!([{"overlay": "keys", "state": s}]);
        let rec = |seq: u64, time: i64, machine: &str, st: &str| {
            let mut r = Record::new("commit");
            r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
            r.machine = machine.into();
            r.seq = seq;
            r.time = time;
            r.data = serde_json::json!({"line": line, "stack": {"line": line, "layers": layers(st)}});
            r
        };
        let (a, b) = ("01a12290-0000-7000-8000-000000000a01", "01a12290-0000-7000-8000-000000000a02");
        let local = "01a12290-0000-7000-8000-00000000aaaa";
        db.derive(&rec(2, 20, local, b), &|_| true, local).unwrap();
        // An older record that arrives later does not replace the newer stack.
        db.derive(&rec(1, 10, local, a), &|_| true, local).unwrap();
        assert_eq!(db.config(&format!("stack.{line}")).unwrap(), Some(layers(b)));
        // Another machine's stacks stay apart.
        let other = "01a12290-0000-7000-8000-00000000bbbb";
        db.derive(&rec(3, 30, other, a), &|_| true, local).unwrap();
        assert_eq!(db.config(&format!("stack.{line}")).unwrap(), Some(layers(b)));
        assert_eq!(db.config(&format!("stack.{line}@{other}")).unwrap(), Some(layers(a)));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn records_of_caches_add_to_the_caches_of_a_state() {
        let dir = std::env::temp_dir().join(format!("layr-db-caches-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let st = "01a12290-0000-7000-8000-00000000c0c0";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        db.derive(&state_record(1, st, vec![]), &|_| true, machine).unwrap();
        let caches = |seq: u64, path: &str, uuid: &str| {
            let mut r = Record::new("save.caches");
            r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
            r.machine = machine.into();
            r.seq = seq;
            r.time = seq as i64;
            let snap = format!("{st}@{path}");
            r.data = serde_json::json!({"state": st, "caches": {path: snap}, "uuids": {snap: uuid}, "shared": true});
            r
        };
        // Two saves of caches on the same state, one for each folder.
        db.derive(&caches(2, "target", "01a12290-0000-7000-8000-000000000001"), &|_| true, machine).unwrap();
        db.derive(&caches(3, ".cache", "01a12290-0000-7000-8000-000000000002"), &|_| true, machine).unwrap();
        let s = db.state(st).unwrap().unwrap();
        assert_eq!(s.caches.keys().map(|k| k.as_str()).collect::<Vec<_>>(), [".cache", "target"]);
        assert!(s.cache_shared);
        for (path, uuid) in
            [("target", "01a12290-0000-7000-8000-000000000001"), (".cache", "01a12290-0000-7000-8000-000000000002")]
        {
            assert_eq!(db.cache_uuid(st, &format!("{st}@{path}")).unwrap().as_deref(), Some(uuid));
        }
        // Records of an older layr name no platform.
        assert_eq!(s.cache_platform, None);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn a_state_keeps_the_platform_of_its_caches() {
        let dir = std::env::temp_dir().join(format!("layr-db-platform-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let st = "01a12290-0000-7000-8000-00000000c1c1";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        db.derive(&state_record(1, st, vec![]), &|_| true, machine).unwrap();
        let mut r = Record::new("save.layers");
        r.id = "01a12290-0000-7000-8000-000000000002".into();
        r.machine = machine.into();
        r.seq = 2;
        r.time = 2;
        let snap = format!("{st}@target");
        r.data = serde_json::json!({"state": st, "layers": {"target": snap}, "uuids": {snap: "01a12290-0000-7000-8000-000000000003"}, "shared": false, "platform": "macos-aarch64"});
        db.derive(&r, &|_| true, machine).unwrap();
        assert_eq!(db.state(st).unwrap().unwrap().cache_platform.as_deref(), Some("macos-aarch64"));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn a_state_id_cannot_be_named_twice() {
        let dir = std::env::temp_dir().join(format!("layr-db-test-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let st = "01a12290-0000-7000-8000-00000000bbbb";
        let parent = "01a12290-0000-7000-8000-00000000cccc".to_string();
        db.derive(&state_record(1, st, vec![parent.clone()]), &|_| true, "x").unwrap();
        let other = "01a12290-0000-7000-8000-00000000dddd".to_string();
        assert!(db.derive(&state_record(2, st, vec![other]), &|_| true, "x").is_err());
        assert_eq!(db.state(st).unwrap().unwrap().parents, vec![parent]);
        let _ = std::fs::remove_file(&dir);
    }
    fn memory() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db { conn, stale: false }
    }

    #[test]
    fn migration_mappings_survive_early_arrival_and_clock_skew() -> Result<()> {
        let state = "01a12290-0000-7000-8000-00000000c0c0";
        let source = state_record(10, state, vec![]);
        let mapping = |seq, digit: &str| {
            let mut record = state_record(seq, state, vec![]);
            record.states.clear();
            record.op = "state.tree".into();
            record.data = serde_json::json!({"states": {state: {"tree": digit.repeat(64), "times": "a".repeat(64)}}});
            record
        };
        let newer = mapping(3, "3");
        let older = mapping(2, "2");
        for records in [vec![newer.clone(), older.clone(), source.clone()], vec![source.clone(), newer.clone(), older.clone()]] {
            let db = memory();
            for record in &records {
                db.insert_record(record, &serde_json::json!({"r": record}).to_string())?;
                db.derive(record, &|_| true, &source.machine)?;
            }
            assert_eq!(db.state(state)?.unwrap().tree, Some("3".repeat(64)));
            let model = layr_model::Project::from_records("", records, &source.machine)?;
            assert_eq!(model.states[state].state.tree, Some("3".repeat(64)));
        }
        Ok(())
    }

    #[test]
    fn refs_replay_in_order_and_keep_their_history() {
        let db = memory();
        let a = "01a12290-0000-7000-8000-000000000003";
        let b = "01a12290-0000-7000-8000-000000000004";
        let rec = |n: u64, target: &str| {
            let mut r = state_record(n, a, vec![]);
            r.states.clear();
            r.op = "ref.set".into();
            r.data = serde_json::json!({"ref_change":RefChange { name:"env/prod".into(),before:None,after:Some(RefState{name:"env/prod".into(),target:target.into(),expires_at:Some(100)}) }});
            r
        };
        db.derive(&rec(2, b), &|_| true, "local").unwrap();
        db.derive(&rec(1, a), &|_| true, "local").unwrap();
        assert_eq!(db.ref_value("env/prod", 99).unwrap().unwrap().target, b);
        assert_eq!(db.ref_log("env/prod").unwrap().len(), 2);
        assert!(db.refs("env/", 100).unwrap().is_empty());
        assert_eq!(db.expired_refs(100).unwrap().len(), 1);
    }

    /// A signed `attest` record of `kind` on `state`, with optional extra data (a check result).
    fn attest_record(seq: u64, state: &str, kind: &str, check: Option<serde_json::Value>) -> Record {
        let mut r = state_record(seq, state, vec![]);
        r.states.clear();
        r.op = "attest".into();
        let digest = match &check {
            Some(c) => hex::encode(ContentAddress::digest(serde_json::to_vec(c).unwrap())),
            None => "0".repeat(64),
        };
        r.data = serde_json::json!({"attestation":Attestation {format:1,attestation_id:r.id.clone(),subject:Subject{kind:"state".into(),id:state.into()},change_id:None,kind:kind.into(),payload_digest:digest,payload_ref:None,signer:r.machine.clone(),time:seq as i64}});
        if let Some(c) = check {
            r.data["check"] = c;
        }
        r
    }

    #[test]
    fn records_of_the_removed_evidence_ops_are_no_evidence() {
        // `approve`, `approve.withdraw` and `check` records of layr before the attestation table
        // stay in the records, but they give no attestation, approval or check result.
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        for (seq, op) in [(1, "approve"), (2, "check")] {
            let mut r = state_record(seq, state, vec![]);
            r.states.clear();
            r.op = op.into();
            r.data = serde_json::json!({"state":state,"command":"true","argv":["true"],"exit":0});
            let raw = crate::oplog::sign_line(&r, &key).unwrap();
            db.insert_record(&r, &raw).unwrap();
            db.derive(&r, &|_| true, machine).unwrap();
        }
        assert!(db.attestations(state, None).unwrap().is_empty());
        assert!(db.approvals(state).unwrap().is_empty());
        assert!(db.checks(state).unwrap().is_empty());
        assert_eq!(db.check_exit(state, &["true".into()], None).unwrap(), None);
        // The original signed bytes are unchanged.
        assert!(crate::oplog::parse_line(&db.record_line(machine, 2).unwrap().unwrap(), Some(&key.verifying_key())).is_ok());
    }

    #[test]
    fn a_revoke_does_not_withdraw_another_signers_evidence() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let first = attest_record(1, state, "approval", None);
        db.derive(&first, &|_| true, machine).unwrap();
        let mut other = first.clone();
        other.id = "01a12290-0000-7000-8000-000000000002".into();
        other.seq = 2;
        other.op = "attest".into();
        other.time = 2;
        other.actor.uid = 99;
        other.data = serde_json::json!({"attestation":Attestation {format:1,attestation_id:other.id.clone(),subject:Subject{kind:"state".into(),id:state.into()},change_id:None,kind:"revoke".into(),payload_digest:"0".repeat(64),payload_ref:None,signer:machine.into(),time:2},"revokes":[first.id]});
        db.derive(&other, &|_| true, machine).unwrap();
        assert_eq!(db.approvals(state).unwrap(), vec![(0, 1)]);
    }
    #[test]
    fn root_revokes_the_evidence_of_every_user_of_its_machine_only() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let attest = |seq: u64, uid: u32, signer: &str, kind: &str, revokes: Vec<String>| {
            let mut r = state_record(seq, state, vec![]);
            r.states.clear();
            r.id = format!("01a12290-0000-7000-8000-{seq:012}");
            r.op = "attest".into();
            r.time = seq as i64;
            r.actor.uid = uid;
            r.machine = signer.into();
            r.data = serde_json::json!({"attestation":Attestation {format:1,attestation_id:r.id.clone(),subject:Subject{kind:"state".into(),id:state.into()},change_id:None,kind:kind.into(),payload_digest:"0".repeat(64),payload_ref:None,signer:signer.into(),time:seq as i64},"revokes":revokes});
            r
        };
        let other_machine = "01a12290-0000-7000-8000-00000000bbbb";
        let runner = attest(1, 1001, machine, "check", vec![]);
        let foreign = attest(2, 1001, other_machine, "check", vec![]);
        for r in [&runner, &foreign] {
            db.derive(r, &|_| true, machine).unwrap();
        }
        assert_eq!(db.own_attestations(state, machine, Some(0), None).unwrap(), Vec::<String>::new());
        assert_eq!(db.own_attestations(state, machine, None, None).unwrap(), vec![runner.id.clone()]);
        // A root revoke of this machine names the runner's check (and, forged, another machine's).
        let revoke = attest(3, 0, machine, "revoke", vec![runner.id.clone(), foreign.id.clone()]);
        db.derive(&revoke, &|_| true, machine).unwrap();
        // The filter of the evidence reads (`checks`, `check_exit`).
        let mut st = db
            .conn
            .prepare(
                "SELECT id FROM attestations a WHERE kind='check' AND NOT EXISTS (SELECT 1 FROM attestation_revocations v
                WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord) ORDER BY id",
            )
            .unwrap();
        let left: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(left, vec![foreign.id.clone()], "root revokes its machine's users only");
    }
    #[test]
    fn opening_a_current_database_takes_no_write_lock() {
        let dir = std::env::temp_dir().join(format!("layr-db-open-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        drop(Db::open(&path).unwrap());
        // Another process holds the write lock: opening a current database does not wait for it.
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        let db = Db::open(&path).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "open waited for the writer");
        drop(db);
        writer.execute_batch("ROLLBACK").unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scoped_declassification_does_not_count_as_subject_approval() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let mut rec = state_record(1, state, vec![]);
        rec.states.clear();
        rec.op = "attest".into();
        rec.data = serde_json::json!({"attestation":Attestation {format:1,attestation_id:rec.id.clone(),subject:Subject{kind:"state".into(),id:state.into()},change_id:None,kind:"approval".into(),payload_digest:"0".repeat(64),payload_ref:None,signer:machine.into(),time:1},"declassify":{"path":"a:1","hmac":"digest"}});
        let raw = crate::oplog::sign_line(&rec, &key).unwrap();
        db.insert_record(&rec, &raw).unwrap();
        db.derive(&rec, &|_| true, machine).unwrap();
        assert!(db.approvals(state).unwrap().is_empty());
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(db.attestations(state, Some("approval")).unwrap().len(), 1);
    }

    #[test]
    fn queue_status_replay_is_order_independent() {
        let db = memory();
        let id = "01a12290-0000-7000-8000-000000000003";
        let entry = QueueEntry {
            id: id.into(),
            into: "main".into(),
            target_line: "01a12290-0000-7000-8000-000000000004".into(),
            state: "01a12290-0000-7000-8000-000000000005".into(),
            change_id: None,
            added_by: 0,
            time: 1,
            status: "pending".into(),
            attempts: 0,
            result: None,
            reason: None,
            probes: None,
            title: None,
        };
        let event = |seq, status: Option<&str>| {
            let mut rec = state_record(seq, id, vec![]);
            rec.states.clear();
            rec.op = "queue.result".into();
            let after = status.map(|status| {
                let mut entry = entry.clone();
                entry.status = status.into();
                entry
            });
            rec.data = serde_json::json!({"queue_change":QueueChange{id:id.into(),before:None,after}});
            rec
        };
        db.derive(&event(3, Some("done")), &|_| true, "local").unwrap();
        db.derive(&event(1, Some("pending")), &|_| true, "local").unwrap();
        assert_eq!(db.queue_entry(id).unwrap().unwrap().status, "done");
        db.derive(&event(4, None), &|_| true, "local").unwrap();
        assert!(db.queue_entry(id).unwrap().is_none());
        db.derive(&event(5, Some("pending")), &|_| true, "local").unwrap();
        assert_eq!(db.queue_entries(Some(&entry.target_line)).unwrap().len(), 1);
    }
    #[test]
    fn check_reuse_preserves_argument_boundaries() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let insert = |seq, data: serde_json::Value| {
            let rec = attest_record(seq, state, "check", Some(data));
            let raw = crate::oplog::sign_line(&rec, &key).unwrap();
            db.insert_record(&rec, &raw).unwrap();
            db.derive(&rec, &|_| true, machine).unwrap();
        };
        insert(1, serde_json::json!({"state":state,"command":"sh -c exit 1","argv":["sh","-c","exit 1"],"exit":1}));
        assert_eq!(db.check_exit(state, &["sh".into(), "-c".into(), "exit 1".into()], None).unwrap(), Some(1));
        assert_eq!(db.check_exit(state, &["sh".into(), "-c".into(), "exit".into(), "1".into()], None).unwrap(), None);
        insert(2, serde_json::json!({"state":state,"command":"sh -c true","exit":0}));
        assert_eq!(db.check_exit(state, &["sh".into(), "-c".into(), "true".into()], None).unwrap(), None);
        // A result without its arguments matches no command, not even its one-word command text.
        insert(3, serde_json::json!({"state":state,"command":"true","exit":0}));
        assert_eq!(db.check_exit(state, &["true".into()], None).unwrap(), None);
        insert(4, serde_json::json!({"state":state,"command":"true","argv":["true"],"exit":1}));
        assert_eq!(db.check_exit(state, &["true".into()], None).unwrap(), Some(1));
        assert_eq!(db.check_exit(state, &["true".into()], Some(0)).unwrap(), None);
    }
}

// Addressable view descriptors stay in their signed records; no derived schema is needed.
impl Db {
    pub fn saved_view(&self, id: &str) -> Result<Option<crate::overlay::View>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(raw,'$.r.data.view') FROM records WHERE op='view.save'
             AND json_extract(raw,'$.r.data.view_id')=?1 ORDER BY time,seq LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    #[test]
    fn saved_descriptors_and_checks_accept_view_subjects() -> Result<()> {
        let path = std::env::temp_dir().join(format!("layr-view-db-{}", crate::ids::new_id()));
        std::fs::create_dir(&path)?;
        let db = Db::open(&path.join("test.sqlite"))?;
        let view = crate::overlay::View::new("01a12290-0000-7000-8000-000000000003".into(), Vec::new());
        let subject = view.id()?;
        let raw = serde_json::json!({"r":{"data":{"view_id":subject,"view":view}}});
        db.conn.execute("INSERT INTO records(id,op,raw,time,seq) VALUES('view','view.save',?1,1,1)", params![raw.to_string()])?;
        assert_eq!(db.saved_view(&subject)?, Some(view));
        let argv = vec!["sh".to_string(), "-c".into(), "check argument".into()];
        for (id, code, ord) in [("first", 7, "1"), ("second", 0, "2")] {
            let check = serde_json::json!({"argv":argv,"command":"sh -c check argument","exit":code});
            let digest = {
                use layr_hashers::{ContentAddress, Digest};
                hex::encode(ContentAddress::digest(serde_json::to_vec(&check)?))
            };
            let raw = serde_json::json!({"r":{"data":{"check":check,"attestation":{"payload_digest":digest}}}});
            db.conn.execute("INSERT INTO records(id,op,raw) VALUES(?1,'attest',?2)", params![id, raw.to_string()])?;
            db.conn.execute(
                "INSERT INTO attestations(id,subject,kind,machine,uid,ord) VALUES(?1,?2,'check','machine',1,?3)",
                params![id, subject, ord],
            )?;
        }
        assert_eq!(db.check_exit(&subject, &argv, None)?, Some(0));
        assert_eq!(db.check_exit(&subject, &argv, Some(7))?, Some(7));
        assert_eq!(db.check_exit(&subject, &["sh -c check argument".into()], None)?, None);
        assert_eq!(db.check_exit(&"0".repeat(64), &argv, None)?, None);
        db.conn.execute(
            "INSERT INTO attestation_revocations(id,target,machine,uid,ord) VALUES('revoke','second','machine',1,'3')",
            [],
        )?;
        assert_eq!(db.check_exit(&subject, &argv, None)?, Some(7));
        drop(db);
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
