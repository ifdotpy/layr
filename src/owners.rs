//! Owners of states. The table `owners(state, kind, holder)` says why a state is kept, and
//! `Db::derive` keeps it current for every record. A state that loses its last owner is dead:
//! `states.present=0`, `states.deleted=1`, and a row in `free_queue`. The storage goes in the free
//! step after the command (`free.rs`). `layr gc` checks the table against a replay of the records
//! and `layr gc --repair` rebuilds it. Rules: docs/ownership.md.
//!
//! Every write goes through `set_owners`, which adds before it removes. A state that gains its
//! first owner holds its parents, derived source and conflict sources; a state that loses its last
//! owner releases them. Rings (automatic states, undo records, deleted lines) are sets of the
//! newest entries, so the table does not depend on the order in which records arrive.

/// Owner kinds that keep the tree of a state (docs/ownership.md, "Storage that a live state needs"). `parent`,
/// `derived`, `ref`, `tag`, `remote-ref` and `export` keep only the record. `conflict` needs the trees of its sides.
pub const TREE_OWNERS: [&str; 9] = ["head", "auto", "undo", "deleted", "park", "overlay", "pin", "conflict", "keep"];

use crate::db::{kind_str, Db, SCHEMA};
use crate::model::{Record, StateKind};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};

/// Owner kinds of live processes that no record makes: pins and tree reservations (`drop`). A replay
/// never gives them, so the owners check leaves them out. Keeps (`keep`) come from no record either,
/// but from the `keeps` table, which every replay applies (`restore_keeps`).
pub const LOCAL_OWNERS: [&str; 2] = ["pin", "drop"];

/// Ring sizes of the owners model. Each default is defined here and nowhere else.
pub const AUTO_RING_DEFAULT: usize = 10;
pub const UNDO_OPS_DEFAULT: usize = 20;
pub const UNDO_DELETED_DEFAULT: usize = 5;

/// Config keys of the rings. `retention.auto` is also read by `gc`, which keeps 50 when unset.
pub const KEY_AUTO: &str = "retention.auto";
pub const KEY_UNDO_OPS: &str = "undo.ops";
pub const KEY_UNDO_DELETED: &str = "undo.deleted";

/// One owner: (state, kind, holder).
pub type Row = (String, String, String);

thread_local! {
    /// True while a replay runs (`fill_only`): owners are filled and no state is freed.
    static FILL_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with freeing off. A rebuild replays the records this way: it fills the owners, and
/// the states that the replay leaves unowned stay (`layr gc` reports them, `--repair` frees them).
pub fn fill_only<T>(f: impl FnOnce() -> T) -> T {
    let was = FILL_ONLY.with(|c| c.replace(true));
    let v = f();
    FILL_ONLY.with(|c| c.set(was));
    v
}

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
            // An automatic save: undo reads the working state it changes from.
            if rec.op == "save" {
                named
                    .extend(["working_before", "working_after"].iter().filter_map(|k| rec.data[*k].as_str().map(str::to_string)));
            }
            (format!("{line}|{ord}"), named)
        })
        .collect()
}

