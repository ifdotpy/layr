//! wait: block until a project changes on its Layr Cloud remote (docs/remotes.md, "Wait for a change").
//!
//! A machine job (a check runner, a follow, a release) runs `layr wait` before each pass instead of a
//! timer. The command long-polls the events of the project (25 seconds for each request) and ends
//! when an event comes. With `--cursor-file`, the cursor of the last answer stays in that file, so
//! a change during the pass of the caller ends the next wait at once.
use super::{exit, here_with_git_follow};
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
fn transient(e: &anyhow::Error, deadline: Option<Instant>) -> bool {
    let status = e.downcast_ref::<crate::store_remote::api::ResponseError>().map(|e| e.status).or_else(|| {
        e.chain().find_map(|cause| {
            let message = cause.to_string();
            let (_, status) = message.split_once(": HTTP ")?;
            status.split_ascii_whitespace().next()?.parse().ok()
        })
    });
    crate::store_remote::gateway::unreachable(e)
        || (deadline.is_some()
            && e.chain()
                .any(|cause| cause.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)))
        || matches!(status, Some(429 | 500 | 502 | 503 | 504))
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
    let deadline = a
        .get("--timeout")
        .map(|t| t.parse::<u64>().map_err(|_| exit(128, "--timeout takes whole seconds (0: no limit)")))
        .transpose()?
        .filter(|&t| t > 0)
        .map(|seconds| {
            Instant::now().checked_add(Duration::from_secs(seconds)).ok_or_else(|| exit(128, "--timeout is too large"))
        })
        .transpose()?;
    let file: Option<PathBuf> = a.get("--cursor-file").map(|p| ctx.cwd.join(p));
    let as_json = a.has("--json");
    // A finite wait only needs the project. Following Git can block on the line lock.
    let h = here_with_git_follow(ctx, deadline.is_none())?;
    let mut store = crate::store_remote::open_named(a.pos.first().map(String::as_str).unwrap_or("origin"), &h.repo)?;
    if let Some(deadline) = deadline {
        store.set_wait_deadline(deadline)?;
    }
    let Some(()) = initialize(deadline, || store.prepare_requests(ctx))? else {
        return report(&End::Timeout, None, as_json);
    };
    let backend = store.backend();
    let own = a.has("--others").then(|| &*ctx.store.machine.id);
    let keep = |c: &str| file.as_deref().map_or(Ok(()), |f| write_cursor(ctx, f, c));
    let events = |after: &str, seconds| backend.events(Some(after), seconds);
    let saved = file.as_deref().map(|path| read_cursor(ctx, path)).transpose()?.flatten();
    let (end, cursor) = match saved {
        Some(after) => poll(events, after, deadline, own, keep)?,
        None => {
            // No cursor: fetch the current one without long-polling.
            let Some(answer) = initialize(deadline, || backend.events(None, 0))? else {
                return report(&End::Timeout, None, as_json);
            };
            let now = cursor_of(&answer)?;
            if file.is_some() {
                (End::Start, now)
            } else {
                poll(events, now, deadline, own, keep)?
            }
        }
    };
    keep(&cursor)?;
    report(&end, Some(&cursor), as_json)
}

fn cursor_of(answer: &Value) -> Result<String> {
    answer["cursor"].as_str().filter(|c| valid_cursor(c)).map(str::to_string).context("the events answer has no cursor")
}

/// Preserve a single setup attempt for unlimited waits.
fn initialize<T>(deadline: Option<Instant>, mut request: impl FnMut() -> Result<T>) -> Result<Option<T>> {
    let Some(limit) = deadline else { return request().map(Some) };
    Ok(retry_request(deadline, |_| request())?.filter(|_| Instant::now() < limit))
}

fn retry_request<T>(deadline: Option<Instant>, mut request: impl FnMut(Option<Duration>) -> Result<T>) -> Result<Option<T>> {
    let remaining = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
    let (mut delay, mut reported) = (5, String::new());
    loop {
        let left = remaining();
        if left == Some(Duration::ZERO) {
            return Ok(None);
        }
        match request(left) {
            Ok(answer) => return Ok(Some(answer)),
            Err(e) if transient(&e, deadline) => {
                let backoff = Duration::from_secs(delay);
                let sleep = remaining().map_or(backoff, |left| backoff.min(left));
                if sleep.is_zero() {
                    continue;
                }
                let message = format!("{e:#}");
                if message != reported {
                    eprintln!("layr wait: {message}; trying again");
                    reported = message;
                }
                std::thread::sleep(sleep);
                delay = (delay * 2).min(60);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Long-polls from `after` until a counted event, a resync or the timeout. `keep` checkpoints each reply.
fn poll(
    mut events: impl FnMut(&str, u64) -> Result<Value>,
    mut after: String,
    deadline: Option<Instant>,
    own: Option<&str>,
    mut keep: impl FnMut(&str) -> Result<()>,
) -> Result<(End, String)> {
    let mut found = 0;
    loop {
        let answer = retry_request(deadline, |left| {
            // After a counted event, read the rest of a burst without waiting, then end.
            let seconds = if found > 0 { 0 } else { left.map_or(25, |l| l.as_secs().clamp(1, 25)) };
            events(&after, seconds)
        })?;
        let Some(answer) = answer else {
            return Ok((if found == 0 { End::Timeout } else { End::Change(found) }, after));
        };
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

fn report(end: &End, cursor: Option<&str>, as_json: bool) -> Result<i32> {
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
        assert!(transient(&anyhow::anyhow!("gateway: HTTP 503"), None));
        assert!(transient(&anyhow::anyhow!("Layr Cloud events: HTTP 502 busy"), None));
        assert!(!transient(&anyhow::anyhow!("gateway: HTTP 404"), None));
        assert!(!transient(&anyhow::anyhow!("gateway: HTTP 401 (previous HTTP 503)"), Some(Instant::now())));
    }

    #[test]
    fn a_busy_server_cannot_extend_a_wait_past_its_deadline() -> Result<()> {
        let budget = Duration::from_millis(100);
        let started = Instant::now();
        let (end, cursor) = poll(
            |_, _| Err(anyhow::anyhow!("gateway: HTTP 503")),
            "1.1.1".into(),
            Some(started + budget),
            None,
            |_| anyhow::bail!("the cursor did not advance"),
        )?;
        assert_eq!((end, cursor.as_str()), (End::Timeout, "1.1.1"));
        assert!(started.elapsed() < Duration::from_secs(2), "elapsed: {:?}", started.elapsed());
        Ok(())
    }

    #[test]
    fn a_body_timeout_is_transient_only_with_a_deadline() {
        let error = std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled body").into();
        assert!(!transient(&error, None));
        assert!(transient(&error, Some(Instant::now())));
    }

    #[test]
    fn a_wait_reports_counted_events_if_the_deadline_interrupts_the_burst() -> Result<()> {
        let answers = [
            Ok(json!({"cursor": "1.1.1", "events": []})),
            Ok(json!({"cursor": "1.2.1", "events": [{}], "more": true})),
            Err(anyhow::anyhow!("gateway: HTTP 503")),
        ];
        let mut answers = answers.into_iter();
        let mut saved = Vec::new();
        let (end, cursor) = poll(
            |_, _| answers.next().context("three polls before the deadline")?,
            "1.1.1".into(),
            Some(Instant::now() + Duration::from_millis(100)),
            None,
            |cursor| {
                saved.push(cursor.to_owned());
                Ok(())
            },
        )?;
        assert_eq!((end, cursor.as_str()), (End::Change(1), "1.2.1"));
        assert_eq!(saved, ["1.1.1", "1.2.1"]);
        Ok(())
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
