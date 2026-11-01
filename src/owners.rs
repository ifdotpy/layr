//! Owners of states, phase 1 (shadow mode). The table `owners(state, kind, holder)` says why a
//! state is kept. `Db::derive` keeps it current for every record. No command frees a state by it
//! yet: `layr gc` still frees by its retention plan and prints how the two differ, and `layr fsck`
//! checks the table against a replay of the records. Rules: docs/ownership.md.
//!
//! Every write goes through `set_owners`, which adds before it removes. A state that gains its
//! first owner holds its parents, derived source and conflict sources; a state that loses its last
//! owner releases them. Rings (automatic states, undo records, deleted lines) are sets of the
//! newest entries, so the table does not depend on the order in which records arrive.

use crate::db::{kind_str, Db, SCHEMA};
use crate::model::{Record, StateKind};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};

/// Ring sizes of the owners model. Each default is defined here and nowhere else.
pub const AUTO_RING_DEFAULT: usize = 10;
pub const UNDO_OPS_DEFAULT: usize = 20;
pub const UNDO_DELETED_DEFAULT: usize = 5;

/// Statement cache size of a connection that runs the owners code (it has about 20 statements).
pub const OWNERS_STATEMENTS: usize = 64;

/// Config keys of the rings. `retention.auto` is also read by `gc`, which keeps 50 when unset.
pub const KEY_AUTO: &str = "retention.auto";
pub const KEY_UNDO_OPS: &str = "undo.ops";
pub const KEY_UNDO_DELETED: &str = "undo.deleted";

/// One owner: (state, kind, holder).
pub type Row = (String, String, String);

/// The sort key of a record (time, machine, sequence number). Unique per record.
pub fn record_ord(rec: &Record) -> String {
    format!("{:016}:{}:{:012}", rec.time.max(0), rec.machine, rec.seq)
}

/// The half-open range of holders that start with `prefix` + `sep`.
fn span(prefix: &str, sep: char) -> (String, String) {
    (format!("{prefix}{sep}"), format!("{prefix}{}", (sep as u8 + 1) as char))
}

/// The undo entries of a record: one per line it belongs to. A record belongs to the lines it
/// changes and to the lines of the states it makes (an automatic save changes no line pointer).
/// The holder names the line and the record.
pub fn undo_entries(rec: &Record, ord: &str) -> Vec<(String, Vec<String>)> {
    let mut lines: BTreeSet<&str> = rec.lines.keys().map(|k| k.as_str()).collect();
    lines.extend(rec.states.iter().filter_map(|s| s.line.as_deref()));
    lines
        .into_iter()
        .map(|line| {
            let mut named: Vec<String> = rec.states.iter().map(|s| s.id.clone()).collect();
            if let Some(ch) = rec.lines.get(line) {
                named.extend([&ch.before, &ch.after].into_iter().flatten().map(|h| h.head.clone()));
                named.extend([&ch.working_before, &ch.working_after].into_iter().flatten().cloned());
            }
            (format!("{line}|{ord}"), named)
        })
        .collect()
}

/// The deleted-line entry of a record that deletes a line: its final head and staged work.
pub fn deleted_entry(rec: &Record, ord: &str) -> Vec<(String, Vec<String>)> {
    rec.lines
        .iter()
        .filter(|(_, ch)| ch.after.is_none())
        .filter_map(|(line, ch)| {
            let before = ch.before.as_ref()?;
            let mut named = vec![before.head.clone()];
            named.extend(ch.working_before.clone());
            Some((format!("{ord}|{line}"), named))
        })
        .collect()
}

