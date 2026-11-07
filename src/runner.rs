//! A check runner joined to a Layr Cloud gateway (docs/checks.md, "Cloud runners").
//!
//! `layr checks join <url> --code <code>` registers this store's machine key with the gateway
//! (a one-time join code from the site), keeps the runner token in `<store>/runner/token` (0600)
//! and the settings in `<store>/runner/config.json`. The layr daemon then keeps one connection to
//! the gateway ([`serve`]): a WebSocket that the gateway uses to push work. Work names only a
//! project: the runner fetches it and runs `layr checks watch --once`, so the commands come from
//! the project's own check settings and the executor, sandbox, attestations and save-back are the
//! ones of `checks watch`. The runner never runs a command that the gateway sends.
//!
//! The connection pings every 20 seconds, reconnects with backoff and sends its cursor (the
//! gateway epoch and the last event sequence) and the project that it still checks, so the gateway
//! resumes without losing work. The gateway gives one runner per project and platform a scan at a
//! time, so two runners never run the same check at once.

use crate::ctx::Ctx;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The folder of the runner settings in the store.
pub const DIR: &str = "runner";
/// The platform that a join code binds today.
pub const PLATFORM: &str = "macos-aarch64";
const PING: Duration = Duration::from_secs(20);
/// Renew the token when it has less than this left (tokens live 24 hours).
const RENEW_BEFORE_MS: u64 = 2 * 3_600_000;
const MAX_BACKOFF: u64 = 60;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The gateway, `https://host` (or `http://127.0.0.1:<port>` in tests).
    pub url: String,
    pub runner: String,
    pub projects: Vec<String>,
    pub platform: String,
}

/// The resume point of the connection: the gateway epoch and the last event sequence.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

pub fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

pub fn config(root: &Path) -> Result<Option<Config>> {
    match std::fs::read(dir(root).join("config.json")) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data).context("invalid runner config")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `https://host` or a loopback `http://127.0.0.1:<port>` / `http://[::1]:<port>` (tests).
fn gateway_url(url: &str) -> Result<String> {
    let url = url.trim_end_matches('/');
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://").filter(|h| h.starts_with("127.0.0.1:") || h.starts_with("[::1]:")))
        .context("the gateway URL must be https://<host>")?;
    if host.is_empty() || host.contains(['/', '?', '#', '@']) {
        bail!("the gateway URL must be https://<host> with no path");
    }
    Ok(url.to_string())
}

fn ws_url(url: &str) -> String {
    match url.strip_prefix("https://") {
        Some(host) => format!("wss://{host}/v1/runners/connect"),
        None => format!("ws://{}/v1/runners/connect", url.trim_start_matches("http://")),
    }
}

fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

fn post(url: &str, path: &str, body: &Value) -> Result<Value> {
    let response = http()?.post(format!("{url}{path}")).json(body).send()?;
    let status = response.status();
    let value: Value = response.json().unwrap_or(json!({}));
    if !status.is_success() {
        bail!("the gateway refused ({status}): {}", value["error"].as_str().unwrap_or("no reason"));
    }
    Ok(value)
}

/// The expiry (ms) in a runner token `run1.<runner>.<expires>.<nonce>.<mac>`.
pub fn token_expiry(token: &str) -> Option<u64> {
    token.strip_prefix("run1.")?.split('.').nth(1)?.parse().ok()
}