/// The deleted-line entry of a record that deletes a line: its final head, its last working state
/// and the staged state (`staged_state` of the record's data).
pub fn deleted_entry(rec: &Record, ord: &str) -> Vec<(String, Vec<String>)> {
    rec.lines
        .iter()
        .filter(|(_, ch)| ch.after.is_none())
        .filter_map(|(line, ch)| {
            let before = ch.before.as_ref()?;
            let mut named = vec![before.head.clone()];
            named.extend(ch.working_before.clone());
            named.extend(rec.data["staged_state"].as_str().map(str::to_string));
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

    /// True when a row of this kind and holder exists.
    pub fn holder_exists(&self, kind: &str, holder: &str) -> Result<bool> {
        Ok(self.one("SELECT EXISTS(SELECT 1 FROM owners WHERE kind=?1 AND holder=?2)", params![kind, holder], |r| r.get(0))?)
    }

    /// True when at least one owner names the state (a pin counts).
    pub fn is_owned(&self, state: &str) -> Result<bool> {
        self.owners_live(state)
    }

    fn owners_live(&self, state: &str) -> Result<bool> {
        Ok(self.one("SELECT EXISTS(SELECT 1 FROM owners WHERE state=?1)", params![state], |r| r.get(0))?)
    }

    fn line_alive(&self, line: &str) -> Result<bool> {
        let deleted: Option<i64> = self.one("SELECT deleted FROM lines WHERE id=?1", params![line], |r| r.get(0)).optional()?;
        Ok(deleted == Some(0))
    }

    /// Whether an `undo` owner `{line}|{ord}` may exist. A deleted line keeps an undo owner only for
    /// its delete record, and only while the `deleted` ring keeps that record's `deleted` owner
    /// `{ord}|{line}`. Other lines keep all their undo owners.
    fn undo_allowed(&self, holder: &str) -> Result<bool> {
        let Some((line, ord)) = holder.split_once('|') else { return Ok(true) };
        if !self.line_deleted(line)? {
            return Ok(true);
        }
        self.holder_exists("deleted", &format!("{ord}|{line}"))
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

    /// Whether a command pins `state` now (a reader outside the store lock).
    pub fn is_pinned(&self, state: &str) -> Result<bool> {
        Ok(self.one("SELECT EXISTS(SELECT 1 FROM owners WHERE state=?1 AND kind='pin')", params![state], |r| r.get(0))?)
    }

    /// Whether a tree owner (`TREE_OWNERS`) keeps the tree of `state`.
    pub fn has_tree_owner(&self, state: &str) -> Result<bool> {
        const SQL: &str = "SELECT EXISTS(SELECT 1 FROM owners WHERE state=?1 AND kind IN \
            ('head','auto','undo','deleted','park','overlay','pin','conflict','keep'))";
        Ok(self.one(SQL, params![state], |r| r.get(0))?)
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

    /// A state lost its last owner: it is dead, and the free step (`free.rs`) moves its storage
    /// after the commit. Nothing is freed by a replay (`fill_only`).
    fn mark_dead(&self, state: &str) -> Result<()> {
        if FILL_ONLY.with(|c| c.get()) {
            return Ok(());
        }
        let n = self.exec("UPDATE states SET present=0, deleted=1 WHERE id=?1", params![state])?;
        if n > 0 {
            self.exec("INSERT OR IGNORE INTO free_queue(state) VALUES(?1)", params![state])?;
        }
        Ok(())
    }

    /// Remove rows. A state that loses its last owner is dead and releases the rows it holds. A state that loses
    /// its last tree owner but keeps others goes to the free queue too: the free step drops its tree when git can
    /// build it again (an imported state), and keeps the record.
    fn remove_rows(&self, rows: Vec<Row>) -> Result<()> {
        let mut work = rows;
        while let Some((s, k, h)) = work.pop() {
            let n = self.exec("DELETE FROM owners WHERE state=?1 AND kind=?2 AND holder=?3", params![s, k, h])?;
            if n > 0 && TREE_OWNERS.contains(&k.as_str()) && !FILL_ONLY.with(|c| c.get()) && !self.has_tree_owner(&s)? {
                self.exec("INSERT OR IGNORE INTO free_queue(state) VALUES(?1)", params![s])?;
            }
            if n > 0 && !self.owners_live(&s)? {
                self.mark_dead(&s)?;
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
        // At least one: the newest automatic state of a live line always stays (docs/ownership.md).
        let keep = self.ring_size(KEY_AUTO, AUTO_RING_DEFAULT).max(1);
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
        if !alive {
            self.exec("DELETE FROM keeps WHERE line=?1", params![line])?;
            let (lo, hi) = span(line, '/');
            for holder in self.holders("keep", &lo, &hi)? {
                self.set_owners("keep", &holder, &[])?;
            }
        }
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
            self.release_deleted(h)?;
        }
        Ok(())
    }

    /// Release a `deleted` owner and the `undo` owner of its delete record: both live as long as
    /// the line is in the deleted ring.
    fn release_deleted(&self, holder: &str) -> Result<()> {
        self.set_owners("deleted", holder, &[])?;
        if let Some((ord, line)) = holder.split_once('|') {
            self.set_owners("undo", &format!("{line}|{ord}"), &[])?;
        }
        Ok(())
    }

    /// Release the undo owners of a deleted line that `undo_allowed` refuses (its earlier operations).
    fn prune_undo(&self, line: &str) -> Result<()> {
        let (lo, hi) = span(line, '|');
        for h in self.holders("undo", &lo, &hi)? {
            if !self.undo_allowed(&h)? {
                self.set_owners("undo", &h, &[])?;
            }
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
            // Deleted owners first: the undo owner of a delete record needs its deleted owner.
            for (h, named) in deleted_entry(&rec, &ord) {
                self.set_owners("deleted", &h, &named)?;
            }
            for (h, named) in undo_entries(&rec, &ord) {
                if self.undo_allowed(&h)? {
                    self.set_owners("undo", &h, &named)?;
                }
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
        // Adds before releases: the rings of this record take their owners first, so a state that
        // the record keeps (a deleted line's final state) never looks dead in between. A state that
        // was dead in between would be queued for freeing and would miss the command's counts.
        let deleted = deleted_entry(rec, ord);
        for (holder, named) in &deleted {
            self.set_owners("deleted", holder, named)?;
        }
        let undo = undo_entries(rec, ord);
        // A deleted line keeps no undo owner of its earlier operations (`undo_allowed`): `undo` of
        // the delete works while the `deleted` ring keeps the delete record's owners.
        for (holder, named) in &undo {
            if self.undo_allowed(holder)? {
                self.set_owners("undo", holder, named)?;
            }
        }
        for line in &lines {
            if self.line_deleted(line)? {
                self.prune_undo(line)?;
            }
        }
        for line in &lines {
            self.sync_line(line)?;
        }
        for (holder, _) in &undo {
            let line = holder.split('|').next().unwrap_or_default().to_string();
            self.trim_undo(&line)?;
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
            "remote.remove" => {
                if let Some(remote) = d["remote"].as_str() {
                    self.clear_remote_refs_of(remote)?;
                }
            }
            "import" | "remote.ref" => {
                if let Some(name) = d["ref"].as_str() {
                    self.sync_remote_ref(name)?;
                }
            }
            "push" => {
                // A state that a store remote receives is kept by it (`store` owner). The store never
                // deletes a state it holds, so the owner is never released.
                if d["result"].as_str() == Some("success") {
                    if let Some(url) = d["url"].as_str() {
                        if matches!(crate::cmd::remote::kind(url), Ok(crate::cmd::remote::Kind::Store)) {
                            let remote = d["remote"].as_str().unwrap_or_default().to_string();
                            let rows: Vec<Row> = d["state_ids_sent"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|v| v.as_str())
                                .map(|s| (s.to_string(), "store".to_string(), remote.clone()))
                                .collect();
                            self.add_rows(rows)?;
                        }
                    }
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
            .prepare_cached("SELECT target FROM refs WHERE name=?1 AND deleted=0")?
            .query_row(params![name], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .into_iter()
            .collect();
        self.set_owners("ref", name, &target)
    }

    fn sync_tag(&self, name: &str) -> Result<()> {
        let target: Vec<String> = self
            .conn
            .prepare_cached("SELECT state FROM tags WHERE name=?1 AND deleted=0")?
            .query_row(params![name], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .into_iter()
            .collect();
        self.set_owners("tag", name, &target)
    }

    /// The refs of a removed remote have no owner.
    fn clear_remote_refs_of(&self, remote: &str) -> Result<()> {
        let mut st = self.conn.prepare_cached("SELECT name FROM remote_refs WHERE substr(name, 1, length(?1)+1)=?1||'/'")?;
        let names = st.query_map(params![remote], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        for name in names {
            self.set_owners("remote-ref", &name, &Vec::<String>::new())?;
        }
        Ok(())
    }

    fn sync_remote_ref(&self, name: &str) -> Result<()> {
        let target: Vec<String> = self
            .conn
            .prepare_cached("SELECT state FROM remote_refs WHERE name=?1")?
            .query_row(params![name], |r| r.get::<_, Option<String>>(0))
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
            .prepare_cached("SELECT state FROM exports WHERE remote=?1 AND branch=?2 ORDER BY ord DESC LIMIT 1")?
            .query_row(params![remote, branch], |r| r.get::<_, Option<String>>(0))
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

    /// The owners that the stored records give: a replay into a temporary database of the records that `counts`
    /// (`Project::refusal`).
    pub fn recomputed_owners(&self, local: &str, counts: &dyn Fn(&crate::model::Record) -> Result<bool>) -> Result<Vec<Row>> {
        let mem = Db { conn: Connection::open_in_memory()?, stale: false };
        mem.conn.set_prepared_statement_cache_capacity(crate::db::STATEMENT_CACHE_CAPACITY);
        mem.conn.execute_batch(SCHEMA)?;
        fill_only(|| -> Result<()> {
            for raw in self.all_lines()? {
                let rec = crate::oplog::parse_line(&raw, None)?.record;
                mem.insert_record(&rec, &raw)?;
                if counts(&rec)? {
                    mem.derive(&rec, &|_| true, local)?;
                }
            }
            mem.restore_keeps(self)
        })?;
        mem.owner_rows()
    }

    /// The rows missing from and extra in the stored table, against a recomputation. Local owners
    /// (`LOCAL_OWNERS`) come from no record, so they are not compared.
    pub fn owners_diff(&self, want: &[Row]) -> Result<(Vec<Row>, Vec<Row>)> {
        let want: BTreeSet<Row> = want.iter().cloned().collect();
        let have: BTreeSet<Row> = self.owner_rows()?.into_iter().filter(|r| !LOCAL_OWNERS.contains(&r.1.as_str())).collect();
        Ok((want.difference(&have).cloned().collect(), have.difference(&want).cloned().collect()))
    }

    /// Make the stored table equal to a replay: add the `missing` rows, remove the `extra` ones
    /// (`owners_diff` against that replay). Pins are not touched.
    pub fn apply_owners_diff(&self, missing: &[Row], extra: &[Row]) -> Result<()> {
        if missing.is_empty() && extra.is_empty() {
            return Ok(());
        }
        self.with_tx(|| {
            for (s, k, h) in extra {
                self.exec("DELETE FROM owners WHERE state=?1 AND kind=?2 AND holder=?3", params![s, k, h])?;
            }
            for (s, k, h) in missing {
                self.exec("INSERT OR IGNORE INTO owners(state, kind, holder) VALUES(?1,?2,?3)", params![s, k, h])?;
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
    /// Set the base rows `set` and remove the rows of the layers `gone`, in one transaction.
    pub fn fix_bases(&self, set: &BTreeMap<String, String>, gone: &[String]) -> Result<()> {
        self.with_tx(|| {
            for (state, base) in set {
                self.record_base(state, base)?;
            }
            for state in gone {
                self.exec("DELETE FROM bases WHERE state=?1", params![state])?;
            }
            Ok(())
        })
    }

    pub fn replace_bases(&self, bases: &BTreeMap<String, String>) -> Result<()> {
        self.with_tx(|| {
            self.exec("DELETE FROM bases", [])?;
            for (state, base) in bases {
                self.record_base(state, base)?;
            }
            Ok(())
        })
    }

    /// The states in the free queue: dead states whose storage may still be on disk.
    pub fn queued_frees(&self) -> Result<Vec<String>> {
        let mut st = self.cached("SELECT state FROM free_queue ORDER BY state")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Remove states from the free queue (their storage is gone, or they are live again).
    pub fn dequeue_frees(&self, states: &[String]) -> Result<()> {
        self.with_tx(|| {
            for s in states {
                self.exec("DELETE FROM free_queue WHERE state=?1", params![s])?;
            }
            Ok(())
        })
    }

    /// Present states that no owner names. Only a replay of an old database or a crash leaves them.
    pub fn leaked_states(&self) -> Result<Vec<String>> {
        let mut st = self
            .cached("SELECT id FROM states WHERE present=1 AND deleted=0 AND id NOT IN (SELECT state FROM owners) ORDER BY id")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The `undo` owners that `undo_allowed` refuses: a deleted line outside the deleted ring. A
    /// store written before the rule keeps them; `gc --repair` releases them.
    pub fn stale_undo_rows(&self) -> Result<Vec<Row>> {
        let rows: Vec<Row> = {
            let mut st = self.cached("SELECT state, kind, holder FROM owners WHERE kind='undo' ORDER BY state, holder")?;
            let v = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<Result<Vec<Row>, _>>()?;
            v
        };
        let mut out = Vec::new();
        for row in rows {
            if !self.undo_allowed(&row.2)? {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// The present states that none of `rows` names: what a repair to `rows` leaves unowned,
    /// so `gc --repair` frees them.
    pub fn unowned_by(&self, rows: &[Row]) -> Result<Vec<String>> {
        let named: BTreeSet<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        let mut st = self.cached("SELECT id FROM states WHERE present=1 AND deleted=0 ORDER BY id")?;
        let all = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(all.into_iter().filter(|s| !named.contains(s.as_str())).collect())
    }

    /// Mark dead the present states that no owner names and that were made before `before_ms`. A command
    /// gives each state it records an owner within that command; a state without one for longer is left
    /// by a replay (`fill_only`) or a crash. The daemon runs this hourly, so leaks need no `gc --repair`.
    /// The free step of the next command moves their storage. Returns how many states it marked.
    pub fn free_old_leaks(&self, before_ms: i64) -> Result<usize> {
        let old: Vec<String> = {
            let mut st = self.cached(
                "SELECT id FROM states WHERE present=1 AND deleted=0 AND time<?1 AND id NOT IN (SELECT state FROM owners) ORDER BY id",
            )?;
            let v = st.query_map(params![before_ms], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            v
        };
        if !old.is_empty() {
            self.free_leaks(&old)?;
        }
        Ok(old.len())
    }

    /// Mark present states dead (`gc --repair`: their owners are gone).
    pub fn free_leaks(&self, states: &[String]) -> Result<()> {
        self.with_tx(|| {
            for s in states {
                self.mark_dead(s)?;
            }
            Ok(())
        })
    }

    /// Pin states for a command in one durable transaction (`free.rs`). States without an
    /// owner need no pin. A refused state rolls back the whole batch before the caller reads.
    pub fn pins_add(&self, states: &[&str], holder: &str) -> Result<Vec<String>> {
        self.with_tx(|| {
            let mut pinned = Vec::new();
            for state in states {
                let dropping: bool =
                    self.one("SELECT EXISTS(SELECT 1 FROM owners WHERE state=?1 AND kind='drop')", params![state], |r| r.get(0))?;
                anyhow::ensure!(
                    !dropping,
                    "layr gc is removing the tree of state {} from this machine; read it again",
                    ids_short(state)
                );
                if !self.owners_live(state)? {
                    // The free queue no longer contains a state after its storage is removed.
                    // Its tombstone must still refuse a reader that selected it before removal.
                    let freed: bool = self.one(
                        "SELECT EXISTS(SELECT 1 FROM free_queue WHERE state=?1)
                         OR EXISTS(SELECT 1 FROM states WHERE id=?1 AND deleted=1)",
                        params![state],
                        |r| r.get(0),
                    )?;
                    anyhow::ensure!(!freed, "state {} was freed by another layr command; read it again", ids_short(state));
                    continue;
                }
                self.exec("INSERT OR IGNORE INTO owners(state, kind, holder) VALUES(?1,'pin',?2)", params![state, holder])?;
                pinned.push((*state).to_string());
            }
            Ok(pinned)
        })
    }

    /// Reserve the trees of `states` for removal by `holder`, while their records stay (eviction to a
    /// store remote, imports that git builds again): a row `drop` each. A state that a reader pinned
    /// is left out; a pin after the reservation is refused (`pins_add`) until `release_trees`. So a
    /// reader never reads a tree that goes. Returns the reserved states.
    pub fn reserve_trees(&self, states: &[String], holder: &str) -> Result<Vec<String>> {
        self.with_tx(|| {
            let mut reserved = Vec::new();
            for s in states {
                if !self.is_pinned(s)? {
                    self.exec("INSERT OR IGNORE INTO owners(state, kind, holder) VALUES(?1,'drop',?2)", params![s, holder])?;
                    reserved.push(s.clone());
                }
            }
            Ok(reserved)
        })
    }

    /// End the reservations of `holder` on `reserved`; the states of `gone` lost their trees and are
    /// absent now. A state that lost its other owners meanwhile is dead. One transaction: a reader pins either before (the tree is there) or after (the
    /// state is absent and comes back from its remote or from git).
    pub fn release_trees(&self, reserved: &[String], gone: &[String], holder: &str) -> Result<()> {
        self.with_tx(|| {
            for s in gone {
                self.set_present(s, false)?;
            }
            // A record may take the last other owner meanwhile: the state is dead then.
            self.remove_rows(reserved.iter().map(|s| (s.clone(), "drop".to_string(), holder.to_string())).collect())
        })
    }

    /// Release pins in one transaction. States that lose their last owner become dead.
    pub fn pins_remove(&self, states: &[String], holder: &str) -> Result<()> {
        self.with_tx(|| self.remove_rows(states.iter().map(|state| (state.clone(), "pin".into(), holder.to_string())).collect()))
    }

    /// Keep `state` under `name` in `line` (`layr save --keep`), in place of the state it kept before.
    /// The `keeps` table is the source; the `keep` owner `{line}/{name}` follows it.
    pub fn keep_set(&self, line: &str, name: &str, state: &str) -> Result<()> {
        self.with_tx(|| {
            self.exec("INSERT OR REPLACE INTO keeps(line, name, state) VALUES(?1,?2,?3)", params![line, name, state])?;
            self.set_owners("keep", &format!("{line}/{name}"), &[state.to_string()])
        })
    }

    /// Release the keep `name` of `line`. False: there was none.
    pub fn keep_drop(&self, line: &str, name: &str) -> Result<bool> {
        let removed = self.with_tx(|| {
            let n = self.exec("DELETE FROM keeps WHERE line=?1 AND name=?2", params![line, name])?;
            self.set_owners("keep", &format!("{line}/{name}"), &[])?;
            Ok(n)
        })?;
        Ok(removed > 0)
    }

    /// The keeps of `line`: (name, state), by name.
    pub fn keeps(&self, line: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.cached("SELECT name, state FROM keeps WHERE line=?1 ORDER BY name")?;
        let rows = st.query_map(params![line], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Give the keeps of `from` (the project database) their owners here, after a replay into this
    /// database (a rebuild, or the replay of the owners check): a replay makes no keep, and a kept
    /// state the ring dropped holds its parents again. A keep of a deleted line is left out.
    pub fn restore_keeps(&self, from: &Db) -> Result<()> {
        let mut st = from.cached("SELECT line, name, state FROM keeps")?;
        let keeps =
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        drop(st);
        for (line, name, state) in keeps {
            if self.line_alive(&line)? {
                self.set_owners("keep", &format!("{line}/{name}"), &[state])?;
            }
        }
        Ok(())
    }

    /// Holders of pins (`pid:start`), for the check of dead processes.
    pub fn pin_holders(&self) -> Result<Vec<String>> {
        self.all_holders("pin")
    }

    /// Release every pin of one holder (its process is gone).
    pub fn release_holder(&self, holder: &str) -> Result<()> {
        self.with_tx(|| {
            let mut st = self.cached("SELECT state FROM owners WHERE kind='pin' AND holder=?1")?;
            let rows: Vec<Row> = st
                .query_map(params![holder], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|s| (s, "pin".to_string(), holder.to_string()))
                .collect();
            drop(st);
            self.remove_rows(rows)
        })
    }
}

fn ids_short(id: &str) -> String {
    id.chars().take(8).collect()
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
            cache_of: None,
            tree: None,
            times: None,
            cache_key_parts: Default::default(),
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

    #[test]
    fn old_leaks_are_freed_and_young_or_owned_states_stay() {
        let db = memory();
        for (sid, time) in [("old", 1_000i64), ("young", 9_000), ("owned", 1_000)] {
            db.conn
                .execute(
                    "INSERT INTO states(id, display, kind, time, present) VALUES(?1, ?1, 'commit', ?2, 1)",
                    params![sid, time],
                )
                .unwrap();
        }
        db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES('owned', 'head', 'x')", []).unwrap();
        assert_eq!(db.free_old_leaks(5_000).unwrap(), 1);
        let row = |sid: &str| -> (i64, i64) {
            db.conn.query_row("SELECT present, deleted FROM states WHERE id=?1", [sid], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(row("old"), (0, 1), "a state without an owner past the age is dead");
        assert_eq!(row("young"), (1, 0), "a young state may still get its owner from a running command");
        assert_eq!(row("owned"), (1, 0));
        let queued: i64 = db.conn.query_row("SELECT count(*) FROM free_queue WHERE state='old'", [], |r| r.get(0)).unwrap();
        assert_eq!(queued, 1, "the free step moves its storage");
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
        let want = forward.recomputed_owners(MACHINE, &|_| Ok(true)).unwrap();
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

    /// A save of `line`: its head stays `head`, and `working` is its new automatic state.
    fn save_on(db: &Db, seq: u64, line: &str, head_id: String, working: String) {
        let mut r = record(seq, "save");
        r.states.push(state(&working, StateKind::Auto, Some(line), vec![]));
        r.lines.insert(
            line.into(),
            LineChange { before: None, after: Some(head(&head_id)), working_before: None, working_after: Some(working) },
        );
        apply(db, &r);
    }

    /// The delete of `line`: its final head and its last working state are its final state.
    fn delete_of(db: &Db, seq: u64, line: &str, head_id: String, working: String) -> Record {
        let mut del = record(seq, "line.delete");
        del.lines.insert(
            line.into(),
            LineChange { before: Some(head(&head_id)), after: None, working_before: Some(working), working_after: None },
        );
        apply(db, &del);
        del
    }

    /// A config record that sets one ring size (a ring change runs `resync_rings`).
    fn set_ring(db: &Db, seq: u64, key: &str, value: u64) {
        let mut r = record(seq, "config");
        r.data = serde_json::json!({"config": {key: value}});
        apply(db, &r);
    }

    const LINE2: &str = "01a12290-0000-7000-8000-00000000ffff";

    #[test]
    fn a_deleted_line_releases_its_undo_owner_when_the_deleted_ring_drops_it() {
        let db = memory();
        set_ring(&db, 1, KEY_UNDO_DELETED, 1);
        save_on(&db, 2, LINE, id(1), id(2));
        let del = delete_of(&db, 3, LINE, id(1), id(2));
        let undo_of_delete = format!("{LINE}|{}", record_ord(&del));
        let deleted_of_line = format!("{}|{LINE}", record_ord(&del));
        assert!(!owners_of(&db, "undo", &undo_of_delete).is_empty(), "the delete keeps its undo owner");
        assert!(!owners_of(&db, "deleted", &deleted_of_line).is_empty(), "the delete keeps its deleted owner");
        assert!(db.is_owned(&id(2)).unwrap());
        // A second deleted line pushes the first one out of the ring of one.
        save_on(&db, 4, LINE2, id(5), id(6));
        delete_of(&db, 5, LINE2, id(5), id(6));
        assert!(owners_of(&db, "deleted", &deleted_of_line).is_empty(), "the deleted owner left the ring");
        assert!(owners_of(&db, "undo", &undo_of_delete).is_empty(), "the undo owner of the delete goes with it");
        assert!(!db.is_owned(&id(2)).unwrap(), "the final state of the line has no owner");
        assert_eq!(
            db.queued_frees().unwrap().iter().filter(|s| *s == &id(2)).count(),
            1,
            "the final state is queued for freeing"
        );
    }

    #[test]
    fn a_ring_change_gives_a_deleted_line_no_undo_of_its_earlier_operations() {
        let db = memory();
        save_on(&db, 1, LINE, id(1), id(2));
        save_on(&db, 2, LINE, id(1), id(3));
        let del = delete_of(&db, 3, LINE, id(1), id(3));
        let undo_of_delete = format!("{LINE}|{}", record_ord(&del));
        let earlier =
            [format!("{LINE}|{}", record_ord(&record(1, "save"))), format!("{LINE}|{}", record_ord(&record(2, "save")))];
        for h in &earlier {
            assert!(owners_of(&db, "undo", h).is_empty(), "the delete released the earlier undo {h}");
        }
        // A ring change replays the records: the deleted line must not get its earlier undo back.
        set_ring(&db, 4, KEY_UNDO_OPS, UNDO_OPS_DEFAULT as u64 + 1);
        for h in &earlier {
            assert!(owners_of(&db, "undo", h).is_empty(), "the ring change gave back the earlier undo {h}");
        }
        let line_undo: Vec<String> = db.all_holders("undo").unwrap().into_iter().filter(|h| h.starts_with(LINE)).collect();
        assert_eq!(line_undo, vec![undo_of_delete], "only the undo of the delete record stays");
        assert_eq!(db.owner_rows().unwrap(), db.recomputed_owners(MACHINE, &|_| Ok(true)).unwrap(), "the replay agrees");
    }

    #[test]
    fn pin_batches_roll_back_on_a_refused_state() {
        let db = memory();
        let (a, b) = (id(1), id(2));
        db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'store','cloud')", params![a]).unwrap();
        // A late refusal must undo pins acquired earlier in this batch, for either reason.
        for refusal in
            ["INSERT INTO owners(state, kind, holder) VALUES(?1,'drop','gc')", "INSERT INTO free_queue(state) VALUES(?1)"]
        {
            db.conn.execute(refusal, params![b]).unwrap();
            let before = db.owner_rows().unwrap();
            assert!(db.pins_add(&[&a, &b], "reader").unwrap_err().to_string().contains("read it again"));
            assert_eq!(db.owner_rows().unwrap(), before);
            assert!(!db.is_pinned(&a).unwrap());
            db.conn.execute("DELETE FROM owners WHERE kind='drop'", []).unwrap();
            db.conn.execute("DELETE FROM free_queue", []).unwrap();
        }
    }

    #[test]
    fn pin_refuses_a_freed_state_after_the_free_queue_is_drained() {
        let db = memory();
        let (live, freed) = (id(1), id(2));
        db.conn.execute("INSERT INTO states(id, present, deleted) VALUES(?1,1,0)", params![live]).unwrap();
        db.conn.execute("INSERT INTO states(id, present, deleted) VALUES(?1,0,1)", params![freed]).unwrap();
        db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'head','line')", params![live]).unwrap();
        assert!(db.queued_frees().unwrap().is_empty());
        let before = db.owner_rows().unwrap();
        let error = db.pins_add(&[&live, &freed], "publisher").unwrap_err();
        assert!(error.to_string().contains("read it again"));
        assert_eq!(db.owner_rows().unwrap(), before, "the earlier pin rolls back");
        assert_eq!(db.state_flags(&freed).unwrap(), Some((false, true)));
    }

    #[test]
    fn pin_release_batches_roll_back_on_failure() {
        let db = memory();
        let (a, b) = (id(1), id(2));
        for s in [&a, &b] {
            db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'store','cloud')", params![s]).unwrap();
        }
        let pinned = db.pins_add(&[&a, &b], "reader").unwrap();
        let before = db.owner_rows().unwrap();
        // remove_rows visits the last state first: fail after it has already removed b.
        db.conn
            .execute_batch(&format!(
                "CREATE TRIGGER refuse_pin_release BEFORE DELETE ON owners WHEN OLD.state='{a}' AND OLD.kind='pin'
             BEGIN SELECT RAISE(ABORT, 'test release failure'); END;"
            ))
            .unwrap();
        assert!(db.pins_remove(&pinned, "reader").is_err());
        assert_eq!(db.owner_rows().unwrap(), before);
        db.conn.execute_batch("DROP TRIGGER refuse_pin_release").unwrap();
        db.pins_remove(&pinned, "reader").unwrap();
        assert!(!db.is_pinned(&a).unwrap());
        assert!(!db.is_pinned(&b).unwrap());
    }

    #[test]
    fn pin_batches_keep_duplicates_safe_and_release_last_owners() {
        let db = memory();
        let (a, b, unowned) = (id(1), id(2), id(3));
        for s in [&a, &b] {
            db.conn.execute("INSERT INTO states(id) VALUES(?1)", params![s]).unwrap();
            db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'head','line')", params![s]).unwrap();
        }
        let pinned = db.pins_add(&[&a, &b, &a, &unowned], "reader").unwrap();
        assert_eq!(pinned, vec![a.clone(), b.clone(), a.clone()]);
        assert_eq!(db.owner_rows().unwrap().iter().filter(|row| row.1 == "pin").count(), 2);
        assert!(db.reserve_trees(&[a.clone(), b.clone()], "gc").unwrap().is_empty());
        db.conn.execute("DELETE FROM owners WHERE state=?1 AND kind='head'", params![a]).unwrap();
        db.pins_remove(&pinned, "reader").unwrap();
        assert!(!db.is_pinned(&a).unwrap());
        assert!(!db.is_pinned(&b).unwrap());
        assert!(!db.is_owned(&a).unwrap());
        assert!(db.is_owned(&b).unwrap());
        assert_eq!(db.queued_frees().unwrap(), vec![a]);
    }

    #[test]
    fn a_reserved_tree_refuses_pins_and_a_pinned_tree_is_not_reserved() {
        let db = memory();
        let (a, b) = (id(1), id(2));
        for s in [&a, &b] {
            db.conn.execute("INSERT INTO owners(state, kind, holder) VALUES(?1,'store','cloud')", params![s]).unwrap();
        }
        assert_eq!(db.pins_add(&[&a], "1:reader").unwrap(), vec![a.clone()]);
        // A pinned state stays; the other is reserved and refuses a pin until the release.
        assert_eq!(db.reserve_trees(&[a.clone(), b.clone()], "2:gc").unwrap(), vec![b.clone()]);
        let refused = db.pins_add(&[&b], "1:reader").unwrap_err().to_string();
        assert!(refused.contains("read it again"), "{refused}");
        db.release_trees(&[b.clone()], &[b.clone()], "2:gc").unwrap();
        assert_eq!(
            db.pins_add(&[&b], "1:reader").unwrap(),
            vec![b],
            "after the release the absent state is pinned (fetched on demand)"
        );
        assert!(db.owner_rows().unwrap().iter().all(|r| r.1 != "drop"));
    }
}
