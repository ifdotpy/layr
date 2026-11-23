//! wait: block until a project changes on its Layr Cloud remote (docs/remotes.md, "Wait for a change").
//!
//! A machine job (a check runner, a follow, a release) runs `layr wait` before each pass instead of a
//! timer. The command long-polls the events of the project (25 seconds for each request) and ends
//! when an event comes. With `--cursor-file`, the cursor of the last answer stays in that file, so
//! a change during the pass of the caller ends the next wait at once.
use super::{exit, here};
use crate::args::{parse, Spec};
use crate::ctx::Ctx;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: layr wait [<remote>] [--cursor-file <path>] [--others] [--timeout <seconds>] [--json]";

/// What ends a wait.
#[derive(Debug, PartialEq)]
enum End {
    /// The cursor file had no cursor: the caller has not seen the project yet.
    Start,
    Change(usize),
    /// The server cannot prove that no event was lost: read the project in full.
    Resync,
    /// The server restarted (another epoch in the cursor): a new release of the server, for example.
    Restart,
    Timeout,
}

impl End {
    fn name(&self) -> &'static str {
        match self {
            End::Start => "start",
            End::Change(_) => "change",
            End::Resync => "resync",
            End::Restart => "restart",
            End::Timeout => "timeout",
        }
    }
}

/// A cursor of the events API is `<epoch>.<seq>.<mark>` (digits). Anything else in the file counts as no cursor.
fn valid_cursor(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 20 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The events of an answer that end the wait. With `others`, only a record segment that another machine
/// wrote counts: each change of a project writes one, and the passes of this machine (check results,
/// approvals) do not wake it again.
fn counted(answer: &Value, others: Option<&str>) -> usize {
    let events = answer["events"].as_array().map(Vec::as_slice).unwrap_or_default();
    match others {
        None => events.len(),
        Some(own) => {
            events.iter().filter(|e| e["kind"] == "record" && e["ids"]["writer"].as_str().is_some_and(|w| w != own)).count()
        }
    }
}

/// The epoch of a cursor: one name for each process lifetime of the server's event hub.
fn epoch(cursor: &str) -> &str {
    cursor.split('.').next().unwrap_or_default()
}

/// An error that a later request can end: no connection, or a server that is busy or restarts (5xx, 429).
fn transient(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    crate::store_remote::gateway::unreachable(e)
        || ["HTTP 429", "HTTP 500", "HTTP 502", "HTTP 503", "HTTP 504"].iter().any(|s| text.contains(s))
}

fn read_cursor(ctx: &Ctx, path: &Path) -> Result<Option<String>> {
    crate::privs::as_caller_fs(ctx, || match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_string()).filter(|c| valid_cursor(c))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    })
}

fn write_cursor(ctx: &Ctx, path: &Path, cursor: &str) -> Result<()> {
    crate::privs::as_caller_fs(ctx, || {
        let name = path.file_name().context("the cursor file needs a file name")?.to_string_lossy();
        let temporary = path.with_file_name(format!(".{name}.{}", std::process::id()));
        std::fs::write(&temporary, format!("{cursor}\n")).with_context(|| format!("write {}", temporary.display()))?;
        std::fs::rename(&temporary, path).with_context(|| format!("write {}", path.display()))
    })
}

pub fn wait(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().flag("--json").flag("--others").value("--cursor-file").value("--timeout"), args)?;
    if a.pos.len() > 1 {
        return Err(exit(128, USAGE));
    }
    let timeout = a
        .get("--timeout")
        .map(|t| t.parse::<u64>().map_err(|_| exit(128, "--timeout takes whole seconds (0: no limit)")))
        .transpose()?
        .filter(|&t| t > 0)
        .map(Duration::from_secs);
    let file: Option<PathBuf> = a.get("--cursor-file").map(|p| ctx.cwd.join(p));
    let h = here(ctx)?;
    let mut store = crate::store_remote::open_named(a.pos.first().map(String::as_str).unwrap_or("origin"), &h.repo)?;
    let backend = store.request_backend(ctx)?;
    let own = a.has("--others").then(|| ctx.store.machine.id.clone());
    let saved = match &file {
        Some(path) => read_cursor(ctx, path)?,
        None => None,
    };
    let (end, cursor) = match saved {
        Some(after) => {
            poll(backend, after, timeout, own.as_deref(), |c| file.as_deref().map_or(Ok(()), |f| write_cursor(ctx, f, c)))?
        }
        None => {
            // No cursor: the current one, from an answer that does not wait.
            let answer = backend.events(None, 0)?;
            let now = cursor_of(&answer)?;
            match &file {
                // The caller has not seen the project with this file yet: it reads the project now.
                Some(_) => (End::Start, now),
                None => poll(backend, now, timeout, own.as_deref(), |_| Ok(()))?,
            }
        }
    };
    if let Some(path) = &file {
        write_cursor(ctx, path, &cursor)?;
    }
    report(&end, &cursor, a.has("--json"))
}

