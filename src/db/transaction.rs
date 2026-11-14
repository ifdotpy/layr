//! Catalog write boundaries and durable commit resolution.
use super::owners::Row;
use super::{retry_locked, Db};
use crate::model::Record;
use crate::store::LockedProject;
#[cfg(test)]
use anyhow::anyhow;
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeMap;

/// A reported failure is distinct from whether the transaction committed.
#[derive(Debug)]
#[must_use]
pub enum CommitResolution<T> {
    Committed(T),
    NotCommitted(anyhow::Error),
    Unknown(CommitRecovery),
}

/// The catalog could not establish the commit outcome. Prepared data must stay.
#[derive(Debug)]
pub struct CommitRecovery {
    cause: anyhow::Error,
}

impl std::fmt::Display for CommitRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; commit outcome is unknown; prepared data was retained for recovery", self.cause)
    }
}
impl std::error::Error for CommitRecovery {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub type PublishError = CommitResolution<anyhow::Error>;
pub type PublishResult<T> = std::result::Result<T, PublishError>;

impl<T> CommitResolution<T> {
    pub fn into_result(self) -> PublishResult<T> {
        match self {
            Self::Committed(value) => Ok(value),
            Self::NotCommitted(error) => Err(CommitResolution::NotCommitted(error)),
            Self::Unknown(recovery) => Err(CommitResolution::Unknown(recovery)),
        }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Committed(error) | Self::NotCommitted(error) => write!(f, "{error:#}"),
            Self::Unknown(recovery) => std::fmt::Display::fmt(recovery, f),
        }
    }
}
impl From<anyhow::Error> for PublishError {
    fn from(error: anyhow::Error) -> Self {
        if error.is::<CommitRecovery>() {
            match error.downcast::<CommitRecovery>() {
                Ok(recovery) => Self::Unknown(recovery),
                Err(error) => Self::NotCommitted(error),
            }
        } else if error.is::<ReportedCommit>() {
            match error.downcast::<ReportedCommit>() {
                Ok(committed) => Self::Committed(committed.0),
                Err(error) => Self::NotCommitted(error),
            }
        } else {
            Self::NotCommitted(error)
        }
    }
}
impl From<std::io::Error> for PublishError {
    fn from(error: std::io::Error) -> Self {
        Self::NotCommitted(error.into())
    }
}
impl From<rusqlite::Error> for PublishError {
    fn from(error: rusqlite::Error) -> Self {
        Self::NotCommitted(error.into())
    }
}
impl From<serde_json::Error> for PublishError {
    fn from(error: serde_json::Error) -> Self {
        Self::NotCommitted(error.into())
    }
}
impl From<PublishError> for anyhow::Error {
    fn from(error: PublishError) -> Self {
        match error {
            CommitResolution::Committed(cause) => Self::new(ReportedCommit(cause)),
            CommitResolution::NotCommitted(cause) => cause,
            CommitResolution::Unknown(recovery) => Self::new(recovery),
        }
    }
}

// Preserve a confirmed commit across general command helpers, just as CommitRecovery preserves Unknown.
#[derive(Debug)]
struct ReportedCommit(anyhow::Error);
impl std::fmt::Display for ReportedCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for ReportedCommit {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Debug)]
pub enum CleanupEffect {
    HeadMoved(String),
    HeadLeft(String),
    LineDeleted(String),
}

/// Only a confirmed commit can construct effects that the application may release.
#[derive(Debug)]
#[must_use]
pub struct CommittedEffects(Vec<CleanupEffect>);
impl IntoIterator for CommittedEffects {
    type Item = CleanupEffect;
    type IntoIter = std::vec::IntoIter<CleanupEffect>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[must_use]
pub struct CommittedWrite {
    pub effects: CommittedEffects,
    pub reported_error: Option<anyhow::Error>,
}

enum TransactionState {
    Active,
    Finished,
}
struct SqlTxn<'db> {
    db: &'db Db,
    state: TransactionState,
    effects: RefCell<Vec<CleanupEffect>>,
    witnesses: RefCell<Vec<String>>,
}
impl<'db> SqlTxn<'db> {
    fn begin(db: &'db Db) -> Result<Self> {
        retry_locked(|| db.conn.execute_batch("BEGIN IMMEDIATE"))?;
        Ok(Self { db, state: TransactionState::Active, effects: Default::default(), witnesses: Default::default() })
    }
    fn finish(mut self) -> CommitResolution<CommittedWrite> {
        #[allow(unused_mut)]
        let mut committed = self.db.conn.execute_batch("COMMIT").map_err(anyhow::Error::from);
        #[cfg(test)]
        if committed.is_ok() && super::FAIL_AFTER_COMMIT.with(|flag| flag.replace(false)) {
            committed = Err(anyhow!("injected: COMMIT reported an error after the commit point"));
        }
        self.state = TransactionState::Finished;
        let reported_error = match committed {
            Ok(()) => None,
            Err(error) => {
                if !self.db.conn.is_autocommit() {
                    if self.db.conn.execute_batch("ROLLBACK").is_ok() {
                        return CommitResolution::NotCommitted(error);
                    }
                    return CommitResolution::Unknown(CommitRecovery { cause: error });
                }
                #[cfg(test)]
                if FAIL_RECONCILE.with(|flag| flag.replace(false)) {
                    return CommitResolution::Unknown(CommitRecovery { cause: error });
                }
                let witnesses = self.witnesses.borrow();
                let observed = witnesses.iter().try_fold(0usize, |count, id| -> Result<usize> {
                    let present: bool =
                        self.db.conn.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE id=?1)", [id], |row| row.get(0))?;
                    Ok(count + usize::from(present))
                });
                match observed {
                    Ok(count) if !witnesses.is_empty() && count == witnesses.len() => Some(error),
                    Ok(0) if !witnesses.is_empty() => return CommitResolution::NotCommitted(error),
                    _ => return CommitResolution::Unknown(CommitRecovery { cause: error }),
                }
            }
        };
        CommitResolution::Committed(CommittedWrite {
            effects: CommittedEffects(std::mem::take(self.effects.get_mut())),
            reported_error,
        })
    }
}
impl Drop for SqlTxn<'_> {
    fn drop(&mut self) {
        if matches!(self.state, TransactionState::Active) {
            let _ = self.db.conn.execute_batch("ROLLBACK");
        }
    }
}