fn token_ok(token: &str) -> bool {
    (1..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    crate::fsutil::atomic_write(path, data, 0o600)
}

fn read_token(root: &Path) -> Result<String> {
    let token = std::fs::read_to_string(dir(root).join("token"))?.trim().to_string();
    if !token_ok(&token) {
        bail!("invalid runner token file");
    }
    Ok(token)
}

/// Renew the token with a signature of the machine key when it ends within [`RENEW_BEFORE_MS`].
pub fn renew(root: &Path, config: &Config, key: &ed25519_dalek::SigningKey, force: bool) -> Result<()> {
    let now = crate::ids::now_ms() as u64;
    let left = read_token(root).ok().and_then(|t| token_expiry(&t)).map(|e| e.saturating_sub(now)).unwrap_or(0);
    if !force && left > RENEW_BEFORE_MS {
        return Ok(());
    }
    let text = format!("layr-runner-renew-v1\n{}\n{now}", config.runner);
    let signature = layr_store::keys::sign_bytes(text.as_bytes(), key);
    let reply = post(&config.url, "/v1/runners/renew", &json!({"runner": config.runner, "time": now, "signature": signature}))?;
    let token = reply["grant"].as_str().filter(|t| token_ok(t)).context("the gateway sent no runner token")?;
    write_private(&dir(root).join("token"), format!("{token}\n").as_bytes())
}

const JOIN_USAGE: &str =
    "usage: layr checks join <gateway url> --code <join code> --project <a,b> [--name <runner name>] [--json]";

/// The projects that the person at this Mac chose: `--project a,b` (required).
fn chosen_projects(value: Option<&str>) -> Result<Vec<String>> {
    let mut projects: Vec<String> =
        value.unwrap_or("").split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect();
    projects.sort();
    projects.dedup();
    if projects.is_empty() {
        return Err(crate::cmd::exit(128, JOIN_USAGE));
    }
    for project in &projects {
        crate::store::check_name("project", project)?;
    }
    Ok(projects)
}

/// The gateway's reply may name only projects that this Mac chose.
fn accept_projects(chosen: &[String], reply: &[String]) -> Result<()> {
    if let Some(other) = reply.iter().find(|p| !chosen.contains(p)) {
        bail!("the gateway named project {other}, which this Mac did not choose (--project); nothing was saved");
    }
    if reply.is_empty() {
        bail!("the gateway named no project; nothing was saved");
    }
    Ok(())
}

/// `layr checks join <url> --code <code>`.
pub fn join(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a =
        crate::args::parse(&crate::args::Spec::new().value("--code").value("--project").value("--name").flag("--json"), args)?;
    let (Some(url), Some(code), 1) = (a.pos.first(), a.get("--code"), a.pos.len()) else {
        return Err(crate::cmd::exit(128, JOIN_USAGE));
    };
    if ctx.caller.is_root() {
        bail!("join as the user who runs the checks, not as root");
    }
    if crate::cmd::checks::platform() != PLATFORM {
        bail!("a join code binds {PLATFORM}; this machine is {}", crate::cmd::checks::platform());
    }
    let url = gateway_url(url)?;
    let chosen = chosen_projects(a.get("--project"))?;
    let root = ctx.store.root.clone();
    let folder = dir(&root);
    if let Some(existing) = config(&root)? {
        bail!(
            "this store is already runner {} of {}; remove {} first to join again",
            existing.runner,
            existing.url,
            folder.display()
        );
    }
    let machine = &ctx.store.machine;
    // The gateway takes host-name characters only.
    let name: String = a
        .get("--name")
        .unwrap_or(&machine.name)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '-' })
        .take(64)
        .collect();
    let reply = post(
        &url,
        "/v1/runners/join",
        &json!({
            "code": code,
            "machine": machine.id,
            "public_key": hex::encode(machine.key.verifying_key().to_bytes()),
            "name": name,
            "platform": PLATFORM,
            "projects": chosen,
        }),
    )?;
    let runner = reply["runner"].as_str().context("the gateway sent no runner id")?;
    let token = reply["grant"].as_str().filter(|t| token_ok(t)).context("the gateway sent no runner token")?;
    let projects: Vec<String> = serde_json::from_value(reply["projects"].clone()).context("the gateway sent no projects")?;
    accept_projects(&chosen, &projects)?;
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&folder)?;
    }
    write_private(&folder.join("token"), format!("{token}\n").as_bytes())?;
    let config = Config { url: url.clone(), runner: runner.to_string(), projects: projects.clone(), platform: PLATFORM.into() };
    write_private(&folder.join("config.json"), &serde_json::to_vec_pretty(&config)?)?;
    let daemon = install_daemon(&root)?;
    if a.has("--json") {
        crate::outln!(
            ctx,
            "{}",
            json!({"runner": runner, "url": url, "projects": projects, "machine": machine.id, "daemon": daemon})
        );
    } else {
        crate::outln!(ctx, "Joined {url} as runner {runner} (machine {} \"{name}\") for {}.", machine.id, projects.join(", "));
        crate::outln!(ctx, "Daemon: {daemon}.");
        crate::outln!(
            ctx,
            "Next: a gateway administrator clicks \"Trust this runner\" on the site; the owner's machine then trusts it."
        );
        crate::outln!(ctx, "Until then the runner waits. Check results count only from a trusted machine.");
        crate::outln!(ctx, "A project may need one setup step with sudo; the site shows it in the runner's last result.");
    }
    Ok(0)
}

