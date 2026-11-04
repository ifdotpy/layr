//! Command-scoped overlay database leases.
use crate::db::Db;
use anyhow::{bail, Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct Entry {
    pid: u32,
    identity: (u64, u64),
    schema: (i64, i64),
    read: bool,
    used: u64,
    conn: Option<Rc<rusqlite::Connection>>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.pid != std::process::id() {
            if let Some(conn) = self.conn.take() { std::mem::forget(conn); }
        }
    }
}

struct Cache {
    entries: HashMap<PathBuf, Entry>,
    capacity: usize,
    clock: u64,
}

thread_local! {
    static CACHE: RefCell<Option<Cache>> = const { RefCell::new(None) };
}

/// Starts a cache for one command. Dropping it releases every connection.
pub struct Command;
impl Command {
    pub fn begin() -> Self {
        // SQLite may hold the database, WAL and shared-memory descriptors. A quarter of the
        // soft limit bounds cached descriptors while leaving room for files and active commands.
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current.unwrap_or(u64::MAX);
        let capacity = (limit / 4).clamp(1, 1024) as usize;
        Self::with_capacity(capacity)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        CACHE.with(|cache| *cache.borrow_mut() = Some(Cache { entries: HashMap::new(), capacity, clock: 0 }));
        Self
    }
}
pub fn active() -> bool { CACHE.with(|cache| cache.borrow().is_some()) }

impl Drop for Command {
    fn drop(&mut self) { CACHE.with(|cache| { cache.borrow_mut().take(); }); }
}

pub fn identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = path.metadata().with_context(|| format!("stat overlay database {}", path.display()))?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Check out the sole lease. The cache owns one Rc; all other references belong to the
/// currently checked-out Db, so its connection cannot be borrowed by another caller.
pub fn get(path: &Path, read: bool) -> Result<Option<Db>> {
    CACHE.with(|cache| {
        let mut cache = cache.try_borrow_mut().context("overlay handle cache already borrowed")?;
        let Some(cache) = cache.as_mut() else { return Ok(None) };
        let id = identity(path)?;
        cache.clock = cache.clock.wrapping_add(1);
        if let Some(entry) = cache.entries.get_mut(path) {
            if entry.pid == std::process::id() {
                let conn = entry.conn.as_ref().context("missing overlay connection")?;
                if Rc::strong_count(conn) != 1 {
                    bail!("overlay database {} is already checked out", path.display());
                }
                if !conn.is_autocommit() {
                    bail!("overlay database {} has an open transaction", path.display());
                }
                if entry.identity == id && (read || !entry.read)
                    && Db::cache_schema_cookie(conn)? == entry.schema && !Db::cache_has_legacy(conn)? {
                    entry.used = cache.clock;
                    return Ok(Some(Db { conn: conn.clone(), stale: false }));
                }
            }
        }
        cache.entries.remove(path);
        Ok(None)
    })
}

/// Make room before opening a database, when doing so may allocate several descriptors.
pub fn prepare_open() -> Result<()> {
    CACHE.with(|cache| {
        let mut cache = cache.try_borrow_mut().context("overlay handle cache already borrowed")?;
        let Some(cache) = cache.as_mut() else { return Ok(()) };
        while cache.entries.len() >= cache.capacity {
            let victim = cache.entries.iter()
                .filter(|(_, entry)| entry.pid != std::process::id()
                    || entry.conn.as_ref().is_some_and(|conn| Rc::strong_count(conn) == 1 && conn.is_autocommit()))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(path, _)| path.clone())
                .context("all overlay database handles are checked out or in a transaction")?;
            cache.entries.remove(&victim);
        }
        Ok(())
    })
}

