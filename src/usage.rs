//! A local count of the commands that run on this machine, to decide later which commands people
//! and agents use (issue #363, "Delete by actual use"). One row per day, command, subcommand,
//! caller and kind of caller (an agent sets `LAYR_SESSION`). The count stays in
//! `<store>/machine/usage.sqlite` and is never sent anywhere; `layr usage` prints it.
//!
//! A count never fails or slows a command: errors are ignored, the database waits at most 200 ms
//! for another writer, and a write keeps its journal in memory and does not wait for the disk
//! (0.3 ms in a debug build; a WAL database took 3.7 ms). A crash in a write can damage the file:
//! the next count starts a new one. The counts are a hint, not a record.

use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS usage (day TEXT, command TEXT, sub TEXT, uid INTEGER, agent INTEGER,
    runs INTEGER, failures INTEGER, ms INTEGER, PRIMARY KEY(day, command, sub, uid, agent))";

/// Commands whose first argument is a subcommand. Only a lowercase word is counted as one: never
/// a path, a revision or a name.
const GROUPS: &[&str] = &[
    "line", "park", "ref", "overlay", "remote", "git", "queue", "checks", "index", "release", "view", "change", "bisect",
    "retest", "op", "project", "volume", "person", "probe", "request", "requests", "access", "debug", "usage",
];

fn file(root: &Path) -> PathBuf {
    root.join("machine").join("usage.sqlite")
}

/// The command and subcommand that `argv` names, as counted.
fn names(argv: &[String]) -> Option<(String, String)> {
    let command = argv.first()?;
    if !crate::cmd::COMMANDS.contains(&command.as_str()) {
        return None;
    }
    let word =
        |s: &str| !s.is_empty() && s.len() <= 24 && s.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !s.starts_with('-');
    let sub = match argv.get(1) {
        Some(s) if GROUPS.contains(&command.as_str()) && word(s) => s.clone(),
        _ => String::new(),
    };
    Some((command.trim_start_matches('-').to_string(), sub))
}

/// Open the database; `create` also makes the file and its table (the first count).
fn open(root: &Path, create: bool) -> rusqlite::Result<Connection> {
    let conn = Connection::open(file(root))?;
    conn.busy_timeout(std::time::Duration::from_millis(200))?;
    conn.pragma_update(None, "journal_mode", "MEMORY")?;
    conn.pragma_update(None, "synchronous", "OFF")?;
    if create {
        conn.execute_batch(SCHEMA)?;
    }
    Ok(conn)
}

/// Count one run of `argv` by `uid` that exited with `code` after `ms` milliseconds.
pub fn count(root: &Path, argv: &[String], uid: u32, agent: bool, code: i32, ms: u64) {
    let Some((command, sub)) = names(argv) else { return };
    if !root.join("machine").is_dir() {
        return;
    }
    let day = crate::cmd::history::fmt_date(crate::ids::now_ms(), "short");
    let add = |conn: Connection| {
        conn.execute(
            "INSERT INTO usage(day, command, sub, uid, agent, runs, failures, ms) VALUES(?1,?2,?3,?4,?5,1,?6,?7)
             ON CONFLICT(day, command, sub, uid, agent) DO UPDATE SET runs=runs+1, failures=failures+?6, ms=ms+?7",
            params![day, command, sub, uid, agent as i64, (code != 0) as i64, ms as i64],
        )
    };
    // The table exists after the first count: one statement, no schema work.
    if open(root, false).and_then(add).is_err() {
        if let Err(e) = open(root, true).and_then(add) {
            // A damaged file (a crash in a write) is replaced; any other error loses this count only.
            if matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)) {
                let _ = std::fs::remove_file(file(root));
                let _ = open(root, true).and_then(add);
            }
        }
    }
}

