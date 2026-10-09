//! Replication between machines over any byte channel (ssh, the paired Studio server channel).
//!
//! `layr sync -- <command>` starts `<command>`, which must run `layr serve-peer <project>` on the
//! other machine, and talks to it over the command's stdin and stdout:
//!
//! 1. hello: project id, per-machine sequence numbers, states present.
//! 2. records: each side sends the records the other lacks. Records are checked against the
//!    machine key from the first record of that machine's log, and appended verbatim.
//! 3. states: `btrfs send -p <latest common state>` for every state the peer lacks
//!    (commits, imports and stashes; automatic states with `--auto`).

use super::{exit, here};
use crate::args::{parse, Spec};
use crate::btrfs;
use crate::ctx::Ctx;
use crate::ids;
use crate::model::StateKind;
use crate::oplog;
use crate::repo::Repo;
use crate::store::Project;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

struct Wire<R: Read, W: Write> {
    r: R,
    w: W,
}

impl<R: Read, W: Write> Wire<R, W> {
    fn send(&mut self, kind: u8, data: &[u8]) -> Result<()> {
        self.w.write_all(&[kind])?;
        self.w.write_all(&(data.len() as u32).to_be_bytes())?;
        self.w.write_all(data)?;
        Ok(())
    }
    fn json(&mut self, v: &Value) -> Result<()> {
        self.send(b'J', serde_json::to_string(v)?.as_bytes())?;
        self.w.flush()?;
        Ok(())
    }
    fn recv(&mut self) -> Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 5];
        self.r.read_exact(&mut h).context("peer closed the channel")?;
        let n = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut d = vec![0u8; n];
        self.r.read_exact(&mut d)?;
        Ok((h[0], d))
    }
    fn recv_json(&mut self) -> Result<Value> {
        let (k, d) = self.recv()?;
        if k != b'J' {
            bail!("protocol error: expected a message");
        }
        let v: Value = serde_json::from_slice(&d)?;
        if v["op"] == "error" {
            bail!("peer: {}", v["message"].as_str().unwrap_or("error"));
        }
        Ok(v)
    }
    /// Send a btrfs stream from a child process.
    fn send_stream(&mut self, mut child: std::process::Child) -> Result<u64> {
        let mut out = child.stdout.take().unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0;
        loop {
            let n = out.read(&mut buf)?;
            if n == 0 {
                break;
            }
            self.send(b'D', &buf[..n])?;
            total += n as u64;
        }
        let st = child.wait_with_output()?;
        if !st.status.success() {
            self.send(b'X', &[])?;
            bail!("btrfs send failed: {}", String::from_utf8_lossy(&st.stderr).trim());
        }
        self.send(b'E', &[])?;
        self.w.flush()?;
        Ok(total)
    }
    /// Receive a btrfs stream into a folder.
    fn recv_stream(&mut self, dir: &std::path::Path) -> Result<()> {
        let mut child =
            Command::new("btrfs").args(["receive", "-q"]).arg(dir).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let mut ok = true;
        loop {
            let (k, d) = self.recv()?;
            match k {
                b'D' => {
                    if ok && stdin.write_all(&d).is_err() {
                        ok = false;
                    }
                }
                b'E' => break,
                b'X' => {
                    ok = false;
                    break;
                }
                _ => bail!("protocol error in a stream"),
            }
        }
        drop(stdin);
        let out = child.wait_with_output()?;
        if !ok || !out.status.success() {
            bail!("btrfs receive failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

fn seqs(p: &Project) -> Result<BTreeMap<String, u64>> {
    Ok(p.db.max_seqs()?.into_iter().collect())
}

fn present(p: &Project) -> Result<BTreeSet<String>> {
    Ok(p.db.present_states()?.into_iter().filter(|(_, d)| !d).map(|(id, _)| id).filter(|id| p.state_path(id).exists()).collect())
}

/// Records this side has beyond the peer's sequence numbers, as stored lines.
fn missing_lines(p: &Project, peer: &BTreeMap<String, u64>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for m in p.db.record_machines()? {
        out.extend(p.db.lines_after(&m, peer.get(&m).copied().unwrap_or(0))?);
    }
    Ok(out)
}

/// Check and store records from the peer.
fn take_lines(p: &Project, lines: &[String]) -> Result<usize> {
    p.import_lines(lines)
}

fn wanted(p: &Project, id: &str, auto: bool) -> bool {
    match p.db.state(id).ok().flatten() {
        Some(s) => auto || s.kind != StateKind::Auto,
        None => false,
    }
}

/// States to send, parents first, each with the best parent the peer has.
fn plan(repo: &Repo, mine: &BTreeSet<String>, theirs: &BTreeSet<String>, auto: bool) -> Result<Vec<(String, Option<String>)>> {
    let mut todo: Vec<(i64, String)> = mine
        .iter()
        .filter(|id| !theirs.contains(*id) && wanted(&repo.p, id, auto))
        .filter_map(|id| repo.p.db.state(id).ok().flatten().map(|s| (s.time, id.clone())))
        .collect();
    todo.sort();
    let mut have: BTreeSet<String> = theirs.intersection(mine).cloned().collect();
    let mut out = Vec::new();
    for (_, id) in todo {
        let mut parent = None;
        for a in repo.ancestors(&id)?.into_iter().skip(1) {
            if have.contains(&a) {
                parent = Some(a);
                break;
            }
        }
        if parent.is_none() {
            parent =
                have.iter().filter_map(|h| repo.p.db.state(h).ok().flatten().map(|s| (s.time, h.clone()))).max().map(|x| x.1);
        }
        have.insert(id.clone());
        out.push((id, parent));
    }
    Ok(out)
}

fn send_states<R: Read, W: Write>(
    w: &mut Wire<R, W>,
    repo: &Repo,
    list: &[(String, Option<String>)],
    layers: bool,
) -> Result<(usize, u64)> {
    let mut bytes = 0;
    for (id, parent) in list {
        w.json(&json!({"op": "state", "id": id, "parent": parent}))?;
        let pp = parent.as_ref().map(|p| repo.p.state_path(p));
        let child =
            btrfs::send_command(pp.as_deref(), &repo.p.state_path(id)).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        bytes += w.send_stream(child)?;
        let v = w.recv_json()?;
        if v["op"] != "ok" {
            bail!("peer did not store state {}", ids::short(id));
        }
        // Regenerable layers travel only for a live move (warm builds on the peer).
        if layers {
            if let Some(s) = repo.p.db.state(id)? {
                for snap in s.layers.values() {
                    let path = repo.p.states_dir().join(snap);
                    if !path.exists() {
                        continue;
                    }
                    w.json(&json!({"op": "layer", "name": snap}))?;
                    let child = btrfs::send_command(None, &path).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
                    bytes += w.send_stream(child)?;
                    let v = w.recv_json()?;
                    if v["op"] != "ok" {
                        bail!("peer did not store layer {snap}");
                    }
                }
            }
        }
    }
    w.json(&json!({"op": "done"}))?;
    Ok((list.len(), bytes))
}

fn recv_states<R: Read, W: Write>(w: &mut Wire<R, W>, p: &Project) -> Result<usize> {
    let mut n = 0;
    loop {
        let v = w.recv_json()?;
        match v["op"].as_str() {
            Some("done") => break,
            Some("layer") => {
                let name = v["name"].as_str().ok_or_else(|| anyhow!("bad layer message"))?.to_string();
                let (id, _) = name.split_once('@').ok_or_else(|| anyhow!("bad layer name"))?;
                if uuid::Uuid::parse_str(id).is_err() || name.contains('/') {
                    bail!("bad layer name");
                }
                let dst = p.states_dir().join(&name);
                if dst.exists() {
                    btrfs::delete_tree(&dst)?;
                }
                match w.recv_stream(&p.states_dir()) {
                    Ok(()) if dst.exists() => w.json(&json!({"op": "ok"}))?,
                    other => {
                        let _ = btrfs::delete_tree(&dst);
                        w.json(&json!({"op": "error", "message": format!("layer {name} not received")}))?;
                        other?;
                        bail!("layer {name} not received");
                    }
                }
            }
            Some("state") => {
                let id = v["id"].as_str().ok_or_else(|| anyhow!("bad state message"))?.to_string();
                if uuid::Uuid::parse_str(&id).is_err() {
                    bail!("bad state id");
                }
                // Receive into states/ directly: a read-only subvolume cannot move to another
                // folder. A failed receive leaves a writable subvolume without a received UUID;
                // it is deleted.
                let dst = p.state_path(&id);
                if dst.exists() {
                    bail!("state {} exists here already", ids::short(&id));
                }
                let r = w.recv_stream(&p.states_dir());
                let res = r.and_then(|_| {
                    let info =
                        btrfs::subvol_info(&dst).with_context(|| format!("stream did not contain state {}", ids::short(&id)))?;
                    if !info.readonly || info.received_uuid.is_none() {
                        bail!("incomplete receive of state {}", ids::short(&id));
                    }
                    p.db.set_present(&id, true)?;
                    Ok(())
                });
                if res.is_err() && dst.exists() {
                    let _ = btrfs::delete_tree(&dst);
                }
                match res {
                    Ok(()) => {
                        w.json(&json!({"op": "ok"}))?;
                        n += 1;
                    }
                    Err(e) => {
                        w.json(&json!({"op": "error", "message": e.to_string()}))?;
                        return Err(e);
                    }
                }
            }
            _ => bail!("protocol error: {v}"),
        }
    }
    Ok(n)
}

pub fn sync(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec =
        Spec::new().flag("--auto").flag("--layers").flag("--push-only").flag("--pull-only").flag("-q|--quiet").value("--project");
    let a = parse(&spec, args)?;
    if a.paths.is_empty() {
        return Err(exit(
            128,
            "usage: layr sync [--auto] [--push-only|--pull-only] -- <command that runs 'layr serve-peer <project>' on the peer>",
        ));
    }
    let h = here(ctx)?;
    h.repo.require_lead(ctx, "replicate the project")?;
    let repo = &h.repo;
    let mut c = Command::new(&a.paths[0]);
    c.args(&a.paths[1..]).stdin(Stdio::piped()).stdout(Stdio::piped());
    if unsafe { libc::geteuid() } == 0 && ctx.caller.uid != 0 {
        use std::os::unix::process::CommandExt;
        c.uid(ctx.caller.uid).gid(ctx.caller.gid);
        c.env("HOME", &ctx.caller.home);
    }
    for k in ["SSH_AUTH_SOCK", "PATH", "LAYR_ROOT"] {
        if let Some(v) = ctx.env(k) {
            c.env(k, v);
        }
    }
    let mut child = c.spawn().context("start the peer command")?;
    let mut w = Wire { r: child.stdout.take().unwrap(), w: std::io::BufWriter::new(child.stdin.take().unwrap()) };
    let started = std::time::Instant::now();
    let auto = a.has("--auto");
    let result = (|| -> Result<(usize, usize, usize, usize, u64)> {
        let mine = present(&repo.p)?;
        w.json(&json!({"op": "hello", "project": repo.p.name, "project_id": repo.p.id, "machine": repo.p.store.machine.id, "seqs": seqs(&repo.p)?, "present": mine}))?;
        let hello = w.recv_json()?;
        let peer_seqs: BTreeMap<String, u64> = serde_json::from_value(hello["seqs"].clone())?;
        let peer_present: BTreeSet<String> = serde_json::from_value(hello["present"].clone())?;
        // Records both ways.
        let out_lines = missing_lines(&repo.p, &peer_seqs)?;
        let sent_records = out_lines.len();
        w.json(&json!({"op": "records", "lines": out_lines}))?;
        let theirs = w.recv_json()?;
        let lines: Vec<String> = serde_json::from_value(theirs["lines"].clone())?;
        let got_records = take_lines(&repo.p, &lines)?;
        let _ = w.recv_json()?;
        // States to the peer.
        let (mut sent, mut bytes) = (0, 0);
        if !a.has("--pull-only") {
            let list = plan(repo, &mine, &peer_present, auto)?;
            let (n, b) = send_states(&mut w, repo, &list, a.has("--layers"))?;
            sent = n;
            bytes = b;
        } else {
            w.json(&json!({"op": "done"}))?;
        }
        // States from the peer.
        let mine_now = present(&repo.p)?;
        let want: Vec<String> = if a.has("--push-only") {
            vec![]
        } else {
            peer_present.iter().filter(|id| !mine_now.contains(*id) && wanted(&repo.p, id, auto)).cloned().collect()
        };
        w.json(&json!({"op": "want", "ids": want, "have": mine_now, "layers": a.has("--layers")}))?;
        let got = recv_states(&mut w, &repo.p)?;
        w.json(&json!({"op": "bye"}))?;
        Ok((sent_records, got_records, sent, got, bytes))
    })();
    drop(w);
    let _ = child.wait();
    let (sr, gr, ss, gs, bytes) = result?;
    if !a.has("-q") {
        outln!(
            ctx,
            "sync {}: records sent {sr}, received {gr}; states sent {ss} ({:.1} MiB), received {gs}; {:.2} s",
            repo.p.name,
            bytes as f64 / 1048576.0,
            started.elapsed().as_secs_f64()
        );
    }
    Ok(0)
}

/// The peer side of `layr sync`, on stdin and stdout.
pub fn serve(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let spec = Spec::new().flag("--create");
    let a = parse(&spec, args)?;
    let name = a.pos.first().ok_or_else(|| exit(128, "usage: layr serve-peer <project>"))?.clone();
    if !ctx.caller.is_root() {
        return Err(exit(1, "error: serve-peer needs root (it receives btrfs streams)"));
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut w = Wire { r: stdin.lock(), w: std::io::BufWriter::new(stdout.lock()) };
    let hello = w.recv_json()?;
    let pid = hello["project_id"].as_str().unwrap_or("").to_string();
    let exists = ctx.store.projects_dir().join(&name).join("meta").join("layr.sqlite").is_file();
    if !exists {
        Project::create_empty(&ctx.store, &name)?;
    }
    let p = match Project::open(&ctx.store, &name) {
        Ok(p) => p,
        Err(_) => Project::open_partial(&ctx.store, &name)?,
    };
    if !p.id.is_empty() && p.id != pid {
        w.json(&json!({"op": "error", "message": format!("project '{name}' here has another id")}))?;
        bail!("project id mismatch");
    }
    let repo = Repo { p };
    let peer_seqs: BTreeMap<String, u64> = serde_json::from_value(hello["seqs"].clone())?;
    w.json(&json!({"op": "hello", "machine": repo.p.store.machine.id, "seqs": seqs(&repo.p)?, "present": present(&repo.p)?}))?;
    let recs = w.recv_json()?;
    let lines: Vec<String> = serde_json::from_value(recs["lines"].clone())?;
    let mine = missing_lines(&repo.p, &peer_seqs)?;
    w.json(&json!({"op": "records", "lines": mine}))?;
    take_lines(&repo.p, &lines)?;
    w.json(&json!({"op": "count", "count": mine.len()}))?;
    let _ = a.has("--create");
    recv_states(&mut w, &repo.p)?;
    let want = w.recv_json()?;
    let ids_v: Vec<String> = serde_json::from_value(want["ids"].clone())?;
    let have: BTreeSet<String> = serde_json::from_value(want["have"].clone())?;
    let mine_set = present(&repo.p)?;
    let mut list = Vec::new();
    let mut have2 = have.clone();
    let mut todo: Vec<(i64, String)> = ids_v
        .iter()
        .filter(|i| mine_set.contains(*i))
        .filter_map(|i| repo.p.db.state(i).ok().flatten().map(|s| (s.time, i.clone())))
        .collect();
    todo.sort();
    for (_, id) in todo {
        let mut parent = None;
        for a in repo.ancestors(&id)?.into_iter().skip(1) {
            if have2.contains(&a) && mine_set.contains(&a) {
                parent = Some(a);
                break;
            }
        }
        have2.insert(id.clone());
        list.push((id, parent));
    }
    send_states(&mut w, &repo, &list, want["layers"].as_bool().unwrap_or(false))?;
    let _ = w.recv_json();
    let _ = ctx;
    Ok(0)
}