/// Install and start the daemon LaunchAgent (`ws.layr.daemon`) of this user when it is missing.
fn install_daemon(root: &Path) -> Result<String> {
    if !cfg!(target_os = "macos") {
        return Ok("start `layr daemon` as the store user".into());
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let plist = home.join("Library/LaunchAgents/ws.layr.daemon.plist");
    if plist.exists() {
        let text = std::fs::read_to_string(&plist)?;
        if !text.contains(&*root.to_string_lossy()) {
            return Ok(format!("{} uses another store; set its LAYR_ROOT to {}", plist.display(), root.display()));
        }
        return Ok(format!("{} runs; it starts the runner within a minute", plist.display()));
    }
    let template = include_str!("../packaging/launchd/ws.layr.daemon.plist");
    let escaped = root.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let text = template.replace("/Users/you/Library/Application Support/layr", &escaped);
    std::fs::create_dir_all(plist.parent().expect("LaunchAgents folder"))?;
    std::fs::create_dir_all(home.join("Library/Logs"))?;
    std::fs::write(&plist, text)?;
    let uid = rustix::process::getuid().as_raw();
    let status = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(&plist)
        .status()
        .context("launchctl")?;
    if !status.success() {
        bail!("launchctl bootstrap {} failed; start it by hand", plist.display());
    }
    Ok(format!("installed {} (log ~/Library/Logs/layr-daemon.log)", plist.display()))
}

/// The work of one project: clone once, fetch, run the pending checks, push the results.
/// Returns the report for the gateway: {status, work_done, count, summary}.
pub fn run_project(root: &Path, config: &Config, project: &str) -> Value {
    match run_project_inner(root, config, project) {
        Ok(report) => report,
        Err(e) => json!({"status": "error", "summary": format!("{e:#}").chars().take(500).collect::<String>()}),
    }
}

fn layr(root: &Path, args: &[&str]) -> Result<(i32, String)> {
    let exe = std::env::current_exe()?;
    let home = std::env::var("HOME").unwrap_or_default();
    let output = std::process::Command::new(exe)
        .args(args)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", format!("{home}/.cargo/bin:{home}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin"))
        .env("LAYR_ROOT", root)
        .env("LAYR_GATEWAY_TOKEN_FILE", dir(root).join("token"))
        .stdin(std::process::Stdio::null())
        .output()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.code().unwrap_or(-1), text))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" | ").chars().take(500).collect()
}

fn run_project_inner(root: &Path, config: &Config, project: &str) -> Result<Value> {
    if !config.projects.iter().any(|p| p == project) {
        bail!("the gateway sent work of project {project}, which this runner does not serve");
    }
    let store = crate::store::Store::open(root)?;
    if !store.project_names()?.iter().any(|p| p == project) {
        let url = format!("layr+{}/{project}", config.url);
        let (code, out) = layr(root, &["clone", &url, project, "--heads", "--caches-of", "main", "-q"])?;
        if code != 0 && code != 2 {
            // A private project opens only after the owner trusts this machine (its machine slot).
            return Ok(json!({"status": "waiting", "summary": format!("clone: {}", tail(&out))}));
        }
        let (code, out) = layr(root, &["--project", project, "config", "--local", "checks.runner", "true"])?;
        if code != 0 {
            bail!("config --local checks.runner true: {}", tail(&out));
        }
    } else {
        let (code, out) = layr(root, &["--project", project, "fetch", "origin"])?;
        if code != 0 {
            return Ok(json!({"status": "waiting", "summary": format!("fetch: {}", tail(&out))}));
        }
    }
    let (code, out) = layr(root, &["--project", project, "checks", "watch", "--once", "--json"])?;
    // Exit 1: a check failed; its result is pushed like a pass.
    if code > 1 {
        bail!("checks watch: {}", tail(&out));
    }
    let result: Value = out.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap_or(json!({}));
    let work_done = result["work_done"] == true;
    if work_done {
        // The results first, so a failing cache upload never holds back a merge.
        let (push, out) = layr(root, &["--project", project, "push", "origin"])?;
        if push != 0 {
            bail!("push: {}", tail(&out));
        }
        let _ = layr(root, &["--project", project, "push", "origin", "--caches"]);
    }
    let checks = result["checks"].as_array().cloned().unwrap_or_default();
    let summary: Vec<String> = checks
        .iter()
        .map(|c| {
            let state: String = c["state"].as_str().unwrap_or("").chars().take(8).collect();
            format!("{} {state} exit {}", c["name"].as_str().unwrap_or("?"), c["exit"])
        })
        .collect();
    Ok(json!({
        "status": if code == 1 { "fail" } else { "pass" },
        "work_done": work_done,
        "count": checks.len(),
        "pending": result["pending"],
        "summary": if summary.is_empty() { "no pending check".to_string() } else { summary.join(", ") },
    }))
}