pub fn insert_if_missing(path: &Path, identity: (u64, u64), schema: (i64, i64), read: bool, db: &Db) -> Result<()> {
    CACHE.with(|cache| {
        let mut cache = cache.try_borrow_mut().context("overlay handle cache already borrowed")?;
        if let Some(cache) = cache.as_mut() {
            if cache.entries.contains_key(path) { return Ok(()); }
            cache.clock = cache.clock.wrapping_add(1);
            cache.entries.insert(path.to_path_buf(), Entry { pid: std::process::id(), identity, schema, read, used: cache.clock, conn: Some(db.conn.clone()) });
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_capacity_evicts_idle_handles_and_reopens_with_same_data() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-overlay-lru-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let paths: Vec<_> = (0..3).map(|n| dir.join(format!("db{n}"))).collect();
        for (n, path) in paths.iter().enumerate() {
            let db = Db::open(path)?;
            db.conn.execute("INSERT INTO config(key,value,ord) VALUES ('test',?1,'1')", [n.to_string()])?;
        }
        let command = Command::with_capacity(2);
        for path in &paths {
            prepare_open()?;
            let db = Db::open(path)?;
            insert_if_missing(path, identity(path)?, Db::validated_cache_cookie(&db.conn)?, false, &db)?;
        }
        assert!(get(&paths[0], false)?.is_none());
        let checked_out = get(&paths[1], false)?.context("missing second db")?;
        prepare_open()?;
        assert!(get(&paths[1], false).is_err());
        let reopened = Db::open(&paths[0])?;
        insert_if_missing(&paths[0], identity(&paths[0])?, Db::validated_cache_cookie(&reopened.conn)?, false, &reopened)?;
        let value: String = reopened.conn.query_row("SELECT value FROM config WHERE key='test'", [], |r| r.get(0))?;
        assert_eq!(value, "0");
        drop((checked_out, reopened, command));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn schema_change_between_validation_and_insertion_is_not_cached_as_valid() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-schema-insert-race-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("db");
        let command = Command::with_capacity(2);
        let db = Db::open(&path)?;
        let validated = Db::validated_cache_cookie(&db.conn)?;
        let other = rusqlite::Connection::open(&path)?;
        other.pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1)?;
        drop(other);
        insert_if_missing(&path, identity(&path)?, validated, false, &db)?;
        drop(db);
        assert!(get(&path, false)?.is_none());
        let error = match Db::open_for(&path, false) {
            Ok(_) => anyhow::bail!("newer schema unexpectedly opened"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("newer layr"));
        drop(command);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn changed_schema_invalidates_the_cache_and_uses_normal_migration() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-overlay-schema-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("db");
        let command = Command::with_capacity(2);
        let db = Db::open(&path)?;
        insert_if_missing(&path, identity(&path)?, Db::validated_cache_cookie(&db.conn)?, false, &db)?;
        drop(db);
        let other = rusqlite::Connection::open(&path)?;
        other.pragma_update(None, "user_version", 0)?;
        drop(other);
        assert!(get(&path, false)?.is_none());
        let migrated = Db::open_for(&path, false)?;
        assert!(!migrated.stale);
        let version: i64 = migrated.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        assert_eq!(version, crate::db::SCHEMA_VERSION);
        insert_if_missing(&path, identity(&path)?, Db::validated_cache_cookie(&migrated.conn)?, false, &migrated)?;
        drop(migrated);
        let other = rusqlite::Connection::open(&path)?;
        other.execute_batch("DROP INDEX records_time")?;
        drop(other);
        assert!(get(&path, false)?.is_none());
        let repaired = Db::open_for(&path, false)?;
        let exists: bool = repaired.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='records_time')", [], |r| r.get(0))?;
        assert!(exists);
        drop((repaired, command));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn replacement_between_validation_and_insertion_keeps_the_validated_identity() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-overlay-insert-race-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("db");
        let replacement = dir.join("replacement");
        let command = Command::with_capacity(2);
        let db = Db::open(&path)?;
        let validated = identity(&path)?;
        let other = Db::open(&replacement)?;
        drop(other);
        std::fs::rename(&replacement, &path)?;
        insert_if_missing(&path, validated, Db::validated_cache_cookie(&db.conn)?, false, &db)?;
        drop(db);
        assert!(get(&path, false)?.is_none());
        drop(command);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn replacement_invalidates_cached_handle() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-overlay-replaced-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("db");
        let replacement = dir.join("replacement");
        let command = Command::begin();
        let db = Db::open(&path)?;
        insert_if_missing(&path, identity(&path)?, Db::validated_cache_cookie(&db.conn)?, false, &db)?;
        drop(db);
        let other = Db::open(&replacement)?;
        drop(other);
        std::fs::rename(&replacement, &path)?;
        assert!(get(&path, false)?.is_none());
        drop(command);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn inherited_entry_is_not_closed() -> Result<()> {
        let conn = Rc::new(rusqlite::Connection::open_in_memory()?);
        let entry = Entry { pid: std::process::id().wrapping_add(1), identity: (0, 0), schema: (0, 0), read: false, used: 0, conn: Some(conn.clone()) };
        drop(entry);
        assert_eq!(Rc::strong_count(&conn), 2);
        Ok(())
    }

    #[test]
    fn lease_rejects_nested_checkout_and_open_transaction() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-overlay-handles-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("db");
        let _command = Command::begin();
        let db = Db::open(&path)?;
        insert_if_missing(&path, identity(&path)?, Db::validated_cache_cookie(&db.conn)?, false, &db)?;
        assert!(get(&path, false).is_err());
        db.conn.execute_batch("BEGIN")?;
        drop(db);
        assert!(get(&path, false).is_err());
        CACHE.with(|cache| -> Result<()> {
            let cache = cache.borrow();
            let entry = cache.as_ref().and_then(|cache| cache.entries.get(&path)).context("missing entry")?;
            entry.conn.as_ref().context("missing connection")?.execute_batch("ROLLBACK")?;
            Ok(())
        })?;
        let db = get(&path, false)?.context("missing cached db")?;
        drop(db);
        drop(_command);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
