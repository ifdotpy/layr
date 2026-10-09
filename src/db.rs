//! The project database (SQLite). The `records` table holds the signed, immutable operation
//! records: the source of truth. All other tables are derived from the records. A record and
//! its derived rows are written in one transaction, so they cannot disagree after a crash. The
//! derived tables can be rebuilt from the records at any time.

use crate::ids;
use crate::model::{Line, Record, StateKind, StateRec};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Db {
    pub conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS records (id TEXT PRIMARY KEY, machine TEXT, seq INTEGER, time INTEGER, op TEXT, actor_uid INTEGER, raw TEXT);
CREATE INDEX IF NOT EXISTS records_time ON records(time);
CREATE UNIQUE INDEX IF NOT EXISTS records_seq ON records(machine, seq);
CREATE TABLE IF NOT EXISTS states (id TEXT PRIMARY KEY, display TEXT UNIQUE, kind TEXT, line TEXT, time INTEGER, rec TEXT, present INTEGER DEFAULT 1, deleted INTEGER DEFAULT 0, dropped INTEGER DEFAULT 0, machine TEXT);
CREATE INDEX IF NOT EXISTS states_line ON states(line, kind, time);
CREATE TABLE IF NOT EXISTS parents (state TEXT, parent TEXT, ord INTEGER, PRIMARY KEY(state, ord));
CREATE INDEX IF NOT EXISTS parents_parent ON parents(parent);
CREATE TABLE IF NOT EXISTS lines (id TEXT PRIMARY KEY, name TEXT, owner INTEGER, machine TEXT, head TEXT, grp TEXT, deleted INTEGER DEFAULT 0, time INTEGER);
CREATE TABLE IF NOT EXISTS line_heads (line TEXT, machine TEXT, head TEXT, time INTEGER, PRIMARY KEY(line, machine));
CREATE TABLE IF NOT EXISTS tags (name TEXT PRIMARY KEY, state TEXT, message TEXT, time INTEGER);
CREATE TABLE IF NOT EXISTS remote_refs (name TEXT PRIMARY KEY, state TEXT, gcommit TEXT, time INTEGER);
CREATE TABLE IF NOT EXISTS git_map (state TEXT, gcommit TEXT, PRIMARY KEY(state, gcommit));
CREATE INDEX IF NOT EXISTS git_map_commit ON git_map(gcommit);
CREATE TABLE IF NOT EXISTS exports (id TEXT PRIMARY KEY, state TEXT, remote TEXT, branch TEXT, gcommit TEXT, status TEXT, time INTEGER);
CREATE TABLE IF NOT EXISTS checks (id TEXT PRIMARY KEY, state TEXT, command TEXT, exit INTEGER, time INTEGER, actor TEXT);
CREATE TABLE IF NOT EXISTS groups (name TEXT PRIMARY KEY, epoch INTEGER, base TEXT, accepted TEXT, time INTEGER);
CREATE TABLE IF NOT EXISTS group_lines (grp TEXT, line TEXT, epoch INTEGER, PRIMARY KEY(grp, line));
CREATE TABLE IF NOT EXISTS machines (id TEXT PRIMARY KEY, name TEXT, public_key TEXT);
CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS slots (path TEXT PRIMARY KEY, state TEXT, line TEXT, time INTEGER);
"#;

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL: a committed record survives a power loss (NORMAL can lose the last commits).
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn })
    }

    /// Run `f` in one write transaction (`BEGIN IMMEDIATE`): all or nothing.
    pub fn with_tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Delete all derived rows (records stay).
    pub fn clear_derived(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM states; DELETE FROM parents; DELETE FROM lines; DELETE FROM line_heads;
             DELETE FROM tags; DELETE FROM remote_refs; DELETE FROM git_map; DELETE FROM exports; DELETE FROM checks;
             DELETE FROM groups; DELETE FROM group_lines; DELETE FROM machines; DELETE FROM config; DELETE FROM slots;",
        )?;
        Ok(())
    }

    /// Insert a stored record line. Returns false if the record exists already.
    pub fn insert_record(&self, rec: &Record, line: &str) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO records(id, machine, seq, time, op, actor_uid, raw) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![rec.id, rec.machine, rec.seq as i64, rec.time, rec.op, rec.actor.uid as i64, line],
        )?;
        Ok(n > 0)
    }

    pub fn last_seq(&self, machine: &str) -> Result<u64> {
        let v: Option<i64> =
            self.conn.query_row("SELECT MAX(seq) FROM records WHERE machine=?1", params![machine], |r| r.get(0))?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// Stored lines of one machine after a sequence number, in order.
    pub fn lines_after(&self, machine: &str, seq: u64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT raw FROM records WHERE machine=?1 AND seq>?2 ORDER BY seq")?;
        let v = st.query_map(params![machine, seq as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn record_line(&self, machine: &str, seq: u64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT raw FROM records WHERE machine=?1 AND seq=?2", params![machine, seq as i64], |r| r.get(0))
            .optional()?)
    }

    pub fn record_machines(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT machine FROM records ORDER BY machine")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// All stored lines in apply order.
    pub fn all_lines(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT raw FROM records ORDER BY time, machine, seq")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Update the derived tables for one record. Runs inside the caller's transaction.
    pub fn derive(&self, rec: &Record, state_present: &dyn Fn(&str) -> bool) -> Result<()> {
        let tx = &self.conn;
        for s in &rec.states {
            let present = state_present(&s.id);
            tx.execute(
                "INSERT OR REPLACE INTO states(id, display, kind, line, time, rec, present, machine) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    s.id,
                    ids::display(&s.id),
                    kind_str(s.kind),
                    s.line,
                    s.time,
                    serde_json::to_string(s)?,
                    present as i64,
                    rec.machine
                ],
            )?;
            for (i, p) in s.parents.iter().enumerate() {
                tx.execute("INSERT OR REPLACE INTO parents(state, parent, ord) VALUES(?1,?2,?3)", params![s.id, p, i as i64])?;
            }
        }
        for (id, ch) in &rec.lines {
            match &ch.after {
                Some(a) => {
                    tx.execute(
                        "INSERT INTO lines(id, name, owner, machine, head, grp, deleted, time) VALUES(?1,?2,?3,?4,?5,?6,0,?7)
                         ON CONFLICT(id) DO UPDATE SET name=?2, owner=?3, machine=?4, head=?5, grp=?6, deleted=0, time=?7",
                        params![id, a.name, a.owner as i64, a.machine, a.head, a.group, rec.time],
                    )?;
                    tx.execute(
                        "INSERT INTO line_heads(line, machine, head, time) VALUES(?1,?2,?3,?4)
                         ON CONFLICT(line, machine) DO UPDATE SET head=?3, time=?4",
                        params![id, rec.machine, a.head, rec.time],
                    )?;
                }
                None => {
                    tx.execute("UPDATE lines SET deleted=1, time=?2 WHERE id=?1", params![id, rec.time])?;
                }
            }
        }
        let d = &rec.data;
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
        match rec.op.as_str() {
            "machine" => {
                tx.execute(
                    "INSERT OR REPLACE INTO machines(id, name, public_key) VALUES(?1,?2,?3)",
                    params![rec.machine, s("name"), s("public_key")],
                )?;
            }
            "project" | "config" => {
                if let Some(obj) = d.get("config").and_then(|v| v.as_object()) {
                    for (k, v) in obj {
                        if v.is_null() {
                            tx.execute("DELETE FROM config WHERE key=?1", params![k])?;
                        } else {
                            tx.execute("INSERT OR REPLACE INTO config(key, value) VALUES(?1,?2)", params![k, v.to_string()])?;
                        }
                    }
                }
            }
            "tag" => {
                tx.execute(
                    "INSERT OR REPLACE INTO tags(name, state, message, time) VALUES(?1,?2,?3,?4)",
                    params![s("name"), s("state"), s("message"), rec.time],
                )?;
            }
            "tag.delete" => {
                tx.execute("DELETE FROM tags WHERE name=?1", params![s("name")])?;
            }
            "import" | "remote.ref" => {
                if let (Some(name), Some(st)) = (s("ref"), s("state")) {
                    tx.execute(
                        "INSERT OR REPLACE INTO remote_refs(name, state, gcommit, time) VALUES(?1,?2,?3,?4)",
                        params![name, st, s("commit"), rec.time],
                    )?;
                }
                if let (Some(st), Some(c)) = (s("state"), s("commit")) {
                    tx.execute("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)", params![st, c])?;
                }
            }
            "export" => {
                tx.execute(
                    "INSERT OR REPLACE INTO exports(id, state, remote, branch, gcommit, status, time) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![s("export"), s("state"), s("remote"), s("branch"), s("commit"), s("status"), rec.time],
                )?;
                if let (Some(st), Some(c)) = (s("state"), s("commit")) {
                    tx.execute("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)", params![st, c])?;
                }
                if s("status").as_deref() == Some("pushed") {
                    if let (Some(r), Some(b), Some(st), Some(c)) = (s("remote"), s("branch"), s("state"), s("commit")) {
                        tx.execute(
                            "INSERT OR REPLACE INTO remote_refs(name, state, gcommit, time) VALUES(?1,?2,?3,?4)",
                            params![format!("{r}/{b}"), st, c, rec.time],
                        )?;
                    }
                }
            }
            "check" => {
                tx.execute(
                    "INSERT OR REPLACE INTO checks(id, state, command, exit, time, actor) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![rec.id, s("state"), s("command"), d.get("exit").and_then(|v| v.as_i64()), rec.time, rec.actor.name],
                )?;
            }
            "group.create" => {
                tx.execute(
                    "INSERT OR REPLACE INTO groups(name, epoch, base, accepted, time) VALUES(?1, 0, ?2, NULL, ?3)",
                    params![s("name"), s("base"), rec.time],
                )?;
            }
            "group.add" => {
                tx.execute(
                    "INSERT OR REPLACE INTO group_lines(grp, line, epoch) VALUES(?1, ?2, (SELECT epoch FROM groups WHERE name=?1))",
                    params![s("name"), s("line")],
                )?;
            }
            "group.accept" => {
                tx.execute(
                    "UPDATE groups SET epoch=epoch+1, accepted=?2, time=?3 WHERE name=?1",
                    params![s("name"), s("line"), rec.time],
                )?;
            }
            "gc" => {
                if let Some(arr) = d.get("deleted").and_then(|v| v.as_array()) {
                    for id in arr.iter().filter_map(|v| v.as_str()) {
                        tx.execute("UPDATE states SET deleted=1, present=0 WHERE id=?1", params![id])?;
                    }
                }
            }
            "save.layers" => {
                if let Some(st) = s("state") {
                    let raw: Option<String> =
                        tx.query_row("SELECT rec FROM states WHERE id=?1", params![st], |r| r.get(0)).optional()?;
                    if let Some(raw) = raw {
                        let mut v: serde_json::Value = serde_json::from_str(&raw)?;
                        v["layers"] = d.get("layers").cloned().unwrap_or_default();
                        tx.execute("UPDATE states SET rec=?2 WHERE id=?1", params![st, v.to_string()])?;
                    }
                }
            }
            "stash.drop" => {
                tx.execute("UPDATE states SET dropped=1 WHERE id=?1", params![s("state")])?;
            }
            "slot" => {
                tx.execute(
                    "INSERT OR REPLACE INTO slots(path, state, line, time) VALUES(?1,?2,?3,?4)",
                    params![s("path"), s("state"), s("line"), rec.time],
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn state(&self, id: &str) -> Result<Option<StateRec>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT rec FROM states WHERE id=?1", params![id], |r| r.get(0)).optional()?;
        Ok(match raw {
            Some(r) => Some(serde_json::from_str(&r)?),
            None => None,
        })
    }

    pub fn state_flags(&self, id: &str) -> Result<Option<(bool, bool)>> {
        Ok(self
            .conn
            .query_row("SELECT present, deleted FROM states WHERE id=?1", params![id], |r| {
                Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)? != 0))
            })
            .optional()?)
    }

    pub fn set_present(&self, id: &str, present: bool) -> Result<()> {
        self.conn.execute("UPDATE states SET present=?2 WHERE id=?1", params![id, present as i64])?;
        Ok(())
    }

    pub fn states_by_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM states WHERE display LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn present_states(&self) -> Result<Vec<(String, bool)>> {
        let mut st = self.conn.prepare("SELECT id, deleted FROM states WHERE present=1")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    fn line_row(r: &rusqlite::Row) -> rusqlite::Result<Line> {
        Ok(Line {
            id: r.get(0)?,
            name: r.get(1)?,
            owner: r.get::<_, i64>(2)? as u32,
            machine: r.get(3)?,
            head: r.get(4)?,
            group: r.get(5)?,
        })
    }

    pub fn line_by_name(&self, name: &str) -> Result<Option<Line>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, owner, machine, head, grp FROM lines WHERE name=?1 AND deleted=0 ORDER BY time DESC LIMIT 1",
                params![name],
                Self::line_row,
            )
            .optional()?)
    }

    pub fn line_by_id(&self, id: &str) -> Result<Option<Line>> {
        Ok(self
            .conn
            .query_row("SELECT id, name, owner, machine, head, grp FROM lines WHERE id=?1", params![id], Self::line_row)
            .optional()?)
    }

    pub fn lines(&self) -> Result<Vec<Line>> {
        let mut st = self.conn.prepare("SELECT id, name, owner, machine, head, grp FROM lines WHERE deleted=0 ORDER BY name")?;
        let v = st.query_map([], Self::line_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn line_heads(&self, line: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT machine, head FROM line_heads WHERE line=?1")?;
        let v = st
            .query_map(params![line], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// States of a kind for a line, newest first.
    pub fn line_states(&self, line: &str, kind: StateKind) -> Result<Vec<StateRec>> {
        let mut st = self.conn.prepare(
            "SELECT rec FROM states WHERE line=?1 AND kind=?2 AND deleted=0 AND dropped=0 ORDER BY time DESC, id DESC",
        )?;
        let v = st.query_map(params![line, kind_str(kind)], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn tag(&self, name: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT state FROM tags WHERE name=?1", params![name], |r| r.get(0)).optional()?)
    }

    pub fn tags(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT name, state, message FROM tags ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn remote_ref(&self, name: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row("SELECT state, gcommit FROM remote_refs WHERE name=?1", params![name], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn remote_refs(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT name, state, gcommit FROM remote_refs ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn git_commit_of(&self, state: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT gcommit FROM git_map WHERE state=?1 LIMIT 1", params![state], |r| r.get(0)).optional()?)
    }

    pub fn state_of_commit(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT state FROM git_map WHERE gcommit LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn exports(&self) -> Result<Vec<(String, String, String, String, String, String)>> {
        let mut st = self.conn.prepare("SELECT id, state, remote, branch, gcommit, status FROM exports ORDER BY time")?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn checks(&self, state: &str) -> Result<Vec<(String, i64, i64, String)>> {
        let mut st = self.conn.prepare("SELECT command, exit, time, actor FROM checks WHERE state=?1 ORDER BY time")?;
        let v = st
            .query_map(params![state], |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(-1), r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn group(&self, name: &str) -> Result<Option<(i64, String, Option<String>)>> {
        Ok(self
            .conn
            .query_row("SELECT epoch, base, accepted FROM groups WHERE name=?1", params![name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?)
    }

    pub fn group_line_epoch(&self, grp: &str, line: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT epoch FROM group_lines WHERE grp=?1 AND line=?2", params![grp, line], |r| r.get(0))
            .optional()?)
    }

    pub fn groups(&self) -> Result<Vec<(String, i64, String, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT name, epoch, base, accepted FROM groups ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn group_lines(&self, grp: &str) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare("SELECT line, epoch FROM group_lines WHERE grp=?1")?;
        let v = st.query_map(params![grp], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn config(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let v: Option<String> =
            self.conn.query_row("SELECT value FROM config WHERE key=?1", params![key], |r| r.get(0)).optional()?;
        Ok(match v {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub fn config_all(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT key, value FROM config ORDER BY key")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn machine_key(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT public_key FROM machines WHERE id=?1", params![id], |r| r.get(0)).optional()?)
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
        let mut st = self.conn.prepare("SELECT id, COALESCE(name, '') FROM machines ORDER BY id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn slot(&self, path: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row("SELECT state, line FROM slots WHERE path=?1", params![path], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn record(&self, id: &str) -> Result<Option<Record>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT raw FROM records WHERE id=?1", params![id], |r| r.get(0)).optional()?;
        match raw {
            Some(l) => Ok(Some(crate::oplog::parse_line(&l, None)?.record)),
            None => Ok(None),
        }
    }

    /// Records newest first.
    pub fn records(&self, limit: usize) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare("SELECT raw FROM records ORDER BY time DESC, seq DESC LIMIT ?1")?;
        let v = st.query_map(params![limit as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    pub fn records_since(&self, time: i64) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare("SELECT raw FROM records WHERE time>=?1 ORDER BY time")?;
        let v = st.query_map(params![time], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    pub fn max_seqs(&self) -> Result<Vec<(String, u64)>> {
        let mut st = self.conn.prepare("SELECT machine, MAX(seq) FROM records GROUP BY machine")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn undone(&self, id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM records WHERE op='undo' AND json_extract(raw, '$.r.data.undo')=?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

pub fn kind_str(k: StateKind) -> &'static str {
    match k {
        StateKind::Commit => "commit",
        StateKind::Auto => "auto",
        StateKind::Stash => "stash",
        StateKind::Import => "import",
    }
}

pub fn need<T>(v: Option<T>, what: &str) -> Result<T> {
    v.ok_or_else(|| anyhow!("{what} not found"))
}