impl Db {
    /// Statements of the owners code are prepared once per connection: a replay runs them for
    /// every record, and preparing each one again was most of the time.
    fn cached(&self, sql: &str) -> rusqlite::Result<rusqlite::CachedStatement<'_>> {
        self.conn.prepare_cached(sql)
    }

    fn exec(&self, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<usize> {
        self.conn.prepare_cached(sql)?.execute(p)
    }

    fn one<T>(
        &self,
        sql: &str,
        p: impl rusqlite::Params,
        f: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.conn.prepare_cached(sql)?.query_row(p, f)
    }

    fn owners_live(&self, state: &str) -> Result<bool> {
        Ok(self.one("SELECT EXISTS(SELECT 1 FROM owners WHERE state=?1)", params![state], |r| r.get(0))?)
    }

    fn line_alive(&self, line: &str) -> Result<bool> {
        let deleted: Option<i64> = self.one("SELECT deleted FROM lines WHERE id=?1", params![line], |r| r.get(0)).optional()?;
        Ok(deleted == Some(0))
    }

    fn ring_size(&self, key: &str, default: usize) -> usize {
        self.config(key).ok().flatten().and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(default)
    }

    /// The rows that a state holds: its parents, its derived source and its conflict sources.
    fn held_rows(&self, state: &str) -> Result<Vec<Row>> {
        let Some(s) = self.state(state)? else { return Ok(vec![]) };
        let mut rows: Vec<Row> = s.parents.iter().map(|p| (p.clone(), "parent".into(), state.to_string())).collect();
        if let Some(d) = &s.derived_from {
            rows.push((d.clone(), "derived".into(), state.to_string()));
        }
        for c in &s.conflicts {
            for x in [&c.base, &c.ours, &c.theirs].into_iter().flatten() {
                rows.push((x.clone(), "conflict".into(), state.to_string()));
            }
        }
        Ok(rows)
    }

    /// A state was made: if owners already name it, it holds its parents and sources now.
    pub(crate) fn owners_state_made(&self, state: &str) -> Result<()> {
        if self.owners_live(state)? {
            let rows = self.held_rows(state)?;
            self.add_rows(rows)?;
        }
        Ok(())
    }

    /// Add rows. A state that gains its first owner also gets the rows it holds.
    fn add_rows(&self, rows: Vec<Row>) -> Result<()> {
        let mut work = rows;
        while let Some((s, k, h)) = work.pop() {
            let had = self.owners_live(&s)?;
            let n = self.exec("INSERT OR IGNORE INTO owners(state, kind, holder) VALUES(?1,?2,?3)", params![s, k, h])?;
            if n > 0 && !had {
                work.extend(self.held_rows(&s)?);
            }
        }
        Ok(())
    }

    /// Remove rows. A state that loses its last owner releases the rows it holds.
    fn remove_rows(&self, rows: Vec<Row>) -> Result<()> {
        let mut work = rows;
        while let Some((s, k, h)) = work.pop() {
            let n = self.exec("DELETE FROM owners WHERE state=?1 AND kind=?2 AND holder=?3", params![s, k, h])?;
            if n > 0 && !self.owners_live(&s)? {
                let mut st = self.cached(
                    "SELECT state, kind, holder FROM owners WHERE holder=?1 AND kind IN ('parent','derived','conflict')",
                )?;
                let held = st
                    .query_map(params![s], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                work.extend(held);
            }
        }
        Ok(())
    }

    /// The states of one holder, after `kind` (the holder of a row is its owner's name).
    fn holder_states(&self, kind: &str, holder: &str) -> Result<BTreeSet<String>> {
        let mut st = self.cached("SELECT state FROM owners WHERE kind=?1 AND holder=?2")?;
        let v = st.query_map(params![kind, holder], |r| r.get::<_, String>(0))?.collect::<Result<BTreeSet<_>, _>>()?;
        Ok(v)
    }

    /// Make `targets` the owned states of one holder. Adds first, so a state that stays owned
    /// does not release and take its rows again.
    fn set_owners(&self, kind: &str, holder: &str, targets: &[String]) -> Result<()> {
        let old = self.holder_states(kind, holder)?;
        let want: BTreeSet<&String> = targets.iter().collect();
        let add: Vec<Row> =
            want.iter().filter(|t| !old.contains(**t)).map(|t| ((*t).clone(), kind.into(), holder.into())).collect();
        let del: Vec<Row> = old.iter().filter(|o| !want.contains(o)).map(|o| (o.clone(), kind.into(), holder.into())).collect();
        self.add_rows(add)?;
        self.remove_rows(del)
    }

    fn holders(&self, kind: &str, lo: &str, hi: &str) -> Result<Vec<String>> {
        let mut st =
            self.cached("SELECT DISTINCT holder FROM owners WHERE kind=?1 AND holder>=?2 AND holder<?3 ORDER BY holder")?;
        let v = st.query_map(params![kind, lo, hi], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    fn all_holders(&self, kind: &str) -> Result<Vec<String>> {
        self.holders(kind, "", "\u{10ffff}")
    }

    /// Heads, the automatic-state ring and the parks of one line, from the tables.
    fn sync_line(&self, line: &str) -> Result<()> {
        let alive = self.line_alive(line)?;
        let mut heads: BTreeMap<String, String> = BTreeMap::new();
        if alive {
            for (machine, head) in self.line_heads(line)? {
                heads.insert(format!("{line}@{machine}"), head);
            }
        }
        let (lo, hi) = span(line, '@');
        let mut holders: BTreeSet<String> = self.holders("head", &lo, &hi)?.into_iter().collect();
        holders.extend(heads.keys().cloned());
        for h in holders {
            let target: Vec<String> = heads.get(&h).cloned().into_iter().collect();
            self.set_owners("head", &h, &target)?;
        }
        let keep = self.ring_size(KEY_AUTO, AUTO_RING_DEFAULT);
        let autos = if alive {
            let mut st = self.cached("SELECT id FROM states WHERE line=?1 AND kind=?2 ORDER BY time DESC, id DESC LIMIT ?3")?;
            let v = st
                .query_map(params![line, kind_str(StateKind::Auto), keep as i64], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            v
        } else {
            vec![]
        };
        self.set_owners("auto", line, &autos)?;
        let parks = if alive {
            let mut st = self.cached("SELECT id FROM states WHERE line=?1 AND kind=?2 AND dropped=0")?;
            let v = st
                .query_map(params![line, kind_str(StateKind::Stash)], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            v
        } else {
            vec![]
        };
        self.set_owners("park", line, &parks)?;
        self.sync_overlays(line, alive)
    }

    /// The overlay layers of a line (`stack.<line>`): one holder per overlay, `<line>|<overlay>`.
    fn sync_overlays(&self, line: &str, alive: bool) -> Result<()> {
        let mut want: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if alive {
            if let Some(layers) = self.config(&format!("stack.{line}"))?.and_then(|v| v.as_array().cloned()) {
                for l in layers {
                    if let (Some(o), Some(s)) = (l["overlay"].as_str(), l["state"].as_str()) {
                        want.entry(format!("{line}|{o}")).or_default().push(s.to_string());
                    }
                }
            }
        }
        let (lo, hi) = span(line, '|');
        let mut holders: BTreeSet<String> = self.holders("overlay", &lo, &hi)?.into_iter().collect();
        holders.extend(want.keys().cloned());
        for h in holders {
            let target = want.get(&h).cloned().unwrap_or_default();
            self.set_owners("overlay", &h, &target)?;
        }
        Ok(())
    }

    /// Keep the newest undo records of a line (`undo.ops`).
    fn trim_undo(&self, line: &str) -> Result<()> {
        let keep = self.ring_size(KEY_UNDO_OPS, UNDO_OPS_DEFAULT);
        let (lo, hi) = span(line, '|');
        let holders = self.holders("undo", &lo, &hi)?;
        for h in holders.iter().take(holders.len().saturating_sub(keep)) {
            self.set_owners("undo", h, &[])?;
        }
        Ok(())
    }

    /// Keep the newest deleted lines (`undo.deleted`).
    fn trim_deleted(&self) -> Result<()> {
        let keep = self.ring_size(KEY_UNDO_DELETED, UNDO_DELETED_DEFAULT);
        let holders = self.all_holders("deleted")?;
        for h in holders.iter().take(holders.len().saturating_sub(keep)) {
            self.set_owners("deleted", h, &[])?;
        }
        Ok(())
    }

    /// Rebuild the undo and deleted rings from the stored records. Used when a ring size changes:
    /// a ring that shrank had dropped its oldest records, which a bigger ring must bring back.
    fn resync_rings(&self) -> Result<()> {
        let mut st = self.cached("SELECT id FROM lines")?;
        let lines = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        drop(st);
        for line in &lines {
            self.sync_line(line)?;
        }
        for h in self.all_holders("undo")? {
            self.set_owners("undo", &h, &[])?;
        }
        for h in self.all_holders("deleted")? {
            self.set_owners("deleted", &h, &[])?;
        }
        for raw in self.all_lines()? {
            let rec = crate::oplog::parse_line(&raw, None)?.record;
            let ord = record_ord(&rec);
            for (h, named) in undo_entries(&rec, &ord) {
                self.set_owners("undo", &h, &named)?;
            }
            for (h, named) in deleted_entry(&rec, &ord) {
                self.set_owners("deleted", &h, &named)?;
            }
        }
        let mut st = self.cached("SELECT DISTINCT substr(holder, 1, instr(holder, '|') - 1) FROM owners WHERE kind='undo'")?;
        let undo_lines = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        drop(st);
        for line in &undo_lines {
            self.trim_undo(line)?;
        }
        self.trim_deleted()
    }

    /// The state of a line's own stash or head lookups.
    fn state_line(&self, state: &str) -> Result<Option<String>> {
        Ok(self.one("SELECT line FROM states WHERE id=?1", params![state], |r| r.get(0)).optional()?.flatten())
    }

    /// The owners that one record changes. Runs at the end of `Db::derive`, after the record's own
    /// rows are written.
    pub(crate) fn owners_after(&self, rec: &Record, ord: &str, foreign: bool) -> Result<()> {
        let d = &rec.data;
        let mut lines: BTreeSet<String> = rec.lines.keys().cloned().collect();
        lines.extend(rec.states.iter().filter_map(|s| s.line.clone()));
        if rec.op == "stash.drop" {
            if let Some(line) = d["state"].as_str().map(|st| self.state_line(st)).transpose()?.flatten() {
                lines.insert(line);
            }
        }
        if !foreign {
            if let Some(line) = d["stack"]["line"].as_str() {
                lines.insert(line.to_string());
            }
        }
        for line in &lines {
            self.sync_line(line)?;
        }
        for (holder, named) in undo_entries(rec, ord) {
            self.set_owners("undo", &holder, &named)?;
            let line = holder.split('|').next().unwrap_or_default().to_string();
            self.trim_undo(&line)?;
        }
        let deleted = deleted_entry(rec, ord);
        for (holder, named) in &deleted {
            self.set_owners("deleted", holder, named)?;
        }
        if !deleted.is_empty() {
            self.trim_deleted()?;
        }
        if let Some(name) = d["ref_change"]["name"].as_str() {
            self.sync_ref(name)?;
        }
        match rec.op.as_str() {
            "tag" | "tag.delete" => {
                if let Some(name) = d["name"].as_str() {
                    self.sync_tag(name)?;
                }
            }
            "import" | "remote.ref" => {
                if let Some(name) = d["ref"].as_str() {
                    self.sync_remote_ref(name)?;
                }
            }
            "export" => {
                if let (Some(remote), Some(branch)) = (d["remote"].as_str(), d["branch"].as_str()) {
                    self.sync_export(remote, branch)?;
                    if d["status"].as_str() == Some("pushed") {
                        self.sync_remote_ref(&format!("{remote}/{branch}"))?;
                    }
                }
            }
            _ => {}
        }
        if !foreign {
            let ring_key = d
                .get("config")
                .and_then(|v| v.as_object())
                .is_some_and(|c| [KEY_AUTO, KEY_UNDO_OPS, KEY_UNDO_DELETED].iter().any(|k| c.contains_key(*k)));
            if ring_key {
                self.resync_rings()?;
            }
        }
        Ok(())
    }

    fn sync_ref(&self, name: &str) -> Result<()> {
        let target: Vec<String> = self
            .conn
            .query_row("SELECT target FROM refs WHERE name=?1 AND deleted=0", params![name], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .into_iter()
            .collect();
        self.set_owners("ref", name, &target)
    }

    fn sync_tag(&self, name: &str) -> Result<()> {
        let target: Vec<String> = self
            .conn
            .query_row("SELECT state FROM tags WHERE name=?1 AND deleted=0", params![name], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .into_iter()
            .collect();
        self.set_owners("tag", name, &target)
    }

    fn sync_remote_ref(&self, name: &str) -> Result<()> {
        let target: Vec<String> = self
            .conn
            .query_row("SELECT state FROM remote_refs WHERE name=?1", params![name], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .into_iter()
            .collect();
        self.set_owners("remote-ref", name, &target)
    }

    /// The newest export of a branch (by record order), whatever its status.
    fn sync_export(&self, remote: &str, branch: &str) -> Result<()> {
        let target: Vec<String> = self
            .conn
            .query_row(
                "SELECT state FROM exports WHERE remote=?1 AND branch=?2 ORDER BY ord DESC LIMIT 1",
                params![remote, branch],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .into_iter()
            .collect();
        self.set_owners("export", &format!("{remote}|{branch}"), &target)
    }

    /// Owners as stored, sorted.
    pub fn owner_rows(&self) -> Result<Vec<Row>> {
        let mut st = self.cached("SELECT state, kind, holder FROM owners ORDER BY state, kind, holder")?;
        let v = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The owners that the stored records give: a replay into a temporary database.
    pub fn recomputed_owners(&self, local: &str) -> Result<Vec<Row>> {
        let mem = Db { conn: Connection::open_in_memory()?, stale: false };
        mem.conn.set_prepared_statement_cache_capacity(OWNERS_STATEMENTS);
        mem.conn.execute_batch(SCHEMA)?;
        for raw in self.all_lines()? {
            let rec = crate::oplog::parse_line(&raw, None)?.record;
            mem.insert_record(&rec, &raw)?;
            mem.derive(&rec, &|_| true, local)?;
        }
        mem.owner_rows()
    }

    /// The rows missing from and extra in the stored table, against a recomputation.
    pub fn owners_diff(&self, want: &[Row]) -> Result<(Vec<Row>, Vec<Row>)> {
        let want: BTreeSet<Row> = want.iter().cloned().collect();
        let have: BTreeSet<Row> = self.owner_rows()?.into_iter().collect();
        Ok((want.difference(&have).cloned().collect(), have.difference(&want).cloned().collect()))
    }

    /// Replace the stored table with rows (`fsck --repair`).
    pub fn replace_owners(&self, rows: &[Row]) -> Result<()> {
        self.with_tx(|| {
            self.exec("DELETE FROM owners", [])?;
            for (s, k, h) in rows {
                self.exec("INSERT INTO owners(state, kind, holder) VALUES(?1,?2,?3)", params![s, k, h])?;
            }
            Ok(())
        })
    }

    /// Record that a thin layer of `state` is made over `base` (APFS only; no other file system
    /// writes this table).
    pub fn record_base(&self, state: &str, base: &str) -> Result<()> {
        self.exec("INSERT OR REPLACE INTO bases(state, base) VALUES(?1,?2)", params![state, base])?;
        Ok(())
    }

    /// Replace the layer bases with the ones found on disk (`layr reindex`).
    pub fn replace_bases(&self, bases: &BTreeMap<String, String>) -> Result<()> {
        self.with_tx(|| {
            self.exec("DELETE FROM bases", [])?;
            for (state, base) in bases {
                self.record_base(state, base)?;
            }
            Ok(())
        })
    }

    /// Shadow mode: the states that the plan frees and owners keep (with the kinds that keep them),
    /// and the present states that the plan keeps and no owner names.
    pub fn owners_difference(&self, keep: &BTreeSet<String>, freed: &[String]) -> Result<(Vec<(String, String)>, Vec<String>)> {
        let mut owned: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut st = self.cached("SELECT state, kind FROM owners")?;
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (state, kind) = row?;
            owned.entry(state).or_default().insert(kind);
        }
        let mut frees: Vec<(String, String)> = freed
            .iter()
            .filter_map(|s| owned.get(s).map(|k| (s.clone(), k.iter().cloned().collect::<Vec<_>>().join(","))))
            .collect();
        frees.sort();
        frees.dedup();
        let mut unowned: Vec<String> = self
            .present_states()?
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| keep.contains(id) && !owned.contains_key(id))
            .collect();
        unowned.sort();
        Ok((frees, unowned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, LineChange, LineState, StateKind, StateRec};

    const MACHINE: &str = "01a12290-0000-7000-8000-00000000aaaa";
    const LINE: &str = "01a12290-0000-7000-8000-00000000eeee";

    fn id(n: u64) -> String {
        format!("01a12290-0000-7000-8000-{:012}", n)
    }

    fn memory() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db { conn, stale: false }
    }

    fn state(id: &str, kind: StateKind, line: Option<&str>, parents: Vec<String>) -> StateRec {
        StateRec {
            id: id.into(),
            kind,
            change_id: None,
            derived_from: None,
            context: None,
            parents,
            line: line.map(|l| l.to_string()),
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
        }
    }

    /// A record of the test machine. `seq` is also the time.
    fn record(seq: u64, op: &str) -> Record {
        let mut r = Record::new(op);
        r.id = format!("01a12290-0000-7000-8000-{:012}", 0x100000 + seq);
        r.machine = MACHINE.into();
        r.seq = seq;
        r.time = seq as i64;
        r
    }

    fn head(h: &str) -> LineState {
        LineState { name: "main".into(), head: h.into(), owner: 0, machine: MACHINE.into() }
    }

    /// Store a record the way the store does (signed line, derived rows) and return it.
    fn apply(db: &Db, r: &Record) {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let raw = crate::oplog::sign_line(r, &key).unwrap();
        db.insert_record(r, &raw).unwrap();
        db.derive(r, &|_| true, MACHINE).unwrap();
    }

    fn owners_of(db: &Db, kind: &str, holder: &str) -> Vec<String> {
        db.holder_states(kind, holder).unwrap().into_iter().collect()
    }

    #[test]
    fn auto_ring_keeps_the_newest_states_and_releases_the_oldest() {
        let db = memory();
        for n in 1..=11 {
            let mut r = record(n, "save");
            r.states.push(state(&id(n), StateKind::Auto, Some(LINE), vec![]));
            r.lines.insert(
                LINE.into(),
                LineChange { before: None, after: Some(head(&id(n))), working_before: None, working_after: None },
            );
            apply(&db, &r);
        }
        let kept = owners_of(&db, "auto", LINE);
        assert_eq!(kept.len(), AUTO_RING_DEFAULT);
        assert!(!kept.contains(&id(1)), "the oldest automatic state left the ring");
        assert!(kept.contains(&id(11)));
    }

    #[test]
    fn undo_ring_releases_the_states_of_the_oldest_record() {
        let db = memory();
        for n in 1..=(UNDO_OPS_DEFAULT as u64 + 1) {
            let mut r = record(n, "save");
            let mut ch = LineChange { before: None, after: None, working_before: None, working_after: Some(id(200 + n)) };
            ch.after = Some(head(&id(100)));
            r.lines.insert(LINE.into(), ch);
            apply(&db, &r);
        }
        let holders: BTreeSet<String> = db.all_holders("undo").unwrap().into_iter().collect();
        assert_eq!(holders.len(), UNDO_OPS_DEFAULT);
        assert!(owners_of(&db, "undo", &format!("{LINE}|{}", record_ord(&record(1, "save")))).is_empty());
        assert!(!owners_of(&db, "undo", &format!("{LINE}|{}", record_ord(&record(2, "save")))).is_empty());
        assert!(!db.owner_rows().unwrap().iter().any(|(s, _, _)| s == &id(201)), "the oldest working state is released");
    }

    #[test]
    fn automatic_saves_without_a_line_pointer_belong_to_the_undo_ring_of_their_line() {
        let db = memory();
        for n in 1..=(UNDO_OPS_DEFAULT as u64 + 1) {
            let mut r = record(n, "save");
            r.states.push(state(&id(300 + n), StateKind::Auto, Some(LINE), vec![]));
            apply(&db, &r);
        }
        let first = format!("{LINE}|{}", record_ord(&record(1, "save")));
        let second = format!("{LINE}|{}", record_ord(&record(2, "save")));
        assert!(owners_of(&db, "undo", &first).is_empty(), "the oldest record left the ring");
        assert_eq!(owners_of(&db, "undo", &second), vec![id(302)]);
        assert_eq!(db.all_holders("undo").unwrap().len(), UNDO_OPS_DEFAULT);
    }

    #[test]
    fn a_line_delete_releases_its_head_and_auto_ring_and_keeps_the_final_state() {
        let db = memory();
        let mut r = record(1, "save");
        r.states.push(state(&id(1), StateKind::Auto, Some(LINE), vec![]));
        r.lines.insert(
            LINE.into(),
            LineChange { before: None, after: Some(head(&id(1))), working_before: None, working_after: Some(id(1)) },
        );
        apply(&db, &r);
        assert_eq!(owners_of(&db, "head", &format!("{LINE}@{MACHINE}")), vec![id(1)]);
        let mut del = record(2, "line.delete");
        del.lines.insert(
            LINE.into(),
            LineChange { before: Some(head(&id(1))), after: None, working_before: Some(id(1)), working_after: None },
        );
        apply(&db, &del);
        assert!(owners_of(&db, "head", &format!("{LINE}@{MACHINE}")).is_empty());
        assert!(owners_of(&db, "auto", LINE).is_empty());
        assert_eq!(db.all_holders("deleted").unwrap().len(), 1);
        assert!(!owners_of(&db, "deleted", &format!("{}|{LINE}", record_ord(&del))).is_empty());
    }

    #[test]
    fn the_stored_owners_equal_a_recomputation_in_any_arrival_order() {
        let (a, b, c) = (id(1), id(2), id(3));
        let mut recs = Vec::new();
        let mut r1 = record(1, "save");
        r1.states.push(state(&a, StateKind::Commit, None, vec![]));
        r1.lines
            .insert(LINE.into(), LineChange { before: None, after: Some(head(&a)), working_before: None, working_after: None });
        recs.push(r1);
        let mut r2 = record(2, "save");
        r2.states.push(state(&b, StateKind::Commit, None, vec![a.clone()]));
        r2.states.push(state(&c, StateKind::Auto, Some(LINE), vec![b.clone()]));
        r2.lines.insert(
            LINE.into(),
            LineChange { before: Some(head(&a)), after: Some(head(&b)), working_before: None, working_after: Some(c.clone()) },
        );
        recs.push(r2);
        let mut r3 = record(3, "ref.set");
        r3.data = serde_json::json!({"ref_change": {"name": "env/prod", "before": null, "after": {"name": "env/prod", "target": a, "expires_at": null}}});
        recs.push(r3);
        let mut r4 = record(4, "tag");
        r4.data = serde_json::json!({"name": "v1", "state": c});
        recs.push(r4);

        let forward = memory();
        for r in &recs {
            apply(&forward, r);
        }
        let want = forward.recomputed_owners(MACHINE).unwrap();
        assert_eq!(forward.owner_rows().unwrap(), want, "stored owners equal the replay");
        // Reverse arrival: the same rows once every record is in.
        let reverse = memory();
        for r in recs.iter().rev() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
            let raw = crate::oplog::sign_line(r, &key).unwrap();
            reverse.insert_record(r, &raw).unwrap();
            reverse.derive(r, &|_| true, MACHINE).unwrap();
        }
        assert_eq!(reverse.owner_rows().unwrap(), want, "arrival order does not change the owners");
        // The parent of the newest state is kept by it.
        assert!(owners_of(&forward, "parent", &b).contains(&a));
    }
}
