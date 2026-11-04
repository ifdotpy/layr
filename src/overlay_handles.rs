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
    read: bool,
    conn: Option<Rc<rusqlite::Connection>>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.pid != std::process::id() {
            if let Some(conn) = self.conn.take() { std::mem::forget(conn); }
        }
    }
}

thread_local! {
    static CACHE: RefCell<Option<HashMap<PathBuf, Entry>>> = const { RefCell::new(None) };
}

/// Starts a cache for one command. Dropping it releases every connection.
pub struct Command;
impl Command {
    pub fn begin() -> Self {
        CACHE.with(|cache| *cache.borrow_mut() = Some(HashMap::new()));
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
        if let Some(entry) = cache.get(path) {
            if entry.pid == std::process::id() {
                let conn = entry.conn.as_ref().context("missing overlay connection")?;
                if Rc::strong_count(conn) != 1 {
                    bail!("overlay database {} is already checked out", path.display());
                }
                if !conn.is_autocommit() {
                    bail!("overlay database {} has an open transaction", path.display());
                }
                if entry.identity == id && (read || !entry.read) {
                    return Ok(Some(Db { conn: conn.clone(), stale: false }));
                }
            }
        }
        cache.remove(path);
        Ok(None)
    })
}

pub fn insert_if_missing(path: &Path, read: bool, db: &Db) -> Result<()> {
    CACHE.with(|cache| {
        let mut cache = cache.try_borrow_mut().context("overlay handle cache already borrowed")?;
        if let Some(cache) = cache.as_mut() {
            if cache.contains_key(path) { return Ok(()); }
            cache.insert(path.to_path_buf(), Entry { pid: std::process::id(), identity: identity(path)?, read, conn: Some(db.conn.clone()) });
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_invalidates_cached_handle() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("layr-overlay-replaced-{}", crate::ids::new_id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("db");
        let replacement = dir.join("replacement");
        let command = Command::begin();
        let db = Db::open(&path)?;
        insert_if_missing(&path, false, &db)?;
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
        let entry = Entry { pid: std::process::id().wrapping_add(1), identity: (0, 0), read: false, conn: Some(conn.clone()) };
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
        insert_if_missing(&path, false, &db)?;
        assert!(get(&path, false).is_err());
        db.conn.execute_batch("BEGIN")?;
        drop(db);
        assert!(get(&path, false).is_err());
        CACHE.with(|cache| -> Result<()> {
            let cache = cache.borrow();
            let entry = cache.as_ref().and_then(|entries| entries.get(&path)).context("missing entry")?;
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