/// `layr usage [--days <n>] [--json]`: the counted runs of the last `n` days (default 28), per
/// command and subcommand, most used first.
pub fn show(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--days").flag("--json"), args)?;
    if !a.pos.is_empty() {
        return Err(crate::cmd::exit(128, "usage: layr usage [--days <n>] [--json]"));
    }
    let days: i64 = a.get("--days").unwrap_or("28").parse().map_err(|_| crate::cmd::exit(128, "--days takes a number"))?;
    let since = crate::cmd::history::fmt_date(crate::ids::now_ms() - (days.max(1) - 1) * 86_400_000, "short");
    let rows: Vec<(String, String, i64, i64, i64, i64, i64)> = if file(&ctx.store.root).exists() {
        let conn = open(&ctx.store.root, true)?;
        let mut st = conn.prepare(
            "SELECT command, sub, SUM(runs), SUM(failures), SUM(CASE WHEN agent=1 THEN runs ELSE 0 END), COUNT(DISTINCT uid), SUM(ms)
             FROM usage WHERE day>=?1 GROUP BY command, sub ORDER BY SUM(runs) DESC, command, sub",
        )?;
        let v = st
            .query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    } else {
        Vec::new()
    };
    if a.has("--json") {
        let v: Vec<serde_json::Value> = rows
            .iter()
            .map(|(c, s, runs, failures, agent, users, ms)| {
                serde_json::json!({"command": c, "sub": s, "runs": runs, "failures": failures, "agent_runs": agent, "users": users, "ms": ms})
            })
            .collect();
        outln!(ctx, "{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    outln!(ctx, "{:<24} {:>7} {:>7} {:>7} {:>5}", format!("command (since {since})"), "runs", "agent", "failed", "users");
    for (c, s, runs, failures, agent, users, _) in &rows {
        let name = if s.is_empty() { c.clone() } else { format!("{c} {s}") };
        outln!(ctx, "{name:<24} {runs:>7} {agent:>7} {failures:>7} {users:>5}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_command_words_are_counted() {
        assert_eq!(names(&argv(&["line", "new", "x"])), Some(("line".into(), "new".into())));
        assert_eq!(names(&argv(&["save", "-m", "secret"])), Some(("save".into(), String::new())));
        assert_eq!(names(&argv(&["show", "main:secret.txt"])), Some(("show".into(), String::new())));
        assert_eq!(names(&argv(&["line", "list", "--json"])), Some(("line".into(), "list".into())));
        assert_eq!(names(&argv(&["view", "Main"])), Some(("view".into(), String::new())));
        assert_eq!(names(&argv(&["view", "/tmp/x"])), Some(("view".into(), String::new())));
        assert_eq!(names(&argv(&["--version"])), Some(("version".into(), String::new())));
        assert_eq!(names(&argv(&["no-such-command"])), None);
        assert_eq!(names(&argv(&[])), None);
    }

    #[test]
    fn runs_add_up_per_day_command_and_caller() {
        let root = std::env::temp_dir().join(format!("layr-usage-{}", crate::ids::new_id()));
        std::fs::create_dir_all(root.join("machine")).unwrap();
        count(&root, &argv(&["status"]), 501, false, 0, 3);
        count(&root, &argv(&["status"]), 501, false, 1, 4);
        count(&root, &argv(&["status"]), 502, true, 0, 5);
        count(&root, &argv(&["line", "new", "a"]), 501, true, 0, 9);
        let conn = open(&root, false).unwrap();
        let (runs, failures, ms): (i64, i64, i64) = conn
            .query_row("SELECT SUM(runs), SUM(failures), SUM(ms) FROM usage WHERE command='status'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((runs, failures, ms), (3, 1, 12));
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM usage WHERE command='status'", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
        assert_eq!(conn.query_row("SELECT sub FROM usage WHERE command='line'", [], |r| r.get::<_, String>(0)).unwrap(), "new");
        // A damaged file is replaced by the next count.
        std::fs::write(file(&root), b"not a database, but long enough to look like a damaged header....").unwrap();
        count(&root, &argv(&["status"]), 501, false, 0, 1);
        let conn = open(&root, false).unwrap();
        assert_eq!(conn.query_row("SELECT SUM(runs) FROM usage", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        // A store without its machine folder (not made yet) gets no file.
        let bare = root.join("bare");
        count(&bare, &argv(&["status"]), 501, false, 0, 1);
        assert!(!file(&bare).exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
