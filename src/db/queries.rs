//! Catalog queries shared by command and maintenance callers.
use super::{Db, Result};
use rusqlite::{params, OptionalExtension};

impl Db {
    pub fn project_id(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT json_extract(raw, '$.r.data.id') FROM records WHERE op='project' ORDER BY time LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn with_read_snapshot<T>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN")?;
        let result = read();
        let _ = self.conn.execute_batch("ROLLBACK");
        result
    }

    pub fn records_count(&self) -> Result<usize> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?)
    }

    pub fn integrity_check(&self) -> Result<String> {
        Ok(self.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?)
    }

    pub fn state_rows(&self) -> Result<Vec<(crate::model::StateRec, bool)>> {
        let mut stmt = self.conn.prepare_cached("SELECT rec, deleted FROM states")?;
        let rows =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(raw, deleted)| Ok((serde_json::from_str(&raw)?, deleted))).collect()
    }

    pub fn stored_eviction_candidates(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.id FROM states s WHERE s.present=1 AND s.deleted=0
         AND EXISTS (SELECT 1 FROM owners o WHERE o.state=s.id AND o.kind='store')
         AND NOT EXISTS (SELECT 1 FROM owners o WHERE o.state=s.id
            AND o.kind IN ('head','auto','undo','deleted','park','overlay','pin','keep'))",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn cache_eviction_candidates(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.id FROM states s WHERE s.present=1 AND s.deleted=0
                 AND NOT EXISTS (SELECT 1 FROM owners o WHERE o.state=s.id
                    AND o.kind IN ('auto','park','pin','keep','deleted'))",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    #[cfg(target_os = "macos")]
    pub fn deleted_owned_states(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT DISTINCT state FROM owners WHERE kind='deleted'")?;
        let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn foreign_line_head(&self, line: &str, local: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT head FROM line_heads WHERE line=?1 AND machine<>?2 ORDER BY time DESC, machine DESC LIMIT 1",
                params![line, local],
                |r| r.get(0),
            )
            .optional()?)
    }
}