/// How one connection ended.
#[derive(Debug, PartialEq)]
pub enum End {
    /// The token ended or the gateway refused it: renew, then reconnect.
    Renew,
    /// The runner was removed: stop until the settings change.
    Removed,
    /// Any other end: reconnect after the backoff.
    Lost(String),
    /// The daemon stops.
    Stop,
}

/// The state that outlives one connection.
pub struct Session<F> {
    pub cursor: Cursor,
    /// The running scan: project, event sequence and the task.
    job: Option<(String, u64, tokio::task::JoinHandle<Value>)>,
    /// Runs the work of one project (blocking). Tests replace it.
    pub work: F,
    pub stop: fn() -> bool,
}

impl<F: Fn(&str) -> Value + Send + Sync + Clone + 'static> Session<F> {
    pub fn new(work: F, stop: fn() -> bool) -> Self {
        Session { cursor: Cursor::default(), job: None, work, stop }
    }

    /// One connection: hello, then work, done reports and pings until it ends.
    pub async fn connect(&mut self, url: &str, token: &str, version: &str) -> End {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let mut request = match ws_url(url).into_client_request() {
            Ok(r) => r,
            Err(e) => return End::Lost(format!("{e}")),
        };
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().expect("token is ASCII"));
        let socket = tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::connect_async(request)).await;
        let mut ws = match socket {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                return match response.status().as_u16() {
                    401 => End::Renew,
                    403 => End::Removed,
                    status => End::Lost(format!("HTTP {status}")),
                };
            }
            Ok(Err(e)) => return End::Lost(format!("{e}")),
            Err(_) => return End::Lost("connect timed out".into()),
        };
        let hello = json!({
            "type": "hello",
            "cursor": self.cursor,
            "busy": self.job.as_ref().map(|(p, _, _)| p.clone()),
            "version": version,
        });
        if ws.send(Message::text(hello.to_string())).await.is_err() {
            return End::Lost("hello failed".into());
        }
        let mut ping = tokio::time::interval(PING);
        ping.tick().await;
        let mut stop = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                frame = ws.next() => {
                    let text = match frame {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(frame))) => {
                            let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(1005);
                            return match code {
                                4001 => End::Renew,
                                4003 => End::Removed,
                                _ => End::Lost(format!("closed {code}")),
                            };
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return End::Lost(format!("{e}")),
                        None => return End::Lost("connection ended".into()),
                    };
                    let Ok(message) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                    match message["type"].as_str() {
                        Some("welcome") => {
                            self.cursor.epoch = message["epoch"].as_str().unwrap_or("").to_string();
                        }
                        Some("work") => {
                            let project = message["project"].as_str().unwrap_or("").to_string();
                            let seq = message["seq"].as_u64().unwrap_or(0);
                            let ack = json!({"type": "ack", "seq": seq, "project": project});
                            if ws.send(Message::text(ack.to_string())).await.is_err() {
                                return End::Lost("ack failed".into());
                            }
                            if self.job.is_none() && !project.is_empty() {
                                let work = self.work.clone();
                                let name = project.clone();
                                let task = tokio::task::spawn_blocking(move || work(&name));
                                self.job = Some((project, seq, task));
                            }
                        }
                        _ => {}
                    }
                }
                report = async { (&mut self.job.as_mut().expect("job").2).await }, if self.job.is_some() => {
                    let (project, seq, _) = self.job.take().expect("job");
                    let report = report.unwrap_or_else(|e| json!({"status": "error", "summary": format!("{e}")}));
                    let done = json!({"type": "done", "project": project, "seq": seq, "report": report});
                    if ws.send(Message::text(done.to_string())).await.is_err() {
                        return End::Lost("done failed".into());
                    }
                    self.cursor.seq = self.cursor.seq.max(seq);
                }
                _ = ping.tick() => {
                    if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return End::Lost("ping failed".into());
                    }
                }
                _ = stop.tick() => {
                    if (self.stop)() {
                        let _ = ws.close(None).await;
                        return End::Stop;
                    }
                }
            }
        }
    }
}