fn cursor_of(answer: &Value) -> Result<String> {
    answer["cursor"].as_str().filter(|c| valid_cursor(c)).map(str::to_string).context("the events answer has no cursor")
}

/// Long-polls from `after` until a counted event, a resync or the timeout. `keep` saves each new cursor.
fn poll(
    backend: &crate::store_remote::backend::Backend,
    mut after: String,
    timeout: Option<Duration>,
    own: Option<&str>,
    keep: impl Fn(&str) -> Result<()>,
) -> Result<(End, String)> {
    let started = Instant::now();
    let (mut found, mut delay, mut reported) = (0, 5, String::new());
    loop {
        let left = timeout.map(|t| t.saturating_sub(started.elapsed()));
        if found == 0 && left.is_some_and(|l| l.is_zero()) {
            return Ok((End::Timeout, after));
        }
        // After a counted event, read the rest of a burst without waiting, then end.
        let seconds = if found > 0 { 0 } else { left.map_or(25, |l| l.as_secs().clamp(1, 25)) };
        let answer = match backend.events(Some(&after), seconds) {
            Ok(answer) => answer,
            Err(e) if transient(&e) => {
                let message = format!("{e:#}");
                if message != reported {
                    eprintln!("layr wait: {message}; trying again");
                    reported = message;
                }
                std::thread::sleep(Duration::from_secs(delay));
                delay = (delay * 2).min(60);
                continue;
            }
            Err(e) => return Err(e),
        };
        (delay, reported) = (5, String::new());
        let next = cursor_of(&answer)?;
        let restarted = epoch(&next) != epoch(&after);
        after = next;
        if answer["resync"] == true {
            return Ok((End::Resync, after));
        }
        if restarted {
            return Ok((End::Restart, after));
        }
        found += counted(&answer, own);
        keep(&after)?;
        if found > 0 && answer["more"] != true {
            return Ok((End::Change(found), after));
        }
    }
}

fn report(end: &End, cursor: &str, as_json: bool) -> Result<i32> {
    let events = if let End::Change(n) = end { *n } else { 0 };
    if as_json {
        println!("{}", json!({"reason": end.name(), "events": events, "cursor": cursor}));
    } else if events > 0 {
        println!("layr wait: change ({events} events)");
    } else {
        println!("layr wait: {}", end.name());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_is_three_numbers() {
        assert!(valid_cursor("1906678581563927610.4227.1795330731"));
        for bad in ["", "1.2", "1.2.3.4", "a.2.3", "1..3", "1.2.-3", "123456789012345678901.1.1"] {
            assert!(!valid_cursor(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_restart_is_another_epoch_and_a_busy_server_is_tried_again() {
        assert_eq!(epoch("19.4.17"), "19");
        assert_ne!(epoch("19.4.17"), epoch("20.0.17"));
        assert!(transient(&anyhow::anyhow!("gateway: HTTP 503")));
        assert!(transient(&anyhow::anyhow!("Layr Cloud events: HTTP 502 busy")));
        assert!(!transient(&anyhow::anyhow!("gateway: HTTP 404")));
    }

    #[test]
    fn others_counts_only_record_segments_of_other_machines() {
        let answer = json!({"events": [
            {"kind": "record", "ids": {"writer": "me"}},
            {"kind": "record", "ids": {"writer": "merge"}},
            {"kind": "ref", "ids": {"name": "main"}},
            {"kind": "state", "ids": {"state": "s"}},
        ]});
        assert_eq!(counted(&answer, None), 4);
        assert_eq!(counted(&answer, Some("me")), 1);
        assert_eq!(counted(&answer, Some("merge")), 1);
        assert_eq!(counted(&json!({"events": []}), Some("me")), 0);
    }
}