#[cfg(test)]
thread_local! { pub(crate) static FAIL_RECONCILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// A durable catalog transaction can only be obtained from the actual locked project.
#[must_use]
pub struct WriteTxn<'p> {
    sql: SqlTxn<'p>,
}
impl LockedProject<'_> {
    pub fn begin_write(&self) -> Result<WriteTxn<'_>> {
        Ok(WriteTxn { sql: SqlTxn::begin(self.catalog())? })
    }
}
impl WriteTxn<'_> {
    pub fn insert_record(&self, record: &Record, line: &str) -> Result<bool> {
        let inserted = self.sql.db.insert_record(record, line)?;
        if inserted {
            self.sql.witnesses.borrow_mut().push(record.id.clone());
        }
        Ok(inserted)
    }
    pub fn derive(&self, record: &Record, present: &dyn Fn(&str) -> bool, local: &str) -> Result<()> {
        self.sql.db.derive_rows(record, present, local)?;
        let mut effects = self.sql.effects.borrow_mut();
        for (id, change) in &record.lines {
            if let Some(after) = &change.after {
                effects.push(CleanupEffect::HeadMoved(after.head.clone()));
            }
            match (&change.before, &change.after) {
                (Some(before), Some(after)) if before.head != after.head => {
                    effects.push(CleanupEffect::HeadLeft(before.head.clone()))
                }
                (Some(before), None) => {
                    effects.push(CleanupEffect::HeadLeft(before.head.clone()));
                    effects.push(CleanupEffect::LineDeleted(id.clone()));
                }
                _ => {}
            }
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    pub fn record_base(&self, state: &str, base: &str) -> Result<()> {
        self.sql.db.record_base(state, base)
    }
    pub fn set_present(&self, state: &str, present: bool) -> Result<()> {
        self.sql.db.set_present(state, present)
    }
    pub fn apply_owners_diff(&self, missing: &[Row], extra: &[Row]) -> Result<()> {
        self.sql.db.apply_owners_diff(missing, extra)
    }
    pub fn fix_bases(&self, set: &BTreeMap<String, String>, gone: &[String]) -> Result<()> {
        self.sql.db.fix_bases(set, gone)
    }
    #[cfg(target_os = "macos")]
    pub fn replace_bases(&self, bases: &BTreeMap<String, String>) -> Result<()> {
        self.sql.db.replace_bases(bases)
    }
    pub fn dequeue_frees(&self, states: &[String]) -> Result<()> {
        self.sql.db.dequeue_frees(states)
    }
    pub fn free_old_leaks(&self, before_ms: i64) -> Result<usize> {
        self.sql.db.free_old_leaks(before_ms)
    }
    pub fn free_leaks(&self, states: &[String]) -> Result<()> {
        self.sql.db.free_leaks(states)
    }
    pub fn keep_set(&self, line: &str, name: &str, state: &str) -> Result<()> {
        self.sql.db.keep_set(line, name, state)
    }
    pub fn keep_drop(&self, line: &str, name: &str) -> Result<bool> {
        self.sql.db.keep_drop(line, name)
    }
    #[cfg(target_os = "macos")]
    pub fn forget_base(&self, state: &str) -> Result<()> {
        self.sql.db.forget_base(state)
    }
    pub fn rebuild(&self, only_older: bool, replay: impl FnOnce(&Self) -> Result<()>) -> Result<bool> {
        self.sql.db.rebuild_rows(only_older, || replay(self))
    }
    pub fn all_lines(&self) -> Result<Vec<String>> {
        self.sql.db.all_lines()
    }
    pub fn present_states(&self) -> Result<Vec<(String, bool)>> {
        self.sql.db.present_states()
    }
    pub fn fill_bases(&self, bases: &BTreeMap<String, String>) -> Result<()> {
        self.sql.db.fill_bases(bases)
    }
    pub fn finish(self) -> CommitResolution<CommittedWrite> {
        self.sql.finish()
    }
}

/// Reader pins and removal reservations finish independently of the project flock.
struct PinTxn<'db> {
    sql: SqlTxn<'db>,
}
impl Db {
    pub(super) fn with_pin_tx<T>(&self, change: impl FnOnce() -> Result<T>) -> Result<T> {
        let txn = PinTxn { sql: SqlTxn::begin(self)? };
        let value = change()?;
        let committed = txn.sql.finish().into_result().map_err(anyhow::Error::from)?;
        if let Some(error) = committed.reported_error {
            return Err(error);
        }
        Ok(value)
    }
}

#[cfg(test)]
pub(super) fn test_rebuild(db: &Db, only_older: bool, replay: impl FnOnce(&Db) -> Result<()>) -> Result<bool> {
    let txn = SqlTxn::begin(db)?;
    let rebuilt = db.rebuild_rows(only_older, || replay(db))?;
    let committed = txn.finish().into_result().map_err(anyhow::Error::from)?;
    if let Some(error) = committed.reported_error {
        return Err(error);
    }
    Ok(rebuilt)
}