/// The runner loop of the daemon: runs while the store has runner settings and the daemon lives.
pub fn serve(root: &Path) -> Result<()> {
    let Some(config) = config(root)? else {
        return Ok(());
    };
    let store = crate::store::Store::open(root)?;
    let key = store.machine.key.clone();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let version = format!("layr {}", env!("CARGO_PKG_VERSION"));
    let work_root = root.to_path_buf();
    let work_config = config.clone();
    let work = move |project: &str| run_project(&work_root, &work_config, project);
    let mut session = Session::new(work, crate::sys::terminate_requested);
    let cursor_path = dir(root).join("cursor.json");
    if let Ok(data) = std::fs::read(&cursor_path) {
        session.cursor = serde_json::from_slice(&data).unwrap_or_default();
    }
    let mut backoff = 1u64;
    loop {
        if crate::sys::terminate_requested() {
            return Ok(());
        }
        if let Err(e) = renew(root, &config, &key, false) {
            eprintln!("layr runner: renewal failed: {e:#}");
        }
        let token = read_token(root)?;
        let started = std::time::Instant::now();
        let end = runtime.block_on(session.connect(&config.url, &token, &version));
        let _ = write_private(&cursor_path, &serde_json::to_vec(&session.cursor)?);
        match end {
            End::Stop => return Ok(()),
            End::Renew => {
                if let Err(e) = renew(root, &config, &key, true) {
                    eprintln!("layr runner: renewal failed: {e:#}");
                }
            }
            End::Removed => {
                eprintln!("layr runner: the gateway removed runner {}; remove {} to stop", config.runner, dir(root).display());
                // Wait for new settings (a new join) or the stop.
                for _ in 0..300 {
                    if crate::sys::terminate_requested() {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                return Ok(());
            }
            End::Lost(reason) => eprintln!("layr runner: connection lost: {reason}"),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        let jitter = rand::random::<u64>() % 1000;
        let wait = Duration::from_millis(backoff * 1000 + jitter);
        let until = std::time::Instant::now() + wait;
        while std::time::Instant::now() < until {
            if crate::sys::terminate_requested() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn urls_and_tokens() {
        assert_eq!(gateway_url("https://cloud.layr.ws/").unwrap(), "https://cloud.layr.ws");
        assert!(gateway_url("http://cloud.layr.ws").is_err());
        assert!(gateway_url("https://cloud.layr.ws/x").is_err());
        assert!(gateway_url("https://user@cloud.layr.ws").is_err());
        assert_eq!(ws_url("https://cloud.layr.ws"), "wss://cloud.layr.ws/v1/runners/connect");
        assert_eq!(ws_url("http://127.0.0.1:9"), "ws://127.0.0.1:9/v1/runners/connect");
        assert_eq!(token_expiry(&format!("run1.{}.1794000000000.{}.ab", "a".repeat(32), "b".repeat(32))), Some(1794000000000));
        assert!(!token_ok("run1 x"));
        assert_eq!(chosen_projects(Some("b, a,a")).unwrap(), ["a", "b"]);
        assert!(chosen_projects(None).is_err());
        assert!(chosen_projects(Some(",")).is_err());
        assert!(chosen_projects(Some("../x")).is_err());
        let chosen = vec!["a".to_string(), "b".to_string()];
        assert!(accept_projects(&chosen, &["a".into()]).is_ok());
        assert!(accept_projects(&chosen, &["a".into(), "c".into()]).is_err(), "a project the Mac did not choose");
        assert!(accept_projects(&chosen, &[]).is_err());
    }

    /// A fake gateway: each accepted connection runs `script` with the socket and the hello.
    #[allow(clippy::result_large_err)]
    async fn fake<S, Fut>(script: S) -> (String, tokio::task::JoinHandle<()>)
    where
        S: Fn(usize, tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, Value) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for n in 0..usize::MAX {
                let (stream, _) = listener.accept().await.unwrap();
                let mut auth = String::new();
                let callback =
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        auth = request.headers().get("authorization").unwrap().to_str().unwrap().to_string();
                        if auth == "Bearer expired" {
                            let mut refused = tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(None);
                            *refused.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
                            return Err(refused);
                        }
                        Ok(response)
                    };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else { continue };
                let hello = match ws.next().await {
                    Some(Ok(Message::Text(t))) => serde_json::from_str(t.as_str()).unwrap(),
                    _ => continue,
                };
                script(n, ws, hello).await;
            }
        });
        (url, task)
    }

    async fn next_json(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Value {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
                _ => continue,
            }
        }
    }

    fn never() -> bool {
        false
    }

    #[tokio::test]
    async fn work_runs_the_project_and_reports_done_with_the_cursor() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let (url, _server) = fake(move |_, mut ws, hello| {
            let log = log.clone();
            async move {
                assert_eq!(hello["type"], "hello");
                ws.send(Message::text(json!({"type": "welcome", "epoch": "e1", "seq": 4}).to_string())).await.unwrap();
                ws.send(Message::text(json!({"type": "work", "project": "p", "seq": 5}).to_string())).await.unwrap();
                let ack = next_json(&mut ws).await;
                assert_eq!(ack["type"], "ack");
                let done = next_json(&mut ws).await;
                log.lock().unwrap().push(done);
                let close = CloseFrame { code: CloseCode::Library(4100), reason: "test".into() };
                let _ = ws.close(Some(close)).await;
            }
        })
        .await;
        let mut session = Session::new(|project: &str| json!({"status": "pass", "summary": project}), never);
        let end = session.connect(&url, "t", "test").await;
        assert_eq!(end, End::Lost("closed 4100".into()));
        let done = seen.lock().unwrap()[0].clone();
        assert_eq!(done["project"], "p");
        assert_eq!(done["report"]["summary"], "p");
        assert_eq!(session.cursor, Cursor { epoch: "e1".into(), seq: 5 });
    }

    #[tokio::test]
    async fn a_scan_that_outlives_its_connection_reports_on_the_next_one() {
        let hellos = Arc::new(Mutex::new(Vec::new()));
        let log = hellos.clone();
        let (url, _server) = fake(move |n, mut ws, hello| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(hello.clone());
                ws.send(Message::text(json!({"type": "welcome", "epoch": "e1", "seq": 1}).to_string())).await.unwrap();
                if n == 0 {
                    ws.send(Message::text(json!({"type": "work", "project": "p", "seq": 1}).to_string())).await.unwrap();
                    let _ack = next_json(&mut ws).await;
                    // The connection breaks while the scan runs.
                    drop(ws);
                } else {
                    let done = next_json(&mut ws).await;
                    assert_eq!(done["type"], "done");
                    assert_eq!(done["seq"], 1);
                    let _ = ws.close(Some(CloseFrame { code: CloseCode::Library(4003), reason: "removed".into() })).await;
                }
            }
        })
        .await;
        let mut session = Session::new(
            |_: &str| {
                std::thread::sleep(Duration::from_millis(300));
                json!({"status": "pass"})
            },
            never,
        );
        assert!(matches!(session.connect(&url, "t", "test").await, End::Lost(_)));
        assert_eq!(session.connect(&url, "t", "test").await, End::Removed);
        let hellos = hellos.lock().unwrap();
        assert_eq!(hellos[1]["busy"], "p", "the reconnect says which scan still runs");
        assert_eq!(hellos[1]["cursor"]["epoch"], "e1", "the reconnect resumes from its cursor");
    }

    #[tokio::test]
    async fn an_expired_token_asks_for_renewal() {
        let (url, _server) = fake(|_, mut ws, _| async move {
            let _ = ws.close(Some(CloseFrame { code: CloseCode::Library(4001), reason: "runner token expired".into() })).await;
        })
        .await;
        let mut session = Session::new(|_: &str| json!({}), never);
        assert_eq!(session.connect(&url, "expired", "test").await, End::Renew, "refused upgrade (401)");
        assert_eq!(session.connect(&url, "t", "test").await, End::Renew, "close 4001 at expiry");
    }
}
